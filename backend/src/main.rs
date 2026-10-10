//! sidecar 进程入口：把宿主的 JSON-RPC 请求分发到风险引擎和存储层。
//! 运行方式：DBX 宿主按 manifest 启动本二进制，用 stdin/stdout 上的 JSON Lines 通信
//! （每行一个 JSON-RPC 2.0 消息，单条上限 8 MiB）。进程常驻、一次启动服务整个插件生命周期，
//! 所以这里的所有状态都必须能长期存活。
//! 模块：model 跨边界数据结构、risk 离线词法风险引擎（纯函数、不碰数据库）、
//! store SQLite 存储 + 哈希链（唯一有副作用的模块）、mcp 给宿主 AI 的三个工具、util 哈希与时间戳。
//! 请求由 SDK 的 worker 线程池（2~16 线程）并发派发，`handle()` 可能在多个线程上同时执行，
//! 因此 `Guard` 必须是 `Sync`，存储层靠一把 Mutex 串行化。

mod mcp;
mod model;
mod risk;
mod store;
mod util;

use std::sync::Arc;

use dbx_plugin_sdk::{
    PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, RequestContext,
};
use serde_json::{json, Value};

use model::{ColumnFact, DraftRecord, PlanFacts, RecordFilter, RecordStatus, RuleSet, TableFacts};
use risk::{analyze, check_recordable, redact_literals, AnalyzeContext};
use store::{GuardStore, SqliteStore};
use util::format_ms_utc;

const PLUGIN_ID: &str = "io.github.zaojiaoci.sql-guard";
/// 单次审查接受的 SQL 上限。词法分析是 O(n) 但常数不小，白名单关键字匹配还要对全文反复扫描；
/// 硬上限既能挡住误传的整库 dump，也让最坏耗时可控。超限时返回 -32602（参数错误），不是内部错误。
const MAX_SQL_CHARS: usize = 200_000;

/// handler 本体。SDK 要求它 `Send + Sync` 并被 Arc 共享给所有 worker 线程，所以内部只放一个
/// `Arc<dyn GuardStore>`：换存储实现只需改 main() 里的构造，其余不动。
pub struct Guard {
    store: Arc<dyn GuardStore>,
}

impl PluginHandler for Guard {
    fn handle(
        &self,
        _context: RequestContext,
        method: &str,
        params: Value,
        _emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        // SDK 在 worker 线程池里直接调用 handle()，而且不做 catch_unwind：这里一旦 panic，
        // 当前 worker 线程会永久消失（池子不会补线程），本次请求也永远不会写回响应，宿主只能
        // 干等到 invoke 超时；panic 次数达到线程数后整个插件会静默失去响应而进程还活着，比崩溃
        // 更难排查。所以在此兜住展开，把 panic 转成正常的协议错误。panic 信息仍由默认 hook
        // 打到 stderr，开发宿主的 diagnostics 接口能看到。
        // AssertUnwindSafe 是必要的：&self 里含 Mutex，不是 UnwindSafe。可接受，因为 store 的
        // 锁被污染后会自行恢复（见 store.rs 的 lock()），且写操作都在事务里。
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(method, params)))
        {
            Ok(result) => result,
            Err(_) => Err(internal(format!(
                "internal panic while handling `{method}`; see the sidecar log"
            ))),
        }
    }
}

