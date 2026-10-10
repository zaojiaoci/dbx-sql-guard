//! 数据模型：所有跨边界（RPC / 数据库 / UI）流动的结构体都在这里。
//!
//! 两个约定贯穿全文件：
//! 1. `#[serde(rename_all = "camelCase")]`：UI 和 MCP 工具的字段名都是驼峰。
//! 2. `#[serde(default)]` 让调用方少传字段、旧库 JSON 缺字段都能读通——升级不需要迁移。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 风险等级，**声明顺序即大小顺序**：`Low < Medium < High`。`analyze()` 的 `level.max(…)`
/// 和 `requires_approval` 的等级比较都依赖这个派生顺序，所以变体不能重排，新等级要插在正确位置上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(&self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }

    pub fn parse(value: &str) -> Option<Risk> {
        match value {
            "low" => Some(Risk::Low),
            "medium" => Some(Risk::Medium),
            "high" => Some(Risk::High),
            _ => None,
        }
    }
}

/// 一条审计记录的生命周期状态。`AutoApproved` = 风险低于 `approval_from_level` 或命中白名单 / 只读放行，提交时自动通过。
/// 库里存 `as_str()`、读回用 `parse()`；`parse()` 返回 `None` 时 store 层兜底成 `Pending`（宁可让人再看一眼，也不默认放行）。
/// `decide()` 只接受 `Pending` → `Approved`/`Rejected`，已决定的记录不能改判。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordStatus {
    Pending,
    AutoApproved,
    Approved,
    Rejected,
}

impl RecordStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordStatus::Pending => "pending",
            RecordStatus::AutoApproved => "autoApproved",
            RecordStatus::Approved => "approved",
            RecordStatus::Rejected => "rejected",
        }
    }

    pub fn parse(value: &str) -> Option<RecordStatus> {
        match value {
            "pending" => Some(RecordStatus::Pending),
            "autoApproved" => Some(RecordStatus::AutoApproved),
            "approved" => Some(RecordStatus::Approved),
            "rejected" => Some(RecordStatus::Rejected),
            _ => None,
        }
    }
}

/// 一条风险命中，由 `analyze()` 逐条产出。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// 稳定的机器可读键（如 `noWhereDml`），UI 按它查字典做中英本地化：code 一旦发布就不能改名，否则老记录显示不出文案。
    pub code: String,
    pub risk: Risk,
    /// 英文兜底文案：CSV 导出和 MCP 工具直接展示原始报告，不经过 UI 的字典。
    pub message: String,
    /// 多语句脚本里标注命中在第几条（如 `#2`），单语句时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
}

/// 单条语句的摘要，供 UI 的"语句列表"表格使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementSummary {
    /// 在脚本中的序号，从 0 开始。
    pub index: usize,
    /// 语句类型，已转大写（`DELETE` / `DROP` / `SELECT` …）。
    pub kind: String,
    /// 语句作用的对象名（表 / 视图 / 索引…），解析不出来时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// 这条语句自身的最高风险，不等于整段脚本的 `risk_level`。
    pub risk: Risk,
    /// 可拿去问宿主 `explainPlan` 的只读语句，见 [`PlanProbe`]。由**引擎**而不是 UI 决定：只有分词器
    /// 知道顶层 `where` 在哪、目标表清单在哪，让 UI 用正则抠谓词等于把没验证过的 SQL 发给用户的数据库。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_probe: Option<PlanProbe>,
    /// 拿不出探针的原因码（`limit` / `multiTable` / `valuesOnly` …），有探针时省略；UI 用它拼人话。
    /// 存 `String` 而非引擎内部的 `&'static str`：后者让 `Deserialize` 要求 `'de: 'static`，`RiskReport` 就读不回来。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_probe_refused: Option<String>,
}

/// 一行**从原文切出来的只读 SQL**，加上「它凭什么等于影响行数」。刻意不带执行能力：它只是字符串，
/// 交给宿主自己的计划门去判，而门只收只读输入，所以引擎判断失误时最坏是一次被拒的调用，不是执行。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProbe {
    /// 发给 `explainPlan` 的 SELECT 原文，UI 必须**原样显示**它——这条语句是人没写过的改写，
    /// 如果审批人看到的行数来自一条看不见的改写，那数字就是凭空冒出来的，比没有数字更糟。
    pub sql: String,
    /// 探针和被审语句的关系，决定 UI 上的说法：`self`（原语句）/ `predicate`（谓词匹配的行数）/
    /// `wholeTable`（没有谓词，整张表）/ `source`（喂给 INSERT 的那些行）。
    pub basis: String,
}

