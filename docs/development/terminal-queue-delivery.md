# Terminal queue delivery

Queued terminal delivery requires the stored session status to be Idle, a
positive idle detection from the live pane, and a complete, empty composer
throughout the quiet window. The sender checks the pane again before and after
persisting its attempt claim, and immediately before sending Enter.

Each `(session_id, prompt_id)` gets one paste attempt per grant: the first is
implicit, every later one is a `released` receipt. The
`terminal_queue_receipts` table in `acp_events.db` is introduced by migration
v029 and initialized for fresh event stores. Claims and drop/delivery receipts
commit with SQLite `synchronous=FULL` before the corresponding action. They
are independent of queue snapshots, event retention, and daemon lifetimes.
Database errors fail closed.

The dispositions are:

- `legacy_uncertain`: v029 found this terminal row in a pre-upgrade snapshot.
  The old daemon may already have pasted it, so it is held for review without
  another attempt. Migration includes archived/snoozed terminal queues and
  the legacy root snapshot, excludes structured ACP, and preserves all text
  and existing receipts. Unreadable or malformed snapshots abort migration.
- `claimed`: an attempt may have reached the terminal. A crash, failed
  readback, swallowed Enter, or keystroke abort leaves it held for operator
  review. There is no automatic second paste or recovery Enter.
- `released` / `released:N`: one more attempt is granted. Bare `released` is
  an operator's (`aoe session queue release <session> <qid>`, or
  `POST /api/sessions/{id}/queue/{promptId}/release`), written only over a
  held receipt: `claimed`, `legacy_uncertain`, or a `released:N` whose
  automatic attempts are spent (it resets the count). `released:N` is the
  drain's own, written when an Enter was withheld after the paste and the
  composer was verifiably restored (empty, or exactly the human's bytes
  again); N counts the automatic attempts so far, and at the cap (3) the row
  is held. The next claim overwrites a release; a delivered or dropped row
  can never be released.
- `delivered`: the one Enter was sent and an empty rendered composer was
  observed afterward. The receipt is persisted before removing the row.
- `dropped`: removal writes this receipt first, including the CLI's offline
  path. A stale queue snapshot cannot make this qid deliverable again.

Re-enqueue of a consumed id returns HTTP 409 `queue_id_consumed`, with its
disposition (a released id is live and may be re-posted). Delivered/dropped
rows restored by stale state are retired without typing, so they do not block
the next legitimate row. A held row does not block the rows behind it either:
each tick walks the queue in order, retires consumed rows, skips held ones,
and delivers the first row that may be attempted. Held rows wait for an
operator to release or drop them, and the release is refused (HTTP 409
`queue_row_not_held`) for a row that is not held. `GET
/api/sessions/{id}/queue/receipts` lists the receipts of the queued rows;
`aoe session queue <session>` shows them as a HOLD column (`-` never
attempted, `review`, `released`, `exhausted`, `retiring`).

An Enter withheld after the paste (the pane stopped reading as idle, the paste
never rendered whole, or the composer could not be read) does not leave the
paste in the composer. The daemon removes the bytes it can positively attribute
to its own paste (one Backspace for a chip, one per character inline, the
abort path when a human's bytes sit beside it) and never touches text it cannot
explain. When the composer is verifiably restored the row is released for
another attempt, up to three automatic attempts (`held:attempts_exhausted`
after that, until an operator releases it); otherwise it is held for review
with the composer as it was.

The idle read immediately before Enter runs on the capture with the composer
body collapsed to an empty prompt row. The paste itself can fill the box over
many rows on a narrow pane, and neither idle rule survives that: `ready_prompt`
wants an empty `❯` row, and `completed_turn` reads the last non-empty row above
the box, where Claude's `/goal` chrome sits on a session with a goal. A
46-column pane with a goal withheld every attempt as `agent_not_idle` with no
status change logged, purely because of what the drain had typed. With the body
collapsed the capture is the shape the pre-paste check passed on; a spinner or
interrupt banner above the box still reads as running. A box scrolled to its
caret shows only the paste's tail: a visible tail of at least 24 characters
that ends the paste is the daemon's own (`Scrolled`), submitted before Enter
and deleted whole by the strip.

Hold logs include `held:agent_busy`, `held:composer_busy`,
`held:attempt_recorded`, `held:attempts_exhausted`,
`held:unsupported_reader`, and `held:receipt_unavailable`, with session and
qid. A row skipped for review is logged once at INFO, then at DEBUG. Withheld
Enters log `withheld:released`, `withheld:held`, or `withheld:exhausted` with
the reason (`agent_not_idle`, `paste_clipped`, `composer_unverifiable`). Compact `[Pasted text #N]`
and expanded chips are recognized only during readback of this attempt;
preexisting chips always occupy the composer.

Only the Claude terminal composer has a verified reader in this patch.
Codex and other terminal tools are held as `unsupported_reader`; their
automatic delivery is not verified. Structured ACP delivery uses its existing
separate path. Direct, explicitly requested sends retain their existing behavior.

The tmux read and subsequent input operation are separate calls; this cannot
provide an atomic lock against a human typing between them. The quiet window,
boundary checks, and post-paste residue check narrow that race. An unverifiable
or clipped composer withholds Enter, strips the paste, and consumes the attempt
instead of guessing; another attempt follows only from a verified-clean
composer or an operator. This favors no repeated input over guaranteed
delivery after a crash.
