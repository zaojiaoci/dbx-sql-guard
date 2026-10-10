//! 面向 DBX 内置 AI 的 MCP 工具层。宿主先请求 `mcp/tools` 拿到工具清单（含 JSON Schema，交给
//! 模型做函数调用），再对每次调用请求 `mcp/call`；本模块只负责这两件事，风险判定和落库仍然复用
//! `risk::analyze` 和 `GuardStore`——AI 看到的结论必须和工作台看到的完全一致，否则审批留痕失去意义。
//! 宿主用 `readOnlyHint` 决定工具能否不打扰用户直接执行：两个只读工具（分析、查审计）标为 true；
//! 写审计的 `sql_guard_record_review` 故意不标，这样每次调用都会走用户确认，AI 无法静默往留痕里塞记录。
//! 错误一律以 `Result<_, String>` 向上返回，由 main.rs 统一转成 JSON-RPC 错误。

use serde_json::{json, Value};

use crate::model::{DraftRecord, RecordFilter, RecordStatus};
use crate::risk::{analyze, check_recordable, redact_literals, AnalyzeContext};
use crate::store::GuardStore;

/// 工具清单。`description` 和 `inputSchema` 是给模型看的提示词，措辞直接影响模型会不会在恰当
/// 时机调用它，所以描述里明确写了 "never executes SQL"——避免模型误以为这个工具能替它跑语句。
pub fn tools() -> Value {
    json!({
        "tools": [
            {
                "name": "sql_guard_analyze",
                "description": "Rate the risk of one SQL script (destructive DDL, missing WHERE, sensitive columns, multi-statement scripts) against the locally stored rule set. Returns a risk level and findings; it never executes SQL.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sql": { "type": "string", "description": "The SQL script to review." },
                        "database": { "type": "string", "description": "Target database name, used for whitelist rules." },
                        "estimatedRows": { "type": "integer", "description": "Optional row estimate; escalates the level when it reaches the configured threshold." }
                    },
                    "required": ["sql"]
                },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "sql_guard_recent_audit",
                "description": "Read the most recent SQL guard audit records (who ran what, risk level, approval status).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "How many records to return, 1 to 50. Default 10." },
                        "riskLevel": { "type": "string", "enum": ["low", "medium", "high"], "description": "Only return records at this risk level." }
                    }
                },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "sql_guard_record_review",
                "description": "Store an audit record for a SQL script the agent intends to propose, so the review trail survives the session. The record starts pending and needs a human decision in the SQL Guard workbench. Truncated or comment-only SQL is refused: the audit chain is append-only, so send a complete statement.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sql": { "type": "string" },
                        "database": { "type": "string" },
                        "operator": { "type": "string", "description": "Who is asking; defaults to the DBX user name." }
                    },
                    "required": ["sql"]
                }
            }
        ]
    })
}

/// 执行一次工具调用。返回值遵循 MCP 的内容块格式（`content: [{type,text}]`），文本里放格式化过的
/// JSON 或一句人类可读的结论——模型读文本，不读结构体。
pub fn call(store: &dyn GuardStore, tool: &str, arguments: &Value) -> Result<Value, String> {
    // 工具的正常返回都走这个闭包，保证 isError 字段一定存在。
    let text = |value: String| {
        json!({
            "content": [{ "type": "text", "text": value }],
            "isError": false
        })
    };
    match tool {
        // 只评估，不写库。模型在给出危险建议前可以先自查一遍。
        "sql_guard_analyze" => {
            let sql = required_str(arguments, "sql")?;
            let rules = store.rules()?;
            // 探针里那条 SELECT 是从用户原文逐字节切出来的，谓词的字面量就在里面。这份输出会进模型
            // 服务商的上下文，所以落库用的那道脱敏在这里也必须过一遍；UI 走 guard/analyze，那条路保留可执行的原文。
            let mut report = analyze(
                &sql,
                &rules,
                &AnalyzeContext {
                    database: optional_str(arguments, "database").as_deref(),
                    estimated_rows: arguments
                        .get("estimatedRows")
                        .and_then(Value::as_i64),
                    // AI 这条路上没有元数据可用：模型侧的 MCP 调用不带 UI 那次 getTableMetadata 的结果，依赖元数据的规则在这里保持沉默。
                    ..Default::default()
                },
            );
            for statement in &mut report.statements {
                if let Some(probe) = &mut statement.plan_probe {
                    probe.sql = redact_literals(&probe.sql);
                }
            }
            Ok(text(serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?))
        }
        // 读最近的审计记录。limit 收敛到 1..=50：模型可能要求"给我全部"，但整库塞进上下文既慢又贵，
        // 50 条足够它判断"这类操作最近是否被批准过"。返回精简投影（不含 note、时间戳等），findings 只留 code。
        "sql_guard_recent_audit" => {
            let limit = arguments
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(10)
                .clamp(1, 50);
            let risk_level = optional_str(arguments, "riskLevel")
                .as_deref()
                .and_then(crate::model::Risk::parse);
            let (_, records) = store.list(&RecordFilter {
                risk_level,
                limit: Some(limit),
                ..Default::default()
            })?;
            let compact: Vec<Value> = records
                .iter()
                .map(|record| {
                    json!({
                        "recordId": record.record_id,
                        "sql": record.sql,
                        "riskLevel": record.risk_level.as_str(),
                        "status": record.status.as_str(),
                        "database": record.database_name,
                        "operator": record.operator,
                        "approver": record.approver,
                        "findings": record.findings.iter().map(|f| f.code.clone()).collect::<Vec<_>>()
                    })
                })
                .collect();
            Ok(text(serde_json::to_string_pretty(&json!({ "records": compact })).map_err(|e| e.to_string())?))
        }
        // 写一条待审批记录，让 AI 会话里的意图留下痕迹。与 guard/submit 同源：状态同样由引擎判定（需要审批
        // → pending，否则 autoApproved），模型无法自己指定 approved；SQL 在 store 层脱敏后才落库。
        "sql_guard_record_review" => {
            let sql = required_str(arguments, "sql")?;
            let rules = store.rules()?;
            let report = analyze(
                &sql,
                &rules,
                &AnalyzeContext {
                    database: optional_str(arguments, "database").as_deref(),
                    estimated_rows: None,
                    ..Default::default()
                },
            );
            // 和 guard/submit 同一道闸门：AI 生成的 SQL 更容易是半截的（模型被打断、或只输出了片段），而链是 append-only 的，写进去就删不掉。
            check_recordable(&report)?;
            let record = store.submit(DraftRecord {
                sql: sql.clone(),
                sql_hash: report.sql_hash.clone(),
                connection_id: None,
                database_name: optional_str(arguments, "database"),
                db_type: None,
                operator: optional_str(arguments, "operator")
                    .unwrap_or_else(unknown_operator),
                risk_level: report.risk_level,
                findings: report.findings,
                estimated_rows: None,
                status: if report.requires_approval {
                    RecordStatus::Pending
                } else {
                    RecordStatus::AutoApproved
                },
            })?;
            Ok(text(format!(
                "Recorded {} as {} (risk {}). Decide it in the DBX SQL Guard workbench.",
                record.record_id,
                record.status.as_str(),
                record.risk_level.as_str()
            )))
        }
        other => Err(format!("Unknown tool: {other}")),
    }
}

