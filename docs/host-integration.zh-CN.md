# 宿主集成：AI 桥、库表事实、估算计划

回到 [README](../README.zh-CN.md) · [English](host-integration.md)

## AI 预审

审查页有一张 **AI 预审** 卡片，把引擎的命中项翻成一份审批人能直接用的意见。两条路径
都走宿主自己的模型——插件不直连任何 AI 服务，也不持有任何 API key：

- **生成预审意见** → `ai.generateText({ configId, model, prompt })`。需要 `host.ai`
  权限外加 `capabilities.aiCompletion`。宿主每次生成都会弹窗要你确认，且单个工作台
  同时只允许一个生成。
- **在 AI 对话中继续** → `ai.openConversation({ title, prompt, context, send })`。
  只需要 `host.ai`，所以它是解析不出模型时的兜底。

`send: true` 是这个调用里唯一非默认的字段。
对照宿主源码已经查清了真机 0.6.38 上那个现象：`openConversation` 无论有没有选定模型都会 resolve，插件这边
看到的是成功；那句"没有配置 AI"是 DBX 助手面板自己弹的提示（`AiAssistant.vue` 里的
`noConfig` 与 `pluginHttpModelOnly`）。两条路读的是不同的配置：`generateText` 按插件
传过去的 `configId` 解析配置，而对话面板要的是它自己模型菜单里选中的那个——所以预审
能用、对话却提示没配置。
去掉 `send` 的重试仍然保留，它覆盖的是宿主真的拒绝这个调用的那些情况（没声明权限、
对话面板不可用）；成功那一路现在只说"已把报告交给 AI 面板"，并把宿主的原话
显示出来。两次都失败时，卡片会把那句原话和实际发过去的参数形状一起留下：toast 会消失，
而这段文本是唯一能分清"面板里没选模型"和"参数不被接受"的东西。

补救的地方在面板本身，不在设置页：DBX 的助手有自己的模型菜单，插件对话只会用它在那里
选中的模型。如果那里选的是 CLI 类的 agent，宿主会换一句提示（「插件分析请在模型菜单中
选择 HTTP 模型或 Ollama。」），因为插件分析不走 CLI 供应商。`generateText` 不受这两条
影响——这就是"预审意见能出、对话按钮却像没配"的根本原因。

配置在 DBX 那边，不在插件里。在编辑器设置 → **AI** 里添加供应商（Claude、OpenAI、
Gemini、DeepSeek、Qwen、MiniMax、Ollama、OpenAI 兼容端点、Anthropic 兼容端点或自定义；
桌面客户端还会识别 Claude Code CLI、Codex CLI 和 Pi Coding Agent），填 API Key、
认证方式、端点、代理和自定义请求头。之后插件通过 `listModels()` 读你配好的东西，
读不到再退回 `listProviders()` + `discoverModels()`，取标了 default 的那个、否则取
第一个。插件自己没有任何 AI 配置项，也没法替你选供应商——DBX 自己的文档同样没有
定义自动默认。

给模型的内容是：引擎的判定、命中项、语句列表和 SQL。要求它做三件事——用业务语言说清
为什么危险、指出词法引擎看不到的影响面、给出更安全的改写（如果有的话），并用工作台
当前的语言回答。

回复是**渲染**出来的，不是原样贴出来的：模型写的是 Markdown，压成一整块纯文本之后，它的
标题、列表和代码块就都不表达任何东西了。但这只是*排版*层面的决定，那条底线一点没让：每个
节点都由 `createElement` + `textContent` 建出来，这条路径上没有任何 `innerHTML`，所以回复里
出现的 `<script>`、`javascript:` 只会是这些字符；链接也永远不是链接——工作台没有打开外链的
能力，把 URL 原样印出来才看得见有人往你面前塞了什么。另有两条规矩是冲着「回复在讲 SQL」定的：
星号只有在前一个字符是空白或标点时才算强调的起点，而 `_斜体_` 这种写法整个不支持——`update_time`
和 `create_time` 写在同一行时，后者差点把中间那截列名变成斜体。

有三条性质值得说清楚：

