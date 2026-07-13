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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::file_watch::FileWatchService;
use crate::pane_rules::{self, CompiledRule};

use super::ben_gate_surface;
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
}

/// Map a rule's `kind` string (config-facing) to the watchdog's action
/// signal. Unknown kinds are rejected at spawn time.
fn kind_to_signal(kind: &str) -> Option<PaneSignal> {
    match kind {
        "cap" => Some(PaneSignal::Capped),
        "auth" => Some(PaneSignal::DeviceCode),
        "overload" => Some(PaneSignal::Overloaded),
        "action" => Some(PaneSignal::ActionRequired),
        _ => None,
    }
}

/// The built-in battery, compiled once. Classification for callers outside
/// the watchdog loop goes through this set, never through user config.
static DEFAULT_COMPILED: LazyLock<Vec<CompiledRule>> =
    LazyLock::new(|| pane_rules::compile(&pane_rules::default_rules()));

/// Classify a raw `capture-pane` tail against the built-in rules. Returns
/// the highest-priority signal (Capped > DeviceCode > Overloaded >
/// ActionRequired) or None for a healthy pane.
pub(crate) fn classify_pane_tail(raw: &str) -> Option<PaneSignal> {
    pane_rules::classify(raw, &DEFAULT_COMPILED).and_then(|r| kind_to_signal(&r.kind))
}

/// The hard account draw order for cap relocation. Sessions on profiles
/// outside this pool are never auto-moved (escalate only).
pub(crate) const DRAW_ORDER: [&str; 5] = [
    "forit-main",
    "forit-backup",
    "gna-main",
    "xce-main",
    "RAS-Main",
];

/// Pick the relocation target for a capped session: the first profile in
/// [`DRAW_ORDER`] that is not the session's current profile and not itself
/// capped. `None` when the session is not on a pool profile (gated / personal
/// accounts are never touched) or when every other pool profile is capped
/// (the ALL-CAPPED Ben-gate case).
pub(crate) fn next_uncapped(current: &str, capped: &HashSet<String>) -> Option<String> {
    let pos = DRAW_ORDER.iter().position(|p| *p == current)?;
    (1..DRAW_ORDER.len())
        .map(|i| DRAW_ORDER[(pos + i) % DRAW_ORDER.len()])
        .find(|cand| !capped.contains(*cand))
        .map(str::to_string)
}

/// How long an observed cap on a profile is trusted before it is assumed to
/// have reset, and also the floor between repeated ALL-CAPPED escalations.
const CAP_TTL: Duration = Duration::from_secs(60 * 60);

/// Floor between watchdog actions (move or wake) on the same session, so a
/// pane that stays blocked does not generate an action every tick.
const ACTION_COOLDOWN: Duration = Duration::from_secs(30 * 60);

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

struct PaneScan {
    id: String,
    title: String,
    profile: String,
    signal: Option<PaneSignal>,
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
}

/// Capture and classify every registered non-structured session's pane tail.
/// Blocking (tmux subprocesses + storage reads); run under `spawn_blocking`.
fn scan_panes(file_watch: &Arc<FileWatchService>, rules: &[CompiledRule]) -> Vec<PaneScan> {
    let instances = match super::load_all_instances(file_watch) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(target: "server.pane_watchdog", error = %e, "load_all_instances failed; skipping tick");
            return Vec::new();
        }
    };
    instances
        .iter()
        .filter(|inst| !inst.is_structured())
        .filter_map(|inst| {
            let sess = inst.tmux_session().ok()?;
            if !sess.exists() {
                return None;
            }
            let content = sess.capture_pane(60).ok()?;
            // classify_fp yields the winning rule AND its churn-stable content
            // fingerprint in one pass; keep the fp only for the ActionRequired
            // signal (the sole content-gated path, WO #139).
            let hit = pane_rules::classify_fp(&content, rules);
            let signal = hit.as_ref().and_then(|(r, _)| kind_to_signal(&r.kind));
            let action_fp = match (signal, &hit) {
                (Some(PaneSignal::ActionRequired), Some((_, fp))) => Some(fp.clone()),
                _ => None,
            };
            // Cross-surfacer claim key (MIT-1): prefer the labelled gate id from
            // the pane (shared across all four notifiers for an outward-comms
            // gate), else the watchdog's own session+fingerprint fallback.
            let surface_key = action_fp
                .as_ref()
                .map(|fp| match ben_gate_surface::extract_gate_id(&content) {
                    Some(g) => format!("gate:{g}"),
                    None => format!("sess:{}|fp:{}", inst.id, fp),
                });
            let drift = charter_drift::detect(&inst.title, &inst.project_path, &content);
            if signal.is_none() && drift.is_none() {
                return None;
            }
            Some(PaneScan {
                id: inst.id.clone(),
                title: inst.title.clone(),
                profile: inst.source_profile.clone(),
                signal,
                action_fp,
                surface_key,
                drift,
            })
        })
        .collect()
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
}