/// 缺省的操作人。宿主不会告诉我们"是谁在问"，所以退回操作系统账号，并尽量带上主机名组成
/// `user@host`：MCP 调用来自内置 AI，现场没有人可以问名字，而光有 `USER` 在多人共用一台机、
/// 或同名账号分散在多台机时是分不开的。主机名拿不到就退回单纯的账号名；账号也拿不到时写
/// "unknown" 而不是留空——空白的 operator 会让人以为字段丢了。注意这只是**上下文**不是认证：账号和主机名都来自本机环境，谁都能改。
pub fn unknown_operator() -> String {
    let account = ["USER", "USERNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty());
    match (account, crate::util::hostname()) {
        (Some(account), Some(host)) => format!("{account}@{host}"),
        (Some(account), None) => account,
        (None, Some(host)) => format!("unknown@{host}"),
        (None, None) => "unknown".to_string(),
    }
}

/// 必填字符串参数。长度上限复用 main.rs 的 `MAX_SQL_CHARS`，按字符数而不是字节数计，这样 AI 提交的
/// 中文脚本和工作台提交的行为一致；超长时和缺失同样报 "Missing or invalid"，对模型来说这两种都是"重新给一个"。
fn required_str(arguments: &Value, key: &str) -> Result<String, String> {
    optional_str(arguments, key)
        .filter(|value| !value.trim().is_empty() && value.chars().count() <= crate::MAX_SQL_CHARS)
        .ok_or_else(|| format!("Missing or invalid `{key}`"))
}

/// 可选字符串参数。这里不 trim：SQL 前后的空白无害，而 operator / database 的空格属于用户意图，保留原样交给 store 层。
fn optional_str(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RuleSet;
    use crate::store::SqliteStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn store() -> SqliteStore {
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "dbx-sql-guard-mcp-test-{}-{index}.db",
            std::process::id()
        ));
        SqliteStore::open(&path).expect("store opens")
    }

    fn first_text(value: &Value) -> String {
        value["content"][0]["text"]
            .as_str()
            .expect("text block")
            .to_string()
    }

    /// `guard/analyze` 必须把可执行的探针原文交给 UI（UI 要把它发给宿主的 explainPlan），而 `mcp/call`
    /// 的同一份报告要发给模型服务商——同一段代码不能既保留字面量又不泄漏，所以这里钉住两侧的行为。
    #[test]
    fn analyze_tool_never_hands_an_unredacted_literal_to_the_model() {
        let canary = "mcp_canary_value";
        let sql = format!("DELETE FROM orders WHERE note = '{canary}' AND id = 7");
        let text = first_text(&call(&store(), "sql_guard_analyze", &json!({ "sql": sql })).unwrap());
        assert!(!text.contains(canary), "probe text reached the model verbatim: {text}");
        // 探针本身还要留着，否则模型无从知道这个数字是怎么来的；basis 也在。
        assert!(text.contains("planProbe"), "probe shape lost: {text}");
        assert!(text.contains("'?'"), "probe was not redacted: {text}");
        // 数字刻意保留（和落库脱敏同一条规矩），否则影响范围又说不清了。
        assert!(text.contains("id = 7"), "numbers were over-redacted: {text}");
    }

    /// 同一份输入走 RPC 时，探针必须是能直接发给宿主的原文。
    #[test]
    fn guard_analyze_keeps_the_executable_probe_for_the_ui() {
        let sql = "UPDATE accounts SET balance = 0 WHERE id = 7";
        let report = analyze(sql, &RuleSet::default(), &AnalyzeContext::default());
        let probe = report.statements[0]
            .plan_probe
            .as_ref()
            .expect("probe for a single-table update");
        assert_eq!(probe.basis, "predicate");
        assert!(
            probe.sql.contains("id = 7"),
            "probe must stay byte-exact: {}",
            probe.sql
        );
    }
}

