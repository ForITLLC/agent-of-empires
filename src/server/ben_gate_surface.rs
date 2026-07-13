//! Shared cross-surfacer Ben-gate surface-dedup ledger (Rust / pane-watchdog lane).
//!
//! MISTAKE-f9e3f8d8 MIT-1. Contract (single source of truth for ALL lanes):
//! `~/GitProjects/per-dev/docs/fleet-ben-gate-surface-dedup-contract.md`.
//!
//! One pending Ben-gate reaches Ben through four independent notifiers (worker
//! Stop-hook, worker->Commander report, THIS pane-watchdog wake, fleet-tick page).
//! Each self-dampens its own re-fires but shares no state, so the same
//! approve-this-send decision was put in front of Ben more than once. This module
//! is the on-disk claim ledger the watchdog consults before waking the Commander:
//! the FIRST surfacer to claim an open gate surfaces it; a DIFFERENT surfacer's
//! live claim suppresses this one until the gate's state changes (consumed/minted/
//! expired), which the py/token lane clears.
//!
//! Byte-compatible with the per-hooks `ben_gate_surface_ledger.py` and per-dev
//! `fleet_ben_gate_surface.py` purely through the on-disk format defined in the
//! contract (same dir, same sha256 hashing, same JSON claim record) - the lanes
//! never import each other.
//!
//! FAIL-OPEN: any error -> "not yet surfaced" (return true / no-op). The watchdog
//! must never be wedged by this ledger - a rare double-wake is acceptable, a
//! silently-dropped Ben-gate is not.

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};

/// The 12-hex ben-token gate id, but ONLY when carried by the `GATE-ID:` or
/// `BEN-GATE ` label (a bare "gate <hex>" in prose must NOT key as primary).
/// Label case-insensitive; the id is normalized to lowercase by the caller.
fn gate_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:GATE-ID:\s*|BEN-GATE\s+)([0-9a-fA-F]{12})").unwrap())
}

/// Extract the labelled 12-hex gate id (lowercased) from arbitrary text, or None.
pub(crate) fn extract_gate_id(text: &str) -> Option<String> {
    gate_id_re()
        .captures(text)
        .map(|c| c[1].to_ascii_lowercase())
}

/// Canonical cross-lane gate key. Mirrors `surface_key` in the py lanes:
/// primary `gate:<12hex>` when a GATE-ID/BEN-GATE label is present, else the
/// `sess:<id>|fp:<action_gate_fingerprint>` fallback.
///
/// The pane-watchdog scan path does NOT call this: it already has the winning
/// rule's `classify_fp` fingerprint (the `action_gate_fingerprint` of the
/// matched *candidate* line, tighter than re-fingerprinting the whole pane
/// tail), so it composes the fallback inline from that. This function is the
/// byte-for-byte parity anchor the canonical-vector test checks against the py
/// lanes; keep it and its test even though non-test code inlines the same shape.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn surface_key(session_id: &str, action_text: &str) -> String {
    match extract_gate_id(action_text) {
        Some(g) => format!("gate:{g}"),
        None => format!(
            "sess:{session_id}|fp:{}",
            crate::pane_rules::action_gate_fingerprint(action_text)
        ),
    }
}

/// `<BEN_GATE_DIR or ~/.config/fleet/ben-gates>/surfaced/`. `BEN_GATE_DIR`
/// overrides the PARENT (mirrors the py lanes / `ben_gate_tokens.py`).
fn surfaced_dir() -> Option<PathBuf> {
    let parent = match std::env::var_os("BEN_GATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()?
            .join(".config")
            .join("fleet")
            .join("ben-gates"),
    };
    Some(parent.join("surfaced"))
}

fn claim_path(dir: &std::path::Path, key: &str) -> PathBuf {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{hex}.json"))
}

/// True == you own the surface (notify); false == a DIFFERENT surfacer's live
/// claim already surfaced this gate (suppress). A live claim held by the SAME
/// `surfacer` is refreshed and returns true - each surfacer keeps its own
/// re-notify cadence (the watchdog's WO#139 `last_action_fp` dampener); MIT-1
/// only stops the OTHER notifiers piling on. `now` is unix seconds (matches the
/// py lanes' `time.time()`). Fail-OPEN: any error -> true.
pub(crate) fn claim_surface(key: &str, surfacer: &str, ttl_secs: u64, now: f64) -> bool {
    let Some(dir) = surfaced_dir() else {
        return true; // no home dir -> cannot dedup; never wedge
    };
    claim_in(&dir, key, surfacer, ttl_secs, now)
}

