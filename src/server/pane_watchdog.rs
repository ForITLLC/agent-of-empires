//! Continuous pane-content watchdog (INC-2026-07-06).
//!
//! The `/api/sessions` status pipeline reports what the status hooks and
//! spinner heuristics can see, but several operator-blocking states render
//! ONLY as pane text: the usage-cap options modal, a worker's `ACTION
//! REQUIRED` gate line, and device-code login prompts. All three reported
//! `Idle` while ~20 capped sessions and a gated worker sat undetected for
//! hours. This module is the daemon-side backstop: a supervised interval
//! task captures every registered session's pane tail, classifies it with
//! the configurable rule engine in [`crate::pane_rules`] (built-in battery
//! plus `[[watchdog.rules]]` config entries), auto-relocates capped pool
//! sessions down the account draw order, and escalates everything else to
//! the AoE-Commander through the urgent-wake channel. The same scan also
//! checks every pane against its session's charter
//! ([`super::charter_drift`]) and escalates out-of-scope work through the
//! same channel.
//!
//! Everything here is fail-open: a capture error, a failed move, or an
//! unwritable state file logs and skips; the daemon never crashes or stalls
//! on watchdog work.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::file_watch::FileWatchService;
use crate::pane_rules::{self, CompiledRule};

use super::ben_gate_surface;
use super::capacity;
use super::charter_drift::{self, DriftHit};

/// What a pane tail says the session is blocked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneSignal {
    /// A real usage/session cap or credit-out banner (NOT the Fable promo
    /// blurb, NOT a transient server-side 429).
    Capped,
    /// A device-code / browser-auth login prompt at the bottom of the pane.
    DeviceCode,
    /// A live 529 server-overload error banner at the pane edge.
    Overloaded,
    /// A worker's `ACTION REQUIRED` gate line.
    ActionRequired,
    /// The session's own account is logged out or its credential is rejected.
    /// Distinct from [`PaneSignal::DeviceCode`], which is a sign-in prompt the
    /// session is deliberately driving; this is a credential that stopped
    /// working underneath it.
    AuthLoss,
}

/// Map a rule's `kind` string (config-facing) to the watchdog's action
/// signal. Unknown kinds are rejected at spawn time.
fn kind_to_signal(kind: &str) -> Option<PaneSignal> {
    match kind {
        "cap" => Some(PaneSignal::Capped),
        "auth" => Some(PaneSignal::DeviceCode),
        "overload" => Some(PaneSignal::Overloaded),
        "action" => Some(PaneSignal::ActionRequired),
        "authloss" => Some(PaneSignal::AuthLoss),
        _ => None,
    }
}

/// The built-in battery, compiled once. Classification for callers outside
/// the watchdog loop goes through this set, never through user config.
/// Test-only since the loop itself compiles rules from config; kept as the
/// harness for exercising the default battery.
#[cfg(test)]
static DEFAULT_COMPILED: LazyLock<Vec<CompiledRule>> =
    LazyLock::new(|| pane_rules::compile(&pane_rules::default_rules()));

/// Classify a raw `capture-pane` tail against the built-in rules. Returns
/// the highest-priority signal (Capped > DeviceCode > Overloaded >
/// ActionRequired) or None for a healthy pane.
#[cfg(test)]
fn classify_pane_tail(raw: &str) -> Option<PaneSignal> {
    pane_rules::classify(raw, &DEFAULT_COMPILED).and_then(|r| kind_to_signal(&r.kind))
}

/// The Fable model-drift battery, compiled once. Built-in only (not user
/// configurable), like the default battery above. WO d6bcae49.
static DEFAULT_FABLE_COMPILED: LazyLock<Vec<CompiledRule>> =
    LazyLock::new(|| pane_rules::compile(&pane_rules::fable_drift_rules()));

/// Classify a raw pane tail against the Fable-drift rules. Returns the winning
/// rule name and its churn-stable content fingerprint, or None for a clean
/// pane. WO d6bcae49.
pub(crate) fn classify_fable_drift(raw: &str) -> Option<(String, String)> {
    pane_rules::classify_fp(raw, &DEFAULT_FABLE_COMPILED).map(|(r, fp)| (r.name.clone(), fp))
}

/// Fable model-drift for one session, GATED on the session being Fable-pinned.
/// This gate is the whole safety property: a NON-Fable session running a
/// Sonnet/Opus subagent is normal and returns None; only a Fable-pinned session
/// hitting a silent Fable cap, announcing a downgrade, or dispatching a
/// non-Fable subagent yields a hit. Returns (rule, fingerprint). WO d6bcae49
/// (subagent-vector + silent-Fable-limit). The watchdog PAGES the Commander on
/// a hit and never auto-swaps the model.
pub(crate) fn fable_scan_hit(extra_args: &str, content: &str) -> Option<(String, String)> {
    if !pane_rules::is_fable_pinned(extra_args) {
        return None;
    }
    classify_fable_drift(content)
}

/// The hard account draw order for cap relocation. Sessions on profiles
/// outside this pool are never auto-moved (escalate only).
pub(crate) const DRAW_ORDER: [&str; 7] = [
    "forit-main",
    "forit-backup",
    "gna-main",
    "xce-main",
    "RAS-Main",
    "RAS-Work",
    "bp-main",
];

/// Pick the relocation target for a capped session: the first profile in
/// [`DRAW_ORDER`] that is not the session's current profile and holds a
/// FRESH POSITIVE headroom claim in the shared capacity state. `None` when
/// the session is not on a pool profile (gated / personal accounts are never
/// touched) or when no other pool profile has verified headroom (park and
/// escalate instead of moving).
///
/// WO#414 thrash fix: the old selector treated "no cap observed" as a
/// target, but absence of an observation is not headroom, and it bounced
/// sessions capped-to-capped across accounts that were all out of credit.
/// Unknown now parks; only an empirically probed claim (PATCH /api/capacity)
/// re-opens relocation.
pub(crate) fn next_verified_headroom(
    current: &str,
    state: &capacity::CapacityState,
    now_secs: u64,
) -> Option<String> {
    let pos = DRAW_ORDER.iter().position(|p| *p == current)?;
    (1..DRAW_ORDER.len())
        .map(|i| DRAW_ORDER[(pos + i) % DRAW_ORDER.len()])
        .find(|cand| state.verified_headroom(cand, now_secs))
        .map(str::to_string)
}

/// Whether EVERY pool profile holds a FRESH probed NEGATIVE headroom claim.
/// Only then is "all accounts capped" empirically proven. An absent entry, a
/// positive claim, or a stale negative all leave the pool state unknown: the
/// parked escalation must then demand a probe, never assert a credits outage
/// (the WO#449 phantom top-up pages came from equating "no verified target"
/// with "all accounts out of credits").
pub(crate) fn all_pool_probed_capped(state: &capacity::CapacityState, now_secs: u64) -> bool {
    DRAW_ORDER.iter().all(|p| {
        state.profiles.get(*p).is_some_and(|e| {
            !e.headroom && now_secs.saturating_sub(e.updated) <= capacity::HEADROOM_TTL_SECS
        })
    })
}

/// The (kind, reason) for a parked capped session's Commander wake, split on
/// whether the all-capped state is PROBED (every pool profile fresh-negative)
/// or merely unknown. Only the probed shape may talk about credits; the
/// unknown shape demands an empirical probe and explicitly forbids relaying a
/// credits/money gate to Ben off this wake alone. WO#449 directive 4.
pub(crate) fn parked_wake(
    all_probed: bool,
    kind: &str,
    title: &str,
    id: &str,
    profile: &str,
) -> (&'static str, String) {
    if all_probed {
        (
            "capped-all-accounts",
            format!(
                "capped [{kind}] session '{title}' ({id}) on '{profile}' PARKED: ALL {} pool accounts hold fresh probed NEGATIVE headroom claims. A credits escalation is warranted",
                DRAW_ORDER.len()
            ),
        )
    } else {
        (
            "capped-parked",
            format!(
                "capped [{kind}] session '{title}' ({id}) on '{profile}' PARKED: no pool profile holds a fresh verified-headroom claim. Probe accounts empirically and PATCH /api/capacity to re-open relocation; do NOT surface a credits/money gate to Ben from this alone"
            ),
        )
    }
}

/// Commander page for a pool relocation of a capped session. An auto-move
/// (or a failed one) must never be silent: capacity-capped sessions fail
/// /compact invisibly, so the Commander verifies the landing (WO #362).
/// Names the cap family so the Commander knows which clock fired (WO#414).
pub(crate) fn capped_move_reason(
    title: &str,
    id: &str,
    from: &str,
    target: &str,
    kind: &str,
    moved: bool,
) -> String {
    if moved {
        format!(
            "capped [{kind}] session '{title}' ({id}) on '{from}' auto-moved to '{target}'. Verify it resumed and is serving"
        )
    } else {
        format!(
            "capped [{kind}] session '{title}' ({id}) on '{from}' auto-move to '{target}' FAILED; needs a manual profile move"
        )
    }
}

/// How long an observed cap on a profile is trusted before it is assumed to
/// have reset, and also the floor between repeated ALL-CAPPED escalations.
const CAP_TTL: Duration = Duration::from_secs(60 * 60);

/// Floor between watchdog actions (move or wake) on the same session, so a
/// pane that stays blocked does not generate an action every tick.
const ACTION_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Floor between repeated Commander pages for a session that stays capped on
/// the SAME non-pool profile. A non-pool cap is deliberately never auto-moved
/// (see [`Watchdog::handle_capped`]), so the page is a *notification*, not an
/// act-now prompt — re-notifying every [`ACTION_COOLDOWN`] (~30 min) floods the
/// Commander over a standing, unchanged condition (the WO#605 re-page bug). One
/// page fires; a session that moves to a different non-pool profile, or that
/// stays put past this window, pages again.
const NON_POOL_PAGE_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);

/// Floor between charter-drift escalations for the same (session, observed)
/// pair. Drift is advisory, not operator-blocking, so it re-fires slowly.
const DRIFT_COOLDOWN: Duration = Duration::from_secs(2 * 60 * 60);

/// How long a session's ACTION REQUIRED fingerprint survives once its gate is
/// no longer observed, before the content dampener forgets it. Longer than a
/// handful of scan intervals so a transient one-tick capture miss does not
/// reset the dampener (which would spuriously re-wake), but short enough that
/// a genuinely cleared-then-reopened gate is treated as new. WO #139.
const ACTION_FP_TTL: Duration = Duration::from_secs(60 * 60);

/// Last escalated ACTION REQUIRED gate fingerprint for a session, plus when it
/// was last observed (for TTL pruning). The fingerprint is content-derived
/// (see [`pane_rules::classify_fp`]); an unchanged fingerprint means the gate
/// is the same one already surfaced to the Commander, so it must not re-wake.
#[derive(Clone)]
struct ActionFp {
    fp: String,
    seen: Instant,
}

/// Whether an observed ACTION REQUIRED gate warrants a *fresh* Commander wake:
/// only when its content fingerprint differs from the last one escalated for
/// that session. A first sighting (`None`) wakes; an unchanged, already-
/// surfaced gate is suppressed; a new/changed gate wakes immediately. This
/// replaces the old purely-temporal `ACTION_COOLDOWN` re-fire for the action
/// path, which re-paged every standing gate every 30 min (and on every daemon
/// restart, since the cooldown map was in-memory only). WO #139.
fn action_should_wake(last_fp: Option<&str>, current_fp: &str) -> bool {
    last_fp != Some(current_fp)
}

/// Last observed cap-banner fingerprint for a session, with when that exact
/// banner content was FIRST seen (wall clock, for comparison against a
/// capacity grant's `updated`) and last observed (monotonic, for TTL
/// pruning). Backs the WO#445 replayed-banner gate: a banner whose content
/// predates the Commander's verified grant is stale scrollback, not a fresh
/// cap observation, so it must not eat the grant.
#[derive(Clone)]
struct CapFp {
    fp: String,
    first_seen_secs: u64,
    seen: Instant,
}

/// Last Commander page emitted for a session capped on a NON-POOL profile: the
/// profile it was capped on and the wall-clock second it was paged. Backs the
/// WO#605 non-pool re-page dampener, persisted across restarts so a daemon
/// bounce does not re-flood every parked non-pool cap.
#[derive(Clone)]
struct NonPoolPage {
    profile: String,
    paged_at_secs: u64,
}

/// Whether a live cap on a NON-POOL profile warrants a *fresh* Commander page.
/// `last` is the `(profile, paged_at_secs)` of the previous page for this
/// session, if any. Pages on: a first sighting (`None`); a move to a DIFFERENT
/// non-pool profile (a genuine state change — e.g. a session shuffled off
/// `codex`); or once [`NON_POOL_PAGE_COOLDOWN`] has elapsed since the last page.
/// A standing, unchanged cap on the same profile within the window is
/// suppressed — the fix for the WO#605 re-page flood, where the non-pool branch
/// woke the Commander on every [`ACTION_COOLDOWN`] re-entry. Pure, so the
/// decision is unit-tested without a Watchdog fixture.
fn non_pool_should_page(last: Option<(&str, u64)>, current_profile: &str, now_secs: u64) -> bool {
    match last {
        None => true,
        Some((prof, _)) if prof != current_profile => true,
        Some((_, at)) => now_secs.saturating_sub(at) >= NON_POOL_PAGE_COOLDOWN.as_secs(),
    }
}

/// Parse the persisted `{"pages": {id: {"profile": …, "paged_at": …}}}`
/// document into the in-memory non-pool-page map. Pure, like [`parse_cap_fp`];
/// malformed input or a non-string profile yields an empty / skipped entry.
/// WO#605.
fn parse_non_pool_page(raw: &str) -> HashMap<String, NonPoolPage> {
    let mut map = HashMap::new();
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else {
        return map;
    };
    if let Some(pages) = val.get("pages").and_then(|p| p.as_object()) {
        for (id, entry) in pages {
            let Some(profile) = entry.get("profile").and_then(|p| p.as_str()) else {
                continue;
            };
            let paged_at_secs = entry.get("paged_at").and_then(|a| a.as_u64()).unwrap_or(0);
            map.insert(
                id.clone(),
                NonPoolPage {
                    profile: profile.to_string(),
                    paged_at_secs,
                },
            );
        }
    }
    map
}

/// True when the pane's LIVE EDGE (the last few non-empty lines) shows the
/// running footer ("esc to interrupt") — the account is empirically serving
/// a request right now, whatever stale banners sit above in scrollback.
/// The window is deliberately small: the footer only renders at the very
/// bottom while working (spinner + input box + status ≈ 5 lines), so a
/// replayed footer buried under real output stays outside it.
fn pane_is_actively_working(content: &str) -> bool {
    content
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(8)
        .any(|l| l.to_ascii_lowercase().contains("esc to interrupt"))
}

/// Unix-secs first-seen for a session's current cap-banner fingerprint:
/// sticky while the banner content is unchanged (`prev` fp matches), reset
/// to `now_secs` when the banner changes or was never seen.
fn cap_first_seen(prev: Option<(&str, u64)>, current_fp: &str, now_secs: u64) -> u64 {
    match prev {
        Some((fp, first)) if fp == current_fp => first,
        _ => now_secs,
    }
}

/// Whether an observed cap banner must NOT revoke the profile's headroom
/// (returns the suppression reason) — the WO#445 false-revoke gate:
/// (a) `profile-serving`: another pane on the profile (or this one) is
///     empirically serving right now, so the account is not capped;
/// (b) `pre-grant-banner`: the profile holds a verified grant and this exact
///     banner content was first seen at-or-before the grant's `updated` —
///     the Commander granted with the banner already on screen, so it is
///     stale scrollback, not a new observation.
/// `None` = observation beats claim as before (WO#414): revoke.
fn cap_revoke_suppressed(
    profile_serving: bool,
    verified_headroom: bool,
    grant_updated_secs: u64,
    banner_first_seen_secs: u64,
) -> Option<&'static str> {
    if profile_serving {
        return Some("profile-serving");
    }
    if verified_headroom && banner_first_seen_secs <= grant_updated_secs {
        return Some("pre-grant-banner");
    }
    None
}

/// Parse the persisted `{"caps": {id: {"fp": …, "first_seen": …}}}` document
/// into the in-memory map, stamping every entry's `seen` with `now`. Pure,
/// like [`parse_action_fp`]; malformed input yields an empty map. WO#445.
fn parse_cap_fp(raw: &str, now: Instant) -> HashMap<String, CapFp> {
    let mut map = HashMap::new();
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else {
        return map;
    };
    if let Some(caps) = val.get("caps").and_then(|c| c.as_object()) {
        for (id, entry) in caps {
            let Some(fp) = entry.get("fp").and_then(|f| f.as_str()) else {
                continue;
            };
            let first_seen_secs = entry
                .get("first_seen")
                .and_then(|f| f.as_u64())
                .unwrap_or(0);
            map.insert(
                id.clone(),
                CapFp {
                    fp: fp.to_string(),
                    first_seen_secs,
                    seen: now,
                },
            );
        }
    }
    map
}

/// What the watchdog DID about a live cap this tick (WO#450 classification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapAction {
    /// Relocated (or tried to relocate) the session to a verified-headroom
    /// pool profile. `ok` false means the move command failed.
    Moved { target: String, ok: bool },
    /// Still capped but inside the per-session action cooldown: no new
    /// action, the state is logged so the hold is auditable.
    Cooldown,
    /// Capped on a non-pool profile: never auto-moved. `paged` is true when
    /// this tick actually woke the Commander, false when the WO#605 re-page
    /// dampener suppressed a standing, unchanged cap within its cooldown.
    NonPool { paged: bool },
}

/// Per-session per-tick classification: what state the pane is in, what the
/// watchdog decided, and why. EVERY live session gets exactly one disposition
/// per tick, healthy or not, so silent misses are visible in the log (WO#450).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Healthy pane: no signal, no drift, nothing suppressed.
    Serving,
    /// A LIVE cap banner (survived the WO#445/#449 replayed-banner gate).
    LiveCap {
        kind: &'static str,
        action: CapAction,
    },
    /// A cap banner proven to be replayed scrollback or contradicted by live
    /// serving evidence: ignored, headroom kept, never a page.
    ReplayedBanner { why: &'static str },
    /// A Fable-pinned session showing off-Fable drift (silent limit,
    /// downgrade announcement, or non-Fable subagent). `why` names the
    /// specific no-page path when `paged` is false, so the classification log
    /// distinguishes a voided page from a dampened one.
    FableDrift {
        rule: String,
        paged: bool,
        why: &'static str,
    },
    /// Capped with no verified-headroom relocation target: parked.
    Parked {
        kind: &'static str,
        all_probed: bool,
    },
    /// An ACTION REQUIRED gate line. `suppressed` names why the page was
    /// withheld (commander-exempt / pane-actively-working); `None` means the
    /// gate is genuine and `paged` says whether this tick woke the Commander.
    ActionGate {
        paged: bool,
        suppressed: Option<&'static str>,
    },
    /// A device-code sign-in prompt at the pane edge.
    DeviceCode { paged: bool },
    /// A transient 529 overload banner: red row only, self-clearing.
    Overloaded,
    /// The account behind this session is logged out or rejecting its
    /// credential. Worse than a cap: no reset time, no countdown, and a pane
    /// that reads as merely idle while producing nothing.
    AuthLoss { paged: bool },
}

