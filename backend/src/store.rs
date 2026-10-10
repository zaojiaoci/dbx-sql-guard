//! 存储层：`GuardStore` 抽象 + 本地 SQLite 实现 + 防篡改哈希链。
//! 持久化只经过 `GuardStore`，将来换远端存储只需再实现一个类型，main.rs / mcp.rs 不动。
//!
//! 每行记录存两个 digest：
//! - `digest` = SHA-256(上一行 digest ‖ 本行不可变字段)，第一行的"上一行"是全零 `GENESIS`。
//!   改动任意行的 SQL / operator / 风险等级 / 时间戳，或删掉中间某行，都会让它后面所有行的
//!   digest 对不上——`verify()` 按 rowid 重算一遍就能发现，并报出第一个断点。
//! - `decision_digest` = SHA-256(本行审批字段)，**不参与**链：这样"先提交、后审批"不会让
//!   整条链失效，而审批信息被单独改过同样能查出来。
//!
//! 字段用 `\u{1}` 而不是空格/换行分隔：SQL 和备注都可含任何可见字符，只有不可能出现的控制
//! 字符才能避免"不同字段组合出同一字符串"从而伪造出相同 digest。
//!
//! 边界：链只能证明"库里的行没被改过"，证明不了"文件还在"（整库删除后 checked 为 0 而 ok
//! 仍为 true），也证明不了操作人身份；要对抗能改文件的人得把 digest 定期外发存证。这些限制
//! 在 docs/known-limits.md 里对用户写明了。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row};

use crate::model::{
    AuditRecord, CsvExport, DraftRecord, FacetValue, Facets, Finding, RecordFilter, RecordStatus,
    Risk, RuleSet, Stats, VerifyReport,
};
use crate::risk::redact_literals;
use crate::util::{format_ms_utc, now_ms, sha256_hex, short_id};

/// `include_str!` 编译期嵌入，所以 schema.sql 缺失会在编译期失败而不是运行时；
/// 语句都带 `IF NOT EXISTS`，老库升级时重复执行是安全的。
const SCHEMA: &str = include_str!("../sql/schema.sql");
/// 规则集在 `meta` 表里的键名。整份规则作为一个 JSON 文档存一行。
const RULES_KEY: &str = "rules";
/// 链的起点：第一行记录的 `prev_digest`。64 个 0，与 SHA-256 的输出等长。
const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// 列表查询的默认页大小和硬上限。UI 一次只渲染一页，上限挡住"一次拉全库"。
const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;
/// 单次 CSV 导出的行数上限。导出要绕过分页，但整份 CSV 是一条 JSON-RPC 响应，
/// 超过协议的 8 MiB 就会整体失败，所以宁可截断并明确告知。
const MAX_EXPORT_ROWS: i64 = 10_000;
/// 每个分面的候选值上限。库名和操作人都是用户自报的自由文本，数量不可控（拼错的库名
/// 会各自成为一个取值）。按计数倒序取，所以最常用的值一定在里面。
const MAX_FACET_VALUES: i64 = 100;

/// 存储层统一用 String 当错误类型：错误最终变成 JSON-RPC 的 message 给 UI 显示，不需要结构化。
pub type StoreError = String;
pub type StoreResult<T> = Result<T, StoreError>;

/// `decide` 的失败原因。只有这个方法需要区分类型，因为它的失败里混着两种性质完全不同的东西：
/// - `Rejected`：这次审批**不被允许**（记录不存在、已经决定过、审批人不合格）。调用方能自己
///   修正，映射成 JSON-RPC 的 -32602，message 原样展示给人看。
/// - `Store`：数据库本身出了问题。映射成 -32000，UI 应当当成插件故障。
///
/// 都走 `StoreError` 的后果是"你不是这条记录的审批人"被报成 -32000，用户以为是插件坏了。
#[derive(Debug)]
pub enum DecideError {
    Rejected(String),
    Store(String),
}

impl From<rusqlite::Error> for DecideError {
    fn from(error: rusqlite::Error) -> Self {
        DecideError::Store(error.to_string())
    }
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecideError::Rejected(message) | DecideError::Store(message) => {
                formatter.write_str(message)
            }
        }
    }
}

/// 存储抽象。`Send + Sync` 是硬性要求：SDK 在多个 worker 线程上共享同一个 handler，store 会被并发调用。
pub trait GuardStore: Send + Sync {
    fn rules(&self) -> StoreResult<RuleSet>;
    fn save_rules(&self, rules: &RuleSet) -> StoreResult<RuleSet>;
    fn submit(&self, draft: DraftRecord) -> StoreResult<AuditRecord>;
    /// 人工决定。审批人资格（名单、按库分组、不得与提交人相同）在这里校验而不是 main.rs：
    /// 这是审计完整性的一部分，任何调用方（UI、MCP、将来的远端实现）都必须经过同一道门。
    fn decide(
        &self,
        record_id: &str,
        status: RecordStatus,
        approver: &str,
        note: Option<&str>,
    ) -> Result<AuditRecord, DecideError>;
    fn list(&self, filter: &RecordFilter) -> StoreResult<(i64, Vec<AuditRecord>)>;
    fn stats(&self) -> StoreResult<Stats>;
    /// 分面候选值（审计页两个下拉：库、操作人）的内容。
    fn facets(&self) -> StoreResult<Facets>;
    fn verify(&self) -> StoreResult<VerifyReport>;
    /// 导出 CSV：过滤条件生效，但分页（limit/offset）被忽略并改用 `MAX_EXPORT_ROWS`，
    /// 返回值说明实际导出行数和是否被截断。
    fn export_csv(&self, filter: &RecordFilter) -> StoreResult<CsvExport>;
}

/// 本地 SQLite 实现：只维护**一条**连接并用 Mutex 串行化（写入频率是"人手点一次按钮"
/// 级别，不需要连接池）。`sequence` 是进程内自增计数、只用于让记录 ID 唯一，不表示顺序
/// （顺序由 rowid 和 created_at_ms 决定），所以 Relaxed 就够了。
pub struct SqliteStore {
    connection: Mutex<Connection>,
    sequence: AtomicU64,
}

