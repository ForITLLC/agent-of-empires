//! `agent-of-empires send` subcommand implementation

use anyhow::{bail, Result};
use clap::Args;

use crate::session::{EnsureReadyError, EnsureReadyOutcome, Storage};

#[derive(Args)]
pub struct SendArgs {
    /// Session ID or title
    identifier: String,

    /// Message to send to the agent
    message: String,

    /// Fail loud on dead/stopped sessions instead of auto-respawning. Default
    /// behavior is to revive the session so a `send` after a crash or stop
    /// just works; pass this for scripts that want the previous bail-out.
    #[arg(long = "no-revive")]
    no_revive: bool,

    /// Refuse (exit non-zero, 423-style) instead of queueing when the target
    /// composer holds an operator's unsent draft. By default the refused
    /// message is parked on the session's server-owned prompt queue and the
    /// daemon delivers it as its own turn once the composer clears; pass this
    /// for callers that want to decide for themselves.
    #[arg(long = "no-queue")]
    no_queue: bool,
}

#[tracing::instrument(target = "cli.send", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: SendArgs) -> Result<()> {
    // Without `-p` the target may be registered in any profile; find its
    // owner first (full id or unique title only — see `cli::resolve_scope`).
    let scope = super::resolve_scope(profile, &args.identifier)?;
    if scope.profile != profile {
        eprintln!(
            "  (session {} is registered in profile '{}')",
            scope.identifier, scope.profile
        );
    }
    let profile = scope.profile.as_str();
    let storage = Storage::open_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;

    if args.message.trim().is_empty() {
        bail!("Message cannot be empty");
    }

    let inst = super::resolve_session(&scope.identifier, &instances)?;
    let session_id = inst.id.clone();
    let session_title = inst.title.clone();
    let tool = inst.tool.clone();

    // Revive the pane if needed before delivering keystrokes. Without this,
    // a send to a dead pane silently writes to a corpse with no agent to
    // respond to it.
    if !args.no_revive {
        if let Some(target) = instances.iter_mut().find(|i| i.id == session_id) {
            match target.ensure_pane_ready() {
                Ok(EnsureReadyOutcome::Respawned) => {
                    eprintln!("  (respawned dead pane before send)");
                }
                Ok(EnsureReadyOutcome::Started) => {
                    eprintln!("  (started stopped session before send)");
                }
                Ok(EnsureReadyOutcome::ResumeFailed { sid }) => {
                    bail!("Resume failed for sid {sid}; preserved for explicit retry")
                }
                Ok(EnsureReadyOutcome::AlreadyAlive) => {}
                Err(EnsureReadyError::Transient(status)) => {
                    bail!("Session is mid-lifecycle ({status:?}); cannot send right now")
                }
                Err(EnsureReadyError::StructuredView) => {
                    bail!("Acp-mode sessions have no tmux pane; send is not supported")
                }
                Err(EnsureReadyError::Tmux(e)) => bail!("{}", e),
            }
        }
    }

    let tmux_session = crate::tmux::Session::new(&session_id, &session_title)?;
    if !tmux_session.exists() {
        bail!(
            "Session is not running. Start it first with: aoe session start {}",
            args.identifier
        );
    }

    // Wait for the pane to become ready before typing. A pane that exists
    // is not necessarily an agent that's finished booting: a session
    // started by an earlier, separate `aoe session start` reports
    // `EnsureReadyOutcome::AlreadyAlive` above with no wait at all, and
    // agents with no interposed shell (e.g. opencode) clear the pane's
    // "running a shell" check almost immediately even though their own TUI
    // can still take several more seconds to render and accept input. A
    // message typed into that window is silently dropped with no error.
    // Bounded so a genuinely busy/streaming agent doesn't block `send`
    // forever.
    tmux_session.wait_until_ready(
        std::time::Duration::from_secs(5),
        crate::agents::ready_marker(&tool),
    );

    let delay = crate::agents::send_keys_enter_delay(&tool);
    // Verified send: waits for the Claude composer, REFUSES if an operator's
    // unsent draft is parked in it (the refusal names the draft's size,
    // never its text, and exits non-zero), and confirms the Enter landed.
    // There is deliberately no --force: forcing would submit the human's
    // words under their name.
    if let Err(e) = tmux_session.send_keys_verified(&args.message, delay, &tool) {
        return match e.downcast::<crate::tmux::ParkedDraftRefusal>() {
            Ok(refusal) if !args.no_queue => {
                queue_behind_draft(
                    &storage,
                    &session_id,
                    &session_title,
                    &args.message,
                    &refusal,
                )
                .await
            }
            Ok(refusal) => Err(refusal.into()),
            Err(e) => Err(e),
        };
    }
    // Delivered: acknowledge the hook-written urgent flag (sticky kinds
    // survive machine traffic — see `hooks::ack_hook_urgent_on_send`).
    let urgent_ack = crate::hooks::ack_hook_urgent_on_send(&session_id, &args.message);
    tracing::debug!(session = %session_id, ack = urgent_ack.as_str(), "send: urgent ack");

    // Stamp last_accessed_at so the "last activity" column reflects user
    // interaction, and remap the status to Running. The agent has just been
    // given fresh input; the next status poll will reconcile the real state,
    // but flipping to Running immediately keeps the row from sticking on a
    // stale Idle/Waiting label during the gap between send and poll.
    // `touch_last_accessed` also auto-clears `archived_at` and `snoozed_until`
    // (see Instance::touch_last_accessed), so a user can wake any sunk row by
    // sending to it.
    let id_for_save = session_id.clone();
    if let Err(err) = storage.update(|instances, _groups| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_save) {
            inst.touch_last_accessed();
            inst.status = crate::session::Status::Running;
        }
        Ok(())
    }) {
        // The tmux send succeeded; the storage write is best-effort
        // bookkeeping (status remap + auto-unarchive). Surfacing this as a
        // hard error would tell the user "send failed" when the message
        // actually reached the agent, so log a warning and keep the success
        // line. The next status poll will reconcile the row anyway.
        tracing::warn!(
            ?err,
            "send: failed to persist status remap after successful send"
        );
    }

    println!("Sent message to '{}'", session_title);
    Ok(())
}

