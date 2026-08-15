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

/// Classify which cap family a pane tail's banner belongs to. The explicit
/// clock names win over the Fable heuristic: the real class-3 banner reads
/// "hit your monthly spend limit ... keep using Fable 5", so a Fable-first
/// check classified every spend ceiling as a model credit pool and the
/// spend arm was unreachable on live banner text (WO#1283A, three missed
/// spend blocks on 2026-08-06). Fable still wins over a bare week mention
/// inside its own banner ("hit your Fable usage limit for the week"): that
/// phrasing names no "weekly limit", so the weekly arm does not fire.
pub fn classify_cap_kind(content: &str) -> CapKind {
    let lower = content.to_ascii_lowercase();
    let fable_capped = (lower.contains("fable")
        && (lower.contains("limit") || lower.contains("credit")))
        || lower.contains("out of usage credits");
    if lower.contains("monthly spend") {
        CapKind::MonthlySpend
    } else if lower.contains("weekly limit") {
        CapKind::Weekly
    } else if lower.contains("session limit") || lower.contains("5-hour limit") {
        CapKind::Session
    } else if fable_capped {
        CapKind::Fable
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
    /// Per-family cap observations: cap-kind string -> unix seconds the
    /// banner was last seen. `cap_kind` alone remembers only the LAST family,
    /// so a session-cap observation used to erase the knowledge that the same
    /// account was weekly-dead, and the mover would stage a weekly-capped
    /// session into a weekly-dead account (WO#1393 fix a). Each family decays
    /// on its own clock via [`CapacityState::kind_dead`].
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dead_kinds: HashMap<String, u64>,
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
        entry.dead_kinds.insert(kind.as_str().to_string(), now_secs);
        entry.updated = now_secs;
        self.updated = now_secs;
    }

    /// True while `profile` carries a live observation of THIS cap family:
    /// stamped within the family's own decay window and not past a recorded
    /// reset time. The mover uses this to refuse staging a capped session
    /// into an account that is dead for the SAME kind, without freezing out
    /// accounts whose observation is about a different clock (WO#1393 fix a).
    pub fn kind_dead(&self, profile: &str, kind: CapKind, now_secs: u64) -> bool {
        let Some(entry) = self.profiles.get(profile) else {
            return false;
        };
        let Some(&stamped) = entry.dead_kinds.get(kind.as_str()) else {
            return false;
        };
        // A recorded reset in the past means the clock has already rolled
        // over; the observation is spent regardless of its TTL.
        if entry.reset_at.is_some_and(|reset| now_secs >= reset) {
            return false;
        }
        now_secs.saturating_sub(stamped) <= dead_kind_ttl_secs(kind)
    }

    /// Record an EMPIRICAL serving observation: a pane on this profile was
    /// seen actively working. That is stronger evidence than any stale cap
    /// stamp, so it grants headroom in-daemon (the one exception to the
    /// "claims only enter from outside" rule; the pane doing real work IS
    /// the serve probe). Account-scoped cap kinds (session/weekly/monthly)
    /// are cleared by any serving pane; the fable credit pool is cleared
    /// only when the serving pane was actually ON a fable model, since a
    /// pane serving on opus proves nothing about fable credit (WO#1393
    /// fix b: live capacity, not a stale capacity-file cache).
    pub fn grant_observed_serving(&mut self, profile: &str, fable_serving: bool, now_secs: u64) {
        let entry = self.profiles.entry(profile.to_string()).or_default();
        entry.headroom = true;
        entry.note = Some("observed serving pane".to_string());
        entry.updated = now_secs;
        let fable = CapKind::Fable.as_str();
        entry
            .dead_kinds
            .retain(|kind, _| kind == fable && !fable_serving);
        if entry.dead_kinds.is_empty() {
            entry.cap_kind = None;
        }
        self.updated = now_secs;
    }
}

/// How long an observed cap of each family stays trusted as "this account is
/// dead for that kind" absent a recorded reset time. Session windows roll
/// every 5 hours; weekly and monthly clocks move slowly enough that a day of
/// caution is cheap; the fable credit pool keeps the generic headroom TTL.
fn dead_kind_ttl_secs(kind: CapKind) -> u64 {
    match kind {
        CapKind::Session => 5 * 60 * 60,
        CapKind::Weekly | CapKind::MonthlySpend => 24 * 60 * 60,
        CapKind::Fable | CapKind::Unknown => HEADROOM_TTL_SECS,
    }
}

