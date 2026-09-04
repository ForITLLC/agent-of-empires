//! Drain of the server-owned prompt queue into TERMINAL (tmux) sessions.
//!
//! `POST /api/sessions/{id}/send` refuses to type into a Claude composer that
//! holds an operator's unsent draft (423 `parked_draft`; the refusal is the
//! WO#1872 guarantee). Before this module the refused message was simply
//! gone: the sender had to retry, and a fleet of senders whose retry windows
//! lapsed lost their reports. Now the refusal parks the message on the
//! session's server-owned prompt queue, the same `Instance::queued_prompts`
//! the structured view drains, persisted in the profile's `sessions.json`,
//! so it survives a daemon restart, and this loop delivers it once the
//! composer is clear.
//!
//! The guarantee is absolute here too, and it has a second half. Delivering
//! "the moment the composer clears" RACES THE HUMAN'S KEYSTROKES: an
//! operator who clears their draft and starts typing has the daemon paste
//! into the composer between two of their keys, and the paste's Enter
//! submits it fused with their first letter (observed live: a machine
//! message ending in a stray `c`, a human draft that then read `csn we
//! extend`). So delivery is gated on a QUIET WINDOW (`quiet_gate`): the
//! composer must have been clear, with no client keystroke on the pane, for
//! `QueueTiming::quiet` (3 s by default) before anything is typed. The typing
//! itself is guarded (`Session::send_keys_verified_guarded`): the paste is
//! typed without Enter, read back after a settle, and submitted only if the
//! composer holds exactly the paste; any human byte beside it aborts the
//! delivery, strips the paste, leaves the human's bytes, and re-queues the
//! row (same id, same position) behind a back-off. Never is a turn
//! submitted that mixes injected text with a human's.
//!
//! One row per session per tick, FIFO by `seq`, each confirmed submitted
//! before the next is considered, so a burst never fuses into one paste and
//! every queued message lands as its own turn. Every decision is logged at
//! `server.send_queue` with the row id: `delivered`, `held:composer_busy`,
//! `held:quiet_window`, `held:typing`, `held:backoff`, `aborted:keystroke`.
//!
//! Structured sessions are NOT handled here: their queue drains through the
//! ACP worker (`acp_reconciler::drain_queued_prompts`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::state::AppState;
use crate::session::Instance;
use crate::tmux::{GuardedSend, KeystrokeAbort};

/// The drain's clocks. Defaults are the tuned values; each has an
/// environment override (milliseconds) read once at daemon start so a fleet
/// can re-tune without a rebuild: `AOE_SEND_QUEUE_TICK_MS`,
/// `AOE_SEND_QUEUE_QUIET_MS`, `AOE_SEND_QUEUE_SETTLE_MS`,
/// `AOE_SEND_QUEUE_ABORT_BACKOFF_MS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueTiming {
    /// Tick period. Also the resolution of the quiet window's end: delivery
    /// latency after the window elapses is bounded by this plus one guarded
    /// send.
    pub tick: Duration,
    /// How long the composer must have been clear, with no client keystroke
    /// on the pane, before a delivery is typed. Sized so a human who cleared
    /// their draft and paused to think is still "typing".
    pub quiet: Duration,
    /// Pause between the paste and the read-back that decides its Enter.
    /// Long enough for Claude Code to render a bracketed paste (measured
    /// well under 200 ms), short enough that a human's next key most often
    /// lands after the check rather than inside the window.
    pub settle: Duration,
    /// After an abort, how long the row waits before the quiet window may
    /// even start: the human is demonstrably typing.
    pub abort_backoff: Duration,
}

impl QueueTiming {
    pub(crate) const DEFAULT: QueueTiming = QueueTiming {
        tick: Duration::from_millis(500),
        quiet: Duration::from_millis(3000),
        settle: Duration::from_millis(300),
        abort_backoff: Duration::from_millis(5000),
    };

