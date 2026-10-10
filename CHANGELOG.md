# Changelog

Versions in `manifest.json`, `backend/Cargo.toml` and this file are kept in lockstep.
The installable `ui/index.html` is copied from the repository root at package time, so the
UI and the sidecar always ship from the same commit — see [README](README.md#release).

## 0.4.1 — 2026-10-10

Everything that happened after the published `v0.4.0` tag (commit `1ae1e8b`). The engine, the store,
the hash chain and the declared permissions are untouched — every item below is screen copy or where
one line of information about the audit file is placed. The only new code is the `~` folding of that
path and the button that copies it; no rule, no record and no wire method changed shape
(`guard/ping` only gained a field). The version is a patch bump rather than an in-place rebuild because
0.4.0 is already published: upstream allows rebuilding in place only *before* publication.

- **The AI busy line says only `生成中…` / `Generating…`.** It used to append 「宿主会弹窗要你确认」,
  which the AI card's own notice (`ai.notice`, the paragraph above the two buttons) already states —
  so the progress line was repeating a standing fact while you wait. The harness check that a failed
  generation clears the line still matches, and still has to: a stale `生成中…` on screen is a lie
  about work in progress.
  「生成中……宿主会弹窗要你确认」改成只说「生成中…」。后半句是 AI 卡片开头那段说明（`ai.notice`）
  里已经写着的常态，不该在进度行里重复一遍。脚本钉住的那条「生成失败必须清掉生成中」的检查照旧成立，
  也必须照旧成立——界面上挂着一句假的进度就是说谎。
- **The host card is named after what it does.** Its heading was still `What the host actually
  told us` — a name from the probe era, when that card *was* the probe — while what it holds now is
  the row-count read the user asked for: the two manual `connectionId` / table inputs, the dialect and
  plan-support line, and *Read estimated plan rows*, whose raw plan JSON is printed right there. The
  heading is now `Connection metadata & estimated rows` / 「连接元数据与估算行数」, and the paragraph
  under it lost its developer content: the sentence explaining that `context` is
  `{workbenchId, restored, surface}` on other entry points is documentation (it is in
  `docs/verify-on-real-host.md` and `docs/host-integration.md`), so the notice now says only the one
  thing that decides whether the card is usable — *these reads need a connectionId, and the host
  only hands one over when you enter from a table's right-click menu*. Credentials-stay-with-DBX
  stayed. Two harness checks pin the new copy in both dictionaries and forbid a context-shape dump
  in UI text. No logic changed.
  这张卡片的标题还叫「宿主到底告诉了我们什么」——那是它当探针面板时的名字，而现在它装的是用户要求的
  行数的读取：手填的连接/表名两格、方言与计划支持、「读取估算计划行数」，以及宿主返回的原始计划
  JSON。标题改成「连接元数据与估算行数」，标题下面那段话把开发内容拿掉了：解释 `context` 在各入口
  是什么形状属于文档（`docs/` 里已有），界面上只留决定这张卡片能不能用的那一句——这些读取都要
  connectionId，而宿主只在从表的右键菜单进入时才把它递过来。「凭据始终在 DBX 手里」那句保留。
  两条脚本检查钉住两个字典里的新文案，并禁止界面文案里再出现 context 形状。逻辑一行没动。
- **The audit database's path left the header and came back folded.** The line under the plugin name
  used to read `io.github.zaojiaoci.sql-guard 0.4.1 · /home/<user>/.local/share/com.dbx.app/…/sql_guard.db`.
  An absolute path's first segment is the OS username, and the review screen is the thing people
  screenshot and forward, while "which file is this" is a question about the audit trail — so the path
  now sits under the audit list, where the records are. `guard/ping` answers with two forms: `store`
  (absolute, unchanged for anyone already reading it) and `storeDisplay`, which folds the home prefix
  to `~`. The screen prints the folded one, keeps the absolute one in the `title`, and *Copy full path*
  writes the **absolute** version to the clipboard, because a `~` label pasted into a shell is wrong.
  Folding is `fold_home` in `main.rs`: an empty home is left alone (an empty prefix matches every
  path) and a path is folded only when home ends at a directory boundary, so `/home/x` does not turn
  `/home/xavier/…` into `~avier/…`. Copying is capability-guarded (`clipboardWrite`): with no clipboard
  the toast says so and the line is styled `user-select: all`, so one click selects the whole path and
  copying by hand needs no letter-picking. When the sidecar answers nothing, the row says exactly that
  rather than printing an empty path — a blank there reads as "there are no records". This is placement,
  not secrecy: every record already carries an operator, whose fallback is `USER@hostname`, so the
  username was never hidden from a reader of the trail. Seven `guard.js` checks cover it — the header is
  identity-only, the audit row shows the folded form, the raw one is one hover away, both clipboard
  branches say what happened, and the copy writes the raw path. The harness now runs the sidecar with
  `DBX_SQL_GUARD_DB` *and* `HOME` pointing into one throwaway directory, so the two forms are guaranteed
  to differ and "copied the raw path, not the tilde label" is an assertion rather than a tautology. A new
  `e2e.js` invariant asserts that no `sql_guard_*` tool result contains the database location or a `store`
  field — the path must not reach a model provider's context if a debug tool is ever added — and another
  checks `guard/ping` returns both forms. Five Rust tests cover the folding rules.
  页眉那行原来写着插件身份，后面跟着审计库的绝对路径。绝对路径的头一截就是 OS 用户名，而审批界面是
  会被截图外发的东西；「这是哪个文件」问的是留痕本身，所以路径搬到审计列表下面。`guard/ping` 现在给
  两份：`store` 还是绝对路径（已经读它的东西不受影响），新增 `storeDisplay` 把 home 前缀折成 `~`。
  界面显示折叠版，绝对路径挂在 `title` 里，「复制完整路径」写进剪贴板的是**绝对**那份——带 `~` 的粘进
  shell 是错的。折叠逻辑是 `main.rs` 里的 `fold_home`：home 为空就原样返回（空前缀能匹配任何路径），
  且只有 home 正好落在目录边界时才折，所以 `/home/x` 不会把 `/home/xavier/…` 折成 `~avier/…`。复制受
  能力门控（`clipboardWrite`）：宿主没给剪贴板时提示实话实说，那一行同时设成 `user-select: all`，
  点一下整段选中，手动复制不必挑字。sidecar 没答上来时那行直说「没有报告位置」，不留一段空路径让人
  以为记录不存在。这是位置的调整，不是保密：每条记录本来就带操作人，而操作人兜底就是 `USER@hostname`，
  能读留痕的人本来就看得到用户名。`guard.js` 里七条检查覆盖它——页眉只有身份、审计那行显示折叠版、
  原始路径悬停即现、有剪贴板和没剪贴板两条分支都各自说话、复制写的是原始路径；验证脚本现在把 sidecar
  的 `DBX_SQL_GUARD_DB` 和 `HOME` 一起指向同一个临时目录，两份路径因此**必然不同**，「复制的是原始版而
  不是 `~` 版」才是一句断言而不是废话。`e2e.js` 新增一条不变式——任何 `sql_guard_*` 工具的返回里不许
  出现审计库位置或 `store` 字段（将来万一有人加 debug 工具，这条路径不能进模型上下文），另有一条检查
  `guard/ping` 两份都给。折叠规则由五个 Rust 测试覆盖。
- Verification for the whole bump: `cargo test` (106 — 101 before the store-path work, plus the five
  folding tests), `cargo clippy --all-targets -D warnings`, the five jsdom harnesses (`guard`, `plan`,
  `e2e`, `operator`, `manifest`) green twice — once against the release build and once with `SG_BIN=`
  pointing at the **extracted 0.4.1 `.dbxp` binary** — and the packaged `ui/index.html` sha256 equals
  source.
  `manifest.json`, `backend/Cargo.toml` and both READMEs' version line moved together to 0.4.1.
  这一版的验证：`cargo test`（106 条——搬路径之前 101 条，加上新写的五条折叠规则）、
  `cargo clippy --all-targets -D warnings`、五个 jsdom 脚本（guard / plan / e2e / operator / manifest）
  跑两轮（一轮对着 release 构建，一轮 `SG_BIN=` 指向**解包后的 0.4.1 `.dbxp` 二进制**），包里的
  `ui/index.html` sha256 与源码一致；`manifest.json`、`backend/Cargo.toml` 和两个 README 的版本行
  一起升到 0.4.1。

## 0.4.0 — 2026-10-10

Four changes from using it, plus a documentation pass.

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
- **Documentation reorganization.** The two READMEs were 758 and 639 lines; the material that made
  them that long now lives in `docs/` — `review-model.md`, `host-integration.md`,
  `verify-on-real-host.md`, `known-limits.md`, `internals.md`, each bilingual — and the READMEs are
  168 and 148 lines. Moved text was sliced out of the originals and checked byte-for-byte against
  them, so nothing was reworded on the way; the README keeps a compressed limits list and points at
  the full one. Code comments were trimmed in place (526 whole-line comments removed across
  `risk.rs`, `store.rs`, `main.rs`, `model.rs`, `util.rs`, `mcp.rs` and `ui/index.html`) under one
  rule: keep every *why*, delete every restatement of *what*. Verified by re-checking that the files
  are identical once comment-only lines are stripped.
  两个 README 从 758 / 639 行瘦到 168 / 148 行，长内容整段搬进 `docs/`（判定模型、宿主集成、真机
  验证清单、已知限制完整版、内部实现），搬运是逐字切片并按字节核对过的，没有在搬的过程中改写措辞。
  代码注释原地瘦身：七个文件合计删掉 526 行整行注释，规则是「保留所有 why，删掉所有对 what 的
  复述」，校验方式是把整行注释剥掉之后逐字节比对代码（完全一致）。
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
