# Host integration: AI bridge, schema facts, estimated plans

Back to [README](../README.md) · [简体中文](host-integration.zh-CN.md)

## AI pre-review

The Review tab has an **AI pre-review** card that turns the engine's findings into an
opinion a human approver can act on. Both paths go through the host's own models — the
plugin talks to no AI service directly and holds no API key:

- **Generate opinion** → `ai.generateText({ configId, model, prompt })`. Needs the
  `host.ai` permission plus `capabilities.aiCompletion`. The host asks you to confirm
  every generation and allows only one at a time per workbench.
- **Continue in AI chat** → `ai.openConversation({ title, prompt, context, send })`.
  Needs only `host.ai`, so it is the fallback when no model can be resolved.

Verified against the host source: `openConversation` resolves whether or not a model is
selected, so the plugin's promise succeeds and DBX then toasts its own message from inside
the assistant panel (`AiAssistant.vue`, the `noConfig` and `pluginHttpModelOnly` strings).
The two paths read different settings: `generateText` resolves a config by the `configId`
the plugin passes, while the panel needs a model chosen in its own model menu — which is why
pre-review works while chat appears "not configured". A refused auto-send is still retried
once without `send` (that branch covers genuine rejections such as `host.ai` missing or the
panel being unavailable), and the success path now only claims that the report was handed to
the panel, because the plugin cannot observe the send.
If both attempts fail, the card keeps the host's message verbatim next to the exact shape
that was sent: a toast disappears, and that text is the only thing separating "the panel has
no model" from "these arguments are not accepted".

The remedy is in that panel, not in its settings page: DBX's assistant has its own model
menu, and a plugin conversation runs only what is selected there. Choosing a CLI-style agent
there produces a different host message ("select an HTTP model or Ollama"), because plugin
analysis is not routed through CLI providers. `generateText` is unaffected by both, which is
why an opinion can look perfectly configured while the chat button appears not to be.

Configuration lives in DBX, not here. Add a provider under the editor settings → **AI**
(Claude, OpenAI, Gemini, DeepSeek, Qwen, MiniMax, Ollama, an OpenAI- or
Anthropic-compatible endpoint, or a custom one; the desktop client also recognises
Claude Code CLI, Codex CLI and Pi Coding Agent), with its API key, auth method,
endpoint, proxy and headers. The plugin then reads whatever you configured through
`listModels()`, falling back to `listProviders()` + `discoverModels()`, and picks the
entry flagged as default or the first one. It has no AI settings of its own and cannot
choose a provider for you — DBX's own documentation defines no automatic default
either.

The model is given the engine's verdict, its findings, the statement list and the SQL,
and asked to explain the risk in business terms, name what a lexical engine cannot see,
and propose a safer rewrite — in the workbench's locale.

The reply is rendered, not dumped: model output is Markdown, and flattened into one block of
text its headings, bullets and code blocks stop meaning anything. That is a *typography*
decision only. Every node comes from `createElement` + `textContent`, there is no `innerHTML`
anywhere in the path, so `<script>` and `javascript:` in a reply stay the characters they are
and a link is never a link — the workbench has no way to open one, and printing the URL is how
you see what was pushed at you. Two rules exist because the replies are about SQL: a `*` only
starts emphasis when the character before it is whitespace or punctuation, and the `_em_` form
is not supported at all, since `update_time` next to `create_time` on one line would otherwise
italicize the middle of a column list.

Three properties worth stating plainly:

