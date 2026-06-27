//! Shared session restart logic.
//!
//! Restarting a session re-runs the start cascade. For sandboxed sessions that
//! shells out to Docker (image pull with no built-in timeout, container
//! create/start) and runs the `before_start` host hook, any of which can block
//! for seconds. Running it on the TUI event loop froze the whole UI, so the TUI
//! drives this off the UI thread via `RestartPoller`, mirroring `StopPoller`.

use crate::session::{Instance, StartOutcome};

pub struct RestartRequest {
    pub session_id: String,
    /// The instance to restart. `perform_restart` mutates it through the start
    /// cascade and hands the post-cascade snapshot back in `RestartResult`.
    pub instance: Instance,
    pub size: Option<(u16, u16)>,
    /// Keys to send once the pane is live again. Empty disables the wake-up
    /// (the documented opt-out via `session.restart_wake_message`).
    pub wake_message: String,
}

pub struct RestartResult {
    pub session_id: String,
    /// Pre-cascade snapshot used as a compare-and-swap baseline when merging
    /// peer-writable identity fields back into a live row.
    pub before: Box<Instance>,
    /// Post-cascade instance snapshot. Written back into the TUI's in-memory
    /// copy so `#[serde(skip)]` fields (e.g. `last_start_time`) and the
    /// cascade's mutations (cleared stale `agent_session_id`, container id)
    /// survive without a disk reload.
    pub instance: Box<Instance>,
    pub outcome: Result<StartOutcome, String>,
}

pub fn perform_restart(request: RestartRequest) -> RestartResult {
    let RestartRequest {
        session_id,
        mut instance,
        size,
        wake_message,
    } = request;

    let title = instance.title.clone();
    let tool = instance.tool.clone();
    let before = instance.clone();

    // Honor the same on_launch / before_start hook timeout the startup-recovery
    // worker installs (`run_recovery_for_instance`). Without it, a hanging
    // before_start hook (e.g. a `mint` script waiting on the network) runs with
    // no kill timer and wedges this serial worker thread forever, taking every
    // future restart down with it.
    let outcome = {
        let _scope = crate::session::recovery::HookTimeoutScope::new(
            crate::session::recovery::recovery_hook_timeout(),
        );
        instance.restart_with_size(size).map_err(|e| e.to_string())
    };

    // On a successful restart, send the wake-up keys on a detached thread so
    // the result (and the row's status update) propagate back immediately
    // rather than waiting out the up-to-3s pane-readiness probe.
    let should_wake = should_send_restart_wake(&outcome);
    if should_wake && !wake_message.is_empty() {
        spawn_wake_worker(session_id.clone(), title, tool, wake_message);
    }

    RestartResult {
        session_id,
        before: Box::new(before),
        instance: Box::new(instance),
        outcome,
    }
}

fn should_send_restart_wake(outcome: &Result<StartOutcome, String>) -> bool {
    matches!(
        outcome,
        Ok(StartOutcome::Fresh
            | StartOutcome::Resumed
            | StartOutcome::FreshAfterFailedResume { .. })
    )
}

/// The single action the wake worker should take next, given the current pane
/// contents. Computed by [`classify_wake_pane`] so the decision is a pure
/// function we can unit-test without a live tmux pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeStep {
    /// The pane is already generating (or for a non-resume start, the agent is
    /// otherwise live) — the wake landed. Nothing left to do.
    Done,
    /// The pane is parked on Claude's resume-from-summary picker. Press Enter to
    /// select the highlighted (recommended) option, then re-evaluate. A wake
    /// *message* sent here would type into the menu instead of resuming.
    DismissPicker,
    /// The pane is at an idle prompt — or, crucially, showing a *stale*
    /// prior-account usage-limit banner in scrollback after a cross-account
    /// relocation. Send the wake message to spool generation. If the new account
    /// is genuinely capped, generation simply won't start and we retry-then-give
    /// up; the stale banner never blocks the attempt.
    SendWake,
}

/// Decide the next wake action from a pane capture. Pure + tool-aware so the
/// retry loop in [`spawn_wake_worker`] stays trivial and the policy is tested.
///
/// Order matters: the resume picker is checked *first* and explicitly, because
/// `detect_status_from_content` already collapses the picker to `Waiting`
/// (alongside a real approval prompt and a stale limit banner) — and a `Waiting`
/// pane otherwise routes to `SendWake`, which would dump the wake text into the
/// menu. Only an actively-*Running* pane is `Done`; every other state (Idle,
/// or Waiting-because-stale-banner) sends the wake.
fn classify_wake_pane(content: &str, tool: &str) -> WakeStep {
    if tool == "claude" && crate::tmux::status_detection::claude_pane_has_resume_picker(content) {
        return WakeStep::DismissPicker;
    }
    match crate::tmux::status_detection::detect_status_from_content(content, tool) {
        crate::session::Status::Running => WakeStep::Done,
        _ => WakeStep::SendWake,
    }
}