/// 宿主 `getTableMetadata` 返回的一列，只保留引擎真正敢用的两个字段。刻意**不**接收 `dataType` /
/// `length` / `default`：类型名是各方言自己拼的字符串（`character varying(20)`、`int4`、`bigint`），拿它判定等于把方言知识写进引擎。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ColumnFact {
    pub name: String,
    /// `None` = 这个宿主没告诉我们可空性。**绝不能当成 false（非空）**，否则每个老宿主
    /// 都会让所有列看起来 NOT NULL，把无害的语句判成高危。
    pub nullable: Option<bool>,
}

/// 一张表的宿主元数据：UI 在分析前从 `getTableMetadata` 原样取来、随参数传入，插件不缓存、不落库、也不自己连数据库。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TableFacts {
    /// 元数据描述的是哪张表。语句的目标表和它不一致时下面所有判定整体跳过：拿 A 表的列清单判 B 表的语句只会造出假阳性。
    pub table: String,
    pub columns: Vec<ColumnFact>,
}

/// 宿主 plan API 给的事实，由 UI 随 `guard/analyze` / `guard/submit` 传入；和 `TableFacts` 一样
/// 是**可选的升级**：没有就照旧，有也只会往上报。
/// 刻意只收这几个字段——`host.plans:read` 拿到的是宿主自己拼的估算计划，计划树里 `Seq Scan` /
/// `Index Scan` 之类的节点名同样是方言产物，拿它们判定等于把方言知识写进一个不懂方言的引擎。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanFacts {
    /// `getPlanCapabilities.dbType`（如 `postgres`）。引擎**不**用它判定，只是给记录补上「这条语句对着哪种库审的」这个可追溯事实。
    pub db_type: Option<String>,
    /// 宿主已经知道时才填：它是连接的既有知识，插件从不替宿主连库。
    pub db_version: Option<String>,
    /// `supports.estimatedPlan`：false 时这个方言在 DBX 里没有估算计划这条路，`estimated_rows` 必须为空。
    pub supports_estimated_plan: bool,
    /// 从估算计划里**明确读到的**行数，真的读到才填：被截断的、非 JSON 的、根节点没有行数的计划都填
    /// None，因为猜出来的行数会直接变成一条 High 的审批要求。
    pub estimated_rows: Option<i64>,
    /// 这个数字是不是来自**插件自己改写的探针**而不是被审语句本身。
    /// `#[serde(default)]`：不带这个字段的载荷就是 0.2.x 的行为（数字直接来自用户写的那条只读语句）。
    /// 每个行数都该知道自己是怎么来的——把看不见的改写说成「宿主的计划」是假话。
    #[serde(default)]
    pub rewritten: bool,
}

/// `guard/analyze` 的返回值：整段脚本的评估结果。它是"纯计算结果"，不落库——只有
/// `guard/submit` 才把它转成 `AuditRecord` 写进 SQLite，`sql_hash` 是两段文本唯一的关联。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskReport {
    pub risk_level: Risk,
    /// 是否需要人工审批。白名单命中或"只读自动放行"开启且脚本全为只读时为 false。
    pub requires_approval: bool,
    pub whitelisted: bool,
    pub statement_count: usize,
    pub findings: Vec<Finding>,
    pub statements: Vec<StatementSummary>,
    /// 规范化后 SQL 的 SHA-256（大小写与空格已折叠，字面量内容保留）。注意它算的是**原始** SQL，
    /// 而库里存的 `sql` 字段是脱敏后的文本。
    pub sql_hash: String,
}

/// 数据库里的一行审计记录，也是 UI 审批列表和 CSV 导出的数据源。字段分两半：`digest` 之前是不再
/// 修改、算进哈希链的"不可变的一半"；`approver` / `note` / `decided_at_ms` / `status` 是审批时才填的
/// "决定的一半"，单独用 `decision_digest` 保护（后补的审批不会让整条链失效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRecord {
    pub record_id: String,
    /// **脱敏后**的 SQL：字面量值和注释内容都被替换成 `?`，避免审计库变成密码和身份证号的集中存放处。
    /// 原文只能用 `sql_hash` 去比对，无法还原。
    pub sql: String,
    pub sql_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_type: Option<String>,
    /// 提交人。由调用方（UI / MCP 工具）传入，sidecar 只在缺省时退回操作系统用户名，
    /// 因此它证明不了"到底是谁敲的"——这点在 docs/known-limits.md 里写明了。
    pub operator: String,
    pub risk_level: Risk,
    pub status: RecordStatus,
    pub findings: Vec<Finding>,
    /// 调用方给的行数估计。引擎自己从不执行 SQL，所以没有估计值时大扫描规则不会触发。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at_ms: Option<i64>,
    /// 上一条记录的 digest + 本条不可变字段的 SHA-256；改任何被覆盖的字段，`guard/verify` 重算就对不上。
    pub digest: String,
}

