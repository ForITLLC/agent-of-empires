//! REST handlers for the shared per-account capacity state.
//!
//! Two endpoints under `/api/capacity`:
//!   - GET → the whole [`CapacityState`] map, as persisted
//!   - PATCH → merge `{profiles: {name: {headroom, cap_kind?, note?,
//!     reset_at?}}}` into the persisted state, stamping each named entry
//!     with now
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

use crate::server::capacity::{capacity_path, CapacityState, ProfileCapacity};

use super::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    pub profiles: HashMap<String, ProfilePatch>,
}

/// One profile's claim in a PATCH body. `headroom` is mandatory so a
/// caller can never touch an entry without taking a position on whether
/// the account is serving; `cap_kind`/`note`/`reset_at` are kept when
/// omitted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePatch {
    pub headroom: bool,
    #[serde(default)]
    pub cap_kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub reset_at: Option<u64>,
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
        if patch.reset_at.is_some() {
            entry.reset_at = patch.reset_at;
        }
        entry.updated = now_secs;
    }
    state.updated = now_secs;
}

/// Merge one profile's PATCH into the state and return the resulting
/// entry. Same semantics as [`apply_patch`] for a single profile; backs
/// `PATCH /api/capacity/{profile}` (WO#445 — before it existed the
/// per-profile URL fell through to the SPA fallback and answered 405,
/// so the Commander could not re-open relocation).
fn apply_profile_patch(
    state: &mut CapacityState,
    name: &str,
    patch: ProfilePatch,
    now_secs: u64,
) -> ProfileCapacity {
    apply_patch(state, HashMap::from([(name.to_string(), patch)]), now_secs);
    state.profiles.get(name).cloned().unwrap_or_default()
}

pub async fn get_capacity(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = capacity_path().ok_or_else(no_app_dir)?;
    Ok(Json(crate::server::capacity::annotate_staleness(
        &CapacityState::load(&path),
        now_secs(),
    )))
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

/// GET /api/capacity/{profile} — one profile's entry, 404 when absent.
pub async fn get_capacity_profile(
    State(_state): State<Arc<AppState>>,
    axum::extract::Path(profile): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = capacity_path().ok_or_else(no_app_dir)?;
    let annotated =
        crate::server::capacity::annotate_staleness(&CapacityState::load(&path), now_secs());
    annotated
        .get("profiles")
        .and_then(|p| p.get(&profile))
        .cloned()
        .map(Json)
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no capacity entry for profile '{profile}'"),
        ))
}

/// PATCH /api/capacity/{profile} — merge one profile's claim and return
/// the resulting entry (WO#445: the Commander grant path).
pub async fn patch_capacity_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(profile): axum::extract::Path<String>,
    req: Result<Json<ProfilePatch>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ProfileCapacity>, (StatusCode, String)> {
    if state.read_only {
        return Err((StatusCode::FORBIDDEN, "Server is in read-only mode".into()));
    }
    let Json(patch) = req.map_err(|rej| (rej.status(), rej.body_text()))?;
    let path = capacity_path().ok_or_else(no_app_dir)?;
    let mut cap = CapacityState::load(&path);
    let entry = apply_profile_patch(&mut cap, &profile, patch, now_secs());
    cap.save(&path);
    tracing::info!(
        target: "server.capacity",
        profile = %profile,
        headroom = entry.headroom,
        "capacity profile patched"
    );
    Ok(Json(entry))
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
            reset_at: None,
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
                updated: NOW - 100,
                ..Default::default()
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
                ..Default::default()
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
    fn patch_sets_reset_at_and_keeps_when_omitted() {
        // WO#942 item A: a cap observation can carry the reset clock the
        // banner named; a later patch that omits it must not erase it.
        let mut state = CapacityState::default();
        let with_reset = ProfilePatch {
            headroom: false,
            cap_kind: Some("weekly".to_string()),
            note: None,
            reset_at: Some(NOW + 7200),
        };
        apply_patch(
            &mut state,
            HashMap::from([("forit-main".to_string(), with_reset)]),
            NOW,
        );
        assert_eq!(state.profiles["forit-main"].reset_at, Some(NOW + 7200));

        apply_patch(
            &mut state,
            HashMap::from([("forit-main".to_string(), patch(true, None, None))]),
            NOW + 10,
        );
        let entry = &state.profiles["forit-main"];
        assert!(entry.headroom);
        assert_eq!(entry.reset_at, Some(NOW + 7200));
    }

    // ── WO#445: per-profile PATCH (Commander grant path) ─────────────

    #[test]
    fn profile_patch_grants_headroom_and_returns_entry() {
        let mut state = CapacityState::default();
        let entry = apply_profile_patch(
            &mut state,
            "forit-main",
            patch(true, None, Some("WO#445 probe")),
            NOW,
        );
        assert!(entry.headroom);
        assert_eq!(entry.updated, NOW);
        assert_eq!(entry.note.as_deref(), Some("WO#445 probe"));
        assert!(state.verified_headroom("forit-main", NOW));
        assert_eq!(state.updated, NOW);
    }

    #[test]
    fn profile_patch_keeps_kind_and_note_when_omitted() {
        let mut state = CapacityState::default();
        state.profiles.insert(
            "xce-main".to_string(),
            ProfileCapacity {
                headroom: false,
                cap_kind: Some("fable-credit".to_string()),
                note: Some("watchdog observed".to_string()),
                updated: NOW - 100,
                ..Default::default()
            },
        );
        let entry = apply_profile_patch(&mut state, "xce-main", patch(true, None, None), NOW);
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
