# DBX SQL Guard

**English** | [简体中文](README.zh-CN.md)

SQL risk review and approval trail for DBX: a Rust sidecar that rates SQL statements offline,
keeps a hash-chained audit log in SQLite, and a sandboxed workbench that reviews, approves and
exports it. Version 0.4.0, validated against DBX 0.6.38.

Generated from the `dbx-plugin create --template rust` scaffold, then rewritten. Code comments
are in Chinese; the UI and the docs are bilingual.

## What it actually does

DBX has **no pre-execution hook**: a plugin cannot intercept or block a statement before the
database runs it. This is therefore an *advisory* guard.

1. Paste or generate SQL, run `guard/analyze`, get a risk level and findings.
2. `guard/submit` writes an immutable audit record (hash-chained, tamper-evident).
3. A human approves or rejects it in the workbench; the decision is digested separately, so a
   later approval does not invalidate the content chain.
4. The built-in DBX AI can call the same engine through three MCP tools, so it can self-check a
   destructive statement before proposing it.
5. The workbench can ask the host's AI for a pre-review opinion — advisory only, built from the
   redacted SQL, and unable to approve anything
   ([host integration](docs/host-integration.md#ai-pre-review)).
6. Opened from a table's right-click menu, it reads that table's real column list from the host
   and can add findings the text alone cannot support — read-only, never stored, and the report
   says whether it happened. That same entry fills the empty SQL box with a **commented-out**
   `SELECT` naming the table: comments are not statements, so a template can be read as text but
   can never pass for a verdict on something the plugin wrote for you.
7. Full-width punctuation typed by a Chinese IME is called out, because a `；` does not end a
   statement — everything behind it would otherwise be reviewed as part of the one in front.
8. For one statement the workbench reads an *estimated plan* from the host and uses the row count
   its optimizer reports, so the large-scan finding becomes the database's own answer instead of
   a number someone typed. A write gets planned through a read-only `SELECT` the engine derives
   from it, and every shape where that equality would lie is refused by name
   ([the probe](docs/host-integration.md#the-hosts-estimated-plan)).

Enforcement, if you need it, has to live upstream (database accounts, a proxy, or a future DBX
pre-execute API). Do not sell this as a blocker.

## Documentation

| Page | Covers |
| --- | --- |
| [docs/review-model.md](docs/review-model.md) | Operator identity, who may approve, what the trail stores, what may not enter the chain |
| [docs/host-integration.md](docs/host-integration.md) | The AI bridge and what is unverified on a real host, host schema facts, `explainPlan` and the read-only probe, scoping by database |
| [docs/verify-on-real-host.md](docs/verify-on-real-host.md) | The checklist to run against a real DBX install, and what each step proves |
| [docs/known-limits.md](docs/known-limits.md) | The full limits list with the reasoning behind each one |
| [docs/internals.md](docs/internals.md) | Why the SDK is vendored, the concurrency and panic model |

Every page has a `*.zh-CN.md` counterpart.

## Layout

| Path | Purpose |
| --- | --- |
| `backend/src/main.rs` | Sidecar entry: RPC dispatch, parameter validation, panic guard |
| `backend/src/risk.rs` | Lexical risk engine: tokenizer, statement splitting, detectors, redaction |
| `backend/src/store.rs` | SQLite storage, hash chain, verification, CSV export |
| `backend/src/mcp.rs` | The three tools exposed to the DBX built-in AI |
| `backend/src/model.rs` | Shared data structures (RPC / database / UI) |
| `backend/src/util.rs` | SHA-256, epoch-millisecond formatting, record IDs |
| `backend/sql/schema.sql` | Table and index definitions, embedded at compile time |
| `ui/index.html` | Self-contained workbench (no build step, no CDN) |
| `vendor/dbx-plugin-sdk/` | Vendored copy of the Rust plugin SDK |
| `manifest.json` | Manifest v1: workbench, table context menu, dock command, MCP |
| `docs/` | Long-form documentation, bilingual |

## Develop

Requires Node.js 22+ and a Rust toolchain.

```bash
cargo test --manifest-path backend/Cargo.toml
cargo clippy --manifest-path backend/Cargo.toml --all-targets -- -D warnings

npx @dbx-app/plugin-cli dev --path . --port 5190
```

Open the printed URL, then click the workbench in the left panel. The dev host rebuilds the
sidecar on source changes and serves redacted diagnostics:

```bash
curl -sS 'http://127.0.0.1:5190/api/diagnostics?after=0&limit=100&level=error'
```

Do not run `cargo build` inside `backend/` while the dev host is running: it watches the whole
`backend/` directory and crashes on the transient `target/debug/incremental/*` folders a
concurrent build creates.

The sidecar stores its database at `$DBX_PLUGIN_DATA_DIR/sql_guard.db`; `DBX_SQL_GUARD_DB`
overrides it, which is how the tests and the jsdom harnesses point it at a throwaway file.

### Without a desktop host

`ui/index.html` is verified by loading the **real file** into jsdom with a mock
`window.dbxPlugin` bridge, against a real sidecar process. Three harnesses cover the engine, the
plan path and the operator flow; entry-point-dependent behaviour (the table context menu) gets a
second jsdom instance, because `context` differs per entry. Set `SG_BIN` to the binary inside an
unpacked `.dbxp` to run a harness against a packaged build.

## Package

```bash
npx @dbx-app/plugin-cli package .
```

Writes an **unsigned** review candidate `dist/*.dbxp` plus `*.artifact.json`. Installing it
locally requires enabling *Allow unsigned development packages* in the DBX Plugin Center;
marketplace installs need DBX Store signing.

## Known limits

Stated plainly, because the value here depends on trusting what it says no to. The reasoning
behind each one is in [docs/known-limits.md](docs/known-limits.md).

- **Advisory only.** Nothing stops a statement from running.
- **`operator` and `approver` are self-declared and unverified.** The host hands the plugin no
  identity. The approval rules enforce *separation of names*, not separation of people. The hash
  chain proves a record was not edited after the fact, not who typed it — treat the trail as
  evidence of intent, not as non-repudiation.
- **The truncated-statement gate is lexical**, not a parser.
- **The AI opinion** comes from a model this plugin does not control, is not persisted, and is
  advisory. It needs `host.ai` and a host that really exposes `bridge.ai`; without one the card
  says so and both buttons stay disabled.
- **Stored SQL is redacted**: string literals and comment bodies become `?`, numbers are kept. A
  literal that carries the blast radius (`created_at < '2024-01-01'`) is invisible in an old
  record; plaintext is only available for a record submitted in the current workbench session.
- **The SQLite file is plaintext and can be deleted wholesale.** The chain detects edited or
  missing rows, not a missing file.
- **One export holds at most 10,000 rows** (single JSON-RPC message under the 8 MiB cap); the
  response reports `truncated` and the UI says so instead of implying completeness.
- **A decision is final.** Correcting a mistake means submitting a new record.
- **Availability is part of the verdict.** A statement judged after entering from a table can
  carry findings the same text does not get from the plugin center or the bottom dock, because
  only the first yields a `connectionId`. The report line naming the mode is what tells the two
  runs apart — compare verdicts entered the same way. Installing 0.2.x or later makes DBX ask
  for consent again (`host.schema:read`, `host.plans:read`).
- **Writes get a number only through a rewrite.** DBX's plan gate refuses DML on every dialect
  but Oracle. Where the engine will not vouch for a rewrite there is no number, and the blast
  radius stays a human judgement or a self-declared one marked as such.
- **A plan is this connection's optimizer's guess**, not a measurement: it says how many rows
  *match*, not how many a trigger, a cascade or a `RETURNING` clause would move. Used to raise a
  finding, never to clear one.
- **The engine is a lexical classifier.** Five cross-dialect shape rules can prove a statement
  *cannot be* valid SQL; nothing here proves it *is*. A real dialect grammar would close the rest
  of the gap.
- **Full-width punctuation gets its own finding**, but cannot catch full-width *letters* (`Ａ`),
  which tokenize as ordinary words.
- **No host version field** is exposed, so the manifest's `engines.dbx` bound is the only version
  gate.

## Release

1. Publish a GitHub Release; the generated workflow builds unsigned candidates per target.
2. If this repository is registered with `autoUpdate: true`, DBX Store opens or updates the
   candidate PR automatically. Otherwise open one PR against **`t8y2/dbx-store:main`** with the
   release and `release-candidates.json` URL.
3. DBX Store reviews, signs approved candidates with the official repository key, and updates
   the same PR with installable assets.

Unsigned candidates and source stay here; DBX users install the signed assets from the official
catalog. Plugin source does not belong in `t8y2/dbx`.
