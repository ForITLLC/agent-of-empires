//! REST handler for the pane watchdog's per-tick classification snapshot.
//!
//! `GET /api/watchdog/classifications` returns the latest tick's per-session
//! classification rows (`{"updated": <secs>, "sessions": [{ts, title, id,
//! profile, model, state, decision, reason}, ...]}`), exactly as written by
//! the watchdog at the end of each tick. The tailable history lives in
//! `watchdog-classifications.log` next to the snapshot. WO#450.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use super::AppState;

pub async fn get_watchdog_classifications(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Some(path) = crate::server::pane_watchdog::class_snapshot_path() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no app dir; classification snapshot unavailable".into(),
        ));
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // No tick has run yet (or the watchdog is disabled): an empty
        // snapshot is a valid answer, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Json(serde_json::json!({ "updated": 0, "sessions": [] })));
        }
        Err(e) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("classification snapshot unreadable: {e}"),
            ));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("classification snapshot malformed: {e}"),
        )
    })?;
    Ok(Json(value))
}