impl Guard {
    /// 全部 RPC 方法的分发表。返回 Err 会被 SDK 原样写成 JSON-RPC error 对象。
    fn dispatch(&self, method: &str, params: Value) -> Result<Value, PluginError> {
        match method {
            // 健康检查。UI 启动时先调它确认 sidecar 活着：版本进页脚那一行，数据路径只把
            // 折叠过的 `~` 版本放到审计页（见 store_path_display），原始路径留给复制。
            "guard/ping" => Ok(json!({
                "ok": true,
                "plugin": PLUGIN_ID,
                "version": env!("CARGO_PKG_VERSION"),
                "store": store_path_label(),
                "storeDisplay": store_path_display()
            })),
            // 只评估、不落库。UI 的"检查"按钮和 AI 的 sql_guard_analyze 都走这条路。
            "guard/analyze" => {
                let sql = required_sql(&params)?;
                let rules = self.rules()?;
                let report = analyze(&sql, &rules, &self.context_from(&params));
                let mut value = json_of(&report)?;
                // 额外带一份脱敏文本给 UI 的 AI 预审用。prompt 里只能出现脱敏后的 SQL：原始字面量可能含
                // 口令，发给模型服务商等于交给第三方，违背"审计库不当密码本"的同一条原则。放在 RPC 层而
                // 不是 RiskReport 里，因为引擎的输出是纯计算结果，不该关心下游怎么消费。
                if let Value::Object(map) = &mut value {
                    map.insert("redactedSql".into(), Value::String(redact_literals(&sql)));
                }
                Ok(value)
            }
            // 评估 + 落库，一次调用完成。规则在同一个请求里读一次，保证"判定用的规则"和"写进记录的
            // 结论"一致；status 完全由引擎决定，调用方不能直接指定 approved，否则审批环节可以被绕过。
            "guard/submit" => {
                let sql = required_sql(&params)?;
                let rules = self.rules()?;
                let report = analyze(&sql, &rules, &self.context_from(&params));
                // 落库闸门，必须在 submit 之前：审计链只能追加，一条被截断的语句写进去就永远留在
                // 那里，还会占着一个待审批位。返回 -32602 而不是 -32000，因为这是调用方能自己修正
                // 的问题（把 SQL 写完），不是插件故障。
                check_recordable(&report).map_err(|reason| bad_request(&reason))?;
                // 记录里的方言和行数按「宿主优先」取：调用方可以只打 planFacts 而不带
                // `dbType`/`estimatedRows`，那种情况下这两个字段不该是空的——审计留痕要能回答
                // 「对着哪种库审的、影响多大」。
                let plan = plan_facts_from(&params);
                let record = self.store.submit(DraftRecord {
                    sql,
                    sql_hash: report.sql_hash.clone(),
                    connection_id: optional_string(&params, "connectionId"),
                    database_name: optional_string(&params, "database"),
                    db_type: optional_string(&params, "dbType")
                        .or_else(|| plan.as_ref().and_then(|facts| facts.db_type.clone())),
                    operator: optional_string(&params, "operator")
                        .unwrap_or_else(mcp::unknown_operator),
                    risk_level: report.risk_level,
                    findings: report.findings.clone(),
                    estimated_rows: integer_field(&params, "estimatedRows")
                        .or_else(|| plan.as_ref().and_then(|facts| facts.estimated_rows)),
                    status: if report.requires_approval {
                        RecordStatus::Pending
                    } else {
                        RecordStatus::AutoApproved
                    },
                }).map_err(internal)?;
                Ok(json!({ "record": record, "report": report }))
            }
            // 人工决定。`approver` 必填：留痕的意义就在于知道是谁批准的，所以这里不给默认值。
            "guard/decide" => {
                let record_id = required_string(&params, "recordId")?;
                let decision = match required_string(&params, "decision")?.as_str() {
                    "approved" => RecordStatus::Approved,
                    "rejected" => RecordStatus::Rejected,
                    other => {
                        return Err(bad_request(&format!(
                            "decision must be approved or rejected, got `{other}`"
                        )))
                    }
                };
                let approver = required_string(&params, "approver")?;
                let note = optional_string(&params, "note");
                let record = self
                    .store
                    .decide(&record_id, decision, &approver, note.as_deref())
                    // 两类失败映射到不同错误码：审批人不合格是调用方能自己修正的（换个人批、
                    // 或者去规则页改名单），报 -32000 会让人以为插件坏了。
                    .map_err(|error| match error {
                        store::DecideError::Rejected(message) => bad_request(&message),
                        store::DecideError::Store(message) => internal(message),
                    })?;
                Ok(json!({ "record": record }))
            }
            // total 是"符合过滤条件的总行数"，items 只是当前页；UI 的分页靠这两个值。
            "guard/records" => {
                let filter = filter_from(&params);
                let (total, records) = self.store.list(&filter).map_err(internal)?;
                Ok(json!({ "total": total, "items": records }))
            }
            "guard/stats" => json_of(&self.store.stats().map_err(internal)?),
            // 分面候选值：审计页「库」「操作人」两个下拉的内容。
            "guard/facets" => json_of(&self.store.facets().map_err(internal)?),
            // 重算整条哈希链，用来证明库文件没被人绕过插件直接改过。
            "guard/verify" => json_of(&self.store.verify().map_err(internal)?),
            "guard/rules/get" => json_of(&self.rules()?),
            // 规则来自用户手填的文本框，必须先过 sanitize_rules 再落库。
            "guard/rules/save" => {
                let rules: RuleSet = serde_json::from_value(params.get("rules").cloned().unwrap_or(Value::Null))
                    .map_err(|error| bad_request(&format!("Invalid rules: {error}")))?;
                json_of(&self.store.save_rules(&sanitize_rules(rules)?).map_err(internal)?)
            }
            // CSV 在服务端拼好整串再回传，由 UI 负责落地（fileTransfer → 剪贴板 → 弹窗兜底）。
            // 文件名里的日期取 UTC，避免跨时区导出时同名不同天。`total` 是符合条件的全部行数、
            // `exported` 是真正写进文件的行数，两者不同就说明被上限截断了，UI 必须讲清这个差别。
            "guard/export" => {
                let filter = filter_from(&params);
                let (total, _) = self.store.list(&filter).map_err(internal)?;
                let export = self.store.export_csv(&filter).map_err(internal)?;
                let day = format_ms_utc(crate::util::now_ms())[..10].to_string();
                Ok(json!({
                    "filename": format!("sql-guard-audit-{day}.csv"),
                    "total": total,
                    "exported": export.exported,
                    "truncated": export.truncated,
                    "csv": export.csv
                }))
            }
            // 服务于 DBX 内置 AI：实现都在 mcp.rs，和 UI 走的是同一套引擎与存储。
            "mcp/tools" => Ok(mcp::tools()),
            "mcp/call" => {
                let tool = required_string(&params, "tool")?;
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                mcp::call(self.store.as_ref(), &tool, &arguments).map_err(|message| bad_request(&message))
            }
            _ => Err(PluginError::method_not_found(method)),
        }
    }