- **Your string literals never leave the machine.** The prompt is built from
  `redactedSql`, which `guard/analyze` derives with the same `risk::redact_literals` the
  store applies before persisting. `DELETE FROM t WHERE token = 'hunter2' AND id = 7`
  reaches the model as `DELETE FROM t WHERE token = '?' AND id = 7` — the same reason
  the audit database is not a password store. Numbers are kept on purpose so the model
  can reason about scale; see
  [What the audit trail stores](review-model.md#what-the-audit-trail-stores).
- **It cannot approve anything.** `decide` is not reachable from `mcp.rs`, the card has
  no approve control, and the prompt tells the model never to state or imply a decision.
  That instruction sits *after* the SQL, so when the prompt has to be truncated it is the
  SQL that gets cut, not the constraint.
- **Its reply is untrusted text.** It is rendered with `textContent` into a `<pre>`,
  never `innerHTML`.

The opinion is **not persisted**. It is not part of `ChainInput`, so it cannot affect a
digest, and clearing the form discards it. To put it on the record, copy it into the
decision's `note` field — which is outside the content chain by design.

### What is not verified on a real host

The signatures and limits above come from upstream `plugins/README.md`. The bridge
surface itself **is** confirmed: on DBX 0.6.38 the bridge probe dump (behind the
`SHOW_HOST_DIAGNOSTICS` flag) reports all nine
methods (`cancelGeneration, clearRecommendations, discoverModels, generateText,
generateTextStream, listModels, listProviders, openConversation, setRecommendations`)
and `ai`, `aiModelDiscovery`, `aiCompletion`, `aiCompletionStream`, `aiRecommendations`
all `true` in `bridge.capabilities`.

That inventory is no longer printed under the buttons by default — it answered a developer's
question ("what is the host exposing right now"), not an approver's, and whether the two buttons
are live already says what a reviewer needs. The same `SHOW_HOST_DIAGNOSTICS` switch brings it
back; the one line that stays is the honest failure message for a host with no AI namespace at
all.

What remains unknown is narrower than it was. On a real 0.6.38 host with a provider
configured, **Generate opinion** is reported working, so `listModels`, `generateText`
and the return-shape guesses behind them hold up in practice. **Continue in AI chat**
was reported failing on that same host with a message saying no AI was configured; that
one is now traced to the host, not to this plugin — `openConversation` always resolves, and
the message is DBX's own panel complaining that its model menu has nothing selected. The
shapes returned by `listProviders` and `discoverModels` are likewise still unexercised —
upstream documents those only descriptively. Hence:

- Gating is on *whether the method exists*, not on a capability flag. A `true` flag says
  the host supports the API, not that a call will succeed.
- `asList` / `firstText` / `extractText` accept the plausible variants, and when nothing
  matches they show the raw JSON instead of implying the model said nothing.
- When model resolution fails, the card prints **what the host returned at each step**.
  "A provider is configured but the field names differ" and "no provider at all" look
  identical from the outside; only the raw response tells them apart. A discovery call
  that *throws* gets its own message rather than being folded into the empty case.
- The reply renderer is exercised against **synthetic** Markdown, not against what your
  provider actually emits. The supported subset is headings, both list styles, fenced code,
  `code`, `**bold**`, `*italic*`, blockquote and rule; a table (`| a | b |`) or a nested list
  will therefore show up as an ordinary paragraph. What is pinned by a test is the boundary
  that matters: a reply containing `<img src=x onerror=…>` must stay text.

`ai.setRecommendations` (quick-question chips) and `ai.generateTextStream` (streaming,
matching `aiCompletionStream`) are available and not used yet.

## Host metadata reads

Risk findings are advice about *this text*; a column name is a fact about *the database*.
The workbench can now use the host's own answers for those facts — as an upgrade, never as
a requirement.

**When it reads.** `getTableMetadata` needs a `connectionId`, and the verdict needs to know
*which table* the statement is about. Both come from the entry point: opening SQL Guard from
a table's right-click menu puts `{connectionId, database, schema, table}` into `context`, so
the review tab reads once per `(connectionId, database, schema, table)` and caches it for the
session. From the plugin center or the bottom dock, `context` is `{}` or
`{workbenchId, restored, surface}` — no `connectionId` — and nothing is read. Typing a
connection id and a table into the **Connection metadata & estimated rows** card works too, since
those inputs feed the same target lookup.

**What crosses the RPC boundary.** Only `name` and `nullable` per column. `dataType`,
`length` and `default` are deliberately dropped in `ui/index.html`: a type name is a string
each dialect invents (`character varying(20)`, `int4`, `bigint`), and feeding it to a
dialect-agnostic engine would be writing dialect knowledge into an engine that has none.
`default` also cannot distinguish a `GENERATED`/`IDENTITY` column from a literal default on
the measured host, which is why "a required column is missing from this INSERT" is **not** a
rule here. Nothing is persisted: column lists live in page memory and never enter the audit
database, which is content-hash-bound and must not become a schema snapshot of some
production table.

**What it can add.** Three findings, each requiring the facts to be present, the statement's
target table to equal the table the facts describe, and the statement to be single-table:

| Code | Risk | Fires on |
| --- | --- | --- |
| `unknownColumn` | Medium | A `SET` target or `INSERT` column list naming a column the host does not list |
| `nullIntoNotNull` | High | `SET col = NULL` where the host says `nullable: false` |
| `sensitiveAccess` | Medium | `SELECT *` that expands to a sensitive column the host listed (deduplicated against the static projection hits) |

`unknownColumn` looks only at write targets, never at `WHERE`: a lexical engine cannot tell a
subquery's columns, an alias, or an outer-scope column from a typo, and a false "this column
does not exist" is worse than silence.

"Single-table" here means the **write target is unambiguous**. `UPDATE a JOIN b SET …` and
MySQL's `UPDATE a, b SET …` are not: a bare `SET col = …` could belong to either table, so the
facts of one must not be used to call it nonexistent. `UPDATE t SET … FROM x` (Postgres, SQL
Server) *is* — the `FROM` after `SET` only adds source tables, and a `SET` target can still
only be a column of `t`. When in doubt the answer is "not single-table", because losing one
upgrade is far cheaper than a false positive.

**What it can never do.** Metadata findings do not remove, downgrade, or replace a static
finding — a harness check pins that the static set is identical with and without facts. They
do not refuse the audit write either; only `incompleteStatement` refuses
(see [What cannot enter the chain](review-model.md#what-cannot-enter-the-chain)). Unknown nullability stays
unknown, never guessed as `NOT NULL`. An oversized (`> 2000` columns), wrongly shaped, or
empty column list is dropped whole rather than truncated: a shortened list makes real columns
look absent, so partial facts are worse than no facts.

**The MCP path gets no facts.** `sql_guard_analyze` ignores a caller-supplied `tableFacts` —
the model that wants a lower risk score is the one best placed to fabricate a column list.

**The report says which mode it ran in.** Every report carries a line naming either the table
and column count that was used, or exactly why there were no facts this run (no connection,
no table, the host has no such method, the host refused — with its own words quoted — or it
returned no columns). The same SQL legitimately producing a different finding list depending
on the entry point has to be visible, or the verdict looks like a coin flip.

`getPlanCapabilities` is consumed: `ui/index.html` asks it once per `connectionId`, caches the
answer for the session, and paints the dialect and version in the host card. The engine does
not branch on `dbType` — it has no dialect knowledge to branch with — but the record does:
`db_type` becomes "the host said this was a postgres connection" instead of an empty column.
`supports.estimatedPlan` gates the row estimate described in
[The host's estimated plan](#the-hosts-estimated-plan).

## The host's estimated plan

`host.plans:read` is "read-only access to host-generated estimated plans; it never grants SQL
execution, writes, DDL, or actual plans". Upstream adds the gate that decides what this plugin
can therefore do with it: **DBX's plan path rejects multi-statement input, DDL and DML for
every dialect except Oracle**, and the connection has to be open already. So the *input* the host
will plan is exactly one class — a single read-only statement. A write reaches that door through
the probe described below, never by being sent as it was typed.

**How it is reached.** A separate button in the host card, *Read estimated plan rows*. It is
not part of Analyze and Analyze never calls it. Before touching the host it checks four things
locally and prints which one failed: the connection answered `getPlanCapabilities`, that
answer said `estimatedPlan: true`, the SQL box parsed to exactly one statement, and that
statement has a *plan probe* — the read-only statement the engine is willing to plan on its
behalf (see below). A statement with no probe produces a sentence naming the reason it has none,
as a toast as well as in the card, rather than a red error — because the alternative is a
refusal this plugin would only be relaying, and a button that does nothing looks like a broken
button. The request is `explainPlan({connectionId, sql, mode: "estimated", timeoutMs})` with
`timeoutMs` clamped to the host's own `maxTimeoutMs`; `mode` has no legal value other than
`"estimated"`, since an actual plan (`EXPLAIN ANALYZE`) *runs the statement*.

`database` and `schema` are attached **only when the probe names the table that was right-clicked**.
They come from `context`, which says where *that* table lives; pinning them onto a probe for some
other table makes the host search a namespace that does not contain it and answer "does not exist"
— an error that reads like a mistyped table name, when the cause may simply be that the plugin
narrowed the scope too far. Anything else goes to the connection's own default, and when a table
genuinely is unreachable, the host's own refusal is what gets reported. The read needs no table
name either way: the relation comes out of the statement.

**How a write gets a number: the read-only probe.** `analyze` returns, per statement, an
optional `planProbe` — one read-only statement the host may plan, plus the `basis` on which the
number it yields should be believed — and a `planProbeRefused` code when there is none. The
reason this lives in the engine and not in the workbench: building it means cutting the user's
text at token boundaries, and only the tokenizer knows where a string literal ended. The UI never
concatenates SQL; it sends `planProbe.sql` verbatim and prints it in the card first, so what was
planned is visible before anything is claimed.

- `basis: "self"` — a single read-only statement, passed through unchanged.
- `basis: "predicate"` — `UPDATE t SET … WHERE p` becomes `SELECT * FROM t WHERE p`, and
  `DELETE FROM t WHERE p` becomes the same. The rows a write touches *are* the rows its
  predicate matches; that is the whole trick, and it is why `SET` disappears from the rewrite.
- `basis: "wholeTable"` — the same two shapes with no `WHERE` become `SELECT * FROM t`, because
  an unconditional write is a full-table one.
- `basis: "source"` — `INSERT … SELECT q` is planned as `SELECT q`, unwrapped: the rows read are
  the rows written.

Every refusal is a named code, not a shrug: `nothingToPlan`, `fullWidthPunctuation`, `limit`,
`multiTable`, `updateFrom`, `cursor`, `valuesOnly`, `conflict`, `notRowEstimable`, `noTarget`,
`semicolonInText`, `tooLong`. `multiTable` and `updateFrom` are the ones that matter most — a
cartesian `UPDATE t, u SET …` rewrites to a *legal* `SELECT` whose row count is wildly wrong,
which is worse than no number, so the guard is a top-level comma between the relation and `SET`
rather than an attempt to be clever about it. `limit` is refused for the same reason: a
`DELETE … LIMIT n` says nothing about the predicate's selectivity.

The probe never leaves the plugin's own hands, and it is never executed: the host's plan path is
the only thing that runs it, `EXPLAIN`-style, and DDL and DML cannot reach it because the
engine's probes are `SELECT`s by construction.

**Only one number is read out of the plan.** `format === "json"`, `truncated !== true`, an
empty `warnings[]`, and `rawPlan[0].Plan["Plan Rows"]`. Anything else yields no number and says
so. Text plans contain `rows=` too, but the spelling is per dialect, and the same reasoning
that drops `dataType` from the metadata path drops them here. A truncated plan is a partial
tree, and a partial tree's row count is not this statement's blast radius — that number turns
into a High-risk approval requirement, so guessing is the one thing it cannot afford.

**Provenance is the point.** The engine takes `plan.estimatedRows` *over* a caller-declared
`estimatedRows`, and writes which one it used into the finding's message ("from the host's
estimated plan"). Without that ordering, filling the box with `0` is a switch for silencing the
large-scan detector; with it, a plan the host produced cannot be argued down. A number that came
from a probe is attributed a third way: `planFacts.rewritten` travels with it, and the message
says the plan is of *a read-only SELECT SQL Guard wrote, not of this statement*. The report also
carries a separate line saying whether this run's number came from a plan, from a rewrite, was
declared by hand, or does not exist — 250 000 estimated by an optimizer, 250 000 planned from a
rewrite, and 250 000 typed by the submitter are three different pieces of evidence for whoever is
approving. The approve/reject dialog repeats that line and the findings, so the number is in front
of the approver at the moment it is used.

**What it can never do.** Same rules as the metadata path: a plan can raise `largeAffectedRows`
and nothing else, never remove or downgrade a static finding, never refuse the audit write. The
row count and the `(sql, connectionId)` pair it belongs to are kept together in page memory and
retired the moment either changes — a stale number rebounded onto an edited statement is a
fabricated fact with a host-shaped label on it. Nothing is cached across sessions and nothing
but the resulting number is persisted. `mcp/call` ignores a caller-supplied `planFacts` exactly
like `tableFacts`.

**The probe is redacted on the MCP path only.** `guard/analyze` must hand the workbench a
statement it can send to `explainPlan` byte-for-byte, so there the literals stay. `mcp/call`
returns the same report to the built-in AI, and that text leaves the machine for a model
provider — so `sql_guard_analyze` runs every `planProbe.sql` through the same `redact_literals`
the audit write uses. The probe's shape and `basis` still reach the model; its values do not.

## Scoping by database

`database` is a **scope label, not a connection** — as this build ships. The plugin
declares no connection provider, so it cannot enumerate the connections the user
configured in DBX, and a workbench opened from the Plugin Center receives an empty
`context`. That is how this repository measured `{}` and concluded the read APIs were
unreachable.

**That conclusion was wrong, and the correction matters.** A `context-menu` contribution
with `menu: "table"` and an `open-workbench` action makes DBX pass a stable `TableContext`
— `{connectionId, database?, schema?, table}` — in as the Workbench context, and
`menu: "connection"` passes `{id, dbType, name, database}`. Object identity only: neither
path ever includes host, port, username, password or a connection string. With a
`connectionId` for a connection the user already opened, the host's read APIs become
reachable while the plugin holds no credential at all — `getTableMetadata`
(`host.schema:read`), `getPlanCapabilities` + `explainPlan` (`host.plans:read`, which
"never grants SQL execution, writes, DDL, or actual plans"), and `queryData`
(`host.data:read`, single read-only statements on a connection the user consented to per
plugin and connection, revocable in Plugin Center). All of it is documented upstream and
present in the 0.6.21 release notes. **The read path has now been exercised on a real
0.6.38 host**: entering from a table's right-click menu really does yield a `connectionId`,
and both `getPlanCapabilities` and `getTableMetadata` answered with the shapes quoted in
[Host metadata reads](#host-metadata-reads). `explainPlan` is now called, but only from the
plan button and only for the one read-only statement the engine is willing to vouch for — the
statement itself if it is read-only, a probe derived from it if it is a write
([The host's estimated plan](#the-hosts-estimated-plan)); `queryData` is still never called,
because `host.data:read` is deliberately not declared.

The workbench used to render read-only `connectionId` and `dbType` boxes fed from
`context`. They could never be filled from the only entry point this build declares, so
they were removed rather than left on screen looking broken. The columns remain in the
schema and in `AuditRecord` so older rows
still read back and export; new records leave them NULL.

What the label does support:

- The workbench completes it from names you have submitted before, kept in host
  storage under `sqlGuard.databases` (most recent first, capped at 20, deduplicated
  case-insensitively). It is a datalist, so typing a new name still works.
- The audit tab filters by database and by operator, and both dropdowns are built from
  `guard/facets`, which groups case-insensitively and reports a count per value.
  Filters match exactly rather than by prefix, so `prod` does not silently pull in
  `prod-replica`.
- Facets are capped at 100 values per dimension, ordered by count, because both fields
  are free text and a typo creates a new value.

Making the scope real is therefore **not** the credential problem it looked like. An
estimated row count from `explainPlan` needs a `connectionId` the host supplies when the
user opens the guard from a table, plus `host.plans:read` — DBX does the connecting and
the plugin never sees a password. Declaring a `connection-provider` (the plugin holding
database credentials itself) remains a deliberate non-goal; what matters is who holds the
credential, and through the context menu the answer stays "DBX does".