/// The verified send refused: a human's unsent draft is parked in the
/// target composer. Park the message on the session's server-owned prompt
/// queue instead of dropping it. The daemon's `server::send_queue` drain
/// delivers it as its own turn the moment the composer clears, and the row
/// lives in the profile's `sessions.json`, so it survives a daemon restart.
/// Daemon-first because the API owns the queue; without a reachable daemon
/// the row is written to disk directly and the next `aoe serve` delivers
/// it. Exit 0: the message was accepted, not delivered, and the printed
/// line says so.
async fn queue_behind_draft(
    storage: &Storage,
    session_id: &str,
    session_title: &str,
    message: &str,
    refusal: &crate::tmux::ParkedDraftRefusal,
) -> Result<()> {
    let qid = new_queue_id();
    let sender = sender_label();
    let position = match daemon_enqueue(session_id, &qid, message, &sender).await? {
        Some(position) => position,
        None => {
            let position = disk_enqueue(storage, session_id, &qid, message, &sender)?;
            eprintln!("  (no daemon reachable; queued on disk, delivered once `aoe serve` runs)");
            position
        }
    };
    println!("{}", queued_line(&qid, position, session_title, refusal));
    Ok(())
}

/// `send-` + 12 hex: short enough to type into `aoe session queue drop`.
fn new_queue_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("send-{}", &raw[..12])
}

/// The one line a caller greps for. Draft SIZE only, never its text.
fn queued_line(
    qid: &str,
    position: usize,
    title: &str,
    refusal: &crate::tmux::ParkedDraftRefusal,
) -> String {
    format!(
        "queued behind operator draft (qid {qid}, position {position}) for '{title}': \
         {} chars / {} lines parked in its composer; the daemon delivers this message \
         as its own turn once the composer is clear (aoe session queue {title})",
        refusal.chars, refusal.lines
    )
}

