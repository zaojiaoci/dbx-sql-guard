//! 本地离线 SQL 风险引擎：分词器 + 规则检测器。
//!
//! ## 设计取舍
//!
//! 这是**词法**分类器，不是任何方言的语法解析器：要同时支持 MySQL / PostgreSQL /
//! SQLite / SQL Server，真解析器既做不到也不该由这个插件做；要回答的问题很窄——
//! "这条语句值不值得留下审批痕迹"，不是"它能否执行"。所以判断偏保守：看不懂的结构
//! 宁可评高（多打扰一次），也不要评低（漏掉不可逆操作）；误报是可接受的代价，
//! 白名单机制就是给用户关掉误报用的。
//!
//! ## 处理流水线
//!
//! `analyze()` 是唯一入口，内部顺序：`tokenize`（文本 → token）→
//! `split_statements_paired`（按顶层分号切分）→ 每条语句 `parse`（kind/target/有无 WHERE
//! 等结构信息）→ 逐个检测器 → 汇总最高风险等级 → 白名单和只读放行覆盖审批要求。
//! 全程纯函数、不碰数据库、不执行 SQL，可安全地在多个 worker 线程上并发调用。

use crate::model::{
    Finding, PlanFacts, PlanProbe, Risk, RiskReport, RuleSet, StatementSummary, TableFacts,
};
use crate::util::sha256_hex;

/// 词法单元：只标"这是什么类型的东西"，不理解语法。
/// `Word`（裸词/关键字）与 `Name`（引号标识符）都转小写以便不敏感比较，但引号标识符可能是保留字当表名用，检测器要区别对待。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// 裸词或关键字，已转小写。
    Word(String),
    /// 带引号的标识符，内部文本已转小写。
    Name(String),
    /// 字符串字面量，内容原样保留（`normalize_sql` 要把它算进 sqlHash）。
    Str(String),
    /// 数字字面量，原样保留文本（`1.0` 和 `1` 视为不同）。
    Num(String),
    /// 标点：单字符或双字符运算符、括号、分号等。
    Punct(String),
}

/// 标识符首字符。`@` 也算，因为 SQL Server 的变量和 MySQL 的用户变量以它开头。
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '@'
}

/// 标识符后续字符。`> 127` 是为了让中文表名、列名被完整识别成一个词，否则 `删除用户` 会被拆成四个单字符 token。
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '@' || (c as u32) > 127
}

/// 把 SQL 文本切成 token 序列（单遍线性扫描、无递归，嵌套再深也不会爆栈）。几处刻意选择：
/// - `/* */` 未闭合时用 `(i + 2).min(len)` 兜底，保证下标不越界；引号标识符（`"` / `` ` `` / `[`）同样容忍未闭合；
/// - 未闭合的字符串把剩余输入全部吃掉——宁可少几个 token，也不要把引号里的内容当成 SQL 关键字去判定风险；
/// - 转义支持反斜杠（MySQL）和连续两个单引号（标准 SQL）两种；
/// - `->>` 是三字符，所以在 `->` 分支里多看一个字符。
pub fn tokenize(sql: &str) -> Vec<Tok> {
    tokenize_spans(sql).into_iter().map(|(tok, _)| tok).collect()
}

/// 词法分析，并给出每个 token 在原文里的**字节**区间 `[start, end)`。
/// 存在的理由：分词器把裸词和引号标识符都转了小写，所以**不能拿 token 反着拼出一条 SQL**——`"myTable"` 会碎成 `mytable`，
/// 而 MySQL 在 Linux 下表名分大小写，拼出来的那条 SELECT 查的就是另一张表。凡是要回原文取文本的地方（`plan_probe`）都用区间切。
pub fn tokenize_spans(sql: &str) -> Vec<Spanned> {
    let chars: Vec<char> = sql.chars().collect();
    // 字符下标 → 字节下标；末尾多一格让「最后一个 token 的右边界」和「整个输入的结尾」用同一个式子取。
    let mut bytes: Vec<usize> = sql.char_indices().map(|(offset, _)| offset).collect();
    bytes.push(sql.len());
    // 未闭合的引号标识符会把 `i` 推到 `chars.len() + 1`，切区间时统一夹住。
    let end_at = |i: usize| bytes[i.min(chars.len())];
    let mut out = Vec::new();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if (c == '-' && chars.get(i + 1) == Some(&'-')) || c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        if c == '\'' {
            i += 1;
            let mut raw = String::new();
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        raw.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                raw.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Str(raw));
            spans.push((bytes[start], end_at(i)));
            continue;
        }
        if c == '"' || c == '`' || c == '[' {
            let close = match c {
                '"' => '"',
                '`' => '`',
                _ => ']',
            };
            i += 1;
            let mut raw = String::new();
            while i < chars.len() && chars[i] != close {
                raw.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Name(raw.to_lowercase()));
            spans.push((bytes[start], end_at(i)));
            continue;
        }
        if c.is_ascii_digit() {
            let mut raw = String::new();
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '_') {
                raw.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Num(raw));
            spans.push((bytes[start], end_at(i)));
            continue;
        }
        if is_ident_start(c) {
            let mut raw = String::new();
            while i < chars.len() && is_ident_char(chars[i]) {
                raw.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Word(raw.to_lowercase()));
            spans.push((bytes[start], end_at(i)));
            continue;
        }
        // `chars.len() - i >= 2` 保证下面 `i..i+2` 不越界（此处 i < len 恒成立，减法不会下溢）。
        if chars.len() - i >= 2 {
            let two: String = chars[i..i + 2].iter().collect();
            if matches!(
                two.as_str(),
                "<=" | ">=" | "<>" | "!=" | "||" | "::" | "->" | "=>" | "&&" | ":="
            ) {
                if two == "->" && chars.get(i + 2) == Some(&'>') {
                    out.push(Tok::Punct("->>".to_string()));
                    i += 3;
                    spans.push((bytes[start], end_at(i)));
                    continue;
                }
                out.push(Tok::Punct(two));
                i += 2;
                spans.push((bytes[start], end_at(i)));
                continue;
            }
        }
        out.push(Tok::Punct(c.to_string()));
        i += 1;
        spans.push((bytes[start], end_at(i)));
    }
    out.into_iter().zip(spans).collect()
}

fn word(tok: &Tok) -> Option<&str> {
    match tok {
        Tok::Word(w) => Some(w.as_str()),
        _ => None,
    }
}

/// 计算 `sqlHash` 用的规范形式：token 用单个空格重拼，所以大小写、缩进、换行、多余空格
/// 全部折叠——`DELETE FROM t` 和 `delete   from\n  T` 同哈希，这正是审计要的。
/// 字面量内容**保留**：`WHERE id=1` 和 `WHERE id=2` 是不同的操作，不能算成同一条。
/// 它算的是原始 SQL；库里存的 `sql` 列是脱敏后的文本，两者不能互推。
pub fn normalize_sql(sql: &str) -> String {
    tokenize(sql)
        .iter()
        .map(|tok| match tok {
            Tok::Word(name) | Tok::Name(name) => name.clone(),
            Tok::Str(literal) => format!("'{literal}'"),
            Tok::Num(number) => number.clone(),
            Tok::Punct(symbol) => symbol.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_punct(tok: &Tok, text: &str) -> bool {
    matches!(tok, Tok::Punct(p) if p == text)
}

/// 把字符串字面量和注释内容替换成 `?`，保留标识符（它们指库对象名，不是值）、**数字**、标点和排版。
/// 审计记录存的是这个结果，不是原文：一条 `INSERT INTO creds VALUES ('admin','hunter2')` 如果被
/// 原样记下来，审计库就成了密码的集中存放处，而它的保护级别通常远低于业务库。脱敏之后仍然能看出
/// "往 creds 表插了两列值"，足够审查用。
/// **数字刻意不脱敏**。早先的版本把 `WHERE id = 2` 也写成 `WHERE id = ?`，结果审查者连"这是改一行
/// 还是改一百万行"都判断不了，AI 预审同理——被拿掉的正好是唯一能说明影响范围的信息。数字确实可能
/// 是敏感的（账号、手机号），但和"审批人看不懂就没法审"相比，这个代价更小；要更细的粒度就得让脱敏
/// 变成规则项，那是后面的事。
/// 与 `tokenize` 不同，这里必须**逐字符重建输出**而不是重新拼接 token——原有的空格、换行和缩进要
/// 保留，否则审查者读到的 SQL 会挤成一行。扫描逻辑因此和分词器平行但独立：两边对"什么算字符串/
/// 注释"的判断必须一致，改一边就要检查另一边。
pub fn redact_literals(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if (c == '-' && chars.get(i + 1) == Some(&'-')) || c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push_str("-- ?");
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            out.push_str("/* ? */");
            continue;
        }
        if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str("'?'");
            continue;
        }
        // 引号标识符指的是库对象名，不是值：原样保留，审查时才读得懂。
        if c == '"' || c == '`' || c == '[' {
            let close = match c {
                '"' => '"',
                '`' => '`',
                _ => ']',
            };
            out.push(c);
            i += 1;
            while i < chars.len() && chars[i] != close {
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                out.push(close);
                i += 1;
            }
            continue;
        }
        // 数字原样保留（见函数文档）：影响范围就写在数字里。
        out.push(c);
        i += 1;
    }
    out
}

/// 按顶层分号把 token 流切成多条语句，这样脚本能给出逐条的风险。
/// `depth` 跟踪括号层级：只有 depth ≤ 0 的分号才算语句结束，否则 `INSERT INTO t SELECT ... WHERE x IN (1;2)` 这种
/// （虽然不合法）以及函数体里的分号会把语句切碎；用 `<= 0` 而不是 `== 0`，是为了容忍多余的右括号（`SELECT 1)`）不至于
/// 让后面全部合并成一条。末尾没有分号的最后一条也要收；空语句直接丢弃。
/// 括号**不进入**检测器用的那条流：这是历史行为，也是多个检测器的前提（见 `projects_whole_table`），改动它等于改了所有判定的语句形状。
/// 但 `plan_probe` 必须要括号：它要在 `UPDATE … SET … WHERE p` 里认出那个**顶层** `where`，而 `p` 里的子查询也写着 `WHERE`，
/// 少了层级就分不出这两个，探针会把子查询的谓词当成整条语句的条件。所以两条流必须**成对**返回，而不是各切各的：`();` 这种分组
/// 去掉括号就空了，得整组丢掉——两条流水线分别过滤再按下标配对的话，`statements[i]` 和 `spans[i]` 会错位，探针拿到邻居的语句，
/// 那比没有探针糟得多。复用同一个 `split_on` 也是刻意的：一份按 token 内容切、另一份按下标切，迟早会在某个边界情况上分叉。
/// 一个 token 和它在原文里的字节区间。
type Spanned = (Tok, (usize, usize));
/// 一条语句的两条形流：检测器用的（括号已丢）和探针用的（保留括号、带区间）。
type StatementPair = (Vec<Tok>, Vec<Spanned>);

pub fn split_statements_paired(items: &[Spanned]) -> Vec<StatementPair> {
    split_on(items, |(tok, _)| (classify(tok), depth_of(tok)))
        .into_iter()
        // `();` 这种分组滤掉括号就成了空语句，而原实现根本不会产出它（括号从来不进 `current`，分号时 `current.is_empty()` 直接跳过），所以要连着空组一起丢。
        .map(|group| {
            let plain: Vec<Tok> = group
                .iter()
                .filter(|(tok, _)| !is_bracket(tok))
                .map(|(tok, _)| tok.clone())
                .collect();
            (plain, group)
        })
        .filter(|(plain, _)| !plain.is_empty())
        .collect()
}

fn is_bracket(tok: &Tok) -> bool {
    matches!(tok, Tok::Punct(p) if p == "(" || p == ")")
}

/// 切分时对一个 token 的处置。
enum Split {
    /// 候选的语句结束点。只有顶层（`depth <= 0`）才算，见 `split_on`。
    Break,
    Keep,
}

fn depth_of(tok: &Tok) -> i32 {
    match tok {
        Tok::Punct(p) if p == "(" => 1,
        Tok::Punct(p) if p == ")" => -1,
        _ => 0,
    }
}

fn classify(tok: &Tok) -> Split {
    match tok {
        Tok::Punct(p) if p == ";" => Split::Break,
        _ => Split::Keep,
    }
}