    fn rules(&self) -> Result<RuleSet, PluginError> {
        self.store.rules().map_err(internal)
    }

    /// 构造引擎需要的上下文。`estimated_rows` 有两个来源：调用方自报、宿主估算计划，后者在引擎里
    /// 优先（见 `risk::analyze`），因为宿主的数字至少来自数据库自己的优化器，自报的可以是任何值。
    fn context_from<'a>(&self, params: &'a Value) -> AnalyzeContext<'a> {
        AnalyzeContext {
            database: match params.get("database") {
                Some(Value::String(name)) => Some(name.as_str()),
                _ => None,
            },
            estimated_rows: integer_field(params, "estimatedRows"),
            plan: plan_facts_from(params),
            table_facts: table_facts_from(params),
        }
    }
}

fn store_path_label() -> String {
    SqliteStore::default_path().display().to_string()
}

/// 界面上显示的路径：把 home 前缀折成 `~`。数据路径本身是要给人看的（备份、迁移、校验前
/// 总得知道动的是哪个文件），但绝对路径的头一截是 OS 用户名，而审计页那行是会被截图外发的
/// 位置。原始路径仍然通过 `store` 返回，复制或排查用。
fn store_path_display() -> String {
    let raw = store_path_label();
    for key in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(key) {
            let folded = fold_home(&raw, &home.to_string_lossy());
            if folded != raw {
                return folded;
            }
        }
    }
    raw
}

