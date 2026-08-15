//! Durable per-session cap-incident ledger: the "idle on limit" timer.
//!
//! WO#1393 D2: a capped session used to announce its cap at the edge and
//! then sit silent; nothing recorded WHEN the limit hit, how the state
//! evolved, or when it resolved, so "how long has gna-finance been dead"
//! had no queryable answer. This module keeps one incident per cap episode:
//! opened on the cap edge, a state-change trail while it stands, resolved
//! when the pane demonstrably serves again (or its staged rebind restarts),
//! never deleted. The pane watchdog is the sole writer; `GET
//! /api/cap-incidents` reads the same file the watchdog persists.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One entry in an incident's state trail, appended only when the
/// classification state actually changes (LIVE-CAP-weekly → PARKED, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateChange {
    pub at: u64,
    pub state: String,
}

/// One cap episode for one session: onset to resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapIncident {
    pub session: String,
    pub title: String,
    pub profile: String,
    /// The cap family (a `CapKind::as_str` value); refined in place when a
    /// later tick classifies what an earlier `unknown` banner meant.
    pub kind: String,
    pub opened_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<u64>,
    /// Why the incident closed: `serving-turn`, `rebind-restart`, or
    /// `backfill-horizon` for reconstructed history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default)]
    pub changes: Vec<StateChange>,
    /// True when reconstructed from the notifier ledger rather than
    /// observed live; backfilled incidents are always closed, so they can
    /// never absorb a fresh cap into last week's onset.
    #[serde(default)]
    pub backfilled: bool,
}

impl CapIncident {
    pub fn duration_secs(&self, now_secs: u64) -> u64 {
        self.resolved_at
            .unwrap_or(now_secs)
            .saturating_sub(self.opened_at)
    }
}

/// The whole persisted ledger. Append-mostly: incidents resolve, they do
/// not disappear.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapLedger {
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub incidents: Vec<CapIncident>,
}

impl CapLedger {
    /// Missing or corrupt file yields the empty ledger; history is
    /// convenience, never worth failing a tick over.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) {
        match serde_json::to_string(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!(target: "server.cap_ledger", path = %path.display(), error = %e, "cap ledger write failed");
                }
            }
            Err(e) => {
                tracing::warn!(target: "server.cap_ledger", error = %e, "cap ledger serialize failed");
            }
        }
    }

    fn open_idx(&self, session: &str) -> Option<usize> {
        self.incidents
            .iter()
            .rposition(|i| i.session == session && i.resolved_at.is_none())
    }

    /// Seconds this session's OPEN incident has stood, when one exists.
    /// This is the running duration the pushes carry ("capped 14h32m").
    pub fn open_duration(&self, session: &str, now_secs: u64) -> Option<u64> {
        self.open_idx(session)
            .map(|i| self.incidents[i].duration_secs(now_secs))
    }

    /// Record a live cap observation: open an incident on the edge, extend
    /// the open one otherwise, appending to its state trail only when the
    /// classification state changed. Returns whether the ledger mutated
    /// (the caller's cue to persist).
    pub fn observe_cap(
        &mut self,
        session: &str,
        title: &str,
        profile: &str,
        kind: &str,
        state: &str,
        now_secs: u64,
    ) -> bool {
        match self.open_idx(session) {
            Some(i) => {
                let inc = &mut self.incidents[i];
                let mut changed = false;
                if inc.changes.last().map(|c| c.state.as_str()) != Some(state) {
                    inc.changes.push(StateChange {
                        at: now_secs,
                        state: state.to_string(),
                    });
                    changed = true;
                }
                if inc.kind != kind && kind != "unknown" {
                    inc.kind = kind.to_string();
                    changed = true;
                }
                if inc.profile != profile {
                    inc.profile = profile.to_string();
                    changed = true;
                }
                if changed {
                    inc.updated_at = now_secs;
                    self.updated = now_secs;
                }
                changed
            }
            None => {
                self.incidents.push(CapIncident {
                    session: session.to_string(),
                    title: title.to_string(),
                    profile: profile.to_string(),
                    kind: kind.to_string(),
                    opened_at: now_secs,
                    updated_at: now_secs,
                    resolved_at: None,
                    resolution: None,
                    changes: vec![StateChange {
                        at: now_secs,
                        state: state.to_string(),
                    }],
                    backfilled: false,
                });
                self.updated = now_secs;
                true
            }
        }
    }

    /// Close the session's open incident, naming why. Returns whether one
    /// was actually open (no-op resolves must not trigger a persist).
    pub fn resolve(&mut self, session: &str, resolution: &str, now_secs: u64) -> bool {
        let Some(i) = self.open_idx(session) else {
            return false;
        };
        let inc = &mut self.incidents[i];
        inc.resolved_at = Some(now_secs);
        inc.resolution = Some(resolution.to_string());
        inc.updated_at = now_secs;
        self.updated = now_secs;
        true
    }

    /// Filter for the query endpoint: any combination of session, profile
    /// and open-ness, newest first.
    pub fn query(
        &self,
        session: Option<&str>,
        profile: Option<&str>,
        open: Option<bool>,
    ) -> Vec<&CapIncident> {
        let mut rows: Vec<&CapIncident> = self
            .incidents
            .iter()
            .filter(|i| session.is_none_or(|s| i.session == s))
            .filter(|i| profile.is_none_or(|p| i.profile == p))
            .filter(|i| open.is_none_or(|o| i.resolved_at.is_none() == o))
            .collect();
        rows.sort_by_key(|i| std::cmp::Reverse(i.opened_at));
        rows
    }
}