/// 切分的唯一实现。`classify` 同时给出「处置」和「层级变动」：分号看的是**它之前**的层级（原实现的匹配守卫
/// `";" && depth <= 0` 就是这个意思），所以层级变动必须排在处置之后，否则 `(SELECT 1);` 的后半会被错切。
fn split_on<T: Clone, F>(items: &[T], mut classify: F) -> Vec<Vec<T>>
where
    F: FnMut(&T) -> (Split, i32),
{
    let mut out: Vec<Vec<T>> = Vec::new();
    let mut current: Vec<T> = Vec::new();
    let mut depth = 0i32;
    for item in items {
        let (action, delta) = classify(item);
        match action {
            Split::Break if depth <= 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            Split::Break => current.push(item.clone()),
            Split::Keep => current.push(item.clone()),
        }
        depth += delta;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

// 下面几组常量是检测器的"词表"：判断语句类型只看第一个词（`resolve_kind` 的结果），
// 所以它们必须是能出现在语句开头的关键字。全部小写，因为分词器已经统一转小写了。

/// 只读语句。全部语句都属于这一组时，可以按 `auto_allow_read_only` 直接放行。
const READ_ONLY_KINDS: &[&str] = &["select", "show", "desc", "describe", "explain"];
/// 破坏性 DDL：不可逆地删数据或删对象，一律 High。
const DESTRUCTIVE_DDL: &[&str] = &["drop", "truncate", "rename"];
/// 改结构的 DDL：Medium。会锁表、会让依赖它的视图/存储过程失效，但不直接删数据。
const SCHEMA_DDL: &[&str] = &["alter", "create"];
/// 写数据的 DML。这组决定"没有 WHERE"和"WHERE 恒真"两个检测器是否适用。
const WRITER_KINDS: &[&str] = &["delete", "update", "insert", "merge"];
/// 直接作用于数据库服务器本身：进程状态、会话、缓存，动的不是某张表的行。
const INSTANCE_CONTROL: &[&str] = &["shutdown", "kill", "flush"];
/// 读写服务器端文件的批量导入导出，影响行数在结构上就是无界的。
const BULK_LOAD: &[&str] = &["load", "copy"];
/// 调用存储过程/匿名块。过程体在数据库里，词法引擎看不到它会做什么，只能整体评 High——"看不懂就评高"最典型的体现。
const OPAQUE_CALL: &[&str] = &["call", "exec", "execute", "do"];

/// 语句**结尾**停在悬空的子句关键字上时，几乎肯定是被人从中间截断的半成品（`DELETE FROM`、`UPDATE t SET`、
/// `SELECT * FROM t WHERE`、`DROP TABLE`）。只列"合法语句绝不可能以它收尾"的词，这是误报的唯一防线——多写一个词就会
/// 有一类正常语句被拒之门外：所以刻意不含 `end`（`SET x = CASE … END` 是完整的），也不含 `asc` / `desc`（`ORDER BY a DESC` 是完整的）。
/// 只认 `Tok::Word`，不认 `Tok::Name`：`DELETE FROM "on"` 里的 `"on"` 是表名而不是关键字。
const DANGLING_TAIL: &[&str] = &[
    "select", "from", "where", "set", "into", "values", "value", "table", "join", "inner", "left",
    "right", "full", "outer", "cross", "on", "and", "or", "by", "as", "using", "group", "order",
    "having", "limit", "offset", "union", "all", "distinct", "like", "between", "in", "not",
    "insert", "update", "delete", "drop", "truncate", "alter", "create", "rename", "case", "when",
    "then", "else", "over", "partition",
];

/// 这条语句看起来是不是被截断了（判据：最后一个 token 是悬空关键字）。空语句（只剩注释）不会走到这里，因为切分器根本不产出它。
fn looks_truncated(tokens: &[Tok]) -> bool {
    matches!(tokens.last(), Some(Tok::Word(word)) if DANGLING_TAIL.contains(&word.as_str()))
}

/// 几条**任何方言都不成立**的形状错误。词法引擎没有语法树也没有方言知识，没资格判一条语句"合法"；但有资格判"这条
/// 不可能通过服务端解析"，而后者才是有用的那一半。挡住的是 `delete * from user where id = 3` 这种把 SELECT 的 `*` 写法
/// 带进 DELETE 的语句——发出去只会收到语法错误，之前却被评为 Low 一路放行，审查者看到的是一个根本没打算执行的半成品。
/// 刻意只列四条，而且每条都要在主流方言里都不成立：`*` 不能跟在 `DELETE` 或 `FROM` 后面；`DELETE` 不能没有 `FROM`；
/// `UPDATE` 不能没有 `SET`（MySQL 的多表更新也得写 `SET`）；`INSERT` 没有 `VALUES` / `SELECT` / `SET`（MySQL 的
/// `INSERT ... SET`）/ `DEFAULT VALUES` 就没说要插什么。
/// 和 `looks_truncated` 的分工要说清楚，因为处置完全不同：那边是"这段文本还不是完整意图，别拿它下结论"，拦住入库；这里是
/// "文本完整，但不可能被任何服务端解析成功"，那是有价值的事实，照实记录、只上报一条 Medium，不拦入库。两者不重叠。
fn malformed_shape(tokens: &[Tok], kind: &str) -> Option<&'static str> {
    let has = |needle: &str| tokens.iter().any(|tok| word(tok) == Some(needle));
    let star_after = |anchor: &str| {
        tokens
            .windows(2)
            .any(|pair| word(&pair[0]) == Some(anchor) && is_punct(&pair[1], "*"))
    };
    match kind {
        "delete" if star_after("delete") => Some("`*` cannot follow DELETE"),
        "delete" if star_after("from") => Some("`*` cannot follow DELETE FROM"),
        "delete" if !has("from") => Some("DELETE requires FROM"),
        "update" if !has("set") => Some("UPDATE requires SET"),
        "insert"
            if !(has("values")
                || has("value")
                || has("select")
                || has("set")
                || has("default")) =>
        {
            Some("INSERT declares no values")
        }
        _ => None,
    }
}

/// 语句里出现的「非 ASCII 标点」，按字符计数。
/// 中文输入法留下的东西：`；`、`，`、`（`、`「`、全角引号、全角冒号，还有从网页粘过来的 `–`。分词器把它们当成普通标点，
/// 于是有两类后果：数据库根本不认它们；而且**`split_statements_paired` 只认真分号**，所以 `a = 1；UPDATE …；` 整块会被当成一条语句
/// 评估，后面那几条的风险就这样被藏起来了。字符串字面量和注释里的全角字符不算——那是数据不是语法，把它们报错纯属误伤。
/// 已知没盖住的：全角**字母**（`Ａ`、`ｔ`）。`is_ident_start` 用的 `is_alphabetic()` 满足全角字母，于是它变成一个看起来完全正常的词；
/// 判它们需要 Unicode 归一化，属于方言知识之外的另一层，这里刻意不做，在 docs/known-limits.md 里写成了限制。
/// 全角**数字**反而能抓到：它不满足 `is_alphabetic()`，只能走标点那条支路。
fn full_width_punctuation(tokens: &[Tok]) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for tok in tokens {
        if let Tok::Punct(symbol) = tok {
            if !symbol.is_ascii() {
                *counts.entry(symbol.clone()).or_insert(0) += 1;
            }
        }
    }
    counts.into_iter().collect()
}

/// 出现在目标名之前、用来修饰它但本身不是目标名的词。`DROP TABLE IF EXISTS public.users` 里 `table`/`if`/`exists`/`public`
/// 都要跳过，真正的目标是 `users`；漏掉这张表就会把 `TABLE` 当成表名报给用户。
const TARGET_NOISE: &[&str] = &[
    "table", "database", "schema", "index", "view", "sequence", "function", "procedure", "trigger",
    "materialized", "temp", "temporary", "or", "replace", "unique", "if", "not", "exists", "only",
    "public",
];

/// 单条语句的结构信息，由 `parse()` 一次性提取、供所有检测器共用。只记录"检测器需要知道的事"，不构建语法树——
/// 这也是这个引擎能保持几百行的原因。
#[derive(Debug, Clone)]
struct Parsed {
    /// 语句类型，小写（`select` / `delete` / `drop` …）。
    kind: String,
    /// 语句作用的对象名，解析不出来为 None（此时 finding 的文案里就不带表名）。
    target: Option<String>,
    has_where: bool,
    /// WHERE 条件是恒真的（`WHERE 1=1`），等于没写条件。
    where_constant_true: bool,
    has_limit: bool,
    /// SELECT 投影里出现的列名，用于敏感列检测。已排除函数名和修饰词。
    projected: Vec<String>,
    /// `ALTER SYSTEM` 改的是服务器配置，不是普通结构变更，要走实例控制的判定。
    alter_system: bool,
}

/// 取语句类型：默认就是第一个词。唯一特例是 `WITH`（CTE）：`WITH x AS (SELECT ...) DELETE FROM t` 开头是 WITH，
/// 但整体是一条写语句。所以遇到 WITH 时继续在**括号外**找写关键字，找到就用它，找不到才当只读的 SELECT——
/// 只看括号外是因为 CTE 内部必然是 SELECT。
fn resolve_kind(tokens: &[Tok]) -> String {
    let head = tokens.iter().find_map(word).unwrap_or_default().to_string();
    if head != "with" {
        return head;
    }
    // 在括号外继续找写关键字。
    let mut depth = 0i32;
    for tok in tokens {
        if is_punct(tok, "(") {
            depth += 1;
            continue;
        }
        if is_punct(tok, ")") {
            depth -= 1;
            continue;
        }
        if depth == 0 {
            if let Some(w) = word(tok) {
                if WRITER_KINDS.contains(&w) {
                    return w.to_string();
                }
            }
        }
    }
    "select".to_string()
}

/// 找出语句的目标对象名。`anchors` 指"目标跟在哪个词后面"：`DROP TABLE users` 是 `drop`，`DELETE FROM users` 是 `from`，
/// `INSERT INTO users` 是 `into`。只在括号外（depth == 0）找锚点，否则子查询里的 `FROM` 会被当成主语句的目标。
/// 锚点后收集 `catalog.schema.table` 这样的点分名，最后**只保留最后一段**——对用户来说 `public.users` 里的 `users`
/// 才是要看的表名，前缀是库/schema。
fn target_after(tokens: &[Tok], anchors: &[&str]) -> Option<String> {
    let mut depth = 0i32;
    for (index, tok) in tokens.iter().enumerate() {
        if is_punct(tok, "(") {
            depth += 1;
            continue;
        }
        if is_punct(tok, ")") {
            depth -= 1;
            continue;
        }
        if depth != 0 {
            continue;
        }
        let Some(head) = word(tok) else { continue };
        if !anchors.contains(&head) {
            continue;
        }

        // `pieces.is_empty()` 保证修饰词只在名字开始前被跳过：一旦开始收集名字，后面的词就都算名字的一部分。
        let mut pieces: Vec<String> = Vec::new();
        let mut cursor = index + 1;
        while cursor < tokens.len() {
            match &tokens[cursor] {
                Tok::Word(name) if pieces.is_empty() && TARGET_NOISE.contains(&name.as_str()) => {
                    cursor += 1;
                }
                Tok::Word(name) | Tok::Name(name) => {
                    pieces.push(name.clone());
                    cursor += 1;
                    if !matches!(tokens.get(cursor), Some(dot) if is_punct(dot, ".")) {
                        break;
                    }
                }
                Tok::Punct(p) if p == "." => cursor += 1,
                _ => break,
            }
        }
        return pieces.last().cloned();
    }
    None
}

/// 截出 WHERE 子句的范围：到 `GROUP` / `ORDER` / `LIMIT` / `HAVING` / `WINDOW` / `FOR` 之前为止。`is_constant_true` 只看条件本身，排掉排序和分页能少很多误判。
fn where_span(rest: &[Tok]) -> &[Tok] {
    for (index, tok) in rest.iter().enumerate() {
        if let Some(w) = word(tok) {
            if matches!(w, "group" | "order" | "limit" | "having" | "window" | "for") {
                return &rest[..index];
            }
        }
    }
    rest
}

/// 判断 WHERE 条件是否恒真（`WHERE 1=1` / `true` / `1`）：效果上等于没有条件，却看起来"写了 WHERE"，
/// 是最容易骗过人工审查的一种，所以单独检测。先把括号去掉再看（`WHERE (1=1)` 和 `WHERE 1=1` 是一回事）。
/// 只认这三种最简单的形式是有意的——真正的恒真表达式（`a=a`、`1>0`）需求值能力，已超出词法引擎的范围，硬猜只会增加误报。
fn is_constant_true(clause: &[Tok]) -> bool {
    let flat: Vec<Tok> = clause
        .iter()
        .filter(|tok| !is_punct(tok, "(") && !is_punct(tok, ")"))
        .cloned()
        .collect();
    let Some(first) = flat.first() else {
        return false;
    };
    if matches!(word(first), Some("true")) {
        return true;
    }
    if flat.len() == 1 {
        return matches!(word(first), Some("1"))
            || matches!(first, Tok::Num(n) if n == "1");
    }
    if flat.len() >= 3
        && matches!(&flat[0], Tok::Num(n) if n == "1" || n == "1.0")
        && is_punct(&flat[1], "=")
        && matches!(&flat[2], Tok::Num(n) if n == "1" || n == "1.0")
    {
        return true;
    }
    false
}

/// 找顶层的 WHERE，返回 (有没有, 条件是否恒真)。只看括号外：`DELETE FROM t WHERE id IN (SELECT id FROM x WHERE ...)` 里
/// 子查询的 WHERE 不能算外层有条件，否则会漏报"外层没条件"这种最危险的情况。
fn has_where(tokens: &[Tok]) -> (bool, bool) {
    let mut depth = 0i32;
    for (index, tok) in tokens.iter().enumerate() {
        if is_punct(tok, "(") {
            depth += 1;
            continue;
        }
        if is_punct(tok, ")") {
            depth -= 1;
            continue;
        }
        if depth == 0 && word(tok) == Some("where") {
            return (true, is_constant_true(where_span(&tokens[index + 1..])));
        }
    }
    (false, false)
}

/// 截出 `SELECT` 和顶层 `FROM` 之间的投影部分，供敏感列检测使用。`started` 跳过 SELECT 之前的内容
/// （`WITH x AS (...)` 里的括号 SELECT 不算投影），`depth == 0` 保证遇到的是主查询的 FROM 而不是子查询的。
fn projection_tokens(tokens: &[Tok]) -> Vec<Tok> {
    let mut depth = 0i32;
    let mut started = false;
    let mut out: Vec<Tok> = Vec::new();
    for tok in tokens {
        if started && depth == 0 && word(tok) == Some("from") {
            break;
        }
        if is_punct(tok, "(") {
            depth += 1;
        } else if is_punct(tok, ")") {
            depth -= 1;
        }
        if !started {
            if depth == 0 && word(tok) == Some("select") {
                started = true;
            }
            continue;
        }
        out.push(tok.clone());
    }
    out
}

/// 投影里要忽略的词：修饰关键字和会出现但不是列名的东西。
const PROJECTION_NOISE: &[&str] = &[
    "distinct", "all", "as", "case", "when", "then", "else", "end", "select", "from", "top",
];

/// 判断一个列名是否命中敏感名单：按分隔符（空格、`-`、`_`、`.`、`$`、`\`）把名字切成词再比，所以 `user_phone`、
/// `"User Phone"`、`phone` 都能命中名单里的 `phone`，而 `hotel` 不会（子串匹配会误报这种）。
/// 多词针（如 `id_card`）要求整名相等，不会退化成只匹配 `id`——否则所有带 id 的主键列都会被标成敏感，误报到没人再看这个提示。
fn is_sensitive(name: &str, needles: &[String]) -> bool {
    let parts = split_name(name);
    if parts.is_empty() {
        return false;
    }
    let joined = parts.join("_");
    needles.iter().any(|raw| {
        let needle_parts = split_name(raw);
        match needle_parts.len() {
            // 空针（用户留了空行）不匹配任何东西。
            0 => false,
            1 => parts.iter().any(|part| *part == needle_parts[0]),
            _ => needle_parts.join("_") == joined,
        }
    })
}

