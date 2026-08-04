//! Master power switch: one authoritative ON/OFF for aoe activity, plus the
//! wake registry that makes OFF enforceable against wakeups armed before the
//! flip.
//!
//! Wake state historically lived in three layers that could not see each
//! other (harness-side scheduled wakeups, OS launchd/crontab, daemon
//! re-invoke paths), so flipping one layer off never stopped the others.
//! This module gives the daemon the single word: `GET /api/power` is the
//! truth, `POST /api/power` flips it, and every armed wakeup is expected to
//! register here so OFF can enumerate and cancel what already exists rather
//! than only refusing future arms. Clients treat an unreachable daemon as
//! OFF, never ON.
//!
//! State persists to `<app_dir>/power.json` (registry included) and survives
//! daemon restarts. Absent file = ON with an empty registry, which preserves
//! pre-feature behavior on first boot.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::util::now_ms;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WakeEntry {
    pub id: String,
    pub session_id: String,
    /// What armed it: "schedule_wakeup", "cron", "monitor", ...
    pub kind: String,
    #[serde(default)]
    pub fire_at_ms: Option<u64>,
    #[serde(default)]
    pub note: String,
    pub armed_at_ms: u64,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub cancelled_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PowerFile {
    on: bool,
    since_ms: u64,
    changed_by: String,
    #[serde(default)]
    wakes: Vec<WakeEntry>,
}

impl Default for PowerFile {
    fn default() -> Self {
        Self {
            on: true,
            since_ms: now_ms(),
            changed_by: "default".to_string(),
            wakes: Vec::new(),
        }
    }
}

pub struct PowerRegistry {
    path: PathBuf,
    inner: Mutex<PowerFile>,
}

/// Registry entries older than this are pruned on save: a one-shot wakeup
/// whose fire time is long past is dead weight either way (it fired or its
/// session is gone), and pruning keeps power.json bounded.
const WAKE_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

pub fn power_file_path(app_dir: &std::path::Path) -> PathBuf {
    app_dir.join("power.json")
}

impl PowerRegistry {
    /// Registry rooted at the real install's app dir. An unresolvable app dir
    /// falls back to the working directory rather than panicking the daemon.
    pub fn load_from_app_dir() -> Self {
        match crate::session::get_app_dir() {
            Ok(dir) => Self::load(&dir),
            Err(_) => Self::load(std::path::Path::new(".")),
        }
    }

    /// Fresh registry in a unique temp dir; the test-support AppState uses
    /// this so tests never touch the real install's power.json.
    pub fn ephemeral() -> Self {
        let dir = std::env::temp_dir().join(format!("aoe-power-{}", uuid::Uuid::new_v4().simple()));
        let _ = std::fs::create_dir_all(&dir);
        Self::load(&dir)
    }

    pub fn load(app_dir: &std::path::Path) -> Self {
        let path = power_file_path(app_dir);
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<PowerFile>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PowerFile> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save_locked(&self, f: &mut PowerFile) {
        let cutoff = now_ms().saturating_sub(WAKE_RETENTION_MS);
        f.wakes.retain(|w| w.armed_at_ms >= cutoff);
        if let Ok(json) = serde_json::to_string_pretty(&*f) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    pub fn is_on(&self) -> bool {
        self.lock().on
    }

    pub fn snapshot(&self) -> (bool, u64, String, usize) {
        let f = self.lock();
        let live = f.wakes.iter().filter(|w| !w.cancelled).count();
        (f.on, f.since_ms, f.changed_by.clone(), live)
    }

    /// Flip the switch. Turning OFF cancels every live wake and returns their
    /// ids, so OFF is a fact about existing arms, not just future ones.
    pub fn set(&self, on: bool, changed_by: &str) -> Vec<String> {
        let mut f = self.lock();
        f.on = on;
        f.since_ms = now_ms();
        f.changed_by = changed_by.to_string();
        let mut cancelled = Vec::new();
        if !on {
            for w in f.wakes.iter_mut().filter(|w| !w.cancelled) {
                w.cancelled = true;
                w.cancelled_at_ms = Some(now_ms());
                cancelled.push(w.id.clone());
            }
        }
        self.save_locked(&mut f);
        cancelled
    }

    /// Register an armed wakeup. `None` when OFF: a session must not be able
    /// to re-arm around the kill switch.
    pub fn arm(
        &self,
        session_id: &str,
        kind: &str,
        fire_at_ms: Option<u64>,
        note: &str,
    ) -> Option<WakeEntry> {
        let mut f = self.lock();
        if !f.on {
            return None;
        }
        let entry = WakeEntry {
            id: uuid::Uuid::new_v4().simple().to_string()[..16].to_string(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            fire_at_ms,
            note: note.to_string(),
            armed_at_ms: now_ms(),
            cancelled: false,
            cancelled_at_ms: None,
        };
        f.wakes.push(entry.clone());
        self.save_locked(&mut f);
        Some(entry)
    }

    pub fn list(&self, session_id: Option<&str>) -> Vec<WakeEntry> {
        self.lock()
            .wakes
            .iter()
            .filter(|w| session_id.is_none_or(|s| w.session_id == s))
            .cloned()
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<WakeEntry> {
        self.lock().wakes.iter().find(|w| w.id == id).cloned()
    }

    pub fn cancel(&self, id: &str) -> Option<WakeEntry> {
        let mut f = self.lock();
        let entry = f.wakes.iter_mut().find(|w| w.id == id)?;
        if !entry.cancelled {
            entry.cancelled = true;
            entry.cancelled_at_ms = Some(now_ms());
        }
        let out = entry.clone();
        self.save_locked(&mut f);
        Some(out)
    }
}

/// 403 body every OFF-refused endpoint returns. The note tells the caller the
/// one way back on, so a refused session can surface it verbatim.
pub fn power_off_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "power_off",
            "note": "AOE is OFF (master kill switch). Flip with `aoe on` or POST /api/power {\"state\":\"on\"}",
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct SetPowerRequest {
    pub state: String,
    #[serde(default)]
    pub changed_by: Option<String>,
}

#[derive(Deserialize)]
pub struct ArmWakeRequest {
    pub session_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub fire_at_ms: Option<u64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize)]
pub struct WakeListQuery {
    #[serde(default)]
    pub session_id: Option<String>,
}

fn power_json(reg: &PowerRegistry) -> serde_json::Value {
    let (on, since_ms, changed_by, live_wakes) = reg.snapshot();
    serde_json::json!({
        "state": if on { "on" } else { "off" },
        "since_ms": since_ms,
        "changed_by": changed_by,
        "live_wakes": live_wakes,
    })
}

pub async fn get_power(State(state): State<Arc<AppState>>) -> Response {
    Json(power_json(&state.power)).into_response()
}

pub async fn set_power(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SetPowerRequest>,
) -> Response {
    if state.read_only {
        return super::api::read_only_response();
    }
    let on = match req.state.to_ascii_lowercase().as_str() {
        "on" => true,
        "off" => false,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_state",
                    "note": format!("state must be \"on\" or \"off\", got {other:?}"),
                })),
            )
                .into_response();
        }
    };
    let changed_by = req.changed_by.unwrap_or_else(|| "api".to_string());
    let cancelled = state.power.set(on, &changed_by);
    tracing::info!(
        target: "power.switch",
        state = if on { "on" } else { "off" },
        changed_by = %changed_by,
        cancelled_wakes = cancelled.len(),
        "master power switch flipped"
    );
    let mut body = power_json(&state.power);
    body["cancelled_wakes"] = serde_json::json!(cancelled);
    Json(body).into_response()
}