impl Disposition {
    /// The classification STATE column. A suppressed action gate reads
    /// SERVING: the matched text is not a live bottom-of-pane gate.
    pub(crate) fn state(&self) -> String {
        match self {
            Disposition::Serving => "SERVING".into(),
            Disposition::LiveCap { kind, .. } => format!("LIVE-CAP-{kind}"),
            Disposition::ReplayedBanner { .. } => "REPLAYED-banner-ignored".into(),
            Disposition::FableDrift { .. } => "SUBAGENT-model-drift".into(),
            Disposition::Parked { .. } => "PARKED".into(),
            Disposition::ActionGate { suppressed, .. } => match suppressed {
                Some(_) => "SERVING".into(),
                None => "ACTION-REQUIRED".into(),
            },
            Disposition::DeviceCode { .. } => "DEVICE-CODE".into(),
            Disposition::Overloaded => "OVERLOADED".into(),
            Disposition::AuthLoss { .. } => "AUTH-LOSS".into(),
        }
    }

    /// The classification DECISION column: none, page-commander, or park.
    pub(crate) fn decision(&self) -> &'static str {
        match self {
            Disposition::Serving | Disposition::ReplayedBanner { .. } | Disposition::Overloaded => {
                "none"
            }
            Disposition::LiveCap { action, .. } => match action {
                CapAction::Moved { .. } => "page-commander",
                CapAction::NonPool { paged } => {
                    if *paged {
                        "page-commander"
                    } else {
                        "none"
                    }
                }
                CapAction::Cooldown => "none",
            },
            Disposition::FableDrift { paged, .. }
            | Disposition::ActionGate {
                paged,
                suppressed: None,
            }
            | Disposition::AuthLoss { paged }
            | Disposition::DeviceCode { paged } => {
                if *paged {
                    "page-commander"
                } else {
                    "none"
                }
            }
            Disposition::ActionGate {
                suppressed: Some(_),
                ..
            } => "none",
            Disposition::Parked { .. } => "park",
        }
    }

    /// The classification REASON column: one human-readable sentence.
    pub(crate) fn reason(&self) -> String {
        match self {
            Disposition::Serving => "healthy pane, no signal".into(),
            Disposition::LiveCap { action, .. } => match action {
                CapAction::Moved { target, ok: true } => {
                    format!("live cap, auto-moved to '{target}', verify it resumed")
                }
                CapAction::Moved { target, ok: false } => {
                    format!("live cap, auto-move to '{target}' FAILED, needs manual move")
                }
                CapAction::Cooldown => "still capped, holding within action cooldown".into(),
                CapAction::NonPool { paged: true } => {
                    "capped on a non-pool profile, never auto-moved, paged".into()
                }
                CapAction::NonPool { paged: false } => {
                    "capped on a non-pool profile, standing page suppressed within re-page cooldown"
                        .into()
                }
            },
            Disposition::ReplayedBanner { why } => {
                format!("replayed scrollback banner ({why}), headroom kept, no page")
            }
            Disposition::FableDrift { rule, paged, why } => {
                if *paged {
                    format!("off-Fable drift [{rule}], Commander paged")
                } else {
                    format!("off-Fable drift [{rule}], no page: {why}")
                }
            }
            Disposition::Parked { all_probed, .. } => {
                if *all_probed {
                    "all pool accounts hold fresh probed negative claims, parked".into()
                } else {
                    "no verified headroom target, parked, probe accounts and PATCH /api/capacity"
                        .into()
                }
            }
            Disposition::ActionGate { suppressed, paged } => match suppressed {
                Some("commander-exempt") => {
                    "matched string is scrollback+hook-injection, not bottom-of-pane gate".into()
                }
                Some(why) => format!("gate text is scrollback ({why}), not a parked gate"),
                None => {
                    if *paged {
                        "open ACTION REQUIRED gate at pane bottom, Commander paged".into()
                    } else {
                        "gate already surfaced (unchanged fingerprint or cross-surfacer claim)"
                            .into()
                    }
                }
            },
            Disposition::AuthLoss { paged } => {
                if *paged {
                    "account logged out or credential rejected, no reset time, Commander paged"
                        .into()
                } else {
                    "account logged out or credential rejected, no reset time, page held down"
                        .into()
                }
            }
            Disposition::DeviceCode { paged } => {
                if *paged {
                    "waiting on device-code sign-in, Commander paged".into()
                } else {
                    "waiting on device-code sign-in, within action cooldown".into()
                }
            }
            Disposition::Overloaded => "transient 529 overload, red row only".into(),
        }
    }
}

/// One tailable classification line: `ts title · id · profile · model ·
/// STATE · DECISION · reason`. Pure so tests assert the emitted line, not
/// just the decision (WO#450 acceptance).
/// The DECISION column, corrected for whether paging is actually permitted.
///
/// A `Disposition` describes what the rule would do; it has no idea whether the
/// activity class lets it happen. Left uncorrected, a row reads "page-commander"
/// while `wake` silently declines, and "page-commander" is precisely the
/// sentence an operator reads to conclude someone already knows.
fn effective_decision(disp: &Disposition, paging_on: bool) -> &'static str {
    match disp.decision() {
        "page-commander" if !paging_on => "page-withheld-class-off",
        other => other,
    }
}

/// The REASON column, with the same correction, stating what DID still happen.
/// A bare "withheld" reads as silence and sends the reader hunting for the
/// poller that no longer exists.
fn effective_reason(disp: &Disposition, paging_on: bool) -> String {
    let reason = disp.reason();
    if disp.decision() == "page-commander" && !paging_on {
        format!(
            "{reason} [page WITHHELD: pane_watchdog_page is off; \
             detection and the /api/events push still ran]"
        )
    } else {
        reason
    }
}

pub(crate) fn class_line(
    ts: u64,
    title: &str,
    id: &str,
    profile: &str,
    model: &str,
    disp: &Disposition,
    paging_on: bool,
) -> String {
    format!(
        "{ts} {title} · {id} · {profile} · {model} · {} · {} · {}",
        disp.state(),
        effective_decision(disp, paging_on),
        effective_reason(disp, paging_on)
    )
}

/// The same row as JSON for the snapshot file behind
/// `GET /api/watchdog/classifications`.
pub(crate) fn classification_json(
    ts: u64,
    title: &str,
    id: &str,
    profile: &str,
    model: &str,
    disp: &Disposition,
    paging_on: bool,
) -> serde_json::Value {
    serde_json::json!({
        "ts": ts,
        "title": title,
        "id": id,
        "profile": profile,
        "model": model,
        "state": disp.state(),
        "decision": effective_decision(disp, paging_on),
        "reason": effective_reason(disp, paging_on),
        // Stated per row rather than inferred from the decision text, so a
        // consumer can tell "nothing needed paging" from "paging is off".
        "paging_enabled": paging_on,
    })
}

/// Why an observed ACTION REQUIRED match must NOT page the Commander
/// (WO#450 ADDENDUM). Single API-side source of truth:
/// (a) `commander-exempt`: the pane IS the AoE-Commander session. Its own
///     outward-comms escalations to Ben legitimately contain the literal
///     "ACTION REQUIRED (Ben):" (plus stop-hook injected guidance quoting
///     it), and paging the Commander about the Commander is always noise.
///     WO#444 exempted only the badge; this exempts the page, keyed on the
///     same [`COMMANDER_TITLE`] identity the send fallback resolves.
/// (b) `pane-actively-working`: the live edge shows the running footer, so
///     the matched text is scrollback, not a parked bottom-of-pane gate.
/// `None` means a genuine worker gate: page as before.
pub(crate) fn action_page_suppressed(title: &str, working: bool) -> Option<&'static str> {
    if title == COMMANDER_TITLE {
        return Some("commander-exempt");
    }
    if working {
        return Some("pane-actively-working");
    }
    None
}

/// Whether a Fable-drift hit must NOT page because live evidence contradicts
/// it (WO#450). Only BLOCKAGE-class rules (the limit paraphrases) are voided:
/// they claim the account cannot serve, so a pane actively serving, or a
/// WO#445-suppressed replayed banner on the same pane, disproves them.
/// Drift-class rules (subagent model, downgrade announcements) describe live
/// work on the wrong model and page regardless of serving evidence.
pub(crate) fn fable_page_suppressed(rule: &str, working: bool, banner_suppressed: bool) -> bool {
    is_fable_blockage_rule(rule) && (working || banner_suppressed)
}

/// Blockage-class Fable rules: the limit paraphrases claiming the account
/// cannot serve, as opposed to drift-class rules describing live work on the
/// wrong model.
pub(crate) fn is_fable_blockage_rule(rule: &str) -> bool {
    matches!(rule, "fable-limit-paraphrase" | "fable-credit-out")
}

/// Whether a blockage-class hit must stay silent because the capacity
/// sentinel owns the reroute for cap classes (WO#535 defect 2). While any
/// pool profile still holds a fresh positive headroom claim the sentinel can
/// place the session, so the watchdog paging the Commander is a duplicate of
/// the sentinel's own escalation. Only a fresh probed NEGATIVE on the whole
/// pool, genuine all-dry the sentinel cannot place around, pages.
pub(crate) fn fable_blockage_defers_to_sentinel(rule: &str, pool_all_probed_capped: bool) -> bool {
    is_fable_blockage_rule(rule) && !pool_all_probed_capped
}

/// Byte cap for the classification log before it rotates to `.log.1`.
const CLASS_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Append one entry to the classification log, rotating the file aside to
/// `<name>.log.1` (replacing any previous rotation) when the append would
/// push it past `max_bytes`. Fail-open: any IO error logs and skips.
pub(crate) fn append_class_log(path: &std::path::Path, entry: &str, max_bytes: u64) {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > 0 && size + entry.len() as u64 > max_bytes {
        if let Err(e) = std::fs::rename(path, path.with_extension("log.1")) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "classification log rotation failed");
        }
    }
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, entry.as_bytes()));
    if let Err(e) = write {
        tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "classification log append failed");
    }
}

/// Resolve the tailable classification log: `AOE_WATCHDOG_CLASS_LOG`
/// override, else `<app_dir>/watchdog-classifications.log`. `None` when no
/// app dir resolves (fail-open, classification degrades to tracing only).
fn class_log_path() -> Option<PathBuf> {
    match std::env::var("AOE_WATCHDOG_CLASS_LOG") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => crate::session::get_app_dir()
            .ok()
            .map(|d| d.join("watchdog-classifications.log")),
    }
}

/// Resolve the latest-tick JSON snapshot read by
/// `GET /api/watchdog/classifications`: `AOE_WATCHDOG_CLASS_FILE` override,
/// else `<app_dir>/watchdog-classifications.json`.
pub(crate) fn class_snapshot_path() -> Option<PathBuf> {
    match std::env::var("AOE_WATCHDOG_CLASS_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => crate::session::get_app_dir()
            .ok()
            .map(|d| d.join("watchdog-classifications.json")),
    }
}

/// Spawn the supervised watchdog interval task. No-op when disabled by env
/// or config. Env overrides win over the `[watchdog]` config section, which
/// wins over the built-in interval/rule defaults.
pub(crate) fn spawn_pane_watchdog(state: Arc<super::AppState>) {
    let cfg = crate::session::Config::load_or_warn().watchdog;
    if std::env::var("AOE_PANE_WATCHDOG_DISABLE").as_deref() == Ok("1") || cfg.disabled {
        tracing::info!(
            target: "server.pane_watchdog",
            "pane watchdog disabled (AOE_PANE_WATCHDOG_DISABLE=1 or [watchdog] disabled=true)"
        );
        return;
    }
    let secs = std::env::var("AOE_PANE_WATCHDOG_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or(cfg.interval_secs)
        .filter(|s| *s >= 10)
        .unwrap_or(180);
    let rules: Vec<CompiledRule> = pane_rules::compile(&cfg.effective_rules())
        .into_iter()
        .filter(|r| {
            let known = kind_to_signal(&r.kind).is_some();
            if !known {
                tracing::warn!(
                    target: "server.pane_watchdog",
                    rule = %r.name,
                    kind = %r.kind,
                    "unknown rule kind; rule skipped (known: cap, auth, overload, action)"
                );
            }
            known
        })
        .collect();
    let rules = Arc::new(rules);
    let shutdown = state.shutdown.clone();
    crate::task_util::spawn_supervised(
        "server.pane_watchdog",
        crate::task_util::PanicPolicy::Log,
        async move {
            let mut watchdog = Watchdog::new(rules);
            let mut interval = tokio::time::interval(Duration::from_secs(secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Consume the immediate first tick so startup recovery settles
            // before the first pane scan.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => watchdog.tick(&state).await,
                    _ = shutdown.cancelled() => break,
                }
            }
        },
    );
}

/// A soft-stale rule hit at the pane edge: real banner text sitting above an
/// idle composer, a layout IDENTICAL for replayed scrollback and a live
/// standing block. The watchdog admits or holds it on temporal evidence
/// across ticks (pane pid, fingerprint, working history), never on the text
/// alone. WO#1283 D1.
struct StaleHit {
    signal: PaneSignal,
    fp: String,
    /// Cap family named by the banner when `signal` is `Capped`, else `None`.
    cap_kind: Option<capacity::CapKind>,
}

struct PaneScan {
    id: String,
    title: String,
    profile: String,
    signal: Option<PaneSignal>,
    /// Which cap family the pane banner names when `signal` is `Capped`
    /// (classified once at scan time), else `None`. Rides into the shared
    /// capacity state and the Commander pages so escalations name WHICH
    /// clock fired (fable credit vs weekly vs monthly spend vs session
    /// window). WO#414.
    cap_kind: Option<capacity::CapKind>,
    /// Content fingerprint of the winning rule's match when `signal` is
    /// `Capped`, else `None`. Backs the replayed-banner discriminator: an
    /// unchanged fingerprint that predates the profile's current headroom
    /// grant is stale scrollback, not fresh cap evidence. WO#445.
    cap_fp: Option<String>,
    /// Content fingerprint of the winning rule's match when `signal` is
    /// `ActionRequired`, else `None`. Drives the WO #139 content dampener:
    /// an unchanged fingerprint is the same already-surfaced gate.
    action_fp: Option<String>,
    /// Cross-surfacer claim key for the shared Ben-gate surface ledger
    /// (MISTAKE-f9e3f8d8 MIT-1), set only for `ActionRequired`. Primary
    /// `gate:<id>` when the pane carries a GATE-ID/BEN-GATE label, else the
    /// `sess:<id>|fp:<action_fp>` fallback.
    surface_key: Option<String>,
    drift: Option<DriftHit>,
    /// Fable model-drift evidence `(rule, fingerprint)` when this is a
    /// Fable-pinned session showing off-Fable drift, else `None`. Content-gated
    /// on its own fingerprint dampener (mirror of `action_fp`), pages the
    /// Commander, never auto-swaps. WO d6bcae49.
    fable_hit: Option<(String, String)>,
    /// The session's `--model` pin from extra_args, `-` when unpinned.
    /// Classification-log column only (WO#450).
    model: String,
    /// Whether THIS pane's live edge shows the running footer. Feeds the
    /// WO#450 ADDENDUM scrollback discriminators (`action_page_suppressed`,
    /// `fable_page_suppressed`).
    working: bool,
    /// The account's utilization as this pane's footer last rendered it, or
    /// `None` when the footer has scrolled out of the captured tail. Belongs
    /// to the ACCOUNT; the pane is only where it happened to be visible.
    usage: Option<crate::pane_rules::UsageMeter>,
    /// Soft-stale rule hit pending the watchdog's temporal admission pass,
    /// else `None`. Mutually exclusive with `signal` (a current hit always
    /// wins inside `classify_fp_admitting`). WO#1283 D1.
    stale_hit: Option<StaleHit>,
    /// The tmux pane's shell PID at capture time. A changed pid between
    /// ticks voids stale-banner admission: a respawned pane replays its
    /// predecessor's scrollback.
    pane_pid: Option<u32>,
    /// Why a soft-stale hit was HELD rather than admitted this tick, else
    /// `None`. Set by the admission pass; classification-log column.
    stale_held: Option<&'static str>,
}

/// Capture and classify every registered non-structured session's pane tail.
/// Also returns the set of profiles with at least one pane actively working
/// ("esc to interrupt" at the live edge) — empirical serving evidence that
/// outranks a replayed cap banner on a sibling pane (WO#445). Blocking (tmux
/// subprocesses + storage reads); run under `spawn_blocking`.
fn scan_panes(
    file_watch: &Arc<FileWatchService>,
    rules: &[CompiledRule],
) -> (Vec<PaneScan>, HashSet<String>) {
    let instances = match super::load_all_instances(file_watch) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(target: "server.pane_watchdog", error = %e, "load_all_instances failed; skipping tick");
            return (Vec::new(), HashSet::new());
        }
    };
    let mut serving: HashSet<String> = HashSet::new();
    // Profile model pins are resolved from profile config once per
    // (profile, tool) pair per tick, not once per session.
    let mut pin_memo: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut scans = Vec::new();
    for inst in instances.iter().filter(|inst| !inst.is_structured()) {
        let Ok(sess) = inst.tmux_session() else {
            continue;
        };
        if !sess.exists() {
            continue;
        }
        let Ok(content) = sess.capture_pane(60) else {
            continue;
        };
        // Serving evidence is collected for EVERY captured pane, before
        // any signal handling: a working pane with no signal at all is
        // exactly the proof that its profile serves (WO#445).
        let working = pane_is_actively_working(&content);
        if working {
            serving.insert(inst.source_profile.clone());
        }
        // classify_fp_admitting yields the winning rule AND its churn-stable
        // content fingerprint in one pass; keep the fp only for the
        // ActionRequired signal (the sole content-gated path, WO #139) and
        // the Capped signal (the replayed-banner discriminator, WO#445). A
        // soft hit (banner above an idle composer) is split off for the
        // temporal admission pass instead of being treated as live.
        let (hit, stale_hit) = match pane_rules::classify_fp_admitting(&content, rules) {
            Some((rule, fp, true)) => {
                let soft = kind_to_signal(&rule.kind).map(|signal| StaleHit {
                    signal,
                    fp,
                    cap_kind: (signal == PaneSignal::Capped)
                        .then(|| capacity::classify_cap_kind(&content)),
                });
                (None, soft)
            }
            Some((rule, fp, false)) => (Some((rule, fp)), None),
            None => (None, None),
        };
        let signal = hit.as_ref().and_then(|(r, _)| kind_to_signal(&r.kind));
        let cap_kind = match signal {
            Some(PaneSignal::Capped) => Some(capacity::classify_cap_kind(&content)),
            _ => None,
        };
        let cap_fp = match (signal, &hit) {
            (Some(PaneSignal::Capped), Some((_, fp))) => Some(fp.clone()),
            _ => None,
        };
        let action_fp = match (signal, &hit) {
            (Some(PaneSignal::ActionRequired), Some((_, fp))) => Some(fp.clone()),
            _ => None,
        };
        // Cross-surfacer claim key (MIT-1): prefer the labelled gate id from
        // the pane (shared across all four notifiers for an outward-comms
        // gate), else the watchdog's own session+fingerprint fallback.
        let surface_key =
            action_fp
                .as_ref()
                .map(|fp| match ben_gate_surface::extract_gate_id(&content) {
                    Some(g) => format!("gate:{g}"),
                    None => format!("sess:{}|fp:{}", inst.id, fp),
                });
        // The leading indicator, read from the same capture as the rules.
        // Present on every healthy pane, long before any banner exists.
        let usage = pane_rules::parse_usage_meter(&content);
        let drift = charter_drift::detect(&inst.title, &inst.project_path, &content);
        // Fable model-drift and the model column are gated on the session's
        // EFFECTIVE pin, not raw extra_args: an unpinned session on a
        // Fable-pinned profile launches on Fable, so its drift matters just
        // as much (WO#1283 D1: for-AVP sat session-unpinned on a pinned
        // profile and every drift scan stayed silent).
        let profile_pin = pin_memo
            .entry((inst.effective_profile(), inst.tool.clone()))
            .or_insert_with_key(|(profile, tool)| {
                crate::session::profile_config::resolve_config_or_warn(profile)
                    .session
                    .agent_extra_args
                    .get(tool)
                    .cloned()
            })
            .clone();
        let pin_args = effective_pin_args(&inst.extra_args, profile_pin.as_deref());
        let fable_hit = fable_scan_hit(&pin_args, &content);
        // EVERY captured live session yields a scan row, healthy or not:
        // the per-tick classification log must show a line per session so
        // silent misses are visible, not inferred from absence (WO#450).
        scans.push(PaneScan {
            id: inst.id.clone(),
            title: inst.title.clone(),
            profile: inst.source_profile.clone(),
            signal,
            cap_kind,
            cap_fp,
            action_fp,
            surface_key,
            drift,
            fable_hit,
            model: pane_rules::model_pin(&pin_args).unwrap_or_else(|| "-".into()),
            working,
            usage,
            stale_hit,
            pane_pid: crate::process::get_pane_pid(sess.name()),
            stale_held: None,
        });
    }
    (scans, serving)
}

