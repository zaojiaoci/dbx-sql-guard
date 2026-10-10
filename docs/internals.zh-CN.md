# 内部实现：内置 SDK、并发、panic 模型

回到 [README](../README.zh-CN.md) · [English](internals.md)

## 为什么把 SDK 放进仓库

`dbx-plugin-sdk` 没有发布到 crates.io；CLI 自带它的源码，Rust 模板用 path 依赖
指向那份源码。把副本 vendored 进来，项目在任何一台只装了 Rust 工具链的机器上都能
编译。它放在仓库根目录而不是 `backend/` 里面，是因为它自带 `[workspace]` 表，
放里面会和插件自己的 workspace 冲突。

`backend/Cargo.toml` 用 registry 形式声明依赖
（`dbx-plugin-sdk = { version = "0.1.0" }`），再用 `[patch.crates-io]` 指向 vendored
副本。**不要**把它改回普通的 path 依赖：CLI 构建时带 `--locked`，并且会为同一个
crate 注入它自己的 patch；用 path 依赖时那条注入的 patch 就变成了未使用的 patch，
cargo 会想在 `Cargo.lock` 里写入 `[[patch.unused]]` —— 而 `--locked` 拒绝修改锁文件，
于是 `package` 直接失败。patch 这种写法既能干净地覆盖，也不会破坏锁文件的可移植性。

## 并发与失败模型

SDK 用 2~16 个 worker 线程派发请求，并且**不会**捕获 panic，所以 sidecar 是照着
"两件事都可能发生"来写的：

- 所有存储访问都由一把 Mutex 串行化（单条 SQLite 连接）；设置了 `busy_timeout`，
  这样第二个进程打开同一个库文件时会等待而不是立刻失败。
- `handle()` 用 `catch_unwind` 包住分发逻辑。没有它的话，一次 panic 会永久杀死
  一条 worker 线程，本次请求永远拿不到响应，宿主只能超时；panic 多了以后进程
  还活着但彻底收不到请求——比崩溃更难排查。
- 持锁时 panic 会让 Mutex 进入 poisoned 状态；`lock()` 用 `into_inner()` 恢复
  而不是永久报错。这样做是安全的：未提交的事务在展开时会自动回滚。
- `panic = "abort"` 必须保持关闭（见 `[profile.release]`），否则上面这层兜底
  根本没机会运行。