    fn from_env() -> Self {
        fn ms(var: &str, default: Duration, floor: u64) -> Duration {
            std::env::var(var)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|v| Duration::from_millis(v.max(floor)))
                .unwrap_or(default)
        }
        let d = Self::DEFAULT;
        QueueTiming {
            tick: ms("AOE_SEND_QUEUE_TICK_MS", d.tick, 100),
            quiet: ms("AOE_SEND_QUEUE_QUIET_MS", d.quiet, 0),
            settle: ms("AOE_SEND_QUEUE_SETTLE_MS", d.settle, 100),
            abort_backoff: ms("AOE_SEND_QUEUE_ABORT_BACKOFF_MS", d.abort_backoff, 0),
        }
    }
}

static TIMING: LazyLock<QueueTiming> = LazyLock::new(QueueTiming::from_env);

pub(crate) fn timing() -> &'static QueueTiming {
    &TIMING
}

/// Tick period (see [`QueueTiming::tick`]).
pub(crate) fn drain_interval() -> Duration {
    timing().tick
}

/// Per-session memory of the quiet gate, kept across ticks.
#[derive(Debug, Default)]
pub(crate) struct QuietState {
    /// When the composer was last seen to BECOME clear (with no later
    /// client keystroke). `None` while it is busy.
    pub clear_since: Option<Instant>,
    /// Set by an abort: no delivery, and no quiet window, until then.
    pub backoff_until: Option<Instant>,
    /// Aborts on this session so far (for the log).
    pub aborts: u32,
    /// Last hold decision logged at INFO and when, to rate-limit repeats.
    last_logged: Option<(&'static str, Instant)>,
}

/// Why a row is held this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    /// The composer is not clear (operator draft, dialog, booting pane).
    ComposerBusy,
    /// Clear, but not yet for the whole quiet window.
    QuietWindow { remaining_ms: u64 },
    /// A client keystroke landed on the pane inside the quiet window: a
    /// human is at the keyboard.
    Typing { key_age_ms: u64 },
    /// Backing off after an abort.
    Backoff { remaining_ms: u64 },
}

impl Hold {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Hold::ComposerBusy => "held:composer_busy",
            Hold::QuietWindow { .. } => "held:quiet_window",
            Hold::Typing { .. } => "held:typing",
            Hold::Backoff { .. } => "held:backoff",
        }
    }
}

/// The quiet gate's verdict for one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Clear and quiet for the whole window: type now.
    Deliver {
        clear_for_ms: u64,
    },
    Hold(Hold),
}

/// Pure decision for one tick of one session. `composer_clear` is the
/// composer read this tick; `key_age` is how long since a tmux client last
/// sent a key to the pane (`None` when unknown). The window restarts
/// whenever the composer is seen busy, and its start is pushed forward to
/// the latest client keystroke, so "quiet" means both: nothing parked AND
/// nobody typing, for `timing.quiet` continuously.
pub(crate) fn quiet_gate(
    st: &mut QuietState,
    now: Instant,
    composer_clear: bool,
    key_age: Option<Duration>,
    timing: &QueueTiming,
) -> Gate {
    if let Some(until) = st.backoff_until {
        if now < until {
            return Gate::Hold(Hold::Backoff {
                remaining_ms: (until - now).as_millis() as u64,
            });
        }
        st.backoff_until = None;
    }
    if !composer_clear {
        st.clear_since = None;
        return Gate::Hold(Hold::ComposerBusy);
    }
    let mut since = st.clear_since.unwrap_or(now);
    if let Some(age) = key_age {
        let key_at = now.checked_sub(age).unwrap_or(now);
        if key_at > since {
            since = key_at;
        }
    }
    st.clear_since = Some(since);
    let elapsed = now - since;
    if elapsed < timing.quiet {
        let remaining_ms = (timing.quiet - elapsed).as_millis() as u64;
        return match key_age {
            Some(age) if age < timing.quiet => Gate::Hold(Hold::Typing {
                key_age_ms: age.as_millis() as u64,
            }),
            _ => Gate::Hold(Hold::QuietWindow { remaining_ms }),
        };
    }
    Gate::Deliver {
        clear_for_ms: elapsed.as_millis() as u64,
    }
}

