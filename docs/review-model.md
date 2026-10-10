# Review model: identity, approval, redaction

Back to [README](../README.md) · [简体中文](review-model.zh-CN.md)

## Operator identity

The host hands the plugin no identity. This was verified on DBX 0.6.38 rather than
assumed: the workbench's `window.dbxPlugin.context` has come back `{}` on one entry and
`{workbenchId, restored, surface}` on another, and none of the bridge's 41 keys is a
user, account, or session field. Even where the host does fill `context` with something
usable — entering from a table's right-click menu puts a `connectionId` and a table name
in it — those name *data*, never *who is asking*. So `operator` and `approver`
are **self-declared** — the workbench asks for a name, remembers it, and sends it.

- The name is persisted in host storage under `sqlGuard.operator`, which is why the
  manifest declares the `host.storage` permission. It follows the DBX install, so
  reinstalling the plugin does not lose it, and it stays out of the audit database.
- If host storage is unavailable (an older host, or the permission was denied), the
  UI says so and the name lasts only as long as the workbench stays open.
- MCP calls come from the built-in AI with nobody at the keyboard, so they keep the
  sidecar's fallback: `USER` (or `USERNAME`) suffixed with the hostname as
  `user@host`, or `"unknown"` when neither is available. The hostname turns "which
  account" into "which account on which machine", which is what actually separates
  people sharing a box or reusing an account name across boxes. It is still context,
  not authentication — both halves come from the local environment.

The hash chain proves a record was not edited after the fact. It does not prove who
typed it. Treat the trail as evidence of intent and review history, not as
non-repudiation.

## Who may approve

Two rules run inside `GuardStore::decide`, so every caller — the workbench, the MCP
tools, a future remote store — goes through the same gate. Both produce a JSON-RPC
`-32602` with a message written for a human, not a `-32000` internal error, because
being refused is something the caller can act on.

**The approver must not be the submitter.** On by default (`requireDistinctApprover`).
Names are compared after trimming and lowercasing, so `Alice` cannot approve what
`alice ` submitted. Turn it off in the Rules tab if you are the only person using the
plugin — otherwise you can never approve your own submissions. It cannot stop two
people from approving each other's records, because both names are self-declared.

**This rule applies to approvals only.** A submitter may always *reject* their own
record: rejection grants nothing, and blocking it would leave the record stuck in
`pending` with nobody able to close it — which is what a solo user hit first, since the
decision dialog used to prefill the approver with the submitter's own name. It now
leaves that field empty when the two would collide and says why. The approver
allow-lists below still apply to rejections, because a rejection rewrites an audit
record and someone outside the list should not be able to.

**Approvers can be restricted, globally or per database.** An empty global list means
no restriction: anyone may approve. Once it has names, only those names may. A
database listed under `scopeApprovers` uses *only* its own list and does not fall back
to the global one, so "only the DBA leads may approve `prod`" stays true even when a
global list exists.

An empty list means different things in the two places, deliberately:

| Configuration | Meaning |
| --- | --- |
| `approvers` empty | No restriction — anyone may approve |
| `prod` absent from `scopeApprovers` | Falls back to `approvers` |
| `prod` present with an empty list | **Nobody** may approve `prod` |

The last row fails closed. A misconfigured approval list should block work, never
widen it — otherwise deleting the names for `prod` would silently hand it to the
global list.

The Rules tab edits `scopeApprovers` as one line per database, `prod = alice, bob`.
A line without `=` is dropped rather than guessed at, and the panel repaints from
what the sidecar stored, so a dropped line visibly disappears.

Scope enforcement inherits the limits of the scope label: a record submitted without
a database can only be checked against the global list.

## What the audit trail stores

The `sql` column holds the output of `risk::redact_literals`, not what you typed.
String literals and comment bodies become `?`; identifiers, quoted identifiers and
**numbers are kept verbatim**:

```sql
-- submitted
DELETE FROM orders WHERE created_at < '2024-01-01' AND token = 'hunter2' LIMIT 100
-- stored, and what the AI pre-review sees
DELETE FROM orders WHERE created_at < '?' AND token = '?' LIMIT 100
```

Why the numbers survive: an earlier version redacted them too, and that removed
exactly the information an approver needs. `WHERE id = ?` cannot be told apart from a
mass update, and the model pre-reviewing it is in the same position. The trade is
honest — a numeric literal can itself be sensitive (an account number, a phone
number) — but "nobody can judge the blast radius" is the worse failure for a tool
whose only job is pre-review. Making redaction configurable per rule is the obvious
next step; it is not done yet.

Redaction happens before the write, so the redacted text is what the content digest
covers. `sql_hash` is computed over the **original** text instead, which is why two
statements differing only in a literal still hash differently while their stored text
looks the same.

Because the chain is append-only, the plaintext cannot be added afterwards. The one
exception is a record you submitted *in this workbench session*: its original SQL is
still in the browser's memory, so the decision dialog shows it, labelled as
never-written-to-disk. Close the workbench and only the redacted text remains — that
is the price of not letting the audit database become a password store. Reviewing old
records at full fidelity would need an explicit opt-in at submit time, which is a
product decision rather than a bug.

## What cannot enter the chain

The chain is append-only, so a bad record cannot be removed later — validation has to
happen before the write. `guard/submit` and the MCP `sql_guard_record_review` tool both
call `risk::check_recordable` and refuse, with `-32602`, when:

- the input has no statements at all (whitespace and comments only); or
- any statement ends on a dangling clause keyword — `DELETE FROM`, `UPDATE t SET`,
  `SELECT * FROM t WHERE`, `DROP TABLE`.

Such a statement is reported as `incompleteStatement` at High risk. `guard/analyze`
still returns the full report for it, because seeing *why* is the point; only the write
is blocked. When the workbench is refused it re-runs the analysis so the finding
appears in the report panel instead of leaving just a toast behind.

This is a heuristic, not a parser. The keyword list holds only words a valid statement
can never end with — `end` is excluded because `SET x = CASE … END` is complete, and
quoted identifiers are excluded because `DELETE FROM "on"` names a table called `on`.
A fragment that ends on an ordinary identifier (`DELETE FROM t WHERE x =`) still gets
through. The rule is not gated by a detector switch: there is no good reason to record
truncated SQL, and a toggle would only create a way to turn the guard off by accident.

