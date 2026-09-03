//! Shared session restart logic.
//!
//! Restarting a session re-runs the start cascade. For sandboxed sessions that
//! shells out to Docker (image pull with no built-in timeout, container
//! create/start) and runs the `before_start` host hook, any of which can block
//! for seconds. Running it on the TUI event loop froze the whole UI, so the TUI
//! drives this off the UI thread via `RestartPoller`, mirroring `StopPoller`.

use std::time::{Duration, Instant};

use crate::session::{Instance, StartOutcome, Status};

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
    // The built-in whose pane shapes and send timing apply to this session. A
    // custom wrapper of claude (`agent_detect_as`) parks on the same resume
    // picker claude does, so the wake is keyed on the resolved agent rather
    // than the raw tool name, the way capture and resume already are (#3715).
    let agent = instance
        .capture_agent_name()
        .map(str::to_string)
        .unwrap_or_else(|| instance.tool.clone());
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
    // rather than waiting out the pane-readiness probe and the guarded wake.
    let should_wake = should_send_restart_wake(&outcome);
    if should_wake && !wake_message.is_empty() {
        spawn_wake_worker(session_id.clone(), title, agent, wake_message);
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

/// Pane lines captured per wake poll: enough scrollback to see a transcript
/// echo of the wake above the input box without making each poll slow.
const WAKE_CAPTURE_LINES: usize = 200;
/// Full wake pastes the worker may send before parking the session.
const MAX_WAKE_SENDS: u32 = 3;
/// Enters the worker may press on Claude's resume picker before giving up.
const MAX_PICKER_DISMISSALS: u32 = 3;
/// After Enter on the picker, give Claude a beat to load the summary.
const PICKER_SETTLE: Duration = Duration::from_millis(700);
/// After a wake send (or a stuck-draft submit), give generation a chance to
/// start before the pane is re-read to verify it.
const WAKE_VERIFY: Duration = Duration::from_millis(1500);
/// Re-poll interval while the agent is still booting.
const BOOT_POLL: Duration = Duration::from_millis(250);
/// Hard wall-clock cap on the whole guarded wake, so a permanently stuck pane
/// cannot pin the worker thread.
const WAKE_DEADLINE: Duration = Duration::from_secs(20);

/// The single action the wake worker should take next, given the current pane
/// contents. Computed by [`classify_wake_pane`] so the decision is a pure
/// function that is unit-tested against pane captures, without a tmux pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeStep {
    /// The wake landed: the pane is generating, or the transcript shows the
    /// prompt consumed the wake since it was sent. Nothing left to do.
    Done,
    /// The pane is parked on Claude's resume-from-summary picker. Press Enter
    /// to select the highlighted (recommended) option, then re-evaluate. A
    /// wake *message* sent here would type into the menu instead of resuming.
    DismissPicker,
    /// The wake message is sitting unsubmitted in Claude's composer: the paste
    /// landed but the submitting Enter was swallowed by boot-time terminal-mode
    /// churn. Press a bare Enter to submit the existing draft. Re-sending the
    /// full message here would double the text in the composer.
    SubmitStuck,
    /// The agent is at an idle prompt, or, after a cross-account relocation,
    /// idle behind a *stale* prior-account usage-limit banner in scrollback.
    /// Send the wake. If the new account is genuinely capped, generation
    /// simply will not start and the send cap ends the attempt; the stale
    /// banner never blocks it.
    SendWake,
    /// The agent is still booting (no composer yet), or is showing a menu the
    /// worker must not type into and has no safe key for (the folder-trust
    /// dialog). Re-poll; the overall deadline bounds this.
    Wait,
}

/// Decide the next wake action from a pane capture. Pure and agent-aware so
/// the retry loop in [`run_guarded_wake`] stays trivial and the policy is
/// what the tests exercise.
///
/// Order matters. Claude's resume picker and a stuck draft are checked first
/// and explicitly: the manifest reads the picker as an idle prompt box, which
/// would otherwise route to `SendWake` and dump the wake text into the menu,
/// and a stuck draft is submitted rather than re-pasted even when a turn is
/// already generating. Then an actively-*Running* pane is `Done`. Then, once
/// something has been sent (`echo_baseline` is `Some`), a transcript that
/// carries one more echo of the wake than it did before the send is `Done`
/// too: Claude may have answered a short wake inside the verify window and be
/// idle again, and without the delta that pane would be re-woken up to the
/// cap. Only after those does readiness decide between `SendWake` and `Wait`.
fn classify_wake_pane(
    content: &str,
    agent: &str,
    wake_message: &str,
    echo_baseline: Option<usize>,
) -> WakeStep {
    use crate::tmux::status_detection as detect;

    let is_claude = agent == "claude";
    if is_claude {
        if detect::claude_pane_has_resume_picker(content) {
            return WakeStep::DismissPicker;
        }
        if detect::claude_message_stuck_in_composer(content, wake_message) {
            return WakeStep::SubmitStuck;
        }
    }
    if detect::detect_status_from_content(content, agent) == Status::Running {
        return WakeStep::Done;
    }
    if is_claude {
        let consumed = echo_baseline.is_some_and(|before| {
            detect::claude_submitted_message_count(content, wake_message) > before
        });
        if consumed {
            return WakeStep::Done;
        }
        if !detect::claude_pane_input_ready(content) {
            return WakeStep::Wait;
        }
    } else if let Some(marker) = crate::agents::ready_marker(agent) {
        // The same agent-declared readiness signal `aoe send` waits on,
        // matched the same way (`tmux::Session::wait_until_ready`).
        let clean = crate::tmux::utils::strip_ansi(content).to_lowercase();
        if !clean.contains(&marker.to_lowercase()) {
            return WakeStep::Wait;
        }
    }
    WakeStep::SendWake
}

