//! Push notification for things the fleet must not miss.
//!
//! A rate limit was discoverable only by POLLING, and every poller had been
//! switched off, so the first detector was a human noticing his own session had
//! stopped working. The instruction that came out of that is the design here:
//! DO NOT CHECK, CATCH. The daemon already knows the instant it sees a cap
//! banner on a pane; the defect was that it told nobody, it only wrote a file
//! and waited to be asked.
//!
//! So detection EMITS, and interested parties SUBSCRIBE:
//!   `GET  /api/events`   Server-Sent Events, live stream, one line per event.
//!   `POST /api/webhooks` register a URL; the daemon POSTs each event to it.
//!
//! Push costs nothing when idle, which is precisely why it survives the
//! teardown that killed the polling layer. There is no timer here and nothing
//! to schedule.
//!
//! EMISSION IS NEVER GATED BY A TOGGLE. That is deliberate and it is the whole
//! lesson: the activity classes govern what SUBSCRIBERS DO (page a human,
//! relocate a session), never whether the daemon is allowed to notice. A
//! detector you can switch off is how a kill switch ends up wired to nothing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::AppState;
use crate::util::now_ms;

/// How many recent events are replayed to a subscriber that connects late.
/// A cap fires once; a notifier that reconnects a second later must still see
/// it, or the push surface has the same "you had to be looking" flaw as the
/// polling it replaces.
const REPLAY_DEPTH: usize = 64;

/// Bound on the live broadcast channel. A slow subscriber lags rather than
/// blocking the detector; the detector must never wait on a consumer.
const CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetEvent {
    /// Monotonic per-daemon sequence, so a subscriber can tell "I missed some"
    /// from "nothing happened".
    pub seq: u64,
    pub at_ms: u64,
    /// What happened: `cap`, `auth`, `overload`, ... Matches the urgent kinds
    /// the pane watchdog already classifies, so the two never drift.
    pub kind: String,
    pub session_id: String,
    pub title: String,
    pub profile: String,
    pub detail: String,
}

pub struct EventBus {
    tx: broadcast::Sender<FleetEvent>,
    recent: Mutex<VecDeque<FleetEvent>>,
    seq: Mutex<u64>,
    webhooks: Mutex<Vec<Webhook>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    /// Empty = every kind. Otherwise only these kinds are delivered.
    #[serde(default)]
    pub kinds: Vec<String>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            tx,
            recent: Mutex::new(VecDeque::new()),
            seq: Mutex::new(0),
            webhooks: Mutex::new(Vec::new()),
        }
    }

    fn lock_recent(&self) -> std::sync::MutexGuard<'_, VecDeque<FleetEvent>> {
        self.recent.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Publish an event. Never fails, never blocks, never consults a toggle.
    ///
    /// Returns the event as published (with its sequence number) so the caller
    /// can log exactly what subscribers will see.
    pub fn emit(
        &self,
        kind: &str,
        session_id: &str,
        title: &str,
        profile: &str,
        detail: &str,
    ) -> FleetEvent {
        let seq = {
            let mut s = self.seq.lock().unwrap_or_else(|p| p.into_inner());
            *s += 1;
            *s
        };
        let ev = FleetEvent {
            seq,
            at_ms: now_ms(),
            kind: kind.to_string(),
            session_id: session_id.to_string(),
            title: title.to_string(),
            profile: profile.to_string(),
            detail: detail.to_string(),
        };
        {
            let mut r = self.lock_recent();
            r.push_back(ev.clone());
            while r.len() > REPLAY_DEPTH {
                r.pop_front();
            }
        }
        // A send with no receivers is an Err, and that is FINE: the replay
        // buffer and the webhook fan-out below are the durable paths. Treating
        // "nobody is streaming right now" as a failure would drop the event
        // exactly when it matters least to the stream and most to the record.
        let _ = self.tx.send(ev.clone());
        ev
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FleetEvent> {
        self.tx.subscribe()
    }

    /// Events still in the replay window, oldest first.
    pub fn recent(&self, since_seq: u64) -> Vec<FleetEvent> {
        self.lock_recent()
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect()
    }

    pub fn register_webhook(&self, url: &str, kinds: Vec<String>) -> Webhook {
        let hook = Webhook {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            url: url.to_string(),
            kinds,
        };
        self.webhooks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(hook.clone());
        hook
    }

    pub fn webhooks(&self) -> Vec<Webhook> {
        self.webhooks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn remove_webhook(&self, id: &str) -> bool {
        let mut w = self.webhooks.lock().unwrap_or_else(|p| p.into_inner());
        let before = w.len();
        w.retain(|h| h.id != id);
        w.len() != before
    }

    /// Webhooks that want this kind. Empty `kinds` means "everything".
    pub fn subscribers_for(&self, kind: &str) -> Vec<Webhook> {
        self.webhooks()
            .into_iter()
            .filter(|h| h.kinds.is_empty() || h.kinds.iter().any(|k| k == kind))
            .collect()
    }
}

/// Emit, then fan out to registered webhooks off-thread.
///
/// The detector calls this and returns immediately: an unreachable webhook must
/// never stall the pane watchdog, because the watchdog is the thing that
/// noticed the cap.
pub fn emit_and_fan_out(
    state: &Arc<AppState>,
    kind: &str,
    session_id: &str,
    title: &str,
    profile: &str,
    detail: &str,
) {
    let ev = state.events.emit(kind, session_id, title, profile, detail);
    tracing::info!(
        target: "server.events",
        seq = ev.seq,
        kind = %ev.kind,
        session = %ev.session_id,
        "fleet event emitted"
    );
    let hooks = state.events.subscribers_for(kind);
    if hooks.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "server.events", error = %e, "webhook client build failed");
                return;
            }
        };
        for h in hooks {
            match client.post(&h.url).json(&ev).send().await {
                Ok(r) => tracing::info!(
                    target: "server.events",
                    webhook = %h.id, status = r.status().as_u16(), "webhook delivered"
                ),
                Err(e) => tracing::warn!(
                    target: "server.events",
                    webhook = %h.id, error = %e, "webhook delivery failed"
                ),
            }
        }
    });
}