/// The extra_args string that carries a session's EFFECTIVE model pin: the
/// session's own args when they hold an explicit `--model` / `-m` flag (a
/// set-model escalation or user-typed pin), else the profile's
/// `session.agent_extra_args` pin for the tool (what a terminal launch would
/// inject), else empty. Mirrors `launch_model_flag_injection` precedence.
fn effective_pin_args(session_extra_args: &str, profile_pin: Option<&str>) -> String {
    if crate::session::config::has_model_flag(session_extra_args) {
        return session_extra_args.to_string();
    }
    profile_pin.map(str::to_string).unwrap_or_default()
}

/// What one pane looked like on the PREVIOUS watchdog tick, for stale-banner
/// admission (WO#1283 D1). In-memory only, deliberately not persisted: after
/// a daemon bounce the map is empty, so a standing banner waits one tick to
/// re-establish pane continuity instead of being trusted off replayed
/// scrollback.
struct PaneTickMemory {
    pane_pid: Option<u32>,
    /// The banner fingerprint this pane showed last tick, live or
    /// soft-stale, else `None`.
    cap_fp: Option<String>,
    working: bool,
    profile: String,
}

/// Decide whether a soft-stale banner (real banner text above an idle
/// composer) is a live standing block or replayed scrollback. The TEXT
/// cannot discriminate: a substring match cannot separate a banner from a
/// discussion of a banner, and the idle-composer layout is identical for
/// both. Continuity across ticks can. `Ok` admits and names the admitting
/// edge; `Err` holds and names the missing evidence.
///
/// Admission requires the same pane pid and profile as last tick, and then
/// one of: the banner just appeared (fresh edge), the banner text changed,
/// or the same banner stood while the pane was idle on BOTH sightings. A
/// pane that served during the window disproves the block it claims.
fn admit_stale_banner(
    prev: Option<&PaneTickMemory>,
    pid: Option<u32>,
    fp: &str,
    profile: &str,
    working: bool,
) -> Result<&'static str, &'static str> {
    let Some(prev) = prev else {
        return Err("idle-composer-first-sighting");
    };
    if pid.is_none() || prev.pane_pid != pid {
        return Err("idle-composer-pane-changed");
    }
    if prev.profile != profile {
        return Err("idle-composer-pre-move");
    }
    match prev.cap_fp.as_deref() {
        None => Ok("fresh-edge"),
        Some(prev_fp) if prev_fp == fp => {
            if !working && !prev.working {
                Ok("standing-idle")
            } else {
                Err("idle-composer-serving-during-window")
            }
        }
        Some(_) => Ok("banner-changed"),
    }
}

struct Watchdog {
    /// The compiled effective rule set (defaults + config), fixed for the
    /// daemon's lifetime.
    rules: Arc<Vec<CompiledRule>>,
    /// Profiles observed capped, with when. Entries expire after [`CAP_TTL`]
    /// so a reset account re-enters the relocation pool without a restart.
    capped_profiles: HashMap<String, Instant>,
    /// Last watchdog action per session id ([`ACTION_COOLDOWN`]).
    last_session_action: HashMap<String, Instant>,
    /// Last ALL-CAPPED escalation, rate-limited to once per [`CAP_TTL`].
    last_all_capped_wake: Option<Instant>,
    /// Last charter-drift escalation per (session id, observed target)
    /// ([`DRIFT_COOLDOWN`]).
    last_drift_wake: HashMap<(String, String), Instant>,
    /// Last ACTION REQUIRED gate fingerprint escalated per session, persisted
    /// across daemon restarts. An unchanged fingerprint suppresses the re-wake;
    /// a changed one wakes immediately. WO #139. Loaded from disk in `new` so a
    /// daemon bounce does not re-page every standing gate (the amplifier bug).
    last_action_fp: HashMap<String, ActionFp>,
    /// Last Fable model-drift fingerprint paged per session, persisted across
    /// restarts. Same content-dampener semantics as `last_action_fp`: an
    /// unchanged drift fingerprint is suppressed, a new/changed one pages the
    /// Commander immediately. WO d6bcae49.
    last_fable_fp: HashMap<String, ActionFp>,
    /// Last observed cap-banner fingerprint per session with its wall-clock
    /// first-seen, persisted across restarts. Backs the WO#445 replayed-banner
    /// gate: a banner whose unchanged content predates the profile's verified
    /// headroom grant is stale scrollback and must not revoke the grant.
    last_cap_fp: HashMap<String, CapFp>,
    /// Last Commander page emitted per session for a NON-POOL cap (profile +
    /// wall-clock secs), persisted across restarts. Backs the WO#605 re-page
    /// dampener: a standing cap on the same non-pool profile is paged at most
    /// once per [`NON_POOL_PAGE_COOLDOWN`], so a daemon bounce does not re-flood
    /// every parked non-pool cap.
    last_non_pool_page: HashMap<String, NonPoolPage>,
    /// Last classification state pushed to the event bus per session, so a
    /// standing condition is announced on entry and not on every tick.
    ///
    /// Deliberately NOT persisted, unlike the page dampeners above. A daemon
    /// bounce SHOULD re-announce every standing cap: a subscriber that comes
    /// up alongside the daemon has no other way to learn the fleet's current
    /// state, and re-stating a condition costs a subscriber one deduplication
    /// while missing one costs a human their afternoon.
    last_event_state: HashMap<String, String>,
    /// Whether each (account, window) was last seen above the usage threshold.
    /// Keyed by ACCOUNT, never by session: one account has one meter however
    /// many panes render it.
    last_usage_above: HashMap<(String, &'static str), bool>,
    /// Per-pane snapshot of the previous tick, feeding [`admit_stale_banner`].
    /// In-memory only (see [`PaneTickMemory`]).
    pane_memory: HashMap<String, PaneTickMemory>,
}

impl Watchdog {
    fn new(rules: Arc<Vec<CompiledRule>>) -> Self {
        Self {
            rules,
            capped_profiles: HashMap::new(),
            last_session_action: load_last_action(),
            last_all_capped_wake: None,
            last_drift_wake: HashMap::new(),
            last_action_fp: load_action_fp(),
            last_fable_fp: load_fable_fp(),
            last_cap_fp: load_cap_fp(),
            last_non_pool_page: load_non_pool_page(),
            last_event_state: HashMap::new(),
            last_usage_above: HashMap::new(),
            pane_memory: HashMap::new(),
        }
    }

