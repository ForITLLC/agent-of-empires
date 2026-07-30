//! Shared per-account capacity source of truth for cap relocation.
//!
//! WO#414 thrash postmortem (2026-07-15): the pane watchdog relocated capped
//! sessions to the first draw-order profile it had no cap OBSERVATION for,
//! and absence of an observation is not headroom. Sessions were bounced
//! capped-to-capped across accounts that were all out of credit. The durable
//! invariant this module enforces: a profile is a relocation target ONLY
//! while it carries a FRESH POSITIVE headroom claim in this shared state
//! file. Claims are written by the Commander (or an operator) after an
//! empirical serve probe, via PATCH /api/capacity; the watchdog itself only
//! ever REVOKES headroom (it observes cap banners, never successful serving),
//! so a claim can only enter the pool from outside the daemon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How long a positive headroom claim stays trusted before the profile drops
/// out of the relocation pool. Bounds how long a stale "serving" verdict can
/// keep attracting relocations between Commander probes.
pub const HEADROOM_TTL_SECS: u64 = 6 * 60 * 60;

/// The cap family observed on a pane banner. The families render different
/// banners and reset on different clocks (rolling session window, weekly,
/// first of month, model credit pool), so relocation state and escalations
/// must name WHICH clock fired instead of a generic "capped".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapKind {
    /// Model credit pool exhausted ("reached your Fable 5 limit",
    /// "out of usage credits").
    Fable,
    /// Weekly account limit ("hit your weekly limit").
    Weekly,
    /// Monthly spend ceiling ("monthly spend limit").
    MonthlySpend,
    /// Rolling session window ("session limit reached", "5-hour limit").
    Session,
    /// A cap banner that names no known family.
    Unknown,
}

impl CapKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CapKind::Fable => "fable-credit",
            CapKind::Weekly => "weekly",
            CapKind::MonthlySpend => "monthly-spend",
            CapKind::Session => "session",
            CapKind::Unknown => "unknown",
        }
    }
}

/// Classify which cap family a pane tail's banner belongs to. Fable is
/// checked first because its banners can also name a week ("hit your Fable
/// usage limit for the week"): the model credit pool is the operative
/// constraint there, not the account week.
pub fn classify_cap_kind(content: &str) -> CapKind {
    let lower = content.to_ascii_lowercase();
    let fable_capped = (lower.contains("fable")
        && (lower.contains("limit") || lower.contains("credit")))
        || lower.contains("out of usage credits");
    if fable_capped {
        CapKind::Fable
    } else if lower.contains("weekly limit") {
        CapKind::Weekly
    } else if lower.contains("monthly spend") {
        CapKind::MonthlySpend
    } else if lower.contains("session limit") || lower.contains("5-hour limit") {
        CapKind::Session
    } else {
        CapKind::Unknown
    }
}

/// One profile's capacity claim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileCapacity {
    /// Positive "this account is serving" claim. Only ever set true from
    /// outside the daemon (a Commander/operator probe via the capacity API);
    /// the watchdog revokes it on any observed cap banner.
    #[serde(default)]
    pub headroom: bool,
    /// The cap family last observed or reported on this profile, when known
    /// (a [`CapKind::as_str`] value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_kind: Option<String>,
    /// Free-text provenance for the claim, e.g. which probe produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Unix seconds when the recorded cap is expected to reset, when the
    /// banner or operator report named one. Lets relocation tooling answer
    /// "when does this account come back" without re-probing (WO#942).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
    /// Unix seconds of the last update to this entry; the freshness gate.
    #[serde(default)]
    pub updated: u64,
}

/// The whole shared capacity map, one entry per account profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapacityState {
    /// Unix seconds of the last mutation to any entry.
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub profiles: HashMap<String, ProfileCapacity>,
}

impl CapacityState {
    /// Read the state file; a missing or corrupt file yields the empty state,
    /// which grants headroom to NOTHING. Failing safe here is the point: with
    /// no readable claims the watchdog parks instead of moving.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Best-effort persist; the daemon must not die over a state write.
    pub fn save(&self, path: &Path) {
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!(target: "server.capacity", path = %path.display(), error = %e, "capacity state write failed");
                }
            }
            Err(e) => {
                tracing::warn!(target: "server.capacity", error = %e, "capacity state serialize failed");
            }
        }
    }

    /// True ONLY when the profile holds a positive headroom claim fresher
    /// than [`HEADROOM_TTL_SECS`]. An absent entry, a revoked entry, and a
    /// stale positive claim all answer false: unknown is never headroom.
    pub fn verified_headroom(&self, profile: &str, now_secs: u64) -> bool {
        self.profiles
            .get(profile)
            .is_some_and(|p| p.headroom && now_secs.saturating_sub(p.updated) <= HEADROOM_TTL_SECS)
    }

    /// Record an observed cap on a profile: drop its headroom claim and stamp
    /// the cap family. Observation beats any standing claim, so a profile
    /// showing a cap banner stops being a relocation target immediately no
    /// matter how fresh its last positive probe was.
    pub fn revoke_headroom(&mut self, profile: &str, kind: CapKind, now_secs: u64) {
        let entry = self.profiles.entry(profile.to_string()).or_default();
        entry.headroom = false;
        entry.cap_kind = Some(kind.as_str().to_string());
        entry.updated = now_secs;
        self.updated = now_secs;
    }
}