/// 只在 home 恰好是一段目录边界时才算命中：`/home/x` 不该把 `/home/xavier/…` 折成 `~avier/…`。
/// home 为空时原样返回——空前缀能匹配任何路径，不挡住就会把一切都折成 `~/…`。
fn fold_home(raw: &str, home: &str) -> String {
    let bare = home.trim_end_matches(['/', '\\']);
    if bare.is_empty() {
        return raw.to_string();
    }
    match raw.strip_prefix(bare) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            format!("~{rest}")
        }
        _ => raw.to_string(),
    }
}

/// 结构体 → JSON。失败属于内部错误：结构体是我们自己定义的，只有字段里出现非法内容（例如
/// 非 UTF-8 的 map 键）才会失败。
fn json_of<T: serde::Serialize>(value: &T) -> Result<Value, PluginError> {
    serde_json::to_value(value).map_err(|error| internal(error.to_string()))
}

/// -32000：插件自身出了问题（数据库、序列化）。UI 应当把它当故障提示。
fn internal(message: String) -> PluginError {
    PluginError::new(-32000, message)
}

/// -32602：调用方传的参数不对。UI 应当把 message 原样展示给用户。
fn bad_request(message: &str) -> PluginError {
    PluginError::new(-32602, message.to_string())
}

/// 取出并校验 `sql` 参数：去空白、非空、不超上限。长度按 `chars()` 而不是字节数计，这样中文注释多的脚本不会被提前拒掉。
fn required_sql(params: &Value) -> Result<String, PluginError> {
    let sql = params
        .get("sql")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if sql.is_empty() {
        return Err(bad_request("`sql` is required"));
    }
    if sql.chars().count() > MAX_SQL_CHARS {
        return Err(bad_request("`sql` is larger than the 200000 character review limit"));
    }
    Ok(sql)
}

/// 必填字符串参数：缺失、非字符串、或只有空白都算没传。
fn required_string(params: &Value, key: &str) -> Result<String, PluginError> {
    optional_string(params, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request(&format!("`{key}` is required")))
}

/// 可选字符串参数：统一 trim，空白串视为"没传"，这样 JSON 里不会出现空字段。
fn optional_string(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 可选非负整数。负数和浮点都丢弃（返回 None）而不是报错：行数估计本来就只是提示，传坏了当没传更合适。
fn integer_field(params: &Value, key: &str) -> Option<i64> {
    params.get(key).and_then(Value::as_i64).filter(|value| *value >= 0)
}

/// 列清单的规模上限。数据由 UI 从宿主原样转发，但 UI 不是可信边界（它可以被换成别的实现），而比对
/// 代价是「语句里的名字 × 列数」。2000 高于主流数据库的单表列数硬上限，所以真被卡住的只可能是乱填。
const MAX_FACT_COLUMNS: usize = 2000;

/// 取出宿主元数据。任何不合形的输入都退回 `None`，也就是「这次没有元数据」。这里刻意不做部分
/// 接受：截断的列清单会让真实存在的列看起来「不存在」，于是造出假阳性的 `unknownColumn`。残缺不如没有。
fn table_facts_from(params: &Value) -> Option<TableFacts> {
    let raw = params.get("tableFacts")?;
    let facts: TableFacts = serde_json::from_value(raw.clone()).ok()?;
    if facts.table.trim().is_empty()
        || facts.table.chars().count() > 256
        || facts.columns.is_empty()
        || facts.columns.len() > MAX_FACT_COLUMNS
    {
        return None;
    }
    let columns: Vec<ColumnFact> = facts
        .columns
        .into_iter()
        .map(|column| ColumnFact { name: column.name.trim().to_string(), ..column })
        .filter(|column| !column.name.is_empty() && column.name.chars().count() <= 128)
        .collect();
    if columns.is_empty() {
        return None;
    }
    Some(TableFacts { table: facts.table.trim().to_string(), columns })
}

/// 字符串字段的上限。方言名和版本号都该是几个字符的东西，能装下几百字符的只可能是把计划文本或
/// 报错信息塞了进来，那种值记进审计里没有可读价值。
const MAX_PLAN_LABEL_CHARS: usize = 64;

/// 取出宿主 plan API 的事实，任何不合形都退回 `None`（「这次没有计划」）。两条刻意规则：
/// - `supportsEstimatedPlan` 为 false 却带着行数 = 自相矛盾的输入，**丢掉行数**。这个数字唯一的
///   用途就是把一条语句抬成 High 的审批要求，不能被连估算计划都声明不支持的载荷喂进来。
/// - 只剩空字段时返回 `None` 而不是全空的结构体，否则记录里会出现「宿主说过它不支持计划」这种
///   其实从没问过宿主的话。
fn plan_facts_from(params: &Value) -> Option<PlanFacts> {
    let raw = params.get("planFacts")?;
    let facts: PlanFacts = serde_json::from_value(raw.clone()).ok()?;
    let label = |value: Option<String>| {
        value
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty() && text.chars().count() <= MAX_PLAN_LABEL_CHARS)
    };
    let rows = facts.estimated_rows.filter(|value| *value >= 0);
    let rows = if facts.supports_estimated_plan { rows } else { None };
    let db_type = label(facts.db_type);
    let db_version = label(facts.db_version);
    if rows.is_none() && db_type.is_none() && db_version.is_none() {
        return None;
    }
    Some(PlanFacts {
        db_type,
        db_version,
        supports_estimated_plan: facts.supports_estimated_plan,
        estimated_rows: rows,
        // 如实透传：这个标志唯一的用处是让消息说明"数字来自插件改写的 SELECT"，它不改变任何等级
        // 判定，所以既不需要替调用方纠正，也不能由引擎猜出来——只有 UI 知道那次 explainPlan 发出去的是原句还是探针。
        rewritten: facts.rewritten,
    })
}