#[derive(Deserialize)]
pub struct EventsQuery {
    /// Replay everything after this sequence before streaming live.
    #[serde(default)]
    pub since: u64,
}

/// `GET /api/events` — SSE. Replays the buffer, then streams live.
pub async fn stream_events(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Response {
    use axum::response::sse::{KeepAlive, Sse};
    use futures_util::stream::{self, StreamExt};

    let backlog = state.events.recent(q.since);
    let rx = state.events.subscribe();

    let replay = stream::iter(backlog).map(sse_line);
    // `unfold` carries the receiver across polls without another stream-adapter
    // crate. A lagged subscriber resumes instead of ending: tearing the stream
    // down because a consumer was briefly slow would recreate, in the push
    // rail itself, the missed notification it exists to prevent.
    let live = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((sse_line(ev), rx)),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(target: "server.events", skipped = n, "SSE subscriber lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(replay.chain(live))
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn sse_line(ev: FleetEvent) -> Result<axum::response::sse::Event, std::convert::Infallible> {
    Ok(axum::response::sse::Event::default()
        .event("fleet")
        .json_data(&ev)
        .unwrap_or_default())
}

#[derive(Deserialize)]
pub struct RegisterWebhookRequest {
    pub url: String,
    #[serde(default)]
    pub kinds: Vec<String>,
}

pub async fn register_webhook(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterWebhookRequest>,
) -> Response {
    if !(req.url.starts_with("http://") || req.url.starts_with("https://")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_url",
                "message": "webhook url must be http:// or https://",
            })),
        )
            .into_response();
    }
    let hook = state.events.register_webhook(&req.url, req.kinds);
    (StatusCode::CREATED, Json(hook)).into_response()
}

pub async fn list_webhooks(State(state): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({ "webhooks": state.events.webhooks() })).into_response()
}

pub async fn delete_webhook(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if state.events.remove_webhook(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not_found", "id": id })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cap_is_emitted_even_with_nobody_listening() {
        // The failure this exists for: the detector saw it and told nobody.
        // With zero subscribers the event must still be recorded, or a notifier
        // that connects one second later learns nothing.
        let bus = EventBus::new();
        let ev = bus.emit(
            "cap",
            "s1",
            "for-Finance",
            "forit-main",
            "usage limit banner",
        );
        assert_eq!(ev.seq, 1);
        assert_eq!(bus.recent(0).len(), 1);
        assert_eq!(bus.recent(0)[0].kind, "cap");
    }

    #[test]
    fn a_late_subscriber_still_sees_what_it_missed() {
        let bus = EventBus::new();
        bus.emit("cap", "s1", "a", "forit-main", "");
        bus.emit("auth", "s2", "b", "gna-main", "");
        let missed = bus.recent(0);
        assert_eq!(missed.len(), 2);
        // and it can resume from where it got to
        assert_eq!(bus.recent(1).len(), 1);
        assert_eq!(bus.recent(1)[0].kind, "auth");
        assert!(bus.recent(2).is_empty());
    }

    #[tokio::test]
    async fn a_live_subscriber_receives_without_asking() {
        // "Do not check, catch": the subscriber never polls.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.emit("cap", "s9", "for-AVP", "forit-main", "5h limit reached");
        let got = rx.try_recv().expect("subscriber got nothing");
        assert_eq!(got.kind, "cap");
        assert_eq!(got.session_id, "s9");
    }

    #[test]
    fn the_replay_window_is_bounded() {
        let bus = EventBus::new();
        for i in 0..(REPLAY_DEPTH + 20) {
            bus.emit("cap", &format!("s{i}"), "t", "p", "");
        }
        assert_eq!(bus.recent(0).len(), REPLAY_DEPTH);
    }

    #[test]
    fn sequence_numbers_let_a_subscriber_detect_a_gap() {
        let bus = EventBus::new();
        let a = bus.emit("cap", "s1", "t", "p", "");
        let b = bus.emit("cap", "s2", "t", "p", "");
        assert_eq!(b.seq, a.seq + 1, "a gap must be detectable, not silent");
    }

    #[test]
    fn a_webhook_filters_by_kind_and_empty_means_everything() {
        let bus = EventBus::new();
        bus.register_webhook("https://example.test/cap", vec!["cap".into()]);
        bus.register_webhook("https://example.test/all", vec![]);
        assert_eq!(bus.subscribers_for("cap").len(), 2);
        assert_eq!(bus.subscribers_for("auth").len(), 1);
        assert_eq!(
            bus.subscribers_for("auth")[0].url,
            "https://example.test/all"
        );
    }

    #[test]
    fn a_webhook_can_be_removed_and_removal_is_reported_honestly() {
        let bus = EventBus::new();
        let h = bus.register_webhook("https://example.test/x", vec![]);
        assert!(bus.remove_webhook(&h.id));
        assert!(
            !bus.remove_webhook(&h.id),
            "a second removal must report false"
        );
        assert!(bus.webhooks().is_empty());
    }
}
