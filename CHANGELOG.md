# Changelog

Versions in `manifest.json`, `backend/Cargo.toml` and this file are kept in lockstep.
The installable `ui/index.html` is copied from the repository root at package time, so the
UI and the sidecar always ship from the same commit — see [README](README.md#release).

## Documentation reorganization — 2026-10-10 (no version bump)

The two READMEs were 758 and 639 lines; they are now 164 and 144, with the long material moved to
`docs/` — `review-model.md`, `host-integration.md`, `verify-on-real-host.md`, `known-limits.md`,
`internals.md`, each bilingual. Moved text was sliced out of the originals and checked
byte-for-byte against them, so nothing was reworded on the way; the README keeps a compressed
limits list and points at the full one. Code comments were trimmed in place (526 whole-line
comments removed across `risk.rs`, `store.rs`, `main.rs`, `model.rs`, `util.rs`, `mcp.rs` and
`ui/index.html`) under one rule: keep every *why*, delete every restatement of *what*. Verified by
re-checking that the files are identical once comment-only lines are stripped, then
`cargo test` (101), `cargo clippy -D warnings`, and the four jsdom harnesses.
No behaviour changed, so `manifest.json` still says 0.4.0; `dist/` was repackaged to match source.
两个 README 从 758 / 639 行压到 164 / 144 行，长内容整段搬进 `docs/`（判定模型、宿主集成、真机
验证清单、已知限制完整版、内部实现），搬运是逐字切片并按字节核对过的，没有在搬的过程中改写措辞。
代码注释原地瘦身：七个文件合计删掉 526 行整行注释，规则是「保留所有 why，删掉所有对 what 的
复述」。校验方式是把整行注释剥掉之后逐字节比对代码（完全一致），再跑 `cargo test`、
`cargo clippy -D warnings` 和四个 jsdom 脚本。行为没有变化，版本号仍是 0.4.0，`dist/` 重新打包
以匹配源码。

## 0.4.0 — 2026-10-10

Three things from using it, plus one line the plugin was writing about itself.

- **The pre-review opinion is rendered as Markdown.** Model replies are Markdown; flattened into
  one block of text their headings, bullets and code blocks stop carrying meaning. The supported
  subset is headings, both list styles, fenced code, `code`, `**bold**`, `*italic*`, blockquote and
  rule. This is a typography change only — every node still comes from `createElement` +
  `textContent`, there is no `innerHTML` in the path, so `<script>` and `javascript:` in a reply
  stay characters and a link stays a URL you can read. Two rules exist because those replies are
  *about* SQL: a `*` starts emphasis only after whitespace or punctuation, and `_em_` is not
  supported at all, or `update_time` beside `create_time` would italicize the middle of a column
  list.
  预审意见现在按 Markdown 渲染（标题、两种列表、围栏代码、行内码、加粗/斜体、引用、分隔线）。
  这只是排版的改变：节点全部出自 `createElement` + `textContent`，路径里没有 `innerHTML`。星号必须
  在前一个字符是空白或标点时才算强调，`_斜体_` 整个不支持——否则一行里的 `update_time`、
  `create_time` 会把中间那截列名斜体化。
- **Entering from a table's right-click menu leaves a template in the empty SQL box**: a
  **commented-out** `SELECT` naming that table with its real schema, plus the database it lives in.
  Comments are not statements, so the text can be edited into a query but cannot be mistaken for a
  verdict on something the plugin wrote. Related: a comment-only input used to come back as
  *Low risk, auto-approve, 0 statements*, which reads like a check that passed. It now says
  "Nothing to analyze: the input contains only comments and whitespace" and carries no badge at all.
  从表的右键菜单进来时，空框里会留一句**注释掉的** `SELECT`（表名和模式名是宿主给的准确值）。
  顺带修掉一个说谎的组合：只有注释的输入以前会得到「低风险 + 自动通过 + 0 条语句」，现在直接说
  「没有可分析的语句」，并且不挂任何徽章。
- **`explainPlan` no longer borrows scope it was not given.** `database` and `schema` come from
  `context`, which says where the *right-clicked* table lives; they were attached to every plan
  request, including a probe for some other table in that database — so the host searched a
  namespace not containing it and answered "does not exist". That is now sent only when the probe
  actually names that table; anything else uses the connection's own default, and a table that
  genuinely is unreachable still gets the host's own refusal. Whether this is the error that was
  reported is not confirmed — the verbatim message has not been seen; it is the one path the
  plugin controlled, so it was fixed first.
  `database` / `schema` 描述的是**被右键那张表**的位置，之前每次计划请求都带上它，于是同库里另一张表
  的探针会被送到一个不含它的命名空间里去查，换回一句「不存在」。现在只有探针确实点名那张表时才带上。
  这是否就是被反馈的那条报错仍未确认——原话没见过；先修掉插件自己能控制的那一条路径。
- **The AI transport inventory under the two AI buttons is no longer printed.** The line listing
  the host's nine `ai.*` methods and its `ai*` capabilities answered a developer's question ("what
  is the host exposing right now"), not an approver's, and the buttons' own disabled state already
  says what a reviewer needs. `paintAiStatus` keeps building the string behind the existing
  `SHOW_HOST_DIAGNOSTICS` switch, and keeps the one honest failure line — a host with no `ai`
  namespace at all. A harness check that reproduces the real 0.6.38 line verbatim moved with it, so
  the hidden path is still tested.
  「生成预审意见」下面那串「宿主 AI：九个方法名 · capabilities=…」默认不再打印。它答的是开发期的
  问题，不是审批人的问题；两颗按钮亮不亮已经说明审批人需要知道的事。构造那段代码原样留着，靠同一个
  `SHOW_HOST_DIAGNOSTICS` 开关回来，而「宿主根本没有 AI 命名空间」那句实话仍然显示。对着真机 0.6.38
  逐字比对那行清单的脚本检查跟着一起搬进了开关里，隐藏的那条路径照样有测试。
- Storage, the hash chain, the CSV columns, the rule set and the declared permissions are
  unchanged; no new `requires`, so installing it does not re-ask for consent.
  存储、哈希链、CSV 列、规则集和声明的权限都没变，不新增 `requires`。

## 0.3.0 — 2026-10-10

Writes can now get a row count. 0.2.x could only plan a `SELECT`, which meant the statements that
most need a number — `DELETE`, `UPDATE` — were exactly the ones that never got one.

- **`analyze` returns a read-only `planProbe` per statement**, plus a `basis` saying why its row
  count may be believed, or a named `planProbeRefused` reason. `UPDATE t SET … WHERE p` probes
  `SELECT * FROM t WHERE p` (`predicate`), the same without a `WHERE` probes `SELECT * FROM t`
  (`wholeTable`), `INSERT … SELECT q` probes `q` unwrapped (`source`), and a read-only statement
  probes itself (`self`). The equality behind all of it: the rows a write touches are the rows its
  predicate matches. Building the text means cutting the user's SQL at token boundaries, so it
  happens in the engine — the workbench never concatenates SQL, it sends `planProbe.sql` verbatim
  and prints it before claiming any number.
  每条语句现在可能带一个只读**探针**：写语句靠改写出来的 `SELECT` 去要行数，依据是「写语句碰到的
  行就是谓词匹配的行」。拼接要在 token 边界上切原文，只有分词器知道字符串在哪里结束，所以这段逻辑
  在引擎里；界面不拼 SQL，只原样发送并先把它显示出来。
- **Every shape where that equality would lie is refused by name**, not with a shrug:
  `nothingToPlan`, `fullWidthPunctuation`, `limit`, `multiTable`, `updateFrom`, `cursor`,
  `valuesOnly`, `conflict`, `notRowEstimable`, `noTarget`, `semicolonInText`, `tooLong`. The two
  that matter most: a cartesian `UPDATE t, u SET …` rewrites into *legal* SQL with a wildly wrong
  row count — worse than no number — and a `DELETE … LIMIT n` says nothing about the predicate's
  selectivity.
  等式不成立的形状一律具名拒绝。最要紧的两个：笛卡尔积写法会改写出一条合法但行数离谱的语句，那比
  没有数字更糟；带 `LIMIT` 的删除说明不了谓词的选择性。
- **A probe-derived number is attributed as one.** `planFacts.rewritten` travels with the row
  count, the finding says the plan is of *a SELECT SQL Guard wrote, not of this statement*, and the
  approve/reject dialog now repeats the risk level, every finding and where the number came from —
  it was the approver's blind spot: the workbench knew the provenance and the dialog showed a
  button.
  来自改写的数字会被明确标注。审批／驳回弹窗里现在有风险等级、全部命中项和这个行数的出处；以前
  工作台知道来源，弹窗却只有一个按钮。
- **`sql_guard_analyze` redacts the probe before the model sees it.** `guard/analyze` must hand the
  workbench byte-exact text to send to `explainPlan`, so literals stay there. The MCP tool returns
  the same report to the built-in AI, and that text leaves the machine — so every `planProbe.sql`
  goes through the same `redact_literals` the audit write uses. Shape and `basis` reach the model;
  values do not.
  `guard/analyze` 必须交给工作台能原样发送的文本，所以那边保留字面量；MCP 的输出会离开这台机器，
  所以探针过一遍落库用的同一道脱敏。
- **Three host-diagnostic buttons are hidden** behind `SHOW_HOST_DIAGNOSTICS` in `ui/index.html`
  (`true` brings them back; the nodes stay in the DOM, so nothing that sets `.disabled` throws).
  They existed to debug the host bridge and a user has no reason to see them. `#hostAvail` is now
  empty in the normal case — an unusable entry point shows a greyed button and a plan-support line
  instead of a lecture.
  三颗诊断按钮收进 `SHOW_HOST_DIAGNOSTICS` 开关后面，节点留在 DOM 里。可用性那行平时不再说话：
  入口不给力时，一颗灰按钮加一行方言支持就说明白了。
- Storage, the hash chain, the CSV columns, the rule set and the declared host permissions are
  unchanged — this release adds no `requires`, so installing it does not ask for consent again.
  存储、哈希链、CSV 列、规则集和声明的宿主权限都没变，没有新增 `requires`。

## 0.2.1 — 2026-10-10

A UI-only fix. `Read estimated plan rows` looked broken: it was refusing, and saying so where
nobody was looking.

- **Every local refusal is now toasted.** `doReadHostPlan` has seven gates (empty box, no
  capabilities, dialect cannot plan, no report for this exact text, more than one statement, not
  a `SELECT`, no usable row count in the plan). Six of them wrote their reason only into
  `#hostStatus`, a small grey hint *below* the buttons — so clicking did visibly nothing. The
  button was not broken; it was declining quietly. Most people hit the `SELECT`-only gate,
  because the statements they most want a row count for are writes.
  那颗按钮不是坏了，是在没人看的地方拒绝了。七道门里有六道只把原因写进按钮下方的小灰字，
  点下去就是一片安静。最常撞上的是「只读」那道门——因为最需要行数的那几条语句恰恰是写语句。
- **Availability moved to its own line (`#hostAvail`).** It shared `#hostStatus` with action
  results, and the two fought: a standing "still needs a table name" could overwrite the refusal
  you had just read, and a refusal could hide the reason a button was greyed out. They are
  different kinds of sentence and now live in different places. New case covered: when the host
  reports `capabilities.planApi === false`, the card says that out loud instead of leaving two
  grey buttons unexplained.
  「为什么灰着」和「刚才那次点击怎么了」是两类话，抢同一行就会互相盖掉。现在分开了，
  并且补上了一种情况：宿主上报 `planApi === false` 时，卡片会明说，而不是留两颗没人解释的灰按钮。
- No Rust logic changed; `backend/Cargo.toml` is bumped only to keep the three versions in
  lockstep. Findings, rule text, the audit CSV and the hash chain are untouched, and no new host
  permission is requested.
  Rust 逻辑没有改动，`backend/Cargo.toml` 只是为了三处版本号保持一致而跟着升。命中项、规则文本、
  审计 CSV 与哈希链都没碰，也没有申请新的宿主权限。

## 0.2.0 — 2026-10-10

The host's plan API is now part of the verdict, gated on what the host actually supports.

- **`getPlanCapabilities` is consumed.** Once per `connectionId`, cached for the session, and
  painted in the host card: the dialect and version the connection really is, plus whether this
  host can produce an estimated plan at all. `db_type` in the audit record stops being an empty
  column — it becomes "the host said postgres". The engine still never branches on dialect; it
  has no dialect knowledge to branch with.
  每个 `connectionId` 问一次、本会话缓存，并把方言、版本和「这个连接有没有估算计划这条路」
  写在宿主卡片上。记录里的 `db_type` 不再永远是空的。引擎依然不按方言分支。
- **`explainPlan` feeds `largeAffectedRows`.** A separate button, never part of Analyze, asks
  for an estimated plan (`mode: "estimated"` — an actual plan would *run* the statement) and
  reads exactly one number out of it: `Plan Rows` from a non-truncated, warning-free JSON
  plan. Anything else yields no number and says why. That number **overrides** a
  caller-declared `estimatedRows` and the finding names the host as its source, so typing `0`
  is no longer a way to silence the detector on a statement the host can plan.
  单独一颗按钮，Analyze 绝不会替用户按它。只从「未截断、无 warning 的 JSON 计划」里取
  `Plan Rows` 这一个数字；取不到就不产生数字并说明原因。宿主的数字**优先于**调用方自报的
  数字，而且命中项会写明来源，所以填 `0` 让检测器闭嘴这条路对有计划的语句不再成立。
- **The read-only gate is checked locally, before the host is asked.** DBX's plan path rejects
  multi-statement input, DDL and DML for every dialect except Oracle, so `DELETE FROM t` gets a
  sentence explaining that boundary instead of a red error relaying it. Same for a script with
  more than one statement, an unsupported dialect, a connection that never answered, and a
  statement that was never analyzed.
  宿主只读语句才有计划，所以多语句、DML、DDL、方言不支持、连接没回答、没点过 Analyze
  这几种情况全都在本地拦下，并把原因写清楚——把一个必然的拒绝伪装成插件故障是不诚实的。
- **A plan number belongs to one statement and one connection.** It is kept with the exact text
  and the exact `connectionId` it was read for, and retired the moment either changes. A stale
  number rebounded onto an edited statement would be a fabricated fact wearing a host label.
  行数与「哪段文本、哪条连接」绑成三元组，任一变化就立即作废。
- `planFacts` is ignored on the MCP path, like `tableFacts`: the model best placed to want a
  softer verdict is the one best placed to invent a row count.
  和 `tableFacts` 一样，MCP 那条路无视调用方 supplied 的 `planFacts`。
- Plan facts can only **add** `largeAffectedRows`. They never remove, downgrade or replace a
  static finding (a harness check pins that), and never refuse the audit write.
  计划只会新增这一条命中项，不会让任何静态结论变松，也不会拒绝落库。
- No new permission: `host.plans:read` was already declared. No schema change, no new stored
  field; `estimated_rows` and `db_type` existed and were simply never filled from the host.
  没有新增权限、没有改表结构，`estimated_rows` 与 `db_type` 两列本来就有，只是此前没有
  来自宿主的来源。

## 0.1.1 — 2026-10-10

Two defects found by using the plugin, both about statements that reach the audit trail
looking safer than they are.

- **`fullWidthPunctuation`** — a Chinese IME leaves a `；` at the end of a statement. That
  character is not SQL syntax, and statements are split on the ASCII `;` only, so everything
  typed behind it was reviewed as part of the *first* statement: a second, riskier statement
  was hidden inside one Low-looking block and stored that way. Now reported (Medium, not
  user-toggleable, and — like the shape rules — not a reason to refuse the audit write, since
  the intent did happen). Covers `；：，（）－`, curly quotes and even full-width digits like
  `１`; it cannot catch full-width **letters** (`Ａ`), which are alphabetic to the tokenizer and
  so become an ordinary identifier.
  中文输入法留下的全角 `；` 不会切分语句，它后面的内容会被当成前一条语句的一部分评估，
  第二条语句的风险就此被吞掉。现在会把找到的全角标点报出来（Medium，不给开关，也不拒绝落库）。
  全角字母（`Ａ`）是唯一漏掉的一类：分词器认为它是字母，于是和正常的中文标识符无法区分。
- **Multi-table write targets are no longer judged by one table's columns.** MySQL's
  `UPDATE a, b SET col = …` puts the table list between `UPDATE` and `SET`, where the old
  single-table check was not looking, so a `col` belonging to `b` was reported as
  "not in this table". `UPDATE a JOIN b SET …` is excluded for the same reason.
  `UPDATE t SET … FROM x` (Postgres / SQL Server) still counts as single-table: the source
  tables arrive after `SET`, and a write target can only be a column of `t`.
  MySQL 的 `UPDATE a, b SET …` 会把两张表写在 `UPDATE` 和 `SET` 之间，先前的单表判定看不见，
  于是拿着其中一张表的列清单去判跨表语句，报出假阳性。`UPDATE t SET … FROM x` 仍算单表。
- Findings, rule text, the audit CSV and the hash chain are unchanged in shape; old records
  verify as before. Nothing new is stored, and no new host permission is requested.
  命中项的结构、规则文本、审计 CSV 与哈希链都没有变化，旧记录照常校验通过。

## 0.1.0 — initial release

Lexical risk engine, hash-chained SQLite audit trail, workbench UI, three built-in-AI MCP
tools, AI pre-review through the host's own model, redaction that keeps numbers, approver
whitelist, and host table metadata feeding the verdict when the workbench is opened from a
table (`unknownColumn`, `nullIntoNotNull`, `SELECT *` expansion).
词法风险引擎、哈希链审计、工作台界面、三个内置 AI 工具、AI 预审、保留数字的脱敏、审批人白名单，
以及从表上右键进入时用宿主列清单升级判定（`unknownColumn`、`nullIntoNotNull`、`SELECT *` 展开）。