/// 解析过滤条件。任何解析失败都退回"不过滤"，因为列表查询宁可多显示也不要因为一个拼错的字段名
/// 就整页报错。limit/offset 的越界收敛在 store 层做。
fn filter_from(params: &Value) -> RecordFilter {
    params
        .get("filter")
        .cloned()
        .and_then(|value| serde_json::from_value::<RecordFilter>(value).ok())
        .unwrap_or_default()
}

const MAX_RULE_ENTRIES: usize = 500;
const MAX_RULE_ENTRY_CHARS: usize = 128;
const ROW_THRESHOLD_RANGE: (i64, i64) = (1, 1_000_000_000);

/// 校验用户手填的规则集，然后才允许落库。规则来自工作台的文本框，等于外部输入，必须校验而不是信任：
/// - 行数阈值为 0 会让每条语句都命中大扫描，审批队列瞬间被垃圾淹没；
/// - 白名单没有上限时每次分析都要对全文反复扫描这些关键字，一次误粘贴就能让所有请求变慢（这是本机
///   可复现的拒绝服务）。条目数超限直接报错，单条过长则静默截断——表名不可能有 128 个字符。
fn sanitize_rules(mut rules: RuleSet) -> Result<RuleSet, PluginError> {
    let (min, max) = ROW_THRESHOLD_RANGE;
    if !(min..=max).contains(&rules.large_scan_row_threshold) {
        return Err(bad_request(&format!(
            "`largeScanRowThreshold` must be between {min} and {max}"
        )));
    }
    for list in [
        &mut rules.sensitive_columns,
        &mut rules.whitelist_databases,
        &mut rules.whitelist_keywords,
        &mut rules.approvers,
    ] {
        if list.len() > MAX_RULE_ENTRIES {
            return Err(bad_request(&format!(
                "rule lists accept at most {MAX_RULE_ENTRIES} entries"
            )));
        }
        *list = list
            .iter()
            .map(|entry| entry.trim().chars().take(MAX_RULE_ENTRY_CHARS).collect::<String>())
            .filter(|entry| !entry.is_empty())
            .collect();
    }
    // 按库分组的审批人名单：键和值都是用户手填的，同样要收敛。键归一化成小写再存，
    // authorize_approver 比对时也做归一化，两边一致才不会"配了 PROD 却按 prod 查"。
    if rules.scope_approvers.len() > MAX_RULE_ENTRIES {
        return Err(bad_request(&format!(
            "at most {MAX_RULE_ENTRIES} scopes can have their own approver list"
        )));
    }
    let mut scoped: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for (scope, names) in std::mem::take(&mut rules.scope_approvers) {
        let key: String = scope.trim().chars().take(MAX_RULE_ENTRY_CHARS).collect();
        // 空键等于"没有库名的记录"，那种情况本来就走全局名单，留着只会让人困惑。
        if key.is_empty() {
            continue;
        }
        // 超长的名单必须报错而不是丢掉这个 scope：丢掉就等于该库退回全局名单，那是**放宽**限制。
        // 审批配置上任何"配错了"的结果都只能是更严，不能更松。
        if names.len() > MAX_RULE_ENTRIES {
            return Err(bad_request(&format!(
                "the approver list for `{key}` exceeds {MAX_RULE_ENTRIES} entries"
            )));
        }
        let cleaned: Vec<String> = names
            .iter()
            .map(|name| name.trim().chars().take(MAX_RULE_ENTRY_CHARS).collect::<String>())
            .filter(|name| !name.is_empty())
            .collect();
        // 名单被清空后仍保留这个键：空名单 = "没人能批这个库"，和"没有单独配置"是两回事，不能悄悄退化。
        scoped.insert(key.to_lowercase(), cleaned);
    }
    rules.scope_approvers = scoped;
    Ok(rules)
}