/// 列表查询条件，字段都可选，缺省即"不过滤"。`limit` / `offset` 由 store 层再收敛（1..=500），
/// UI 传什么都不至于一次拉爆内存；`keyword` 走参数绑定的 LIKE，不拼进 SQL 文本。
/// `database` / `operator` 是**作用域**维度：记录里这两个字段都是自报的标签、不是外键，所以只做
/// 大小写不敏感的等值匹配（见 store.rs 的 `build_filter`），空白值等同不过滤。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RecordFilter {
    pub risk_level: Option<Risk>,
    pub status: Option<RecordStatus>,
    pub keyword: Option<String>,
    pub database: Option<String>,
    pub operator: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// 一个分面取值及其命中记录数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetValue {
    pub value: String,
    pub count: i64,
}

/// 分面：审计页筛选下拉的候选值。不塞进 `Stats` 是因为分面要扫全表做 GROUP BY，统计条却每次刷新都要读。
/// 取值来自用户自报的标签、数量不可控，所以 store 层截断到 `MAX_FACET_VALUES` 并按计数倒序，保证最常用的值一定在。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facets {
    pub databases: Vec<FacetValue>,
    pub operators: Vec<FacetValue>,
}

/// 可调规则集，整体作为一个 JSON 文档存在 `meta` 表的 `rules` 键下。
/// `#[serde(default)]` 是升级的关键：旧库存的 JSON 缺新字段也能反序列化并取 `Default` 里的值，
/// 不需要数据迁移——因此 `Default` 给的值必须对旧数据是安全的。
/// 工作台手填的内容先过 main.rs 的 `sanitize_rules()` 再落库：条目数、单条长度、行数阈值都有上限，否则一次误填就能让每次分析都变慢。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RuleSet {
    /// DROP / TRUNCATE / RENAME：不可逆地删数据或删对象。
    pub detect_destructive_ddl: bool,
    /// ALTER / CREATE：改结构。`ALTER SYSTEM` 不算在这里，归到下面的实例控制。
    pub detect_schema_change: bool,
    /// UPDATE / DELETE 没写 WHERE：影响全表。
    pub detect_no_where_dml: bool,
    /// WHERE 1=1 / WHERE true：写了等于没写。
    pub detect_constant_where: bool,
    /// SELECT 既无 WHERE 也无 LIMIT：全表扫描。
    pub detect_unbounded_select: bool,
    /// 投影列命中 `sensitive_columns`：读了敏感字段。
    pub detect_sensitive_columns: bool,
    /// 一个脚本里塞了多条语句：出问题时难以界定影响范围。
    pub detect_multi_statement: bool,
    /// 实例控制（SHUTDOWN / KILL / FLUSH / ALTER SYSTEM）、服务端文件批量导入（LOAD DATA /
    /// COPY）、引擎看不进去的存储过程调用（CALL / EXEC / DO）：共同点是词法分析判断不了会动多少数据。
    pub detect_admin_commands: bool,
    /// 敏感列名单，按分隔符切词后整词匹配（见 risk.rs 的 `is_sensitive`）。
    pub sensitive_columns: Vec<String>,
    /// 库名白名单：命中即视为已授权，`requires_approval` 直接置 false。
    pub whitelist_databases: Vec<String>,
    /// 关键字白名单：SQL 文本里出现即放行。粒度很粗，只适合"某类脚本整体豁免"。
    pub whitelist_keywords: Vec<String>,
    /// 全为只读语句时自动放行，不产生待审批项。
    pub auto_allow_read_only: bool,
    /// 达到这个等级才开始需要人工审批，默认 High（Medium 只记录不打扰人）。
    pub approval_from_level: Risk,
    /// 调用方给的 `estimatedRows` 达到此值就升级为 largeAffectedRows。
    pub large_scan_row_threshold: i64,
    /// 允许做审批决定的人。**空列表 = 不限制**，谁都能批——这是默认值，因为单人使用时
    /// 根本没有第二个名字可填，一上来就限制会把插件锁死。匹配忽略大小写和首尾空格：名字是
    /// 用户自报的自由文本，`Alice` 和 `alice ` 分成两个人没有意义。
    pub approvers: Vec<String>,
    /// 按作用域（库名）单独指定的审批人名单：键是库名，值是该库允许审批的人。
    /// 命中某个库时**只看**这个库的名单、不退回全局 `approvers`——还要叠加全局名单的话
    /// "生产库只能由 DBA 批"这条规则就失去意义了。所以某库配空名单等于"没人能批这个库"，
    /// 这是有意的 fail-closed：审批是安全控制，配错时应该挡住而不是放行。
    /// 用 `BTreeMap` 而不是 `HashMap`：规则整体是存在 meta 表里的 JSON 文档，键序稳定
    /// 才能让"改了规则"和"只是重新序列化"在文本上区分得开。
    pub scope_approvers: BTreeMap<String, Vec<String>>,
    /// 审批人必须和提交人不是同一个人，默认开启：留痕的价值在于"另一个人看过"，自己批自己
    /// 只能证明流程走过了，证明不了评审发生过。单人使用时可以在规则页关掉。
    /// 注意它拦不住"两个人约好互相点批准"——名字是自报的，见 docs/review-model.md 的身份说明。
    pub require_distinct_approver: bool,
}