fn split_name(name: &str) -> Vec<String> {
    name.split([' ', '-', '_', '.', '$', '\\'])
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
        .collect()
}

/// 把一条语句的 token 提炼成 `Parsed`：所有检测器都只读这个结构体、不再回头看原始 token，所以"怎么理解一条语句"集中在这一个函数里。
fn parse(tokens: &[Tok]) -> Parsed {
    let kind = resolve_kind(tokens);
    // `update` 和 `with` 共用一组锚点：`WITH ... UPDATE t SET` 的目标跟在 update 后面，而 `UPDATE t SET ... FROM x` 也可能出现 from。
    let target = match kind.as_str() {
        "drop" | "truncate" | "rename" | "alter" | "create" => target_after(tokens, &[kind.as_str()]),
        "update" | "with" => target_after(tokens, &["update", "delete", "from"]),
        "delete" | "select" => target_after(tokens, &["from"]),
        "insert" | "replace" | "merge" => target_after(tokens, &["into"]),
        "call" | "exec" | "execute" | "copy" => target_after(tokens, &[kind.as_str()]),
        "load" => target_after(tokens, &["into", "table"]),
        // 其他语句类型不报目标名：与其猜错，不如不说。
        _ => None,
    };
    let (has_where, where_constant_true) = has_where(tokens);
    // LIMIT / TOP / FETCH / ROWNUM 分别是 MySQL、SQL Server、标准、Oracle 的限行写法。
    let has_limit = tokens
        .iter()
        .any(|tok| matches!(word(tok), Some("limit") | Some("top") | Some("fetch") | Some("rownum")));
    let projection = projection_tokens(tokens);
    let mut projected: Vec<String> = Vec::new();
    for (index, tok) in projection.iter().enumerate() {
        let name = match tok {
            Tok::Word(name) | Tok::Name(name) => name.clone(),
            _ => continue,
        };
        if PROJECTION_NOISE.contains(&name.as_str()) {
            continue;
        }
        // 后面紧跟 `(` 的词是函数名而不是列名：`count(*)`、`md5(secret)` 里的 md5 不该被当成敏感列。副作用是其中的 secret 仍参与检测——对脱敏审查正是要这样。
        if matches!(projection.get(index + 1), Some(tok) if is_punct(tok, "(")) {
            continue;
        }
        projected.push(name);
    }
    Parsed {
        kind,
        target,
        has_where,
        where_constant_true,
        has_limit,
        projected,
        alter_system: is_alter_system(tokens),
    }
}

/// 是否是 `ALTER SYSTEM`（取前两个词比对）：`ALTER` 后面既可能是 `TABLE`（结构变更）也可能是 `SYSTEM`（改服务器参数），两者风险性质完全不同。
fn is_alter_system(tokens: &[Tok]) -> bool {
    tokens.iter().filter_map(word).take(2).collect::<Vec<_>>() == ["alter", "system"]
}

/// 调用方提供的额外上下文。所有字段都是可选的，缺省时相关检测器直接不触发。
#[derive(Debug, Clone, Default)]
pub struct AnalyzeContext<'a> {
    /// 目标库名，用于库名白名单。
    pub database: Option<&'a str>,
    /// 预计影响行数，**只能由调用方自报**：引擎不执行 SQL、也不查执行计划，所以没有这个值时 largeAffectedRows 永远不会命中。
    /// `plan.estimated_rows` 存在时它优先——宿主的数字可信。
    pub estimated_rows: Option<i64>,
    /// 宿主 `getPlanCapabilities` + `explainPlan` 给的事实。None = 这次没读或没读全,
    /// 此时行数只能继续信 `estimated_rows`，静态结论既不变严也不变松。
    pub plan: Option<PlanFacts>,
    /// 宿主 `getTableMetadata` 读到的那张表的真实列清单。None = 没读到，此时所有依赖元数据的
    /// 检查一概不触发——静态分析的结论同样既不变严也不变松。
    pub table_facts: Option<TableFacts>,
}

/// `incompleteStatement` 这条 finding 的 code。提成常量是因为 main.rs 和 mcp.rs 的落库闸门都要按 code 找它，三处各写一遍字面量迟早会写岔。
/// code 一旦发布就不能改名：老记录的 findings 是按 JSON 存在库里的，UI 也按 code 查本地化字典。
pub const INCOMPLETE_FINDING: &str = "incompleteStatement";

/// 落库闸门：这段 SQL 能不能进审计链。`guard/submit` 和 MCP 的 `sql_guard_record_review` 都必须在写库前调它。
/// 返回的 String 是给**人**看的原因，会被原样塞进 JSON-RPC 的 error message，所以要说清楚是哪一条语句、哪里不对，而不是只回一个"invalid"。
/// 拦两种情况：一条语句都没有（整段只剩空白和注释；`required_sql` 已挡住空串，走到这里的必然是 `-- 只是注释` 这种），
/// 以及有语句被判定为截断（见 `looks_truncated`）。
/// 词法引擎没法真正判断语法是否合法，这道闸门的目的不是做解析器，而是挡住那些**明显**的半成品——它们既产生不了有意义的审查结论，
/// 又会永久污染一条只能追加的链。
pub fn check_recordable(report: &RiskReport) -> Result<(), String> {
    if report.statement_count == 0 {
        return Err(
            "Nothing to record: the input contains only whitespace and comments".to_string(),
        );
    }
    let truncated: Vec<String> = report
        .findings
        .iter()
        .filter(|item| item.code == INCOMPLETE_FINDING)
        // finding() 在单语句时不加 "#n" 标注，所以这里直接复用它的 statement 字段：有就带上，没有就只说"the statement"。
        .map(|item| {
            item.statement
                .clone()
                .map(|mark| format!("statement {mark}"))
                .unwrap_or_else(|| "the statement".to_string())
        })
        .collect();
    if truncated.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Refusing to record a truncated statement ({}): it ends on a dangling clause keyword. \
         Finish the SQL before submitting — the audit chain is append-only, so a half-written \
         statement could never be removed.",
        truncated.join(", ")
    ))
}

/// `public.orders` / `"public"."orders"` → `orders`。宿主元数据里的表名和语句里的表名都先取最后一段再比，否则 `FROM public.orders` 会因为多了一个模式前缀而认不出是同一张表。
fn bare_name(raw: &str) -> String {
    raw.split('.')
        .next_back()
        .unwrap_or(raw)
        .trim()
        .replace(['"', '`', '[', ']', '\''], "")
        .to_lowercase()
}

fn ident_name(tok: &Tok) -> Option<&str> {
    match tok {
        Tok::Word(name) | Tok::Name(name) => Some(name.as_str()),
        _ => None,
    }
}

/// 取 UPDATE ... SET 的赋值：`(列名, 右值是不是字面量 NULL)`。只看 `set` 之后、`where` / `from` / `returning` / `select`
/// 之前、出现在 `=` 左边的裸标识符：这是 SQL 里少数几个「这个名字必定是这张表的列」的位置，所以才敢拿去和宿主的列清单比对；
/// WHERE 里的标识符不行——可能是别名、函数，或者另一张表的列。
/// 带点号的名字（`u.phone`）整个跳过：限定符指向哪张表词法层判断不了，拿这张表的元数据去判它就是造假日志。
/// 不追括号是刻意的：`split_statements_paired` 只用括号算层级，交给检测器的 token 流里**没有括号**，所以边界只能靠子句关键字。
fn set_assignments(tokens: &[Tok]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut in_set = false;
    let mut index = 0usize;
    while index < tokens.len() {
        let tok = &tokens[index];
        if let Some(head) = word(tok) {
            if !in_set {
                if head == "set" {
                    in_set = true;
                }
                index += 1;
                continue;
            }
            if matches!(head, "where" | "from" | "returning" | "select") {
                break;
            }
        }
        if in_set {
            let before_dot = index > 0 && matches!(&tokens[index - 1], Tok::Punct(p) if p == ".");
            let after_dot = matches!(tokens.get(index + 1), Some(dot) if is_punct(dot, "."));
            let is_assign = matches!(tokens.get(index + 1), Some(eq) if is_punct(eq, "="));
            if is_assign && !before_dot && !after_dot {
                if let Some(name) = ident_name(tok) {
                    let rhs_is_null =
                        matches!(tokens.get(index + 2), Some(item) if word(item) == Some("null"));
                    out.push((name.to_string(), rhs_is_null));
                    index += 2;
                    continue;
                }
            }
        }
        index += 1;
    }
    out
}

/// `INSERT INTO t (a, b) VALUES …` 里的那份列清单。token 流里没有括号，所以用「表名之后、子句关键字之前」来界定：撞上 BOUNDARY
/// （values / select / default…）就收尾，前面收集到的就是列清单；撞上 BAIL（partition / on / key… 这些不该出现在列清单里的词）
/// 或任何字面量就整体放弃返回空——列清单认不出来不等于语句有问题，宁可什么也不判，也不要造出一条假的 `unknownColumn`。
fn inserted_columns(tokens: &[Tok]) -> Vec<String> {
    const BOUNDARY: &[&str] = &["values", "value", "select", "default", "returning", "duplicate"];
    const BAIL: &[&str] = &[
        "into", "table", "overwrite", "ignore", "all", "as", "if", "partition", "set", "on",
        "key", "from", "where", "join", "using", "conflict",
    ];
    let mut cursor = match tokens.iter().position(|tok| word(tok) == Some("into")) {
        Some(at) => at + 1,
        None => return Vec::new(),
    };
    // 先吃掉表名，它可能是点分名（`schema.t`）。
    while cursor < tokens.len() {
        if !matches!(&tokens[cursor], Tok::Word(_) | Tok::Name(_)) {
            break;
        }
        cursor += 1;
        if matches!(tokens.get(cursor), Some(dot) if is_punct(dot, ".")) {
            cursor += 1;
            continue;
        }
        break;
    }
    let mut out: Vec<String> = Vec::new();
    while cursor < tokens.len() {
        let tok = &tokens[cursor];
        if is_punct(tok, ",") {
            cursor += 1;
            continue;
        }
        if let Some(head) = word(tok) {
            if BOUNDARY.contains(&head) {
                break;
            }
            if BAIL.contains(&head) {
                return Vec::new();
            }
        }
        match ident_name(tok) {
            Some(name) => out.push(name.to_string()),
            None => return Vec::new(),
        }
        cursor += 1;
    }
    out
}

/// 谓词之后还可能挂着的子句起点，探针必须在它们之前收尾：`RETURNING` 挑的是写完要回传哪些列，`OPTION (…)` 是 SQL Server 的执行选项、
/// 根本不属于 SELECT，`FOR UPDATE` 会把一条只读探针变成锁请求。
const PROBE_TAIL: &[&str] = &["returning", "option", "for"];

/// 顶层（括号之外）出现这些词的位置，按出现顺序。只匹配 `Tok::Word`：引号标识符 `"where"` 是一张叫 where 的表，不是关键字。
/// `depth <= 0` 与 `split_on` 同一口径，多出来的右括号不至于把后面的子句全看成嵌套。
fn top_level(statement: &[Spanned], words: &[&str]) -> Vec<usize> {
    let mut depth = 0i32;
    let mut hits = Vec::new();
    for (index, (tok, _)) in statement.iter().enumerate() {
        match tok {
            Tok::Punct(p) if p == "(" => depth += 1,
            Tok::Punct(p) if p == ")" => depth -= 1,
            Tok::Word(text) if depth <= 0 && words.contains(&text.as_str()) => hits.push(index),
            _ => {}
        }
    }
    hits
}

fn has_top_level(statement: &[Spanned], words: &[&str]) -> bool {
    !top_level(statement, words).is_empty()
}

/// `[from, to)` 这段里有没有**顶层逗号**。这是探针唯一一处不能"看不准就放过"的地方：`UPDATE t, u SET …` 和
/// `DELETE a, b FROM a, b …` 切出来的关系部分是 `t, u`，拼成 `SELECT * FROM t, u WHERE …` 是一条**完全合法**的语句，
/// 宿主不会拒它，算出来的是两张表的笛卡尔积——一个又大又错、又看起来可信的行数。宁可没有数字。
/// 逗号只算在 `from`/`update` 之后、切段结束之前；括号里的逗号（`IN (1,2)`、列清单）不改变表清单，不算。
fn has_comma_between(statement: &[Spanned], from: usize, to: usize) -> bool {
    let mut depth = 0i32;
    for (index, (tok, _)) in statement.iter().enumerate() {
        if index >= to {
            break;
        }
        match tok {
            Tok::Punct(p) if p == "(" => depth += 1,
            Tok::Punct(p) if p == ")" => depth -= 1,
            Tok::Punct(p) if p == "," && index > from && depth <= 0 => return true,
            _ => {}
        }
    }
    false
}

/// 取 `[from, to)` 这段 token 对应的**原文**（`to` 传 `statement.len()` 表示到结尾）。
/// 切原文而不是拿 token 重拼，有两个不可省略的理由：分词器把裸词和引号标识符都转了小写（`"myTable"` → `mytable`，MySQL 在
/// Linux 上那就是另一张表），而 `split_statements_paired` 还给检测器把括号整个丢了（`count(*)` 只剩下 `*`）。两者都会改掉送进
/// `explainPlan` 的文本，而这个文本要进报告、要进审计记录。
fn statement_text<'a>(
    source: &'a str,
    statement: &[Spanned],
    from: usize,
    to: usize,
) -> &'a str {
    if from >= to || to > statement.len() {
        return "";
    }
    let start = statement[from].1.0;
    let end = statement[to - 1].1.1;
    source.get(start..end).unwrap_or("")
}

/// 探针出门前的两道公共检查，`self` 和改写出来的那条都要过。刻意不给只读语句开后门：用户自己写的 `SELECT … 'a;b'` 一样会被
/// 宿主的多语句门拒掉，与其让 UI 收到一次看不懂的失败，不如在这里就把原因说清楚。
fn finish_probe(sql: String, basis: &str) -> (Option<PlanProbe>, Option<&'static str>) {
    if sql.contains(';') {
        // 真的语句分隔符在上一步切分时就被吃掉了，所以这里的分号只能来自字面量（`WHERE note = 'a;b'`）。它对合法 SQL 无害，
        // 但按分号切多语句的宿主分不清，最坏是一次被拒的调用——这里选择自己先拒掉，并且如实说原因。
        return (None, Some("semicolonInText"));
    }
    if sql.len() > 8_000 {
        // 宿主返回的 `limits.maxPlanBytes` 一定会把它挡回来，不必发出去试。
        return (None, Some("tooLong"));
    }
    (Some(PlanProbe { sql, basis: basis.to_string() }), None)
}

