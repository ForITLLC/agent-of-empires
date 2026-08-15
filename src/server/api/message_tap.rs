//! REST surface for the regex message subscriptions (WO#1393 D1).
//!
//! `POST /api/subscriptions {pattern, label?, session?}` registers a regex
//! against the live transcript tap; `GET /api/subscriptions` lists them;
//! `DELETE /api/subscriptions/{id}` removes one. Matches surface on the
//! daemon event bus as `regex_match` events (SSE `/api/events` + push).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use super::AppState;

#[derive(serde::Deserialize)]
pub struct CreateSubRequest {
    pub pattern: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
}

pub async fn create_subscription(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSubRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sub = state
        .message_tap
        .add(&req.pattern, req.label, req.session, now)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid pattern: {e}")))?;
    // Attach the tap to already-live sessions now rather than waiting for
    // the next watchdog tick.
    crate::server::message_tap::reconcile(&state).await;
    Ok(Json(serde_json::to_value(sub).unwrap_or_default()))
}

pub async fn list_subscriptions(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "subscriptions": state.message_tap.list() }))
}

pub async fn delete_subscription(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.message_tap.remove(&id) {
        crate::server::message_tap::reconcile(&state).await;
        Ok(Json(serde_json::json!({ "removed": id })))
    } else {
        Err((StatusCode::NOT_FOUND, format!("no subscription {id}")))
    }
}