/// Wire-annotate a capacity state with its own age, per profile and for the
/// whole map: `age_secs` + `stale` on every row, `stale`/`stale_profiles`/
/// `age_secs` at the top, and a plain-words `staleness_note` when the map is
/// past [`HEADROOM_TTL_SECS`].
///
/// The stored state never says how old it is, and the automated truth-writer
/// (the fable capacity sentinel) was deliberately retired with the watchdog
/// teardown, so rows can sit for a week while `GET /api/capacity` answers
/// confidently. A capacity surface that a profile-move decision reads must
/// declare its age rather than let stale claims pass as live.
pub fn annotate_staleness(state: &CapacityState, now_secs: u64) -> serde_json::Value {
    let mut v = serde_json::to_value(state).unwrap_or_else(|_| serde_json::json!({}));
    let mut stale_profiles = 0u64;
    if let Some(profiles) = v.get_mut("profiles").and_then(|p| p.as_object_mut()) {
        for row in profiles.values_mut() {
            let updated = row.get("updated").and_then(|u| u.as_u64()).unwrap_or(0);
            let age = now_secs.saturating_sub(updated);
            let stale = age > HEADROOM_TTL_SECS;
            if stale {
                stale_profiles += 1;
            }
            if let Some(obj) = row.as_object_mut() {
                obj.insert("age_secs".into(), serde_json::json!(age));
                obj.insert("stale".into(), serde_json::json!(stale));
            }
        }
    }
    let map_age = now_secs.saturating_sub(state.updated);
    let map_stale = map_age > HEADROOM_TTL_SECS;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("age_secs".into(), serde_json::json!(map_age));
        obj.insert("stale".into(), serde_json::json!(map_stale));
        obj.insert("stale_profiles".into(), serde_json::json!(stale_profiles));
        if map_stale {
            obj.insert(
                "staleness_note".into(),
                serde_json::json!(format!(
                    "capacity data is STALE: no writer has touched this map in {map_age}s \
                     (TTL {HEADROOM_TTL_SECS}s). Treat stale rows as unknown, not as headroom."
                )),
            );
        }
    }
    v
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
    #[test]
    fn staleness_annotation_ages_every_row_and_the_map() {
        use super::*;
        const T: u64 = 1_800_000_000;
        let mut state = CapacityState {
            updated: T - HEADROOM_TTL_SECS - 100,
            ..Default::default()
        };
        state.profiles.insert(
            "fresh".into(),
            ProfileCapacity {
                headroom: true,
                updated: T - 60,
                ..Default::default()
            },
        );
        state.profiles.insert(
            "old".into(),
            ProfileCapacity {
                headroom: true,
                updated: T - HEADROOM_TTL_SECS - 1,
                ..Default::default()
            },
        );
        let v = annotate_staleness(&state, T);
        assert_eq!(v["profiles"]["fresh"]["stale"], false);
        assert_eq!(v["profiles"]["fresh"]["age_secs"], 60);
        assert_eq!(v["profiles"]["old"]["stale"], true);
        assert_eq!(v["profiles"]["old"]["age_secs"], HEADROOM_TTL_SECS + 1);
        assert_eq!(v["stale_profiles"], 1);
        assert_eq!(v["stale"], true, "map-level updated is past the TTL");
        let note = v["staleness_note"].as_str().unwrap_or("");
        assert!(
            note.contains("STALE"),
            "note must say the data is stale: {note}"
        );
        let fresh_map = annotate_staleness(
            &CapacityState {
                updated: T - 5,
                ..Default::default()
            },
            T,
        );
        assert_eq!(fresh_map["stale"], false);
        assert!(fresh_map.get("staleness_note").is_none());
    }

    use super::*;

    fn state_with(entries: &[(&str, bool, u64)]) -> CapacityState {
        let mut state = CapacityState::default();
        for (profile, headroom, updated) in entries {
            state.profiles.insert(
                profile.to_string(),
                ProfileCapacity {
                    headroom: *headroom,
                    updated: *updated,
                    ..Default::default()
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
                reset_at: Some(NOW + 3600),
                updated: NOW,
                ..Default::default()
            },
        );
        state.save(&path);
        let loaded = CapacityState::load(&path);
        assert_eq!(loaded.profiles["xce-main"].reset_at, Some(NOW + 3600));
    }

    #[test]
    fn kind_dead_tracks_families_independently() {
        let mut state = CapacityState::default();
        state.revoke_headroom("forit-main", CapKind::Weekly, NOW - 60);
        state.revoke_headroom("forit-main", CapKind::Session, NOW - 30);
        // Both observations live side by side even though cap_kind only
        // remembers the last one.
        assert!(state.kind_dead("forit-main", CapKind::Weekly, NOW));
        assert!(state.kind_dead("forit-main", CapKind::Session, NOW));
        assert!(!state.kind_dead("forit-main", CapKind::Fable, NOW));
        assert!(!state.kind_dead("forit-backup", CapKind::Weekly, NOW));
    }

    #[test]
    fn kind_dead_decays_per_family_and_respects_reset() {
        let mut state = CapacityState::default();
        // Session observations expire after their 5h window ...
        state.revoke_headroom("a", CapKind::Session, NOW - 5 * 3600 - 1);
        assert!(!state.kind_dead("a", CapKind::Session, NOW));
        // ... while a weekly observation of the same age is still live.
        state.revoke_headroom("b", CapKind::Weekly, NOW - 5 * 3600 - 1);
        assert!(state.kind_dead("b", CapKind::Weekly, NOW));
        // A recorded reset in the past spends the observation early.
        state.revoke_headroom("c", CapKind::Weekly, NOW - 60);
        state.profiles.get_mut("c").unwrap().reset_at = Some(NOW - 1);
        assert!(!state.kind_dead("c", CapKind::Weekly, NOW));
        // A future reset does not.
        state.profiles.get_mut("c").unwrap().reset_at = Some(NOW + 3600);
        assert!(state.kind_dead("c", CapKind::Weekly, NOW));
    }

    #[test]
    fn observed_serving_grants_headroom_and_clears_account_kinds() {
        let mut state = CapacityState::default();
        state.revoke_headroom("forit-main", CapKind::Weekly, NOW - 60);
        state.revoke_headroom("forit-main", CapKind::Fable, NOW - 30);
        // A non-fable serving pane clears the account-scoped kinds but says
        // nothing about the fable credit pool.
        state.grant_observed_serving("forit-main", false, NOW);
        assert!(state.verified_headroom("forit-main", NOW));
        assert!(!state.kind_dead("forit-main", CapKind::Weekly, NOW));
        assert!(state.kind_dead("forit-main", CapKind::Fable, NOW));
        assert_eq!(
            state.profiles["forit-main"].note.as_deref(),
            Some("observed serving pane")
        );
        // A fable serving pane clears the fable stamp too, and with no dead
        // kinds left the summary cap_kind resets.
        state.grant_observed_serving("forit-main", true, NOW);
        assert!(!state.kind_dead("forit-main", CapKind::Fable, NOW));
        assert!(state.profiles["forit-main"].cap_kind.is_none());
    }

    #[test]
    fn dead_kinds_roundtrip_and_default_empty() {
        // Legacy rows without the field must still parse.
        let legacy: ProfileCapacity =
            serde_json::from_str(r#"{"headroom": false, "updated": 5}"#).expect("legacy row");
        assert!(legacy.dead_kinds.is_empty());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capacity.json");
        let mut state = CapacityState::default();
        state.revoke_headroom("xce-main", CapKind::MonthlySpend, NOW);
        state.save(&path);
        let loaded = CapacityState::load(&path);
        assert!(loaded.kind_dead("xce-main", CapKind::MonthlySpend, NOW + 60));
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
        // The real class-3 banner names Fable in its remediation sentence
        // ("keep using Fable 5"), so the spend check must outrank the Fable
        // check or this arm is unreachable (WO#1283 D1, live exemplar from
        // session 8ceba7aa8b234910).
        assert_eq!(
            classify_cap_kind(
                "You've hit your monthly spend limit. Run /usage-credits to \
                 manage your limit and keep using Fable 5 or switch models to \
                 continue this chat."
            ),
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
