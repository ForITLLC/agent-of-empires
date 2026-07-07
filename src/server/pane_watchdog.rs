//! Continuous pane-content watchdog (INC-2026-07-06).
//!
//! The `/api/sessions` status pipeline reports what the status hooks and
//! spinner heuristics can see, but several operator-blocking states render
//! ONLY as pane text: the usage-cap options modal, a worker's `ACTION
//! REQUIRED` gate line, and device-code login prompts. All three reported
//! `Idle` while ~20 capped sessions and a gated worker sat undetected for
//! hours. This module is the daemon-side backstop: a supervised interval
//! task captures every registered session's pane tail, classifies it with a
//! noise-hardened regex battery, auto-relocates capped pool sessions down
//! the account draw order, and escalates everything else to the
//! AoE-Commander through the urgent-wake channel.
//!
//! Everything here is fail-open: a capture error, a failed move, or an
//! unwritable state file logs and skips; the daemon never crashes or stalls
//! on watchdog work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::file_watch::FileWatchService;

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

/// Cap-banner prefixes, matched line-anchored after enumerator/selector
/// stripping and lowercasing. Prefix (not substring) matching is what keeps
/// scrollback prose that merely *mentions* a limit from firing.
const CAP_PREFIXES: [&str; 12] = [
    "claude usage limit reached",
    "usage limit reached",
    "session limit reached",
    "5-hour limit reached",
    "weekly limit reached",
    "stop and wait for limit",
    "switch to usage credits",
    "switch to team plan",
    "out of usage credits",
    "you're out of usage credits",
    "you are out of usage credits",
    "your limit will reset",
];

/// Lines carrying these substrings are never a cap, whatever else they say:
/// the transient server-side 429 banner and the Fable promo blurb both talk
/// about usage limits without the account being capped.
const CAP_NEGATIVE_GUARDS: [&str; 2] = ["not your usage limit", "up to 50% of"];

/// Normalize one pane line for cap matching: drop the selector glyph and
/// other leading decoration, then a `1.` / `2)` option enumerator (digits
/// followed by `.` or `)` only, so `5-hour limit reached` survives), then
/// lowercase.
fn normalize_cap_line(line: &str) -> String {
    let trimmed = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = &trimmed[digits..];
    let deenumerated = if digits > 0 && (rest.starts_with('.') || rest.starts_with(')')) {
        rest[1..].trim_start()
    } else {
        trimmed
    };
    deenumerated.to_lowercase()
}

fn is_cap_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    if CAP_NEGATIVE_GUARDS.iter().any(|g| lower.contains(g)) {
        return false;
    }
    let norm = normalize_cap_line(line);
    if CAP_PREFIXES.iter().any(|p| norm.starts_with(p)) {
        return true;
    }
    // Generalized personal-cap sentence: "You've hit/reached your <X> limit"
    // where <X> varies freely (usage, weekly, monthly spend, "Fable 5",
    // "Opus 4.8", …) — new model names must not need a battery edit. Still
    // line-anchored (prefix) and still requires the word "limit", so prose
    // like "you've reached your goal" never fires.
    ["you've hit your", "you have hit your", "you've reached your", "you have reached your"]
        .iter()
        .any(|p| norm.starts_with(p))
        && norm.contains("limit")
}

/// A 529 server-overload line: the JSON error type, or a word-boundary
/// `529` either directly after an "API Error" banner or within 40 chars of
/// "overload". Plain prose about overload and bare numbers never fire.
/// Non-ASCII pane decoration (`⎿`, `·`) is squashed to spaces up front so
/// byte offsets are safe.
fn is_overload_line(line: &str) -> bool {
    let lower: String = line
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii() { c } else { ' ' })
        .collect();
    if lower.contains("overloaded_error") {
        return true;
    }
    let bytes = lower.as_bytes();
    let mut start = 0;
    while let Some(i) = lower[start..].find("529") {
        let idx = start + i;
        let end = idx + 3;
        let boundary = (idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric())
            && (end >= bytes.len() || !bytes[end].is_ascii_alphanumeric());
        if boundary {
            let prefix = lower[..idx].trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
            if prefix.ends_with("api error") {
                return true;
            }
            let lo = idx.saturating_sub(40);
            let hi = (end + 40).min(lower.len());
            if lower[lo..hi].contains("overload") {
                return true;
            }
        }
        start = end;
    }
    false
}