impl SqliteStore {
    /// 打开（必要时创建）数据库文件，保证 schema 和默认规则就位。失败原因要人类可读：
    /// 它会出现在宿主的插件启动失败提示里，用户得看懂是权限问题还是文件损坏。
    pub fn open(path: &Path) -> StoreResult<Self> {
        // 宿主给的 data dir 在首次运行时可能还不存在。
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("Cannot create data dir: {error}"))?;
        }
        let connection = Connection::open(path).map_err(|error| format!("Cannot open store: {error}"))?;
        // 同一个库文件可能被两个进程打开（例如开发宿主和已安装插件同时运行）。
        // 不设置 busy_timeout 时 SQLite 会立刻返回 SQLITE_BUSY，写审计就会偶发失败。
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| format!("Cannot set busy timeout: {error}"))?;
        // WAL 让读不阻塞写；失败可忽略（例如只读文件系统），退回默认 journal 只是慢一点、不影响正确性，所以 `let _ =`。
        let _ = connection.pragma_update(None, "journal_mode", "WAL");
        connection
            .execute_batch(SCHEMA)
            .map_err(|error| format!("Cannot init store schema: {error}"))?;
        let store = Self {
            connection: Mutex::new(connection),
            sequence: AtomicU64::new(0),
        };
        // 首次运行（或规则行被删）时播种默认规则，之后 rules() 就总能成功，上层不必处理半初始化状态。
        if store.rules().is_err() {
            store.save_rules(&RuleSet::default())?;
        }
        Ok(store)
    }

    /// 路径优先级：`DBX_SQL_GUARD_DB`（测试和临时排查用，可指向空目录）→
    /// `DBX_PLUGIN_DATA_DIR`（宿主为每次 sidecar 运行设置的正式数据目录）→
    /// 系统临时目录下的 `dbx-sql-guard`，后者是为了不起宿主、直接 `cargo run` 也能手工验证协议。
    pub fn default_path() -> PathBuf {
        if let Ok(explicit) = std::env::var("DBX_SQL_GUARD_DB") {
            return PathBuf::from(explicit);
        }
        let root = std::env::var("DBX_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("dbx-sql-guard"));
        root.join("sql_guard.db")
    }

    /// 取连接。SDK 用 2~16 个 worker 线程并发调用 handler，而 `Connection` 不是 `Sync`，
    /// 所以整个存储层靠这一把锁串行化：任何时刻只有一个请求在读写数据库，
    /// "读上一条 digest → 写新记录"因此不会交错。
    ///
    /// 持锁期间 panic 会让 Mutex poisoned，这里选择恢复而不是报错：展开时未提交的
    /// `Transaction` 已析构回滚、连接仍可用，所以 into_inner 拿回的值不会是半写数据。
    /// 不恢复的话，一次 panic 就会让插件在重启前对所有请求返回 "lock is poisoned"。
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// ID 种子混入自增序号、进程号和毫秒时间戳：序号保证同进程内同毫秒不撞，
    /// 进程号保证两个进程写同一个库时不撞。ID 只用于定位记录、不含语义，顺序由 rowid 决定。
    fn next_record_id(&self, sql_hash: &str) -> String {
        let tick = self.sequence.fetch_add(1, Ordering::Relaxed);
        short_id(
            "sg_",
            &format!("{}|{sql_hash}|{tick}|{}", std::process::id(), now_ms()),
        )
    }
}

/// 参与内容 digest 的字段集合，即一条记录"不可变的一半"；审批字段不在这里，见 `decision_digest`。
/// 抽成结构体是为了让写入（`digest_of`）和校验（`verify`）共用同一份字段清单：两边各写一遍
/// 的话，漏一个字段就会让所有历史记录校验失败。
struct ChainInput<'a> {
    record_id: &'a str,
    sql: &'a str,
    sql_hash: &'a str,
    operator: &'a str,
    risk_level: &'a str,
    findings: &'a str,
    created_at_ms: i64,
}

/// 从任意连接（或事务）上读规则。抽成自由函数是因为 `decide` 必须在**同一个事务里**读规则：
/// 先 `self.rules()` 再取锁会自死锁（Mutex 不可重入），先读规则再取锁则留下一个窗口——
/// 期间有人改了名单，这次审批就会按旧规则放行。
fn read_rules(connection: &Connection) -> StoreResult<RuleSet> {
    let raw: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![RULES_KEY],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let raw = raw.ok_or_else(|| "Rules are not initialised".to_string())?;
    serde_json::from_str(&raw).map_err(|error| format!("Cannot parse stored rules: {error}"))
}

/// 名字归一化：名单和操作人都是用户手填的自由文本，`Alice`、`alice`、` alice ` 必须算同一个人，
/// 否则行尾一个空格就能让人绕过整份名单。库名同理（与审计分面的归并规则一致）。
fn normalized(value: &str) -> String {
    value.trim().to_lowercase()
}

