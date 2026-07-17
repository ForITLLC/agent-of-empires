//! REST handler for the durable fleet message log.
//!
//! `GET /api/messages` returns recently logged sends, newest first.
//! Query params:
//!   - `session`: restrict to one target session id
//!   - `limit`: max rows (default 100, capped at 1000)
//!
//! Rows are written best-effort by the send paths (`aoe send` and
//! `POST /api/sessions/{id}/send`); see [`crate::messages`].

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::messages::{default_db_path, MessageLog, DEFAULT_RETENTION};

use super::AppState;

#[derive(Deserialize)]
pub struct MessagesQuery {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

pub async fn get_messages(
    State(_state): State<Arc<AppState>>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(100).min(1000);
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let path = default_db_path()?;
        let log = MessageLog::open(&path, DEFAULT_RETENTION)?;
        let rows = match &q.session {
            Some(session) => log.for_session(session, limit),
            None => log.recent(limit),
        };
        Ok(rows
            .into_iter()
            .filter_map(|(seq, rec)| {
                let mut value = serde_json::to_value(rec).ok()?;
                value.as_object_mut()?.insert("seq".into(), seq.into());
                Some(value)
            })
            .collect())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("message log task panicked: {e}"),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("message log unavailable: {e}"),
        )
    })?;
    Ok(Json(serde_json::json!({ "messages": rows })))
}
