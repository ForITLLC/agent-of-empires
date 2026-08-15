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

/// Filters for `GET /api/cap-incidents`; all optional and combinable.
#[derive(serde::Deserialize)]
pub struct CapIncidentQuery {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub open: Option<bool>,
}

/// `GET /api/cap-incidents?session=&profile=&open=` — the WO#1393 D2 cap
/// incident ledger (onset, state trail, resolution, running duration for
/// open incidents), read from the same file the pane watchdog persists.
pub async fn get_cap_incidents(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<CapIncidentQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Some(path) = crate::server::cap_ledger::ledger_path() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no app dir; cap-incident ledger unavailable".into(),
        ));
    };
    let ledger = crate::server::cap_ledger::CapLedger::load(&path);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rows: Vec<serde_json::Value> = ledger
        .query(q.session.as_deref(), q.profile.as_deref(), q.open)
        .into_iter()
        .map(|i| {
            let mut v = serde_json::to_value(i).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "duration_secs".into(),
                    serde_json::json!(i.duration_secs(now_secs)),
                );
                obj.insert(
                    "duration".into(),
                    serde_json::json!(crate::server::pane_watchdog::fmt_hm(
                        i.duration_secs(now_secs)
                    )),
                );
            }
            v
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "updated": ledger.updated, "incidents": rows }),
    ))
}
