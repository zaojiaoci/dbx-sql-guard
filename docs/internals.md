# Internals: vendored SDK, concurrency, panic model

Back to [README](../README.md) · [简体中文](internals.zh-CN.md)

## Why the SDK is vendored

`dbx-plugin-sdk` is not published to crates.io; the CLI bundles its source and the
Rust template points at it with a path dependency. The vendored copy keeps the
project buildable on any machine with only a Rust toolchain. It lives at the
repository root (not inside `backend/`) because it ships its own `[workspace]`
table, which would otherwise collide with the plugin's workspace.

`backend/Cargo.toml` declares the dependency in registry form
(`dbx-plugin-sdk = { version = "0.1.0" }`) and points at the vendored copy through
a `[patch.crates-io]` entry. Do not collapse that into a plain path dependency:
the CLI builds with `--locked` and injects its own patch for the same crate, and a
path dependency makes that injected patch unused, which cargo reports by wanting to
write `[[patch.unused]]` into `Cargo.lock` — a lock update `--locked` refuses, so
`package` fails. The patch form overrides cleanly and leaves the lock portable.

## Concurrency and failure model

The SDK dispatches requests on a pool of 2–16 worker threads and does not catch
panics, so the sidecar is written to survive both:

- All storage access is serialized by one mutex around a single SQLite connection;
  `busy_timeout` is set so a second process on the same file waits instead of
  failing immediately.
- `handle()` wraps the dispatch in `catch_unwind`. Without it a panic would kill a
  worker thread permanently, the request would never get a response, and the host
  would just time out — repeated panics leave the process alive but deaf.
- A panic while holding the store lock poisons the mutex; the lock recovers with
  `into_inner()` rather than failing forever, which is safe because an uncommitted
  transaction rolls back as it unwinds.
- `panic = "abort"` must stay off (see `[profile.release]`), or the guard above
  cannot run at all.