/// 从一条语句推出「可以拿去问宿主要计划的只读语句」，或者推不出来时的那句原因。
/// 这是对 0.2.1 之前那次错误结论的正面回答。当时的理由是「宿主拒 DML，所以写语句永远拿不到行数」——那只挡住了**把 UPDATE
/// 原样发过去**这一条路。真正能走的是：单表写语句的影响行数，定义上就是它的 WHERE 谓词在目标表上匹配的行数，而那条谓词
/// SELECT 是只读的、宿主的门拦不住它；Postgres 给 `EXPLAIN UPDATE` 时根节点的 `Plan Rows` 本来就是底下那个扫描的估算，和
/// `SELECT * FROM t WHERE p` 用的是同一套统计信息——所以拿到的是同一个数。
/// 于是边界不在"宿主让不让"，而在**什么时候这个等式会撒谎**，那是下面这些 refusals 唯一的存在理由：宁可少一个数字，
/// 也不能给审批人一个看起来精确实则错的数量。
/// - `limit`：`UPDATE … LIMIT n` 实际影响 `min(匹配数, n)`，SELECT 会**高估**。
/// - `multiTable`：`UPDATE a, b SET …` / `UPDATE a JOIN b` / `DELETE … USING x` 没有单一"目标表"，谓词里的列属于谁要靠方言规则才知道。
/// - `updateFrom`：`UPDATE t SET … FROM x WHERE t.a = x.b`，只 FROM `t` 的探针拼不出那个谓词，发出去是语法错误或错得离谱的行数。
/// - `cursor`：`WHERE CURRENT OF` 匹配的是游标位置，不是任何行数。
/// - `valuesOnly`：`INSERT … VALUES` 的行数就写在语句里，不需要问优化器；要问的应该是 `INSERT … SELECT`，那个走 `source`。
/// - `conflict`：`ON CONFLICT` / `ON DUPLICATE KEY` 会把插入改成更新，命中数不再等于来源行数。
/// - `fullWidthPunctuation`：全角标点会破坏语句边界（见 `fullWidthPunctuation` 那条 finding），从这种文本切出的片段不该发给宿主。
/// - `noTarget`：切不出表清单——`DELETE FROM` 这种半成品，语句本身都立不住。
/// - `notRowEstimable`：DDL、管理类命令等等，"影响多少行"这个问题对它们不存在。
/// - `semicolonInText`：切出的文本里还有分号，只可能来自字面量；分不清它的宿主会当成多条语句直接拒，不如我们在发出去之前说清楚。
/// - `tooLong`：超过 8000 字符，宿主的 `limits.maxPlanBytes` 一定会挡回来。
/// - `nothingToPlan`：空语句。`split_statements_paired` 现在不产生它，留着是因为一旦哪天产生了，宁可不发也别把 `SELECT * FROM ` 接个空串发给宿主。
fn plan_probe(
    source: &str,
    statement: &[Spanned],
    kind: &str,
) -> (Option<PlanProbe>, Option<&'static str>) {
    if statement.is_empty() {
        return (None, Some("nothingToPlan"));
    }
    // 切出来的原文里不许有非 ASCII 标点：那种文本的语句边界本来就不可信。
    if statement.iter().any(|(tok, _)| {
        matches!(tok, Tok::Punct(p) if !p.is_ascii())
    }) {
        return (None, Some("fullWidthPunctuation"));
    }
    // 只读语句：宿主计划的就是用户写的那条，不需要任何改写。
    if kind == "select" {
        let text = statement_text(source, statement, 0, statement.len()).trim();
        if text.is_empty() {
            return (None, Some("nothingToPlan"));
        }
        return finish_probe(text.to_string(), "self");
    }

    if has_top_level(statement, &["limit", "offset", "fetch"]) {
        return (None, Some("limit"));
    }

    // 谓词：从顶层 `where` 收尾于 `returning` / `option` / `for` 之前。没有 `where` 就是全表——仍然值得去要一个数字，
    // 因为「碰了整张表」和「整张表 4000 万行」对审批是两个不同的信息，而静态规则只能说出前者。
    let where_at = top_level(statement, &["where"]).into_iter().next();
    let tail_cut = |from: usize| -> usize {
        top_level(statement, PROBE_TAIL)
            .into_iter()
            .find(|index| *index > from)
            .unwrap_or(statement.len())
    };
    let predicate = match where_at {
        Some(index) => {
            if has_top_level(statement, &["current"]) {
                return (None, Some("cursor"));
            }
            Some(statement_text(source, statement, index, tail_cut(index)).trim().to_string())
        }
        None => None,
    };

    let (relation, basis) = match kind {
        "delete" => {
            if has_top_level(statement, &["using"]) {
                return (None, Some("multiTable"));
            }
            let from = match top_level(statement, &["from"]).first() {
                Some(index) => *index,
                None => return (None, Some("noTarget")),
            };
            let stop = where_at.unwrap_or(statement.len());
            if top_level(statement, &["join"]).into_iter().any(|index| index > from && index < stop)
            {
                return (None, Some("multiTable"));
            }
            if has_comma_between(statement, from, stop) {
                return (None, Some("multiTable"));
            }
            let relation = statement_text(source, statement, from + 1, stop).trim();
            if relation.is_empty() {
                return (None, Some("noTarget"));
            }
            (relation, if predicate.is_some() { "predicate" } else { "wholeTable" })
        }
        "update" => {
            if has_top_level(statement, &["join", "using"]) {
                return (None, Some("multiTable"));
            }
            let set = match top_level(statement, &["set"]).first() {
                Some(index) => *index,
                None => return (None, Some("noTarget")),
            };
            // `UPDATE t SET … FROM x`：谓词里的 `x` 不在探针的 FROM 里，拼出来要么语法要么错数。注意 `single_table` 对这种
            // 形状回答 true（写入目标确实只有 `t`），所以这里不能复用那个判断——两件事的"单表"标准不一样。
            if top_level(statement, &["from"]).into_iter().any(|index| index > set) {
                return (None, Some("updateFrom"));
            }
            // 目标表写在 `update` 和 `set` **之间**。用 `where` 的位置当右界是错的：那样 `SET balance = 0` 会被当成表名切进
            // FROM 里，拼出一条语义完全不同的探针。这里不校验切出来的是不是合法表名——切对了就不需要校验。
            let head = top_level(statement, &["update"]).first().copied().unwrap_or(0);
            if set <= head {
                return (None, Some("noTarget"));
            }
            // MySQL 的 `UPDATE a, b SET …`：多表写法，谓词里的列属于哪张表要按方言规则才知道，而拼出来的 `FROM a, b` 是笛卡尔积。
            if has_comma_between(statement, head, set) {
                return (None, Some("multiTable"));
            }
            let relation = statement_text(source, statement, head + 1, set).trim();
            if relation.is_empty() {
                return (None, Some("noTarget"));
            }
            (relation, if predicate.is_some() { "predicate" } else { "wholeTable" })
        }
        "insert" => {
            // 只认 `ON CONFLICT` / `ON DUPLICATE KEY` 这两个词：`ON` 单独出现多半是 `JOIN b ON …` 的连接条件，那种 INSERT…SELECT 的行数依然可估。
            if has_top_level(statement, &["conflict", "duplicate"]) {
                return (None, Some("conflict"));
            }
            match top_level(statement, &["select"]).first() {
                Some(index) => {
                    let text = statement_text(source, statement, *index, tail_cut(*index)).trim();
                    if text.is_empty() {
                        return (None, Some("valuesOnly"));
                    }
                    // 这条**不能**再包一层 `SELECT * FROM …`：喂给 INSERT 的就是这条 SELECT 自己匹配的行数，包一层等于问"这条查询的结果集有多少列组合"。
                    return finish_probe(text.to_string(), "source");
                }
                None => return (None, Some("valuesOnly")),
            }
        }
        _ => return (None, Some("notRowEstimable")),
    };

    let sql = match (relation, predicate) {
        (relation, Some(predicate)) => format!("SELECT * FROM {relation} {predicate}"),
        (relation, None) => format!("SELECT * FROM {relation}"),
    };
    finish_probe(sql, basis)
}

/// 这条语句是不是「只碰一张表」，实际问的是**写入目标**清不清楚：多表写法（`UPDATE a JOIN b …`、MySQL 的 `UPDATE a, b SET …`）
/// 里 SET 的目标列可能属于另一张表，拿这一张表的元数据去判就会假阳性，所以整组元数据判定跳过。
/// 反过来，Postgres / SQL Server 的 `UPDATE t SET … FROM x` 里 `from` 带来的是**来源**表，SET 的目标列仍然只能是 `t` 的列——
/// 所以越过 `set` 就直接回答 true。看不准的时候一律回答 false——少一次升级，比一次误判便宜得多。
fn single_table(tokens: &[Tok]) -> bool {
    let update_head = matches!(tokens.first(), Some(head) if word(head) == Some("update"));
    let mut in_tables = false;
    for tok in tokens {
        let head = word(tok);
        if head == Some("join") {
            return false;
        }
        // 见上面的说明：目标列在 `set` 处已经定死了，后面的 FROM 只是来源。
        if update_head && head == Some("set") {
            return true;
        }
        match head {
            // 表清单的几种写法：`FROM a, b`、`DELETE FROM a USING b`，以及 MySQL 的 `UPDATE a, b SET …`——最后这种清单在
            // `update` 和 `set` 之间，没有 `from` 可看。`update` 只在它是语句首词时才开清单，免得子句里的词误开。
            Some("from") | Some("using") => in_tables = true,
            Some("update") if update_head => in_tables = true,
            Some("where" | "values" | "returning" | "select") => in_tables = false,
            _ => {
                if in_tables && is_punct(tok, ",") {
                    return false;
                }
            }
        }
    }
    true
}

/// 投影里有没有「整表通配」。括号没了，`count(*)` 和 `t.*` 都只剩下光秃秃一个 `*`，所以只能看它前面那个 token：
/// `select *` / `distinct *` / `, *` 算整表通配；前面是标识符（函数名）或点号（`other.*`，限定符指向哪张表判不了）都不算。
fn projects_whole_table(tokens: &[Tok]) -> bool {
    let projection = projection_tokens(tokens);
    projection.iter().enumerate().any(|(index, tok)| {
        if !is_punct(tok, "*") {
            return false;
        }
        match index.checked_sub(1).and_then(|previous| projection.get(previous)) {
            None => true,
            Some(head) => {
                matches!(word(head), Some("select") | Some("distinct") | Some("all"))
                    || is_punct(head, ",")
            }
        }
    })
}