    async fn tick(&mut self, state: &Arc<super::AppState>) {
        let file_watch = state.file_watch.clone();
        let rules = self.rules.clone();
        let (mut scans, serving) = match tokio::task::spawn_blocking(move || {
            scan_panes(&file_watch, &rules)
        })
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "pane scan task failed");
                return;
            }
        };

        let now = Instant::now();
        self.capped_profiles
            .retain(|_, seen| now.duration_since(*seen) < CAP_TTL);
        self.last_drift_wake
            .retain(|_, fired| now.duration_since(*fired) < DRIFT_COOLDOWN);
        // Forget an ACTION REQUIRED fingerprint once its gate has gone
        // unobserved past the TTL, so a genuinely cleared-then-reopened gate
        // is treated as new and wakes again. WO #139.
        self.last_action_fp
            .retain(|_, a| now.duration_since(a.seen) < ACTION_FP_TTL);
        // Same TTL-forget for Fable-drift fingerprints: a drift that stops being
        // observed ages out so a genuinely new one later pages again. WO d6bcae49.
        self.last_fable_fp
            .retain(|_, a| now.duration_since(a.seen) < ACTION_FP_TTL);
        // Cap-banner fingerprints age out on the same TTL so a long-cleared
        // banner's first-seen doesn't linger to misdate a future one. WO#445.
        self.last_cap_fp
            .retain(|_, c| now.duration_since(c.seen) < ACTION_FP_TTL);

        // WO#1283 D1: temporal admission of soft-stale hits, BEFORE the
        // WO#445 discriminator so an admitted banner faces the same
        // serving/pre-grant gates as a hard live one. An admitted hit is
        // promoted to this scan's live signal; a held one only annotates
        // the classification row.
        for scan in &mut scans {
            let Some(hit) = &scan.stale_hit else {
                continue;
            };
            let verdict = admit_stale_banner(
                self.pane_memory.get(&scan.id),
                scan.pane_pid,
                &hit.fp,
                &scan.profile,
                scan.working,
            );
            let (signal, fp, cap_kind) = (hit.signal, hit.fp.clone(), hit.cap_kind);
            match verdict {
                Ok(edge) => {
                    tracing::info!(
                        target: "server.pane_watchdog",
                        id = %scan.id,
                        profile = %scan.profile,
                        edge,
                        "soft-stale banner admitted as live on temporal evidence (WO#1283 D1)"
                    );
                    scan.signal = Some(signal);
                    if signal == PaneSignal::Capped {
                        scan.cap_kind = cap_kind;
                        scan.cap_fp = Some(fp);
                    }
                }
                Err(why) => scan.stale_held = Some(why),
            }
        }
        // Rebuild the per-pane memory from what THIS tick actually saw,
        // admitted or held: next tick's admission compares against it.
        self.pane_memory = scans
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    PaneTickMemory {
                        pane_pid: s.pane_pid,
                        cap_fp: s
                            .cap_fp
                            .clone()
                            .or_else(|| s.stale_hit.as_ref().map(|h| h.fp.clone())),
                        working: s.working,
                        profile: s.profile.clone(),
                    },
                )
            })
            .collect();

        // WO#445: discriminate LIVE cap banners from replayed scrollback
        // BEFORE any revocation. The capacity state is loaded once here so
        // every suppression verdict this tick compares the banner's
        // first-seen against the same pre-revocation grant timestamps.
        let now_secs = unix_secs();
        let cap_state_pre = capacity::capacity_path()
            .map(|p| capacity::CapacityState::load(&p))
            .unwrap_or_default();
        let mut suppressed_caps: HashMap<String, &'static str> = HashMap::new();
        for scan in &scans {
            if scan.signal != Some(PaneSignal::Capped) {
                continue;
            }
            let current_fp = scan.cap_fp.clone().unwrap_or_default();
            let prev = self
                .last_cap_fp
                .get(&scan.id)
                .map(|c| (c.fp.as_str(), c.first_seen_secs));
            let first_seen = cap_first_seen(prev, &current_fp, now_secs);
            self.last_cap_fp.insert(
                scan.id.clone(),
                CapFp {
                    fp: current_fp,
                    first_seen_secs: first_seen,
                    seen: now,
                },
            );
            let grant_updated = cap_state_pre
                .profiles
                .get(&scan.profile)
                .map(|e| e.updated)
                .unwrap_or(0);
            if let Some(reason) = cap_revoke_suppressed(
                serving.contains(&scan.profile),
                cap_state_pre.verified_headroom(&scan.profile, now_secs),
                grant_updated,
                first_seen,
            ) {
                tracing::info!(
                    target: "server.pane_watchdog",
                    id = %scan.id,
                    profile = %scan.profile,
                    reason,
                    "cap banner suppressed: not live cap evidence, headroom kept (WO#445)"
                );
                suppressed_caps.insert(scan.id.clone(), reason);
            }
        }
        self.persist_cap_fp();

        for scan in &scans {
            if scan.signal == Some(PaneSignal::Capped) && !suppressed_caps.contains_key(&scan.id) {
                self.capped_profiles.insert(scan.profile.clone(), now);
            }
        }
        self.persist_cap_state();
        // Fold every observed cap into the shared capacity state: a pane
        // showing a cap banner drops that profile out of the relocation pool
        // immediately, no matter how fresh its last positive probe was.
        // Observation beats a standing claim (WO#414) — unless the WO#445
        // discriminator proved the banner stale/contradicted above.
        let observed_caps: Vec<(String, capacity::CapKind)> = scans
            .iter()
            .filter(|s| {
                s.signal == Some(PaneSignal::Capped) && !suppressed_caps.contains_key(&s.id)
            })
            .map(|s| {
                (
                    s.profile.clone(),
                    s.cap_kind.unwrap_or(capacity::CapKind::Unknown),
                )
            })
            .collect();
        if !observed_caps.is_empty() {
            if let Some(path) = capacity::capacity_path() {
                let now_secs = unix_secs();
                let mut cap_state = capacity::CapacityState::load(&path);
                for (profile, kind) in observed_caps {
                    cap_state.revoke_headroom(&profile, kind, now_secs);
                }
                cap_state.save(&path);
            }
        }

        // WO#450: every live session gets exactly one classification row per
        // tick — state, decision, reason — appended to the tailable log and
        // snapshotted for GET /api/watchdog/classifications.
        let ts = unix_secs();
        // Resolved once per tick, not per row, so every row in a snapshot
        // reports the same answer to "was anyone actually woken".
        let cfg = crate::session::config::Config::load_or_warn();
        let paging_on = paging_allowed(&cfg.activity);

        // The LEADING indicator, resolved before any per-session
        // classification. It is an account-level fact, so it is computed from
        // every pane at once and announced once per account, not once per pane
        // and not once per tick. A cap banner is this same fact arriving too
        // late to act on.
        let threshold = cfg.watchdog.usage_threshold();
        let meters = account_meters(scans.iter().map(|s| (s.profile.as_str(), s.usage)));
        for (profile, window, pct) in usage_alerts(&meters, threshold, &mut self.last_usage_above) {
            let (kind, label) = if window == "5h" {
                ("usage_5h", "5-hour")
            } else {
                ("usage_weekly", "weekly")
            };
            super::event_bus::emit_and_fan_out(
                state,
                kind,
                // The subject is the ACCOUNT. Naming it in the id slot keeps a
                // subscriber's dedup keyed on the thing that runs out, rather
                // than on whichever session happened to render the number.
                &format!("account:{profile}"),
                &profile,
                &profile,
                &format!(
                    "{profile} is at {pct}% of its {label} allowance (threshold \
                     {threshold}%), read from the account's own meter rather than \
                     from a cap banner"
                ),
            );
        }
        let mut class_lines = String::new();
        let mut class_rows: Vec<serde_json::Value> = Vec::new();
        for scan in scans {
            if let Some(hit) = scan.drift.clone() {
                self.handle_drift(&scan, &hit, now).await;
            }
            let banner_suppressed = suppressed_caps.contains_key(&scan.id);
            // Fable model-drift is independent of the general signal (a
            // Fable-pinned session can drift with no cap/auth signal), so handle
            // it before the `signal` dispatch, content-gated on its own dampener.
            let fable_disp = match scan.fable_hit.clone() {
                Some((rule, fp)) => Some(
                    self.handle_fable_drift(&scan, &rule, &fp, now, banner_suppressed)
                        .await,
                ),
                None => None,
            };
            let disp = match (scan.signal, scan.stale_held) {
                // A soft-stale hit held by the admission pass (WO#1283 D1):
                // banner text above an idle composer with no temporal proof
                // of a live block. Logged with the specific missing
                // evidence, never paged.
                (None, Some(why)) => Disposition::ReplayedBanner { why },
                // A signal-bearing pane's disposition wins the row; a pure
                // Fable drift (no cap/auth/gate signal) reports as drift.
                (None, None) => fable_disp.unwrap_or(Disposition::Serving),
                // A suppressed cap banner is stale scrollback or contradicted
                // by live serving evidence (WO#445): no red row, no relocation.
                (Some(PaneSignal::Capped), _) if banner_suppressed => Disposition::ReplayedBanner {
                    why: suppressed_caps
                        .get(&scan.id)
                        .copied()
                        .unwrap_or("suppressed"),
                },
                (Some(signal), _) => self.handle_signal(&scan, signal, now).await,
            };
            // Push, at the moment of detection. The classification row below
            // is the record; this is the notification, and it goes out whether
            // or not anyone happens to be reading the record.
            if is_event_edge(&mut self.last_event_state, &scan.id, &disp.state()) {
                if let Some(kind) = event_kind(&disp) {
                    super::event_bus::emit_and_fan_out(
                        state,
                        kind,
                        &scan.id,
                        &scan.title,
                        &scan.profile,
                        &disp.reason(),
                    );
                }
            }
            class_lines.push_str(&class_line(
                ts,
                &scan.title,
                &scan.id,
                &scan.profile,
                &scan.model,
                &disp,
                paging_on,
            ));
            class_lines.push('\n');
            class_rows.push(classification_json(
                ts,
                &scan.title,
                &scan.id,
                &scan.profile,
                &scan.model,
                &disp,
                paging_on,
            ));
        }
        if let Some(path) = class_log_path() {
            append_class_log(&path, &class_lines, CLASS_LOG_MAX_BYTES);
        }
        if let Some(path) = class_snapshot_path() {
            let snap = serde_json::json!({ "updated": ts, "sessions": class_rows });
            if let Err(e) = std::fs::write(&path, snap.to_string()) {
                tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "classification snapshot write failed");
            }
        }
        // WO#449 anti-bounce: the ACTION_COOLDOWN map must survive a daemon
        // bounce (launchd KeepAlive respawn), else every restart wipes the
        // cooldown and a still-capped pane is re-acted-on immediately —
        // the move/bounce loop Ben saw.
        self.persist_last_action();
    }

    /// Dispatch one live (non-suppressed) pane signal and return the tick's
    /// classification for the row (WO#450).
    async fn handle_signal(
        &mut self,
        scan: &PaneScan,
        signal: PaneSignal,
        now: Instant,
    ) -> Disposition {
        // WO#450 ADDENDUM: the action-required PAGE has the same exemptions
        // as the badge (WO#444) — the Commander's own pane text and any
        // actively-working pane's scrollback are not parked worker gates.
        // Resolved BEFORE the urgent mirror and the fingerprint dampener so a
        // suppressed match leaves no state behind.
        if signal == PaneSignal::ActionRequired {
            if let Some(why) = action_page_suppressed(&scan.title, scan.working) {
                tracing::debug!(
                    target: "server.pane_watchdog",
                    id = %scan.id,
                    title = %scan.title,
                    why,
                    "ACTION REQUIRED match suppressed; not a bottom-of-pane worker gate (WO#450)"
                );
                return Disposition::ActionGate {
                    paged: false,
                    suppressed: Some(why),
                };
            }
        }
        // Mirror the observed block into the instance's attention.json so
        // the TUI/FleetView red row reflects pane truth without any
        // agent-side text scanning (the watchdog is the SOLE text
        // authority for urgency). Every tick refreshes the TTL while the
        // pane stays blocked; expiry clears it after recovery. Runs
        // BEFORE the action cooldown — the row must stay red even when
        // the escalation is rate-limited.
        mirror_urgent(scan, signal);

        // ACTION REQUIRED gates are CONTENT-gated, not time-gated: an
        // unchanged, already-surfaced gate must never re-wake (the flood
        // WO #139 fixes), but a new/changed gate wakes immediately with no
        // cooldown wait. This path runs BEFORE the temporal ACTION_COOLDOWN
        // that still governs cap/auth re-escalation. Every observation
        // refreshes the fingerprint's `seen` (TTL) and persists the map so
        // a daemon restart cannot re-page a standing gate.
        if signal == PaneSignal::ActionRequired {
            let current_fp = scan.action_fp.clone().unwrap_or_default();
            let last_fp = self.last_action_fp.get(&scan.id).map(|a| a.fp.as_str());
            let should = action_should_wake(last_fp, &current_fp);
            self.last_action_fp.insert(
                scan.id.clone(),
                ActionFp {
                    fp: current_fp,
                    seen: now,
                },
            );
            let mut paged = false;
            if should {
                // Cross-surfacer dedup (MISTAKE-f9e3f8d8 MIT-1): even when
                // this gate is new/changed for the watchdog's own WO#139
                // dampener, suppress the wake if another notifier (worker
                // Stop-hook, worker->Commander report, fleet-tick page)
                // already surfaced this exact gate-state to Ben. The claim
                // is content-keyed (`gate:<id>`) so all four lanes converge;
                // a live claim held by a DIFFERENT surfacer suppresses, the
                // same surfacer refreshes. Fail-open: any ledger error ->
                // claim returns true and the wake proceeds.
                let cross_ok = match &scan.surface_key {
                    Some(key) => ben_gate_surface::claim_surface(
                        key,
                        "pane-watchdog",
                        ACTION_FP_TTL.as_secs(),
                        unix_now(),
                    ),
                    None => true,
                };
                if cross_ok {
                    wake(
                        "action-required",
                        &scan.id,
                        format!(
                            "session '{}' ({}) has an open ACTION REQUIRED gate",
                            scan.title, scan.id
                        ),
                    )
                    .await;
                    paged = true;
                } else {
                    tracing::debug!(
                        target: "server.pane_watchdog",
                        id = %scan.id,
                        "ACTION REQUIRED gate already surfaced by another notifier; cross-surfacer deduped (MIT-1)"
                    );
                }
            }
            self.persist_action_fp();
            return Disposition::ActionGate {
                paged,
                suppressed: None,
            };
        }

        let cooling = self
            .last_session_action
            .get(&scan.id)
            .is_some_and(|last| now.duration_since(*last) < ACTION_COOLDOWN);
        match signal {
            // A logged-out account cannot be relocated onto another account's
            // headroom, so there is nothing to attempt: page if the class
            // allows, and let the row and the push carry the fact.
            PaneSignal::AuthLoss => {
                if !cooling {
                    wake(
                        "auth-loss",
                        &scan.id,
                        format!(
                            "session '{}' ({}) is logged out or its credential was \
                             rejected on profile '{}'; there is no reset time and it \
                             will produce nothing until someone signs it back in",
                            scan.title, scan.id, scan.profile
                        ),
                    )
                    .await;
                    self.last_session_action.insert(scan.id.clone(), now);
                }
                Disposition::AuthLoss { paged: !cooling }
            }
            PaneSignal::Capped if cooling => Disposition::LiveCap {
                kind: scan.cap_kind.unwrap_or(capacity::CapKind::Unknown).as_str(),
                action: CapAction::Cooldown,
            },
            PaneSignal::Capped => self.handle_capped(scan, now).await,
            PaneSignal::DeviceCode if cooling => Disposition::DeviceCode { paged: false },
            PaneSignal::DeviceCode => {
                self.last_session_action.insert(scan.id.clone(), now);
                wake(
                    "device-code",
                    &scan.id,
                    format!(
                        "session '{}' ({}) is waiting on a device-code sign-in",
                        scan.title, scan.id
                    ),
                )
                .await;
                Disposition::DeviceCode { paged: true }
            }
            // Transient server-side overload: red row only (mirrored
            // above); relocation/wake would thrash on a condition that
            // clears itself.
            PaneSignal::Overloaded => Disposition::Overloaded,
            // Handled by the content-gated branch above (which returns
            // before reaching this match), so it is unreachable here.
            PaneSignal::ActionRequired => Disposition::ActionGate {
                paged: false,
                suppressed: None,
            },
        }
    }

    /// Escalate a scope/charter drift: the session's pane shows sustained
    /// work on another product's repo or appliance host. Advisory only, so
    /// no relocation and no attention-row mirror; the Commander decides.
    async fn handle_drift(&mut self, scan: &PaneScan, hit: &DriftHit, now: Instant) {
        let key = (scan.id.clone(), hit.observed.clone());
        if self
            .last_drift_wake
            .get(&key)
            .is_some_and(|fired| now.duration_since(*fired) < DRIFT_COOLDOWN)
        {
            return;
        }
        self.last_drift_wake.insert(key, now);
        wake(
            "charter-drift",
            &scan.id,
            format!(
                "session '{}' ({}) is out of charter: chartered as {} but touching {}",
                scan.title, scan.id, hit.charter, hit.observed
            ),
        )
        .await;
    }

    /// Page the Commander that a Fable-pinned session is drifting off Fable: a
    /// silent Fable cap, a runtime downgrade announcement, or a non-Fable
    /// subagent (condition (a)/(b), WO d6bcae49). Content-gated on the drift
    /// fingerprint (mirror of the WO #139 ActionFp dampener) so a standing drift
    /// does not re-flood; a new/changed drift pages immediately. NEVER auto-
    /// swaps the model — the Commander decides. Every observation refreshes the
    /// fingerprint's `seen` (TTL) and persists the map so a daemon restart
    /// cannot re-page a standing drift.
    async fn handle_fable_drift(
        &mut self,
        scan: &PaneScan,
        rule: &str,
        fp: &str,
        now: Instant,
        banner_suppressed: bool,
    ) -> Disposition {
        // WO#450: a BLOCKAGE-class hit (limit paraphrase / credit-out) claims
        // the account cannot serve, so live serving evidence on this pane, or
        // a WO#445 replayed-banner suppression for it, disproves the claim.
        // No wake and NO fingerprint record — when the contradicting evidence
        // later disappears the same content must still be able to page.
        if fable_page_suppressed(rule, scan.working, banner_suppressed) {
            tracing::debug!(
                target: "server.pane_watchdog",
                id = %scan.id,
                rule,
                "Fable blockage-class hit contradicted by live evidence; page voided (WO#450)"
            );
            return Disposition::FableDrift {
                rule: rule.to_string(),
                paged: false,
                why: "contradicted by live serving or suppressed-banner evidence",
            };
        }
        // WO#535 defect 2: cap-class hits belong to the capacity sentinel,
        // which reroutes the session and fires its own single all-dry gate.
        // No fingerprint record here either, so if the pool later drains the
        // same content can still page.
        if is_fable_blockage_rule(rule) {
            let state = match capacity::capacity_path() {
                Some(path) => capacity::CapacityState::load(&path),
                None => capacity::CapacityState::default(),
            };
            if fable_blockage_defers_to_sentinel(rule, all_pool_probed_capped(&state, unix_secs()))
            {
                tracing::debug!(
                    target: "server.pane_watchdog",
                    id = %scan.id,
                    rule,
                    "Fable blockage-class hit deferred to capacity sentinel; pool not all-dry (WO#535)"
                );
                return Disposition::FableDrift {
                    rule: rule.to_string(),
                    paged: false,
                    why: "deferred to capacity sentinel, pool not proven dry",
                };
            }
        }
        let last_fp = self.last_fable_fp.get(&scan.id).map(|a| a.fp.as_str());
        let should = action_should_wake(last_fp, fp);
        self.last_fable_fp.insert(
            scan.id.clone(),
            ActionFp {
                fp: fp.to_string(),
                seen: now,
            },
        );
        if should {
            wake(
                "fable-model-drift",
                &scan.id,
                format!(
                    "Fable-pinned session '{}' ({}) shows off-Fable model drift [{}]: {}",
                    scan.title, scan.id, rule, fp
                ),
            )
            .await;
        }
        self.persist_fable_fp();
        Disposition::FableDrift {
            rule: rule.to_string(),
            paged: should,
            why: if should {
                "new drift fingerprint"
            } else {
                "already surfaced, unchanged fingerprint"
            },
        }
    }

    async fn handle_capped(&mut self, scan: &PaneScan, now: Instant) -> Disposition {
        let cap_kind = scan.cap_kind.unwrap_or(capacity::CapKind::Unknown);
        if !DRAW_ORDER.contains(&scan.profile.as_str()) {
            self.last_session_action.insert(scan.id.clone(), now);
            // WO#605: a non-pool cap is never auto-moved, so the wake is a
            // notification. Page only on a genuine state change — first
            // sighting, a move to a different non-pool profile, or once the
            // re-page cooldown elapses — else the standing cap re-floods the
            // Commander every ACTION_COOLDOWN re-entry.
            let now_secs = unix_secs();
            let last = self
                .last_non_pool_page
                .get(&scan.id)
                .map(|p| (p.profile.as_str(), p.paged_at_secs));
            let paged = non_pool_should_page(last, &scan.profile, now_secs);
            if paged {
                self.last_non_pool_page.insert(
                    scan.id.clone(),
                    NonPoolPage {
                        profile: scan.profile.clone(),
                        paged_at_secs: now_secs,
                    },
                );
                self.persist_non_pool_page();
                wake(
                    "capped-non-pool",
                    &scan.id,
                    format!(
                        "session '{}' ({}) is capped on non-pool profile '{}'; not auto-moving",
                        scan.title, scan.id, scan.profile
                    ),
                )
                .await;
            } else {
                tracing::debug!(
                    target: "server.pane_watchdog",
                    session = %scan.id,
                    profile = %scan.profile,
                    "capped-non-pool page suppressed within re-page cooldown (WO#605)"
                );
            }
            return Disposition::LiveCap {
                kind: cap_kind.as_str(),
                action: CapAction::NonPool { paged },
            };
        }
        // The relocation gate reads the SHARED capacity state, not the
        // in-memory cap map: only a fresh positive claim (written by a
        // Commander probe via PATCH /api/capacity) makes a profile a target.
        // Unknown parks (WO#414).
        let state = match capacity::capacity_path() {
            Some(path) => capacity::CapacityState::load(&path),
            None => capacity::CapacityState::default(),
        };
        match next_verified_headroom(&scan.profile, &state, unix_secs()) {
            Some(target) => {
                self.last_session_action.insert(scan.id.clone(), now);
                tracing::warn!(
                    target: "server.pane_watchdog",
                    session = %scan.id,
                    title = %scan.title,
                    from = %scan.profile,
                    to = %target,
                    cap_kind = cap_kind.as_str(),
                    "capped session detected; relocating to verified-headroom profile"
                );
                // `session move` carries the instance record whole, including
                // extra_args, so a --model pin survives the relocation.
                let moved = match aoe_command(&["session", "move", &scan.id, &target]).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(target: "server.pane_watchdog", session = %scan.id, error = %e, "session move failed");
                        false
                    }
                };
                let kind = if moved {
                    "capped-moved"
                } else {
                    "capped-move-failed"
                };
                wake(
                    kind,
                    &scan.id,
                    capped_move_reason(
                        &scan.title,
                        &scan.id,
                        &scan.profile,
                        &target,
                        cap_kind.as_str(),
                        moved,
                    ),
                )
                .await;
                Disposition::LiveCap {
                    kind: cap_kind.as_str(),
                    action: CapAction::Moved { target, ok: moved },
                }
            }
            None => {
                // WO#449 directive 4: only a fresh probed NEGATIVE on
                // EVERY pool profile justifies a credits-flavored
                // escalation; anything less is "state unknown — probe",
                // never a money gate.
                let all_probed = all_pool_probed_capped(&state, unix_secs());
                let due = self
                    .last_all_capped_wake
                    .is_none_or(|t| now.duration_since(t) >= CAP_TTL);
                if due {
                    self.last_all_capped_wake = Some(now);
                    let (kind, reason) = parked_wake(
                        all_probed,
                        cap_kind.as_str(),
                        &scan.title,
                        &scan.id,
                        &scan.profile,
                    );
                    wake(kind, &scan.id, reason).await;
                }
                Disposition::Parked {
                    kind: cap_kind.as_str(),
                    all_probed,
                }
            }
        }
    }

    /// Best-effort JSON snapshot of the live cap map so operators and hooks
    /// can read it without asking the daemon.
    fn persist_cap_state(&self) {
        let path = match std::env::var("AOE_CAP_STATE_FILE") {
            Ok(p) => PathBuf::from(p),
            Err(_) => match crate::session::get_app_dir() {
                Ok(dir) => dir.join("cap-state.json"),
                Err(e) => {
                    tracing::warn!(target: "server.pane_watchdog", error = %e, "no app dir; cap state not persisted");
                    return;
                }
            },
        };
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now_inst = Instant::now();
        let capped: HashMap<&str, u64> = self
            .capped_profiles
            .iter()
            .map(|(profile, seen)| {
                let age = now_inst.duration_since(*seen).as_secs();
                (profile.as_str(), now_secs.saturating_sub(age))
            })
            .collect();
        let json = serde_json::json!({ "updated": now_secs, "capped": capped });
        if let Err(e) = std::fs::write(&path, json.to_string()) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "cap state write failed");
        }
    }

    /// Persist the ACTION REQUIRED fingerprint map so a daemon restart resumes
    /// the content dampener instead of re-paging every standing gate. Only the
    /// `id -> fingerprint` pairs are stored; `seen` is monotonic and reset to
    /// "now" on load (the fp is what dedups; the TTL window simply restarts).
    /// WO #139.
    fn persist_action_fp(&self) {
        let Some(path) = action_fp_path() else { return };
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let gates: HashMap<&str, &str> = self
            .last_action_fp
            .iter()
            .map(|(id, a)| (id.as_str(), a.fp.as_str()))
            .collect();
        let json = serde_json::json!({ "updated": now_secs, "gates": gates });
        if let Err(e) = std::fs::write(&path, json.to_string()) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "action-fp state write failed");
        }
    }

    fn persist_fable_fp(&self) {
        let Some(path) = fable_fp_path() else { return };
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let gates: HashMap<&str, &str> = self
            .last_fable_fp
            .iter()
            .map(|(id, a)| (id.as_str(), a.fp.as_str()))
            .collect();
        let json = serde_json::json!({ "updated": now_secs, "gates": gates });
        if let Err(e) = std::fs::write(&path, json.to_string()) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "fable-fp state write failed");
        }
    }

    /// Persist the cap-banner fingerprint map (fp + wall-clock first-seen per
    /// session) so a daemon restart keeps knowing which banner content
    /// predates which headroom grant. Without this, every restart would reset
    /// first-seen to "now" and a standing stale banner would immediately eat
    /// a fresh grant again. WO#445.
    fn persist_cap_fp(&self) {
        let Some(path) = cap_fp_path() else { return };
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let caps: HashMap<&str, serde_json::Value> = self
            .last_cap_fp
            .iter()
            .map(|(id, c)| {
                (
                    id.as_str(),
                    serde_json::json!({ "fp": c.fp, "first_seen": c.first_seen_secs }),
                )
            })
            .collect();
        let json = serde_json::json!({ "updated": now_secs, "caps": caps });
        if let Err(e) = std::fs::write(&path, json.to_string()) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "cap-fp state write failed");
        }
    }

    /// Persist the non-pool re-page map (`{"pages": {id: {profile, paged_at}}}`)
    /// so a daemon bounce keeps knowing which parked non-pool caps were already
    /// paged — without it, every restart would re-page every standing non-pool
    /// cap immediately. WO#605.
    fn persist_non_pool_page(&self) {
        let Some(path) = non_pool_page_path() else {
            return;
        };
        let pages: HashMap<&str, serde_json::Value> = self
            .last_non_pool_page
            .iter()
            .map(|(id, p)| {
                (
                    id.as_str(),
                    serde_json::json!({ "profile": p.profile, "paged_at": p.paged_at_secs }),
                )
            })
            .collect();
        let json = serde_json::json!({ "updated": unix_secs(), "pages": pages });
        if let Err(e) = std::fs::write(&path, json.to_string()) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "non-pool-page state write failed");
        }
    }

    /// Persist the per-session ACTION_COOLDOWN map as wall-clock unix seconds
    /// (`{"actions": {id: acted_at_secs}}`) so a daemon bounce resumes the
    /// cooldown instead of resetting it — the WO#449 anti-bounce state.
    /// Monotonic `Instant`s can't be serialized, so each entry is converted to
    /// wall-clock by subtracting its elapsed age from now.
    fn persist_last_action(&self) {
        let Some(path) = last_action_path() else {
            return;
        };
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now_inst = Instant::now();
        let actions: HashMap<&str, u64> = self
            .last_session_action
            .iter()
            .map(|(id, acted)| {
                let age = now_inst.duration_since(*acted).as_secs();
                (id.as_str(), now_secs.saturating_sub(age))
            })
            .collect();
        let json = serde_json::json!({ "updated": now_secs, "actions": actions });
        if let Err(e) = std::fs::write(&path, json.to_string()) {
            tracing::warn!(target: "server.pane_watchdog", path = %path.display(), error = %e, "last-action state write failed");
        }
    }
}

/// Wall-clock unix seconds as f64, matching the py lanes' `time.time()`. Used
/// only to timestamp cross-surfacer ledger claims (MIT-1). Fail-safe: an
/// unresolvable clock -> 0.0 (a claim that instantly looks expired, i.e.
/// fail-open toward surfacing).
fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Wall-clock unix seconds for the capacity-state freshness gate. Fail-safe:
/// an unresolvable clock yields 0, which makes every positive claim look
/// stale, so the watchdog parks instead of moving.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the ACTION REQUIRED fingerprint state file: `AOE_ACTION_FP_FILE`
/// override, else `<app_dir>/action-fp-state.json`. `None` when no app dir is
/// resolvable (fail-open: the dampener degrades to in-memory-only).
fn action_fp_path() -> Option<PathBuf> {
    match std::env::var("AOE_ACTION_FP_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("action-fp-state.json")),
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "no app dir; action-fp state not persisted");
                None
            }
        },
    }
}

/// Load the persisted ACTION REQUIRED fingerprint map on daemon start. A
/// missing / unreadable / malformed file yields an empty map (fail-open).
/// `seen` is set to now so freshly-loaded entries get a full TTL window; they
/// are refreshed or aged out by subsequent ticks.
fn load_action_fp() -> HashMap<String, ActionFp> {
    let Some(path) = action_fp_path() else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    if serde_json::from_str::<serde_json::Value>(&raw).is_err() {
        tracing::warn!(target: "server.pane_watchdog", path = %path.display(), "action-fp state unparseable; starting empty");
    }
    parse_action_fp(&raw, Instant::now())
}

