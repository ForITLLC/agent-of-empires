//! REST handlers for the shared per-account capacity state.
//!
//! Two endpoints under `/api/capacity`:
//!   - GET → the whole [`CapacityState`] map, as persisted
//!   - PATCH → merge `{profiles: {name: {headroom, cap_kind?, note?}}}`
//!     into the persisted state, stamping each named entry with now
//!
//! This is the ONLY path that can grant positive headroom: the pane
//! watchdog reads this state to pick relocation targets and only ever
//! revokes claims (see `crate::server::capacity`). The Commander (or an
//! operator) PATCHes a profile to `headroom: true` after an empirical
//! serve probe, and the claim expires after `HEADROOM_TTL_SECS`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::server::capacity::{capacity_path, CapacityState};

use super::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    pub profiles: HashMap<String, ProfilePatch>,
}

/// One profile's claim in a PATCH body. `headroom` is mandatory so a
/// caller can never touch an entry without taking a position on whether
/// the account is serving; `cap_kind`/`note` are kept when omitted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePatch {
    pub headroom: bool,
    #[serde(default)]
    pub cap_kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn no_app_dir() -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "no app dir; capacity state unavailable".into(),
    )
}

/// Merge a PATCH body into the loaded state. Entries not named in the
/// body are untouched; named entries take the body's headroom verdict,
/// replace cap_kind/note only when provided (an omitted cap_kind keeps
/// the last observed family as provenance), and get stamped with now.
fn apply_patch(state: &mut CapacityState, profiles: HashMap<String, ProfilePatch>, now_secs: u64) {
    for (name, patch) in profiles {
        let entry = state.profiles.entry(name).or_default();
        entry.headroom = patch.headroom;
        if patch.cap_kind.is_some() {
            entry.cap_kind = patch.cap_kind;
        }
        if patch.note.is_some() {
            entry.note = patch.note;
        }
        entry.updated = now_secs;
    }
    state.updated = now_secs;
}

pub async fn get_capacity(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<CapacityState>, (StatusCode, String)> {
    let path = capacity_path().ok_or_else(no_app_dir)?;
    Ok(Json(CapacityState::load(&path)))
}

pub async fn patch_capacity(
    State(state): State<Arc<AppState>>,
    req: Result<Json<PatchRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<CapacityState>, (StatusCode, String)> {
    if state.read_only {
        return Err((StatusCode::FORBIDDEN, "Server is in read-only mode".into()));
    }
    let Json(req) = req.map_err(|rej| (rej.status(), rej.body_text()))?;
    if req.profiles.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "profiles map is empty; name at least one profile".into(),
        ));
    }
    let path = capacity_path().ok_or_else(no_app_dir)?;
    let mut cap = CapacityState::load(&path);
    apply_patch(&mut cap, req.profiles, now_secs());
    cap.save(&path);
    tracing::info!(
        target: "server.capacity",
        profiles = cap.profiles.len(),
        "capacity state patched"
    );
    Ok(Json(cap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::capacity::ProfileCapacity;

    const NOW: u64 = 1_800_000_000;

    fn patch(headroom: bool, cap_kind: Option<&str>, note: Option<&str>) -> ProfilePatch {
        ProfilePatch {
            headroom,
            cap_kind: cap_kind.map(str::to_string),
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn patch_grants_headroom_and_stamps_now() {
        let mut state = CapacityState::default();
        apply_patch(
            &mut state,
            HashMap::from([(
                "forit-main".to_string(),
                patch(true, None, Some("serve probe ok")),
            )]),
            NOW,
        );
        assert!(state.verified_headroom("forit-main", NOW));
        let entry = &state.profiles["forit-main"];
        assert_eq!(entry.updated, NOW);
        assert_eq!(entry.note.as_deref(), Some("serve probe ok"));
        assert_eq!(state.updated, NOW);
    }

    #[test]
    fn patch_revokes_headroom_with_kind() {
        let mut state = CapacityState::default();
        apply_patch(
            &mut state,
            HashMap::from([(
                "forit-backup".to_string(),
                patch(false, Some("fable-credit"), None),
            )]),
            NOW,
        );
        assert!(!state.verified_headroom("forit-backup", NOW));
        assert_eq!(
            state.profiles["forit-backup"].cap_kind.as_deref(),
            Some("fable-credit")
        );
    }

    #[test]
    fn patch_leaves_unnamed_entries_untouched() {
        let mut state = CapacityState::default();
        state.profiles.insert(
            "gna-main".to_string(),
            ProfileCapacity {
                headroom: false,
                cap_kind: Some("monthly-spend".to_string()),
                note: None,
                updated: NOW - 100,
            },
        );
        apply_patch(
            &mut state,
            HashMap::from([("forit-main".to_string(), patch(true, None, None))]),
            NOW,
        );
        let untouched = &state.profiles["gna-main"];
        assert_eq!(untouched.updated, NOW - 100);
        assert_eq!(untouched.cap_kind.as_deref(), Some("monthly-spend"));
    }

    #[test]
    fn patch_keeps_cap_kind_and_note_when_omitted() {
        let mut state = CapacityState::default();
        state.profiles.insert(
            "xce-main".to_string(),
            ProfileCapacity {
                headroom: false,
                cap_kind: Some("fable-credit".to_string()),
                note: Some("watchdog observed".to_string()),
                updated: NOW - 100,
            },
        );
        apply_patch(
            &mut state,
            HashMap::from([("xce-main".to_string(), patch(true, None, None))]),
            NOW,
        );
        let entry = &state.profiles["xce-main"];
        assert!(entry.headroom);
        assert_eq!(entry.cap_kind.as_deref(), Some("fable-credit"));
        assert_eq!(entry.note.as_deref(), Some("watchdog observed"));
        assert_eq!(entry.updated, NOW);
    }

    #[test]
    fn body_rejects_unknown_fields_and_requires_headroom() {
        let err = serde_json::from_str::<PatchRequest>(
            r#"{"profiles": {"forit-main": {"headroom": true, "bogus": 1}}}"#,
        );
        assert!(err.is_err(), "unknown field must be rejected");
        let err = serde_json::from_str::<PatchRequest>(r#"{"profiles": {"forit-main": {}}}"#);
        assert!(err.is_err(), "headroom is mandatory");
    }
}