/// Wait for the restarted pane to become live and past its boot shell, then
/// drive a *guarded auto-resume*: dismiss the resume picker if one is showing,
/// send the wake message, and verify the pane actually starts generating —
/// retrying a bounded number of times. This replaces the old single-shot,
/// send-and-forget keystroke, which left cross-account relocations parked dark
/// at the resume picker (or idle behind a stale usage-limit banner) because the
/// one wake keystroke fired into a not-yet-ready pane or into the picker menu
/// and was lost. Best-effort: a failure to spawn or send is logged, never fatal.
fn spawn_wake_worker(session_id: String, title: String, tool: String, wake_message: String) {
    let spawn_result = std::thread::Builder::new()
        .name(format!("aoe-restart-wake/{}", session_id))
        .stack_size(256 * 1024)
        .spawn(move || {
            let Ok(tmux_session) = crate::tmux::Session::new(&session_id, &title) else {
                return;
            };
            // Phase 1 — wait for the pane to be live and past its boot shell.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
            loop {
                if !tmux_session.exists() {
                    return;
                }
                let pane_alive = !tmux_session.is_pane_dead();
                let hook_active = crate::hooks::read_hook_status(&session_id).is_some();
                if pane_alive && (hook_active || !tmux_session.is_pane_running_shell()) {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            // Phase 2 — guarded auto-resume: classify, act, verify, retry.
            // After a cross-account boot Claude may still be loading well past the
            // 3s readiness wait, may present the resume-from-summary picker, or may
            // sit idle showing the prior account's stale cap banner. A single
            // keystroke can't survive any of those; this loop re-checks the pane
            // after each action so a wake that didn't take is resent.
            const WAKE_CAPTURE_LINES: usize = 200;
            const MAX_WAKE_SENDS: u32 = 3;
            const MAX_PICKER_DISMISSALS: u32 = 3;
            // After Enter on the picker, give Claude a beat to load the summary.
            const PICKER_SETTLE: std::time::Duration = std::time::Duration::from_millis(700);
            // After a wake send, give generation a chance to start before re-polling.
            const WAKE_VERIFY: std::time::Duration = std::time::Duration::from_millis(1500);
            // Hard wall-clock cap so a permanently-stuck pane can't pin the thread.
            let overall_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(20);

            let delay = crate::agents::send_keys_enter_delay(&tool);
            let mut wake_sends: u32 = 0;
            let mut picker_dismissals: u32 = 0;

            while std::time::Instant::now() < overall_deadline {
                if !tmux_session.exists() {
                    return;
                }
                let content = tmux_session
                    .capture_pane(WAKE_CAPTURE_LINES)
                    .unwrap_or_default();
                match classify_wake_pane(&content, &tool) {
                    WakeStep::Done => {
                        tracing::info!(
                            target: "session.restart",
                            session_id = %session_id,
                            wake_sends,
                            picker_dismissals,
                            "restart wake confirmed: pane is generating"
                        );
                        return;
                    }
                    WakeStep::DismissPicker => {
                        if picker_dismissals >= MAX_PICKER_DISMISSALS {
                            tracing::warn!(
                                target: "session.restart",
                                session_id = %session_id,
                                "restart wake: resume picker persisted after {MAX_PICKER_DISMISSALS} dismissals; giving up"
                            );
                            return;
                        }
                        picker_dismissals += 1;
                        // Bare Enter selects the highlighted (recommended) option;
                        // send raw so no message text leaks into the menu.
                        if let Err(e) = tmux_session.send_raw_bytes(b"\r") {
                            tracing::warn!(
                                target: "session.restart",
                                session_id = %session_id,
                                "restart wake: failed to dismiss resume picker: {e}"
                            );
                        }
                        std::thread::sleep(PICKER_SETTLE);
                    }
                    WakeStep::SendWake => {
                        if wake_sends >= MAX_WAKE_SENDS {
                            tracing::warn!(
                                target: "session.restart",
                                session_id = %session_id,
                                "restart wake: pane not generating after {MAX_WAKE_SENDS} wake sends; parked"
                            );
                            return;
                        }
                        wake_sends += 1;
                        if let Err(e) = tmux_session.send_keys_with_delay(&wake_message, delay) {
                            tracing::warn!(
                                target: "session.restart",
                                session_id = %session_id,
                                "restart wake: failed to send wake-up message: {e}"
                            );
                        }
                        std::thread::sleep(WAKE_VERIFY);
                    }
                }
            }
            tracing::warn!(
                target: "session.restart",
                session_id = %session_id,
                wake_sends,
                picker_dismissals,
                "restart wake: overall deadline hit without confirmed generation"
            );
        });
    if let Err(err) = spawn_result {
        tracing::warn!(target: "session.restart", ?err, "failed to spawn restart wake-up worker");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

    #[test]
    #[serial_test::serial]
    fn perform_restart_preserves_session_id_and_returns_instance() {
        let instance = test_instance();
        let id = instance.id.clone();
        let title = instance.title.clone();
        let result = perform_restart(RestartRequest {
            session_id: id.clone(),
            instance,
            size: None,
            wake_message: String::new(),
        });
        // The cascade may create a real tmux session; tear it down so the test
        // cleans up after itself.
        if let Ok(session) = crate::tmux::Session::new(&id, &title) {
            let _ = session.kill();
        }
        assert_eq!(result.session_id, id);
        assert_eq!(result.instance.id, id);
    }

    #[test]
    fn restart_wake_is_suppressed_for_resume_failed() {
        let outcome = Ok(StartOutcome::ResumeFailed {
            sid: "11111111-2222-3333-4444-555555555555".to_string(),
        });

        assert!(!should_send_restart_wake(&outcome));
    }

    // --- classify_wake_pane: the guarded auto-resume decision kernel ----------
    //
    // These are the selftest the WO asks for. They lock in the policy that broke
    // cross-account relocation: a pane parked at the resume picker must be
    // dismissed (not wake-typed into); a pane showing a *stale* prior-account
    // usage-limit banner must still get a wake (the banner is not a live cap on
    // the new account); only a genuinely-generating pane is Done.

    /// Claude's resume-from-summary picker as it renders after a `--resume` boot.
    fn resume_picker_pane() -> &'static str {
        "\
 Resuming session 11111111-2222-3333-4444-555555555555

 How would you like to resume?

 ❯ 1. Resume from summary (recommended)
   2. Resume full session

 Press enter to confirm"
    }

    #[test]
    fn classify_dismisses_resume_picker_before_waking() {
        // Picker present -> Enter, NOT a wake message typed into the menu.
        assert_eq!(
            classify_wake_pane(resume_picker_pane(), "claude"),
            WakeStep::DismissPicker
        );
    }

    #[test]
    fn classify_sends_wake_through_stale_usage_limit_banner() {
        // The exact relocation failure: the pane sits idle after resume, with the
        // PRIOR account's limit banner still in scrollback. detect_status sees a
        // limit banner -> Waiting, but that cap is stale; we must still wake.
        let pane = "\
 Claude usage limit reached. Your limit will reset at 1pm (America/Chicago).

> ";
        assert_eq!(classify_wake_pane(pane, "claude"), WakeStep::SendWake);
    }

    #[test]
    fn classify_sends_wake_for_plain_idle_prompt() {
        let pane = "\
 Some earlier output from before the restart.

> ";
        assert_eq!(classify_wake_pane(pane, "claude"), WakeStep::SendWake);
    }

    #[test]
    fn classify_done_when_pane_is_generating() {
        // A live token counter == actively generating -> the wake took, Done.
        let pane = "\
⏺ Picking up where I left off…

  s · ↓ 412 tokens · esc to interrupt";
        assert_eq!(classify_wake_pane(pane, "claude"), WakeStep::Done);
    }

    #[test]
    fn classify_done_when_esc_to_interrupt_present() {
        let pane = "\
⏺ Working on the task now.

  ✻ Thinking… (esc to interrupt)";
        assert_eq!(classify_wake_pane(pane, "claude"), WakeStep::Done);
    }

    #[test]
    fn classify_ignores_picker_detection_for_non_claude_tools() {
        // The picker is Claude-specific; a codex pane that merely contains the
        // words must not be treated as a dismissable menu. With no live-run
        // signal it falls through to SendWake (the safe default).
        assert_eq!(
            classify_wake_pane(resume_picker_pane(), "codex"),
            WakeStep::SendWake
        );
    }
}
