# Verify on a real DBX host

Back to [README](../README.md) · [简体中文](verify-on-real-host.zh-CN.md)

1. Install DBX (validated against v0.6.38) and enable unsigned development packages.
2. Install the `.dbxp`, open the **DBX SQL Guard** workbench, and run a
   `DELETE` without a `WHERE` clause: it must come back `high` and `pending`.
3. Click **Approve**. The approver field must come up **empty**, with a hint naming
   you as the submitter and pointing at the Rules toggle. Typing your own name must
   then be *refused* with "cannot also approve it" — that is `requireDistinctApprover`.
   Any other name must go through; **Verify chain** must report the chain intact, and
   approving the same record a second time must be refused. The dialog must show the
   SQL exactly as you typed it, labelled as session-only, while the audit list shows
   the redacted text.
4. Submit another `DELETE` without a `WHERE` and click **Reject**, leaving your own
   name prefilled: it must go through. Rejecting your own record is allowed; approving
   it is not.
5. Type `DELETE FROM` — no table — into the SQL box and click **Send to audit trail**.
   It must be refused, the report panel must show a `incompleteStatement` /
   "Truncated statement" finding, and the record count must not increase.
6. In Plugin Center → Installed → **Built-in AI tools**, confirm the three
   `sql_guard_*` tools are listed. MCP tools are documented as being offered for
   *open plugin connections*; this plugin declares no connection provider, so
   whether they surface without one has not been confirmed on a real host.
7. Type a name into **Operator**, close the workbench, and reopen it: the name must
   still be there. That exercises the `host.storage` permission end to end.
8. In **Rules**, set `prod = someone-else` under per-database approvers and save.
   Reopening the panel must show the same line back. A `prod` record must now be
   approvable only by `someone-else`; deleting the name and saving must make `prod`
   unapprovable by anyone, including people on the global list.
9. Analyze any statement and click **Generate opinion** in the **AI pre-review** card.
   The host must ask you to confirm, and the reply must then appear with its Markdown
   structure kept — headings, bullets, `code`, **bold**, fenced blocks. If it instead
   reports that no model could be resolved, the panel prints what `listModels()`
   and `listProviders()` really returned — that output is what separates "no provider
   configured" from "provider configured, unexpected field names"
   (see [What is not verified on a real host](host-integration.md#what-is-not-verified-on-a-real-host)).
10. Click **Continue in AI chat**. It must either open a conversation with the report
    already sent, or open one with a toast telling you to press send yourself, or — if
    both attempts fail — leave the host's exact error message and the arguments that
    were sent in the card. Whatever it says there is the answer to why; this path has
    been reported failing on a host where **Generate opinion** works.
11. Right-click a table in the sidebar and choose the SQL Guard entry. The SQL box must arrive
    holding a **commented-out** `SELECT` that names that table with its real schema — clicking
    **Analyze** on the template untouched must say *Nothing to analyze: the input contains only
    comments and whitespace*, with no risk badge and no "auto-approve" claim, because a low-risk
    verdict for text that was never a statement is a lie. Delete the two dashes and it is a real
    statement you can review; clear the box and the same table must not push the template back.
    The **What the host
    actually told us** card must already carry the connection and table it was handed, and after
    **Analyze** its dialect and plan-support line must be filled in — that is
    `getPlanCapabilities` and `getTableMetadata` having been read on its own, without a click.
    Opened from the plugin center or the bottom dock instead, `context` carries only
    `{workbenchId, restored, surface}` — no connection — so *Read estimated plan rows* stays
    **disabled** and the card's availability line (directly above the status line) says why; that
    is the entry point, not a failure. Pasting the connection's **display name** does not
    work: the host answers `Connection config not found`. A `Connection is not open`
    refusal is the host being correct, not a bug — open the connection in DBX first and
    call again. The three buttons that dumped the host's raw answers by hand (bridge probe,
    `getPlanCapabilities`, `getTableMetadata`) are still in the markup, hidden behind the
    `SHOW_HOST_DIAGNOSTICS` flag in `ui/index.html`; set it to `true` when you need to tell
    "nothing configured" apart from "configured, unexpected field names".
12. Stay on that right-click entry, put `UPDATE <that table> SET nope = 1` in the SQL box and
    analyze. The report must show a `unknownColumn` finding *and* the line naming the table
    and column count it used. Re-analyze the same text from the plugin center: the finding is
    gone, the line says there were no facts this run, and every other finding must be
    unchanged. If `nope` is a real column, neither entry may report it.
13. Switch the input method to Chinese and type `UPDATE t SET a = 1；UPDATE t SET b = 2；`,
    then analyze. The report must say **1 statement** (that is the bug this finding exists to
    expose, not a mistake) and must carry `Full-width punctuation where SQL syntax is required`
    naming `；(2)`. Sending it to the audit trail must still succeed. Then type
    `UPDATE 用户 SET 备注 = '中文，里面有；' WHERE id = 1` — no such finding may appear.
14. Still from the table's right-click menu, with the connection open in DBX, put a single
    `SELECT` in the box, run **Analyze**, then click **Read estimated plan rows**. The dialect
    and plan support must already be showing in the host card, and the raw plan must be printed
    there. The row count then appears in *Estimated rows* and the next report attributes the
    large-scan finding to the host. Replace the box with `DELETE FROM <that table> WHERE <a real
    column> = 1` and analyze: the card must now show the *probe* — `SELECT * FROM <that table>
    WHERE <a real column> = 1` — before any host call happens, and clicking the button must plan
    exactly that text. The next report must attribute the number to a **rewrite**, not to the
    `DELETE`. Now type `UPDATE t, u SET a = 1 WHERE b = 2`: no probe, and a named refusal
    (`multiTable`) as a toast as well as in the card — a refusal nobody sees is the failure mode
    0.2.1 fixed for the other gates, and it still applies to every new one.
    Type `SELECT 1; SELECT 2`: same, refused locally, as one statement's plan is not an
    estimate for the script. Finally change the SQL after a successful read: the number must
    disappear rather than follow the old plan onto the new text.
15. On that same right-click entry, review a statement about a **different table in the same
    database** (`DELETE FROM <another table> WHERE <a real column> = 1`) and read its plan rows.
    It must work. `database` and `schema` describe where the *right-clicked* table lives, so
    pinning them onto a probe for some other table makes the host look inside a namespace that
    does not contain it and answer "does not exist" — which reads like a mistyped table name and
    may in fact just be the plugin narrowing the scope too far. Only a probe that names the
    right-clicked table may carry that scope; anything else goes to the connection's own default,
    and if the table genuinely is not reachable, the host's refusal is the report.