/// Wait for the restarted pane to become live and past its boot shell, then
/// drive a *guarded auto-resume* ([`run_guarded_wake`]). Best-effort: a
/// failure to spawn or send is logged, never fatal.
fn spawn_wake_worker(session_id: String, title: String, agent: String, wake_message: String) {
    let spawn_result = std::thread::Builder::new()
        .name(format!("aoe-restart-wake/{}", session_id))
        .stack_size(256 * 1024)
        .spawn(move || {
            let Ok(tmux_session) = crate::tmux::Session::new(&session_id, &title) else {
                return;
            };
            // Phase 1: wait for the pane to be live and past its boot shell.
            let deadline = Instant::now() + Duration::from_millis(3000);
            loop {
                if !tmux_session.exists() {
                    return;
                }
                let pane_alive = !tmux_session.is_pane_dead();
                let hook_active = crate::hooks::read_hook_status(&session_id).is_some();
                if pane_alive && (hook_active || !tmux_session.is_pane_running_shell()) {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }

            // Phase 2: classify, act, verify, retry.
            run_guarded_wake(&tmux_session, &session_id, &agent, &wake_message);
        });
    if let Err(err) = spawn_result {
        tracing::warn!(target: "session.restart", ?err, "failed to spawn restart wake-up worker");
    }
}

/// The guarded auto-resume loop. This replaces a single-shot, send-and-forget
/// keystroke, which was silently swallowed whenever the agent was not at a
/// bare composer when it arrived: a cross-account `--resume` of a compacted
/// session parks on Claude's resume-from-summary picker (the keys went into
/// the menu), a boot that takes longer than the shell probe leaves the paste
/// rendered with its Enter eaten by terminal-mode churn (the message sat
/// unsubmitted while the row looked awake), and a relocated pane idle behind
/// the prior account's stale usage-limit banner read as capped when it was
/// simply never woken. Each pass re-reads the pane, so an action that did not
/// take is retried, bounded per action (`MAX_PICKER_DISMISSALS`,
/// `MAX_WAKE_SENDS`) and overall (`WAKE_DEADLINE`).
fn run_guarded_wake(
    tmux_session: &crate::tmux::Session,
    session_id: &str,
    agent: &str,
    wake_message: &str,
) {
    let delay = crate::agents::send_keys_enter_delay(agent);
    let deadline = Instant::now() + WAKE_DEADLINE;
    let mut wake_sends: u32 = 0;
    let mut stuck_submits: u32 = 0;
    let mut picker_dismissals: u32 = 0;
    // Transcript echoes of the wake as of the first send; only consulted for
    // claude (see `classify_wake_pane`).
    let mut echo_baseline: Option<usize> = None;
    let mut last_step: Option<WakeStep> = None;

    while Instant::now() < deadline {
        if !tmux_session.exists() {
            return;
        }
        let content = tmux_session
            .capture_pane(WAKE_CAPTURE_LINES)
            .unwrap_or_default();
        let step = classify_wake_pane(&content, agent, wake_message, echo_baseline);
        last_step = Some(step);
        match step {
            WakeStep::Done => {
                tracing::info!(
                    target: "session.restart",
                    session_id = %session_id,
                    wake_sends,
                    stuck_submits,
                    picker_dismissals,
                    "restart wake confirmed"
                );
                return;
            }
            WakeStep::Wait => std::thread::sleep(BOOT_POLL),
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
                // Bare Enter selects the highlighted (recommended) option; sent
                // raw so no message text leaks into the menu.
                if let Err(e) = tmux_session.send_raw_bytes(b"\r") {
                    tracing::warn!(
                        target: "session.restart",
                        session_id = %session_id,
                        "restart wake: failed to dismiss resume picker: {e}"
                    );
                }
                std::thread::sleep(PICKER_SETTLE);
            }
            WakeStep::SubmitStuck => {
                if stuck_submits >= MAX_WAKE_SENDS {
                    tracing::warn!(
                        target: "session.restart",
                        session_id = %session_id,
                        "restart wake: message still stuck in composer after {MAX_WAKE_SENDS} submits; parked"
                    );
                    return;
                }
                stuck_submits += 1;
                echo_baseline.get_or_insert_with(|| {
                    crate::tmux::status_detection::claude_submitted_message_count(
                        &content,
                        wake_message,
                    )
                });
                // The wake text already landed in the composer; only its Enter
                // was swallowed. Submit it with a bare Enter, never a re-paste,
                // which would double the message text.
                if let Err(e) = tmux_session.send_raw_bytes(b"\r") {
                    tracing::warn!(
                        target: "session.restart",
                        session_id = %session_id,
                        "restart wake: failed to submit stuck composer message: {e}"
                    );
                }
                std::thread::sleep(WAKE_VERIFY);
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
                echo_baseline.get_or_insert_with(|| {
                    crate::tmux::status_detection::claude_submitted_message_count(
                        &content,
                        wake_message,
                    )
                });
                if let Err(e) = tmux_session.send_keys_with_delay(wake_message, delay) {
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
        stuck_submits,
        picker_dismissals,
        ?last_step,
        "restart wake: deadline hit without confirmed wake"
    );
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
    // These lock in the policy that broke cross-account relocation: a pane
    // parked at the resume picker must be dismissed (not wake-typed into); a
    // pane showing a *stale* prior-account usage-limit banner must still get
    // a wake (the banner is not a live cap on the new account); a wake whose
    // Enter was swallowed is submitted, never re-pasted; only a genuinely
    // generating pane, or one that visibly consumed the wake, is Done.

    const WAKE: &str = "wake up: pick up what you were doing";

    /// Claude's resume-from-summary picker as it renders after a `--resume`
    /// boot of a compacted session.
    fn resume_picker_pane() -> &'static str {
        "\
 Resuming session 11111111-2222-3333-4444-555555555555

 How would you like to resume?

 ❯ 1. Resume from summary (recommended)
   2. Resume full session

 Press enter to confirm"
    }

    fn classify(pane: &str, tool: &str) -> WakeStep {
        classify_wake_pane(pane, tool, WAKE, None)
    }

    #[test]
    fn classify_dismisses_resume_picker_before_waking() {
        // Picker present -> Enter, NOT a wake message typed into the menu.
        assert_eq!(
            classify(resume_picker_pane(), "claude"),
            WakeStep::DismissPicker
        );
    }

    #[test]
    fn classify_sends_wake_through_stale_usage_limit_banner() {
        // The exact relocation failure: the pane sits idle after resume, with
        // the PRIOR account's limit banner still in scrollback above a live
        // composer. That cap is stale on the new account; we must still wake.
        let pane = "\
 Claude usage limit reached. Your limit will reset at 1pm (America/Chicago).

────────────────────────────────
 ❯ 
────────────────────────────────";
        assert_eq!(classify(pane, "claude"), WakeStep::SendWake);
    }

    #[test]
    fn classify_sends_wake_for_plain_idle_prompt() {
        let pane = "\
 Some earlier output from before the restart.

────────────────────────────────
 ❯ 
────────────────────────────────
   ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(classify(pane, "claude"), WakeStep::SendWake);
    }

    #[test]
    fn classify_done_when_pane_is_generating() {
        // A live interrupt hint == actively generating -> the wake took.
        let pane = "\
⏺ Picking up where I left off…

  s · ↓ 412 tokens · esc to interrupt";
        assert_eq!(classify(pane, "claude"), WakeStep::Done);
        let pane = "\
⏺ Working on the task now.

  ✻ Thinking… (esc to interrupt)";
        assert_eq!(classify(pane, "claude"), WakeStep::Done);
    }

    #[test]
    fn classify_waits_while_claude_is_still_booting() {
        // The version banner is up but no composer yet: a paste now renders
        // its text and loses its Enter. Wait, do not send.
        let pane = "\
 ▐▛███▜▌   Claude Code v2.1.197
▝▜█████▛▘  Sonnet 4.5 · Claude Max
  ▘▘ ▝▝    /home/user/project";
        assert_eq!(classify(pane, "claude"), WakeStep::Wait);
        assert_eq!(classify("", "claude"), WakeStep::Wait);
    }

    #[test]
    fn classify_waits_on_folder_trust_dialog_instead_of_typing_into_it() {
        // A numbered menu that is not the resume picker: never type the wake
        // into it, and there is no safe key to press for the user. Wait it out
        // (the worker's deadline bounds this).
        let pane = "\
 Do you trust the files in this folder?

 /home/user/project

 ❯ 1. Yes, I trust this folder
   2. No, exit";
        assert_eq!(classify(pane, "claude"), WakeStep::Wait);
    }

    #[test]
    fn classify_submits_stuck_wake_instead_of_repasting() {
        // The post-restart boot race: the first wake paste landed in the
        // composer but its submitting Enter was swallowed. Re-sending the FULL
        // message would double the text; the stuck draft gets a bare Enter.
        let pane = "\
────────────────────────────────
 ❯ wake up: pick up what you were doing
────────────────────────────────
   ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(classify(pane, "claude"), WakeStep::SubmitStuck);
    }

    #[test]
    fn classify_unrelated_composer_draft_is_not_stuck() {
        // A draft that is not our wake message must not draw a submitting
        // Enter (it would fire text this worker does not own); with the pane
        // otherwise idle the wake is sent normally.
        let pane = " ❯ some other half-typed draft\n   ⏵⏵ bypass permissions on";
        assert_eq!(classify(pane, "claude"), WakeStep::SendWake);
    }

    #[test]
    fn classify_done_when_the_wake_was_consumed_and_the_turn_already_finished() {
        // Verification after a send: Claude answered the wake inside the
        // verify window, so the pane is idle again. The transcript now carries
        // one more echo of the wake than it did before the send: consumed,
        // Done. Without the baseline delta this would re-send up to the cap.
        let pane = "\
> wake up: pick up what you were doing

⏺ Nothing was pending; standing by.

✻ Cooked for 2s
────────────────
 ❯ 
────────────────";
        assert_eq!(
            classify_wake_pane(pane, "claude", WAKE, Some(0)),
            WakeStep::Done
        );
    }

    #[test]
    fn classify_ignores_a_prior_restart_echo_already_in_scrollback() {
        // A `--resume` re-renders the tail of the transcript, which for a
        // routinely-restarted session includes an EARLIER restart's wake
        // echo. Only an echo that appeared after our send counts.
        let pane = "\
> wake up: pick up what you were doing

⏺ Earlier answer from the previous restart.

────────────────
 ❯ 
────────────────";
        // Before any send there is no baseline: the echo is history.
        assert_eq!(classify(pane, "claude"), WakeStep::SendWake);
        // After a send whose baseline already counted it: still not consumed.
        assert_eq!(
            classify_wake_pane(pane, "claude", WAKE, Some(1)),
            WakeStep::SendWake
        );
    }

    #[test]
    fn classify_ignores_claude_shapes_for_other_tools() {
        // The picker, composer and stuck-draft shapes are Claude-specific; a
        // codex pane that merely contains the words is not a dismissable menu
        // or a parked draft. With no live-run signal and no ready marker for
        // the tool it falls through to SendWake (the pre-existing default).
        assert_eq!(classify(resume_picker_pane(), "codex"), WakeStep::SendWake);
        let stuck = " ❯ wake up: pick up what you were doing";
        assert_eq!(classify(stuck, "codex"), WakeStep::SendWake);
        assert_eq!(classify("", "codex"), WakeStep::SendWake);
    }

    #[test]
    fn classify_waits_for_a_tool_ready_marker_when_one_is_known() {
        // opencode declares a ready marker (`AgentDef::ready_marker`); until
        // its input box shows it the TUI drops typed input on the floor.
        assert!(
            crate::agents::ready_marker("opencode").is_some(),
            "fixture invariant: opencode declares a ready marker"
        );
        let booting = "opencode v1.0\nloading…";
        assert_eq!(classify(booting, "opencode"), WakeStep::Wait);
        let ready = "┃ Ask anything…\n┗━━━━━━━━━━━━━";
        assert_eq!(classify(ready, "opencode"), WakeStep::SendWake);
    }

    #[test]
    fn wake_caps_are_bounded_and_nonzero() {
        // Evaluated at compile time: a cap of zero would never wake, an
        // unbounded cap would spam the pane, and a deadline shorter than the
        // verify windows would time out before the last allowed send.
        const {
            assert!(MAX_WAKE_SENDS >= 1 && MAX_WAKE_SENDS <= 5);
            assert!(MAX_PICKER_DISMISSALS >= 1 && MAX_PICKER_DISMISSALS <= 5);
            assert!(WAKE_VERIFY.as_millis() >= 500);
            assert!(
                WAKE_DEADLINE.as_millis() > WAKE_VERIFY.as_millis() * (MAX_WAKE_SENDS as u128 + 1)
            );
        }
    }
}