/// Where the ledger lives: `AOE_CAP_INCIDENTS_FILE` when set, else
/// `cap-incidents.json` in the app dir.
pub fn ledger_path() -> Option<PathBuf> {
    match std::env::var("AOE_CAP_INCIDENTS_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("cap-incidents.json")),
            Err(e) => {
                tracing::warn!(target: "server.cap_ledger", error = %e, "no app dir; cap ledger unavailable");
                None
            }
        },
    }
}

/// Reconstruct historical incidents from the event-notifier ledger jsonl
/// (one `{ts, kind, session, result:{message}}` object per line), for the
/// first daemon start after this ledger ships: the fleet's cap history
/// predates the ledger and would otherwise be unqueryable. Consecutive cap
/// sightings of one session within [`BACKFILL_GAP_SECS`] merge into one
/// incident; every reconstructed incident is CLOSED at its last sighting
/// (`backfill-horizon`) and flagged `backfilled`, so live observation
/// always opens a fresh incident with a truthful onset.
pub const BACKFILL_GAP_SECS: u64 = 6 * 60 * 60;

pub fn backfill_from_notifier(raw: &str) -> Vec<CapIncident> {
    let mut incidents: Vec<CapIncident> = Vec::new();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("kind").and_then(|k| k.as_str()) != Some("cap") {
            continue;
        }
        let Some(session) = v.get("session").and_then(|s| s.as_str()) else {
            continue;
        };
        let ts = v.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0) as u64;
        if ts == 0 {
            continue;
        }
        let message = v
            .pointer("/result/message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let (title, profile) = parse_notifier_message(message);
        // The cap family rides in a bracketed token like `[fable-credit]`;
        // scan from the right so the `[aoe daemon push]` prefix never wins.
        let kind = message
            .rsplit('[')
            .filter_map(|seg| seg.split_once(']').map(|(k, _)| k))
            .find(|k| !k.is_empty() && !k.contains(' '))
            .map(|k| k.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        match incidents.iter_mut().rev().find(|i| i.session == session) {
            Some(last) if ts.saturating_sub(last.updated_at) <= BACKFILL_GAP_SECS => {
                last.updated_at = ts;
                last.resolved_at = Some(ts);
                if last.kind == "unknown" && kind != "unknown" {
                    last.kind = kind;
                }
            }
            _ => incidents.push(CapIncident {
                session: session.to_string(),
                title: title.unwrap_or_else(|| session.to_string()),
                profile: profile.unwrap_or_else(|| "unknown".to_string()),
                kind,
                opened_at: ts,
                updated_at: ts,
                resolved_at: Some(ts),
                resolution: Some("backfill-horizon".to_string()),
                changes: Vec::new(),
                backfilled: true,
            }),
        }
    }
    incidents
}