/// A hold is logged at INFO when its kind changes and every 30 s while it
/// persists; the ticks in between go to DEBUG.
fn should_log_hold(st: &mut QuietState, label: &'static str, now: Instant) -> bool {
    const REPEAT: Duration = Duration::from_secs(30);
    let log = match st.last_logged {
        Some((last, at)) => last != label || now - at >= REPEAT,
        None => true,
    };
    if log {
        st.last_logged = Some((label, now));
    }
    log
}

static QUIET: LazyLock<Mutex<HashMap<String, QuietState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_quiet<R>(id: &str, f: impl FnOnce(&mut QuietState) -> R) -> R {
    let mut map = QUIET.lock().unwrap_or_else(|e| e.into_inner());
    f(map.entry(id.to_string()).or_default())
}

/// Terminal sessions whose queue this loop may drain: a non-empty queue on a
/// non-structured session that is not sunk (archived / snoozed / trashed).
/// Pure so the selection is unit-testable. Status is deliberately NOT a
/// criterion: a Claude pane accepts (and itself queues) input mid-turn, and
/// the composer read at delivery time is the real gate.
pub(crate) fn drain_candidates(instances: &[Instance]) -> Vec<String> {
    instances
        .iter()
        .filter(|i| {
            !i.queued_prompts.is_empty()
                && !i.is_structured()
                && !i.is_archived()
                && !i.is_snoozed()
                && !i.is_trashed()
        })
        .map(|i| i.id.clone())
        .collect()
}

pub(crate) async fn send_queue_drain_loop(state: Arc<AppState>) {
    let t = timing();
    tracing::info!(target: "server.send_queue",
        tick_ms = t.tick.as_millis() as u64, quiet_ms = t.quiet.as_millis() as u64,
        settle_ms = t.settle.as_millis() as u64,
        abort_backoff_ms = t.abort_backoff.as_millis() as u64,
        "send queue drain started");
    let mut interval = tokio::time::interval(t.tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // One in-flight delivery per session; a slow pane never blocks the rest.
    let mut in_flight: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        interval.tick().await;
        in_flight.retain(|_, h| !h.is_finished());
        let candidates = {
            let instances = state.instances.read().await;
            drain_candidates(&instances)
        };
        for id in candidates {
            if in_flight.contains_key(&id) {
                continue;
            }
            let st = Arc::clone(&state);
            let task_id = id.clone();
            let handle = crate::task_util::spawn_supervised(
                "server.send_queue_deliver",
                crate::task_util::PanicPolicy::Log,
                async move {
                    deliver_head_once(st, &task_id).await;
                },
            );
            in_flight.insert(id, handle);
        }
    }
}

/// Outcome of one delivery attempt, for logs and tests.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeliverOutcome {
    /// The row left the queue: it was submitted as a turn in the pane.
    Delivered,
    /// Held this tick by the quiet gate (composer busy, window not elapsed,
    /// human typing, or backing off after an abort).
    Held(Hold),
    /// A human's keystroke landed beside the paste; the paste was stripped,
    /// nothing submitted, the row stays at its position behind a back-off.
    Aborted(KeystrokeAbort),
    /// The pane is not running; the row waits for a start or restart.
    PaneMissing,
    /// A send failure; the row stays queued for the next tick.
    Failed(String),
    /// Nothing to deliver (empty queue, session gone, or not a candidate).
    Nothing,
}