/// 进程入口：打开数据库 → 声明能力 → 进入 SDK 的服务循环（阻塞到 stdin 关闭）。
/// 数据库打不开时直接退出码 1 并把原因写到 stderr：宿主会显示成插件启动失败，比带着一个坏掉的
/// store 继续跑、在每个请求上返回错误要好排查得多。stderr 只用于诊断，stdout 是协议通道，调试输出不能写进去。
fn main() -> std::io::Result<()> {
    let path = SqliteStore::default_path();
    let store = SqliteStore::open(&path).unwrap_or_else(|error| {
        eprintln!("[dbx-sql-guard] {error}");
        std::process::exit(1);
    });
    let metadata = PluginMetadata::new(PLUGIN_ID, env!("CARGO_PKG_VERSION"))
        .with_capability("guard-analyze")
        .with_capability("guard-audit")
        .with_capability("mcp-tools");
    PluginServer::new(
        metadata,
        Guard {
            store: Arc::new(store),
        },
    )
    .serve()
}

#[cfg(test)]
mod tests {
    use super::fold_home;

    #[test]
    fn home_prefix_becomes_tilde() {
        assert_eq!(
            fold_home("/home/alex/.local/share/x/sql_guard.db", "/home/alex"),
            "~/.local/share/x/sql_guard.db"
        );
    }

    #[test]
    fn a_trailing_separator_in_home_does_not_double_up() {
        assert_eq!(fold_home("/home/alex/x", "/home/alex/"), "~/x");
    }

    #[test]
    fn a_sibling_sharing_the_prefix_is_not_folded() {
        assert_eq!(
            fold_home("/home/alexander/x", "/home/alex"),
            "/home/alexander/x"
        );
    }

    #[test]
    fn a_windows_profile_folds_on_backslashes_too() {
        assert_eq!(
            fold_home(
                "C:\\Users\\alex\\AppData\\x\\sql_guard.db",
                "C:\\Users\\alex"
            ),
            "~\\AppData\\x\\sql_guard.db"
        );
    }

    #[test]
    fn an_empty_home_leaves_the_path_alone() {
        assert_eq!(
            fold_home("/tmp/dbx-sql-guard/sql_guard.db", ""),
            "/tmp/dbx-sql-guard/sql_guard.db"
        );
    }
}