/// Who is waiting, for `aoe session queue`: the aoe session this command
/// runs inside when there is one, else a bare `aoe send`.
fn sender_label() -> String {
    let inside = std::env::var("TMUX_PANE")
        .ok()
        .and_then(|_| crate::tmux::get_current_session_name())
        .and_then(|tmux_name| {
            let profiles = crate::session::list_profiles().ok()?;
            profiles.iter().find_map(|p| {
                let instances = Storage::open_unwatched(p).ok()?.load().ok()?;
                instances
                    .iter()
                    .find(|i| crate::tmux::agent_session_belongs_to(&tmux_name, &i.id))
                    .map(|i| i.title.clone())
            })
        });
    match inside {
        Some(title) => format!("aoe send from {title}"),
        None => "aoe send".to_string(),
    }
}

/// Park the row through the daemon. `Ok(None)` when no daemon is reachable
/// (the caller falls back to disk); `Err` when the daemon answered and
/// refused (queue full, session unknown) — that refusal is the answer, the
/// disk must not be used to route around it.
async fn daemon_enqueue(
    session_id: &str,
    qid: &str,
    message: &str,
    sender: &str,
) -> Result<Option<usize>> {
    use crate::acp::client::{discovery, HttpClient, HttpError};
    let Ok(endpoint) = discovery::discover_local() else {
        return Ok(None);
    };
    let Ok(client) = HttpClient::new(endpoint) else {
        return Ok(None);
    };
    let entry = match client
        .queue_enqueue(session_id, qid, message, Some(sender))
        .await
    {
        Ok(entry) => entry,
        Err(HttpError::Transport(e)) => {
            tracing::debug!(target: "cli.send", "daemon unreachable for queue: {e}");
            return Ok(None);
        }
        Err(e) => bail!("daemon refused to queue the message: {e}"),
    };
    let position = client
        .queue_list(session_id)
        .await
        .ok()
        .and_then(|q| q.iter().position(|e| e.id == entry.id))
        .map(|p| p + 1)
        .unwrap_or(1);
    Ok(Some(position))
}

/// No daemon: append the row to the profile's `sessions.json` directly. The
/// daemon's disk reload picks it up (queue rows are disk-authoritative) and
/// its drain delivers it.
fn disk_enqueue(
    storage: &Storage,
    session_id: &str,
    qid: &str,
    message: &str,
    sender: &str,
) -> Result<usize> {
    storage.update(|instances, _groups| {
        let Some(inst) = instances.iter_mut().find(|i| i.id == session_id) else {
            bail!("Session {session_id} vanished from its profile while queueing");
        };
        let seq = inst.queued_prompt_next_seq;
        inst.queued_prompt_next_seq = seq.saturating_add(1);
        inst.queued_prompts
            .push(crate::acp::state::QueuedPromptEntry {
                id: qid.to_string(),
                seq,
                text: message.to_string(),
                attachments: Vec::new(),
                created_at: chrono::Utc::now().to_rfc3339(),
                origin_device: Some(sender.to_string()),
            });
        Ok(inst.queued_prompts.len())
    })
}

#[cfg(test)]
mod tests {
    use super::{new_queue_id, queued_line};

    #[test]
    fn queued_line_names_qid_position_and_size_only() {
        let refusal = crate::tmux::ParkedDraftRefusal::from_draft(
            "yes, authorize the vendor bump\nand send it",
        );
        let line = queued_line("send-0123456789ab", 2, "probe-target", &refusal);
        assert!(line.starts_with(
            "queued behind operator draft (qid send-0123456789ab, position 2) for 'probe-target'"
        ));
        assert!(line.contains("2 lines"));
        assert!(!line.contains("vendor"), "line leaked the draft: {line}");
    }

    #[test]
    fn queue_ids_are_short_and_prefixed() {
        let id = new_queue_id();
        assert!(id.starts_with("send-") && id.len() == 17, "{id}");
        assert_ne!(id, new_queue_id());
    }
}