/// Resolve the ACTION_COOLDOWN persistence file: `AOE_LAST_ACTION_FILE`
/// override, else `<app_dir>/last-action-state.json`. `None` when no app dir
/// is resolvable (fail-open: the cooldown degrades to in-memory-only). WO#449.
fn last_action_path() -> Option<PathBuf> {
    match std::env::var("AOE_LAST_ACTION_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("last-action-state.json")),
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "no app dir; last-action state not persisted");
                None
            }
        },
    }
}

/// Parse the persisted `{"actions": {id: acted_at_secs}}` document back into
/// the in-memory cooldown map, backdating each entry by its wall-clock elapsed
/// so the remaining cooldown carries across a daemon bounce. Entries already
/// past [`ACTION_COOLDOWN`] (and non-numeric values) are dropped; malformed
/// input yields an empty map. Pure for testability. WO#449.
fn parse_last_action(raw: &str, now: Instant, now_secs: u64) -> HashMap<String, Instant> {
    let mut map = HashMap::new();
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else {
        return map;
    };
    let Some(actions) = val.get("actions").and_then(|a| a.as_object()) else {
        return map;
    };
    for (id, acted_at) in actions {
        let Some(acted_secs) = acted_at.as_u64() else {
            continue;
        };
        let elapsed = now_secs.saturating_sub(acted_secs);
        if elapsed >= ACTION_COOLDOWN.as_secs() {
            continue;
        }
        if let Some(backdated) = now.checked_sub(Duration::from_secs(elapsed)) {
            map.insert(id.clone(), backdated);
        }
    }
    map
}

/// Load the persisted ACTION_COOLDOWN map on daemon start. Missing /
/// unreadable / malformed file yields an empty map (fail-open — worst case is
/// one extra action, same as before WO#449, not a crash).
fn load_last_action() -> HashMap<String, Instant> {
    let Some(path) = last_action_path() else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    if serde_json::from_str::<serde_json::Value>(&raw).is_err() {
        tracing::warn!(target: "server.pane_watchdog", path = %path.display(), "last-action state unparseable; starting empty");
    }
    parse_last_action(&raw, Instant::now(), unix_secs())
}

/// Resolve the Fable-drift fingerprint state file: `AOE_FABLE_FP_FILE` override,
/// else `<app_dir>/fable-fp-state.json`. Separate file from the ACTION REQUIRED
/// dampener so the two never collide. WO d6bcae49.
fn fable_fp_path() -> Option<PathBuf> {
    match std::env::var("AOE_FABLE_FP_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("fable-fp-state.json")),
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "no app dir; fable-fp state not persisted");
                None
            }
        },
    }
}

/// Load the persisted Fable-drift fingerprint map on daemon start. Reuses the
/// `{"gates": {id: fp}}` shape and pure [`parse_action_fp`]; missing / malformed
/// yields an empty map (fail-open). WO d6bcae49.
fn load_fable_fp() -> HashMap<String, ActionFp> {
    let Some(path) = fable_fp_path() else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    parse_action_fp(&raw, Instant::now())
}

/// Resolve the cap-banner fingerprint state file: `AOE_CAP_FP_FILE` override,
/// else `<app_dir>/cap-fp-state.json`. Separate file from the other dampeners
/// because its entries carry a wall-clock first-seen, not just a fp. WO#445.
fn cap_fp_path() -> Option<PathBuf> {
    match std::env::var("AOE_CAP_FP_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("cap-fp-state.json")),
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "no app dir; cap-fp state not persisted");
                None
            }
        },
    }
}

/// Load the persisted cap-banner fingerprint map on daemon start via the pure
/// [`parse_cap_fp`]. Missing / malformed yields an empty map (fail-open: the
/// first post-restart sighting just re-dates first-seen to now). WO#445.
fn load_cap_fp() -> HashMap<String, CapFp> {
    let Some(path) = cap_fp_path() else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    parse_cap_fp(&raw, Instant::now())
}

/// Resolve the non-pool re-page dampener state file: `AOE_NON_POOL_PAGE_FILE`
/// override, else `<app_dir>/non-pool-page-state.json`. Separate file so it
/// never collides with the other dampeners. `None` -> in-memory-only (fail-open:
/// worst case is one extra page after a bounce, never a crash). WO#605.
fn non_pool_page_path() -> Option<PathBuf> {
    match std::env::var("AOE_NON_POOL_PAGE_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("non-pool-page-state.json")),
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "no app dir; non-pool-page state not persisted");
                None
            }
        },
    }
}

/// Load the persisted non-pool re-page map on daemon start via the pure
/// [`parse_non_pool_page`]. Missing / malformed yields an empty map (fail-open).
/// WO#605.
fn load_non_pool_page() -> HashMap<String, NonPoolPage> {
    let Some(path) = non_pool_page_path() else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    parse_non_pool_page(&raw)
}

/// Parse the persisted `{"gates": {id: fp}}` document into the in-memory map,
/// stamping every entry with `now` as its `seen`. Pure, so the restart-survival
/// round-trip is unit-tested without touching the filesystem. A malformed doc
/// yields an empty map. WO #139.
fn parse_action_fp(raw: &str, now: Instant) -> HashMap<String, ActionFp> {
    let mut map = HashMap::new();
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else {
        return map;
    };
    if let Some(gates) = val.get("gates").and_then(|g| g.as_object()) {
        for (id, fp) in gates {
            if let Some(fp) = fp.as_str() {
                map.insert(
                    id.clone(),
                    ActionFp {
                        fp: fp.to_string(),
                        seen: now,
                    },
                );
            }
        }
    }
    map
}

/// Urgent-flag TTLs for the attention.json mirror. Cap/auth match the
/// hook writers' 60-min ceiling (the watchdog re-stamps every tick while
/// blocked); overload is transient so it ages out fast after recovery.
const URGENT_TTL_BLOCKED: Duration = Duration::from_secs(60 * 60);
const URGENT_TTL_OVERLOAD: Duration = Duration::from_secs(5 * 60);

/// Mirror a pane signal into the instance's hook attention.json (red row in
/// the TUI). Fail-open: an unwritable status dir logs and skips.
///
/// Deliberately tmpfs-only; the durable record marker (`Instance::urgent_at`,
/// WO#1283A) is NOT stamped here. Watchdog urgents are TTL-scoped transients
/// (cap/auth/overload) that self-clear after recovery and are cleared by a
/// genuine Ben prompt, while the record marker is cleared only via
/// `PATCH /api/sessions/{id}/urgent`; stamping it per tick would pin rows
/// red past recovery and break the Ben-clears flow. Nothing is lost across
/// a restart or move: the watchdog re-detects a still-live condition from
/// pane text and re-stamps within one tick.
fn mirror_urgent(scan: &PaneScan, signal: PaneSignal) {
    let (kind, ttl, what) = match signal {
        PaneSignal::Capped => ("cap", URGENT_TTL_BLOCKED, "usage/session cap banner"),
        PaneSignal::DeviceCode => ("auth", URGENT_TTL_BLOCKED, "device-code sign-in prompt"),
        PaneSignal::Overloaded => ("overload", URGENT_TTL_OVERLOAD, "529 server overload"),
        PaneSignal::AuthLoss => ("auth", URGENT_TTL_BLOCKED, "account logged out"),
        // ACTION REQUIRED gates flow through the wake channel; the row-level
        // attention state for them stays owned by the worker's stop-hook.
        PaneSignal::ActionRequired => return,
    };
    let reason = format!("pane-watchdog: {} on '{}'", what, scan.title);
    if let Err(e) = crate::hooks::merge_watchdog_urgent(&scan.id, &reason, kind, ttl) {
        tracing::warn!(
            target: "server.pane_watchdog",
            session = %scan.id,
            error = %e,
            "urgent mirror write failed"
        );
    }
}

/// The highest meter reading per profile.
///
/// Every session on an account renders the SAME two numbers, so N panes are N
/// views of one fact. Panes disagree only when one is mid-refresh, and the
/// highest reading is the least stale of them; under-reporting here is the
/// failure that matters, because it is the one that arrives too late.
fn account_meters<'a>(
    panes: impl Iterator<Item = (&'a str, Option<crate::pane_rules::UsageMeter>)>,
) -> BTreeMap<String, crate::pane_rules::UsageMeter> {
    let mut out: BTreeMap<String, crate::pane_rules::UsageMeter> = BTreeMap::new();
    for (profile, meter) in panes {
        // A pane whose footer has scrolled off contributes nothing. Reading it
        // as zero would drag its account's number down.
        let Some(m) = meter else { continue };
        let slot = out.entry(profile.to_string()).or_insert(m);
        slot.five_hour_pct = slot.five_hour_pct.max(m.five_hour_pct);
        slot.weekly_pct = slot.weekly_pct.max(m.weekly_pct);
    }
    out
}

/// Accounts that crossed the threshold on THIS tick, as (profile, window, pct).
///
/// Edge-triggered per (account, window). A meter that is still high is not news
/// and re-announcing it every tick is how an alert becomes a stream nobody
/// reads. The latch clears when the reading falls back under, which is what a
/// window reset looks like, so the alarm re-arms for the next window instead of
/// warning once per daemon lifetime.
fn usage_alerts(
    meters: &BTreeMap<String, crate::pane_rules::UsageMeter>,
    threshold: u8,
    last: &mut HashMap<(String, &'static str), bool>,
) -> Vec<(String, &'static str, u8)> {
    let mut hits = Vec::new();
    for (profile, m) in meters {
        for (window, pct) in [("5h", m.five_hour_pct), ("wk", m.weekly_pct)] {
            let above = pct >= threshold;
            let key = (profile.clone(), window);
            let was = last.insert(key, above).unwrap_or(false);
            if above && !was {
                hits.push((profile.clone(), window, pct));
            }
        }
    }
    hits
}

/// The push-event kind for a tick's disposition, or `None` when nothing
/// happened that a subscriber needs to hear about.
///
/// The watchdog already knew about every one of these the instant it read the
/// pane. What it did with that knowledge was write a file and wait to be
/// asked, and when the asking stopped, the knowledge went nowhere. This
/// function is the other half: what it noticed, it now says out loud.
///
/// A page that a cooldown suppressed is still an event. The cooldowns govern
/// how often the COMMANDER is woken; they were never meant to decide whether
/// the fleet is allowed to know a session is capped.
fn event_kind(disp: &Disposition) -> Option<&'static str> {
    match disp {
        // Parked is the worse cap, not the quieter one: it means the daemon
        // found nowhere to move the session to.
        Disposition::LiveCap { .. } | Disposition::Parked { .. } => Some("cap"),
        Disposition::DeviceCode { .. } => Some("auth"),
        // Its own kind, never folded into `cap`: a subscriber that cannot tell
        // them apart waits for a reset that is never coming.
        Disposition::AuthLoss { .. } => Some("auth_loss"),
        Disposition::Overloaded => Some("overload"),
        Disposition::FableDrift { .. } => Some("model_drift"),
        Disposition::ActionGate {
            suppressed: None, ..
        } => Some("action_required"),
        // Nothing happened, or the watchdog already proved the matched text was
        // scrollback. Emitting these would teach every subscriber to filter us
        // out, which is how a notification rail dies without anyone noticing.
        Disposition::Serving
        | Disposition::ReplayedBanner { .. }
        | Disposition::ActionGate {
            suppressed: Some(_),
            ..
        } => None,
    }
}

/// Whether this tick is the EDGE into `state` for `id`, recording it either way.
///
/// A cap that is still a cap is not news. Re-announcing one every tick would
/// rebuild, on the push rail, the same unreadable flood that made the previous
/// surface easy to switch off.
fn is_event_edge(last: &mut HashMap<String, String>, id: &str, state: &str) -> bool {
    match last.get(id) {
        Some(prev) if prev == state => false,
        _ => {
            last.insert(id.to_string(), state.to_string());
            true
        }
    }
}

/// Run our own binary with the given args (the daemon-safe way to reuse the
/// full `session move` / `send` code paths, locks included).
async fn aoe_command(args: &[&str]) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let out = tokio::process::Command::new(exe)
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Like [`aoe_command`] but returns captured stdout, for read subcommands
/// (`list --all --json`) whose output the caller must parse.
async fn aoe_output(args: &[&str]) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let out = tokio::process::Command::new(exe)
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        anyhow::bail!(
            "exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// The fleet manager session's title. It runs under an AOE *account* profile
/// (aoe-wmw / aoe-fiw), never the daemon's default profile, so a plain
/// `aoe send AoE-Commander` resolves in the wrong profile and 404s. Every
/// escalation must resolve it cross-profile first.
const COMMANDER_TITLE: &str = "AoE-Commander";

/// The AoE-Commander's session id and the profile that owns it, needed to
/// target a cross-profile `aoe send -p <profile> <id>`.
struct CommanderTarget {
    id: String,
    profile: String,
}

/// Parse `aoe list --all --json` for the AoE-Commander's id and owning
/// profile. Pure, so the resolution is unit-tested without a live daemon.
/// Accepts either a bare array (the current CLI shape) or a `{"sessions":[…]}`
/// wrapper.
fn parse_commander_target(json: &str) -> Option<CommanderTarget> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let rows = value
        .as_array()
        .or_else(|| value.get("sessions").and_then(|s| s.as_array()))?;
    rows.iter().find_map(|r| {
        if r.get("title").and_then(|t| t.as_str()) != Some(COMMANDER_TITLE) {
            return None;
        }
        let id = r.get("id").and_then(|v| v.as_str())?.to_string();
        let profile = r.get("profile").and_then(|v| v.as_str())?.to_string();
        Some(CommanderTarget { id, profile })
    })
}

/// Resolve the AoE-Commander across all profiles. `None` on any failure
/// (daemon-safe: fail-open, the caller just logs a skipped escalation).
async fn resolve_commander() -> Option<CommanderTarget> {
    match aoe_output(&["list", "--all", "--json"]).await {
        Ok(out) => parse_commander_target(&out),
        Err(e) => {
            tracing::warn!(target: "server.pane_watchdog", error = %e, "commander resolve failed: aoe list --all --json");
            None
        }
    }
}

/// The activity class governing whether the watchdog may wake a human.
///
/// It governs the PAGE and nothing else. Detection still runs, the
/// classification row is still written, and the `/api/events` push still fires
/// with this off. That separation is the entire lesson of the incident this
/// gate comes from: a switch labelled as though it stops noticing, which
/// actually stops noticing, is how a fleet ends up blind and confident.
const PAGE_CLASS: &str = "pane_watchdog_page";

fn paging_allowed(activity: &crate::session::config::ActivityConfig) -> bool {
    activity.is_on(PAGE_CLASS)
}