fn claim_in(dir: &std::path::Path, key: &str, surfacer: &str, ttl_secs: u64, now: f64) -> bool {
    let path = claim_path(dir, key);
    // A live claim by a DIFFERENT surfacer suppresses; same surfacer refreshes.
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(rec) = serde_json::from_str::<serde_json::Value>(&raw) {
            let expires = rec.get("expires").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let owner = rec.get("surfacer").and_then(|v| v.as_str()).unwrap_or("");
            if expires > now && owner != surfacer {
                return false;
            }
        }
    }
    // (Re)claim: atomic write via temp + rename. Any failure -> fail-open true.
    if std::fs::create_dir_all(dir).is_err() {
        return true;
    }
    let rec = serde_json::json!({
        "key": key,
        "surfacer": surfacer,
        "ts": now,
        "expires": now + ttl_secs as f64,
    });
    let tmp = dir.join(format!(".claim-{}.tmp", std::process::id()));
    let write_ok = serde_json::to_vec(&rec)
        .ok()
        .and_then(|bytes| std::fs::write(&tmp, bytes).ok())
        .and_then(|_| std::fs::rename(&tmp, &path).ok())
        .is_some();
    let _ = std::fs::remove_file(&tmp);
    // Whether or not the persist succeeded, we own the surface this tick.
    let _ = write_ok;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical shared vectors - MUST match per-hooks + per-dev py byte-for-byte.
    // Mirror of the fenced table in the contract doc.
    #[test]
    fn surface_key_matches_canonical_vectors() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "s1",
                "BLOCKED — OUTBOUND COMMS GATE — approve to send\nGATE-ID: 56511f20a4dc\nTool: productivity_Send_draft",
                "gate:56511f20a4dc",
            ),
            (
                "s1",
                "BEN-GATE 56511f20a4dc: productivity_Send_draft → caroline@x.com",
                "gate:56511f20a4dc",
            ),
            (
                "s2",
                "ACTION REQUIRED: approve gate 8b68c414c23e now",
                "sess:s2|fp:action required: approve gate 8b68c414c23e now",
            ),
            (
                "s3",
                "ACTION REQUIRED (Ben): certify probe #24 and WO #18",
                "sess:s3|fp:action-gate:#18,#24",
            ),
            (
                "s3",
                "ACTION REQUIRED (Ben): certify probe #24 then #18 again #18",
                "sess:s3|fp:action-gate:#18,#24",
            ),
            (
                "s4",
                "ACTION REQUIRED: do the neko entra sign-in and confirm",
                "sess:s4|fp:action required: do the neko entra sign-in and confirm",
            ),
        ];
        for (sid, text, want) in cases {
            assert_eq!(&surface_key(sid, text), want, "sid={sid} text={text:?}");
        }
    }

    #[test]
    fn uppercase_label_and_hex_normalize_to_lowercase_gate_key() {
        assert_eq!(
            surface_key("sX", "ben-gate 56511F20A4DC: x"),
            "gate:56511f20a4dc"
        );
    }

    #[test]
    fn bare_gate_hex_without_label_is_not_primary() {
        assert!(surface_key("sX", "please approve gate 56511f20a4dc").starts_with("sess:sX|fp:"));
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("aoe-bengate-surface-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    // Mirrors the canonical on-disk claim protocol in the per-dev py lane's
    // `test_fleet_ben_gate_surface_dedup.py` (same key, base T, and surfacer
    // sequence) so all three lanes exercise identical claim semantics. The Rust
    // pane-watchdog lane does not implement `clear_surface` (mint/consume clears
    // are the token/py lane's job), so the py test's clear+re-claim lines have
    // no Rust counterpart; every surviving line matches byte-for-byte.
    #[test]
    fn claim_protocol_cross_surfacer_and_same_surfacer_refresh() {
        let dir = tmpdir("claim");
        let k = "gate:56511f20a4dc";
        let t = 1_000_000.0_f64;

        assert!(
            claim_in(&dir, k, "fleet-tick", 3600, t),
            "first claim owns surface"
        );
        assert!(
            !claim_in(&dir, k, "pane-watchdog", 3600, t + 10.0),
            "second claim (diff surfacer) suppressed"
        );
        assert!(
            claim_in(&dir, k, "fleet-tick", 3600, t + 20.0),
            "SAME surfacer re-claims -> true (own re-notify cadence, refresh)"
        );
        assert!(
            !claim_in(&dir, k, "worker-stop", 3600, t + 3599.0),
            "still suppressed for a DIFFERENT surfacer just before expiry"
        );
        assert!(
            claim_in(&dir, k, "fleet-tick", 3600, t + 3601.0),
            "owner re-pages on its own cadence -> true"
        );

        // A genuinely EXPIRED claim re-surfaces once even for a DIFFERENT
        // surfacer (anti-rot). Use a fresh key so the sequence above is
        // undisturbed: owner claims, then a different surfacer past expiry wins.
        let k3 = "gate:deadbeefdead0";
        assert!(
            claim_in(&dir, k3, "worker-stop", 3600, t),
            "k3 first claim owns"
        );
        assert!(
            claim_in(&dir, k3, "fleet-tick", 3600, t + 3601.0),
            "expired claim re-surfaces once for a different surfacer"
        );

        // Two different keys are independent. `k` is still owned by fleet-tick
        // (expires t+3601+3600), so a pane-watchdog claim on it stays suppressed
        // while a claim on the distinct k2 succeeds.
        let k2 = "sess:s3|fp:action-gate:#18,#24";
        assert!(
            claim_in(&dir, k2, "fleet-tick", 3600, t + 3603.0),
            "distinct key claims independently"
        );
        assert!(
            !claim_in(&dir, k, "pane-watchdog", 3600, t + 3604.0),
            "k still owned by fleet-tick; independent from k2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fail_open_when_dir_unwritable() {
        // A path under a file (not a dir) cannot be created -> fail-open true.
        let bogus = std::env::temp_dir().join("aoe-bengate-nonexistent-parent-file");
        let _ = std::fs::write(&bogus, b"x");
        let under = bogus.join("surfaced");
        assert!(claim_in(
            &under,
            "gate:deadbeefdead",
            "pane-watchdog",
            3600,
            1.0
        ));
        let _ = std::fs::remove_file(&bogus);
    }
}
