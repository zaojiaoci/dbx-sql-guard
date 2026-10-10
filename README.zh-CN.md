# DBX SQL Guard

[English](README.md) | **简体中文**

给 DBX 用的 SQL 风险审查与审批留痕插件：一个 Rust sidecar 在本地离线评估 SQL
语句，把哈希链式的审计日志写进 SQLite，再由沙箱里的工作台完成审查、审批和导出。
当前版本 0.4.0，对着 DBX 0.6.38 验证过。

项目由 `dbx-plugin create --template rust` 脚手架生成后重写。代码注释是中文，
界面和文档是中英双语。

## 它到底能做什么

DBX **没有执行前钩子**：插件无法在数据库执行之前拦截或阻止一条语句。所以这个
插件是一个**建议性**的关卡，而不是强制关卡：

1. 粘贴或生成 SQL，调用 `guard/analyze`，得到风险等级和命中项。
2. `guard/submit` 写入一条不可变的审计记录（哈希链，可检测篡改）。
3. 由人在工作台里批准或驳回；决定单独计算摘要，所以后补的审批不会让内容链失效。
4. DBX 内置 AI 可以通过三个 MCP 工具调用同一套引擎，在给出危险建议前先自查。
5. 工作台可以让宿主的 AI 把这些命中项翻成一份给审批人看的预审意见——仅供参考、
   用脱敏后的 SQL 构造、而且无权审批（[宿主集成](docs/host-integration.zh-CN.md#ai-预审)）。
6. 从表的右键菜单进来时，会向宿主读那张表的真实列清单，据此补出光看文本不可能知道的
   命中项——只读、不入库，报告会写明这次有没有用。同一个入口还会在空的 SQL 框里留下
   一句**注释掉的** `SELECT`，写的正是那张表的准确名字：注释不是语句，所以模板既能
   照着改，又不会被当成插件替谁下的一句判定。
7. 中文输入法打出来的全角标点会被单独指出来：`；` 不是语句结束符，它后面那一块会被
   当成前一条语句的一部分一起审查，第二条语句的风险就这样被吞掉了。
8. 一条语句可以让宿主读出它的**估算计划**，并用优化器给出的行数来判定，于是「大范围
   扫描」这条命中项变成数据库自己的回答，而不是有人在框里敲的数字。写语句靠的是插件
   替它改写出来的那条只读 `SELECT`；凡是这个等式不成立的形状，引擎会按名字拒绝
   （[只读探针](docs/host-integration.zh-CN.md#宿主的估算计划)）。

如果确实需要"强制执行"，只能放在上游（数据库账号权限、代理层，或者将来 DBX
提供的执行前 API）。不要把它当成拦截器来宣传。

## 文档

| 页面 | 讲什么 |
| --- | --- |
| [docs/review-model.zh-CN.md](docs/review-model.zh-CN.md) | 操作人身份、谁可以审批、审计库里存什么、什么进不了链 |
| [docs/host-integration.zh-CN.md](docs/host-integration.zh-CN.md) | AI 桥与真机上尚未核实的部分、宿主元数据读取、`explainPlan` 与只读探针、按数据库划分作用域 |
| [docs/verify-on-real-host.zh-CN.md](docs/verify-on-real-host.zh-CN.md) | 在真实 DBX 里逐项跑的检查清单，以及每一步证明什么 |
| [docs/known-limits.zh-CN.md](docs/known-limits.zh-CN.md) | 已知限制的完整版，含每条的理由 |
| [docs/internals.zh-CN.md](docs/internals.zh-CN.md) | 为什么把 SDK 放进仓库、并发与 panic 模型 |

每页都有对应的英文文件（去掉 `.zh-CN`）。

## 目录结构

| 路径 | 作用 |
| --- | --- |
| `backend/src/main.rs` | sidecar 入口：RPC 分发、参数校验、panic 兜底 |
| `backend/src/risk.rs` | 词法风险引擎：分词、语句切分、检测器、脱敏 |
| `backend/src/store.rs` | SQLite 存储、哈希链、校验、CSV 导出 |
| `backend/src/mcp.rs` | 暴露给 DBX 内置 AI 的三个工具 |
| `backend/src/model.rs` | 共用数据结构（RPC / 数据库 / UI） |
| `backend/src/util.rs` | SHA-256、毫秒时间戳格式化、记录 ID |
| `backend/sql/schema.sql` | 建表与索引语句，编译期嵌入二进制 |
| `ui/index.html` | 单文件工作台（无构建步骤、不引 CDN） |
| `vendor/dbx-plugin-sdk/` | 内置的 Rust 插件 SDK 副本 |
| `manifest.json` | Manifest v1：工作台、表右键菜单、底部面板命令、MCP |
| `docs/` | 长文档，中英双语 |

## 开发

需要 Node.js 22+ 和 Rust 工具链。

```bash
cargo test --manifest-path backend/Cargo.toml
cargo clippy --manifest-path backend/Cargo.toml --all-targets -- -D warnings

npx @dbx-app/plugin-cli dev --path . --port 5190
```

打开命令输出的 URL，在左侧面板点工作台。开发宿主会在源码变化时重新编译 sidecar，
并提供脱敏后的诊断接口：

```bash
curl -sS 'http://127.0.0.1:5190/api/diagnostics?after=0&limit=100&level=error'
```

开发宿主运行期间**不要**在 `backend/` 里执行 `cargo build`：它监听整个 `backend/`
目录，会被并发构建产生的临时目录 `target/debug/incremental/*` 搞崩（FSWatcher ENOENT）。

sidecar 的数据库位于 `$DBX_PLUGIN_DATA_DIR/sql_guard.db`；`DBX_SQL_GUARD_DB` 可以覆盖它，
测试和 jsdom 验证脚本就是靠这个变量指向一个临时库文件。

### 没有桌面宿主时怎么验

`ui/index.html` 的验证方式是把**真实文件**加载进 jsdom，宿主侧换成一个假的
`window.dbxPlugin` 桥，sidecar 用真进程。三个脚本分别覆盖引擎、计划路径和操作人流程；
依赖入口的行为（表的右键菜单）会再起**第二个 jsdom 实例**，因为 `context` 每个入口
不一样。把 `SG_BIN` 指向解包后的 `.dbxp` 里的二进制，就能拿打包产物跑同一套检查。

## 打包

```bash
npx @dbx-app/plugin-cli package .
```

生成**未签名**的评审候选包 `dist/*.dbxp` 和配套的 `*.artifact.json`。
本地安装需要在 DBX 插件中心开启「允许未签名的开发包」；上架安装则需要 DBX Store 签名。

## 已知限制

下面这些必须说清楚，因为这个插件的价值取决于你能相信它对什么说了"不"。逐条的理由见
[docs/known-limits.zh-CN.md](docs/known-limits.zh-CN.md)。

- **只是建议性的。** 拦不住任何一条语句真的被执行。
- **`operator` 和 `approver` 都是自报的、未经认证**：宿主不给插件任何身份。审批规则强制的是
  *名字上的分离*，不是人的分离。哈希链证明记录事后没被改过，证明不了是谁敲的——把它当成
  意图与审查历史的留痕，不要当成不可否认性。
- **半成品语句的门禁是词法层面的**，不是解析器。
- **AI 预审意见**出自本插件控制不了的模型、不落库、且仅供参考；它需要 `host.ai`，也需要宿主
  真的提供 `bridge.ai`，没有的话卡片如实说明、两个按钮保持灰态。
- **入库的 SQL 是脱敏的**：字符串字面量和注释内容变成 `?`，数字保留。真正决定影响范围的
  字面量在旧记录里看不见；只有本次工作台会话提交的记录还能对着原文审。
- **SQLite 文件是明文，可以被整个删掉**：链能发现被改和被删的行，发现不了文件本身消失。
- **单次导出最多 10000 行**（整份 CSV 受协议 8 MiB 单条消息上限约束）；响应会给出
  `truncated`，界面如实提示，不会让人误以为拿到了全量。
- **决定是一次性的**：要纠正错误只能重新提交一条记录，而不是改写历史。
- **可用性现在也是判定的一部分**：从表进来的语句可能带着同样文本在插件中心评不出来的命中项，
  因为只有前者给得出 `connectionId`；要比对判定就得用同一个入口进来。安装 0.2.x 及以后会让
  DBX 重新征求 `host.schema:read`、`host.plans:read` 的同意。
- **写语句的数字是一次改写换来的**：DBX 的计划门除 Oracle 外都拒绝 DML。引擎不肯为某个形状
  背书时就没有数字，影响面回到人的判断，或者由提交者自报并被如实标注。
- **估算计划是优化器的猜测**，不是测量：说的是多少行*匹配*，不是触发器、级联、`RETURNING`
  实际会动多少行。它只被用来提高命中项，从不被用来豁免一条。
- **引擎是词法分类器**：五条跨方言形状规则能证明一条语句*不可能*合法，但这里没有任何东西能
  证明它*确实*合法；补上剩下的缺口要靠真正的方言语法。
- **全角标点有专门的命中项**，但抓不到全角*字母*（`Ａ`），后者会分词成一个普通词。
- **宿主不暴露版本字段**，所以 manifest 里的 `engines.dbx` 是唯一能起作用的版本门槛。

## 发布

1. 发布一个 GitHub Release；生成的 workflow 会为各个目标平台构建未签名候选包。
2. 如果本仓库已按 `autoUpdate: true` 登记，DBX Store 会自动创建或更新候选 PR；
   否则向 **`t8y2/dbx-store:main`** 提一个 PR，附上 release 和
   `release-candidates.json` 的地址。
3. DBX Store 审核通过后用官方仓库密钥签名，并把可安装的产物更新到同一个 PR。

未签名的候选包和源码留在本仓库；DBX 用户安装的是官方目录里已签名的产物。
插件源码不要提交到 `t8y2/dbx`。

## 开源协议

Apache-2.0，见 [LICENSE](LICENSE)。
