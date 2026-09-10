# Terminal queue delivery

Queued terminal delivery requires the stored session status to be Idle, a
positive idle detection from the live pane, and a complete, empty composer
throughout the quiet window. The sender checks the pane again before and after
persisting its attempt claim, and immediately before sending Enter.

Each `(session_id, prompt_id)` gets at most one automatic paste attempt. The
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
- `delivered`: the one Enter was sent and an empty rendered composer was
  observed afterward. The receipt is persisted before removing the row.
- `dropped`: removal writes this receipt first, including the CLI's offline
  path. A stale queue snapshot cannot make this qid deliverable again.

Re-enqueue of a consumed id returns HTTP 409 `queue_id_consumed`, with its
disposition. Delivered/dropped rows restored by stale state are retired
without typing, so they do not block the next legitimate row. Claimed rows
remain held until an operator inspects and drops them. An intentional new
delivery needs a new qid after inspection; retrying the same id is not a reset.

Hold logs include `held:agent_busy`, `held:composer_busy`,
`held:attempt_recorded`, `held:unsupported_reader`, and
`held:receipt_unavailable`, with session and qid. Compact `[Pasted text #N]`
and expanded chips are recognized only during readback of this attempt;
preexisting chips always occupy the composer.

Only the Claude terminal composer has a verified reader in this patch.
Codex and other terminal tools are held as `unsupported_reader`; their
automatic delivery is not verified. Structured ACP delivery uses its existing
separate path. Direct, explicitly requested sends retain their existing behavior.

The tmux read and subsequent input operation are separate calls; this cannot
provide an atomic lock against a human typing between them. The quiet window,
boundary checks, and post-paste residue check narrow that race. An unverifiable
or clipped composer withholds Enter and consumes the attempt instead of guessing.
This favors no repeated input over guaranteed delivery after a crash.