/// Where the shared capacity state lives: `AOE_CAPACITY_FILE` when set, else
/// `capacity.json` in the app dir. `None` only when no app dir resolves.
pub fn capacity_path() -> Option<PathBuf> {
    match std::env::var("AOE_CAPACITY_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("capacity.json")),
            Err(e) => {
                tracing::warn!(target: "server.capacity", error = %e, "no app dir; capacity state unavailable");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(entries: &[(&str, bool, u64)]) -> CapacityState {
        let mut state = CapacityState::default();
        for (profile, headroom, updated) in entries {
            state.profiles.insert(
                profile.to_string(),
                ProfileCapacity {
                    headroom: *headroom,
                    cap_kind: None,
                    note: None,
                    reset_at: None,
                    updated: *updated,
                },
            );
        }
        state
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn absent_profile_is_never_headroom() {
        let state = CapacityState::default();
        assert!(!state.verified_headroom("forit-main", NOW));
    }

    #[test]
    fn fresh_positive_claim_is_headroom() {
        let state = state_with(&[("forit-main", true, NOW - 60)]);
        assert!(state.verified_headroom("forit-main", NOW));
    }

    #[test]
    fn stale_positive_claim_is_not_headroom() {
        let state = state_with(&[("forit-main", true, NOW - HEADROOM_TTL_SECS - 1)]);
        assert!(!state.verified_headroom("forit-main", NOW));
    }

    #[test]
    fn fresh_revoked_claim_is_not_headroom() {
        let state = state_with(&[("forit-backup", false, NOW - 60)]);
        assert!(!state.verified_headroom("forit-backup", NOW));
    }

    #[test]
    fn revoke_drops_claim_and_stamps_kind() {
        let mut state = state_with(&[("forit-main", true, NOW - 60)]);
        state.revoke_headroom("forit-main", CapKind::Weekly, NOW);
        assert!(!state.verified_headroom("forit-main", NOW));
        let entry = &state.profiles["forit-main"];
        assert_eq!(entry.cap_kind.as_deref(), Some("weekly"));
        assert_eq!(entry.updated, NOW);
        assert_eq!(state.updated, NOW);
    }

    #[test]
    fn revoke_creates_entry_for_unknown_profile() {
        let mut state = CapacityState::default();
        state.revoke_headroom("xce-main", CapKind::Fable, NOW);
        assert!(!state.verified_headroom("xce-main", NOW));
        assert_eq!(
            state.profiles["xce-main"].cap_kind.as_deref(),
            Some("fable-credit")
        );
    }

    #[test]
    fn reset_at_defaults_none_and_roundtrips() {
        // Legacy rows without the field must still parse (WO#942 item A).
        let legacy: ProfileCapacity =
            serde_json::from_str(r#"{"headroom": false, "updated": 5}"#).expect("legacy row");
        assert_eq!(legacy.reset_at, None);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capacity.json");
        let mut state = CapacityState::default();
        state.profiles.insert(
            "xce-main".to_string(),
            ProfileCapacity {
                headroom: false,
                cap_kind: Some("weekly".to_string()),
                note: None,
                reset_at: Some(NOW + 3600),
                updated: NOW,
            },
        );
        state.save(&path);
        let loaded = CapacityState::load(&path);
        assert_eq!(loaded.profiles["xce-main"].reset_at, Some(NOW + 3600));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capacity.json");
        let mut state = state_with(&[("forit-main", true, NOW)]);
        state.updated = NOW;
        state.save(&path);
        let loaded = CapacityState::load(&path);
        assert!(loaded.verified_headroom("forit-main", NOW));
        assert_eq!(loaded.updated, NOW);
    }

    #[test]
    fn load_missing_or_corrupt_grants_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = CapacityState::load(&dir.path().join("nope.json"));
        assert!(missing.profiles.is_empty());
        let corrupt_path = dir.path().join("bad.json");
        std::fs::write(&corrupt_path, "{not json").expect("write");
        let corrupt = CapacityState::load(&corrupt_path);
        assert!(corrupt.profiles.is_empty());
    }

    #[test]
    fn classify_fable_credit() {
        assert_eq!(
            classify_cap_kind("You've reached your Fable 5 limit."),
            CapKind::Fable
        );
        assert_eq!(
            classify_cap_kind("Error: out of usage credits."),
            CapKind::Fable
        );
        assert_eq!(
            classify_cap_kind("You've hit your Fable usage limit for the week."),
            CapKind::Fable
        );
    }

    #[test]
    fn classify_weekly() {
        assert_eq!(
            classify_cap_kind("You've hit your weekly limit. It resets Jul 18."),
            CapKind::Weekly
        );
    }

    #[test]
    fn classify_monthly_spend() {
        assert_eq!(
            classify_cap_kind("Your account reached its monthly spend limit."),
            CapKind::MonthlySpend
        );
    }

    #[test]
    fn classify_session() {
        assert_eq!(
            classify_cap_kind("Session limit reached · resets 3pm"),
            CapKind::Session
        );
        assert_eq!(classify_cap_kind("5-hour limit reached"), CapKind::Session);
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_cap_kind("Claude usage limit reached."),
            CapKind::Unknown
        );
    }
}