/// Deliver the head of one session's queue if its composer is clear and has
/// been quiet for the whole window.
async fn deliver_head_once(state: Arc<AppState>, id: &str) -> DeliverOutcome {
    // Same lock order as `send_message`: the instance lock serialises this
    // pane's keystrokes against a concurrent POST /send; the submission
    // guard freezes the queue rows across snapshot -> send -> retire so an
    // edit or removal cannot land inside that window (#3621).
    let inst_lock = state.instance_lock(id).await;
    let _pane = inst_lock.lock().await;
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return DeliverOutcome::Nothing;
    };
    let (session_id, title, tool, head, history) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return DeliverOutcome::Nothing;
        };
        if inst.is_structured() || inst.is_archived() || inst.is_snoozed() || inst.is_trashed() {
            return DeliverOutcome::Nothing;
        }
        let mut queue = inst.queued_prompts.clone();
        queue.sort_by_key(|e| e.seq);
        let Some(head) = queue.first().cloned() else {
            return DeliverOutcome::Nothing;
        };
        // Every queued text is machine text: a copy of any of them parked
        // unsubmitted by an earlier attempt is completed, never refused as
        // a human draft.
        let history: Vec<String> = queue.iter().map(|e| e.text.clone()).collect();
        (
            inst.id.clone(),
            inst.title.clone(),
            inst.tool.clone(),
            head,
            history,
        )
    };
    if head.text.trim().is_empty() {
        // A husk (text-less row) can never be typed into a pane; retire it
        // so the queue behind it drains, exactly as the ACP drain does.
        tracing::warn!(target: "server.send_queue", session = %id, qid = %head.id,
            "queued send has no text; retiring it");
        state
            .session_service
            .retire_delivered_prompt(id, &head.id)
            .await;
        return DeliverOutcome::Nothing;
    }
    let text = head.text.clone();
    let text_for_send = text.clone();
    let tool_for_send = tool.clone();
    let quiet_id = id.to_string();
    let t = *timing();
    let outcome = tokio::task::spawn_blocking(move || -> DeliverOutcome {
        let session = match crate::tmux::Session::new(&session_id, &title) {
            Ok(s) => s,
            Err(e) => return DeliverOutcome::Failed(e.to_string()),
        };
        if !session.exists() {
            return DeliverOutcome::PaneMissing;
        }
        let composer_clear =
            match session.composer_clear_for_delivery(&text_for_send, &tool_for_send, &history) {
                Ok(clear) => clear,
                Err(e) => return DeliverOutcome::Failed(e.to_string()),
            };
        let key_age = session.client_key_age();
        let now = Instant::now();
        let gate = with_quiet(&quiet_id, |st| {
            quiet_gate(st, now, composer_clear, key_age, &t)
        });
        let clear_for_ms = match gate {
            Gate::Deliver { clear_for_ms } => clear_for_ms,
            Gate::Hold(hold) => return DeliverOutcome::Held(hold),
        };
        let delay = crate::agents::send_keys_enter_delay(&tool_for_send);
        match session.send_keys_verified_guarded(
            &text_for_send,
            delay,
            &tool_for_send,
            &history,
            t.settle,
        ) {
            Ok(GuardedSend::Delivered) => {
                tracing::debug!(target: "server.send_queue", session = %quiet_id,
                    clear_for_ms, key_age_ms = key_age.map(|a| a.as_millis() as u64),
                    "delivery typed and confirmed");
                DeliverOutcome::Delivered
            }
            Ok(GuardedSend::Aborted(abort)) => {
                with_quiet(&quiet_id, |st| {
                    st.clear_since = None;
                    st.backoff_until = Some(Instant::now() + t.abort_backoff);
                    st.aborts += 1;
                    st.last_logged = None;
                });
                DeliverOutcome::Aborted(abort)
            }
            Err(e) => match e.downcast::<crate::tmux::ParkedDraftRefusal>() {
                // The human started typing between the pre-check and the
                // send: nothing was typed, the row simply waits.
                Ok(_) => {
                    with_quiet(&quiet_id, |st| st.clear_since = None);
                    DeliverOutcome::Held(Hold::ComposerBusy)
                }
                Err(e) => DeliverOutcome::Failed(e.to_string()),
            },
        }
    })
    .await
    .unwrap_or_else(|e| DeliverOutcome::Failed(format!("delivery task panicked: {e}")));

    match &outcome {
        DeliverOutcome::Delivered => {
            // Same acknowledgement a live send makes: delivery is the
            // authoritative "someone is handling this" for the urgent flag.
            let ack = crate::hooks::ack_hook_urgent_on_send(id, &text);
            state
                .session_service
                .retire_delivered_prompt(id, &head.id)
                .await;
            with_quiet(id, |st| {
                st.clear_since = None;
                st.last_logged = None;
            });
            tracing::info!(target: "server.send_queue", session = %id, qid = %head.id,
                decision = "delivered",
                sender = head.origin_device.as_deref().unwrap_or("-"),
                urgent_ack = ack.as_str(), "queued send delivered");
        }
        DeliverOutcome::Held(hold) => {
            let label = hold.label();
            let now = Instant::now();
            if with_quiet(id, |st| should_log_hold(st, label, now)) {
                tracing::info!(target: "server.send_queue", session = %id, qid = %head.id,
                    decision = label, detail = ?hold, "queued send held");
            } else {
                tracing::debug!(target: "server.send_queue", session = %id, qid = %head.id,
                    decision = label, detail = ?hold, "queued send held");
            }
        }
        DeliverOutcome::Aborted(abort) => {
            let aborts = with_quiet(id, |st| st.aborts);
            tracing::warn!(target: "server.send_queue", session = %id, qid = %head.id,
                decision = "aborted:keystroke",
                before_chars = abort.before_chars, after_chars = abort.after_chars,
                typed_during_abort = abort.typed_during_abort, chip = abort.chip,
                rounds = abort.rounds, restored = abort.restored, aborts,
                backoff_ms = timing().abort_backoff.as_millis() as u64,
                "queued send aborted: human keystroke beside the paste; paste stripped, \
                 row re-queued at its position: {}", abort.detail);
        }
        DeliverOutcome::PaneMissing => {
            tracing::debug!(target: "server.send_queue", session = %id, qid = %head.id,
                decision = "held:pane_missing", "pane not running; queued send waits");
        }
        DeliverOutcome::Failed(e) => {
            // Typed-but-unconfirmed lands here too: the next tick finds the
            // parked copy, classifies it as this row's own text and submits
            // it with a bare Enter.
            tracing::warn!(target: "server.send_queue", session = %id, qid = %head.id,
                decision = "failed", "queued send not delivered: {e}");
        }
        DeliverOutcome::Nothing => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::{drain_candidates, quiet_gate, Gate, Hold, QueueTiming, QuietState};
    use crate::acp::state::QueuedPromptEntry;
    use crate::session::Instance;
    use std::time::{Duration, Instant};

    fn queued(id: &str) -> Instance {
        let mut i = Instance::new(id, "/tmp");
        i.id = id.to_string();
        i.queued_prompts.push(QueuedPromptEntry {
            id: format!("send-{id}"),
            seq: 1,
            text: "STATUS: shipped".to_string(),
            attachments: Vec::new(),
            created_at: "2026-09-04T08:00:00Z".to_string(),
            origin_device: Some("aoe send".to_string()),
        });
        i
    }

    #[test]
    fn only_terminal_sessions_with_a_queue_are_candidates() {
        let empty = {
            let mut i = Instance::new("empty", "/tmp");
            i.id = "empty".to_string();
            i
        };
        let live = queued("live");
        let mut archived = queued("archived");
        archived.archived_at = Some(chrono::Utc::now());
        let mut structured = queued("structured");
        structured.view = crate::session::View::Structured;
        let picked = drain_candidates(&[empty, live, archived, structured]);
        assert_eq!(picked, vec!["live".to_string()]);
    }

    const T: QueueTiming = QueueTiming::DEFAULT;
    const S: Duration = Duration::from_secs(1);

    /// The race itself: the composer clears and the human starts typing.
    /// The gate must hold for the whole quiet window, not fire on the first
    /// clear read.
    #[test]
    fn clear_composer_waits_out_the_quiet_window() {
        let mut st = QuietState::default();
        let t0 = Instant::now();
        // Old keystroke (well outside the window): the clock starts now.
        assert!(matches!(
            quiet_gate(&mut st, t0, true, Some(60 * S), &T),
            Gate::Hold(Hold::QuietWindow { .. })
        ));
        assert!(matches!(
            quiet_gate(&mut st, t0 + 2 * S, true, Some(62 * S), &T),
            Gate::Hold(Hold::QuietWindow { remaining_ms }) if remaining_ms <= 1000
        ));
        assert_eq!(
            quiet_gate(&mut st, t0 + 3 * S, true, Some(63 * S), &T),
            Gate::Deliver { clear_for_ms: 3000 }
        );
    }

    #[test]
    fn a_keystroke_inside_the_window_restarts_it() {
        let mut st = QuietState::default();
        let t0 = Instant::now();
        assert!(matches!(
            quiet_gate(&mut st, t0, true, Some(60 * S), &T),
            Gate::Hold(Hold::QuietWindow { .. })
        ));
        // Two seconds in, a human key lands on the pane (age 0).
        assert_eq!(
            quiet_gate(&mut st, t0 + 2 * S, true, Some(Duration::ZERO), &T),
            Gate::Hold(Hold::Typing { key_age_ms: 0 })
        );
        // The window now runs from that key, not from the first clear read.
        assert!(matches!(
            quiet_gate(&mut st, t0 + 4 * S, true, Some(2 * S), &T),
            Gate::Hold(Hold::Typing { key_age_ms: 2000 })
        ));
        assert!(matches!(
            quiet_gate(&mut st, t0 + 5 * S, true, Some(3 * S), &T),
            Gate::Deliver { clear_for_ms: 3000 }
        ));
    }

    #[test]
    fn busy_composer_resets_the_window() {
        let mut st = QuietState::default();
        let t0 = Instant::now();
        quiet_gate(&mut st, t0, true, None, &T);
        assert_eq!(
            quiet_gate(&mut st, t0 + 2 * S, false, None, &T),
            Gate::Hold(Hold::ComposerBusy)
        );
        assert!(st.clear_since.is_none());
        // Clear again: a fresh window, not the remainder of the old one.
        assert!(matches!(
            quiet_gate(&mut st, t0 + 3 * S, true, None, &T),
            Gate::Hold(Hold::QuietWindow { remaining_ms: 3000 })
        ));
    }

    #[test]
    fn abort_backoff_holds_then_expires_into_a_fresh_window() {
        let mut st = QuietState::default();
        let t0 = Instant::now();
        st.backoff_until = Some(t0 + T.abort_backoff);
        assert!(matches!(
            quiet_gate(&mut st, t0 + S, true, None, &T),
            Gate::Hold(Hold::Backoff { .. })
        ));
        assert!(matches!(
            quiet_gate(&mut st, t0 + T.abort_backoff, true, None, &T),
            Gate::Hold(Hold::QuietWindow { remaining_ms: 3000 })
        ));
        assert!(st.backoff_until.is_none());
    }

    #[test]
    fn unknown_key_age_falls_back_to_the_composer_clock() {
        let mut st = QuietState::default();
        let t0 = Instant::now();
        quiet_gate(&mut st, t0, true, None, &T);
        assert!(matches!(
            quiet_gate(&mut st, t0 + 3 * S, true, None, &T),
            Gate::Deliver { clear_for_ms: 3000 }
        ));
    }

    #[test]
    fn zero_quiet_window_delivers_on_first_clear_read() {
        let t = QueueTiming {
            quiet: Duration::ZERO,
            ..T
        };
        let mut st = QuietState::default();
        assert!(matches!(
            quiet_gate(&mut st, Instant::now(), true, Some(Duration::ZERO), &t),
            Gate::Deliver { .. }
        ));
    }
}
