//! Drain of the server-owned prompt queue into TERMINAL (tmux) sessions.
//!
//! `POST /api/sessions/{id}/send` refuses to type into a Claude composer that
//! holds an operator's unsent draft (423 `parked_draft`; the refusal is the
//! WO#1872 guarantee). Before this module the refused message was simply
//! gone: the sender had to retry, and a fleet of senders whose retry windows
//! lapsed lost their reports. Now the refusal parks the message on the
//! session's server-owned prompt queue, the same `Instance::queued_prompts`
//! the structured view drains, persisted in the profile's `sessions.json`,
//! so it survives a daemon restart, and this loop delivers it the moment the
//! composer is clear.
//!
//! The guarantee is absolute here too. Every delivery goes through the same
//! verified send as a live one (`send_keys_verified_with_history`), which
//! re-reads the composer and refuses a human draft; this loop only adds a
//! cheap pre-check (`composer_clear_for_delivery`) so a session holding a
//! draft costs one capture per tick rather than a ten-second draft wait.
//! One row per session per tick, FIFO by `seq`, each confirmed submitted
//! before the next is considered, so a burst never fuses into one paste and
//! every queued message lands as its own turn.
//!
//! Structured sessions are NOT handled here: their queue drains through the
//! ACP worker (`acp_reconciler::drain_queued_prompts`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::state::AppState;
use crate::session::Instance;

/// Tick period. Delivery latency after a composer clears is bounded by this
/// plus one verified send (about a second of settle).
pub(crate) const DRAIN_INTERVAL: Duration = Duration::from_secs(3);

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
    let mut interval = tokio::time::interval(DRAIN_INTERVAL);
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
    /// The composer is not clear (operator draft, dialog, or still booting).
    ComposerBusy,
    /// The pane is not running; the row waits for a start or restart.
    PaneMissing,
    /// A send failure; the row stays queued for the next tick.
    Failed(String),
    /// Nothing to deliver (empty queue, session gone, or not a candidate).
    Nothing,
}

/// Deliver the head of one session's queue if its composer is clear.
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
    let outcome = tokio::task::spawn_blocking(move || -> DeliverOutcome {
        let session = match crate::tmux::Session::new(&session_id, &title) {
            Ok(s) => s,
            Err(e) => return DeliverOutcome::Failed(e.to_string()),
        };
        if !session.exists() {
            return DeliverOutcome::PaneMissing;
        }
        match session.composer_clear_for_delivery(&text_for_send, &tool_for_send, &history) {
            Ok(true) => {}
            Ok(false) => return DeliverOutcome::ComposerBusy,
            Err(e) => return DeliverOutcome::Failed(e.to_string()),
        }
        let delay = crate::agents::send_keys_enter_delay(&tool_for_send);
        match session.send_keys_verified_with_history(
            &text_for_send,
            delay,
            &tool_for_send,
            &history,
        ) {
            Ok(()) => DeliverOutcome::Delivered,
            Err(e) => match e.downcast::<crate::tmux::ParkedDraftRefusal>() {
                // The human started typing between the pre-check and the
                // send: nothing was typed, the row simply waits.
                Ok(_) => DeliverOutcome::ComposerBusy,
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
            tracing::info!(target: "server.send_queue", session = %id, qid = %head.id,
                sender = head.origin_device.as_deref().unwrap_or("-"),
                urgent_ack = ack.as_str(), "queued send delivered");
        }
        DeliverOutcome::ComposerBusy => {
            tracing::debug!(target: "server.send_queue", session = %id, qid = %head.id,
                "composer not clear; queued send waits");
        }
        DeliverOutcome::PaneMissing => {
            tracing::debug!(target: "server.send_queue", session = %id, qid = %head.id,
                "pane not running; queued send waits");
        }
        DeliverOutcome::Failed(e) => {
            // Typed-but-unconfirmed lands here too: the next tick finds the
            // parked copy, classifies it as this row's own text and submits
            // it with a bare Enter.
            tracing::warn!(target: "server.send_queue", session = %id, qid = %head.id,
                "queued send not delivered: {e}");
        }
        DeliverOutcome::Nothing => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::drain_candidates;
    use crate::acp::state::QueuedPromptEntry;
    use crate::session::Instance;

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
}