/// 审批人资格判定。返回 `Err(原因)` 时这条决定不允许落库，原因直接给人看。
/// 两道检查，顺序是有意的：
/// 1. **不得自批**（可关）：先查这个，因为它和名单无关，在名单里也不该批自己提交的记录。
///    **只约束 approved**——驳回不授予任何权限，挡住它只会让记录永远卡在 pending，
///    单人使用时等于没有任何一条记录能被自己关掉。
/// 2. **名单**：按库查 `scope_approvers`，命中就只看那份名单，没命中才退回全局 `approvers`。
///    两种"空"含义不同：全局名单为空 = 没配限制，谁都能批；某库配了空名单 = 没人能批这个库
///    （fail-closed）。名单对驳回同样生效：驳回不授权，但会改写审计记录。
///
/// 记录没有库名时只能走全局名单——这是"库名是自报标签"的代价：不填就绕开了按库的限制。真要
/// 按库隔离得让宿主下发连接信息，而那需要插件持有数据库凭据，见 docs/known-limits.md。
fn authorize_approver(
    rules: &RuleSet,
    database: Option<&str>,
    operator: &str,
    approver: &str,
    status: RecordStatus,
) -> Result<(), String> {
    let who = normalized(approver);
    if who.is_empty() {
        return Err("`approver` is required".to_string());
    }
    if status == RecordStatus::Approved
        && rules.require_distinct_approver
        && who == normalized(operator)
    {
        return Err(format!(
            "`{approver}` submitted this record and cannot also approve it. \
             Ask someone else, or turn off `requireDistinctApprover` in the Rules tab \
             if you are the only person using this plugin."
        ));
    }
    // 按库查名单。键也做归一化，这样 `PROD` 和 `prod` 配的是同一份。
    let scope_label = database.unwrap_or("(no database)");
    let scope = database
        .map(normalized)
        .filter(|db| !db.is_empty())
        .and_then(|db| {
            rules
                .scope_approvers
                .iter()
                .find(|(key, _)| normalized(key) == db)
                .map(|(_, names)| names)
        });
    // 命中库就**只看**这份名单，哪怕它是空的；没命中才退回全局名单。
    // 全局名单为空表示"根本没配限制"，直接放行——单人使用时插件必须开箱可用。
    let allowed = match scope {
        Some(names) => names,
        None => {
            if rules.approvers.is_empty() {
                return Ok(());
            }
            &rules.approvers
        }
    };
    // 配了这个库、却一个审批人都没填。这里必须 fail-closed 而不是退回全局名单：
    // 审批配置出错时结果只能更严，不能更松，否则一次误删就等于把生产库放开。
    if allowed.is_empty() {
        return Err(format!(
            "`{scope_label}` has an approver list configured but it is empty, so nobody can \
             approve its records. Add a name in the Rules tab."
        ));
    }
    if allowed.iter().any(|name| normalized(name) == who) {
        return Ok(());
    }
    Err(format!(
        "`{approver}` is not allowed to approve records on `{scope_label}`. \
         Allowed: {}.",
        allowed
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// 内容 digest = `SHA-256(prev_digest ‖ 各字段)`。`prev_digest` 放最前面就是"链"的来源：
/// 每行都依赖前一行的结果，任何一处改动都会向后传播。字段之间用 `\u{1}` 分隔（见模块文档）。
fn content_digest(input: &ChainInput<'_>, prev_digest: &str) -> String {
    sha256_hex(&[
        prev_digest,
        input.record_id,
        input.sql,
        input.sql_hash,
        input.operator,
        input.risk_level,
        input.findings,
        &input.created_at_ms.to_string(),
    ]
    .join("\u{1}"))
}

fn digest_of(
    record_id: &str,
    draft: &DraftRecord,
    findings_json: &str,
    created_at_ms: i64,
    prev_digest: &str,
) -> String {
    content_digest(
        &ChainInput {
            record_id,
            sql: &draft.sql,
            sql_hash: &draft.sql_hash,
            operator: &draft.operator,
            risk_level: draft.risk_level.as_str(),
            findings: findings_json,
            created_at_ms,
        },
        prev_digest,
    )
}

/// 审批 digest：覆盖 status / approver / note / 决定时间。未决定时（submit 阶段）后三个
/// 参数传 None，算出的是"pending 且无人审批"的 digest；`verify()` 用同样规则重算，所以能
/// 区分"从未审批"和"审批信息被清空"。
fn decision_digest(
    record_id: &str,
    status: &str,
    approver: Option<&str>,
    note: Option<&str>,
    decided_at_ms: Option<i64>,
) -> String {
    sha256_hex(
        &[
            record_id,
            status,
            approver.unwrap_or(""),
            note.unwrap_or(""),
            &decided_at_ms.map(|value| value.to_string()).unwrap_or_default(),
        ]
        .join("\u{1}"),
    )
}

/// 数据库行 → `AuditRecord`。三处"读不出来就兜底"是有意的：`findings` 的 JSON 解析失败退回
/// 空列表、未知 risk 退回 `Low`、未知 status 退回 `Pending`。数据库文件可能被旧版本或外部
/// 工具写过，此时宁可少显示一条 finding、多让人审批一次，也不要让整个列表接口 500 掉——
/// 列表打不开，用户连"哪些记录有问题"都看不到。
fn row_to_record(row: &Row<'_>) -> rusqlite::Result<AuditRecord> {
    let findings_raw: String = row.get("findings")?;
    let findings: Vec<Finding> = serde_json::from_str(&findings_raw).unwrap_or_default();
    Ok(AuditRecord {
        record_id: row.get("record_id")?,
        sql: row.get("sql")?,
        sql_hash: row.get("sql_hash")?,
        connection_id: row.get("connection_id")?,
        database_name: row.get("database_name")?,
        db_type: row.get("db_type")?,
        operator: row.get("operator")?,
        risk_level: Risk::parse(&row.get::<_, String>("risk_level")?).unwrap_or(Risk::Low),
        status: RecordStatus::parse(&row.get::<_, String>("status")?).unwrap_or(RecordStatus::Pending),
        findings,
        estimated_rows: row.get("estimated_rows")?,
        approver: row.get("approver")?,
        note: row.get("note")?,
        created_at_ms: row.get("created_at_ms")?,
        decided_at_ms: row.get("decided_at_ms")?,
        digest: row.get("digest")?,
    })
}

/// WHERE 片段和它对应的参数值。两者必须成对返回、按同样顺序使用：SQL 文本里只出现 `?`
/// 占位符，来自调用方的值都通过参数绑定传入。这是本文件唯一的动态 SQL，也是最需要小心的
/// 一处——过滤条件里的关键字直接来自用户输入，拼进字符串就是 SQL 注入。
struct Clauses {
    sql: String,
    values: Vec<SqlValue>,
}

/// 把 `RecordFilter` 翻译成 WHERE 子句；没有条件时返回空串（不是 "WHERE 1=1"）。
fn build_filter(filter: &RecordFilter) -> Clauses {
    let mut parts: Vec<String> = Vec::new();
    let mut values: Vec<SqlValue> = Vec::new();
    if let Some(risk) = &filter.risk_level {
        parts.push("risk_level = ?".to_string());
        values.push(SqlValue::Text(risk.as_str().to_string()));
    }
    if let Some(status) = &filter.status {
        parts.push("status = ?".to_string());
        values.push(SqlValue::Text(status.as_str().to_string()));
    }
    if let Some(from) = filter.from_ms {
        parts.push("created_at_ms >= ?".to_string());
        values.push(SqlValue::Integer(from));
    }
    if let Some(to) = filter.to_ms {
        parts.push("created_at_ms <= ?".to_string());
        values.push(SqlValue::Integer(to));
    }
    // 作用域维度（库名、操作人都是自报自由文本）用大小写不敏感**等值**而不是 LIKE：
    // 分面下拉给的就是完整取值，模糊匹配会让 "prod" 顺带命中 "prod-replica"，
    // 筛选结果和下拉里显示的数字对不上。
    for (column, value) in [
        ("database_name", &filter.database),
        ("operator", &filter.operator),
    ] {
        if let Some(text) = value.as_ref().map(|raw| raw.trim()).filter(|raw| !raw.is_empty()) {
            // 列名来自上面写死的数组、不含外部输入，可以直接拼进 SQL 文本；值仍走参数绑定。
            parts.push(format!("lower({column}) = lower(?)"));
            values.push(SqlValue::Text(text.to_string()));
        }
    }
    if let Some(keyword) = filter
        .keyword
        .as_ref()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
    {
        // `%` 和 `_` 是 LIKE 的通配符，这里故意不转义：搜 "delete%" 时前缀匹配符合直觉，
        // 而通配不会造成越权读取（值本身仍走参数绑定，不进 SQL 文本）。
        parts.push("lower(sql) LIKE ?".to_string());
        values.push(SqlValue::Text(format!("%{keyword}%")));
    }
    Clauses {
        sql: if parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", parts.join(" AND "))
        },
        values,
    }
}

impl GuardStore for SqliteStore {
    /// `open()` 保证已播种，所以这里的 "not initialised" 只在规则行被外部删掉时出现；
    /// JSON 解析失败说明这份规则是别的版本写的。两种都要报错而不是静默用默认值——
    /// 静默替换会让用户以为自己配的规则还在生效。
    fn rules(&self) -> StoreResult<RuleSet> {
        let connection = self.lock();
        read_rules(&connection)
    }

    /// 整体覆盖规则（UPSERT）：规则只有一份、不做版本历史，所以单条语句即可。
    /// 调用方必须先用 `sanitize_rules` 校验过。
    fn save_rules(&self, rules: &RuleSet) -> StoreResult<RuleSet> {
        // 序列化放在取锁之前：这一步不涉及数据库，没必要占着锁做。
        let payload = serde_json::to_string(rules).map_err(|error| error.to_string())?;
        let connection = self.lock();
        connection
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![RULES_KEY, payload],
            )
            .map_err(|error| error.to_string())?;
        Ok(rules.clone())
    }

    /// 写入一条新的审计记录并接到哈希链末尾。"读上一行 digest → 算新 digest → INSERT"
    /// 必须在同一个事务、同一把锁里完成，否则两个并发提交会读到同一个 prev_digest，长出两条分叉的链。
    fn submit(&self, draft: DraftRecord) -> StoreResult<AuditRecord> {
        // 字面量值和注释内容在落盘前就被替换成 `?`：审计库不该变成密码和身份证号的集中存放处。
        // `sql_hash` 仍按**原始** SQL 计算（调用方传入），所以还能用来判断"这次提交的是不是
        // 同一条语句"，但无法从库里还原原文；digest 覆盖脱敏后的文本，verify() 才能只靠库里的列重算。
        let draft = DraftRecord {
            sql: redact_literals(&draft.sql),
            ..draft
        };
        // findings 以 JSON 文本存一列并参与 digest：改成别的存法就失去"命中项被人动过"的检测能力。
        let findings_json = serde_json::to_string(&draft.findings).map_err(|error| error.to_string())?;
        let created_at_ms = now_ms();
        let record_id = self.next_record_id(&draft.sql_hash);
        let mut connection = self.lock();
        let transaction = connection.transaction().map_err(|error| error.to_string())?;

        // 只有"表是空的"才算创世块，必须区分空表和真正的查询错误：吞掉错误的话，一次磁盘故障
        // 就会让新记录接在 GENESIS 后面悄悄长出第二条链，而防篡改校验会把它当成正常起点。
        let prev_digest: String = transaction
            .query_row(
                "SELECT digest FROM audit_record ORDER BY rowid DESC LIMIT 1",
                rusqlite::params![],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| GENESIS.to_string());

        let risk_text = draft.risk_level.as_str();
        let digest = digest_of(&record_id, &draft, &findings_json, created_at_ms, &prev_digest);
        let status = draft.status;
        let decision = decision_digest(&record_id, status.as_str(), None, None, None);

        transaction
            .execute(
                "INSERT INTO audit_record (
                    record_id, sql, sql_hash, connection_id, database_name, db_type, operator,
                    risk_level, status, findings, estimated_rows, approver, note,
                    created_at_ms, decided_at_ms, prev_digest, digest, decision_digest
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                rusqlite::params![
                    record_id,
                    draft.sql,
                    draft.sql_hash,
                    draft.connection_id,
                    draft.database_name,
                    draft.db_type,
                    draft.operator,
                    risk_text,
                    status.as_str(),
                    findings_json,
                    draft.estimated_rows,
                    Option::<String>::None,
                    Option::<String>::None,
                    created_at_ms,
                    Option::<i64>::None,
                    prev_digest,
                    digest,
                    decision,
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;

        // 直接用手上的 draft 拼返回值，不再查一次库：内容完全相同，省一次往返。
        // findings 的反序列化不可能失败（同一个进程刚序列化出来的），兜底成空列表只是为了避免 unwrap。
        Ok(AuditRecord {
            record_id,
            sql: draft.sql,
            sql_hash: draft.sql_hash,
            connection_id: draft.connection_id,
            database_name: draft.database_name,
            db_type: draft.db_type,
            operator: draft.operator,
            risk_level: draft.risk_level,
            status,
            findings: serde_json::from_str(&findings_json).unwrap_or_default(),
            estimated_rows: draft.estimated_rows,
            approver: None,
            note: None,
            created_at_ms,
            decided_at_ms: None,
            digest,
        })
    }

    /// 人工审批：把 pending 记录改成 approved 或 rejected，并更新审批 digest。内容链不动，
    /// 所以审批不会让历史记录"失效"。所有提前 return（记录不存在、已决定过、审批人不合格）
    /// 都发生在持锁状态：`Transaction` 析构时自动回滚、`MutexGuard` 自动释放，不需要手动
    /// 清理，也不会死锁。
    fn decide(
        &self,
        record_id: &str,
        status: RecordStatus,
        approver: &str,
        note: Option<&str>,
    ) -> Result<AuditRecord, DecideError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        // 一次取齐判定需要的所有列：status 决定"还能不能改"，operator / database_name 决定"谁能批"。
        let current: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT status, operator, database_name FROM audit_record WHERE record_id = ?1",
                rusqlite::params![record_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((current, operator, database)) = current else {
            return Err(DecideError::Rejected(format!("Unknown record: {record_id}")));
        };
        // 决定一旦落定就不能改判。decision_digest 故意不在内容链里（后补的审批不会让整条链失效），
        // 代价是覆盖上一次决定不会留下任何痕迹，所以只能在入口处禁止：已 approved 的记录被
        // 改成 rejected，事后无法察觉。
        if current != RecordStatus::Pending.as_str() {
            return Err(DecideError::Rejected(format!(
                "Record {record_id} is already `{current}` and cannot be decided again"
            )));
        }
        // 规则在同一个事务里读（见 read_rules）。读规则失败算存储故障，不算"这次审批不被
        // 允许"——拿不到名单时不能假装校验通过了。
        let rules = read_rules(&transaction).map_err(DecideError::Store)?;
        authorize_approver(&rules, database.as_deref(), &operator, approver, status)
            .map_err(DecideError::Rejected)?;
        let decided_at_ms = now_ms();
        let next = decision_digest(record_id, status.as_str(), Some(approver), note, Some(decided_at_ms));
        transaction
            .execute(
                "UPDATE audit_record
                    SET status = ?2, approver = ?3, note = ?4, decided_at_ms = ?5, decision_digest = ?6
                  WHERE record_id = ?1",
                rusqlite::params![record_id, status.as_str(), approver, note, decided_at_ms, next],
            )?;
        transaction.commit()?;
        // 必须先释放锁再调 fetch()：它自己也要取同一把锁，std::sync::Mutex 不可重入，否则自死锁。
        drop(connection);
        self.fetch(record_id).map_err(DecideError::Store)
    }

    /// 分页查询，返回 (符合条件的总行数, 当前页)。total 和 items 用同一份 WHERE、分两条
    /// 语句（先 COUNT 再取页，UI 的分页控件要显示总数），且都在同一把锁内，中间不会插入新写入。
    /// 排序用 `created_at_ms DESC, rowid DESC`：同一毫秒可能有多条记录，不按 rowid 兜底的话
    /// 翻页时会出现重复或漏项。
    fn list(&self, filter: &RecordFilter) -> StoreResult<(i64, Vec<AuditRecord>)> {
        let connection = self.lock();
        let clauses = build_filter(filter);
        let count_params = clipped_params(&clauses.values);
        let total: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM audit_record{}", clauses.sql),
                count_params.as_slice(),
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        // 分页参数在这里收敛，不信任调用方：limit 为 0 或负数没有意义，过大会把
        // 整库读进内存再序列化成一条 JSON 响应（受协议 8 MiB 上限限制会直接失败）。
        let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = filter.offset.unwrap_or(0).max(0);
        let mut statement = connection
            .prepare(&format!(
                "SELECT record_id, sql, sql_hash, connection_id, database_name, db_type, operator,
                        risk_level, status, findings, estimated_rows, approver, note,
                        created_at_ms, decided_at_ms, digest
                   FROM audit_record{}
                  ORDER BY created_at_ms DESC, rowid DESC
                  LIMIT ? OFFSET ?",
                clauses.sql
            ))
            .map_err(|error| error.to_string())?;
        // LIMIT/OFFSET 也走参数绑定，追加在过滤条件之后，顺序必须和 SQL 里的 `?` 一致。
        let mut values = clauses.values.clone();
        values.push(SqlValue::Integer(limit));
        values.push(SqlValue::Integer(offset));
        let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|value| value as &dyn rusqlite::ToSql).collect();
        let rows = statement
            .query_map(refs.as_slice(), row_to_record)
            .map_err(|error| error.to_string())?;
        // query_map 是惰性的：必须逐行取出并把行级错误转成 String，否则错误会被留在迭代器里直到丢弃。
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|error| error.to_string())?);
        }
        Ok((total, records))
    }

    /// 五条 COUNT 都在同一把锁内，所以数字之间是自洽的
    /// （不会出现 pending + approved + rejected > total 这种跨状态快照错位）。
    fn stats(&self) -> StoreResult<Stats> {
        let connection = self.lock();
        // 状态值是写死的字面量，不含任何外部输入，因此这里不需要参数绑定。
        let read = |sql: &str| -> StoreResult<i64> {
            connection
                .query_row(sql, rusqlite::params![], |row| row.get(0))
                .map_err(|error| error.to_string())
        };
        Ok(Stats {
            total: read("SELECT COUNT(*) FROM audit_record")?,
            pending: read("SELECT COUNT(*) FROM audit_record WHERE status = 'pending'")?,
            approved: read(
                "SELECT COUNT(*) FROM audit_record WHERE status IN ('approved','autoApproved')",
            )?,
            rejected: read("SELECT COUNT(*) FROM audit_record WHERE status = 'rejected'")?,
            high_risk: read("SELECT COUNT(*) FROM audit_record WHERE risk_level = 'high'")?,
        })
    }

    /// 库名和操作人都是自报的自由文本，同一个库可能被写成 `prod` / `PROD` / `Prod`，所以按 `lower()`
    /// 归并分组、显示取组内字典序最小的拼写（`MIN`）保证结果稳定。筛选侧同样是大小写不敏感等值匹配
    /// （见 `build_filter`），所以点下拉里任一拼写都能命中整组，计数和列表条数不会打架。
    fn facets(&self) -> StoreResult<Facets> {
        let connection = self.lock();
        // 列名只可能来自下面写死的两个字面量，不含外部输入，所以能直接拼进 SQL；LIMIT 仍走参数绑定。
        let facet = |column: &str| -> StoreResult<Vec<FacetValue>> {
            let query = format!(
                "SELECT MIN({column}) AS value, COUNT(*) AS total FROM audit_record
                 WHERE {column} IS NOT NULL AND trim({column}) <> ''
                 GROUP BY lower({column})
                 ORDER BY total DESC, value ASC
                 LIMIT ?1"
            );
            let mut statement = connection.prepare(&query).map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(rusqlite::params![MAX_FACET_VALUES], |row| {
                    Ok(FacetValue { value: row.get(0)?, count: row.get(1)? })
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())
        };
        Ok(Facets {
            databases: facet("database_name")?,
            operators: facet("operator")?,
        })
    }

    /// 从头重算整条链，检查有没有人绕过插件直接改过数据库；全程只读，不改数据库。
    /// 按 rowid 升序（即写入顺序）遍历，维护一个 `expected_prev`：
    /// - `prev_digest != expected_prev` 说明链接断了——中间被删过行，或 prev 被改过；
    /// - `重算的 digest != 存的 digest` 说明这一行内容被改过（SQL、operator、风险等级、findings、时间戳）。
    ///
    /// 两者只记录**第一个**断点，因为后面的行必然跟着错位，全列出来没有信息量。
    /// 审批字段单独校验：`decision_digest` 不在链里，改审批不会造成链断裂，必须逐行重算比对。
    ///
    /// 整库文件被删时会返回 checked=0 且 ok=true，这是这套机制的固有边界（见模块文档）。
    fn verify(&self) -> StoreResult<VerifyReport> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(
                "SELECT record_id, sql, sql_hash, operator, risk_level, findings, created_at_ms,
                        prev_digest, digest, status, approver, note, decided_at_ms, decision_digest
                   FROM audit_record ORDER BY rowid ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(rusqlite::params![], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, String>(13)?,
                ))
            })
            .map_err(|error| error.to_string())?;

        // 链从 GENESIS 开始；`expected_prev` 是"这一行的 prev_digest 应该是什么"。
        let mut expected_prev = GENESIS.to_string();
        let mut checked = 0i64;
        let mut first_broken_record = None;
        let mut decision_mismatches = Vec::new();
        for row in rows {
            let (
                record_id,
                sql,
                sql_hash,
                operator,
                risk_level,
                findings,
                created_at_ms,
                prev_digest,
                digest,
                status,
                approver,
                note,
                decided_at_ms,
                stored_decision,
            ) = row.map_err(|error| error.to_string())?;
            checked += 1;
            let recomputed = content_digest(
                &ChainInput {
                    record_id: &record_id,
                    sql: &sql,
                    sql_hash: &sql_hash,
                    operator: &operator,
                    risk_level: &risk_level,
                    findings: &findings,
                    created_at_ms,
                },
                &prev_digest,
            );
            let chain_broken = prev_digest != expected_prev;
            let content_broken = recomputed != digest;
            if (chain_broken || content_broken) && first_broken_record.is_none() {
                first_broken_record = Some(record_id.clone());
            }
            let recomputed_decision = decision_digest(
                &record_id,
                &status,
                approver.as_deref(),
                note.as_deref(),
                decided_at_ms,
            );
            if stored_decision != recomputed_decision {
                decision_mismatches.push(record_id.clone());
            }
            // 关键细节：下一行的期望值取**库里存的** digest，而不是刚重算出来的。这样篡改只会
            // 在被改的那一行暴露（content_broken），而不会让后面每一行都跟着报错；用
            // recomputed 的话一次改动会把整条链后面全部标红，反而看不出到底哪一行被动过。
            expected_prev = digest;
        }
        Ok(VerifyReport {
            ok: first_broken_record.is_none() && decision_mismatches.is_empty(),
            checked,
            first_broken_record,
            decision_mismatches,
        })
    }

    /// 导出 CSV，列顺序固定、第一行是表头。这里刻意覆盖调用方传来的 limit/offset：导出的
    /// 语义是"把符合过滤条件的记录都给我"，沿用列表页的分页参数会让用户点"导出全部"却只拿到
    /// 当前页，而界面上显示的还是全量条数。上限改由 `MAX_EXPORT_ROWS` 控制，并如实上报截断。
    fn export_csv(&self, filter: &RecordFilter) -> StoreResult<CsvExport> {
        let query = RecordFilter {
            limit: Some(MAX_EXPORT_ROWS),
            offset: Some(0),
            ..filter.clone()
        };
        let (total, records) = self.list(&query)?;
        let exported = records.len() as i64;
        let header = [
            "recordId",
            "createdAtUtc",
            "decidedAtUtc",
            "operator",
            "approver",
            "database",
            "connectionId",
            "dbType",
            "riskLevel",
            "status",
            "estimatedRows",
            "findings",
            "note",
            "sql",
            "digest",
        ];
        let mut lines = Vec::with_capacity(records.len() + 1);
        lines.push(header.join(","));
        for record in records {
            let findings = record
                .findings
                .iter()
                .map(|finding| format!("{}:{}", finding.code, finding.risk.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            lines.push(
                [
                    csv(&record.record_id),
                    csv(&format_ms_utc(record.created_at_ms)),
                    csv(&record.decided_at_ms.map(format_ms_utc).unwrap_or_default()),
                    csv(&record.operator),
                    csv(record.approver.as_deref().unwrap_or("")),
                    csv(record.database_name.as_deref().unwrap_or("")),
                    csv(record.connection_id.as_deref().unwrap_or("")),
                    csv(record.db_type.as_deref().unwrap_or("")),
                    csv(record.risk_level.as_str()),
                    csv(record.status.as_str()),
                    csv(&record.estimated_rows.map(|value| value.to_string()).unwrap_or_default()),
                    csv(&findings),
                    csv(record.note.as_deref().unwrap_or("")),
                    csv(&record.sql),
                    csv(&record.digest),
                ]
                .join(","),
            );
        }
        // 开头的 BOM 让 Excel 正确识别 UTF-8，否则中文备注和 SQL 会显示成乱码。
        // 行分隔用 CRLF（RFC 4180），单元格内部的换行由 csv() 加引号包住。
        let csv = format!("\u{FEFF}{}\n", lines.join("\r\n"));
        Ok(CsvExport {
            csv,
            exported,
            truncated: total > exported,
        })
    }
}

impl SqliteStore {
    /// 按 ID 取单条完整记录：`decide()` 提交事务后用它拿回最新状态，避免手工拼一遍审批字段。
    fn fetch(&self, record_id: &str) -> StoreResult<AuditRecord> {
        let connection = self.lock();
        connection
            .query_row(
                "SELECT record_id, sql, sql_hash, connection_id, database_name, db_type, operator,
                        risk_level, status, findings, estimated_rows, approver, note,
                        created_at_ms, decided_at_ms, digest
                   FROM audit_record WHERE record_id = ?1",
                rusqlite::params![record_id],
                row_to_record,
            )
            .map_err(|error| error.to_string())
    }
}

fn clipped_params(values: &[SqlValue]) -> Vec<&dyn rusqlite::ToSql> {
    values.iter().map(|value| value as &dyn rusqlite::ToSql).collect()
}

/// CSV 单元格转义，两件事都必须做：
/// 1. 用双引号包住并把内部的 `"` 翻倍——SQL 和备注里既有逗号也有引号和换行；
/// 2. 以 `= + - @ TAB CR` 开头时在前面加单引号，防止 Excel / WPS 把单元格当公式执行：
///    导出会被别人用表格软件打开，而内容来自数据库里的字符串，等于不可信输入，
///    这就是典型的 CSV 公式注入。
fn csv(value: &str) -> String {
    let guarded = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Finding, RecordStatus, Risk, RuleSet};
    use std::sync::atomic::AtomicUsize;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_db() -> PathBuf {
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("dbx-sql-guard-test-{}-{index}.db", std::process::id()))
    }

    fn draft(sql: &str) -> DraftRecord {
        DraftRecord {
            sql: sql.to_string(),
            sql_hash: sha256_hex(sql),
            connection_id: Some("conn-1".to_string()),
            database_name: Some("prod".to_string()),
            db_type: Some("postgres".to_string()),
            operator: "zaojiaoci".to_string(),
            risk_level: Risk::High,
            findings: vec![Finding {
                code: "noWhereDml".to_string(),
                risk: Risk::High,
                message: "no where".to_string(),
                statement: None,
            }],
            estimated_rows: Some(12),
            status: RecordStatus::Pending,
        }
    }

    fn store() -> SqliteStore {
        let path = temp_db();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
        SqliteStore::open(&path).expect("open store")
    }

    #[test]
    fn seeds_default_rules_and_round_trips_edits() {
        let store = store();
        let rules = store.rules().expect("seeded rules");
        assert_eq!(rules.approval_from_level, Risk::High);
        assert!(rules.detect_no_where_dml);
        let edited = RuleSet {
            approval_from_level: Risk::Medium,
            whitelist_databases: vec!["sandbox".to_string()],
            ..rules
        };
        store.save_rules(&edited).expect("save");
        let reread = store.rules().expect("reread");
        assert_eq!(reread.approval_from_level, Risk::Medium);
        assert_eq!(reread.whitelist_databases, vec!["sandbox".to_string()]);
    }

    #[test]
    fn submit_stores_findings_and_reports_status() {
        let store = store();
        let record = store.submit(draft("DELETE FROM t")).expect("submit");
        assert_eq!(record.status, RecordStatus::Pending);
        assert_eq!(record.findings[0].code, "noWhereDml");
        assert!(!record.digest.is_empty());

        let (total, items) = store
            .list(&RecordFilter {
                keyword: Some("delete from".to_string()),
                ..Default::default()
            })
            .expect("list");
        assert_eq!(total, 1);
        assert_eq!(items[0].record_id, record.record_id);
    }

    #[test]
    fn decision_moves_pending_to_approved() {
        let store = store();
        let record = store.submit(draft("DELETE FROM a")).expect("submit");
        let decided = store
            .decide(&record.record_id, RecordStatus::Approved, "dba", Some("ok"))
            .expect("decide");
        assert_eq!(decided.status, RecordStatus::Approved);
        assert_eq!(decided.approver.as_deref(), Some("dba"));
        assert_eq!(decided.note.as_deref(), Some("ok"));
        assert!(decided.decided_at_ms.is_some());
        assert!(store
            .decide("sg_missing", RecordStatus::Approved, "dba", None)
            .is_err());
    }

    #[test]
    fn a_decided_record_cannot_be_decided_again() {
        let store = store();
        let record = store.submit(draft("DELETE FROM a")).expect("submit");
        store
            .decide(&record.record_id, RecordStatus::Approved, "dba", None)
            .expect("first decision");
        assert!(
            store
                .decide(&record.record_id, RecordStatus::Rejected, "dba", None)
                .is_err(),
            "改判必须被拒绝，否则上一次的批准会被无声覆盖"
        );
        let (_, items) = store.list(&RecordFilter::default()).expect("list");
        assert_eq!(items[0].status, RecordStatus::Approved);
        assert_eq!(items[0].approver.as_deref(), Some("dba"));
    }

    fn with_rules(store: &SqliteStore, edit: impl FnOnce(&mut RuleSet)) -> RuleSet {
        let mut rules = store.rules().expect("seeded rules");
        edit(&mut rules);
        store.save_rules(&rules).expect("save rules")
    }

    fn submit_as(store: &SqliteStore, database: Option<&str>, operator: &str) -> AuditRecord {
        store
            .submit(DraftRecord {
                database_name: database.map(str::to_string),
                operator: operator.to_string(),
                ..draft("DELETE FROM a WHERE id = 1")
            })
            .expect("submit")
    }

    #[test]
    fn self_approval_is_rejected_by_default() {
        let store = store();
        // draft() 的 operator 是 "zaojiaoci"，同一个人来批必须被挡住。
        let record = store.submit(draft("DELETE FROM a WHERE id = 1")).expect("submit");
        let error = store
            .decide(&record.record_id, RecordStatus::Approved, "ZAOJIAOCI ", None)
            .expect_err("自己批自己必须被拒绝");
        assert!(
            matches!(error, DecideError::Rejected(_)),
            "这是策略拒绝，不是存储故障，必须报 -32602 而不是 -32000: {error}"
        );
        // 大小写和首尾空格不能成为绕过手段。
        assert!(error.to_string().contains("cannot also approve it"));
        // 记录仍然是 pending：被拒的决定一点痕迹都不该留下。
        let (_, items) = store.list(&RecordFilter::default()).expect("list");
        assert_eq!(items[0].status, RecordStatus::Pending);
        assert_eq!(items[0].approver, None);
    }

    #[test]
    fn submitter_can_reject_their_own_record() {
        let store = store();
        // require_distinct_approver 默认开着：驳回不授予任何权限，挡住它只会让记录永远卡在 pending，单人使用时没有任何一条记录能被自己关掉。
        let record = store.submit(draft("DELETE FROM a WHERE id = 1")).expect("submit");
        let decided = store
            .decide(&record.record_id, RecordStatus::Rejected, "zaojiaoci", Some("自己撤回"))
            .expect("提交人必须能驳回自己的记录");
        assert_eq!(decided.status, RecordStatus::Rejected);
        assert_eq!(decided.approver.as_deref(), Some("zaojiaoci"));
        // 驳回同样是终局：不能借着"再批一次"把 rejected 翻成 approved。
        assert!(matches!(
            store.decide(&record.record_id, RecordStatus::Approved, "dba", None),
            Err(DecideError::Rejected(_))
        ));
    }

    #[test]
    fn self_approval_can_be_switched_off_for_solo_use() {
        let store = store();
        with_rules(&store, |rules| rules.require_distinct_approver = false);
        let record = store.submit(draft("DELETE FROM a WHERE id = 1")).expect("submit");
        let decided = store
            .decide(&record.record_id, RecordStatus::Approved, "zaojiaoci", None)
            .expect("单人使用时必须能自己批");
        assert_eq!(decided.status, RecordStatus::Approved);
    }

    #[test]
    fn approver_list_restricts_who_can_decide() {
        let store = store();
        with_rules(&store, |rules| {
            rules.require_distinct_approver = false;
            rules.approvers = vec!["dba".to_string(), "lead".to_string()];
        });
        let record = submit_as(&store, Some("prod"), "dev");
        let error = store
            .decide(&record.record_id, RecordStatus::Approved, "intern", None)
            .expect_err("不在名单里的人不能批");
        assert!(error.to_string().contains("dba, lead"), "got: {error}");
        // 名单里的人可以批，且大小写与空格不敏感。
        let decided = store
            .decide(&record.record_id, RecordStatus::Approved, " DBA ", None)
            .expect("名单内应当放行");
        assert_eq!(decided.approver.as_deref(), Some(" DBA "));
    }

    #[test]
    fn an_empty_list_means_anyone_can_approve() {
        let store = store();
        // 默认就是空名单：单人场景下插件必须开箱可用。
        with_rules(&store, |rules| rules.require_distinct_approver = false);
        let record = submit_as(&store, Some("prod"), "dev");
        assert!(store
            .decide(&record.record_id, RecordStatus::Approved, "whoever", None)
            .is_ok());
    }

    #[test]
    fn scope_approvers_replace_the_global_list() {
        let store = store();
        with_rules(&store, |rules| {
            rules.require_distinct_approver = false;
            rules.approvers = vec!["dba".to_string()];
            rules.scope_approvers =
                [("PROD".to_string(), vec!["lead".to_string()])].into_iter().collect();
        });
        // 命中 prod：全局名单里的 dba 不再有效，只有 lead 能批。
        let prod = submit_as(&store, Some("prod"), "dev");
        assert!(store
            .decide(&prod.record_id, RecordStatus::Approved, "dba", None)
            .is_err());
        assert!(store
            .decide(&prod.record_id, RecordStatus::Approved, "lead", None)
            .is_ok());
        // 未命中任何 scope 的库仍然走全局名单。
        let staging = submit_as(&store, Some("staging"), "dev");
        assert!(store
            .decide(&staging.record_id, RecordStatus::Approved, "dba", None)
            .is_ok());
    }

    #[test]
    fn an_empty_scope_list_locks_that_scope() {
        let store = store();
        with_rules(&store, |rules| {
            rules.require_distinct_approver = false;
            rules.approvers = vec!["dba".to_string()];
            // 配了 prod 但一个人都没填 = 没人能批 prod，而不是退回全局名单：审批配置出错时只能更严，不能更松。
            rules.scope_approvers = [("prod".to_string(), Vec::new())].into_iter().collect();
        });
        let record = submit_as(&store, Some("prod"), "dev");
        let error = store
            .decide(&record.record_id, RecordStatus::Approved, "dba", None)
            .expect_err("空 scope 名单必须 fail-closed");
        assert!(error.to_string().contains("nobody can approve"), "got: {error}");
    }

    #[test]
    fn records_without_a_database_fall_back_to_the_global_list() {
        let store = store();
        with_rules(&store, |rules| {
            rules.require_distinct_approver = false;
            rules.approvers = vec!["dba".to_string()];
            rules.scope_approvers = [("prod".to_string(), vec!["lead".to_string()])].into_iter().collect();
        });
        let record = submit_as(&store, None, "dev");
        assert!(store
            .decide(&record.record_id, RecordStatus::Approved, "dba", None)
            .is_ok());
    }

    #[test]
    fn unknown_record_is_a_rejection_not_a_store_failure() {
        let store = store();
        let error = store
            .decide("sg_missing", RecordStatus::Approved, "dba", None)
            .expect_err("不存在的记录");
        assert!(matches!(error, DecideError::Rejected(_)), "got: {error:?}");
    }

    #[test]
    fn authorize_normalizes_names_and_scopes() {
        let mut rules = RuleSet {
            require_distinct_approver: false,
            approvers: vec!["Alice".to_string()],
            ..RuleSet::default()
        };
        assert!(authorize_approver(&rules, None, "bob", "alice", RecordStatus::Approved).is_ok());
        assert!(authorize_approver(&rules, None, "bob", " ALICE ", RecordStatus::Approved).is_ok());
        assert!(authorize_approver(&rules, None, "bob", "mallory", RecordStatus::Approved).is_err());
        // 空审批人一律拒绝：留痕的前提是知道是谁批的。
        assert!(authorize_approver(&rules, None, "bob", "   ", RecordStatus::Approved).is_err());
        // 库名同样忽略大小写和空格。
        rules.scope_approvers = [("Prod".to_string(), vec!["dba".to_string()])].into_iter().collect();
        assert!(authorize_approver(&rules, Some(" PROD "), "bob", "dba", RecordStatus::Approved).is_ok());
        assert!(authorize_approver(&rules, Some(" PROD "), "bob", "alice", RecordStatus::Approved).is_err());
    }

    #[test]
    fn store_survives_a_panic_while_holding_the_lock() {
        let store = std::sync::Arc::new(store());
        let doomed = store.clone();
        // 子线程持锁 panic，把 Mutex 打成 poisoned。
        let _ = std::thread::spawn(move || {
            let _guard = doomed.lock();
            panic!("boom");
        })
        .join();
        // 主线程必须还能写：否则一次 panic 就要重启 sidecar 才能恢复。
        let record = store.submit(draft("DELETE FROM a")).expect("submit after panic");
        assert_eq!(record.status, RecordStatus::Pending);
    }

    #[test]
    fn filters_combine_risk_status_and_time() {
        let store = store();
        store.submit(draft("DELETE FROM a")).expect("a");
        store
            .submit(DraftRecord {
                risk_level: Risk::Low,
                status: RecordStatus::AutoApproved,
                sql: "SELECT 1".to_string(),
                ..draft("SELECT 1")
            })
            .expect("b");
        let (high, _) = store
            .list(&RecordFilter {
                risk_level: Some(Risk::High),
                ..Default::default()
            })
            .expect("high");
        assert_eq!(high, 1);
        let (auto, _) = store
            .list(&RecordFilter {
                status: Some(RecordStatus::AutoApproved),
                ..Default::default()
            })
            .expect("auto");
        assert_eq!(auto, 1);
        let (future, _) = store
            .list(&RecordFilter {
                from_ms: Some(now_ms() + 60_000),
                ..Default::default()
            })
            .expect("future");
        assert_eq!(future, 0);
        let stats = store.stats().expect("stats");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.high_risk, 1);
    }

    #[test]
    fn filters_by_database_and_operator_case_insensitively() {
        let store = store();
        // draft() 默认是 prod / zaojiaoci，这里再塞两条不同库、不同操作人的。
        store.submit(draft("DELETE FROM a")).expect("prod");
        store
            .submit(DraftRecord {
                database_name: Some("PROD".to_string()),
                operator: "alice@box".to_string(),
                sql: "DELETE FROM b".to_string(),
                ..draft("DELETE FROM b")
            })
            .expect("prod upper");
        store
            .submit(DraftRecord {
                database_name: Some("staging".to_string()),
                operator: "alice@box".to_string(),
                sql: "DELETE FROM c".to_string(),
                ..draft("DELETE FROM c")
            })
            .expect("staging");
        // 没有库名的记录不该被任何库筛出来，但也不该让分面里出现空值。
        store
            .submit(DraftRecord {
                database_name: Some("   ".to_string()),
                sql: "DELETE FROM d".to_string(),
                ..draft("DELETE FROM d")
            })
            .expect("blank db");

        let by_db = |database: &str| {
            store
                .list(&RecordFilter { database: Some(database.to_string()), ..Default::default() })
                .map(|(total, _)| total)
                .expect("list by database")
        };
        // prod 和 PROD 是同一个作用域：大小写不敏感等值匹配。
        assert_eq!(by_db("prod"), 2);
        assert_eq!(by_db("PROD"), 2);
        assert_eq!(by_db("staging"), 1);
        // 空白等同于不过滤，而不是"匹配空库名"。
        assert_eq!(by_db("   "), 4);
        // 等值而不是前缀匹配：prod 不该顺带命中 prod-replica 这类值。
        assert_eq!(by_db("pro"), 0);

        let (by_operator, _) = store
            .list(&RecordFilter { operator: Some("ALICE@box".to_string()), ..Default::default() })
            .expect("list by operator");
        assert_eq!(by_operator, 2);

        // 两个维度可以叠加，也能和风险等级一起用。
        let (combined, _) = store
            .list(&RecordFilter {
                database: Some("prod".to_string()),
                operator: Some("alice@box".to_string()),
                risk_level: Some(Risk::High),
                ..Default::default()
            })
            .expect("combined");
        assert_eq!(combined, 1);
    }

    #[test]
    fn facets_group_by_lowercase_value_and_skip_blanks() {
        let store = store();
        store.submit(draft("DELETE FROM a")).expect("prod 1");
        store.submit(draft("DELETE FROM b")).expect("prod 2");
        store
            .submit(DraftRecord {
                database_name: Some("Staging".to_string()),
                operator: "alice@box".to_string(),
                sql: "DELETE FROM c".to_string(),
                ..draft("DELETE FROM c")
            })
            .expect("staging");
        store
            .submit(DraftRecord {
                database_name: Some("  ".to_string()),
                operator: "  ".to_string(),
                sql: "DELETE FROM d".to_string(),
                ..draft("DELETE FROM d")
            })
            .expect("blank");

        let facets = store.facets().expect("facets");
        // prod 的两条大小写相同，staging 一条；空库名不进分面。
        assert_eq!(
            facets.databases.iter().map(|entry| (entry.value.as_str(), entry.count)).collect::<Vec<_>>(),
            vec![("prod", 2), ("Staging", 1)]
        );
        // 操作人：draft() 默认的 zaojiaoci 两条（第 3、4 条都被覆盖掉了）、alice@box 一条，按计数倒序。
        assert_eq!(
            facets.operators.iter().map(|entry| (entry.value.as_str(), entry.count)).collect::<Vec<_>>(),
            vec![("zaojiaoci", 2), ("alice@box", 1)]
        );
    }

    #[test]
    fn verification_passes_on_an_untouched_chain() {
        let store = store();        store.submit(draft("DELETE FROM a")).expect("1");
        store.submit(draft("DELETE FROM b")).expect("2");
        store.submit(draft("DELETE FROM c")).expect("3");
        let report = store.verify().expect("verify");
        assert!(report.ok, "{report:?}");
        assert_eq!(report.checked, 3);
    }

    #[test]
    fn verification_detects_a_tampered_sql_cell() {
        let store = store();
        store.submit(draft("DELETE FROM a")).expect("1");
        let second = store.submit(draft("DELETE FROM b")).expect("2");
        store
            .decide(&second.record_id, RecordStatus::Approved, "dba", None)
            .expect("decide");
        {
            let connection = store.lock();
            // 模拟有人用外部 SQLite 工具直接改库文件。
            connection
                .execute(
                    "UPDATE audit_record SET sql = 'DELETE FROM a -- quietly rewritten' WHERE record_id = ?1",
                    rusqlite::params![second.record_id],
                )
                .expect("tamper");
        }
        let report = store.verify().expect("verify");
        assert!(!report.ok);
        assert_eq!(report.first_broken_record.as_deref(), Some(second.record_id.as_str()));
    }

    #[test]
    fn verification_detects_a_status_edit_outside_the_plugin() {
        let store = store();
        let record = store.submit(draft("DROP TABLE t")).expect("submit");
        {
            let connection = store.lock();
            connection
                .execute(
                    "UPDATE audit_record SET status = 'approved', approver = 'self' WHERE record_id = ?1",
                    rusqlite::params![record.record_id],
                )
                .expect("tamper");
        }
        let report = store.verify().expect("verify");
        assert!(!report.ok);
        assert_eq!(report.decision_mismatches, vec![record.record_id]);
    }

    #[test]
    fn deleting_a_row_breaks_the_forward_chain() {
        let store = store();
        let first = store.submit(draft("DELETE FROM a")).expect("1");
        store.submit(draft("DELETE FROM b")).expect("2");
        {
            let connection = store.lock();
            connection
                .execute("DELETE FROM audit_record WHERE record_id = ?1", rusqlite::params![first.record_id])
                .expect("delete");
        }
        let report = store.verify().expect("verify");
        assert!(!report.ok, "{report:?}");
        assert_eq!(report.checked, 1);
    }

    #[test]
    fn csv_export_quotes_escapes_and_neutralises_formulas() {
        let store = store();
        store
            .submit(DraftRecord {
                sql: "=SUM(evil())".to_string(),
                ..draft("=SUM(evil())")
            })
            .expect("submit");
        store
            .submit(DraftRecord {
                sql: "DELETE FROM t WHERE name IN ('a, b', 'c')".to_string(),
                ..draft("DELETE")
            })
            .expect("submit");
        let export = store.export_csv(&RecordFilter::default()).expect("csv");
        let csv_text = export.csv;
        assert!(csv_text.starts_with('\u{FEFF}'), "expected BOM");
        let header = csv_text.lines().next().expect("header");
        assert!(header.contains("recordId") && header.contains("sql"));
        // 开头的 `=` 被加上单引号，表格软件就不会把它当公式执行。
        assert!(csv_text.contains("\"'=SUM(evil())\""), "{csv_text}");
        // 单元格内的逗号被引号包住，仍然是一个字段；字面量值已经消失。
        assert!(
            csv_text.contains("\"DELETE FROM t WHERE name IN ('?', '?')\""),
            "{csv_text}"
        );
        assert!(!csv_text.contains("a, b"), "{csv_text}");
        assert_eq!(csv_text.lines().count(), 3);
        assert_eq!(export.exported, 2);
        assert!(!export.truncated);
    }

    #[test]
    fn csv_export_ignores_the_callers_paging() {
        let store = store();
        for name in ["a", "b", "c"] {
            store.submit(draft(&format!("DELETE FROM {name}"))).expect("submit");
        }
        // 调用方带着列表页的分页参数来导出：不能只给它当前页，否则界面上写"共 3 条"而文件里只有 1 条。
        let export = store
            .export_csv(&RecordFilter {
                limit: Some(1),
                offset: Some(2),
                ..Default::default()
            })
            .expect("csv");
        assert_eq!(export.exported, 3);
        assert!(!export.truncated);
        assert!(export.csv.contains("DELETE FROM a") && export.csv.contains("DELETE FROM c"));
    }

    #[test]
    fn submitted_sql_is_stored_without_literal_values() {
        let store = store();
        let record = store
            .submit(draft("INSERT INTO creds (name, secret) VALUES ('admin', 'hunter2')"))
            .expect("submit");
        assert_eq!(record.sql, "INSERT INTO creds (name, secret) VALUES ('?', '?')");
        let (_, items) = store.list(&RecordFilter::default()).expect("list");
        assert!(!items[0].sql.contains("hunter2"));
        // 链算的是脱敏后的文本，所以校验依然通过。
        assert!(store.verify().expect("verify").ok);
    }
}