/// Escalate through the operator's urgent-wake channel: the first
/// non-comment line of `~/.claude-urgent-wake-command` (override via
/// `URGENT_WAKE_COMMAND_FILE`) run through `bash -lc` with the context in
/// env vars. Falls back to messaging the AoE-Commander session directly
/// (resolved cross-profile, since it never runs under the daemon's default
/// profile).
async fn wake(kind: &str, session: &str, reason: String) {
    let activity = crate::session::config::Config::load_or_warn().activity;
    if !paging_allowed(&activity) {
        tracing::info!(
            target: "server.pane_watchdog",
            kind, session, %reason, class = PAGE_CLASS,
            "page withheld: activity class is off. Detection, the \
             classification row and the /api/events push are unaffected; \
             turn it on with `aoe activity pane_watchdog_page on`"
        );
        return;
    }
    let message = format!("URGENT [pane-watchdog] {kind}: {reason}");
    tracing::warn!(target: "server.pane_watchdog", kind, session, %reason, "escalating");

    let path = std::env::var("URGENT_WAKE_COMMAND_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".claude-urgent-wake-command")
        });
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Some(cmd) = content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let run = tokio::process::Command::new("bash")
                .args(["-lc", cmd])
                .env("URGENT_REASON", &reason)
                .env("URGENT_KIND", kind)
                .env("URGENT_SESSION", session)
                .env("URGENT_MESSAGE", &message)
                .output()
                .await;
            match run {
                Ok(out) if out.status.success() => return,
                Ok(out) => {
                    tracing::warn!(
                        target: "server.pane_watchdog",
                        status = %out.status,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "urgent wake command failed; falling back to Commander send"
                    );
                }
                Err(e) => {
                    tracing::warn!(target: "server.pane_watchdog", error = %e, "urgent wake command spawn failed; falling back to Commander send");
                }
            }
        }
    }

    match resolve_commander().await {
        Some(cmd) => {
            if let Err(e) = aoe_command(&["send", "-p", &cmd.profile, &cmd.id, &message]).await {
                tracing::warn!(
                    target: "server.pane_watchdog",
                    error = %e,
                    profile = %cmd.profile,
                    id = %cmd.id,
                    "fallback Commander send failed"
                );
            }
        }
        None => {
            tracing::warn!(
                target: "server.pane_watchdog",
                "fallback Commander send skipped: AoE-Commander not found across profiles"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capacity state with one fresh claim per entry: (profile, headroom).
    fn claims(entries: &[(&str, bool)]) -> capacity::CapacityState {
        claims_at(entries, TEST_NOW)
    }

    fn claims_at(entries: &[(&str, bool)], updated: u64) -> capacity::CapacityState {
        let mut state = capacity::CapacityState::default();
        for (profile, headroom) in entries {
            state.profiles.insert(
                (*profile).to_string(),
                capacity::ProfileCapacity {
                    headroom: *headroom,
                    cap_kind: None,
                    note: None,
                    reset_at: None,
                    updated,
                },
            );
        }
        state
    }

    const TEST_NOW: u64 = 1_800_000_000;

    // ── commander resolution: cross-profile Commander wake target ───────

    #[test]
    fn commander_resolves_from_bare_array() {
        // The real `aoe list --all --json` shape: a bare array where the
        // Commander sits on an AOE account profile, not the daemon default.
        let json = r#"[
            {"id":"aaaa1111","title":"for-Accountant","profile":"forit-main"},
            {"id":"e284618842464176","title":"AoE-Commander","profile":"aoe-wmw"},
            {"id":"bbbb2222","title":"for-QB-Connector","profile":"forit-backup"}
        ]"#;
        let t = parse_commander_target(json).expect("commander found");
        assert_eq!(t.id, "e284618842464176");
        assert_eq!(t.profile, "aoe-wmw");
    }

    #[test]
    fn commander_resolves_from_sessions_wrapper() {
        let json = r#"{"sessions":[{"id":"zzz","title":"AoE-Commander","profile":"aoe-fiw"}]}"#;
        let t = parse_commander_target(json).expect("commander found");
        assert_eq!(t.id, "zzz");
        assert_eq!(t.profile, "aoe-fiw");
    }

    #[test]
    fn commander_absent_returns_none() {
        let json = r#"[{"id":"aaaa","title":"for-Accountant","profile":"forit-main"}]"#;
        assert!(parse_commander_target(json).is_none());
    }

    #[test]
    fn commander_title_is_case_sensitive_exact() {
        // The cmdtop pin and every fleet lookup key on the exact title; a
        // near-miss must NOT resolve (else escalations target a decoy).
        let json = r#"[{"id":"x","title":"aoe-commander","profile":"aoe-wmw"}]"#;
        assert!(parse_commander_target(json).is_none());
    }

    #[test]
    fn commander_row_missing_profile_is_skipped() {
        // A malformed row without a profile cannot be targeted by
        // `send -p`; resolution must skip it rather than panic.
        let json = r#"[{"id":"x","title":"AoE-Commander"}]"#;
        assert!(parse_commander_target(json).is_none());
    }

    #[test]
    fn commander_parse_bad_json_returns_none() {
        assert!(parse_commander_target("not json at all").is_none());
    }

    // ── classify: real cap banners fire ────────────────────────────────

    #[test]
    fn cap_classic_usage_limit_banner() {
        let pane = "some earlier output\nClaude usage limit reached ∙ resets 3pm\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_weekly_limit_banner() {
        let pane = "You've hit your weekly limit · resets Jul 9 at 8am\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_session_limit_banner() {
        let pane = "  Session limit reached ∙ resets 11pm\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_options_modal_stop_and_wait() {
        // The cap modal renders selectable options; the selector glyph and
        // enumerator must not defeat the match.
        let pane = "\
Your limit will reset at 8pm.

 ❯ 1. Stop and wait for limit reset
   2. Switch to usage credits
";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_switch_to_team_plan_option() {
        let pane = "   2. Switch to Team plan for higher limits\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_out_of_usage_credits() {
        let pane =
            "You're out of usage credits\nBuy more credits or wait for your limit to reset\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_monthly_spend_limit_banner() {
        // Fable credit-out variant #1 (WO d6bcae49 / #102): monthly SPEND cap.
        let pane = "You've hit your monthly spend limit. Increase your limit in settings.\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_reached_model_limit_banner() {
        // Fable credit-out variant #3: per-MODEL cap — the model name varies
        // ("Fable 5", "Opus 4.8", …), so the rule must generalize.
        let pane = "You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_run_usage_credits_option_line() {
        // Fable cap modal variant: the recovery options render as standalone
        // lines WITHOUT the "You've reached your … limit" sentence (it may
        // have scrolled off). Each option line must carry the cap on its own.
        let pane = "\
 ❯ 1. Run /usage-credits to continue
   2. Wait for your limit to reset
";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_switch_models_with_model_option_line() {
        let pane = "   2. Switch models with /model\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_error_during_compaction_spend_limit() {
        // WO #362: a capacity-capped /compact renders the cap sentence behind
        // an "Error during compaction:" prefix. The anchored battery missed
        // it; 5 Fable sessions failed /compact silently on 2026-07-15.
        let pane = "⎿  Error during compaction: You've hit your monthly spend limit\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_error_during_compaction_model_limit() {
        let pane = "Error during compaction: You've reached your Fable 5 limit\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn cap_error_prefixed_out_of_usage_credits() {
        // The credit-out sentence behind a bare "Error:" prefix must fire in
        // the DEFAULT battery too; the fable battery only covers pinned
        // sessions and the capped ones were not pinned.
        let pane = "Error: out of usage credits\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn noise_error_during_compaction_other_reason_is_not_cap() {
        // The compaction-error prefix alone is not a cap; only a cap
        // sentence after it fires.
        let pane = "Error during compaction: request timed out\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn noise_quoted_cap_banner_is_not_cap() {
        // Sessions building this detector, and Commander WOs describing it,
        // quote the banner text. A quote character inside the leading
        // decoration is the template signature (same convention as the
        // action-required quoted-template guard); a real CLI banner is never
        // quote-wrapped. Without this guard the watchdog would auto-move the
        // very session working on the detector.
        for line in [
            "'Error during compaction: You've hit your monthly spend limit'\n",
            "- \"Error during compaction: You've reached your Fable 5 limit\"\n",
            "\u{2018}You've hit your monthly spend limit\u{2019} must fire, per the WO\n",
            "`Error: out of usage credits` is the third shape\n",
        ] {
            assert_eq!(
                classify_pane_tail(line),
                None,
                "quoted template text must not fire: {line:?}"
            );
        }
    }

    #[test]
    fn noise_prose_mentioning_usage_credits_is_not_cap() {
        // Mid-sentence prose discussing the /usage-credits command (e.g. a
        // session building this very detector) must not fire — line anchor.
        let pane = "the modal says run /usage-credits when capped, per the WO\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    // ── classify: cap NOISE must not fire ──────────────────────────────

    #[test]
    fn noise_fable_promo_blurb_is_not_cap() {
        // The Fable rollout promo mentions the weekly usage limit in prose.
        let pane = "\
Try Claude Fable 5 — our most capable model.
Fable responses use up to 50% of your plan's weekly usage limit.
> │
";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn noise_transient_rate_limit_429_is_not_cap() {
        let pane = "  ⎿  API Error (not your usage limit) · Rate limited — retrying in 8s\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn noise_reached_your_goal_prose_is_not_cap() {
        // "reached your …" prose without a limit/cap subject must not fire.
        let pane = "You've reached your goal for today; wrapping up the audit.\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn noise_mid_sentence_prose_mention_is_not_cap() {
        // Scrollback prose discussing caps (line-anchoring guard).
        let pane = "we saw the forit-main acct usage limit reached earlier, weekly cap applied\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn healthy_pane_is_none() {
        let pane = "✻ Thinking…\n  esc to interrupt\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    // ── classify: REPLAYED cap banners never fire (false-revoke fix) ────
    // A resumed/restarted pane replays old scrollback, including a cap
    // banner the account has since recovered from. Activity rendered BELOW
    // the banner (a tool call, a tool result, a running spinner) proves the
    // session is serving again and the banner is history, so it must not
    // classify as Capped; the watchdog would otherwise revoke headroom on
    // an actively serving account (forit-main + xce-main, 2026-07-15).

    #[test]
    fn replayed_cap_banner_above_tool_activity_is_none() {
        let pane = "\
Claude usage limit reached. Your limit will reset at 1:50am (America/Chicago).
⏺ Bash(git -C ~/GitProjects/per-dev status)
  ⎿  On branch main, nothing to commit
> │
";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn replayed_cap_banner_above_running_spinner_is_none() {
        let pane = "\
You've reached your Fable 5 limit · resets 1:50am
✻ Cerebrating… (esc to interrupt · 42s · 1.2k tokens)
";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn replayed_cap_banner_above_tool_result_is_none() {
        let pane = "\
Session limit reached ∙ resets 11pm
  ⎿  Read 214 lines
> │
";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn live_cap_banner_below_earlier_activity_still_fires() {
        // Activity ABOVE the banner is the normal shape of a genuinely live
        // cap: the session worked, then hit the wall. Only activity below
        // voids the banner.
        let pane = "\
⏺ Task(long research sweep)
  ⎿  Running…
Claude usage limit reached. Your limit will reset at 1:50am (America/Chicago).
> │
";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    #[test]
    fn live_cap_options_modal_still_fires() {
        // The cap options modal renders option lines below the banner; none
        // of that chrome is activity, so the banner stays live.
        let pane = "\
You've reached your Fable 5 limit · resets 1:50am
❯ 1. Stop and wait for limit to reset
  2. Switch to usage credits
> │
";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    // ── classify: device-code ──────────────────────────────────────────

    #[test]
    fn device_code_microsoft_devicelogin() {
        let pane = "\
To sign in, use a web browser to open https://microsoft.com/devicelogin
and enter the code H7Q2K9F4P to authenticate.
";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::DeviceCode));
    }

    #[test]
    fn device_code_github_flow() {
        let pane =
            "Open https://github.com/login/device\nFirst copy your one-time code: ABCD-EFGH\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::DeviceCode));
    }

    #[test]
    fn device_code_deep_in_scrollback_is_none() {
        // A devicelogin mention that scrolled well above the live prompt is
        // a stale/completed flow, not a live wait.
        let mut pane = String::from("go to microsoft.com/devicelogin and enter code\n");
        for i in 0..20 {
            pane.push_str(&format!("subsequent output line {i}\n"));
        }
        assert_eq!(classify_pane_tail(&pane), None);
    }

    // ── classify: server overload (529) ────────────────────────────────

    // Overload fires only while the CLI is still retrying, which renders the
    // running footer ("esc to interrupt") below the banner — the rule's
    // require_below guard. Each positive fixture carries that footer.

    #[test]
    fn overload_canonical_api_error_529_banner() {
        let pane = "  ⎿  API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\
                    ⏵⏵ bypass permissions on · esc to interrupt\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Overloaded));
    }

    #[test]
    fn overload_529_parenthesized_with_retry_tail() {
        let pane = "API Error (529 Overloaded) · Retrying in 4 seconds… (attempt 3/10)\n\
                    esc to interrupt\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Overloaded));
    }

    #[test]
    fn overload_raw_overloaded_error_type() {
        let pane = "  ⎿  {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\
                    ⏵⏵ bypass permissions on · esc to interrupt\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Overloaded));
    }

    #[test]
    fn overload_recovered_then_idle_is_none() {
        // The CLI finished past the 529 (turn ended, pane idle at the ready
        // prompt, footer without "esc to interrupt"). The banner sits in the
        // 8-line tail forever; before the require_below guard this re-fired
        // Overloaded every scan and kept the URGENT badge fresh past its TTL.
        let pane = "⏺ API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment.\n\
                    ✻ Crunched for 3m 22s\n\
                    ❯\n\
                    ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn overload_prose_mention_is_none() {
        // Prose about overload with no 529 / error-type shape never fires.
        let pane = "the api felt overloaded yesterday but recovered fine\n> │\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn overload_bare_529_number_is_none() {
        // A bare number is not an API error ("Processed 529 rows").
        let pane = "Processed 529 rows in 1.2s\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    #[test]
    fn overload_deep_in_scrollback_is_none() {
        // A recovered 529 buried under later output is a finished retry flow.
        let mut pane = String::from(
            "API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n",
        );
        for i in 0..20 {
            pane.push_str(&format!("subsequent output line {i}\n"));
        }
        assert_eq!(classify_pane_tail(&pane), None);
    }

    #[test]
    fn cap_wins_over_overload() {
        let pane = "API Error: 529 Overloaded\nSession limit reached ∙ resets 11pm\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    // ── classify: ACTION REQUIRED ───────────────────────────────────────

    #[test]
    fn action_required_gate_line() {
        let pane =
            "ACTION REQUIRED: Ben must approve the outward Christine send before I proceed.\n> │\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::ActionRequired));
    }

    #[test]
    fn action_required_bullet_prefixed() {
        let pane = "  ⏺ ACTION REQUIRED: device sign-in gate open\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::ActionRequired));
    }

    #[test]
    fn lowercase_action_required_prose_is_none() {
        let pane = "no action required here, the fix deployed cleanly\n";
        assert_eq!(classify_pane_tail(pane), None);
    }

    // ── classify: precedence ────────────────────────────────────────────

    #[test]
    fn cap_wins_over_action_required() {
        let pane = "ACTION REQUIRED: something\nSession limit reached ∙ resets 11pm\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Capped));
    }

    // ── WO#414: relocation requires VERIFIED headroom, never absence-of-cap ──

    #[test]
    fn empty_capacity_state_never_moves() {
        // THE thrash regression: the old selector treated "no cap observed"
        // as a target, so with no capacity data at all it moved sessions onto
        // accounts that were themselves capped. No claims -> no moves, park.
        assert_eq!(
            next_verified_headroom("forit-main", &capacity::CapacityState::default(), TEST_NOW),
            None
        );
    }

    #[test]
    fn fresh_claim_picked_in_draw_order() {
        let state = claims(&[("forit-backup", true), ("gna-main", true)]);
        assert_eq!(
            next_verified_headroom("forit-main", &state, TEST_NOW),
            Some("forit-backup".to_string())
        );
    }

    #[test]
    fn unclaimed_profiles_are_skipped() {
        // forit-backup has NO entry (unknown != headroom); gna-main has a
        // fresh positive claim, so the selector skips to it.
        let state = claims(&[("gna-main", true)]);
        assert_eq!(
            next_verified_headroom("forit-main", &state, TEST_NOW),
            Some("gna-main".to_string())
        );
    }

    #[test]
    fn revoked_claim_is_skipped() {
        let state = claims(&[("forit-backup", false), ("xce-main", true)]);
        assert_eq!(
            next_verified_headroom("forit-main", &state, TEST_NOW),
            Some("xce-main".to_string())
        );
    }

    #[test]
    fn stale_claim_is_skipped() {
        // A positive claim past HEADROOM_TTL_SECS no longer counts: the
        // Commander must re-probe before the profile re-enters the pool.
        let state = claims_at(
            &[("forit-backup", true)],
            TEST_NOW - capacity::HEADROOM_TTL_SECS - 1,
        );
        assert_eq!(next_verified_headroom("forit-main", &state, TEST_NOW), None);
    }

    #[test]
    fn non_pool_profile_never_moves() {
        let state = claims(&[("forit-main", true), ("forit-backup", true)]);
        assert_eq!(next_verified_headroom("aoe-wmw", &state, TEST_NOW), None);
        assert_eq!(
            next_verified_headroom("per-macbook", &state, TEST_NOW),
            None
        );
    }

    #[test]
    fn tail_profile_wraps_to_claimed_head() {
        // A session stranded on RAS-Main relocates back up to the head
        // profile once the head holds a fresh verified claim.
        let state = claims(&[("forit-main", true)]);
        assert_eq!(
            next_verified_headroom("RAS-Main", &state, TEST_NOW),
            Some("forit-main".to_string())
        );
    }

    #[test]
    fn current_profile_is_never_its_own_target() {
        // A fresh claim on the CURRENT profile must not produce a self-move.
        let state = claims(&[("forit-main", true)]);
        assert_eq!(next_verified_headroom("forit-main", &state, TEST_NOW), None);
    }

    // ── WO #362 + WO#414: a pool relocation pages the Commander with the
    // outcome AND the cap kind ──

    #[test]
    fn capped_move_reason_reports_outcome_and_kind() {
        let moved = capped_move_reason(
            "for-Support",
            "abc123",
            "gna-main",
            "forit-main",
            "fable-credit",
            true,
        );
        assert!(moved.contains("for-Support"));
        assert!(moved.contains("abc123"));
        assert!(moved.contains("gna-main"));
        assert!(moved.contains("[fable-credit]"));
        assert!(moved.contains("auto-moved to 'forit-main'"));
        assert!(moved.to_lowercase().contains("verify"));

        let failed = capped_move_reason(
            "for-Support",
            "abc123",
            "gna-main",
            "forit-main",
            "weekly",
            false,
        );
        assert!(failed.contains("FAILED"));
        assert!(failed.contains("[weekly]"));
        assert!(failed.contains("'forit-main'"));
        assert!(failed.to_lowercase().contains("manual"));
    }

    // ── WO #139: ACTION REQUIRED re-fire is content-gated, not time-gated ──

    #[test]
    fn action_wakes_only_on_fingerprint_change() {
        // First sighting (no prior fingerprint) → wake.
        assert!(action_should_wake(None, "fp-outward-email"));
        // Unchanged, already-surfaced gate → suppress the re-wake (this is
        // the flood the Commander reported: identical standing gates re-paging
        // every ACTION_COOLDOWN and on every daemon restart).
        assert!(!action_should_wake(
            Some("fp-outward-email"),
            "fp-outward-email"
        ));
        // A new/changed gate on the same session → wake immediately.
        assert!(action_should_wake(Some("fp-outward-email"), "fp-ramp-card"));
    }

    #[test]
    fn action_fp_survives_a_daemon_restart_round_trip() {
        // The amplifier the Commander reported: the fingerprint map was
        // in-memory only, so every daemon restart wiped it and re-paged every
        // standing gate at once. Persisted JSON must reload so a restarted
        // watchdog treats an unchanged gate as already-surfaced (no wake).
        let doc = serde_json::json!({
            "updated": 1_700_000_000u64,
            "gates": { "sess-abc": "fp-outward-email", "sess-def": "fp-ramp-card" }
        })
        .to_string();
        let loaded = parse_action_fp(&doc, Instant::now());
        assert_eq!(loaded.len(), 2);
        // The reloaded fingerprint suppresses the re-wake for the same gate.
        let last = loaded.get("sess-abc").map(|a| a.fp.as_str());
        assert!(!action_should_wake(last, "fp-outward-email"));
        // But a gate that changed while the daemon was down still wakes.
        assert!(action_should_wake(last, "fp-outward-email-v2"));
    }

    #[test]
    fn action_fp_malformed_state_loads_empty() {
        assert!(parse_action_fp("not json", Instant::now()).is_empty());
        assert!(parse_action_fp("{}", Instant::now()).is_empty());
        assert!(parse_action_fp(r#"{"gates":[]}"#, Instant::now()).is_empty());
    }

    // ── fable_scan_hit: the Fable-pinned GATE is the safety property ─────
    // WO d6bcae49. The watchdog only pages the Commander for model-drift on a
    // session that was LAUNCHED Fable-pinned; a non-Fable session running a
    // Sonnet/Opus subagent is normal fleet traffic and must stay silent.

    /// A synthesized bat-Summit-style pane: a Fable session that silently hit a
    /// Fable cap and got relaunched on Sonnet, then dispatched Sonnet subagents.
    const BAT_SUMMIT_TAIL: &str = "\
⏺ Heads up — I hit the Fable usage limit, so this session was relaunched on Sonnet.
⏺ Task(Investigate the pane watchdog)
  ⎿ Running subagent on claude-sonnet-5…
> │
";

    #[test]
    fn fable_scan_non_fable_session_never_fires() {
        // The gate: an Opus/Sonnet session showing the exact bat-Summit tail is
        // ordinary — it was never Fable, so a Sonnet subagent is not drift.
        assert_eq!(
            fable_scan_hit("--model claude-opus-4-8", BAT_SUMMIT_TAIL),
            None
        );
        assert_eq!(fable_scan_hit("", BAT_SUMMIT_TAIL), None);
    }

    #[test]
    fn fable_scan_fable_session_fires_on_bat_summit() {
        // A Fable-pinned session with the same tail IS drift → a (rule, fp) hit.
        let hit = fable_scan_hit("--model fable", BAT_SUMMIT_TAIL);
        assert!(hit.is_some(), "Fable-pinned bat-Summit pane must fire");
        let (rule, fp) = hit.unwrap();
        assert!(!rule.is_empty());
        assert!(
            !fp.is_empty(),
            "fingerprint must be non-empty for the dampener"
        );
    }

    #[test]
    fn fable_scan_fable_session_clean_pane_is_none() {
        // Acceptance: a clean Fable pane (healthy work, its own Fable subagents)
        // does NOT fire.
        let clean = "\
⏺ Task(Draft the quarterly summary)
  ⎿ Running subagent on claude-fable-5…
⏺ Done — the summary is ready for review.
> │
";
        assert_eq!(fable_scan_hit("--model claude-fable-5", clean), None);
    }

    #[test]
    fn fable_scan_does_not_fire_on_opus_code_literal() {
        // Must NOT fire on a Fable session that merely has the model ID as a code
        // literal (e.g. writing claude-api code) — no downgrade, no cap, no
        // non-Fable subagent dispatch.
        let code = "\
⏺ Wrote client.py with model=\"claude-opus-4-8\" and thinking adaptive.
⏺ The migration guide says to use sonnet only when the user asks.
> │
";
        assert_eq!(fable_scan_hit("--model fable", code), None);
    }

    #[test]
    fn fable_scan_hit_matches_direct_classify() {
        // The gated hit, once past the Fable-pin check, is exactly what the
        // ungated classifier returns for the same content.
        assert_eq!(
            fable_scan_hit("--model fable", BAT_SUMMIT_TAIL),
            classify_fable_drift(BAT_SUMMIT_TAIL)
        );
    }

    // ── WO#445: replayed-banner false-revoke gate ───────────────────────

    #[test]
    fn working_footer_at_live_edge_is_actively_working() {
        let pane = "\
⏺ earlier tool output
⏺ more output

✻ Cerebrating… (esc to interrupt · 42s · 1.2k tokens)

╭──────────────────────────╮
│ >                        │
╰──────────────────────────╯
  ⏵⏵ bypass permissions on
";
        assert!(pane_is_actively_working(pane));
    }

    #[test]
    fn idle_prompt_is_not_actively_working() {
        let pane = "\
⏺ Done — committed as cb5027b.

╭──────────────────────────╮
│ >                        │
╰──────────────────────────╯
  ⏵⏵ bypass permissions on
";
        assert!(!pane_is_actively_working(pane));
    }

    #[test]
    fn footer_deep_in_scrollback_is_not_actively_working() {
        // A replayed working footer buried above real output is history,
        // not evidence the account is serving now.
        let mut pane = String::from("✻ Cerebrating… (esc to interrupt · 42s)\n");
        for i in 0..12 {
            pane.push_str(&format!("⏺ output line {i}\n"));
        }
        assert!(!pane_is_actively_working(&pane));
    }

    #[test]
    fn cap_first_seen_first_sighting_is_now() {
        assert_eq!(cap_first_seen(None, "fp-a", TEST_NOW), TEST_NOW);
    }

    #[test]
    fn cap_first_seen_sticky_while_fp_unchanged() {
        assert_eq!(
            cap_first_seen(Some(("fp-a", TEST_NOW - 900)), "fp-a", TEST_NOW),
            TEST_NOW - 900
        );
    }

    #[test]
    fn cap_first_seen_resets_on_changed_fp() {
        assert_eq!(
            cap_first_seen(Some(("fp-a", TEST_NOW - 900)), "fp-b", TEST_NOW),
            TEST_NOW
        );
    }

    #[test]
    fn revoke_suppressed_while_profile_serving() {
        // Empirical serving beats a banner regardless of claim state: the
        // session is not blocked, so revoke+park would be wrong.
        assert_eq!(
            cap_revoke_suppressed(true, false, 0, TEST_NOW),
            Some("profile-serving")
        );
        assert_eq!(
            cap_revoke_suppressed(true, true, TEST_NOW - 300, TEST_NOW),
            Some("profile-serving")
        );
    }

    #[test]
    fn revoke_suppressed_for_pre_grant_banner() {
        // Banner content first seen at-or-before the verified grant: the
        // Commander granted with this very banner on screen (same-second
        // included — the xce-main re-revoke landed the same second as the
        // grant), so it is stale scrollback.
        assert_eq!(
            cap_revoke_suppressed(false, true, TEST_NOW, TEST_NOW),
            Some("pre-grant-banner")
        );
        assert_eq!(
            cap_revoke_suppressed(false, true, TEST_NOW, TEST_NOW - 300),
            Some("pre-grant-banner")
        );
    }

    #[test]
    fn new_banner_after_grant_still_revokes() {
        // A banner whose content changed AFTER the grant is a fresh cap
        // observation — observation beats claim (WO#414 invariant intact).
        assert_eq!(
            cap_revoke_suppressed(false, true, TEST_NOW - 300, TEST_NOW),
            None
        );
    }

    #[test]
    fn banner_without_verified_claim_still_revokes() {
        // No standing grant to protect → plain WO#414 behavior.
        assert_eq!(cap_revoke_suppressed(false, false, 0, TEST_NOW - 900), None);
    }

    #[test]
    fn parse_cap_fp_roundtrip() {
        let now = Instant::now();
        let raw =
            r#"{"updated":1800000000,"caps":{"sess-a":{"fp":"fp-a","first_seen":1799999000}}}"#;
        let map = parse_cap_fp(raw, now);
        let a = map.get("sess-a").expect("entry parsed");
        assert_eq!(a.fp, "fp-a");
        assert_eq!(a.first_seen_secs, 1_799_999_000);
        assert_eq!(a.seen, now);
    }

    #[test]
    fn parse_cap_fp_malformed_yields_empty() {
        assert!(parse_cap_fp("not json", Instant::now()).is_empty());
        assert!(parse_cap_fp(r#"{"caps": 42}"#, Instant::now()).is_empty());
        assert!(
            parse_cap_fp(r#"{"caps":{"sess-a":{"fp":7}}}"#, Instant::now()).is_empty(),
            "entry with non-string fp must be skipped"
        );
    }

    // ── WO#605: non-pool cap re-page dampener ───────────────────────────

    #[test]
    fn non_pool_should_page_dampens_standing_cap() {
        let cd = NON_POOL_PAGE_COOLDOWN.as_secs();
        let t0 = 1_000_000u64;
        // A first sighting always pages.
        assert!(
            non_pool_should_page(None, "aoe-wmw", t0),
            "first non-pool cap must page"
        );
        // The SAME non-pool profile within the cooldown is suppressed — this is
        // the WO#605 fix; the buggy always-page behaviour fails these two.
        assert!(
            !non_pool_should_page(Some(("aoe-wmw", t0)), "aoe-wmw", t0 + 60),
            "standing cap on the same profile just after a page must be suppressed"
        );
        assert!(
            !non_pool_should_page(Some(("aoe-wmw", t0)), "aoe-wmw", t0 + cd - 1),
            "still inside the cooldown window must be suppressed"
        );
        // Once the cooldown elapses on the same profile, it re-pages.
        assert!(
            non_pool_should_page(Some(("aoe-wmw", t0)), "aoe-wmw", t0 + cd),
            "cooldown elapsed on the same profile must re-page"
        );
        // Moving to a DIFFERENT non-pool profile is a state change -> pages now.
        assert!(
            non_pool_should_page(Some(("aoe-wmw", t0)), "aoe-fiw", t0 + 60),
            "a move to a different non-pool profile must page immediately"
        );
    }

    #[test]
    fn parse_non_pool_page_roundtrip() {
        let raw = r#"{"updated":1800000000,"pages":{"sess-a":{"profile":"aoe-wmw","paged_at":1799990000}}}"#;
        let map = parse_non_pool_page(raw);
        let a = map.get("sess-a").expect("entry parsed");
        assert_eq!(a.profile, "aoe-wmw");
        assert_eq!(a.paged_at_secs, 1_799_990_000);
    }

    #[test]
    fn parse_non_pool_page_malformed_yields_empty() {
        assert!(parse_non_pool_page("not json").is_empty());
        assert!(parse_non_pool_page(r#"{"pages": 42}"#).is_empty());
        assert!(
            parse_non_pool_page(r#"{"pages":{"sess-a":{"profile":7}}}"#).is_empty(),
            "entry with non-string profile must be skipped"
        );
    }

    // ── WO#449: anti-bounce cooldown survives a daemon bounce ───────────

    #[test]
    fn parse_last_action_round_trip_preserves_elapsed() {
        // A session acted on 60s before the daemon bounced must come back
        // with ~60s of its ACTION_COOLDOWN already spent, not a reset clock:
        // launchd KeepAlive respawns the daemon in ~1s, and a wiped map is
        // what let the for-tasks/for-Support pair re-move every bounce.
        let now = Instant::now();
        let raw = format!(
            r#"{{"updated":{TEST_NOW},"actions":{{"sess-fresh":{},"sess-expired":{}}}}}"#,
            TEST_NOW - 60,
            TEST_NOW - ACTION_COOLDOWN.as_secs() - 10,
        );
        let map = parse_last_action(&raw, now, TEST_NOW);
        let fresh = map.get("sess-fresh").expect("fresh entry kept");
        let elapsed = now.duration_since(*fresh).as_secs();
        assert!(
            (59..=61).contains(&elapsed),
            "backdated ~60s, got {elapsed}s"
        );
        assert!(
            !map.contains_key("sess-expired"),
            "entry past ACTION_COOLDOWN must be dropped on load"
        );
    }

    #[test]
    fn parse_last_action_malformed_yields_empty() {
        let now = Instant::now();
        assert!(parse_last_action("not json", now, TEST_NOW).is_empty());
        assert!(parse_last_action(r#"{"actions": 42}"#, now, TEST_NOW).is_empty());
        assert!(
            parse_last_action(r#"{"actions":{"sess-a":"soon"}}"#, now, TEST_NOW).is_empty(),
            "entry with non-numeric timestamp must be skipped"
        );
    }

    // ── WO#449: credits escalation only when ALL pool accounts probed capped ──

    #[test]
    fn all_pool_probed_capped_requires_every_profile_fresh_negative() {
        // True ONLY when every DRAW_ORDER profile holds a FRESH probed
        // negative claim. Absence of an observation, a positive claim, or a
        // stale negative all mean "not proven": the Commander must probe,
        // not surface a credits/money gate to Ben.
        let all_neg: Vec<(&str, bool)> = DRAW_ORDER.iter().map(|p| (*p, false)).collect();
        assert!(all_pool_probed_capped(&claims(&all_neg), TEST_NOW));

        let missing_one: Vec<(&str, bool)> = DRAW_ORDER[1..].iter().map(|p| (*p, false)).collect();
        assert!(
            !all_pool_probed_capped(&claims(&missing_one), TEST_NOW),
            "an unprobed profile is unknown, not capped"
        );

        let one_positive: Vec<(&str, bool)> = DRAW_ORDER
            .iter()
            .enumerate()
            .map(|(i, p)| (*p, i == 2))
            .collect();
        assert!(!all_pool_probed_capped(&claims(&one_positive), TEST_NOW));

        let mut mixed = claims(
            &DRAW_ORDER[1..]
                .iter()
                .map(|p| (*p, false))
                .collect::<Vec<_>>(),
        );
        mixed.profiles.extend(
            claims_at(
                &[(DRAW_ORDER[0], false)],
                TEST_NOW - capacity::HEADROOM_TTL_SECS - 1,
            )
            .profiles,
        );
        assert!(
            !all_pool_probed_capped(&mixed, TEST_NOW),
            "a stale negative probe is unknown again, not capped"
        );
    }

    #[test]
    fn parked_wake_distinguishes_probed_all_capped_from_unknown() {
        // Probed-all-capped is the ONLY parked state allowed to talk about
        // credits; the unknown-park wake must demand an empirical probe and
        // must never carry credits/money-gate language the Commander could
        // relay to Ben (the WO#449 phantom top-up pages).
        let (kind, reason) = parked_wake(true, "credit", "for-tasks", "381b98ed", "xce-main");
        assert_eq!(kind, "capped-all-accounts");
        assert!(reason.contains("ALL 7 pool accounts"), "{reason}");
        assert!(reason.to_lowercase().contains("probed"), "{reason}");

        let (kind, reason) = parked_wake(false, "credit", "for-tasks", "381b98ed", "xce-main");
        assert_eq!(kind, "capped-parked");
        assert!(reason.contains("PATCH /api/capacity"), "{reason}");
        assert!(
            reason.contains("do NOT surface a credits/money gate to Ben"),
            "{reason}"
        );
        let lower = reason.to_lowercase();
        assert!(
            !lower.contains("top up") && !lower.contains("out of credits"),
            "unknown-park wake must not carry credits language: {reason}"
        );
    }

    #[test]
    fn all_negative_claims_park() {
        // Every pool profile probed negative → no relocation target. The
        // selector parks; escalation shape is parked_wake's job.
        let all_neg: Vec<(&str, bool)> = DRAW_ORDER.iter().map(|p| (*p, false)).collect();
        let state = claims(&all_neg);
        for profile in DRAW_ORDER {
            assert_eq!(next_verified_headroom(profile, &state, TEST_NOW), None);
        }
    }

    // ── WO#450: per-tick classification, every live session gets one line ──

    fn disp_line(disp: &Disposition) -> String {
        class_line(
            TEST_NOW,
            "for-tasks",
            "381b98ed",
            "xce-main",
            "fable",
            disp,
            true,
        )
    }

    #[test]
    fn serving_session_logs_serving_no_page() {
        // Case (e): a healthy pane still gets a per-tick row, with no action.
        let disp = Disposition::Serving;
        assert_eq!(disp.state(), "SERVING");
        assert_eq!(disp.decision(), "none");
        let line = disp_line(&disp);
        for needle in [
            "for-tasks",
            "381b98ed",
            "xce-main",
            "fable",
            "SERVING",
            "none",
        ] {
            assert!(line.contains(needle), "line missing {needle}: {line}");
        }
    }

    #[test]
    fn live_cap_moved_pages_commander() {
        // Case (a): a live session-window cap that auto-moved must page.
        let disp = Disposition::LiveCap {
            kind: "session",
            action: CapAction::Moved {
                target: "forit-backup".into(),
                ok: true,
            },
        };
        assert_eq!(disp.state(), "LIVE-CAP-session");
        assert_eq!(disp.decision(), "page-commander");
        assert!(disp.reason().contains("forit-backup"), "{}", disp.reason());
        let line = disp_line(&disp);
        assert!(line.contains("LIVE-CAP-session"), "{line}");
        assert!(line.contains("page-commander"), "{line}");
    }

    #[test]
    fn live_cap_failed_move_still_pages() {
        let disp = Disposition::LiveCap {
            kind: "fable-credit",
            action: CapAction::Moved {
                target: "gna-main".into(),
                ok: false,
            },
        };
        assert_eq!(disp.state(), "LIVE-CAP-fable-credit");
        assert_eq!(disp.decision(), "page-commander");
        assert!(disp.reason().contains("FAILED"), "{}", disp.reason());
    }

    #[test]
    fn live_cap_under_cooldown_logs_but_holds() {
        // A still-capped pane inside ACTION_COOLDOWN keeps its LIVE-CAP state
        // in the log while taking no new action, so the row is auditable.
        let disp = Disposition::LiveCap {
            kind: "session",
            action: CapAction::Cooldown,
        };
        assert_eq!(disp.state(), "LIVE-CAP-session");
        assert_eq!(disp.decision(), "none");
        assert!(
            disp.reason().to_lowercase().contains("cooldown"),
            "{}",
            disp.reason()
        );
    }

    #[test]
    fn replayed_banner_logs_ignored_no_page() {
        // Case (b): a replayed credit-out banner (WO#445 suppression) is a
        // REPLAYED-banner-ignored row with decision none, never a page.
        for why in ["pre-grant-banner", "profile-serving"] {
            let disp = Disposition::ReplayedBanner { why };
            assert_eq!(disp.state(), "REPLAYED-banner-ignored");
            assert_eq!(disp.decision(), "none");
            assert!(disp.reason().contains(why), "{}", disp.reason());
            let line = disp_line(&disp);
            assert!(line.contains("REPLAYED-banner-ignored"), "{line}");
        }
    }

    #[test]
    fn fable_drift_logs_subagent_model_drift_and_pages() {
        // Case (d): a Fable-pinned session spawning a non-Fable subagent.
        let disp = Disposition::FableDrift {
            rule: "fable-subagent-model".into(),
            paged: true,
            why: "new drift fingerprint",
        };
        assert_eq!(disp.state(), "SUBAGENT-model-drift");
        assert_eq!(disp.decision(), "page-commander");
        assert!(
            disp.reason().contains("fable-subagent-model"),
            "{}",
            disp.reason()
        );
        // Each no-page path names itself in the reason so the classification
        // log distinguishes voided, deferred, and dampened holds.
        for why in [
            "contradicted by live serving or suppressed-banner evidence",
            "deferred to capacity sentinel, pool not proven dry",
            "already surfaced, unchanged fingerprint",
        ] {
            let held = Disposition::FableDrift {
                rule: "fable-subagent-model".into(),
                paged: false,
                why,
            };
            assert_eq!(held.state(), "SUBAGENT-model-drift");
            assert_eq!(held.decision(), "none");
            assert!(held.reason().contains(why), "{}", held.reason());
        }
    }

    #[test]
    fn admit_stale_banner_requires_pane_continuity() {
        // WO#1283 D1: the idle-composer layout is identical for replayed
        // scrollback and a live standing block, so admission runs on
        // temporal evidence alone. One row per evidence shape.
        let mem =
            |pid: Option<u32>, fp: Option<&str>, working: bool, profile: &str| PaneTickMemory {
                pane_pid: pid,
                cap_fp: fp.map(str::to_string),
                working,
                profile: profile.into(),
            };
        let cases: [(
            Option<PaneTickMemory>,
            Option<u32>,
            &str,
            &str,
            bool,
            Result<&str, &str>,
        ); 9] = [
            // Never seen this pane (fresh daemon): replayed scrollback is
            // indistinguishable, hold a tick.
            (
                None,
                Some(7),
                "fp-a",
                "p",
                false,
                Err("idle-composer-first-sighting"),
            ),
            // Pane pid changed: a respawned pane replays its predecessor's
            // scrollback.
            (
                Some(mem(Some(6), Some("fp-a"), false, "p")),
                Some(7),
                "fp-a",
                "p",
                false,
                Err("idle-composer-pane-changed"),
            ),
            // Pid unresolvable this tick: continuity unproven.
            (
                Some(mem(Some(7), Some("fp-a"), false, "p")),
                None,
                "fp-a",
                "p",
                false,
                Err("idle-composer-pane-changed"),
            ),
            // Session moved profiles between ticks: the banner belongs to
            // the account it was captured under, not the new one.
            (
                Some(mem(Some(7), Some("fp-a"), false, "old")),
                Some(7),
                "fp-a",
                "p",
                false,
                Err("idle-composer-pre-move"),
            ),
            // Same pane, no banner last tick: the banner just APPEARED at
            // an idle edge, which is exactly how a live block arrives.
            (
                Some(mem(Some(7), None, false, "p")),
                Some(7),
                "fp-a",
                "p",
                false,
                Ok("fresh-edge"),
            ),
            // Same banner stood across the window with the pane idle on
            // both sightings: a standing block, admit.
            (
                Some(mem(Some(7), Some("fp-a"), false, "p")),
                Some(7),
                "fp-a",
                "p",
                false,
                Ok("standing-idle"),
            ),
            // The pane SERVED during the window: the block it claims is
            // disproven, hold.
            (
                Some(mem(Some(7), Some("fp-a"), true, "p")),
                Some(7),
                "fp-a",
                "p",
                false,
                Err("idle-composer-serving-during-window"),
            ),
            (
                Some(mem(Some(7), Some("fp-a"), false, "p")),
                Some(7),
                "fp-a",
                "p",
                true,
                Err("idle-composer-serving-during-window"),
            ),
            // The banner text CHANGED under a stable pane: fresh content is
            // fresh evidence.
            (
                Some(mem(Some(7), Some("fp-old"), false, "p")),
                Some(7),
                "fp-a",
                "p",
                false,
                Ok("banner-changed"),
            ),
        ];
        for (prev, pid, fp, profile, working, expected) in &cases {
            assert_eq!(
                admit_stale_banner(prev.as_ref(), *pid, fp, profile, *working),
                *expected,
                "prev={:?} pid={pid:?} fp={fp} profile={profile} working={working}",
                prev.as_ref().map(|m| (
                    m.pane_pid,
                    m.cap_fp.as_deref(),
                    m.working,
                    m.profile.as_str()
                )),
            );
        }
    }

    #[test]
    fn effective_pin_args_prefers_session_model_flag() {
        // WO#1283 D1 (for-AVP silence): an unpinned session on a pinned
        // profile launches on the profile's model, so the profile pin is
        // the effective pin whenever the session's own args carry no
        // --model / -m flag.
        let cases = [
            (
                "--model claude-fable-5[1m]",
                Some("--model opus"),
                "--model claude-fable-5[1m]",
            ),
            (
                "--continue",
                Some("--model claude-fable-5[1m]"),
                "--model claude-fable-5[1m]",
            ),
            (
                "",
                Some("--model claude-fable-5[1m]"),
                "--model claude-fable-5[1m]",
            ),
            ("--continue", None, ""),
            ("", None, ""),
        ];
        for (session, profile, expected) in cases {
            assert_eq!(
                effective_pin_args(session, profile),
                expected,
                "{session:?} / {profile:?}"
            );
        }
    }

    #[test]
    fn parked_capped_session_logs_park() {
        // Case (f): capped with no verified-headroom target parks; the
        // unknown-pool shape must demand a probe, not assert a credits outage.
        let disp = Disposition::Parked {
            kind: "fable-credit",
            all_probed: false,
        };
        assert_eq!(disp.state(), "PARKED");
        assert_eq!(disp.decision(), "park");
        assert!(
            disp.reason().to_lowercase().contains("probe"),
            "{}",
            disp.reason()
        );

        let probed = Disposition::Parked {
            kind: "fable-credit",
            all_probed: true,
        };
        assert_eq!(probed.decision(), "park");
        assert!(
            probed.reason().to_lowercase().contains("all"),
            "{}",
            probed.reason()
        );
    }

    #[test]
    fn genuine_worker_gate_still_pages() {
        // ADDENDUM guard in the other direction: a real bottom-of-pane worker
        // gate keeps paging; the exemption is Commander/working-pane only.
        assert_eq!(action_page_suppressed("for-Migrator", false), None);
        let disp = Disposition::ActionGate {
            paged: true,
            suppressed: None,
        };
        assert_eq!(disp.state(), "ACTION-REQUIRED");
        assert_eq!(disp.decision(), "page-commander");
    }

    #[test]
    fn classification_json_carries_every_column() {
        let disp = Disposition::Serving;
        let row = classification_json(
            TEST_NOW,
            "for-tasks",
            "381b98ed",
            "xce-main",
            "fable",
            &disp,
            true,
        );
        assert_eq!(row["ts"], TEST_NOW);
        assert_eq!(row["title"], "for-tasks");
        assert_eq!(row["id"], "381b98ed");
        assert_eq!(row["profile"], "xce-main");
        assert_eq!(row["model"], "fable");
        assert_eq!(row["state"], "SERVING");
        assert_eq!(row["decision"], "none");
        assert!(row["reason"].is_string());
    }

    #[test]
    fn class_log_rotates_at_byte_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("watchdog-classifications.log");
        append_class_log(&path, "first line\n", 100);
        append_class_log(&path, "second line\n", 100);
        let filler = "x".repeat(80) + "\n";
        append_class_log(&path, &filler, 100);
        append_class_log(&path, "after rotate\n", 100);
        let rotated =
            std::fs::read_to_string(path.with_extension("log.1")).expect("rotated file exists");
        assert!(rotated.contains("first line"), "{rotated}");
        let live = std::fs::read_to_string(&path).expect("live file exists");
        assert!(live.contains("after rotate"), "{live}");
        assert!(
            !live.contains("first line"),
            "rotation must start a fresh file"
        );
    }

    // ── WO#450 ADDENDUM: Commander is EXEMPT from the action-required PAGE ──
    // Live repro 2026-07-17: the watchdog action-paged the Commander because
    // the Commander's OWN outward-comms escalation to Ben contained the
    // literal "ACTION REQUIRED (Ben): ..." (plus stop-hook injected guidance
    // quoting the phrase). WO#444 exempted only the BADGE path; the PAGE path
    // must use the same single is-commander source of truth (COMMANDER_TITLE).

    #[test]
    fn commander_action_gate_never_pages_reads_serving() {
        assert_eq!(
            action_page_suppressed(COMMANDER_TITLE, false),
            Some("commander-exempt")
        );
        // Even a working Commander pane resolves commander-first.
        assert_eq!(
            action_page_suppressed(COMMANDER_TITLE, true),
            Some("commander-exempt")
        );
        let disp = Disposition::ActionGate {
            paged: false,
            suppressed: Some("commander-exempt"),
        };
        assert_eq!(disp.state(), "SERVING");
        assert_eq!(disp.decision(), "none");
        assert_eq!(
            disp.reason(),
            "matched string is scrollback+hook-injection, not bottom-of-pane gate"
        );
    }

    #[test]
    fn commander_pane_with_action_required_ben_produces_no_page() {
        // The Commander repro content DOES match the action rule (it is a real
        // `^ACTION REQUIRED` line), proving suppression comes from the
        // commander exemption, not from the rule failing to fire.
        let pane = "\
⏺ Escalating to Ben now.
ACTION REQUIRED (Ben): approve the Ramp device login for gna-finance
> │
";
        assert_eq!(
            classify_pane_tail(pane),
            Some(PaneSignal::ActionRequired),
            "rule must still fire on the content; suppression is title-keyed"
        );
        assert_eq!(
            action_page_suppressed(COMMANDER_TITLE, false),
            Some("commander-exempt"),
            "the Commander session is never action-paged for its own text"
        );
    }

    #[test]
    fn actively_working_pane_action_gate_is_scrollback() {
        // ADDENDUM (2): "ACTION REQUIRED" text on a pane whose live edge shows
        // the running footer is scrollback, not a parked bottom-of-pane gate.
        assert_eq!(
            action_page_suppressed("for-Migrator", true),
            Some("pane-actively-working")
        );
        let disp = Disposition::ActionGate {
            paged: false,
            suppressed: Some("pane-actively-working"),
        };
        assert_eq!(disp.state(), "SERVING");
        assert_eq!(disp.decision(), "none");
    }

    // ── WO#450: zero-miss Fable-limit paraphrases + silent downgrade ────────

    #[test]
    fn fable_paraphrase_out_of_credits_alone_fires() {
        // A credit-out paraphrase with no "Fable" word in proximity still
        // means the Fable pool on a Fable-pinned session (zero-miss).
        let tail = "\
⏺ The request failed: you are out of usage credits.
> │
";
        assert!(
            fable_scan_hit("--model fable", tail).is_some(),
            "standalone credit-out must fire on a Fable-pinned session"
        );
        assert_eq!(
            fable_scan_hit("--model claude-opus-4-8", tail),
            None,
            "non-Fable sessions stay gated out"
        );
    }

    #[test]
    fn fable_paraphrase_reached_fable_limit_fires() {
        let tail = "\
⏺ You've reached your Fable limit for this billing period.
> │
";
        assert!(
            fable_scan_hit("--model fable", tail).is_some(),
            "generic 'Fable limit' phrasing must fire"
        );
    }

    #[test]
    fn fable_silent_downgrade_now_using_sonnet_fires() {
        // A bottom-of-pane "now using Sonnet" announcement has no strong
        // downgrade verb but is still a downgrade on a Fable-pinned session.
        let tail = "\
⏺ Model changed. Now using Sonnet 5 for this session.
> │
";
        assert!(
            fable_scan_hit("--model fable", tail).is_some(),
            "silent 'now using <model>' announcement must fire"
        );
    }

    #[test]
    fn fable_quoted_using_sonnet_does_not_fire() {
        // Quoted/reported prose describing the string is not a downgrade.
        let tail = "\
⏺ The hook docs say 'using sonnet' should be flagged by the watchdog.
> │
";
        assert_eq!(fable_scan_hit("--model fable", tail), None);
    }

    #[test]
    fn fable_limit_paraphrase_page_gated_on_live_evidence() {
        // WO#450 (3): the limit-paraphrase claims BLOCKAGE, so it is voided by
        // live serving evidence (working pane) or a WO#445-suppressed replayed
        // banner on the same pane. Drift rules that describe live work (a
        // non-Fable subagent, a downgrade announcement) are NOT voided by a
        // working pane, because the session is working on the wrong model.
        assert!(fable_page_suppressed("fable-limit-paraphrase", true, false));
        assert!(fable_page_suppressed("fable-limit-paraphrase", false, true));
        assert!(!fable_page_suppressed(
            "fable-limit-paraphrase",
            false,
            false
        ));
        assert!(!fable_page_suppressed("fable-subagent-model", true, false));
        assert!(!fable_page_suppressed("fable-downgrade-verb", true, false));
    }

    #[test]
    fn fable_blockage_defers_to_sentinel_unless_pool_all_dry() {
        // WO#535 (defect 2): blockage-class hits are cap signals the capacity
        // sentinel owns end to end (reroute plus its own single all-dry gate),
        // so the watchdog stays silent on them while any pool profile can still
        // serve. Only a fresh probed NEGATIVE on the whole pool, the case the
        // sentinel cannot place, pages the Commander.
        assert!(fable_blockage_defers_to_sentinel(
            "fable-limit-paraphrase",
            false
        ));
        assert!(fable_blockage_defers_to_sentinel("fable-credit-out", false));
        assert!(!fable_blockage_defers_to_sentinel(
            "fable-limit-paraphrase",
            true
        ));
        assert!(!fable_blockage_defers_to_sentinel("fable-credit-out", true));
        assert!(!fable_blockage_defers_to_sentinel(
            "fable-subagent-model",
            false
        ));
        assert!(!fable_blockage_defers_to_sentinel(
            "fable-downgrade-verb",
            false
        ));
    }

    #[test]
    fn test_the_meter_is_read_per_account_not_per_pane() {
        use crate::pane_rules::UsageMeter;
        let m = |a, b| {
            Some(UsageMeter {
                five_hour_pct: a,
                weekly_pct: b,
            })
        };
        // Fifteen live sessions share forit-main's ONE meter. Fifteen events
        // for one account is the flood that makes an alert unreadable, and the
        // account is the thing that actually runs out.
        let mut panes: Vec<(&str, Option<UsageMeter>)> =
            (0..15).map(|_| ("forit-main", m(87, 37))).collect();
        panes.push(("gna-main", m(12, 58)));
        // A pane whose footer has scrolled away contributes nothing rather
        // than a zero, which would drag the account's reading down.
        panes.push(("forit-main", None));

        let meters = account_meters(panes.iter().copied());
        assert_eq!(meters.len(), 2);
        assert_eq!(meters["forit-main"].five_hour_pct, 87);
        assert_eq!(meters["gna-main"].weekly_pct, 58);

        // Panes disagree when one is mid-refresh. The HIGHEST reading is the
        // least stale, and under-reporting here is the failure that matters.
        let disagreeing = [
            ("xce-main", m(40, 10)),
            ("xce-main", m(72, 9)),
            ("xce-main", m(58, 11)),
        ];
        let meters = account_meters(disagreeing.iter().copied());
        assert_eq!(meters["xce-main"].five_hour_pct, 72);
        assert_eq!(meters["xce-main"].weekly_pct, 11);
    }

    #[test]
    fn test_a_usage_threshold_alerts_once_per_account_per_window() {
        use crate::pane_rules::UsageMeter;
        let meters = |five, wk| {
            let mut m = BTreeMap::new();
            m.insert(
                "forit-main".to_string(),
                UsageMeter {
                    five_hour_pct: five,
                    weekly_pct: wk,
                },
            );
            m
        };
        let mut last = HashMap::new();

        // Below the line: silence. An alarm that fires early is one an
        // operator learns to dismiss.
        assert!(usage_alerts(&meters(79, 20), 80, &mut last).is_empty());

        // Crossing fires once, naming the window that crossed.
        let hits = usage_alerts(&meters(87, 20), 80, &mut last);
        assert_eq!(hits, vec![("forit-main".to_string(), "5h", 87)]);

        // Still above on the next tick, and the one after: nothing. This is
        // the difference between an alert and a stream.
        assert!(usage_alerts(&meters(88, 20), 80, &mut last).is_empty());
        assert!(usage_alerts(&meters(99, 20), 80, &mut last).is_empty());

        // The two windows are independent: weekly crossing while 5h is still
        // high is its own event, and its own problem.
        let hits = usage_alerts(&meters(99, 81), 80, &mut last);
        assert_eq!(hits, vec![("forit-main".to_string(), "wk", 81)]);

        // The 5h window resets, so the meter falls and the alarm re-arms. A
        // latch that never re-arms only ever warns once per daemon lifetime.
        assert!(usage_alerts(&meters(3, 82), 80, &mut last).is_empty());
        let hits = usage_alerts(&meters(90, 82), 80, &mut last);
        assert_eq!(hits, vec![("forit-main".to_string(), "5h", 90)]);

        // A configured threshold is honoured, not just the default.
        let mut fresh = HashMap::new();
        assert!(usage_alerts(&meters(50, 10), 95, &mut fresh).is_empty());
        assert_eq!(
            usage_alerts(&meters(96, 10), 95, &mut fresh),
            vec![("forit-main".to_string(), "5h", 96)]
        );
    }

    #[test]
    fn test_auth_loss_is_pushed_as_its_own_kind() {
        // Logged out is not capped. A cap has a reset time and a healthy
        // account; an auth loss has neither, and a subscriber that cannot tell
        // them apart will wait for a reset that is never coming.
        assert_eq!(
            event_kind(&Disposition::AuthLoss { paged: true }),
            Some("auth_loss")
        );
        assert_ne!(
            event_kind(&Disposition::AuthLoss { paged: false }),
            event_kind(&Disposition::LiveCap {
                kind: "usage",
                action: CapAction::Cooldown
            })
        );
        assert_eq!(Disposition::AuthLoss { paged: true }.state(), "AUTH-LOSS");
    }

    #[test]
    fn test_a_withheld_page_is_never_reported_as_a_page() {
        // The disposition computes what the rule WOULD do. When the class is
        // off, no row may claim a human was woken, because that sentence is
        // exactly what an operator reads to conclude somebody knows.
        let capped = Disposition::LiveCap {
            kind: "usage",
            action: CapAction::NonPool { paged: true },
        };
        assert_eq!(capped.decision(), "page-commander");
        assert_eq!(effective_decision(&capped, true), "page-commander");
        assert_eq!(
            effective_decision(&capped, false),
            "page-withheld-class-off"
        );
        assert!(effective_reason(&capped, true).ends_with("paged"));
        let withheld = effective_reason(&capped, false);
        assert!(withheld.contains("WITHHELD"), "{withheld}");
        // The correction has to say what still happened, or a reader takes it
        // for silence and goes looking for a poller that no longer exists.
        assert!(withheld.contains("/api/events"), "{withheld}");

        // A row that was never going to page reads identically either way: the
        // correction must not invent a suppression that did not occur.
        for disp in [
            Disposition::Serving,
            Disposition::Overloaded,
            Disposition::Parked {
                kind: "usage",
                all_probed: false,
            },
        ] {
            assert_eq!(effective_decision(&disp, false), disp.decision());
            assert_eq!(effective_reason(&disp, false), disp.reason());
        }
    }

    #[test]
    fn test_paging_is_governed_by_its_own_class() {
        use crate::session::config::ActivityConfig;

        let mut off = ActivityConfig::default();
        assert!(
            !paging_allowed(&off),
            "the class defaults off, so the watchdog must not page"
        );
        off.set(PAGE_CLASS, true);
        assert!(paging_allowed(&off));

        // `is_on` reads an unknown name as FALSE, so a typo in PAGE_CLASS would
        // mute the watchdog permanently while looking like a working gate.
        // Prove the name is real through the same name-keyed setter the CLI
        // uses, which reports an unknown class rather than accepting it.
        let mut probe = ActivityConfig::default();
        assert!(
            probe.set(PAGE_CLASS, true),
            "PAGE_CLASS is not a real activity class"
        );

        // Neighbouring classes must not open this one. `push_notify` in
        // particular reads like it would govern a page, and does not.
        let mut other = ActivityConfig::default();
        other.set("push_notify", true);
        other.set("session_auto_restart", true);
        assert!(!paging_allowed(&other));
    }

    #[test]
    fn test_event_kind_covers_every_disposition() {
        let cases: [(Disposition, Option<&str>); 11] = [
            // A cap is a cap whether the daemon could place the session or
            // not: parking is the WORSE outcome, so it must not be the quiet
            // one.
            (
                Disposition::LiveCap {
                    kind: "usage",
                    action: CapAction::Moved {
                        target: "gna-main".into(),
                        ok: true,
                    },
                },
                Some("cap"),
            ),
            (
                Disposition::LiveCap {
                    kind: "usage",
                    action: CapAction::Cooldown,
                },
                Some("cap"),
            ),
            (
                Disposition::Parked {
                    kind: "usage",
                    all_probed: true,
                },
                Some("cap"),
            ),
            (Disposition::DeviceCode { paged: true }, Some("auth")),
            (Disposition::Overloaded, Some("overload")),
            (
                Disposition::ActionGate {
                    paged: true,
                    suppressed: None,
                },
                Some("action_required"),
            ),
            (
                Disposition::FableDrift {
                    rule: "fable-credit-out".into(),
                    paged: false,
                    why: "already surfaced, unchanged fingerprint",
                },
                Some("model_drift"),
            ),
            // Nothing happened, or the watchdog already proved the text was
            // scrollback. Emitting these would teach subscribers to ignore us.
            (Disposition::Serving, None),
            (Disposition::ReplayedBanner { why: "stale" }, None),
            (
                Disposition::ActionGate {
                    paged: false,
                    suppressed: Some("commander-exempt"),
                },
                None,
            ),
            // A page suppressed by a cooldown is still a cap: the cooldown
            // governs how loudly the COMMANDER is woken, never whether the
            // event exists.
            (
                Disposition::LiveCap {
                    kind: "session",
                    action: CapAction::NonPool { paged: false },
                },
                Some("cap"),
            ),
        ];
        for (disp, expected) in cases {
            assert_eq!(event_kind(&disp), expected, "{}", disp.state());
        }
    }

    #[test]
    fn test_event_edge_fires_on_entry_not_every_tick() {
        let mut last: HashMap<String, String> = HashMap::new();
        // First sight of a state is news.
        assert!(is_event_edge(&mut last, "s1", "LIVE-CAP-usage"));
        // The same cap on the next tick is not. This is the whole difference
        // between a push rail and the flood that made the old one unreadable.
        assert!(!is_event_edge(&mut last, "s1", "LIVE-CAP-usage"));
        assert!(!is_event_edge(&mut last, "s1", "LIVE-CAP-usage"));
        // Recovery is a state change, so the edge fires and the record clears;
        // `event_kind` is what decides SERVING is not worth emitting.
        assert!(is_event_edge(&mut last, "s1", "SERVING"));
        // Capped again after recovering: news again, with no cooldown wait.
        assert!(is_event_edge(&mut last, "s1", "LIVE-CAP-usage"));
        // Sessions do not share an edge.
        assert!(is_event_edge(&mut last, "s2", "LIVE-CAP-usage"));
        assert!(!is_event_edge(&mut last, "s1", "LIVE-CAP-usage"));
        // A cap that escalates from placed to parked is a NEW state, and the
        // one a subscriber most needs: it means nowhere left to move.
        assert!(is_event_edge(&mut last, "s1", "PARKED"));
    }
}