impl Watchdog {
    fn new(rules: Arc<Vec<CompiledRule>>) -> Self {
        Self {
            rules,
            capped_profiles: HashMap::new(),
            last_session_action: HashMap::new(),
            last_all_capped_wake: None,
            last_drift_wake: HashMap::new(),
            last_action_fp: load_action_fp(),
        }
    }

    async fn tick(&mut self, state: &Arc<super::AppState>) {
        let file_watch = state.file_watch.clone();
        let rules = self.rules.clone();
        let scans = match tokio::task::spawn_blocking(move || scan_panes(&file_watch, &rules))
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
        for scan in &scans {
            if scan.signal == Some(PaneSignal::Capped) {
                self.capped_profiles.insert(scan.profile.clone(), now);
            }
        }
        self.persist_cap_state();

        for scan in scans {
            if let Some(hit) = scan.drift.clone() {
                self.handle_drift(&scan, &hit, now).await;
            }
            let Some(signal) = scan.signal else { continue };
            // Mirror the observed block into the instance's attention.json so
            // the TUI/FleetView red row reflects pane truth without any
            // agent-side text scanning (the watchdog is the SOLE text
            // authority for urgency). Every tick refreshes the TTL while the
            // pane stays blocked; expiry clears it after recovery. Runs
            // BEFORE the action cooldown — the row must stay red even when
            // the escalation is rate-limited.
            mirror_urgent(&scan, signal);

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
                    } else {
                        tracing::debug!(
                            target: "server.pane_watchdog",
                            id = %scan.id,
                            "ACTION REQUIRED gate already surfaced by another notifier; cross-surfacer deduped (MIT-1)"
                        );
                    }
                }
                self.persist_action_fp();
                continue;
            }

            if let Some(last) = self.last_session_action.get(&scan.id) {
                if now.duration_since(*last) < ACTION_COOLDOWN {
                    continue;
                }
            }
            match signal {
                PaneSignal::Capped => self.handle_capped(&scan, now).await,
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
                }
                // Transient server-side overload: red row only (mirrored
                // above); relocation/wake would thrash on a condition that
                // clears itself.
                PaneSignal::Overloaded => {}
                // Handled by the content-gated branch above (which `continue`s
                // before reaching this match), so it is unreachable here.
                PaneSignal::ActionRequired => {}
            }
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

    async fn handle_capped(&mut self, scan: &PaneScan, now: Instant) {
        if !DRAW_ORDER.contains(&scan.profile.as_str()) {
            self.last_session_action.insert(scan.id.clone(), now);
            wake(
                "capped-non-pool",
                &scan.id,
                format!(
                    "session '{}' ({}) is capped on non-pool profile '{}'; not auto-moving",
                    scan.title, scan.id, scan.profile
                ),
            )
            .await;
            return;
        }
        let capped: HashSet<String> = self.capped_profiles.keys().cloned().collect();
        match next_uncapped(&scan.profile, &capped) {
            Some(target) => {
                self.last_session_action.insert(scan.id.clone(), now);
                tracing::warn!(
                    target: "server.pane_watchdog",
                    session = %scan.id,
                    title = %scan.title,
                    from = %scan.profile,
                    to = %target,
                    "capped session detected; relocating down the draw order"
                );
                match aoe_command(&["session", "move", &scan.id, &target]).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!(target: "server.pane_watchdog", session = %scan.id, error = %e, "session move failed");
                    }
                }
            }
            None => {
                let due = self
                    .last_all_capped_wake
                    .is_none_or(|t| now.duration_since(t) >= CAP_TTL);
                if due {
                    self.last_all_capped_wake = Some(now);
                    wake(
                        "all-capped",
                        &scan.id,
                        format!(
                            "ALL pool profiles are capped ({}); session '{}' ({}) is stranded — Ben-gate",
                            DRAW_ORDER.join(", "),
                            scan.title,
                            scan.id
                        ),
                    )
                    .await;
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
fn mirror_urgent(scan: &PaneScan, signal: PaneSignal) {
    let (kind, ttl, what) = match signal {
        PaneSignal::Capped => ("cap", URGENT_TTL_BLOCKED, "usage/session cap banner"),
        PaneSignal::DeviceCode => ("auth", URGENT_TTL_BLOCKED, "device-code sign-in prompt"),
        PaneSignal::Overloaded => ("overload", URGENT_TTL_OVERLOAD, "529 server overload"),
        // ACTION REQUIRED gates flow through the wake channel; the row-level
        // attention state for them stays owned by the worker's stop-hook.
        PaneSignal::ActionRequired => return,
    };
    let reason = format!("pane-watchdog: {} on '{}'", what, scan.title);
    if let Err(e) =
        crate::hooks::merge_watchdog_urgent(&scan.id, &reason, kind, ttl)
    {
        tracing::warn!(
            target: "server.pane_watchdog",
            session = %scan.id,
            error = %e,
            "urgent mirror write failed"
        );
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

/// Escalate through the operator's urgent-wake channel: the first
/// non-comment line of `~/.claude-urgent-wake-command` (override via
/// `URGENT_WAKE_COMMAND_FILE`) run through `bash -lc` with the context in
/// env vars. Falls back to messaging the AoE-Commander session directly
/// (resolved cross-profile, since it never runs under the daemon's default
/// profile).
async fn wake(kind: &str, session: &str, reason: String) {
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

    fn caps(profiles: &[&str]) -> HashSet<String> {
        profiles.iter().map(|s| s.to_string()).collect()
    }

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

    #[test]
    fn overload_canonical_api_error_529_banner() {
        let pane = "  ⎿  API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Overloaded));
    }

    #[test]
    fn overload_529_parenthesized_with_retry_tail() {
        let pane = "API Error (529 Overloaded) · Retrying in 4 seconds… (attempt 3/10)\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Overloaded));
    }

    #[test]
    fn overload_raw_overloaded_error_type() {
        let pane = "  ⎿  {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n";
        assert_eq!(classify_pane_tail(pane), Some(PaneSignal::Overloaded));
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
        let mut pane =
            String::from("API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n");
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

    // ── next_uncapped: draw order ───────────────────────────────────────

    #[test]
    fn draw_order_first_hop() {
        assert_eq!(
            next_uncapped("forit-main", &caps(&[])),
            Some("forit-backup".to_string())
        );
    }

    #[test]
    fn draw_order_skips_capped() {
        assert_eq!(
            next_uncapped("forit-main", &caps(&["forit-backup"])),
            Some("gna-main".to_string())
        );
    }

    #[test]
    fn draw_order_tier3_progression() {
        assert_eq!(
            next_uncapped("forit-backup", &caps(&["gna-main", "xce-main"])),
            Some("RAS-Main".to_string())
        );
    }

    #[test]
    fn non_pool_profile_never_moves() {
        assert_eq!(next_uncapped("aoe-wmw", &caps(&[])), None);
        assert_eq!(next_uncapped("per-macbook", &caps(&[])), None);
    }

    #[test]
    fn all_capped_returns_none() {
        assert_eq!(
            next_uncapped(
                "forit-main",
                &caps(&["forit-backup", "gna-main", "xce-main", "RAS-Main"])
            ),
            None
        );
    }

    #[test]
    fn tail_profile_can_fall_back_to_reset_head() {
        // Caps reset over time; a session stranded on RAS-Main may relocate
        // back up to a now-uncapped head profile.
        assert_eq!(
            next_uncapped("RAS-Main", &caps(&["forit-backup", "gna-main", "xce-main"])),
            Some("forit-main".to_string())
        );
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
}