/// 引擎的唯一入口：评估一段 SQL 脚本，产出风险报告。纯函数，不碰数据库也不执行 SQL，可以在任意线程上并发调用。
/// 风险等级是**取最大值**而不是累加：先算每条语句自身的最高等级（`statement_level`），再和整段脚本的 `level` 取 max。所以
/// 十条 Medium 不会变成 High——严重程度取决于最坏的那条，而不是问题的数量；数量信息由 `findings` 列表和 `multiStatement` 单独承载。
/// 检测器的顺序就是 findings 在 UI 里的显示顺序（半成品 → 破坏性 DDL → 结构变更 → 权限变更 → 管理类命令 → 无条件写 →
/// 恒真条件 → 有条件写 → 全表扫描 → 插入 → 敏感列）：最不可逆的放前面，审查者第一眼看到的就是最该看的；半成品排最前是因为
/// 语句本身都不完整时后面那些结论都没意义。
pub fn analyze(sql: &str, rules: &RuleSet, context: &AnalyzeContext) -> RiskReport {
    // 一次切分拿到两条流：`statement` 给检测器（无括号），`spanned` 给 `plan_probe`
    // （有括号、带原文字节区间）。两条来自同一个分组，下标天然对齐。
    let groups = split_statements_paired(&tokenize_spans(sql));
    let count = groups.len();
    let mut findings: Vec<Finding> = Vec::new();
    let mut summaries: Vec<StatementSummary> = Vec::new();
    let mut level = Risk::Low;

    for (index, (statement, spanned)) in groups.iter().enumerate() {
        let parsed = parse(statement);
        // local 收集本条语句的 finding，最后统一并入 findings：这样 statement_level 只看本条语句，
        // 不会被前面语句的等级污染。
        let mut local: Vec<Finding> = Vec::new();
        // 半成品检测在所有检测器之前，而且**不受任何 rules 开关控制**：它的用途不是评估风险，
        // 而是守住审计链的入口——链是 append-only 的，被截断的 SQL 一旦写进去就再也拿不出来，
        // 还会永久占着一个待审批位。`check_recordable` 会拿这条 finding 直接拒绝落库。
        if looks_truncated(statement) {
            local.push(finding(
                INCOMPLETE_FINDING,
                Risk::High,
                "Statement looks truncated: it ends on a dangling clause keyword",
                index,
                count,
            ));
        }
        // 形状错误也不受开关控制：它不是风险判断，而是"这条语句发出去不会执行成功"。
        // 但它**不**拒绝落库（`check_recordable` 只拦半成品）——写错也是发生过的意图，
        // 留在链里有价值，只需要带一条 Medium 让审查者看见它。
        if let Some(reason) = malformed_shape(statement, &parsed.kind) {
            local.push(finding(
                "malformedStatement",
                Risk::Medium,
                reason,
                index,
                count,
            ));
        }
        // 全角标点和形状错误同一档：不受开关控制、Medium、而且**不**拒绝落库。
        // 但它必须被说出来——只认真分号的切分会把 `…；UPDATE …；` 当成一条语句，
        // 于是审查者看到的是被压低了的条数，而发出去的是好几条。
        let fullwidth = full_width_punctuation(statement);
        if !fullwidth.is_empty() {
            let listed = fullwidth
                .iter()
                .map(|(symbol, times)| format!("{symbol}({times})"))
                .collect::<Vec<_>>()
                .join(", ");
            let message = if fullwidth.iter().any(|(symbol, _)| symbol == "；") {
                format!(
                    "Full-width punctuation where SQL syntax is required: {listed}. A database \
                     rejects these, and ； does not end a statement — the block behind it is \
                     reviewed as part of this one statement"
                )
            } else {
                format!(
                    "Full-width punctuation where SQL syntax is required: {listed}. A database \
                     does not accept these as SQL syntax"
                )
            };
            local.push(finding("fullWidthPunctuation", Risk::Medium, &message, index, count));
        }
        // 只有 UPDATE / DELETE 才谈得上"影响已有数据"，INSERT 走单独的 rowWrite。
        let write_dml = matches!(parsed.kind.as_str(), "update" | "delete");

        if rules.detect_destructive_ddl && DESTRUCTIVE_DDL.contains(&parsed.kind.as_str()) {
            local.push(finding(
                "destructiveDdl",
                Risk::High,
                &format!(
                    "{} permanently removes data or objects{}",
                    parsed.kind.to_uppercase(),
                    suffix(&parsed.target)
                ),
                index,
                count,
            ));
        }
        if rules.detect_schema_change
            && SCHEMA_DDL.contains(&parsed.kind.as_str())
            && !parsed.alter_system
        {
            local.push(finding(
                "schemaChange",
                Risk::Medium,
                &format!(
                    "{} changes database structure{}",
                    parsed.kind.to_uppercase(),
                    suffix(&parsed.target)
                ),
                index,
                count,
            ));
        }
        if matches!(parsed.kind.as_str(), "grant" | "revoke") {
            local.push(finding(
                "privilegeChange",
                Risk::High,
                &format!("{} changes access privileges", parsed.kind.to_uppercase()),
                index,
                count,
            ));
        }
        // 三类"引擎看不透"的命令共用一个开关。用 if / else if 而不是三个独立
        // if：一条语句只可能属于其中一类（kind 只有一个），链式判断避免重复报。
        if rules.detect_admin_commands {
            let kind_upper = parsed.kind.to_uppercase();
            if parsed.alter_system || INSTANCE_CONTROL.contains(&parsed.kind.as_str()) {
                local.push(finding(
                    "instanceControl",
                    Risk::High,
                    &format!("{kind_upper} acts on the database server itself, not on rows"),
                    index,
                    count,
                ));
            } else if BULK_LOAD.contains(&parsed.kind.as_str()) {
                local.push(finding(
                    "bulkLoad",
                    Risk::High,
                    &format!(
                        "{kind_upper} touches a server-side file and moves an unbounded row set{}",
                        suffix(&parsed.target)
                    ),
                    index,
                    count,
                ));
            } else if OPAQUE_CALL.contains(&parsed.kind.as_str()) {
                local.push(finding(
                    "opaqueCall",
                    Risk::High,
                    &format!(
                        "{kind_upper} runs stored code this engine cannot inspect{}",
                        suffix(&parsed.target)
                    ),
                    index,
                    count,
                ));
            }
        }
        if write_dml && !parsed.has_where && rules.detect_no_where_dml {
            local.push(finding(
                "noWhereDml",
                Risk::High,
                &format!(
                    "{} has no WHERE clause and touches every row{}",
                    parsed.kind.to_uppercase(),
                    suffix(&parsed.target)
                ),
                index,
                count,
            ));
        }
        if write_dml && parsed.where_constant_true && rules.detect_constant_where {
            local.push(finding(
                "constantWhere",
                Risk::High,
                &format!(
                    "{} WHERE condition is always true{}",
                    parsed.kind.to_uppercase(),
                    suffix(&parsed.target)
                ),
                index,
                count,
            ));
        }
        if write_dml && parsed.has_where && !parsed.where_constant_true {
            local.push(finding(
                "conditionalWrite",
                Risk::Medium,
                &format!(
                    "{} modifies matched rows{}",
                    parsed.kind.to_uppercase(),
                    suffix(&parsed.target)
                ),
                index,
                count,
            ));
        }
        if parsed.kind == "select"
            && rules.detect_unbounded_select
            && !parsed.has_where
            && !parsed.has_limit
        {
            local.push(finding(
                "unboundedScan",
                Risk::Medium,
                &format!("Full scan without WHERE or LIMIT{}", suffix(&parsed.target)),
                index,
                count,
            ));
        }
        if matches!(parsed.kind.as_str(), "insert" | "replace") && !parsed.has_where {
            local.push(finding(
                "rowWrite",
                Risk::Medium,
                &format!("INSERT writes rows{}", suffix(&parsed.target)),
                index,
                count,
            ));
        }
        if rules.detect_sensitive_columns && !rules.sensitive_columns.is_empty() {
            let mut hits: Vec<String> = parsed
                .projected
                .iter()
                .filter(|name| is_sensitive(name, &rules.sensitive_columns))
                .cloned()
                .collect();
            hits.sort();
            hits.dedup();
            if !hits.is_empty() {
                local.push(finding(
                    "sensitiveAccess",
                    Risk::Medium,
                    &format!("Reads sensitive column(s): {}", hits.join(", ")),
                    index,
                    count,
                ));
            }
        }

        // 下面这几条只在调用方真的从宿主读到了这张表的元数据时才存在，而且每条都要求语句的目标表和元数据描述的是同一张表。
        // 拿不到元数据时上面的静态判定原样成立，拿到了时只会往上报、不会往下降。
        let facts = context.table_facts.as_ref().filter(|metadata| {
            !metadata.columns.is_empty()
                && single_table(statement)
                && parsed
                    .target
                    .as_deref()
                    .is_some_and(|target| bare_name(target) == bare_name(&metadata.table))
        });
        if let Some(metadata) = facts {
            // 列名一律小写比对：词法单元已经把标识符转成小写了，宿主给的大小写不作数。
            let known: Vec<String> = metadata
                .columns
                .iter()
                .map(|column| column.name.to_lowercase())
                .collect();
            let mut unknown: Vec<String> = Vec::new();
            let mut nulls: Vec<String> = Vec::new();
            if parsed.kind == "update" {
                for (name, rhs_is_null) in set_assignments(statement) {
                    if !known.contains(&name) {
                        unknown.push(name.clone());
                    }
                    // nullable 为 None 时不判：宿主没告诉我们这一列能不能为空，
                    // 猜"不能为空"会把无害语句判成 High。
                    if rhs_is_null
                        && metadata
                            .columns
                            .iter()
                            .find(|column| column.name.to_lowercase() == name)
                            .and_then(|column| column.nullable)
                            == Some(false)
                    {
                        nulls.push(name);
                    }
                }
            }
            if parsed.kind == "insert" {
                unknown = inserted_columns(statement)
                    .into_iter()
                    .filter(|name| !known.contains(name))
                    .collect();
            }
            unknown.sort();
            unknown.dedup();
            if !unknown.is_empty() {
                local.push(finding(
                    "unknownColumn",
                    Risk::Medium,
                    &format!(
                        "Column(s) not in this table according to the host: {}",
                        unknown.join(", ")
                    ),
                    index,
                    count,
                ));
            }
            if !nulls.is_empty() {
                local.push(finding(
                    "nullIntoNotNull",
                    Risk::High,
                    &format!(
                        "Sets NOT NULL column(s) to NULL: {}",
                        nulls.join(", ")
                    ),
                    index,
                    count,
                ));
            }
            // `SELECT *` 的展开：静态判定看不见通配符背后有哪些列，宿主看得见。
            // 这里只补敏感列这一条，因为它是脱敏审查真正关心的东西；列数、索引
            // 之类的"展开后有多少列"宿主根本没返回（getTableMetadata 不含索引）。
            if rules.detect_sensitive_columns
                && !rules.sensitive_columns.is_empty()
                && parsed.kind == "select"
                && projects_whole_table(statement)
            {
                let mut expanded: Vec<String> = metadata
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .filter(|name| is_sensitive(name, &rules.sensitive_columns))
                    .collect();
                expanded.sort();
                expanded.dedup();
                let listed: Vec<String> = parsed
                    .projected
                    .iter()
                    .filter(|name| is_sensitive(name, &rules.sensitive_columns))
                    .cloned()
                    .collect();
                let extra: Vec<String> = expanded
                    .into_iter()
                    .filter(|name| !listed.iter().any(|hit| hit.to_lowercase() == name.to_lowercase()))
                    .collect();
                if !extra.is_empty() {
                    local.push(finding(
                        "sensitiveAccess",
                        Risk::Medium,
                        &format!(
                            "SELECT * expands to sensitive column(s) on this table: {}",
                            extra.join(", ")
                        ),
                        index,
                        count,
                    ));
                }
            }
        }

        // 本条语句的等级 = 它自己所有 finding 里最高的那个；一条都没有就是 Low。
        let statement_level = local.iter().map(|item| item.risk).max().unwrap_or(Risk::Low);
        level = level.max(statement_level);
        findings.extend(local);
        // 只读计划探针：把这条语句能安全改写成的那句 SELECT 一起交出去。
        // 这里只是**文字**，插件从不执行它——要不要发给宿主是 UI 和 `host.plans:read`
        // 的事。所以也不受任何 rules 开关控制：多给一个可核对的数字没有"关掉"的需求。
        let (probe, refused) = plan_probe(sql, spanned, &parsed.kind);
        summaries.push(StatementSummary {
            index,
            kind: parsed.kind.to_uppercase(),
            target: parsed.target.clone(),
            risk: statement_level,
            plan_probe: probe,
            plan_probe_refused: refused.map(str::to_string),
        });
    }

    // 脚本级 finding：不属于任何单条语句，所以 index/count 传 (0, 1)，
    // `finding()` 在 count == 1 时不会给它们加 "#n" 标注。
    if count > 1 && rules.detect_multi_statement {
        findings.push(finding(
            "multiStatement",
            Risk::Medium,
            &format!("Script bundles {count} statements in one execution"),
            0,
            1,
        ));
        level = level.max(Risk::Medium);
    }

    // 行数估计有两个来源：宿主 `explainPlan` 读回来的估算值，和调用方自报。前者一定
    // 优先——自报可以是 0，用来让这条规则闭嘴，而宿主的数字不能。
    // 已经是 Medium 及以上的语句遇到大行数直接升 High（本来就危险，再加上影响面大），
    // Low 的语句则升到 Medium。
    let host_rows = context.plan.as_ref().and_then(|plan| plan.estimated_rows);
    if let Some(rows) = host_rows.or(context.estimated_rows) {
        if rows >= rules.large_scan_row_threshold {
            let risk = if level >= Risk::Medium { Risk::High } else { Risk::Medium };
            // 来源必须写在消息里：一个 12 万的行数，来自计划和来自自报，审批人该做的
            // 事情完全不同。
            // 第三种来源是插件**改写**出来的那条只读 SELECT：数字仍然来自宿主，但
            // 计划的对象已经不是用户写的那条语句了。这句话必须出现在留痕里，否则
            // 审批记录会让一个没人写过的 SQL 看起来像用户写的。
            let rewritten = context.plan.as_ref().is_some_and(|plan| plan.rewritten);
            let origin = if host_rows.is_some() {
                if rewritten {
                    " from the host's estimated plan of a read-only SELECT SQL Guard wrote, not of this statement"
                } else {
                    " from the host's estimated plan"
                }
            } else {
                ""
            };
            findings.push(finding(
                "largeAffectedRows",
                risk,
                &format!(
                    "Estimated {rows} affected rows{origin} reaches the {} threshold",
                    rules.large_scan_row_threshold
                ),
                0,
                1,
            ));
            level = level.max(risk);
        }
    }

    // 白名单命中时也记一条 Low 的 finding：报告里要能看出"为什么这条没要求审批"，
    // 否则用户会以为引擎漏判了。
    let whitelisted = is_whitelisted(sql, context.database, rules);
    if whitelisted {
        findings.push(finding(
            "whitelisted",
            Risk::Low,
            &format!(
                "Allowed by whitelist{}",
                context
                    .database
                    .map(|db| format!(" (database `{db}`)"))
                    .unwrap_or_default()
            ),
            0,
            1,
        ));
    }

    // "整段脚本全是只读语句"才算只读。空脚本（`!summaries.is_empty()`）不算：
    // 一段什么都不做的输入没有放行与否的问题，交给调用方去拒绝空 SQL。
    let read_only = !summaries.is_empty()
        && summaries
            .iter()
            .all(|item| READ_ONLY_KINDS.contains(&item.kind.to_lowercase().as_str()));

    // 先按等级判断是否需要审批，再由白名单和只读放行把它关掉。
    // 注意 risk_level 保持不变——放行的只是"要不要人点一下"，风险事实照实上报，
    // 这样审计里仍然能看到"这条被评为 High 但因为白名单自动通过了"。
    let mut requires_approval = level >= rules.approval_from_level;
    if whitelisted || (rules.auto_allow_read_only && read_only) {
        requires_approval = false;
    }

    RiskReport {
        risk_level: level,
        requires_approval,
        whitelisted,
        statement_count: count,
        findings,
        statements: summaries,
        sql_hash: sha256_hex(&normalize_sql(sql)),
    }
}

/// 白名单判定：库名精确匹配，关键字子串匹配，命中任一即放行。
/// 两种匹配的粒度差别很大，这是有意为之也需要用户理解：
/// - 库名要求**完全相等**（忽略大小写和首尾空格），不会误伤名字相近的库；
/// - 关键字是**在整段 SQL 里做子串搜索**，非常粗——配一个 `test` 就会把所有
///   文本里出现 test 的语句都放行。它适合"某类脚本整体豁免"，不适合精细控制。
///   UI 的规则页对这一点有提示。
///
/// 空条目一律忽略，否则一个空字符串会让 `contains("")` 恒真、白名单形同全放行。
fn is_whitelisted(sql: &str, database: Option<&str>, rules: &RuleSet) -> bool {
    let lowered = sql.to_lowercase();
    let database_hit = database.map(|db| db.trim().to_lowercase()).is_some_and(|db| {
        rules
            .whitelist_databases
            .iter()
            .any(|entry| !entry.trim().is_empty() && entry.trim().to_lowercase() == db)
    });
    let keyword_hit = rules.whitelist_keywords.iter().any(|entry| {
        let needle = entry.trim().to_lowercase();
        !needle.is_empty() && lowered.contains(&needle)
    });
    database_hit || keyword_hit
}