/// Classify a raw `capture-pane` tail. Returns the highest-priority signal
/// (Capped > DeviceCode > Overloaded > ActionRequired) or None for a healthy pane.
pub(crate) fn classify_pane_tail(raw: &str) -> Option<PaneSignal> {
    let stripped = crate::tmux::utils::strip_ansi(raw);
    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();

    let tail = |n: usize| &lines[lines.len().saturating_sub(n)..];

    if tail(30).iter().any(|l| is_cap_line(l)) {
        return Some(PaneSignal::Capped);
    }

    // Device-code prompts only count while they are still at the live edge of
    // the pane; a devicelogin URL buried under later output is a finished flow.
    let recent = tail(8).join("\n").to_lowercase();
    let device = recent.contains("microsoft.com/devicelogin")
        || recent.contains("/login/device")
        || recent.contains("first copy your one-time code")
        || recent.contains("to sign in, use a web browser")
        || (recent.contains("enter the code") && recent.contains("to authenticate"));
    if device {
        return Some(PaneSignal::DeviceCode);
    }

    // Overload only counts at the live edge too: a recovered 529 buried in
    // scrollback is a finished retry flow, not a blocked session.
    if tail(8).iter().any(|l| is_overload_line(l)) {
        return Some(PaneSignal::Overloaded);
    }

    let action_required = tail(15).iter().any(|l| {
        l.trim_start_matches(|c: char| !c.is_alphanumeric())
            .starts_with("ACTION REQUIRED")
    });
    if action_required {
        return Some(PaneSignal::ActionRequired);
    }

    None
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

/// Spawn the supervised watchdog interval task. No-op when disabled by env
/// or when the daemon is read-only (moving sessions and firing wakes are
/// writes in spirit).
pub(crate) fn spawn_pane_watchdog(state: Arc<super::AppState>) {
    if std::env::var("AOE_PANE_WATCHDOG_DISABLE").as_deref() == Ok("1") {
        tracing::info!(
            target: "server.pane_watchdog",
            "pane watchdog disabled via AOE_PANE_WATCHDOG_DISABLE=1"
        );
        return;
    }
    let secs = std::env::var("AOE_PANE_WATCHDOG_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s >= 10)
        .unwrap_or(180);
    let shutdown = state.shutdown.clone();
    crate::task_util::spawn_supervised(
        "server.pane_watchdog",
        crate::task_util::PanicPolicy::Log,
        async move {
            let mut watchdog = Watchdog::default();
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
    signal: PaneSignal,
}

/// Capture and classify every registered non-structured session's pane tail.
/// Blocking (tmux subprocesses + storage reads); run under `spawn_blocking`.
fn scan_panes(file_watch: &Arc<FileWatchService>) -> Vec<PaneScan> {
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
            let signal = classify_pane_tail(&content)?;
            Some(PaneScan {
                id: inst.id.clone(),
                title: inst.title.clone(),
                profile: inst.source_profile.clone(),
                signal,
            })
        })
        .collect()
}

#[derive(Default)]
struct Watchdog {
    /// Profiles observed capped, with when. Entries expire after [`CAP_TTL`]
    /// so a reset account re-enters the relocation pool without a restart.
    capped_profiles: HashMap<String, Instant>,
    /// Last watchdog action per session id ([`ACTION_COOLDOWN`]).
    last_session_action: HashMap<String, Instant>,
    /// Last ALL-CAPPED escalation, rate-limited to once per [`CAP_TTL`].
    last_all_capped_wake: Option<Instant>,
}

impl Watchdog {
    async fn tick(&mut self, state: &Arc<super::AppState>) {
        let file_watch = state.file_watch.clone();
        let scans = match tokio::task::spawn_blocking(move || scan_panes(&file_watch)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "pane scan task failed");
                return;
            }
        };

        let now = Instant::now();
        self.capped_profiles
            .retain(|_, seen| now.duration_since(*seen) < CAP_TTL);
        for scan in &scans {
            if scan.signal == PaneSignal::Capped {
                self.capped_profiles.insert(scan.profile.clone(), now);
            }
        }
        self.persist_cap_state();

        for scan in scans {
            // Mirror the observed block into the instance's attention.json so
            // the TUI/FleetView red row reflects pane truth without any
            // agent-side text scanning (the watchdog is the SOLE text
            // authority for urgency). Every tick refreshes the TTL while the
            // pane stays blocked; expiry clears it after recovery. Runs
            // BEFORE the action cooldown — the row must stay red even when
            // the escalation is rate-limited.
            mirror_urgent(&scan);

            if let Some(last) = self.last_session_action.get(&scan.id) {
                if now.duration_since(*last) < ACTION_COOLDOWN {
                    continue;
                }
            }
            match scan.signal {
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
                PaneSignal::ActionRequired => {
                    self.last_session_action.insert(scan.id.clone(), now);
                    wake(
                        "action-required",
                        &scan.id,
                        format!(
                            "session '{}' ({}) has an open ACTION REQUIRED gate",
                            scan.title, scan.id
                        ),
                    )
                    .await;
                }
            }
        }
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
}

/// Urgent-flag TTLs for the attention.json mirror. Cap/auth match the
/// hook writers' 60-min ceiling (the watchdog re-stamps every tick while
/// blocked); overload is transient so it ages out fast after recovery.
const URGENT_TTL_BLOCKED: Duration = Duration::from_secs(60 * 60);
const URGENT_TTL_OVERLOAD: Duration = Duration::from_secs(5 * 60);

/// Mirror a pane signal into the instance's hook attention.json (red row in
/// the TUI). Fail-open: an unwritable status dir logs and skips.
fn mirror_urgent(scan: &PaneScan) {
    let (kind, ttl, what) = match scan.signal {
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

/// Escalate through the operator's urgent-wake channel: the first
/// non-comment line of `~/.claude-urgent-wake-command` (override via
/// `URGENT_WAKE_COMMAND_FILE`) run through `bash -lc` with the context in
/// env vars. Falls back to messaging the AoE-Commander session directly.
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

    if let Err(e) = aoe_command(&["send", "AoE-Commander", &message]).await {
        tracing::warn!(target: "server.pane_watchdog", error = %e, "fallback Commander send failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(profiles: &[&str]) -> HashSet<String> {
        profiles.iter().map(|s| s.to_string()).collect()
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
}