/// 首次启动时写入库的默认规则（检测项全开、只有 High 需人工审批），同时充当旧数据反序列化
/// 时的兜底，改动前要想想已有库会怎样。
impl Default for RuleSet {
    fn default() -> Self {
        Self {
            detect_destructive_ddl: true,
            detect_schema_change: true,
            detect_no_where_dml: true,
            detect_constant_where: true,
            detect_unbounded_select: true,
            detect_sensitive_columns: true,
            detect_multi_statement: true,
            detect_admin_commands: true,
            sensitive_columns: DEFAULT_SENSITIVE_COLUMNS.iter().map(|s| s.to_string()).collect(),
            whitelist_databases: Vec::new(),
            whitelist_keywords: Vec::new(),
            auto_allow_read_only: true,
            approval_from_level: Risk::High,
            large_scan_row_threshold: 100_000,
            approvers: Vec::new(),
            scope_approvers: BTreeMap::new(),
            require_distinct_approver: true,
        }
    }
}

/// 内置敏感列名单，`RuleSet::default()` 从这里取值。多词条目（如 `id_card`）必须整名匹配，
/// 不会退化成匹配单个 `id` 列。
pub const DEFAULT_SENSITIVE_COLUMNS: &[&str] = &[
    "phone",
    "mobile",
    "tel",
    "id_card",
    "idcard",
    "identity",
    "passport",
    "ssn",
    "email",
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "credit_card",
    "bank_account",
    "address",
    "birth_date",
];

/// 工作台顶部的统计条。`approved` 把人工批准和自动放行合并计数，因为对"还有多少事要人处理"来说两者是一回事。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub total: i64,
    pub pending: i64,
    pub approved: i64,
    pub rejected: i64,
    pub high_risk: i64,
}

/// CSV 导出结果。`exported` 是真正写进文件的行数，`truncated` 表示有记录超出单次导出上限没被包含。
/// 审计导出宁可明确告知截断，也不能让人以为拿到的是全量——只报"共 N 条"却给了半个文件，
/// 是最难被发现的一种数据丢失。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CsvExport {
    pub csv: String,
    pub exported: i64,
    pub truncated: bool,
}

/// 重算整条防篡改链的结果（`guard/verify`）。`ok` 为 false 有两种独立原因，分开报是因为处置方式不同：
/// `first_broken_record` 指向内容或链接被改过的第一行（改 SQL、删行都会触发），
/// `decision_mismatches` 是审批信息被单独篡改的记录 ID 列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    pub ok: bool,
    /// 实际校验的行数。整库文件被删掉时这里是 0 而 `ok` 仍为 true——
    /// 链能证明"已有的行没被改"，证明不了"文件还在"。
    pub checked: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_broken_record: Option<String>,
    pub decision_mismatches: Vec<String>,
}

/// 新建审计记录的输入，`guard/submit` 和 MCP 的 `sql_guard_record_review` 都构造它。
/// 不含 `record_id` / `created_at_ms` / `digest`：这三样由 store 层生成，调用方无法指定，否则就能伪造时间和 ID。
#[derive(Debug, Clone)]
pub struct DraftRecord {
    pub sql: String,
    pub sql_hash: String,
    pub connection_id: Option<String>,
    pub database_name: Option<String>,
    pub db_type: Option<String>,
    pub operator: String,
    pub risk_level: Risk,
    pub findings: Vec<Finding>,
    pub estimated_rows: Option<i64>,
    pub status: RecordStatus,
}