pub async fn arm_wake(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ArmWakeRequest>,
) -> Response {
    match state.power.arm(
        &req.session_id,
        req.kind.as_deref().unwrap_or("schedule_wakeup"),
        req.fire_at_ms,
        req.note.as_deref().unwrap_or(""),
    ) {
        Some(entry) => (StatusCode::CREATED, Json(entry)).into_response(),
        None => power_off_response(),
    }
}

pub async fn list_wakes(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WakeListQuery>,
) -> Response {
    let wakes = state.power.list(q.session_id.as_deref());
    Json(serde_json::json!({ "wakes": wakes })).into_response()
}

pub async fn get_wake(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.power.get(&id) {
        Some(entry) => Json(serde_json::json!({
            "wake": entry,
            "power": if state.power.is_on() { "on" } else { "off" },
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "wake_not_found", "id": id})),
        )
            .into_response(),
    }
}

pub async fn cancel_wake(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.power.cancel(&id) {
        Some(entry) => Json(entry).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "wake_not_found", "id": id})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(dir: &std::path::Path) -> PowerRegistry {
        PowerRegistry::load(dir)
    }

    #[test]
    fn absent_file_defaults_on_empty() {
        let dir = tempfile::tempdir().unwrap();
        let r = reg(dir.path());
        assert!(r.is_on());
        assert!(r.list(None).is_empty());
    }

    #[test]
    fn state_and_registry_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        {
            let r = reg(dir.path());
            r.arm("sess-a", "schedule_wakeup", Some(123), "tick")
                .unwrap();
            r.set(false, "test");
        }
        let r2 = reg(dir.path());
        assert!(!r2.is_on(), "OFF must survive a reload (daemon restart)");
        let wakes = r2.list(None);
        assert_eq!(wakes.len(), 1);
        assert!(
            wakes[0].cancelled,
            "flip to OFF cancels the armed wake, persisted"
        );
    }

    #[test]
    fn off_cancels_existing_and_refuses_new() {
        let dir = tempfile::tempdir().unwrap();
        let r = reg(dir.path());
        let armed = r.arm("sess-a", "schedule_wakeup", None, "").unwrap();
        let cancelled = r.set(false, "ben");
        assert_eq!(
            cancelled,
            vec![armed.id.clone()],
            "OFF enumerates and cancels what exists"
        );
        assert!(r.get(&armed.id).unwrap().cancelled);
        assert!(
            r.arm("sess-a", "schedule_wakeup", None, "").is_none(),
            "no re-arm while OFF"
        );
        let back_on = r.set(true, "ben");
        assert!(back_on.is_empty());
        assert!(r.arm("sess-a", "schedule_wakeup", None, "").is_some());
    }

    #[test]
    fn cancel_single_wake() {
        let dir = tempfile::tempdir().unwrap();
        let r = reg(dir.path());
        let a = r.arm("s1", "cron", None, "").unwrap();
        let b = r.arm("s2", "cron", None, "").unwrap();
        r.cancel(&a.id).unwrap();
        assert!(r.get(&a.id).unwrap().cancelled);
        assert!(!r.get(&b.id).unwrap().cancelled);
        assert_eq!(r.list(Some("s2")).len(), 1);
    }
}