/// finding 文案里的表名后缀：解析出目标就是 " on \`users\`"，否则空串。
fn suffix(target: &Option<String>) -> String {
    target
        .as_ref()
        .map(|name| format!(" on `{name}`"))
        .unwrap_or_default()
}

/// 构造一条 finding。`index` / `count` 只用来决定要不要加 "#n" 标注：
/// 单语句脚本里加序号是噪音，多语句脚本里不加就分不清是哪一条出的问题。
fn finding(code: &str, risk: Risk, message: &str, index: usize, count: usize) -> Finding {
    Finding {
        code: code.to_string(),
        risk,
        message: message.to_string(),
        statement: if count > 1 { Some(format!("#{}", index + 1)) } else { None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnFact, RuleSet, TableFacts};

    fn report(sql: &str) -> RiskReport {
        analyze(sql, &RuleSet::default(), &AnalyzeContext::default())
    }

    fn codes(sql: &str) -> Vec<String> {
        report(sql).findings.iter().map(|f| f.code.clone()).collect()
    }

    #[test]
    fn drop_table_is_high_and_needs_approval() {
        let report = report("DROP TABLE users;");
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.requires_approval);
        assert!(report.findings.iter().any(|f| f.code == "destructiveDdl"));
        assert_eq!(report.statements[0].target.as_deref(), Some("users"));
    }

    #[test]
    fn delete_without_where_is_high() {
        let report = report("DELETE FROM orders");
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.requires_approval);
        assert!(report.findings.iter().any(|f| f.code == "noWhereDml"));
    }

    #[test]
    fn delete_with_where_is_medium_and_auto_passes_at_high_threshold() {
        let report = report("delete from orders where id = 7");
        assert_eq!(report.risk_level, Risk::Medium);
        assert!(!report.requires_approval);
        assert!(report.findings.iter().any(|f| f.code == "conditionalWrite"));
    }

    #[test]
    fn always_true_where_is_high() {
        for sql in [
            "UPDATE t SET a = 1 WHERE 1=1",
            "update t set a = 1 where true",
            "UPDATE t SET a = 1 WHERE 1",
        ] {
            let report = report(sql);
            assert_eq!(report.risk_level, Risk::High, "{sql} => {:?}", report.findings);
            assert!(report.findings.iter().any(|f| f.code == "constantWhere"), "{sql}");
        }
    }

    #[test]
    fn string_and_comment_text_do_not_fool_the_parser() {
        // 'where 1=1' is a literal, and the trailing comment is not SQL.
        let report = report("UPDATE t SET note = 'where 1=1' -- where id=1\n");
        assert!(report.findings.iter().any(|f| f.code == "noWhereDml"));
        assert!(!report.findings.iter().any(|f| f.code == "constantWhere"));
    }

    #[test]
    fn sensitive_projection_is_flagged_with_column_names() {
        let report = report("SELECT id, user_phone, password AS pw FROM accounts WHERE id = 3");
        assert!(report.findings.iter().any(|f| f.code == "sensitiveAccess"));
        let message = report
            .findings
            .iter()
            .find(|f| f.code == "sensitiveAccess")
            .unwrap()
            .message
            .clone();
        assert!(message.contains("user_phone") && message.contains("password"), "{message}");
    }

    #[test]
    fn similar_names_are_not_sensitive() {
        assert!(!codes("SELECT hotel, total FROM bookings WHERE id = 1").contains(&"sensitiveAccess".to_string()));
    }

    #[test]
    fn unbounded_select_is_medium_but_read_only_never_needs_approval() {
        let report = report("SELECT * FROM big_table");
        assert_eq!(report.risk_level, Risk::Medium);
        assert!(report.findings.iter().any(|f| f.code == "unboundedScan"));
        assert!(!report.requires_approval);
    }

    #[test]
    fn limited_select_is_clean() {
        let report = report("SELECT id FROM users WHERE id = 1 LIMIT 10");
        assert_eq!(report.risk_level, Risk::Low);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn multi_statement_script_splits_on_top_level_semicolon() {
        let report = report("CREATE TABLE t (a INT); DELETE FROM t");
        assert_eq!(report.statement_count, 2);
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.findings.iter().any(|f| f.code == "multiStatement"));
        assert_eq!(report.statements[0].target.as_deref(), Some("t"));
    }

    #[test]
    fn create_or_replace_view_finds_the_real_target() {
        let report = report("CREATE OR REPLACE VIEW v_orders AS SELECT * FROM orders");
        assert_eq!(report.statements[0].target.as_deref(), Some("v_orders"));
    }

    #[test]
    fn drop_if_exists_skips_noise_words() {
        let report = report("DROP TABLE IF EXISTS public.users");
        assert_eq!(report.statements[0].target.as_deref(), Some("users"));
        assert_eq!(report.risk_level, Risk::High);
    }

    #[test]
    fn cte_wrapped_delete_is_treated_as_a_write() {
        let report = report("WITH stale AS (SELECT id FROM t) DELETE FROM t");
        assert_eq!(report.statements[0].kind, "DELETE");
        assert!(report.findings.iter().any(|f| f.code == "noWhereDml"));
    }

    #[test]
    fn grant_is_high() {
        let report = report("GRANT ALL ON users TO guest");
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.findings.iter().any(|f| f.code == "privilegeChange"));
    }

    #[test]
    fn whitelist_database_clears_approval() {
        let rules = RuleSet {
            whitelist_databases: vec!["sandbox".to_string()],
            ..Default::default()
        };
        let report = analyze(
            "DELETE FROM t",
            &rules,
            &AnalyzeContext { database: Some("SANDBOX"), ..AnalyzeContext::default() },
        );
        assert!(report.whitelisted);
        assert!(!report.requires_approval);
        // 白名单只免掉审批，风险等级本身仍然如实上报。
        assert_eq!(report.risk_level, Risk::High);
    }

    #[test]
    fn whitelist_keyword_clears_approval() {
        let rules = RuleSet {
            whitelist_keywords: vec!["explain".to_string()],
            ..Default::default()
        };
        let report = analyze(
            "EXPLAIN DELETE FROM t",
            &rules,
            &AnalyzeContext::default(),
        );
        assert!(report.whitelisted && !report.requires_approval);
    }

    #[test]
    fn estimated_rows_escalate_to_high() {
        let report = analyze(
            "UPDATE t SET a = 1 WHERE b = 2",
            &RuleSet::default(),
            &AnalyzeContext { database: None, estimated_rows: Some(500_000), ..AnalyzeContext::default() },
        );
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.requires_approval);
    }

    #[test]
    fn medium_threshold_policy_requires_approval_for_conditional_write() {
        let rules = RuleSet {
            approval_from_level: Risk::Medium,
            ..Default::default()
        };
        let report = analyze(
            "DELETE FROM t WHERE id = 5",
            &rules,
            &AnalyzeContext::default(),
        );
        // "只读自动放行"对 DELETE 不适用。
        assert!(report.requires_approval);
        let clean = analyze(
            "SELECT id FROM t WHERE id = 5",
            &rules,
            &AnalyzeContext::default(),
        );
        assert!(!clean.requires_approval);
    }

    #[test]
    fn disabled_detector_produces_no_finding() {
        let rules = RuleSet {
            detect_no_where_dml: false,
            detect_multi_statement: false,
            ..Default::default()
        };
        let report = analyze(
            "DELETE FROM t",
            &rules,
            &AnalyzeContext::default(),
        );
        assert_eq!(report.risk_level, Risk::Low);
    }

    #[test]
    fn hash_is_stable_across_whitespace_and_case() {
        let a = report("delete   from t where id=1").sql_hash.clone();
        let b = report("DELETE FROM t\nWHERE id = 1").sql_hash.clone();
        assert_eq!(a, b);
        assert_ne!(a, report("delete from t where id=2").sql_hash);
    }

    #[test]
    fn admin_commands_are_high_risk_and_need_approval() {
        for (sql, code) in [
            ("SHUTDOWN", "instanceControl"),
            ("KILL QUERY 42", "instanceControl"),
            ("FLUSH PRIVILEGES", "instanceControl"),
            ("ALTER SYSTEM SET max_connections = 500", "instanceControl"),
            ("LOAD DATA INFILE '/tmp/x.csv' INTO TABLE t", "bulkLoad"),
            ("COPY t FROM '/tmp/x.csv'", "bulkLoad"),
            ("CALL sp_purge()", "opaqueCall"),
            ("EXEC sp_purge", "opaqueCall"),
            ("DO PERFORM cleanup()", "opaqueCall"),
        ] {
            let report = report(sql);
            assert_eq!(report.risk_level, Risk::High, "{sql}");
            assert!(report.requires_approval, "{sql}");
            assert!(
                report.findings.iter().any(|f| f.code == code),
                "{sql} => {:?}",
                report.findings
            );
        }
    }

    #[test]
    fn admin_detector_can_be_switched_off() {
        let rules = RuleSet {
            detect_admin_commands: false,
            ..Default::default()
        };
        let report = analyze(
            "SHUTDOWN",
            &rules,
            &AnalyzeContext::default(),
        );
        assert_eq!(report.risk_level, Risk::Low);
    }

    #[test]
    fn bulk_load_resolves_the_target_table() {
        let report = report("LOAD DATA INFILE '/tmp/x.csv' INTO TABLE public.orders");
        assert_eq!(report.statements[0].target.as_deref(), Some("orders"));
        assert_eq!(report.statements[0].kind, "LOAD");
    }

    #[test]
    fn shapes_that_cannot_parse_anywhere_are_flagged() {
        // 用户实测过的原句：`*` 被从 SELECT 带进了 DELETE，服务端必然报语法错误，
        // 之前却被判成 Low 一路放行。
        let report = report("delete * from user where id = 3");
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == "malformedStatement" && f.risk == Risk::Medium));
        assert_eq!(report.risk_level, Risk::Medium);
        for sql in [
            "UPDATE accounts level = 1",
            "INSERT INTO logs (a) (1)",
            "DELETE orders",
        ] {
            assert!(
                codes(sql).contains(&"malformedStatement".to_string()),
                "{sql} should not look well-formed"
            );
        }
    }

    #[test]
    fn well_formed_and_dialect_specific_shapes_are_not_flagged() {
        // 这四条都是**合法**的：`SELECT *` 的 `*` 不在 DELETE 之后；MySQL 的
        // 多表删除把目标写在 DELETE 和 FROM 之间；MySQL 支持 INSERT ... SET。
        for sql in [
            "SELECT * FROM users",
            "DELETE a FROM a JOIN b ON a.id = b.id",
            "INSERT INTO t SET a = 1",
            "INSERT INTO t DEFAULT VALUES",
            "DELETE FROM \"from\" WHERE id = 1",
        ] {
            assert!(
                !codes(sql).contains(&"malformedStatement".to_string()),
                "{sql} is valid somewhere and must not be flagged"
            );
        }
    }

    #[test]
    fn full_width_punctuation_is_flagged_because_it_is_not_syntax() {
        // 中文输入法最常见的一次失误：句尾那个「；」。它不切语句，所以整块被当成
        // 一条评估，第二条的风险就这样被藏了。
        let merged = report("UPDATE t_user SET a = 1；UPDATE t_user SET b = 2；");
        assert_eq!(
            merged.statement_count, 1,
            "a full-width semicolon must not be treated as a separator"
        );
        let hit = merged
            .findings
            .iter()
            .find(|finding| finding.code == "fullWidthPunctuation")
            .expect("the full-width semicolon has to be reported");
        assert_eq!(hit.risk, Risk::Medium);
        assert!(hit.message.contains('；'), "{}", hit.message);

        for sql in [
            "UPDATE t_user SET a = 1，b = 2 WHERE id = 1",
            "INSERT INTO t_user （id） VALUES (1)",
            "UPDATE t_user SET a = ‘x’ WHERE id = 1",
            "UPDATE t_user SET a = 1 － 1 WHERE id = 1",
            // 全角数字不是 `is_alphabetic`，所以它在分词里就是标点，照样抓得到。
            "SELECT id FROM t_user WHERE id = １",
        ] {
            assert!(
                codes(sql).contains(&"fullWidthPunctuation".to_string()),
                "{sql} carries full-width punctuation"
            );
        }
    }

    #[test]
    fn full_width_characters_that_are_data_are_not_flagged() {
        // 误伤一次，用户就会把这个规则关掉。中文表名列名、字面量里的中文、注释里的
        // 中文标点，全都不是语法错误。
        for sql in [
            "UPDATE 用户 SET 备注 = '中文，里面有全角标点；' WHERE id = 1",
            "-- 说明：这里的全角分号是注释\nUPDATE t_user SET a = 1 WHERE id = 2",
            "UPDATE t_user SET remark = '好！' WHERE id = 1",
            // 已知没盖住的：全角字母满足 `is_alphabetic`，在分词里和合法的中文标识符
            // 一模一样。钉在测试里是为了别把它误记成「已经全覆盖」。
            "SELECT id FROM t_user WHERE id = Ａ",
        ] {
            assert!(
                !codes(sql).contains(&"fullWidthPunctuation".to_string()),
                "{sql} is legal writing, not an IME slip"
            );
        }
    }

    #[test]
    fn an_ime_slip_does_not_refuse_the_audit_write() {
        // 和 malformedStatement 同一条原则：写得不对也是发生过的意图。
        assert!(codes("UPDATE t_user SET a = 1；").contains(&"fullWidthPunctuation".to_string()));
        assert!(check_recordable(&report("UPDATE t_user SET a = 1；")).is_ok());
    }

    #[test]
    fn comma_separated_update_targets_are_not_single_table() {
        // MySQL 的多表 UPDATE 把清单写在 update 和 set 之间，没有 FROM 可看。早先
        // 只盯 FROM，于是拿 t_user 的列清单去判一条跨两张表的语句，造出假阳性。
        let list = facts("t_user", &[("id", Some(false)), ("nickname", Some(true))]);
        for sql in [
            "UPDATE t_user a, t_order b SET a_nicknae = 1 WHERE a.id = b.id",
            "UPDATE t_user a JOIN t_order b ON a.id = b.id SET a_nicknae = 1",
            "DELETE FROM t_user USING t_order b WHERE b.id = nicknae",
        ] {
            assert!(
                !codes_with(sql, list.clone()).contains(&"unknownColumn".to_string()),
                "{sql} spans more than one table and must not be judged by one table's columns"
            );
        }

        // 反过来：单表写法仍然要照常升级，不能被新加的定界符误伤成「多表」。
        assert!(codes_with("UPDATE t_user SET nicknae = 1 WHERE id = 1", list.clone())
            .contains(&"unknownColumn".to_string()));
        assert!(
            !codes_with("UPDATE t_user SET nickname = 'x' WHERE id IN (1, 2, 3)", list.clone())
                .contains(&"unknownColumn".to_string()),
            "an IN list is not a table list"
        );

        // Postgres / SQL Server 的 `UPDATE 目标 SET 列 = … FROM 来源`：SET 的目标列只能
        // 属于被更新的那张表，所以来源表不构成歧义，写错的列名照样要报。
        assert!(codes_with(
            "UPDATE t_user SET nicknae = 1 FROM t_order WHERE t_user.id = t_order.id",
            list
        )
        .contains(&"unknownColumn".to_string()));
    }

    #[test]
    fn redaction_strips_strings_and_comments_but_keeps_identifiers_and_numbers() {
        assert_eq!(
            redact_literals("INSERT INTO t (a, b) VALUES ('secret', 42)"),
            "INSERT INTO t (a, b) VALUES ('?', 42)"
        );
        assert_eq!(
            redact_literals("SELECT \"User Phone\" FROM t1 WHERE col2 = 'x' -- pw=hunter2\n"),
            "SELECT \"User Phone\" FROM t1 WHERE col2 = '?' -- ?\n"
        );
        // 数字是影响范围的唯一线索，必须留下来。
        assert_eq!(
            redact_literals("UPDATE t SET a = 1 /* token=abc */ WHERE id = 2"),
            "UPDATE t SET a = 1 /* ? */ WHERE id = 2"
        );
        assert_eq!(
            redact_literals("DELETE FROM orders WHERE created_at < '2020-01-01' LIMIT 100"),
            "DELETE FROM orders WHERE created_at < '?' LIMIT 100"
        );
        assert_eq!(redact_literals("SELECT * FROM `odd table`"), "SELECT * FROM `odd table`");
    }

    #[test]
    fn redaction_does_not_change_the_risk_verdict() {
        // 哈希取的是原文，所以只有字面量不同的两条语句仍然能被区分开
        // （脱敏只影响入库的 sql 列，不影响 sqlHash）。
        let original = report("DELETE FROM t WHERE token = 'aaa'");
        let other = report("DELETE FROM t WHERE token = 'bbb'");
        assert_ne!(original.sql_hash, other.sql_hash);
        assert_eq!(original.risk_level, other.risk_level);
    }

    #[test]
    fn quoted_identifiers_and_pg_casts_tokenize() {
        let report = report("SELECT \"user phone\" FROM \"Public\".\"Users\" WHERE id::text = '1'");
        assert_eq!(report.statements[0].target.as_deref(), Some("users"));
        assert!(report.findings.iter().any(|f| f.code == "sensitiveAccess"));
        assert!(!report
            .findings
            .iter()
            .any(|f| f.code == "unboundedScan"));
    }

    #[test]
    fn truncated_statements_are_flagged_incomplete() {
        // 用户实测撞到的那几种：写到一半就点了"审计留痕"。
        for sql in [
            "DELETE FROM",
            "UPDATE t SET",
            "SELECT * FROM t WHERE",
            "DROP TABLE",
            "INSERT INTO",
            "TRUNCATE",
            "SELECT",
        ] {
            let report = report(sql);
            assert!(
                report.findings.iter().any(|f| f.code == INCOMPLETE_FINDING),
                "`{sql}` should be flagged as incomplete, got {:?}",
                report.findings.iter().map(|f| &f.code).collect::<Vec<_>>()
            );
            assert_eq!(report.risk_level, Risk::High, "`{sql}` should be High");
            assert!(check_recordable(&report).is_err(), "`{sql}` must not be recordable");
        }
    }

    #[test]
    fn complete_statements_pass_the_gate() {
        for sql in [
            "DELETE FROM orders WHERE id = 1",
            "UPDATE t SET a = 1 WHERE b = 2",
            "DROP TABLE users",
            "SELECT * FROM t ORDER BY a DESC",
            "SELECT a FROM t LIMIT 10",
            "INSERT INTO t (a) VALUES (1)",
            "CREATE OR REPLACE VIEW v AS SELECT 1",
            // CASE 以 END 收尾是完整的，`end` 刻意不在悬空关键字表里。
            "UPDATE t SET a = CASE WHEN b > 1 THEN 2 ELSE 3 END WHERE id = 4",
            "BEGIN",
            "COMMIT",
        ] {
            let report = report(sql);
            assert!(
                check_recordable(&report).is_ok(),
                "`{sql}` should be recordable, got {:?}",
                report.findings.iter().map(|f| &f.code).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn quoted_identifier_that_looks_like_a_keyword_is_not_truncated() {
        // 只认 Tok::Word：带引号的 "on" 是表名，不是悬空的关键字。
        let report = report("DELETE FROM \"on\" WHERE id = 1");
        assert!(!report.findings.iter().any(|f| f.code == INCOMPLETE_FINDING));
        assert!(check_recordable(&report).is_ok());
    }

    #[test]
    fn comment_only_input_has_no_statements_and_is_refused() {
        let report = report("-- 只是一句注释\n/* 还有一段 */");
        assert_eq!(report.statement_count, 0);
        let reason = check_recordable(&report).expect_err("comments alone must be refused");
        assert!(reason.contains("only whitespace and comments"), "got: {reason}");
    }

    #[test]
    fn gate_names_which_statement_is_truncated() {
        let report = report("SELECT a FROM t WHERE id = 1; DELETE FROM");
        let reason = check_recordable(&report).expect_err("second statement is truncated");
        assert!(reason.contains("statement #2"), "got: {reason}");
        // 第一条是完整的，不能被顺带点名。
        assert!(!reason.contains("statement #1"), "got: {reason}");
    }

    // 宿主元数据（getTableMetadata）参与的判定：这一组用例的重点不是"命中了没有"，而是**没有元数据时必须什么都没发生**——
    // 这些规则是可选升级，不能把静态分析原本的结论改严或改松。

    fn facts(table: &str, columns: &[(&str, Option<bool>)]) -> Option<TableFacts> {
        Some(TableFacts {
            table: table.to_string(),
            columns: columns
                .iter()
                .map(|(name, nullable)| ColumnFact { name: name.to_string(), nullable: *nullable })
                .collect(),
        })
    }

    fn report_with(sql: &str, metadata: Option<TableFacts>) -> RiskReport {
        analyze(sql, &RuleSet::default(), &AnalyzeContext { table_facts: metadata, ..Default::default() })
    }

    fn codes_with(sql: &str, metadata: Option<TableFacts>) -> Vec<String> {
        report_with(sql, metadata).findings.iter().map(|f| f.code.clone()).collect()
    }

    #[test]
    fn a_set_target_the_host_does_not_list_is_flagged() {
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(true))]);
        assert!(codes_with("UPDATE accounts SET prom = 1 WHERE id = 3", metadata)
            .contains(&"unknownColumn".to_string()));
    }

    #[test]
    fn a_real_column_is_not_flagged_as_unknown() {
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(true))]);
        assert!(!codes_with("UPDATE accounts SET level = 2 WHERE id = 3", metadata)
            .contains(&"unknownColumn".to_string()));
    }

    #[test]
    fn nullable_unknown_is_never_guessed_as_not_null() {
        // 宿主没给可空性时判 High 等于凭空造风险，所以这里必须没有 nullIntoNotNull。
        let metadata = facts("accounts", &[("id", None), ("level", None)]);
        assert!(!codes_with("UPDATE accounts SET level = NULL WHERE id = 3", metadata)
            .contains(&"nullIntoNotNull".to_string()));
    }

    #[test]
    fn writing_null_into_a_not_null_column_is_high() {
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(false))]);
        let report = report_with("UPDATE accounts SET level = NULL WHERE id = 3", metadata);
        assert!(report.findings.iter().any(|f| f.code == "nullIntoNotNull"));
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.requires_approval);
    }

    #[test]
    fn qualified_and_multi_table_writes_are_left_alone() {
        // 多表 UPDATE 里 `o.total` 属于另一张表，拿 accounts 的列清单判它就是假阳性。
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(false))]);
        let joined = codes_with("UPDATE accounts a JOIN orders o ON a.id = o.id SET o.note = NULL", metadata.clone());
        assert!(!joined.contains(&"unknownColumn".to_string()));
        assert!(!joined.contains(&"nullIntoNotNull".to_string()));
        // 限定名（`a.note`）即便目标表匹配也不判：词法层分不出限定符指向谁。
        let qualified = codes_with("UPDATE accounts SET a.note = 1 WHERE id = 3", metadata);
        assert!(!qualified.contains(&"unknownColumn".to_string()));
    }

    #[test]
    fn facts_about_a_different_table_are_ignored() {
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(false))]);
        let codes = codes_with("UPDATE orders SET level = NULL WHERE id = 3", metadata);
        assert!(!codes.contains(&"unknownColumn".to_string()));
        assert!(!codes.contains(&"nullIntoNotNull".to_string()));
    }

    #[test]
    fn quoted_and_qualified_table_names_still_match_the_facts() {
        let metadata = facts("public.ACCOUNTS", &[("id", Some(false)), ("level", Some(false))]);
        assert!(codes_with("UPDATE \"accounts\" SET level = NULL WHERE id = 3", metadata)
            .contains(&"nullIntoNotNull".to_string()));
    }

    #[test]
    fn insert_column_list_is_checked_but_a_value_only_insert_is_not() {
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(true))]);
        assert!(codes_with("INSERT INTO accounts (id, prom) VALUES (1, 2)", metadata.clone())
            .contains(&"unknownColumn".to_string()));
        // 没写列清单时没有任何"列名"可比对，不能瞎猜列的顺序。
        assert!(!codes_with("INSERT INTO accounts VALUES (1, 2)", metadata)
            .contains(&"unknownColumn".to_string()));
    }

    #[test]
    fn select_star_expands_into_host_known_sensitive_columns() {
        let metadata = facts("accounts", &[("id", Some(false)), ("phone", Some(true))]);
        let report = report_with("SELECT * FROM accounts WHERE id = 3", metadata);
        let hit = report
            .findings
            .iter()
            .find(|f| f.code == "sensitiveAccess")
            .expect("SELECT * must surface the host's phone column");
        assert!(hit.message.contains("phone"), "got: {}", hit.message);
    }

    #[test]
    fn without_facts_the_metadata_rules_stay_silent() {
        // 同一条语句，没有元数据时既不能多判也不能少判静态规则。
        let sql = "UPDATE accounts SET prom = NULL WHERE id = 3";
        let bare = codes_with(sql, None);
        assert!(!bare.contains(&"unknownColumn".to_string()));
        assert!(!bare.contains(&"nullIntoNotNull".to_string()));
        let upgraded = codes_with(sql, facts("accounts", &[("id", Some(false))]));
        // 升级只增加元数据带来的 finding，静态那一条（noWhereDml 之类）不能因此消失。
        for code in &bare {
            assert!(upgraded.contains(code), "static finding {code} disappeared");
        }
        assert!(upgraded.contains(&"unknownColumn".to_string()));
    }

    #[test]
    fn a_missing_or_single_column_fact_list_is_not_used() {
        // 列清单为空 = 宿主没返回有用的东西；这种情况整套规则不触发。
        assert_eq!(codes_with("UPDATE accounts SET prom = 1", facts("accounts", &[])), codes_with("UPDATE accounts SET prom = 1", None));
    }

    #[test]
    fn metadata_findings_do_not_block_the_audit_write() {
        // 和 malformedStatement 同一条原则：判错列、写 NULL 都是**发生过**的意图，
        // 链里该留着；只有半成品才拒绝落库。
        let metadata = facts("accounts", &[("id", Some(false)), ("level", Some(false))]);
        let report = report_with("UPDATE accounts SET level = NULL WHERE id = 3", metadata);
        assert!(check_recordable(&report).is_ok());
    }

    // 宿主计划（explainPlan）参与的判定：这里唯一要守住的性质是**优先级**——宿主的数字必须压过自报的数字，否则「把
    // estimatedRows 填成 0」就成了一条让大扫描规则闭嘴的后门。反过来，没有计划时结论必须和以前一模一样——这是可选升级，不是新的默认。

    fn plan(rows: Option<i64>) -> Option<PlanFacts> {
        Some(PlanFacts {
            db_type: Some("postgres".to_string()),
            db_version: None,
            supports_estimated_plan: true,
            estimated_rows: rows,
            rewritten: false,
        })
    }

    fn rewritten_plan(rows: Option<i64>) -> Option<PlanFacts> {
        Some(PlanFacts { rewritten: true, ..plan(rows).expect("plan() never returns None") })
    }

    fn report_with_plan(sql: &str, host_rows: Option<i64>, self_reported: Option<i64>) -> RiskReport {
        analyze(
            sql,
            &RuleSet::default(),
            &AnalyzeContext { estimated_rows: self_reported, plan: plan(host_rows), ..Default::default() },
        )
    }

    #[test]
    fn a_host_plan_row_estimate_overrides_a_self_reported_zero() {
        // 自报 0 + 宿主 50 万：必须按宿主的走，并且消息里点明来源。
        // 语句本身挑的是静态层面完全无害的形状（有 WHERE、不取敏感列），这样
        // `risk == Medium` 断言的才是「行数把 Low 抬到了 Medium」这一件事。
        let report = report_with_plan("SELECT id FROM big_table WHERE id = 3", Some(500_000), Some(0));
        let hit = report
            .findings
            .iter()
            .find(|f| f.code == "largeAffectedRows")
            .expect("the host's estimate must win over a self-reported 0");
        assert!(hit.message.contains("host's estimated plan"), "got: {}", hit.message);
        assert_eq!(hit.risk, Risk::Medium);
    }

    #[test]
    fn a_plan_estimate_escalates_an_already_medium_statement_to_high() {
        // 静态层面已经是 Medium（没有 WHERE 的 DELETE），加上大行数就该是 High。
        let report = report_with_plan("DELETE FROM accounts", Some(900_000), None);
        assert_eq!(report.risk_level, Risk::High);
        assert!(report.requires_approval);
        assert!(report.findings.iter().any(|f| f.code == "largeAffectedRows"));
    }

    #[test]
    fn a_self_reported_estimate_is_still_used_when_the_plan_has_no_number() {
        // 计划读回来了但没有行数（非 JSON、被截断）：退回自报值，且消息里不能
        // 出现「来自宿主计划」这种归因。
        let report = report_with_plan("DELETE FROM accounts", None, Some(700_000));
        let hit = report
            .findings
            .iter()
            .find(|f| f.code == "largeAffectedRows")
            .expect("self-reported estimate must still count");
        assert!(!hit.message.contains("host's"), "got: {}", hit.message);
    }

    #[test]
    fn a_small_plan_estimate_adds_nothing() {
        // 计划说只有 12 行：这条规则不该出现，静态结论也不许因此变松。
        let with_plan = report_with_plan("DELETE FROM accounts", Some(12), None);
        assert!(!with_plan.findings.iter().any(|f| f.code == "largeAffectedRows"));
        let without = analyze("DELETE FROM accounts", &RuleSet::default(), &AnalyzeContext::default());
        assert_eq!(with_plan.risk_level, without.risk_level);
    }

    #[test]
    fn a_rewrite_estimate_names_the_rewrite() {
        // 数字来自插件改写的那条 SELECT：消息必须说出来，而且要说它来自宿主、
        // 不是编的。审批人读到的每一句「来自计划」都要能对上一个他看得见的对象。
        let report = analyze(
            "DELETE FROM accounts",
            &RuleSet::default(),
            &AnalyzeContext {
                estimated_rows: None,
                plan: rewritten_plan(Some(900_000)),
                ..Default::default()
            },
        );
        let hit = report
            .findings
            .iter()
            .find(|f| f.code == "largeAffectedRows")
            .expect("a rewrite estimate still counts as a host number");
        assert!(hit.message.contains("SQL Guard wrote"), "got: {}", hit.message);
        assert!(hit.message.contains("not of this statement"), "got: {}", hit.message);
        // 改写不改变判定，只改变说法：等级必须和没标记时一致。
        let plain = report_with_plan("DELETE FROM accounts", Some(900_000), None);
        assert_eq!(hit.risk, plain.findings.iter().find(|f| f.code == "largeAffectedRows").expect("hit").risk);
    }

    #[test]
    fn a_payload_without_the_rewrite_flag_still_reads_as_unmarked() {
        // 0.2.x 的 UI 不带 `rewritten`。`#[serde(default)]` 是这条升级路径唯一的前提：
        // 少一个字段不能变成"解析失败"，也不能默认成 true 而把所有计划数字都说成改写。
        let facts: PlanFacts = serde_json::from_str(
            r#"{"dbType":"postgres","supportsEstimatedPlan":true,"estimatedRows":12}"#,
        )
        .expect("an old planFacts payload must still parse");
        assert!(!facts.rewritten);
    }

    // 只读计划探针：「写语句拿不到行数」是 0.2.1 之前的错判，拿不到的只是**把 UPDATE 原样发过去**。
    // 单表写语句的影响行数定义上就是它的谓词在目标表上匹配的行数，那条 SELECT 是只读的。
    // 于是全部的判断都落在"什么时候这个等式会撒谎"上，下面每一个断言都在钉一条边界：探针切错一处，
    // 发给用户数据库的就是一条语法错误，或者更糟——一个合法但无关的数。

    fn summary(sql: &str) -> StatementSummary {
        let report = report(sql);
        assert_eq!(report.statements.len(), 1, "expected one statement in: {sql}");
        report.statements.into_iter().next().expect("just asserted one")
    }

    fn probe_of(sql: &str) -> String {
        summary(sql)
            .plan_probe
            .unwrap_or_else(|| panic!("expected a probe for: {sql}"))
            .sql
    }

    fn basis_of(sql: &str) -> String {
        summary(sql)
            .plan_probe
            .unwrap_or_else(|| panic!("expected a probe for: {sql}"))
            .basis
    }

    fn refusal_of(sql: &str) -> String {
        let item = summary(sql);
        assert!(item.plan_probe.is_none(), "expected no probe for {sql}, got {:?}", item.plan_probe);
        item.plan_probe_refused.expect("expected a reason code")
    }

    #[test]
    fn a_read_only_statement_probes_itself_verbatim() {
        // 只读语句不需要任何改写，宿主要计划的本来就是这条。
        assert_eq!(probe_of("SELECT id FROM users WHERE a = 1"), "SELECT id FROM users WHERE a = 1");
        assert_eq!(basis_of("SELECT id FROM users WHERE a = 1"), "self");
        // 原文照抄，连大小写都不动：分词器把裸词转了小写，而 `"myTable"` 在 Linux 的
        // MySQL 上就是另一张表。
        assert_eq!(probe_of("select Id From Users Where a = 1"), "select Id From Users Where a = 1");
        // 用户自己的分号也照样要过那两道公共检查。
        assert_eq!(refusal_of("SELECT note FROM t WHERE note = 'a;b'"), "semicolonInText");
    }

    #[test]
    fn a_delete_probe_is_its_predicate_over_its_target() {
        assert_eq!(
            probe_of("DELETE FROM orders WHERE status = 'paid'"),
            "SELECT * FROM orders WHERE status = 'paid'"
        );
        assert_eq!(basis_of("DELETE FROM orders WHERE status = 'paid'"), "predicate");
        // 别名要跟着进 FROM，否则谓词里的 `o.id` 无处可解。
        assert_eq!(
            probe_of("DELETE FROM orders AS o WHERE o.id = 7"),
            "SELECT * FROM orders AS o WHERE o.id = 7"
        );
    }

    #[test]
    fn a_write_without_where_probes_the_whole_table() {
        // 这是那次争论的核心：没有 WHERE 就是全表，而"全表"到底是 400 行还是
        // 4000 万行，静态规则永远说不出来，宿主的统计信息说得出来。
        assert_eq!(probe_of("DELETE FROM orders"), "SELECT * FROM orders");
        assert_eq!(basis_of("DELETE FROM orders"), "wholeTable");
        assert_eq!(probe_of("UPDATE accounts SET balance = 0"), "SELECT * FROM accounts");
        assert_eq!(basis_of("UPDATE accounts SET balance = 0"), "wholeTable");
    }

    #[test]
    fn an_update_probe_stops_at_set() {
        // 这条是回归测试：右界一旦用 `where` 而不是 `set`，赋值部分就被切进 FROM，
        // 拼出 `SELECT * FROM accounts SET balance = 0 WHERE id = 7` 那种东西。
        assert_eq!(
            probe_of("UPDATE accounts SET balance = 0 WHERE id = 7"),
            "SELECT * FROM accounts WHERE id = 7"
        );
        assert_eq!(basis_of("UPDATE accounts SET balance = 0 WHERE id = 7"), "predicate");
        // 赋值里写子查询也不影响表清单。
        assert_eq!(
            probe_of("UPDATE accounts SET balance = (SELECT max(b) FROM ledgers) WHERE id = 7"),
            "SELECT * FROM accounts WHERE id = 7"
        );
    }

    #[test]
    fn a_nested_where_does_not_end_the_predicate() {
        // 保留括号就是为了让这里分得出顶层和嵌套：检测器用的流没有括号，只有这一条
        // 流有。切错的话谓词会在子查询的 WHERE 之前断掉，剩下的部分被丢掉。
        let sql = "DELETE FROM t WHERE id IN (SELECT id FROM u WHERE x = 1) AND y = 2";
        let probe = probe_of(sql);
        assert_eq!(probe, "SELECT * FROM t WHERE id IN (SELECT id FROM u WHERE x = 1) AND y = 2");
        assert!(probe.ends_with("AND y = 2"), "predicate got cut short: {probe}");
    }

    #[test]
    fn the_probe_stops_before_returning_and_friends() {
        // RETURNING 挑的是写完回传哪些列，OPTION 是 SQL Server 的执行参数，
        // FOR 会把只读探针变成锁请求——三个都不该进 SELECT。
        assert_eq!(probe_of("DELETE FROM t WHERE a = 1 RETURNING id"), "SELECT * FROM t WHERE a = 1");
        assert_eq!(
            probe_of("UPDATE t SET x = 1 WHERE a = 2 OPTION (RECOMPILE)"),
            "SELECT * FROM t WHERE a = 2"
        );
    }

    #[test]
    fn row_limits_refuse_the_probe() {
        // `UPDATE … LIMIT n` 真实影响 min(匹配数, n)，SELECT 只会高估。
        assert_eq!(refusal_of("DELETE FROM t WHERE a = 1 LIMIT 10"), "limit");
        assert_eq!(refusal_of("UPDATE t SET x = 1 WHERE a = 2 LIMIT 3"), "limit");
        assert_eq!(refusal_of("UPDATE t SET x = 1 OFFSET 5"), "limit");
    }

    #[test]
    fn multi_table_writes_refuse_the_probe() {
        assert_eq!(refusal_of("DELETE FROM t USING u WHERE t.id = u.id"), "multiTable");
        assert_eq!(refusal_of("UPDATE a JOIN b ON a.id = b.id SET a.x = 1"), "multiTable");
        // MySQL 的逗号写法。这三个是探针唯一一处"放过就等于撒谎"的形状：
        // `SELECT * FROM t, u WHERE …` 语法完全合法，宿主不会拒，算出来是笛卡尔积。
        assert_eq!(refusal_of("UPDATE t, u SET t.x = 1 WHERE t.id = u.id"), "multiTable");
        assert_eq!(refusal_of("DELETE t, u FROM t, u WHERE t.id = u.id"), "multiTable");
        // 列清单和 IN 列表里的逗号不算表清单。
        assert_eq!(
            probe_of("INSERT INTO archive (a, b) SELECT a, b FROM events WHERE day = 1"),
            "SELECT a, b FROM events WHERE day = 1"
        );
        assert_eq!(
            probe_of("DELETE FROM t WHERE id IN (1, 2, 3)"),
            "SELECT * FROM t WHERE id IN (1, 2, 3)"
        );
    }

    #[test]
    fn a_source_table_after_set_refuses_the_probe() {
        assert_eq!(
            refusal_of("UPDATE t SET x = 1 FROM u WHERE t.id = u.id"),
            "updateFrom"
        );
        assert_eq!(refusal_of("UPDATE t SET x = 1 WHERE CURRENT OF c"), "cursor");
    }

    #[test]
    fn inserts_probe_their_source_rows_only() {
        assert_eq!(
            probe_of("INSERT INTO archive SELECT * FROM events WHERE day = 1"),
            "SELECT * FROM events WHERE day = 1"
        );
        assert_eq!(basis_of("INSERT INTO archive SELECT * FROM events WHERE day = 1"), "source");
        assert_eq!(refusal_of("INSERT INTO t (a) VALUES (1)"), "valuesOnly");
        assert_eq!(refusal_of("INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO NOTHING"), "conflict");
        // `ON` 单独出现是 JOIN 条件，不是冲突子句：那种插入的行数照样可估。
        assert_eq!(
            probe_of("INSERT INTO t (a) SELECT a FROM x JOIN y ON x.id = y.id WHERE q = 1"),
            "SELECT a FROM x JOIN y ON x.id = y.id WHERE q = 1"
        );
    }

    #[test]
    fn statements_without_a_row_count_have_no_probe() {
        assert_eq!(refusal_of("DROP TABLE t"), "notRowEstimable");
        assert_eq!(refusal_of("CREATE INDEX i ON t (a)"), "notRowEstimable");
        // 半成品：切不出表清单，也就没什么可发的。
        assert_eq!(refusal_of("DELETE FROM"), "noTarget");
        assert_eq!(refusal_of("UPDATE t WHERE a = 1"), "noTarget");
    }

    #[test]
    fn full_width_punctuation_refuses_the_probe() {
        // 全角分号不切语句，所以这一整块在切分器里就是**一条**语句；切出来的片段
        // 会带着那条中文逗号，发给宿主只会是一次看不懂的失败。
        assert_eq!(refusal_of("UPDATE t SET a = 1；UPDATE t SET b = 2；"), "fullWidthPunctuation");
    }

    #[test]
    fn the_probe_never_carries_a_second_statement() {
        // 探针是发给用户数据库的，一旦把邻居的语句切进来，事情就完全变了。
        let report = report("DELETE FROM a WHERE x = 1; UPDATE b SET y = 2 WHERE z = 3");
        assert_eq!(report.statements.len(), 2);
        assert_eq!(report.statements[0].plan_probe.as_ref().expect("a").sql, "SELECT * FROM a WHERE x = 1");
        assert_eq!(report.statements[1].plan_probe.as_ref().expect("b").sql, "SELECT * FROM b WHERE z = 3");
    }

    #[test]
    fn bracket_only_groups_do_not_desync_the_two_streams() {
        // 成对切分存在的理由：`();` 去掉括号就是空组，检测器那条流里没有它。
        // 如果两条流各自过滤再按下标配对，第 2 条语句会拿到第 3 条的探针。
        let report = report("(); DELETE FROM t WHERE a = 1");
        assert_eq!(report.statements.len(), 1, "{:?}", report.statements);
        let item = &report.statements[0];
        assert_eq!(item.kind, "DELETE");
        assert_eq!(item.plan_probe.as_ref().expect("probe").sql, "SELECT * FROM t WHERE a = 1");
    }

    #[test]
    fn a_probe_is_never_a_write() {
        // 探针唯一的用途是发给只读的计划门，所以它必须自己就像只读的。
        // 这里不证明引擎判得对，只证明它没有把 UPDATE 原样漏出去。
        for sql in [
            "DELETE FROM t WHERE a = 1",
            "UPDATE t SET a = 1 WHERE b = 2",
            "INSERT INTO t SELECT * FROM u WHERE c = 3",
            "SELECT * FROM t",
            "DELETE FROM orders",
        ] {
            let probe = probe_of(sql);
            let head = probe.split_whitespace().next().unwrap_or("").to_lowercase();
            assert_eq!(head, "select", "probe for {sql} starts with: {probe}");
        }
    }
}