/// Pull `(title, profile)` out of a notifier push message, which reads
/// `... LIMIT-HIT pane-cap: <title> (<sid8>) on <profile> -- ...`.
fn parse_notifier_message(message: &str) -> (Option<String>, Option<String>) {
    let after_colon = message.split(": ").nth(1).unwrap_or("");
    let title = after_colon
        .rsplit_once(" (")
        .map(|(t, _)| t.to_string())
        .filter(|t| !t.is_empty());
    let profile = message
        .split_once(") on ")
        .map(|(_, rest)| {
            rest.split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches("--")
                .to_string()
        })
        .filter(|p| !p.is_empty());
    (title, profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn incident_lifecycle_open_update_resolve() {
        let mut ledger = CapLedger::default();
        // Edge opens with the initial state in the trail.
        assert!(ledger.observe_cap(
            "s1",
            "gna-finance",
            "forit-main",
            "weekly",
            "LIVE-CAP-weekly",
            NOW
        ));
        assert_eq!(ledger.open_duration("s1", NOW + 7200), Some(7200));
        // Same state again: no mutation, no persist churn.
        assert!(!ledger.observe_cap(
            "s1",
            "gna-finance",
            "forit-main",
            "weekly",
            "LIVE-CAP-weekly",
            NOW + 60
        ));
        // State change appends to the trail; unknown kind never downgrades.
        assert!(ledger.observe_cap(
            "s1",
            "gna-finance",
            "forit-main",
            "unknown",
            "PARKED",
            NOW + 120
        ));
        let inc = &ledger.incidents[0];
        assert_eq!(inc.kind, "weekly");
        assert_eq!(
            inc.changes
                .iter()
                .map(|c| c.state.as_str())
                .collect::<Vec<_>>(),
            vec!["LIVE-CAP-weekly", "PARKED"]
        );
        // Resolution closes it and stamps why; a second resolve is a no-op.
        assert!(ledger.resolve("s1", "serving-turn", NOW + 3600));
        assert!(!ledger.resolve("s1", "serving-turn", NOW + 3601));
        assert_eq!(ledger.open_duration("s1", NOW + 4000), None);
        let inc = &ledger.incidents[0];
        assert_eq!(inc.resolution.as_deref(), Some("serving-turn"));
        assert_eq!(inc.duration_secs(NOW + 9999), 3600);
        // A fresh cap after resolution opens a NEW incident with a new onset.
        assert!(ledger.observe_cap(
            "s1",
            "gna-finance",
            "forit-main",
            "session",
            "LIVE-CAP-session",
            NOW + 5000
        ));
        assert_eq!(ledger.incidents.len(), 2);
        assert_eq!(ledger.open_duration("s1", NOW + 5100), Some(100));
    }

    #[test]
    fn query_filters_and_roundtrip() {
        let mut ledger = CapLedger::default();
        ledger.observe_cap("s1", "a", "forit-main", "weekly", "LIVE-CAP-weekly", NOW);
        ledger.observe_cap(
            "s2",
            "b",
            "gna-main",
            "session",
            "LIVE-CAP-session",
            NOW + 10,
        );
        ledger.resolve("s2", "rebind-restart", NOW + 20);
        assert_eq!(ledger.query(Some("s1"), None, None).len(), 1);
        assert_eq!(ledger.query(None, Some("gna-main"), None).len(), 1);
        assert_eq!(ledger.query(None, None, Some(true)).len(), 1);
        assert_eq!(ledger.query(None, None, Some(false))[0].session, "s2");
        // Newest first.
        assert_eq!(ledger.query(None, None, None)[0].session, "s2");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cap-incidents.json");
        ledger.save(&path);
        let loaded = CapLedger::load(&path);
        assert_eq!(loaded.incidents.len(), 2);
        assert_eq!(loaded.open_duration("s1", NOW + 100), Some(100));
        assert!(CapLedger::load(&dir.path().join("missing.json"))
            .incidents
            .is_empty());
    }

    #[test]
    fn backfill_reconstructs_closed_flagged_incidents() {
        let raw = concat!(
            r#"{"ts": 1786054646.6, "seq": 1, "kind": "cap", "session": "c3f857e1da6041cb", "result": {"message": "[aoe daemon push] LIMIT-HIT pane-cap: wo1278-cap-probe (c3f857e1) on wo1278-probe -- capped on a non-pool profile -- 2026-08-06"}}"#,
            "\n",
            // 15 min later, same session: merges into the same incident.
            r#"{"ts": 1786055546.9, "seq": 2, "kind": "cap", "session": "c3f857e1da6041cb", "result": {"sent": false}}"#,
            "\n",
            // Non-cap kinds and garbage lines are skipped.
            r#"{"ts": 1786055600.0, "kind": "auth", "session": "c3f857e1da6041cb"}"#,
            "\n",
            "not json\n",
            // 7h later: past the gap, a separate incident.
            r#"{"ts": 1786080748.0, "seq": 3, "kind": "cap", "session": "c3f857e1da6041cb", "result": {"message": "[aoe daemon push] LIMIT-HIT pane-cap: wo1278-cap-probe (c3f857e1) on wo1278-probe -- capped [fable-credit] -- 2026-08-07"}}"#,
            "\n",
        );
        let incidents = backfill_from_notifier(raw);
        assert_eq!(incidents.len(), 2);
        assert!(incidents.iter().all(|i| i.backfilled));
        assert!(incidents.iter().all(|i| i.resolved_at.is_some()));
        assert_eq!(incidents[0].opened_at, 1786054646);
        assert_eq!(incidents[0].updated_at, 1786055546);
        assert_eq!(incidents[0].title, "wo1278-cap-probe");
        assert_eq!(incidents[0].profile, "wo1278-probe");
        assert_eq!(incidents[1].kind, "fable-credit");
        assert_eq!(incidents[1].resolution.as_deref(), Some("backfill-horizon"));
    }
}
