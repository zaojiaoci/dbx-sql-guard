# Known limits, in full

Back to [README](../README.md) · [简体中文](known-limits.zh-CN.md)

The README carries the short list. This page keeps the reasoning behind each limit, because the
value of this plugin depends on trusting what it says no to.

- Advisory only (see the README); nothing stops a statement from running.
- `operator` and `approver` are self-declared and unverified — see
  [Operator identity](review-model.md#operator-identity). The approver rules in
  [Who may approve](review-model.md#who-may-approve) enforce *separation of names*, not separation
  of people.
- The truncated-statement gate is lexical — see
  [What cannot enter the chain](review-model.md#what-cannot-enter-the-chain).
- The AI pre-review opinion comes from a model this plugin does not control, is not
  persisted, and is advisory — see [AI pre-review](host-integration.md#ai-pre-review). It needs the
  `host.ai` permission and a host that really exposes `bridge.ai`; without one the card
  says so and both buttons stay disabled.
- The SQLite file is plaintext and can be deleted wholesale. The chain detects
  edited or missing rows, not a missing file (`checked` would be 0 and `ok` true).
- One export holds at most 10,000 rows, because the CSV travels as a single
  JSON-RPC message under the protocol's 8 MiB cap. The response reports
  `exported` and `truncated`, and the UI says so instead of implying completeness.
- A decision is final: an approved or rejected record cannot be decided again.
  Correcting a mistake means submitting a new record, not rewriting history.
- A row estimate is either the host's plan (of the statement itself, or of a read-only probe the
  engine derived from it) or a number a caller declares; the finding's message says which, and a
  probe-derived one says so explicitly. With neither, the large-scan rule stays silent —
  silence here means "no input", never "small".
- **Availability is now part of the verdict.** A statement judged after entering from a
  table's right-click menu can carry findings the same text does not get from the plugin
  center or the bottom dock, because only the first gives a `connectionId`. The report line
  naming the mode is the only thing telling those two runs apart, so compare verdicts
  entered the same way — and note that the two extra permissions (`host.schema:read`,
  `host.plans:read`) make DBX ask for consent again when you install **0.2.x or later**; an
  install of 0.1.1 or older has to be re-consented, not upgraded in place.
- **Writes get a number only through a rewrite.** DBX's plan gate refuses DML on every dialect
  but Oracle, so the plugin asks the host to plan an equivalent *read-only* `SELECT` instead
  ([The host's estimated plan](host-integration.md#the-hosts-estimated-plan)). The equality it
  relies on — *rows this write touches = rows that predicate matches* — holds for a single-table
  `UPDATE`/`DELETE` and for a whole table when there is no `WHERE`, and the engine refuses to
  offer a probe for every shape where it does not. `INSERT` is not covered: planned rows would
  count the source query, not the rows the write adds. Where no probe exists, the blast radius
  stays a human judgement or a self-declared number the record marks as such.
- An estimated plan is *this connection's optimizer's guess*, not a measurement: it assumes
  current statistics, and the probe is a `SELECT` — so the number says how many rows *match*,
  not how many a trigger, a foreign-key cascade, or a `RETURNING` clause would actually move.
  The finding's message names the rewrite so an approver cannot mistake it for a plan of the
  write itself. It is used to raise a finding, never to clear one.
- The engine is a lexical classifier, not a parser: dialect-specific syntax it does
  not know is scored conservatively but can still be wrong. It can therefore never prove a
  statement is *valid* SQL — only that it cannot be. Five cross-dialect shape rules do the
  latter (`malformedStatement`, Medium, not user-toggleable, and deliberately not a reason to
  refuse the audit write): a `*` after `DELETE` or after `FROM`, a `DELETE` with no `FROM`, an
  `UPDATE` with no `SET`, and an `INSERT` naming no values. That covers
  `delete * from user where id = 3`, which the first builds of this plugin scored Low and let
  straight through. A real dialect grammar (an offline `sqlparser` crate) is what would close
  the rest of the gap. The host's `explainPlan` contributes one number to the verdict
  ([The host's estimated plan](host-integration.md#the-hosts-estimated-plan)); it does not check syntax, and the
  probe only proves the *shape* of a rewrite is safe to plan — nothing has run at that point, so
  a `SELECT * FROM t` probe says nothing about whether `t` exists.
- **Full-width punctuation gets its own finding** (`fullWidthPunctuation`, same tier: Medium,
  not user-toggleable, not a reason to refuse the audit write). The failure this exists for is
  the Chinese IME: `；`, `，`, `（）`, `－` and smart quotes are not SQL syntax, and because
  statements are split on the ASCII `;` only, a trailing `；` silently merges everything behind
  it into one reviewed block — so the second statement's risk never gets counted. The finding
  names the characters it found and the report still shows the real statement count.
  It cannot catch full-width *letters* (`Ａ`, `ｔ`): `is_ident_start` accepts anything
  `is_alphabetic`, and a full-width letter is exactly that, so it becomes an ordinary word and
  looks identical to a legitimate Chinese identifier. Full-width *digits* are caught, since
  those are not alphabetic and land in the punctuation branch.
- Stored SQL is redacted, so a string literal that carries the blast radius
  (`created_at < '2024-01-01'`) is still invisible in an old record. Numbers are kept
  for exactly that reason, and a record submitted in the current session can be
  reviewed against its original text — see
  [What the audit trail stores](review-model.md#what-the-audit-trail-stores).