- **你的字符串字面量不会离开本机。** prompt 用的是 `redactedSql`，由 `guard/analyze`
  调用入库前同一个 `risk::redact_literals` 生成。`DELETE FROM t WHERE token = 'hunter2'
  AND id = 7` 送到模型那里是 `DELETE FROM t WHERE token = '?' AND id = 7`。这和"审计库
  不当密码本"是同一条原则。数字是故意留下的，模型要靠它判断规模，见
  [审计库里存的是什么](review-model.zh-CN.md#审计库里存的是什么)。
- **它无权审批。** `mcp.rs` 里够不到 `decide`，卡片上也没有任何审批控件，prompt 里
  还明确要求模型不得给出或暗示决定。这条指令刻意放在 SQL **之后**：预算不够时被截掉
  的是 SQL，不是这条约束。
- **它的回复是不可信文本。** 一律用 `textContent` 渲染进 `<pre>`，绝不走 `innerHTML`。

预审意见**不落库**。它不属于 `ChainInput`，影响不了任何摘要，清空表单就没了。想留在
记录里，就把它复制进审批时的 `note`——那个字段本来就在内容链之外。

### 未能在真实宿主上核实的部分

上面的签名和限制都来自上游 `plugins/README.md`。bridge 这一层**已经**核实过了：在 DBX
0.6.38 上，bridge 探测那颗按钮（被 `SHOW_HOST_DIAGNOSTICS` 藏着）报出全部九个方法（`cancelGeneration, clearRecommendations,
discoverModels, generateText, generateTextStream, listModels, listProviders,
openConversation, setRecommendations`），且 `bridge.capabilities` 里 `ai`、
`aiModelDiscovery`、`aiCompletion`、`aiCompletionStream`、`aiRecommendations` 全为 `true`。

这串清单现在默认不印在按钮下面了。它答的是开发期的问题（「宿主此刻暴露了什么」），不是
审批人的问题——两颗按钮亮不亮已经把审批人需要知道的事说完了。把同一个
`SHOW_HOST_DIAGNOSTICS` 打开它就回来；留在界面上的只剩一句实话：宿主根本没有 AI 命名空间。

不知道的范围比之前小了。真机 0.6.38 上配好供应商之后，「生成预审意见」按反馈是可用的，
所以 `listModels`、`generateText` 以及背后那几种返回形状的猜测在实践中站得住。
「在 AI 对话中继续」在同一台机器上被反馈为失败，报错说没有配置 AI；这一条现在查清了，
问题在宿主不在插件：`openConversation` 一定会 resolve，那句提示是 DBX 助手面板自己的
模型菜单没选中任何模型。`listProviders` 和 `discoverModels` 的返回
形状同样还没被真正走到过——上游只说"返回配置 ID、名称和默认标记"。所以：

- 门禁看的是**要调的方法在不在**，而不是 capability 开关。开关为 `true` 只说明宿主支持
  这套 API，不保证调用会成功。
- `asList` / `firstText` / `extractText` 认下几种常见写法；都对不上时直接把原始 JSON
  显示出来，而不是假装模型什么都没说。
- 解析不出模型时，卡片会把**宿主每一步真实返回的内容**打印出来。"配了提供商但字段名
  不一样"和"根本没配提供商"从外面看是同一个结果，只有摊开原文才分得清。发现调用
  自己**抛错**时给的是另一条提示，不和"返回空"混在一起。

`ai.setRecommendations`（快捷提问 chips）和 `ai.generateTextStream`（流式，对应
`aiCompletionStream`）都可用，目前没用上。

## 宿主元数据读取

风险命中项说的是**这段文本**；列名说的是**那个库**。工作台现在会把宿主自己的
回答用作这些事实——只当升级，绝不当前提。

**什么时候会读。** `getTableMetadata` 需要一个 `connectionId`，而判定又必须知道这条
语句讲的是**哪张表**。两样都由入口决定：从某张表的右键菜单进 SQL Guard，`context` 里就
有 `{connectionId, database, schema, table}`，于是按 `(connectionId, database, schema,
table)` 这个组合读一次、在这一页的生命周期里缓存。从插件中心或底部面板进来时，
`context` 是 `{}` 或 `{workbenchId, restored, surface}`——没有 connectionId——
于是根本不读。在「连接元数据与估算行数」卡片里手填连接 id 和表名同样有效，
那两个输入框走的是同一套目标解析。

**允许跨过 RPC 边界的。** 每列只有 `name` 和 `nullable`。`dataType`、`length`、
`default` 是在 `ui/index.html` 里**故意丢掉**的：类型名是每种方言自己拼出来的字符串
（`character varying(20)`、`int4`、`bigint`），把它喂给一个不懂方言的引擎，等于把方言
知识写进一个本来就没有方言知识的引擎。而在实测过的宿主上，`default` 区分不出
`GENERATED`/`IDENTITY` 列和普通的字面量默认值——所以「INSERT 漏了必填列」在这里**不是**
一条规则。这些内容一概不入库：列清单只活在页面内存里，绝不进审计库——那个库按内容哈希
成链，不该变成某张生产表的结构快照。

**它能补什么。** 三条命中项，每一条都要求元数据在位、语句的目标表和元数据描述的是同一张
表、而且语句是单表的：

| 码 | 风险 | 触发条件 |
| --- | --- | --- |
| `unknownColumn` | Medium | `SET` 的目标列或 `INSERT` 的列清单里，出现了宿主没列出来的列 |
| `nullIntoNotNull` | High | 宿主说 `nullable: false` 的列被 `SET col = NULL` |
| `sensitiveAccess` | Medium | `SELECT *` 按宿主的列清单展开后含敏感列（和静态那条投影命中去重） |

`unknownColumn` 只看写入目标，**不看 `WHERE`**：词法引擎分不清子查询里的列、别名、外层
作用域的列和真正的拼写错误，而一句假的「这列不存在」比沉默更糟。

这里的「单表」说的是**写入目标没有歧义**。`UPDATE a JOIN b SET …` 和 MySQL 的
`UPDATE a, b SET …` 都不算：裸写的 `SET col = …` 可能属于其中任意一张表，就不能拿另一张表
的列清单去判它「不存在」。而 `UPDATE t SET … FROM x`（Postgres、SQL Server）算——`SET`
之后的 `FROM` 只带来来源表，写入目标仍然只能是 `t` 的列。拿不准的时候一律按「不是单表」
处理：少一次升级远比一次误判便宜。

**它永远不能做什么。** 元数据命中项不会删掉、降级或替代任何静态命中项——有一条测试
用例把这个不变式钉住了：有没有元数据，静态那部分必须一模一样。它们也不会拒绝写入审计链，
只有 `incompleteStatement` 有那个资格（见[什么进不了审计链](review-model.zh-CN.md#什么进不了审计链)）。
可空性未知就是未知，不会猜成 `NOT NULL`。列清单超过 2000 列、形状不对、或者为空时，
**整体丢掉**而不是截断：短了的清单会让真实存在的列看起来「不存在」，
残缺的元数据比没有元数据更坏。

**MCP 那条路没有元数据。** `sql_guard_analyze` 会忽略调用方自己塞进来的 `tableFacts`——
最想把风险评分压低的那个模型，正是最有可能凭空编一份列清单的那个。

**报告会写明自己是哪种模式跑出来的。** 每份报告都有一行，要么写出用到的表名和列数，
要么写清这次为什么没有元数据（没有连接、没有表名、宿主没暴露这个方法、宿主拒绝了并附上
原话、宿主没返回任何列）。同一段 SQL 因为入口不同而给出不同的命中项，是**合法**的，
但这必须看得见，否则判定结果就像掷骰子。

`getPlanCapabilities` 现在已经被用起来了：`ui/index.html` 按 `connectionId` 问一次，
本会话内缓存，并把方言和版本画在宿主信息卡上。引擎**不会**按 `dbType` 分支——它没有
任何方言知识可以拿来分支。但记录会：`db_type` 那一列从此是"宿主说这条连接是 postgres"，
而不是一个空字段。`supports.estimatedPlan` 决定行数的路走不走得通，见
[宿主的估算计划](#宿主的估算计划)。

## 宿主的估算计划

`host.plans:read` 的原文是"只读宿主生成的预估执行计划；永远不等于执行 SQL、写入、DDL
或拿到真实计划"。上游还写了另一条决定这个插件究竟能拿它做什么的门：**DBX 的计划通路
对所有非 Oracle 方言都拒绝多语句、拒绝 DDL 和 DML**，而且连接必须已经打开。所以能直接
送去计划的只有只读语句——写语句靠的是下面那段改写。

**怎么走到它。** 宿主信息卡上单独一颗按钮，「读取估算计划行数」。它不属于 Analyze，
Analyze 也绝不会去调它。碰宿主之前它先在本地过四道门，并且把没过的那道写出来：这条
连接回答过 `getPlanCapabilities`；那个回答里 `estimatedPlan` 为真；SQL 框里解析出来正好
一条语句；这条语句有一个**计划探针**——引擎愿意替它送去计划的那条只读语句（见下面）。
没有探针的语句换来的是**一句写明原因的话**，而且同时弹成 toast，不是一条红色报错——
否则这个插件就只是在转述一句它自己也无法改变的拒绝，而一颗什么都不做的按钮只会被当成
坏掉的按钮。请求是 `explainPlan({connectionId, sql, mode: "estimated", timeoutMs})`，
`timeoutMs` 收敛到宿主自己给的 `maxTimeoutMs` 以内；`mode` 除了 `"estimated"` 没有别的
合法值，因为真实计划（`EXPLAIN ANALYZE`）会**把这条语句执行一遍**。

`database` 和 `schema` **只在探针点名了那张被右键的表时**才带上。它们来自 `context`，说的
是*那张表*住在哪个库哪个模式里；把这套作用域钉到另一张表的探针上，就是让宿主到一个不含它的
命名空间里去找，回报「不存在」——那句报错读起来像用户把表名写错了，原因却很可能只是插件把
范围收窄了。其余情况一律交给连接自己的默认模式；表真的够不着时，宿主那句拒绝就是该看的结果。
反过来说，这颗按钮本来也不需要表名：关系名是从语句里出来的。

**写语句怎么拿到数字：只读探针。** `analyze` 每条语句额外返回一个可选的 `planProbe`——
一条宿主可以被送去计划的只读语句，外加一个 `basis`，说明它给出的数字凭什么可以信；
给不出时返回一个 `planProbeRefused` 原因码。为什么这段逻辑在引擎里而不在界面上：拼这条
语句意味着要在 token 边界上切用户的原文，而只有分词器知道一个字符串字面量在哪里结束。
界面永远不拼接 SQL，它原样把 `planProbe.sql` 发出去，并且**在说任何数字之前先把这段文本
打在卡片上**——要送去计划的东西必须先看得见。

- `basis: "self"`——单条只读语句，原样递过去。
- `basis: "predicate"`——`UPDATE t SET … WHERE p` 改写成 `SELECT * FROM t WHERE p`，
  `DELETE FROM t WHERE p` 同理。写语句碰到的行**就是**谓词匹配的行，这是整套做法依赖的
  等式；也正因为如此，`SET` 那一截必须从改写里消失。
- `basis: "wholeTable"`——同样两种形状，没有 `WHERE` 时改写成 `SELECT * FROM t`，因为
  无条件写就是全表。
- `basis: "source"`——`INSERT … SELECT q` 改写成 `SELECT q`，**不套外层**：读多少行就写
  多少行，套一层 `SELECT * FROM (…)` 问的是结果集的组合数。

每一个拒绝都是一个具名原因码，不是一句"没法判断"：`nothingToPlan`、`fullWidthPunctuation`、
`limit`、`multiTable`、`updateFrom`、`cursor`、`valuesOnly`、`conflict`、`notRowEstimable`、
`noTarget`、`semicolonInText`、`tooLong`。最要紧的是 `multiTable` 和 `updateFrom`——笛卡尔积写法的
`UPDATE t, u SET …` 能改写出一条**语法合法**但行数错得离谱的 `SELECT`，那比没有数字更糟，
所以这里的守卫是"`SET` 和表名之间有没有顶层逗号"，而不是想办法把它猜对。`limit` 同样被拒：
一条 `DELETE … LIMIT n` 说不清谓词的选择性。

探针从不出这个插件的手，也永远不会被执行：唯一跑它的地方是宿主的计划通路，而插件造出来的
探针按构造就是 `SELECT`，DDL 和 DML 根本到不了那道门。

**从计划里只读出一个数字。** 条件是 `format === "json"`、`truncated !== true`、
`warnings[]` 为空，取 `rawPlan[0].Plan["Plan Rows"]`。任何一种不满足就不产生数字，并说明
为什么。文本计划里当然也有 `rows=`，但那个拼写每种方言都不一样，和元数据那条路刻意丢掉
`dataType` 是同一个理由。被截断的计划只是一棵树的一部分，一部分树上的行数不是这条语句的
影响面——而这个数字会变成一条 High 的审批要求，所以它唯一不能做的事就是猜。

**来源才是重点。** 引擎取 `plan.estimatedRows` **优先于**调用方自报的 `estimatedRows`，
并且把用了哪一个写进命中项的消息里（"来自宿主的估算计划"）。不这样排序的话，往框里填个
`0` 就是「大范围扫描」检测器的静音开关；这样排之后，宿主算出来的数字就没人能把它压下去。
来自探针的数字还有第三种说法：`planFacts.rewritten` 跟着它一起走，消息里写明这个计划是
「SQL Guard 自己改写出来的那条只读 SELECT 的计划，不是这条语句本身的计划」。报告里还另有
一行，写明这次的数字是计划给的、是改写出来的、是有人手填的、还是根本没有。优化器估的
25 万、改写后估出来的 25 万、提交者敲的 25 万，对正在审批的那个人来说是三条不同的证据。
审批／驳回的弹窗里也会重复这一行和全部命中项，让这个数字在它被使用的那一刻出现在眼前。

**它永远不能做什么。** 和元数据那条路同样的规则：计划只会**新增** `largeAffectedRows`
这一条命中项，绝不删除、降级或替代任何静态结论，也绝不拒绝落库。行数和它所属的
`(sql, connectionId)` 一起存在页面内存里，两者任一改变就立即作废——一个过期数字被重新
贴到改过的语句上，等于造出一条带着宿主标签的假事实。跨会话不缓存，除了最终生效的那个
数字之外什么都不落库。`mcp/call` 和 `tableFacts` 一样，无视调用方塞进来的 `planFacts`。

**只有 MCP 那条路会把探针脱敏。** `guard/analyze` 必须交给工作台一段能原样发给
`explainPlan` 的文本，所以那里的字面量保留原样。`mcp/call` 返回的是同一份报告，而它的读者是
内置 AI——那段文本会离开这台机器进入模型服务商，所以 `sql_guard_analyze` 把每一条
`planProbe.sql` 过一遍落库时用的同一个 `redact_literals`。探针的形状和 `basis` 照样给模型看，
里面的值不给。

## 按数据库划分作用域

`database` 是**作用域标签，不是连接**——至少当前这个构建是这样。本插件没有声明
connection provider，所以看不到用户在 DBX 里配的那些连接，而从插件中心打开工作台时
`context` 确实是空的。本仓库就是据此测出 `{}`，并得出"那几个只读 API 在这里用不了"
的结论。

**那个结论是错的，而且这个错误的修正很重要。** 一个 `menu: "table"` 的
`context-menu` 贡献点，配上 `open-workbench` 动作，宿主就会把稳定的 `TableContext`
——`{connectionId, database?, schema?, table}`——作为工作台上下文传进来；
`menu: "connection"` 传的是 `{id, dbType, name, database}`。两者都只有对象身份，
绝不含 host、port、username、password 或连接串。拿到一个用户已经打开的连接的
`connectionId` 之后，宿主的只读 API 就都够得着了，而插件依然不持有任何凭据：
`getTableMetadata`（`host.schema:read`）、`getPlanCapabilities` + `explainPlan`
（`host.plans:read`，"只读宿主生成的预估执行计划，永远不等于执行 SQL、写入、DDL
或拿到真实计划"）、以及 `queryData`（`host.data:read`，单条只读语句，且必须用户对
"这个插件 + 这个连接"逐项同意，可在插件中心撤销）。这些在上游文档和 0.6.21 的
release notes 里都写着，而**只读这条路现在已经在真实宿主上验证过了**（0.6.38）：
从表的右键菜单进来确实能拿到 `connectionId`，`getPlanCapabilities` 和
`getTableMetadata` 也都给了返回，形状见[宿主元数据读取](#宿主元数据读取)。
`explainPlan` 现在会被调用了，但只从那颗计划按钮，而且只送引擎愿意背书的那一条只读语句——
语句本身是只读的就送它自己，是写语句就送为它改写出来的探针
（见[宿主的估算计划](#宿主的估算计划)）；`queryData` 依然一次都没调用过，因为
`host.data:read` 根本就没声明。

工作台原先有两个只读的 `connectionId` / `dbType` 输入框，值取自 `context`。在插件中心
那个入口下它们永远填不上，所以直接删掉了，而不是留在界面上装成坏掉的功能——注意这是
入口问题，不是宿主不给（见上一节）。数据库列和
`AuditRecord` 里的字段保留，老记录仍能读出原值、仍能导出；新记录一律为 NULL。

这个标签能支撑的事：

- 工作台会用你**以前提交过的库名**做补全，存在宿主 storage 的 `sqlGuard.databases`
  里（最近优先、上限 20 个、大小写不同算同一个）。它是 datalist，所以手输新库名
  照样可以。
- 审计页支持按库和按操作人筛选，两个下拉的候选值来自 `guard/facets`：按小写归并
  分组，并给出每个取值的记录数。筛选是等值匹配而不是前缀匹配，所以 `prod` 不会
  顺带把 `prod-replica` 捞进来。
- 每个维度最多返回 100 个候选值，按计数倒序。这两个字段都是自由文本，打错一个字
  就会多出一个取值，不截断的话下拉框会比记录列表还长。

要让作用域变成"真的"，其实**不是**当初以为的凭据问题。用 `explainPlan` 拿一个行数
估计，需要的是用户从某张表上打开留痕器时宿主传进来的 `connectionId`，外加
`host.plans:read`——连接由 DBX 自己持有，插件看不到任何口令。声明
`connection-provider`（插件自己保管数据库凭据）仍然有意不做；关键是凭据在谁手上，
而走上下文菜单这条路，答案始终是"在 DBX 手上"。

