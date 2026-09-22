//! `agent-of-empires session` subcommands implementation

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use std::collections::HashSet;

use crate::session::{
    acquire_session_identity_lock, duplicate_session_error, is_duplicate_session, GroupTree,
    Instance, LifecycleOperation, ResumeIntent, StartOutcome, Storage,
};

#[derive(Subcommand)]
pub enum SessionCommands {
    /// Start a session's tmux process
    Start(SessionIdArgs),

    /// Stop session process
    Stop(SessionIdArgs),

    /// Restart session (or all sessions with `--all`)
    Restart(RestartArgs),

    /// Attach to session interactively
    Attach(SessionIdArgs),

    /// Show session details
    Show(ShowArgs),

    /// Rename a session
    Rename(RenameArgs),

    /// Edit a managed worktree session's workdir directory name (and,
    /// optionally, its git branch). Moves the worktree directory in place;
    /// the session must not be running. See #1723.
    SetWorktreeName(SetWorktreeNameArgs),

    /// Capture tmux pane output
    Capture(CaptureArgs),

    /// Auto-detect current session
    Current(CurrentArgs),

    /// Attach another repo to an existing session, creating a worktree for it
    /// and restarting the agent. Moving the session's working directory is
    /// refused while its resume target is a known conversation bound to that
    /// directory. Explicitly clear the resume target to start a new conversation
    /// after attaching. An implicitly preallocated ID is re-linked. See #3103.
    AddProject(AddProjectArgs),

    /// Set the resume target for a session; an agent whose exact native resume
    /// AoE cannot resolve is refused
    SetSessionId(SetSessionIdArgs),

    /// Set or clear the per-session diff base branch. The diff view
    /// compares the worktree against this ref instead of the
    /// auto-detected default. Useful when the PR target differs from
    /// the project default (stacked PRs, hotfix off `release/*`,
    /// renamed default branch). See #970.
    SetBase(SetBaseArgs),

    /// Snooze a session for a duration (temporary archive, auto wakes)
    Snooze(SnoozeArgs),

    /// Wake a snoozed session immediately
    Unsnooze(SessionIdArgs),

    /// Mark a session as a favorite. With `session.favorites_first` on (the
    /// default), favorited rows pin to the top of their sibling scope in every
    /// sort order; with it off, they pin within their status tier in the
    /// Attention sort only. Either way the row renders with a leading `*`
    /// marker plus bold and underline wherever the pin applies. Snoozing a
    /// favorite suspends the pin until it wakes.
    Favorite(SessionIdArgs),

    /// Clear the favorite flag on a session.
    Unfavorite(SessionIdArgs),

    /// Mark a session KEPT (WO#1953): `archive`, `snooze`, `remove`/trash and
    /// every auto-archive placement script refuse it until the flag is
    /// cleared with `--off`. There is no `--force` on the sweep side. Looked
    /// up across ALL profiles (no `-p` needed); daemon-first so a live board
    /// sees the flag at once, direct-to-disk when no daemon is running. The
    /// daemon logs every set/clear with who/when.
    Keep(KeepArgs),

    /// Acknowledge (clear) a session's urgent flag, sticky kinds included —
    /// the explicit counterpart of the ack a delivered `send` performs. For a
    /// row that is healthy again but still flagged (WO#1832).
    UrgentAck(SessionIdArgs),

    /// Set (or clear) a per-session color label, rendered as a colored dot in
    /// the web sidebar for at-a-glance status signaling. Intended for a
    /// running agent to flag its own state, e.g.
    /// `aoe session color $(aoe session current -q) red`. Colors: `red`
    /// (needs attention), `amber` (working), `green` (done); `none` clears it.
    Color(SetColorArgs),

    /// Archive a session: sink it in the Attention sort and tear down its
    /// tmux sessions. Worktree, branch, container preserved. `--no-kill`
    /// skips tmux teardown. See #1868.
    Archive(ArchiveArgs),

    /// Unarchive a session (restores it to its tier in the Attention sort)
    Unarchive(UnarchiveArgs),

    /// Restore a trashed session, returning it to its prior bucket with its
    /// transcript and metadata intact. See #2489.
    Restore(SessionIdArgs),

    /// Import existing Claude Code sessions from disk. Scans the given
    /// path(s) (default: current directory) for Claude Code conversations
    /// whose working directory is at or under a path, and creates an AoE
    /// session for each: a terminal/tmux session that resumes the
    /// conversation with `claude --resume <id>` (default), or a
    /// structured-view session with `--structured`.
    Import(ImportArgs),

    /// List the sessions currently in the trash.
    ListTrash,

    /// Permanently purge every trashed session in the profile (irreversible).
    EmptyTrash,

    /// Move a session to a different account profile in ONE step: relocate
    /// the session RECORD from whatever profile currently owns it into the
    /// target profile's `sessions.json`, then restart it so the live agent
    /// re-binds under the target account (`CLAUDE_CONFIG_DIR`). Replaces the
    /// error-prone manual recipe of hand-editing each profile's
    /// `sessions.json` and then `aoe -p <target> session restart <id>`.
    /// Idempotent: a no-op when the session is already in the target.
    Move(MoveArgs),

    /// Rewrite the `--model` a session launches with, in ONE step: patch the
    /// session RECORD's `extra_args` (and the structured-view `agent_model`
    /// field) in whatever profile currently owns it, then restart so the live
    /// agent relaunches under the new model. Mirrors `session move`, but the
    /// binding it changes is the model rather than the account profile.
    /// Idempotent: a no-op (no restart) when the record already launches the
    /// requested model. Pass an empty model (`""`) to CLEAR the pin and fall
    /// back to the account's default model.
    SetModel(SetModelArgs),

    /// Show a session's server-owned prompt queue: the sends parked behind an
    /// operator's unsent draft (`aoe send` and `POST /send` queue them instead
    /// of dropping them), delivered by the daemon as their own turns once the
    /// composer clears. `aoe session queue <id>` lists sender, age and the
    /// first 80 chars of each; `aoe session queue drop <id> <qid>` removes
    /// one. Looked up across ALL profiles, so no `-p` is needed.
    Queue(QueueArgs),
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct QueueArgs {
    #[command(subcommand)]
    pub action: Option<QueueAction>,

    /// Session ID or title (looked up across ALL profiles)
    pub identifier: Option<String>,

    /// Output as JSON (the queue rows exactly as the daemon holds them)
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum QueueAction {
    /// Remove one queued message by its queue id
    Drop {
        /// Session ID or title (looked up across ALL profiles)
        identifier: String,
        /// Queue id as printed by `aoe session queue <id>` (e.g. `send-0123456789ab`)
        qid: String,
    },
    /// Grant a queued message held for review one more delivery attempt
    /// (after inspecting the pane: nothing of it may still be in the composer)
    Release {
        /// Session ID or title (looked up across ALL profiles)
        identifier: String,
        /// Queue id shown with HOLD `review` by `aoe session queue <id>`
        qid: String,
    },
}

#[derive(Args)]
pub struct ImportArgs {
    /// Directories to scan. Only Claude sessions whose recorded working
    /// directory is at or under one of these are imported. Defaults to the
    /// current directory. Cannot be combined with `--all`.
    pub paths: Vec<String>,

    /// Import every discoverable Claude session, ignoring the path filter.
    #[arg(long, conflicts_with = "paths")]
    pub all: bool,

    /// Import as structured-view sessions (rendered in the web dashboard and
    /// the structured TUI view) instead of terminal/tmux sessions. Structured
    /// sessions replay their transcript under `aoe serve`.
    #[arg(long)]
    pub structured: bool,

    /// Place imported sessions under this session group.
    #[arg(long)]
    pub group: Option<String>,

    /// Start terminal sessions immediately after importing (spawns the tmux
    /// pane running `claude --resume <id>`). Ignored for structured imports.
    #[arg(long)]
    pub launch: bool,

    /// List what would be imported without creating anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the confirmation prompt when importing more than one session.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args)]
pub struct SnoozeArgs {
    /// Session ID or title
    pub identifier: String,

    /// Snooze duration in minutes; if omitted, uses `session.snooze_duration_minutes`
    /// from the active config (default 30)
    #[arg(long)]
    pub minutes: Option<u32>,

    /// The session is kept and you, a person, want to snooze it anyway:
    /// clears the keep flag (logged who/when) and snoozes in one command.
    #[arg(long = "confirm-kept")]
    pub confirm_kept: bool,
}

#[derive(Args)]
pub struct ArchiveArgs {
    /// Session ID or title
    pub identifier: String,

    /// Skip tmux teardown on archive.
    #[arg(long = "no-kill")]
    pub no_kill: bool,

    /// The session is kept and you, a person, want to archive it anyway:
    /// clears the keep flag (logged who/when) and archives in one command.
    #[arg(long = "confirm-kept")]
    pub confirm_kept: bool,
}

#[derive(Args)]
pub struct KeepArgs {
    /// Session ID or title. Looked up across ALL profiles.
    pub identifier: String,

    /// Clear the keep flag instead of setting it.
    #[arg(long)]
    pub off: bool,
}

#[derive(Args)]
pub struct SessionIdArgs {
    /// Session ID or title
    identifier: String,
}

#[derive(Args)]
pub struct UnarchiveArgs {
    /// Session ID or title
    identifier: String,

    /// Unarchive even if another non-trashed session (archived included, in
    /// any profile) already carries this title. Without it the restore is
    /// refused and the blocking row is named.
    #[arg(long = "allow-duplicate")]
    allow_duplicate: bool,
}

#[derive(Args)]
pub struct RestartArgs {
    /// Session ID or title (required unless `--all` is passed)
    pub identifier: Option<String>,

    /// Restart every session in the active profile. Useful after
    /// `aoe update`, after editing `sandbox.environment`, after a
    /// Docker hiccup, or after changing a hook. Mutually exclusive
    /// with `identifier`.
    #[arg(long, conflicts_with = "identifier")]
    pub all: bool,

    /// Concurrency cap for `--all`. Restarting many sandboxed
    /// sessions in parallel pressures dockerd, so the default is
    /// intentionally modest. Ignored when `--all` is not set.
    #[arg(long, default_value_t = 3)]
    pub parallel: usize,
}

#[derive(Args)]
pub struct MoveArgs {
    /// Session ID or title to relocate. Looked up across ALL profiles, so
    /// no `-p` is needed (and `-p` is intentionally ignored for the lookup —
    /// the move always finds the session in whatever profile owns it).
    pub identifier: String,

    /// Destination account profile (must already exist; see `aoe profile
    /// list`). The session's record is rewritten with `source_profile` set
    /// to this, then restarted so the live agent re-binds under it.
    pub target_profile: String,

    /// Relocate the record only; skip the live restart/re-bind. Useful when
    /// the session is stopped or you want to stage the move and restart
    /// later. The new account binding then takes effect on the next start.
    #[arg(long = "no-restart")]
    pub no_restart: bool,
}

#[derive(Args)]
pub struct SetModelArgs {
    /// Session ID or title to retarget. Looked up across ALL profiles, so no
    /// `-p` is needed (the lookup always finds the session in whatever profile
    /// owns it, exactly like `session move`).
    pub identifier: String,

    /// Model to launch with, forwarded to the agent as `--model <model>` (e.g.
    /// `claude-opus-4-8`, `claude-fable-5`, `opus`, `sonnet`). Pass an empty
    /// string (`""`) to CLEAR the pin and let the account default apply.
    pub model: String,

    /// Rewrite the record only; skip the live restart. The new model then
    /// takes effect on the session's next start. Useful for a stopped session
    /// or to stage the change.
    #[arg(long = "no-restart")]
    pub no_restart: bool,
}

#[derive(Args)]
pub struct RenameArgs {
    /// Session ID or title (optional, auto-detects in tmux)
    identifier: Option<String>,

    /// New title for the session
    #[arg(short, long)]
    title: Option<String>,

    /// New group for the session (empty string to ungroup)
    #[arg(short, long)]
    group: Option<String>,

    /// When the session is tied (session.tie_workdir_to_name) and an
    /// aoe-managed worktree, also rename the underlying git branch to match.
    /// Off by default; ignored for untied / non-worktree sessions.
    #[arg(long)]
    rename_branch: bool,

    /// Rename the Git branch without moving the worktree directory
    #[arg(long, conflicts_with = "rename_branch")]
    branch: Option<String>,
}

#[derive(Args)]
pub struct SetWorktreeNameArgs {
    /// Session ID or title (optional, auto-detects in tmux)
    identifier: Option<String>,

    /// New workdir (worktree directory) name
    #[arg(long)]
    name: String,

    /// Also rename the underlying git branch to match the new name
    #[arg(long)]
    rename_branch: bool,
}

#[derive(Args)]
pub struct ShowArgs {
    /// Session ID or title (optional, auto-detects in tmux)
    identifier: Option<String>,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct CaptureArgs {
    /// Session ID or title (auto-detects in tmux if omitted)
    identifier: Option<String>,

    /// Number of lines to capture
    #[arg(short = 'n', long, default_value = "50")]
    lines: usize,

    /// Strip ANSI escape codes
    #[arg(long)]
    strip_ansi: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct CurrentArgs {
    /// Just session name (for scripting)
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct CaptureOutput {
    id: String,
    title: String,
    status: String,
    tool: String,
    content: String,
    lines: usize,
}

#[derive(Args)]
pub struct SetSessionIdArgs {
    /// Session ID or title
    identifier: String,
    /// Conversation to resume. An empty string requests a one-shot fresh
    /// start, which only a terminal session can take: a structured session
    /// keeps its ACP conversation and needs the native ID plus an explicit
    /// `--store` and a bound Claude conversation.
    session_id: String,
    /// Assert the native store: a Claude store directory, or a Pi/OMP transcript file.
    #[arg(long)]
    store: Option<std::path::PathBuf>,
}

#[derive(Args)]
pub struct AddProjectArgs {
    /// Session ID or title
    pub identifier: String,
    /// Repo to attach: a path, or the name of a registered project
    /// (`aoe project list`).
    pub project: String,
    /// Check out a branch that already exists in the repo being attached
    /// instead of refusing. A same-named branch in another repo can hold
    /// unrelated commits, so this is off by default. When set, aoe records the
    /// branch as not its own and leaves it in place when the session is
    /// deleted.
    #[arg(long)]
    pub attach_existing_branch: bool,
}

#[derive(Args)]
pub struct SetBaseArgs {
    /// Session ID or title
    pub identifier: String,
    /// Branch ref to diff against (short name like `main` or
    /// remote-qualified like `upstream/main`). Required unless
    /// `--clear` is passed.
    pub branch: Option<String>,
    /// Clear the override and fall back to the recorded creation base,
    /// then the profile default, then the auto-detected base.
    #[arg(long, conflicts_with = "branch")]
    pub clear: bool,
    /// Workspace repo to set the base for, by directory name (as shown in
    /// the diff panel and `aoe list --json`). Required on a multi-repo
    /// workspace session, where each repo has its own base; omit it on a
    /// single-repo session.
    #[arg(long)]
    pub repo: Option<String>,
}

#[derive(Args)]
pub struct SetColorArgs {
    /// Session ID or title
    pub identifier: String,
    /// Color label: `red` (needs attention), `amber` (working), `green`
    /// (done), or `none`/`clear` to remove the label.
    pub color: String,
}

#[derive(Serialize)]
struct SessionDetails {
    id: String,
    title: String,
    path: String,
    group: String,
    tool: String,
    command: String,
    status: String,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Keep flag (WO#1953), same keys as `/api/sessions` and `list --json`.
    kept: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kept_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kept_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    profile: String,
    /// Live account + usage view (WO#1852); same keys as `/api/sessions`.
    #[serde(flatten)]
    account: crate::session::account::SessionAccount,
}

fn session_details(inst: &Instance, profile: &str) -> SessionDetails {
    SessionDetails {
        id: inst.id.clone(),
        title: inst.title.clone(),
        path: inst.project_path.clone(),
        group: inst.group_path.clone(),
        tool: inst.tool.clone(),
        command: inst.command.clone(),
        status: format!("{:?}", inst.status).to_lowercase(),
        state: super::list::state_tag(inst),
        trashed_at: inst.trashed_at,
        archived_at: inst.archived_at,
        snoozed_until: super::list::active_snoozed_until(inst),
        pinned_at: inst.pinned_at,
        kept: inst.is_kept(),
        kept_at: inst.kept_at,
        kept_by: inst.kept_by.clone(),
        agent_session_id: inst.agent_session_id.clone(),
        parent_session_id: inst.parent_session_id.clone(),
        profile: profile.to_string(),
        account: Default::default(),
    }
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
/// Re-home a profile-scoped id verb onto the profile that owns its session
/// when the invocation named none (see `cli::resolve_scope`); the identifier
/// is rewritten to the session's full id so the verb's own lookup is exact.
/// Verbs without a session identifier (`current`, `import`, the trash verbs,
/// `restart --all`), `restore` (its target lives in a profile's trash, not its
/// registry) and verbs that already resolve across profiles pass through.
fn scope_command(profile: &str, mut command: SessionCommands) -> Result<(String, SessionCommands)> {
    let target: Option<&mut String> = match &mut command {
        SessionCommands::Start(a)
        | SessionCommands::Stop(a)
        | SessionCommands::Attach(a)
        | SessionCommands::Unsnooze(a)
        | SessionCommands::Favorite(a)
        | SessionCommands::Unfavorite(a) => Some(&mut a.identifier),
        SessionCommands::Unarchive(a) => Some(&mut a.identifier),
        SessionCommands::Restart(a) => a.identifier.as_mut(),
        SessionCommands::Show(a) => a.identifier.as_mut(),
        SessionCommands::Rename(a) => a.identifier.as_mut(),
        SessionCommands::SetWorktreeName(a) => a.identifier.as_mut(),
        SessionCommands::Capture(a) => a.identifier.as_mut(),
        SessionCommands::SetSessionId(a) => Some(&mut a.identifier),
        SessionCommands::AddProject(a) => Some(&mut a.identifier),
        SessionCommands::SetBase(a) => Some(&mut a.identifier),
        SessionCommands::Snooze(a) => Some(&mut a.identifier),
        SessionCommands::Color(a) => Some(&mut a.identifier),
        SessionCommands::Archive(a) => Some(&mut a.identifier),
        _ => None,
    };
    match target {
        Some(identifier) => {
            let scope = super::resolve_scope(profile, identifier)?;
            *identifier = scope.identifier;
            Ok((scope.profile, command))
        }
        None => Ok((profile.to_string(), command)),
    }
}

pub async fn run(profile: &str, command: SessionCommands) -> Result<()> {
    let (profile, command) = scope_command(profile, command)?;
    let profile = profile.as_str();
    match command {
        SessionCommands::Start(args) => start_session(profile, args).await,
        SessionCommands::Stop(args) => stop_session(profile, args).await,
        SessionCommands::Restart(args) => restart_session_dispatch(profile, args).await,
        SessionCommands::Attach(args) => attach_session(profile, args).await,
        SessionCommands::Show(args) => show_session(profile, args).await,
        SessionCommands::Capture(args) => capture_session(profile, args).await,
        SessionCommands::Rename(args) => rename_session(profile, args).await,
        SessionCommands::SetWorktreeName(args) => set_worktree_name(profile, args).await,
        SessionCommands::Current(args) => current_session(args).await,
        SessionCommands::SetSessionId(args) => set_session_id(profile, args).await,
        SessionCommands::AddProject(args) => add_project(profile, args).await,
        SessionCommands::SetBase(args) => set_base(profile, args).await,
        SessionCommands::Snooze(args) => snooze_session(profile, args).await,
        SessionCommands::Unsnooze(args) => unsnooze_session(profile, args).await,
        SessionCommands::Favorite(args) => {
            mark_session(profile, args, "Favorited", Instance::favorite).await
        }
        SessionCommands::Unfavorite(args) => {
            mark_session(profile, args, "Unfavorited", Instance::unfavorite).await
        }
        SessionCommands::Keep(args) => keep_session(args).await,
        SessionCommands::UrgentAck(args) => urgent_ack_session(profile, args).await,
        SessionCommands::Color(args) => set_color_session(profile, args).await,
        SessionCommands::Archive(args) => archive_session(profile, args).await,
        SessionCommands::Unarchive(args) => unarchive_session(profile, args).await,
        SessionCommands::Restore(args) => restore_session(profile, args).await,
        SessionCommands::Import(args) => import_sessions(profile, args).await,
        SessionCommands::ListTrash => list_trash(profile).await,
        SessionCommands::EmptyTrash => empty_trash(profile).await,
        SessionCommands::Move(args) => move_session(args).await,
        SessionCommands::SetModel(args) => set_model_session(args).await,
        SessionCommands::Queue(args) => queue_session(args).await,
    }
}

/// `aoe session queue <id>` / `aoe session queue drop <id> <qid>` /
/// `aoe session queue release <id> <qid>`.
async fn queue_session(args: QueueArgs) -> Result<()> {
    match args.action {
        Some(QueueAction::Drop { identifier, qid }) => queue_drop(&identifier, &qid).await,
        Some(QueueAction::Release { identifier, qid }) => queue_release(&identifier, &qid).await,
        None => {
            let identifier = args.identifier.ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: aoe session queue <session> | aoe session queue drop <session> <qid> \
                     | aoe session queue release <session> <qid>"
                )
            })?;
            queue_list(&identifier, args.json).await
        }
    }
}

async fn queue_list(identifier: &str, json: bool) -> Result<()> {
    let (profile, inst) = find_session_across_profiles(identifier)?;
    // Daemon-first: the daemon owns the queue and its view is authoritative
    // for a row it has just retired. Disk is the fallback when no daemon is
    // reachable (the rows are persisted there too).
    let (rows, receipts, source) = match daemon_queue_list(&inst.id).await {
        Some(rows) => (rows, daemon_queue_receipts(&inst.id).await, "daemon"),
        None => {
            let mut rows = inst.queued_prompts.clone();
            rows.sort_by_key(|e| e.seq);
            let receipts = disk_queue_receipts(&inst.id, &rows);
            (rows, receipts, "disk")
        }
    };
    if json {
        return super::output::print_json(&rows);
    }
    if rows.is_empty() {
        println!(
            "No queued messages for '{}' (profile '{}', via {source})",
            inst.title, profile
        );
        return Ok(());
    }
    let held = receipts
        .as_ref()
        .map(|r| {
            rows.iter()
                .filter(|e| {
                    matches!(
                        queue_hold_label(r.get(&e.id).map(String::as_str)),
                        "review" | "exhausted"
                    )
                })
                .count()
        })
        .unwrap_or(0);
    println!(
        "Queued for '{}' (profile '{}', {} pending, {held} held, via {source}):",
        inst.title,
        profile,
        rows.len()
    );
    match &receipts {
        Some(_) => println!(
            "{:<18} {:>6}  {:<9} {:<24} TEXT",
            "QID", "AGE", "HOLD", "SENDER"
        ),
        None => println!("{:<18} {:>6}  {:<24} TEXT", "QID", "AGE", "SENDER"),
    }
    let now = chrono::Utc::now();
    for e in &rows {
        let hold = receipts
            .as_ref()
            .map(|r| queue_hold_label(r.get(&e.id).map(String::as_str)));
        println!("{}", format_queue_row(e, now, hold));
    }
    if held > 0 {
        println!(
            "HOLD review: an attempt was recorded and nothing re-attempts it. HOLD exhausted: \
             {cap} automatic attempts withheld Enter. Inspect the pane; then \
             `aoe session queue release {} <qid>` or `... drop {} <qid>`.",
            inst.id,
            inst.id,
            cap = crate::acp::event_store::terminal_queue::MAX_AUTOMATIC_ATTEMPTS
        );
    }
    Ok(())
}

/// Release a held queued message for one more delivery attempt. Daemon
/// first; on disk (the receipt store) when no daemon is reachable.
async fn queue_release(identifier: &str, qid: &str) -> Result<()> {
    let (profile, inst) = find_session_across_profiles(identifier)?;
    match daemon_queue_release(&inst.id, qid).await? {
        Some(true) => {
            println!(
                "Released {qid} on '{}' (daemon-side); it is attempted again once the composer is clear",
                inst.title
            );
            return Ok(());
        }
        Some(false) => bail!("No queued message {qid:?} on '{}'", inst.title),
        None => {}
    }
    if !inst.queued_prompts.iter().any(|q| q.id == qid) {
        bail!("No queued message {qid:?} on '{}'", inst.title);
    }
    let receipts = crate::acp::event_store::EventStore::open(
        &crate::session::get_app_dir()?.join("acp_events.db"),
        1000,
    )?;
    if !receipts.release_terminal_prompt(&inst.id, qid, 0)? {
        let disposition = receipts.terminal_prompt_receipt(&inst.id, qid)?;
        bail!(
            "{qid} on '{}' is not held (receipt: {})",
            inst.title,
            disposition.as_deref().unwrap_or("none")
        );
    }
    println!(
        "Released {qid} on '{}' (on disk, profile '{profile}'; no daemon reachable)",
        inst.title
    );
    Ok(())
}

async fn queue_drop(identifier: &str, qid: &str) -> Result<()> {
    let (profile, inst) = find_session_across_profiles(identifier)?;
    match daemon_queue_remove(&inst.id, qid).await? {
        Some(true) => {
            println!("Dropped {qid} from '{}' (daemon-side)", inst.title);
            return Ok(());
        }
        Some(false) => bail!("No queued message {qid:?} on '{}'", inst.title),
        None => {}
    }
    let storage = Storage::open_unwatched(&profile)?;
    let target = inst.id.clone();
    let receipts = crate::acp::event_store::EventStore::open(
        &crate::session::get_app_dir()?.join("acp_events.db"),
        1000,
    )?;
    receipts.drop_terminal_prompt(&target, qid)?;
    let found = storage.update(|instances, _groups| {
        Ok(instances
            .iter_mut()
            .find(|i| i.id == target)
            .map(|i| {
                let before = i.queued_prompts.len();
                i.queued_prompts.retain(|q| q.id != qid);
                i.queued_prompts.len() != before
            })
            .unwrap_or(false))
    })?;
    if !found {
        bail!("No queued message {qid:?} on '{}'", inst.title);
    }
    println!(
        "Dropped {qid} from '{}' (on disk, profile '{profile}'; no daemon reachable)",
        inst.title
    );
    Ok(())
}

/// The daemon's view of the queue, or `None` when no daemon is reachable.
async fn daemon_queue_list(session_id: &str) -> Option<Vec<crate::daemon::QueuedPromptEntry>> {
    use crate::acp::client::{discovery, HttpClient};
    let endpoint = discovery::discover_local().ok()?;
    let client = HttpClient::new(endpoint).ok()?;
    client.queue_list(session_id).await.ok()
}

/// The daemon's receipts for the queue, or `None` when it cannot answer
/// (older daemon without the route, or unreachable).
async fn daemon_queue_receipts(
    session_id: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    use crate::acp::client::{discovery, HttpClient};
    let endpoint = discovery::discover_local().ok()?;
    let client = HttpClient::new(endpoint).ok()?;
    client.queue_receipts(session_id).await.ok()
}

/// Receipts read straight from the receipt store when no daemon answers.
fn disk_queue_receipts(
    session_id: &str,
    rows: &[crate::daemon::QueuedPromptEntry],
) -> Option<std::collections::BTreeMap<String, String>> {
    let store = crate::acp::event_store::EventStore::open(
        &crate::session::get_app_dir().ok()?.join("acp_events.db"),
        1000,
    )
    .ok()?;
    let mut receipts = std::collections::BTreeMap::new();
    for row in rows {
        if let Some(d) = store.terminal_prompt_receipt(session_id, &row.id).ok()? {
            receipts.insert(row.id.clone(), d);
        }
    }
    Some(receipts)
}

/// `Ok(Some(true))` released, `Ok(Some(false))` the daemon has no such row,
/// `Ok(None)` no daemon reachable (fall back to disk).
async fn daemon_queue_release(session_id: &str, qid: &str) -> Result<Option<bool>> {
    use crate::acp::client::{discovery, HttpClient, HttpError};
    let Ok(endpoint) = discovery::discover_local() else {
        return Ok(None);
    };
    let Ok(client) = HttpClient::new(endpoint) else {
        return Ok(None);
    };
    match client.queue_release(session_id, qid).await {
        Ok(()) => Ok(Some(true)),
        Err(HttpError::Transport(_)) => Ok(None),
        Err(HttpError::SessionNotFound(_)) => Ok(Some(false)),
        Err(e) => bail!("daemon refused to release {qid}: {e}"),
    }
}

/// `Ok(Some(true))` dropped, `Ok(Some(false))` the daemon has no such row,
/// `Ok(None)` no daemon reachable (fall back to disk).
async fn daemon_queue_remove(session_id: &str, qid: &str) -> Result<Option<bool>> {
    use crate::acp::client::{discovery, HttpClient, HttpError};
    let Ok(endpoint) = discovery::discover_local() else {
        return Ok(None);
    };
    let Ok(client) = HttpClient::new(endpoint) else {
        return Ok(None);
    };
    match client.queue_remove(session_id, qid).await {
        Ok(()) => Ok(Some(true)),
        Err(HttpError::Transport(_)) => Ok(None),
        Err(HttpError::SessionNotFound(_)) => Ok(Some(false)),
        Err(e) => bail!("daemon refused to drop {qid}: {e}"),
    }
}

/// The HOLD column for one queued row, from its delivery receipt: `-` never
/// attempted, `review` held until an operator releases or drops it,
/// `released` waiting for its next attempt, `exhausted` held with its
/// automatic attempts spent, `retiring` consumed (a stale snapshot brought
/// it back; the drain removes it without typing).
fn queue_hold_label(disposition: Option<&str>) -> &'static str {
    use crate::acp::event_store::terminal_queue::{receipt_is_held, released_attempts};
    match disposition {
        None => "-",
        Some("delivered") | Some("dropped") => "retiring",
        Some(d) if released_attempts(d).is_some() => {
            if receipt_is_held(d) {
                "exhausted"
            } else {
                "released"
            }
        }
        Some(_) => "review",
    }
}

/// `<qid> <age> [<hold>] <sender> <first 80 chars>`: one queued row for a
/// human. Whitespace is collapsed so a multi-line report reads as one line.
fn format_queue_row(
    entry: &crate::daemon::QueuedPromptEntry,
    now: chrono::DateTime<chrono::Utc>,
    hold: Option<&str>,
) -> String {
    let age = chrono::DateTime::parse_from_rfc3339(&entry.created_at)
        .map(|t| humanize_age((now - t.with_timezone(&chrono::Utc)).num_seconds().max(0)))
        .unwrap_or_else(|_| "?".to_string());
    let sender: String = entry
        .origin_device
        .as_deref()
        .unwrap_or("-")
        .chars()
        .take(24)
        .collect();
    let collapsed = entry.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text: String = collapsed.chars().take(80).collect();
    if collapsed.chars().count() > 80 {
        text.push('…');
    }
    match hold {
        Some(hold) => format!(
            "{:<18} {:>6}  {:<9} {:<24} {}",
            entry.id, age, hold, sender, text
        ),
        None => format!("{:<18} {:>6}  {:<24} {}", entry.id, age, sender, text),
    }
}

fn humanize_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Rewrite an `extra_args` string so the agent launches with `--model
/// <model>`. Any existing `--model <val>` / `--model=<val>` pair is stripped
/// first (so repeated calls stay idempotent and never accumulate duplicate
/// flags), then the new flag is appended when `model` is non-empty. An empty
/// `model` clears the pin (strip only). Pure and free so it is unit-testable;
/// `set_model_session` delegates here.
fn apply_model_arg(extra: &str, model: &str) -> String {
    let toks: Vec<&str> = extra.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(toks.len() + 2);
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t == "--model" {
            i += 2; // drop the flag and its value
            continue;
        }
        if t.starts_with("--model=") {
            i += 1;
            continue;
        }
        out.push(t);
        i += 1;
    }
    let mut result = out.join(" ");
    let model = model.trim();
    if !model.is_empty() {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str("--model ");
        result.push_str(model);
    }
    result
}

/// Retarget a session's launch model cross-profile, then restart to re-bind.
///
/// Profile is resolved from the session itself (via
/// [`find_session_across_profiles`]), NOT from the caller's `-p`, so the
/// command works the same no matter which profile the CLI defaulted to.
/// Idempotent: when the record already launches the requested model (both the
/// `extra_args` `--model` token and the structured-view `agent_model` field
/// already match), it is a no-op and no restart fires.
async fn set_model_session(args: SetModelArgs) -> Result<()> {
    let model = args.model.trim().to_string();

    let (owner, record) = find_session_across_profiles(&args.identifier)?;
    let id = record.id.clone();
    let title = record.title.clone();

    let old_extra = record.extra_args.clone();
    let new_extra = apply_model_arg(&old_extra, &model);
    // The structured-view `agent_model` field is core: upstream 15be9fdf made
    // the daemon unconditional and kept `serve` only as a deprecated alias of
    // `web`, so a serve-feature cfg gate is never set on a `--features web`
    // build (and upstream CI rejects such a gate).
    let new_agent_model = if model.is_empty() {
        None
    } else {
        Some(model.clone())
    };
    let agent_model_unchanged = record.agent_model == new_agent_model;

    // Idempotency: both surfaces already at the requested model -> nothing to
    // do, and (crucially) no needless restart of a live session.
    if new_extra == old_extra && agent_model_unchanged {
        let shown = if model.is_empty() { "default" } else { &model };
        println!(
            "Session '{}' ({}) already launches model '{}'; nothing to change.",
            title, id, shown
        );
        return Ok(());
    }

    let storage = Storage::new_unwatched(&owner)?;
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.extra_args = new_extra.clone();
            stored.agent_model = new_agent_model.clone();
            Ok(true)
        } else {
            Ok(false)
        }
    })?;
    if !landed {
        bail!(
            "Session {} ({}) was removed from profile '{}' before set-model could land",
            title,
            id,
            owner
        );
    }

    let shown = if model.is_empty() {
        "default (cleared)"
    } else {
        &model
    };
    println!(
        "✓ Set model for '{}' ({}) in profile '{}': '{}'.",
        title, id, owner, shown
    );

    if args.no_restart {
        println!("  (--no-restart) new model takes effect on the session's next start.");
        return Ok(());
    }

    println!("  Restarting to relaunch under the new model...");
    restart_session(&owner, SessionIdArgs { identifier: id }).await?;
    Ok(())
}

/// Locate the single session matching `identifier` (exact id, id-prefix, or
/// title) across EVERY profile's `sessions.json`, returning
/// `(owning_profile, instance)`. An exact-id hit wins outright even if a
/// title collides elsewhere; otherwise more than one matching profile is an
/// ambiguity error (refuse rather than guess which account to touch).
/// `cli:<user>@<host>` — who is asking, recorded on the row and in the
/// daemon's `session.keep` log line.
fn keep_actor() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    // `HOSTNAME` is not exported by every shell; `/etc/hostname` (Linux) and
    // `uname -n` cover the rest without pulling in a crate for one string.
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .or_else(|| {
            std::process::Command::new("uname")
                .arg("-n")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string());
    format!("cli:{user}@{host}")
}

/// `aoe session keep <id> [--off]` (WO#1953). Daemon-first: the running
/// board owns the live row and logs the transition; disk is the fallback
/// when no daemon is up (the row is then picked up on the daemon's next
/// file-watch reload).
async fn keep_session(args: KeepArgs) -> Result<()> {
    let (owner, inst) = find_session_across_profiles(&args.identifier)?;
    let keep = !args.off;
    let by = keep_actor();
    match daemon_session_keep(&inst.id, keep, &by).await? {
        Some(true) => {
            println!(
                "{}: {} ({}) [daemon-side, by {by}]",
                if keep { "Kept" } else { "Keep cleared" },
                inst.title,
                inst.id
            );
            return Ok(());
        }
        Some(false) => {
            // Daemon is up but does not know the row (e.g. an older daemon
            // without the endpoint, or a profile it does not serve): fall
            // through to disk so the flag still lands.
        }
        None => {}
    }
    let storage = Storage::open_unwatched(&owner)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &inst.id, |row| {
            if keep {
                row.keep(Some(&by));
            } else {
                row.unkeep();
            }
            Ok(row.title.clone())
        })
    })?;
    println!(
        "{}: {} ({}) [profile '{owner}', by {by}]",
        if keep { "Kept" } else { "Keep cleared" },
        title,
        inst.id
    );
    Ok(())
}

/// WO#1980-1: the CLI half of the human override. Called by archive /
/// snooze / rm when the row is kept and `--confirm-kept` was given: clears
/// the flag daemon-first (the daemon logs who/when), falling back to disk,
/// and prints the clear so the operator sees both steps. The caller then
/// re-checks `keep_refusal`, which is now `None`, and proceeds.
pub(crate) async fn clear_keep_for_override(profile: &str, id: &str, op: &str) -> Result<()> {
    let by = keep_actor();
    let landed = match daemon_session_keep(id, false, &by).await? {
        Some(true) => "daemon-side",
        _ => {
            let storage = Storage::open_unwatched(profile)?;
            storage.update(|instances, _groups| {
                super::patch_instance(instances, id, |row| {
                    row.unkeep();
                    Ok(())
                })
            })?;
            "on disk"
        }
    };
    tracing::info!(
        target: "session.keep",
        session = %id,
        op = %op,
        by = %by,
        at = %chrono::Utc::now().to_rfc3339(),
        "keep flag CLEARED (override: {op} anyway, --confirm-kept)"
    );
    println!("Keep cleared ({landed}, by {by}) — {op} anyway: {id}");
    Ok(())
}

/// `Some(true)` when the daemon applied the change, `Some(false)` when it
/// answered 404 for the row (fall back to disk), `None` when no daemon is
/// reachable at all.
async fn daemon_session_keep(session_id: &str, keep: bool, by: &str) -> Result<Option<bool>> {
    use crate::acp::client::{discovery, HttpClient, HttpError};
    let Ok(endpoint) = discovery::discover_local() else {
        return Ok(None);
    };
    let Ok(client) = HttpClient::new(endpoint) else {
        return Ok(None);
    };
    match client.session_keep(session_id, keep, by).await {
        Ok(()) => Ok(Some(true)),
        Err(HttpError::Transport(_)) => Ok(None),
        Err(HttpError::SessionNotFound(_)) => Ok(Some(false)),
        Err(e) => bail!("daemon refused keep update for {session_id}: {e}"),
    }
}

fn find_session_across_profiles(identifier: &str) -> Result<(String, crate::session::Instance)> {
    let profiles = crate::session::list_profiles()?;
    let mut hits: Vec<(String, crate::session::Instance)> = Vec::new();
    for p in &profiles {
        let storage = match Storage::new_unwatched(p) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let instances = match storage.load() {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Exact-id match short-circuits: unambiguous regardless of titles.
        if let Some(inst) = instances.iter().find(|i| i.id == identifier) {
            return Ok((p.clone(), inst.clone()));
        }
        if let Ok(inst) = super::resolve_session(identifier, &instances) {
            hits.push((p.clone(), inst.clone()));
        }
    }
    match hits.len() {
        0 => bail!("No session matching {:?} in any profile", identifier),
        1 => Ok(hits.into_iter().next().unwrap()),
        _ => {
            let where_ = hits
                .iter()
                .map(|(p, i)| format!("{} (profile '{}')", i.id, p))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Ambiguous: {:?} matches sessions in multiple profiles: {}. \
                 Re-run with the exact session id.",
                identifier,
                where_
            )
        }
    }
}

/// Pull the `CLAUDE_CONFIG_DIR` value out of a resolved host-environment list
/// (`KEY=value` entries). `None` when the list carries no such entry. Symlink
/// canonicalization is intentionally NOT done here; callers canonicalize the
/// live and expected paths before comparing so this stays a pure string pluck.
fn extract_config_dir(environment: &[String]) -> Option<String> {
    environment
        .iter()
        .find_map(|e| e.strip_prefix("CLAUDE_CONFIG_DIR=").map(|v| v.to_string()))
}

/// Seed Claude Code's folder-trust record for `project_path` into the config
/// tree the destination profile's environment selects: `<CLAUDE_CONFIG_DIR>/
/// .claude.json` when the profile pins one, else the default `~/.claude.json`.
/// The key is the workspace's canonical path (the pane's `process.cwd()` is
/// the physical path); a workspace that does not resolve keeps its configured
/// spelling. Returns the file written and the key used. Every other key in the
/// file is preserved (`hooks::trust_claude_project`).
fn seed_destination_folder_trust(
    environment: &[String],
    home: &std::path::Path,
    project_path: &str,
) -> Result<(std::path::PathBuf, String)> {
    let claude_json = match extract_config_dir(environment) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir).join(".claude.json"),
        _ => home.join(".claude.json"),
    };
    if let Some(parent) = claude_json.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating Claude config dir {}", parent.display()))?;
    }
    let key = std::fs::canonicalize(project_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| project_path.to_string());
    crate::hooks::trust_claude_project(&claude_json, &key, crate::hooks::SymlinkPolicy::Follow)
        .with_context(|| {
            format!(
                "writing folder trust for {} into {}",
                key,
                claude_json.display()
            )
        })?;
    Ok((claude_json, key))
}

/// Canonicalize a config-dir path through symlinks so a compat-alias dir (e.g.
/// `.claude-accounts/pivot-main` -> `gna-main`) compares equal to its real
/// target and never reads as a spurious divergence. Fail-soft to the raw string
/// when the path can't be resolved (missing dir / not a symlink both no-op).
fn canon_config_dir(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Decide whether a session's LIVE account binding has diverged from what its
/// target profile would assign. Divergence requires BOTH sides known AND
/// different (after canonicalization): an unreadable live binding (`None`, e.g.
/// a stopped session or a pane we can't inspect) fails SAFE to "not diverged",
/// so the move never forces a spurious restart on a session it can't observe.
fn config_dir_diverged(live: Option<&str>, expected: Option<&str>) -> bool {
    match (live, expected) {
        (Some(l), Some(e)) => canon_config_dir(l) != canon_config_dir(e),
        _ => false,
    }
}

/// Read the LIVE `CLAUDE_CONFIG_DIR` the session's running pane was launched
/// with, by inspecting the pane's process tree. The pane primary can be a
/// wrapper whose own environment lacks the variable (Linux: `bash
/// /tmp/aoe-pane-env-…` → the agent), so the read walks pane pid and
/// descendants and takes the first hit. `None` when the session has no live
/// pane or no process in the tree carries it (caller fails safe to
/// no-divergence).
fn live_config_dir(inst: &crate::session::Instance) -> Option<String> {
    let session = inst.tmux_session().ok()?;
    let pid = crate::process::get_pane_pid(session.name())?;
    crate::process::get_process_env_var_in_tree(pid, "CLAUDE_CONFIG_DIR")
}

/// Relocate a session's record cross-profile, then re-bind the live account.
///
/// Profile is resolved from the session itself (via
/// [`find_session_across_profiles`]), NOT from the caller's `-p`, so the
/// command works the same no matter which profile the CLI defaulted to.
async fn move_session(args: MoveArgs) -> Result<()> {
    let target = args.target_profile.trim().to_string();
    if target.is_empty() {
        bail!("Target profile must not be empty");
    }

    // Validate the target exists in the registry up front: refuse to strand
    // a record in a profile dir that no account is bound to.
    let profiles = crate::session::list_profiles()?;
    if !profiles.iter().any(|p| p == &target) {
        bail!(
            "Unknown target profile '{}'. Known profiles: {}",
            target,
            profiles.join(", ")
        );
    }

    let (owner, record) = find_session_across_profiles(&args.identifier)?;
    let id = record.id.clone();
    let title = record.title.clone();

    if owner == target {
        // The registry label already equals the target, but the session can
        // STILL be stranded: a pane respawned out-of-band (a `cx-restart` that
        // baked a pinned `CLAUDE_CONFIG_DIR`, or a `--fork-session` recovery)
        // runs under a DIFFERENT live account than the label implies. The old
        // code no-op'd here ("already in profile; nothing to move"), leaving
        // such a label-matches-but-live-diverged session UN-relocatable on a
        // capped account, the exact bug that forced a two-hop workaround. So
        // compare the LIVE CLAUDE_CONFIG_DIR against what the target profile
        // resolves; when they diverge, perform the same restart+rebind a
        // cross-profile move would, so a single call rescues it.
        let expected = extract_config_dir(
            &crate::session::config::profile_config::resolve_config_or_warn(&target).environment,
        );
        let live = live_config_dir(&record);
        if !args.no_restart && config_dir_diverged(live.as_deref(), expected.as_deref()) {
            println!(
                "Session '{}' ({}) label is already '{}', but the LIVE account diverged \
                 (live={}, profile resolves={}); restarting to rebind.",
                title,
                id,
                target,
                live.as_deref().unwrap_or("?"),
                expected.as_deref().unwrap_or("?"),
            );
            // `source_profile` already equals `target` here, so the restart
            // path re-resolves CLAUDE_CONFIG_DIR from it and relaunches the
            // pane on the correct account, clearing the out-of-band override.
            restart_session(&target, SessionIdArgs { identifier: id }).await?;
            return Ok(());
        }
        println!(
            "Session '{}' ({}) is already in profile '{}'; nothing to move.",
            title, id, target
        );
        return Ok(());
    }

    // Trust the workspace on the DESTINATION account before anything moves.
    // A re-bind lands the pane on an account that has never opened this cwd;
    // Claude Code answers that with its workspace-trust prompt, and a headless
    // pane answers the prompt by dying with nothing on screen (WO#1743). Seed
    // now, whether the restart happens below or days later (`--no-restart`),
    // and refuse the move outright when the record cannot be written: a loud
    // refusal here beats a silent dead pane at the next launch.
    let target_config = crate::session::config::profile_config::resolve_config_or_warn(&target);
    let target_environment = target_config.environment;
    let home = dirs::home_dir().context("cannot seed destination folder trust: no home dir")?;
    let (trusted_json, trusted_key) =
        seed_destination_folder_trust(&target_environment, &home, &record.project_path)
            .with_context(|| {
                format!(
                    "refusing to move '{}' ({}) to profile '{}': its workspace {} could not be \
                     trusted on the destination account (the pane would die on the trust prompt)",
                    title, id, target, record.project_path
                )
            })?;
    println!(
        "✓ Destination trust seeded: {} -> {}",
        trusted_key,
        trusted_json.display()
    );
    // The destination account must also carry the fleet's user-scope MCP
    // servers, or the moved session comes up with no MCP tools (WO#1894).
    // Seeded from the one configured template; an account that already lists
    // servers is left as it is. A configured-but-broken template refuses the
    // move for the same reason trust does: loud now beats silent later.
    let destination_json = match extract_config_dir(&target_environment) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir).join(".claude.json"),
        _ => home.join(".claude.json"),
    };
    match crate::session::seed_mcp_servers_from_template(
        target_config.session.claude_mcp_servers_seed.as_deref(),
        &destination_json,
        &home,
    )
    .with_context(|| {
        format!(
            "refusing to move '{}' ({}) to profile '{}': its user-scope MCP servers could not \
             be seeded into {} (the pane would start with no MCP tools)",
            title,
            id,
            target,
            destination_json.display()
        )
    })? {
        Some((template, true)) => println!(
            "✓ Destination MCP servers seeded: {} <- {}",
            destination_json.display(),
            template.display()
        ),
        Some((_, false)) => println!(
            "✓ Destination MCP servers already present: {}",
            destination_json.display()
        ),
        None => {}
    }

    // Build the relocated record: re-home it on the target profile and drop
    // the per-profile group association (group_path is meaningful only
    // within its origin profile; carrying it over would dangle).
    let mut moved = record.clone();
    moved.source_profile = target.clone();
    moved.group_path = String::new();

    // Insert into the target FIRST, then remove from the source. A crash
    // between the two leaves a harmless duplicate (recoverable) rather than
    // a vanished session. Insert is idempotent on id.
    let target_storage = Storage::new_unwatched(&target)?;
    target_storage.update(|instances, _groups| {
        if !instances.iter().any(|i| i.id == id) {
            instances.push(moved.clone());
        }
        Ok(())
    })?;
    let source_storage = Storage::new_unwatched(&owner)?;
    source_storage.update(|instances, _groups| {
        instances.retain(|i| i.id != id);
        Ok(())
    })?;

    println!(
        "✓ Moved record '{}' ({}): profile '{}' -> '{}'.",
        title, id, owner, target
    );

    if args.no_restart {
        println!("  (--no-restart) live account re-binds on the session's next start.",);
        return Ok(());
    }

    // Re-bind the live account by restarting under the target profile. The
    // restart path re-resolves CLAUDE_CONFIG_DIR from source_profile=target.
    println!("  Re-binding live account under '{}'...", target);
    let expected = extract_config_dir(
        &crate::session::config::profile_config::resolve_config_or_warn(&target).environment,
    );
    if let Err(e) = restart_session(
        &target,
        SessionIdArgs {
            identifier: id.clone(),
        },
    )
    .await
    {
        bail!(
            "{}",
            move_incomplete_message(
                &title,
                &id,
                &owner,
                &target,
                None,
                &format!("restart failed: {e}"),
            )
        );
    }

    // Moving the record is not the move. Verify the LIVE binding: the pane's
    // own CLAUDE_CONFIG_DIR must now be the target profile's. A restart that
    // reported success but left the old pane in place (a daemon cascading
    // from a stale snapshot) keeps spending the SOURCE account and must never
    // pass silently. An unreadable pane env fails safe to "not diverged", as
    // in the pre-move check.
    let live = live_config_dir(&moved);
    if config_dir_diverged(live.as_deref(), expected.as_deref()) {
        bail!(
            "{}",
            move_incomplete_message(
                &title,
                &id,
                &owner,
                &target,
                live.as_deref(),
                "the live pane's CLAUDE_CONFIG_DIR is not the target profile's",
            )
        );
    }
    match live {
        Some(l) => println!(
            "✓ Live account re-bound: '{}' ({}) runs under '{}' (CLAUDE_CONFIG_DIR={}).",
            title, id, target, l
        ),
        None => println!(
            "✓ Restarted under '{}' (live CLAUDE_CONFIG_DIR not readable from here; \
             confirm with `aoe -p {} list`).",
            target, target
        ),
    }
    Ok(())
}

/// The one message a failed re-bind prints. Loud on purpose: the record is
/// already in `target`, but the pane is (or may be) still running under
/// `source`, i.e. still spending that account, until the operator runs the
/// restart named here.
fn move_incomplete_message(
    title: &str,
    id: &str,
    source: &str,
    target: &str,
    live: Option<&str>,
    why: &str,
) -> String {
    let live_part = live
        .map(|l| format!(" (live CLAUDE_CONFIG_DIR={l})"))
        .unwrap_or_default();
    format!(
        "MOVE INCOMPLETE: the record for '{title}' ({id}) now lives in profile '{target}', \
         but its live pane is STILL running under '{source}'{live_part}: {why}. \
         The old account keeps being used until you run:\n  aoe -p {target} session restart {id}"
    )
}

/// Flips one boolean marker on a session and reports it with `verb`.
async fn mark_session(
    profile: &str,
    args: SessionIdArgs,
    verb: &str,
    apply: fn(&mut Instance),
) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            apply(inst);
            Ok(inst.title.clone())
        })
    })?;
    println!("{verb}: {title}");
    Ok(())
}

/// `aoe session urgent-ack <id>` — clear the hook-written urgent flag on a
/// session, sticky kinds included (WO#1832). The registry is read only to
/// resolve the identifier; the flag lives in the hook status dir, not in
/// sessions.json, so nothing is persisted here.
async fn urgent_ack_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    let ack = crate::hooks::ack_hook_urgent(&inst.id);
    println!(
        "Urgent ack for '{}' ({}): {}",
        inst.title,
        inst.id,
        ack.as_str()
    );
    Ok(())
}

async fn set_color_session(profile: &str, args: SetColorArgs) -> Result<()> {
    let normalized = args.color.trim().to_lowercase();
    let new_color = match normalized.as_str() {
        "none" | "clear" | "" => None,
        other => Some(other.to_string()),
    };

    let storage = Storage::open_unwatched(profile)?;
    let (title, color) = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.set_color(new_color.clone())
                .map_err(|e| anyhow::anyhow!(e))?;
            Ok((inst.title.clone(), inst.color.clone()))
        })
    })?;

    match color {
        Some(c) => println!("✓ Set color for '{}': {}", title, c),
        None => println!("✓ Cleared color for '{}'", title),
    }
    Ok(())
}

async fn archive_session(profile: &str, args: ArchiveArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();
    let inst = inst.clone();

    // WO#1953: a kept session refuses archive before any teardown — unless
    // a person said `--confirm-kept` (WO#1980-1), which clears the flag
    // (logged) and proceeds in this one command.
    let mut inst = inst;
    if let Some(refusal) = inst.keep_refusal("archive") {
        if !args.confirm_kept {
            bail!("{}", refusal.message());
        }
        clear_keep_for_override(profile, &id, "archive").await?;
        inst.unkeep();
    }

    // Serialize teardown and the archive commit as one lifecycle transition.
    let _lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&id)
        .context("failed to acquire instance archive lock")?;
    if !args.no_kill {
        if let Err(e) = inst.kill_locked() {
            eprintln!("Warning: failed to kill agent tmux session: {}", e);
        }
        inst.kill_ancillary_tmux_sessions_locked();
    }

    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.archive();
            stored.lifecycle_generation = stored.lifecycle_generation.saturating_add(1);
            Ok(true)
        } else {
            Ok(false)
        }
    })?;
    if landed {
        println!("Archived: {}", title);
        Ok(())
    } else {
        bail!(
            "Session {} was removed by another process before archive could land",
            title
        );
    }
}

async fn unarchive_session(profile: &str, args: UnarchiveArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    // Board-wide title hygiene (WO#2205): restoring a row whose title another
    // non-trashed row already carries would put a duplicate back on the
    // board, so it is refused unless the caller opts in.
    if !args.allow_duplicate {
        let instances = storage.load()?;
        let target = super::resolve_session(&args.identifier, &instances)?;
        let rows = crate::session::load_all_profile_rows()?;
        if let Some(hit) = crate::session::find_title_collision(
            rows.iter().map(|(profile, inst)| (profile.as_str(), inst)),
            &target.title,
            Some(&target.id),
        ) {
            return Err(crate::session::title_collision_error(&target.title, &hit));
        }
    }
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.unarchive();
            Ok(inst.title.clone())
        })
    })?;
    println!("Unarchived: {}", title);
    Ok(())
}

async fn restore_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let trashed: Vec<_> = instances
        .iter()
        .filter(|i| i.is_trashed())
        .cloned()
        .collect();
    let mut inst = super::resolve_session(&args.identifier, &trashed)
        .map_err(|_| anyhow::anyhow!("No trashed session matching '{}'", args.identifier))?
        .clone();
    let restore_id = inst.id.clone();

    let _lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&restore_id)
        .context("failed to acquire instance restore lock")?;
    let decision = storage.update(|instances, _groups| {
        crate::session::claim::decide_restore_claim(instances, &restore_id, chrono::Utc::now())
            .map_err(anyhow::Error::new)
    })?;
    let restore_generation = match decision {
        crate::session::claim::RestoreClaimDecision::AlreadyGone => {
            anyhow::bail!("No trashed session matching '{}'", args.identifier)
        }
        crate::session::claim::RestoreClaimDecision::Busy(holder) => anyhow::bail!(
            "Session {} is {}, so it was not restored",
            inst.title,
            holder.busy_reason()
        ),
        crate::session::claim::RestoreClaimDecision::Claimed(generation) => generation,
    };

    if let crate::session::trash::RestoreOutcome::Failed { reason } =
        crate::session::trash::restore_worktree_location(&mut inst)
    {
        release_restore_reservation(&storage, &restore_id, restore_generation);
        anyhow::bail!("Cannot restore worktree: {reason}");
    }
    let restored_path = inst.project_path.clone();
    let restored_pre = inst.pre_trash_project_path.clone();

    let commit = storage.update(|instances, _groups| {
        Ok(crate::session::claim::finalize_restore_commit(
            instances,
            &restore_id,
            restore_generation,
            &restored_path,
            &restored_pre,
        ))
    })?;
    match commit {
        crate::session::claim::RestoreCommit::Committed => {}
        crate::session::claim::RestoreCommit::Superseded => anyhow::bail!(
            "Session {} lost its lifecycle reservation during restore",
            inst.title
        ),
        crate::session::claim::RestoreCommit::AlreadyGone => {
            anyhow::bail!("No trashed session matching '{}'", args.identifier)
        }
    }
    println!("Restored: {}", inst.title);
    Ok(())
}

fn release_restore_reservation(storage: &Storage, restore_id: &str, generation: u64) {
    let _ = storage.update(|instances, _groups| {
        if let Some(stored) = instances
            .iter_mut()
            .find(|instance| instance.id == restore_id)
        {
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation);
        }
        Ok(())
    });
}

async fn list_trash(profile: &str) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let trashed: Vec<_> = instances.iter().filter(|i| i.is_trashed()).collect();
    if trashed.is_empty() {
        println!("Trash is empty.");
        return Ok(());
    }
    println!("Trashed sessions in profile '{}':", storage.profile());
    for inst in trashed {
        let when = inst
            .trashed_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "?".to_string());
        println!("  {}  {}  (trashed {})", inst.id, inst.title, when);
    }
    Ok(())
}

async fn empty_trash(profile: &str) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let mut trashed: Vec<_> = instances
        .iter()
        .filter(|i| i.is_trashed())
        .cloned()
        .collect();
    for instance in &mut trashed {
        instance.source_profile = storage.profile().to_string();
    }
    trashed.sort_by(|left, right| left.id.cmp(&right.id));
    if trashed.is_empty() {
        println!("Trash is empty.");
        return Ok(());
    }

    let mut removed = 0usize;
    let mut restored_after_teardown = 0usize;
    let mut kept_for_retry = 0usize;
    let mut being_restored_elsewhere = 0usize;
    let mut being_purged_elsewhere = 0usize;
    for inst in &trashed {
        let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            profile,
            std::path::Path::new(&inst.project_path),
        );
        let delete_worktree =
            config.worktree.auto_cleanup && inst.has_managed_worktree_or_workspace();
        let delete_branch = delete_worktree && config.worktree.delete_branch_on_cleanup;
        let delete_sandbox =
            inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) && config.sandbox.auto_cleanup;
        let row_storage = Storage::open_unwatched(profile)?;
        let reservation = crate::session::deletion::PurgeTransaction::reserve(
            row_storage,
            crate::session::deletion::DeletionRequest {
                session_id: inst.id.clone(),
                instance: inst.clone(),
                delete_worktree,
                delete_branch,
                delete_sandbox,
                force_delete: true,
                detach_hooks: false,
                keep_scratch: false,
            },
        )?;
        let transaction = match reservation {
            crate::session::deletion::PurgeReservation::Reserved(transaction) => transaction,
            crate::session::deletion::PurgeReservation::Rejected(result) => {
                match result.disposition {
                    crate::session::deletion::DeletionDisposition::KeptRestored => {
                        being_restored_elsewhere += 1;
                    }
                    crate::session::deletion::DeletionDisposition::Busy => {
                        being_purged_elsewhere += 1;
                    }
                    _ => {}
                }
                continue;
            }
        };
        let result = transaction.run_hooks().complete_with(|instance| {
            super::purge_acp_transcript(instance).map_err(|error| {
                format!("transcript not purged, keeping session in trash: {error}")
            })
        });
        for err in &result.errors {
            eprintln!("Warning ({}): {}", inst.title, err);
        }
        match result.disposition {
            crate::session::deletion::DeletionDisposition::Removed => removed += 1,
            crate::session::deletion::DeletionDisposition::KeptRestored => {
                if result.teardown_started {
                    restored_after_teardown += 1;
                } else {
                    being_restored_elsewhere += 1;
                }
            }
            crate::session::deletion::DeletionDisposition::Busy => {
                being_purged_elsewhere += 1;
            }
            crate::session::deletion::DeletionDisposition::Failed => kept_for_retry += 1,
            crate::session::deletion::DeletionDisposition::AlreadyGone => {}
        }
    }
    let outcome = super::EmptyTrashOutcome {
        removed,
        restored_after_teardown,
        kept_for_retry,
    };
    if outcome.restored_after_teardown > 0 {
        eprintln!(
            "Warning: {} session(s) were restored mid-purge after teardown began; kept the \
             restored records, but their worktree, branch, container, or transcript may already \
             have been removed. Inspect and repair them.",
            outcome.restored_after_teardown
        );
    }
    let mut parts = vec![format!("purged {} session(s)", outcome.removed)];
    if outcome.kept_for_retry > 0 {
        parts.push(format!("kept {} for retry", outcome.kept_for_retry));
    }
    if being_restored_elsewhere > 0 {
        parts.push(format!(
            "{being_restored_elsewhere} being restored by another process"
        ));
    }
    if being_purged_elsewhere > 0 {
        parts.push(format!(
            "{being_purged_elsewhere} being purged by another process"
        ));
    }
    if outcome.restored_after_teardown > 0 {
        parts.push(format!(
            "{} restored mid-purge",
            outcome.restored_after_teardown
        ));
    }
    println!(
        "Emptied trash: {} (profile '{}').",
        parts.join(", "),
        storage.profile()
    );
    Ok(())
}

async fn snooze_session(profile: &str, args: SnoozeArgs) -> Result<()> {
    let config = crate::session::config::profile_config::resolve_config(profile)?;

    let raw_minutes = args
        .minutes
        .map(|m| m as u64)
        .unwrap_or(config.session.snooze_duration_minutes as u64);
    crate::session::validate_snooze_duration(raw_minutes).map_err(|e| anyhow::anyhow!("{}", e))?;
    let minutes = raw_minutes as u32;

    let storage = Storage::open_unwatched(profile)?;
    // WO#1980-1: resolve first so `--confirm-kept` can clear the flag
    // (daemon-first, logged) before the locked snooze below re-checks it.
    if args.confirm_kept {
        let (instances, _groups) = storage.load_with_groups()?;
        let inst = super::resolve_session(&args.identifier, &instances)?;
        if inst.is_kept() {
            clear_keep_for_override(profile, &inst.id, "snooze").await?;
        }
    }
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            // WO#1953: a kept session refuses snooze.
            if let Some(refusal) = inst.keep_refusal("snooze") {
                bail!("{}", refusal.message());
            }
            inst.snooze(minutes);
            Ok(inst.title.clone())
        })
    })?;
    println!("Snoozed for {}m: {}", minutes, title);
    Ok(())
}

async fn unsnooze_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &args.identifier, |inst| {
            inst.unsnooze();
            Ok(inst.title.clone())
        })
    })?;
    println!("Woke: {}", title);
    Ok(())
}

async fn start_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "start")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();

    let _ = working.start_with_size_opts(crate::terminal::get_size(), false)?;

    let file_watch = crate::file_watch::FileWatchService::noop();
    crate::session::sync::capture_launched_session_id_blocking(
        &mut working,
        &file_watch,
        crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
        true,
    );

    let title = working.title.clone();
    let id = working.id.clone();

    let _merge_lock = storage
        .acquire_instance_lifecycle_lock(&id)
        .context("failed to acquire instance start merge lock")?;
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.merge_post_start(&working);
            Ok(true)
        } else {
            tracing::warn!(
                target: "session.cli",
                session_id = %id,
                "session row removed by peer between phase 1 and phase 3 of start; tmux session is now orphan"
            );
            Ok(false)
        }
    })?;
    if !landed {
        bail!(
            "Session {} was removed by another process before start could land; tmux session is now orphan",
            title
        );
    }

    println!("✓ Started session: {}", title);
    Ok(())
}

fn bail_if_acp(inst: &crate::session::Instance, verb: &str) -> Result<()> {
    if inst.is_structured() {
        bail!(
            "structured view sessions are managed by `aoe serve`; \
             cannot `aoe session {verb}` from the CLI.\n\
             The ACP worker is auto-spawned within ~2s of an structured-view session \
             while serve is running, or on next `aoe serve` startup.\n\
             To control an structured-view session, use the web dashboard or the REST API."
        );
    }
    Ok(())
}

fn resolve_import_roots(paths: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let raw: Vec<std::path::PathBuf> = if paths.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        paths.iter().map(std::path::PathBuf::from).collect()
    };
    Ok(raw
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect())
}

fn already_imported(instances: &[Instance], id: &str) -> bool {
    instances.iter().any(|inst| {
        if inst.agent_session_id.as_deref() == Some(id) {
            return true;
        }
        if matches!(&inst.resume_intent, ResumeIntent::Use(s) if s == id) {
            return true;
        }
        if inst.acp_session_id.as_deref() == Some(id) {
            return true;
        }
        false
    })
}

fn build_import_instance(
    s: &crate::session::claude_import::ClaudeSessionSummary,
    structured: bool,
    group: &str,
) -> Instance {
    let title = s.title.clone().unwrap_or_else(|| {
        let short = s.session_id.get(..8).unwrap_or(s.session_id.as_str());
        format!("Claude import {short}")
    });
    let mut inst = Instance::new(&title, &s.cwd);
    inst.tool = "claude".to_string();
    if !group.is_empty() {
        inst.group_path = group.to_string();
    }
    apply_import_mode(&mut inst, s, structured);
    inst
}

fn apply_import_mode(
    inst: &mut Instance,
    s: &crate::session::claude_import::ClaudeSessionSummary,
    structured: bool,
) {
    if structured {
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some(s.session_id.clone());
        inst.import_pending = Some(true);
    } else {
        inst.resume_intent = ResumeIntent::Use(s.session_id.clone());
        inst.resume_binding = Some(crate::session::ConversationBinding {
            session_id: s.session_id.clone(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "claude".into(),
                stores: vec![s.config_dir.clone()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: crate::session::capture::canonicalize_or_raw(&s.cwd),
                filesystem: "host".into(),
                cwd_filesystem: "host".into(),
            }),
            provenance: crate::session::ConversationProvenance::Imported,
            transcript_path: None,
        });
    }
}

async fn import_sessions(profile: &str, args: ImportArgs) -> Result<()> {
    use crate::session::claude_import::{scan_sessions, sessions_under_paths, MAX_SESSIONS};

    let structured = args.structured;

    let mut discovered = scan_sessions();
    if !args.all {
        let roots = resolve_import_roots(&args.paths)?;
        discovered = sessions_under_paths(discovered, &roots);
    }

    let (candidates, missing_cwd): (Vec<_>, Vec<_>) =
        discovered.into_iter().partition(|s| s.cwd_exists);

    let (existing, _groups) = Storage::open_unwatched(profile)?.load_with_groups()?;
    let candidate_count = candidates.len();
    let mut to_import: Vec<_> = candidates
        .into_iter()
        .filter(|s| !already_imported(&existing, &s.session_id))
        .collect();
    let already = candidate_count - to_import.len();

    let capped = to_import.len() > MAX_SESSIONS;
    if capped {
        to_import.truncate(MAX_SESSIONS);
    }

    let report_skipped = || {
        if already > 0 {
            println!("  ({already} already imported, skipped)");
        }
        if !missing_cwd.is_empty() {
            println!(
                "  ({} skipped: working directory no longer exists)",
                missing_cwd.len()
            );
        }
        if capped {
            println!("  (capped at {MAX_SESSIONS}; narrow the path(s) to import the rest)");
        }
    };

    if to_import.is_empty() {
        println!("No new Claude Code sessions to import.");
        report_skipped();
        return Ok(());
    }

    let kind = if structured { "structured" } else { "terminal" };
    println!(
        "Found {} Claude Code session(s) to import as {kind} sessions:",
        to_import.len()
    );
    for s in &to_import {
        let short = s.session_id.get(..8).unwrap_or(s.session_id.as_str());
        let title = s.title.as_deref().unwrap_or("(no title)");
        println!("  {short}  {title}  [{}]", s.cwd);
    }
    report_skipped();

    if args.dry_run {
        println!("Dry run: nothing created.");
        return Ok(());
    }

    if to_import.len() > 1 && !args.yes {
        use std::io::Write;
        print!("Import {} session(s)? [y/N] ", to_import.len());
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let group = args.group.clone().unwrap_or_default();
    let storage = Storage::open_unwatched(profile)?;
    let created_ids = storage.update(|all_instances, groups| {
        let mut ids = Vec::new();
        for s in &to_import {
            if already_imported(all_instances, &s.session_id) {
                continue;
            }
            let inst = build_import_instance(s, structured, &group);
            ids.push(inst.id.clone());
            all_instances.push(inst.clone());
            if !inst.group_path.is_empty() {
                let mut tree = GroupTree::new_with_groups(all_instances, groups);
                tree.create_group(&inst.group_path);
                *groups = tree.get_all_groups();
            }
        }
        Ok(ids)
    })?;

    println!("✓ Imported {} session(s).", created_ids.len());

    if structured {
        if args.launch {
            println!("Note: --launch is ignored for structured imports.");
        }
        println!(
            "Structured sessions replay their transcript on the next `aoe serve` \
             (auto-spawned within ~2s while serve is running)."
        );
        return Ok(());
    }

    if args.launch {
        launch_imported(profile, &created_ids)?;
    } else if !created_ids.is_empty() {
        println!("Start them with `aoe session start <id>` (or launch on import with --launch).");
    }
    Ok(())
}

fn launch_imported(profile: &str, ids: &[String]) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let file_watch = crate::file_watch::FileWatchService::noop();
    for id in ids {
        let (instances, _groups) = storage.load_with_groups()?;
        let Some(inst) = instances.iter().find(|i| &i.id == id) else {
            continue;
        };
        let mut working = inst.clone();
        working.source_profile = profile.to_string();
        if let Err(e) = working.start_with_size(crate::terminal::get_size()) {
            eprintln!("Warning: failed to start {}: {e}", working.title);
            continue;
        }
        crate::session::sync::capture_launched_session_id_blocking(
            &mut working,
            &file_watch,
            crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
            true,
        );
        let wid = working.id.clone();
        storage.update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|i| i.id == wid) {
                stored.merge_post_start(&working);
            }
            Ok(())
        })?;
        println!("✓ Started {}", working.title);
    }
    Ok(())
}

async fn stop_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "stop")?;
    let mut working = inst.clone();
    working.source_profile = profile.to_string();
    let session_id = inst.id.clone();
    let title = inst.title.clone();
    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;
    let was_running = tmux_session.exists();
    let had_container = inst.is_sandboxed()
        && match crate::containers::DockerContainer::from_session_id(&inst.id).probe_running() {
            crate::containers::Probe::Running | crate::containers::Probe::Unknown(_) => true,
            crate::containers::Probe::NotRunning => false,
        };

    if !was_running && !had_container {
        println!("Session is not running: {}", title);
        return Ok(());
    }

    working.stop()?;

    let landed = storage.load()?.iter().any(|stored| stored.id == session_id);
    if !landed {
        bail!(
            "Session {} was removed by another process before stop could land",
            title
        );
    }

    if had_container {
        println!("✓ Stopped session and container: {}", title);
    } else {
        println!("✓ Stopped session: {}", title);
    }

    Ok(())
}

async fn restart_session_dispatch(profile: &str, args: RestartArgs) -> Result<()> {
    if args.all {
        return restart_all_sessions(profile, args.parallel).await;
    }
    let identifier = args
        .identifier
        .ok_or_else(|| anyhow::anyhow!("session identifier required (or pass --all)"))?;
    restart_session(profile, SessionIdArgs { identifier }).await
}

async fn restart_all_sessions(profile: &str, parallel: usize) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let target_ids = pick_targets_for_restart_all(&instances);
    if target_ids.is_empty() {
        println!("No sessions to restart in profile '{}'.", profile);
        return Ok(());
    }

    let total = target_ids.len();
    let size = crate::terminal::get_size();
    let parallel = parallel.max(1);

    let mut targets: Vec<crate::session::Instance> = Vec::with_capacity(total);
    for id in &target_ids {
        if let Some(inst) = instances.iter().find(|i| &i.id == id) {
            let mut clone = inst.clone();
            clone.source_profile = profile.to_string();
            targets.push(clone);
        }
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut join_set: tokio::task::JoinSet<(
        String,
        Option<crate::session::Instance>,
        Result<StartOutcome>,
    )> = tokio::task::JoinSet::new();

    for mut inst in targets {
        let permit_sem = semaphore.clone();
        join_set.spawn(async move {
            let _permit = permit_sem
                .acquire_owned()
                .await
                .expect("semaphore not closed");
            let title = inst.title.clone();
            let res = tokio::task::spawn_blocking(move || {
                let result = inst.restart_with_size(size);
                if result.is_ok() {
                    let file_watch = crate::file_watch::FileWatchService::noop();
                    crate::session::sync::capture_launched_session_id_blocking(
                        &mut inst,
                        &file_watch,
                        crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
                        false,
                    );
                }
                (inst, result)
            })
            .await;
            match res {
                Ok((inst, result)) => (title, Some(inst), result),
                Err(join_err) => (
                    title,
                    None,
                    Err(anyhow::anyhow!("worker panicked: {}", join_err)),
                ),
            }
        });
    }

    let mut succeeded: Vec<(String, String)> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut fresh_after_failed_resume: Vec<(String, String)> = Vec::new();
    let mut restarted: Vec<crate::session::Instance> = Vec::new();
    while let Some(joined) = join_set.join_next().await {
        let (title, inst_opt, result) = joined.expect("JoinSet shouldn't panic on join itself");
        let id = inst_opt.as_ref().map(|i| i.id.clone()).unwrap_or_default();
        if let Some(inst) = inst_opt {
            restarted.push(inst);
        }
        match result {
            Ok(StartOutcome::ResumeFailed { sid }) => failed.push((
                title,
                format!("resume failed for sid {sid}; preserved for explicit retry"),
            )),
            Ok(StartOutcome::FreshAfterFailedResume { sid }) => {
                fresh_after_failed_resume.push((title.clone(), sid));
                succeeded.push((id, title));
            }
            Ok(StartOutcome::Resumed | StartOutcome::Fresh) => succeeded.push((id, title)),
            Err(e) => failed.push((title, e.to_string())),
        }
    }

    let orphaned: Vec<(String, String)> = storage.update(|instances, _groups| {
        let mut orphaned = Vec::new();
        for restarted_inst in restarted {
            if let Some(stored) = instances.iter_mut().find(|i| i.id == restarted_inst.id) {
                stored.merge_post_restart(&restarted_inst);
            } else {
                tracing::warn!(
                    target: "session.cli",
                    session_id = %restarted_inst.id,
                    "session row removed by peer between phase 1 and phase 3 of restart --all; tmux session is now orphan"
                );
                orphaned.push((restarted_inst.id.clone(), restarted_inst.title.clone()));
            }
        }
        Ok(orphaned)
    })?;

    let orphaned_ids: HashSet<&String> = orphaned.iter().map(|(id, _)| id).collect();
    succeeded.retain(|(id, _)| !orphaned_ids.contains(id));

    println!("✓ Restarted {}/{} sessions:", succeeded.len(), total);
    for (_id, title) in &succeeded {
        println!("  · {}", title);
    }
    if !fresh_after_failed_resume.is_empty() {
        println!(
            "ℹ {} started fresh (a prior resume attempt failed for the stored sid; the old conversation is still reachable via the agent's own resume/history picker):",
            fresh_after_failed_resume.len()
        );
        for (title, sid) in &fresh_after_failed_resume {
            println!("  · {}: sid {}", title, sid);
        }
    }
    if !orphaned.is_empty() {
        println!(
            "⚠ {} orphaned (row removed by peer mid-flight; tmux running but unrooted):",
            orphaned.len()
        );
        for (_, title) in &orphaned {
            println!("  · {}", title);
        }
    }
    if !failed.is_empty() {
        println!("✗ {} failed:", failed.len());
        for (title, err) in &failed {
            println!("  · {}: {}", title, err);
        }
        bail!("{} session(s) failed to restart", failed.len());
    }

    Ok(())
}

fn pick_targets_for_restart_all(instances: &[crate::session::Instance]) -> Vec<String> {
    use crate::session::Status;
    instances
        .iter()
        .filter(|i| !matches!(i.status, Status::Deleting | Status::Creating))
        .filter(|i| !i.is_structured())
        .map(|i| i.id.clone())
        .collect()
}

/// Daemon-first restart: when a local daemon is reachable, hand it the
/// cascade (`POST /api/sessions/{id}/restart`) and only WATCH from here.
/// The CLI's own survival then no longer matters: a harness timeout or a
/// SIGTERM that kills this process mid-poll leaves the cascade running in
/// the daemon instead of stranding a half-torn-down pane behind a lifecycle
/// reservation. Returns Ok(false) when no daemon is reachable or the POST
/// itself was refused, so the caller falls back to the in-process path;
/// never falls back after the daemon accepted.
async fn try_daemon_restart(session_id: &str, title: &str) -> Result<bool> {
    use crate::acp::client::{discovery, HttpClient};

    let Ok(endpoint) = discovery::discover_local() else {
        return Ok(false);
    };
    let Ok(client) = HttpClient::new(endpoint) else {
        return Ok(false);
    };
    if client.restart_session(session_id).await.is_err() {
        return Ok(false);
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok(body) = client.restart_status(session_id).await {
            match body.get("state").and_then(|v| v.as_str()) {
                Some("stale") => {
                    let age = body
                        .get("age_seconds")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    bail!(
                        "Daemon-side restart for {title} is wedged: its in-flight mark is \
                         {age}s old and the owning cascade died without finishing. \
                         Re-run `aoe session restart`; the daemon replaces the stale \
                         mark on the next request."
                    );
                }
                Some("done") => {
                    if body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                        let profile = body
                            .get("profile")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if profile.is_empty() {
                            println!("✓ Restarted session: {} (daemon-side)", title);
                        } else {
                            println!(
                                "✓ Restarted session: {} (daemon-side, profile '{}')",
                                title, profile
                            );
                        }
                        return Ok(true);
                    }
                    let err = body
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    bail!("Daemon-side restart failed for {title}: {err}");
                }
                _ => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "✓ Restart accepted for {}; still settling daemon-side (watch `aoe list` or \
                 GET /api/sessions/{}/restart-status)",
                title, session_id
            );
            return Ok(true);
        }
    }
}

async fn restart_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    // #99 default-launch drift fix: a plain `aoe session restart <id>` (no
    // `-p`, so `profile` is empty) must operate on — and re-bind under — the
    // profile that ACTUALLY owns the session, not the globally-resolved
    // default profile. The previous code loaded the default profile's storage
    // and stamped `source_profile = ""`, which re-homed every plain restart
    // onto the default account and clustered the whole fleet on one wallet.
    // An explicit `-p <profile>` still overrides (intentional migration — the
    // path `aoe session move` relies on).
    let owning_profile = if profile.is_empty() {
        match find_session_across_profiles(&args.identifier) {
            Ok((p, _)) => p,
            // Not found in any profile's sessions.json: fall back to the
            // resolved default so the normal "session missing" error surfaces
            // from resolve_session below rather than a confusing lookup error.
            Err(_) => crate::session::config::effective_profile(profile),
        }
    } else {
        profile.to_string()
    };
    let storage = Storage::open_unwatched(&owning_profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "restart")?;

    // Daemon-first: the cascade must not depend on this process surviving.
    if try_daemon_restart(&inst.id, &inst.title).await? {
        return Ok(());
    }
    let mut working = inst.clone();
    working.source_profile = owning_profile.clone();

    let outcome = working.restart_with_resume_policy(
        crate::terminal::get_size(),
        false,
        crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting,
    )?;
    let title = working.title.clone();
    let session_id = working.id.clone();
    let tool = working.tool.clone();

    // Resolve the configured wake message (global default with per-profile
    // override). Empty string is the documented opt-out: the restart still
    // runs but no keys are sent.
    let wake_msg = crate::session::resolve_config(&owning_profile)
        .map(|c| c.session.restart_wake_message.clone())
        .unwrap_or_else(|_| "wake up: pick up what you were doing".to_string());

    let mut wake_succeeded = false;
    if !wake_msg.is_empty() && !matches!(outcome, StartOutcome::ResumeFailed { .. }) {
        let tmux_session = crate::tmux::Session::new(&session_id, &title)?;
        tmux_session.wait_until_ready(
            std::time::Duration::from_secs(5),
            crate::agents::ready_marker(&tool),
        );

        if tmux_session.exists() {
            let delay = crate::agents::send_keys_enter_delay(&tool);
            match tmux_session.send_keys_with_delay(&wake_msg, delay) {
                Ok(()) => {
                    wake_succeeded = true;
                }
                Err(e) => {
                    eprintln!("Warning: failed to send wake-up message: {}", e);
                }
            }
        }
    }

    let file_watch = crate::file_watch::FileWatchService::noop();
    crate::session::sync::capture_launched_session_id_blocking(
        &mut working,
        &file_watch,
        crate::session::sync::CLI_SESSION_ID_CAPTURE_TIMEOUT,
        true,
    );

    let _merge_lock = storage
        .acquire_instance_lifecycle_lock(&session_id)
        .context("failed to acquire instance restart merge lock")?;
    let landed = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|i| i.id == session_id) {
            stored.merge_post_restart(&working);
            if wake_succeeded {
                stored.touch_last_accessed();
            }
            Ok(true)
        } else {
            tracing::warn!(
                target: "session.cli",
                session_id = %session_id,
                "session row removed by peer between phase 1 and phase 3 of restart; tmux session is now orphan"
            );
            Ok(false)
        }
    })?;
    if !landed {
        bail!(
            "Session {} was removed by another process before restart could land; tmux session is now orphan",
            title
        );
    }

    match outcome {
        StartOutcome::ResumeFailed { sid } => {
            bail!("Resume failed for sid {sid}; preserved for explicit retry");
        }
        StartOutcome::FreshAfterFailedResume { sid } => {
            println!(
                "✓ Restarted session: {} (started fresh; a prior resume attempt failed for sid {sid}, the old conversation is still reachable via the agent's own resume/history picker)",
                title
            );
        }
        StartOutcome::Resumed | StartOutcome::Fresh => {
            println!("✓ Restarted session: {}", title);
        }
    }
    Ok(())
}

fn supervise_attach_capture(
    inst: &mut Instance,
    attach: impl FnOnce(&Instance) -> Result<()>,
) -> Result<()> {
    inst.maybe_start_poller();
    let result = attach(inst);
    inst.stop_and_flush_poller();
    result
}

async fn attach_session(profile: &str, args: SessionIdArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let inst = super::resolve_session(&args.identifier, &instances)?;
    bail_if_acp(inst, "attach")?;
    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;

    if !tmux_session.exists() {
        bail!(
            "Session is not running. Start it first with: aoe session start {}",
            args.identifier
        );
    }

    let mut working = inst.clone();
    working.source_profile = profile.to_string();
    supervise_attach_capture(&mut working, |_| tmux_session.attach())
}

async fn show_session(profile: &str, args: ShowArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let mut inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?.clone()
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());

        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
                .clone()
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };
    inst.source_profile = storage.profile().to_string();

    crate::session::config::profile_config::resolve_config_or_warn(profile);

    crate::tmux::refresh_session_cache();
    inst.update_status_once(None, None);
    let contended = crate::session::Instance::contended_capture_cwds(&instances);
    inst.self_heal_session_id(profile, &contended);

    if args.json {
        let mut details = session_details(&inst, storage.profile());
        let usage = crate::session::account::daemon_usage_map().await;
        let mut map = crate::session::account::local_session_accounts(
            std::iter::once(&inst),
            storage.profile(),
            &usage,
        );
        if let Some(a) = map.remove(&inst.id) {
            details.account = a;
        }
        super::output::print_json(&details)?;
    } else {
        println!("Session: {}", inst.title);
        println!("  ID:      {}", inst.id);
        println!("  Path:    {}", inst.project_path);
        println!("  Group:   {}", inst.group_path);
        println!("  Tool:    {}", inst.tool);
        println!("  Command: {}", inst.command);
        println!("  Status:  {:?}", inst.status);
        if let Some(at) = inst.trashed_at.or(inst.archived_at) {
            println!(
                "  State:   {} ({})",
                super::list::state_tag(&inst),
                at.to_rfc3339()
            );
        }
        println!("  Profile: {}", storage.profile());
        for line in relationship_lines(&inst, &instances) {
            println!("{line}");
        }
    }

    Ok(())
}

fn relationship_lines(
    inst: &crate::session::Instance,
    instances: &[crate::session::Instance],
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(parent_id) = &inst.parent_session_id {
        lines.push(match instances.iter().find(|i| &i.id == parent_id) {
            Some(parent) => format!("  Parent:  {} ({parent_id})", parent.title),
            None => format!("  Parent:  {parent_id}"),
        });
    }
    let mut children = instances
        .iter()
        .filter(|i| i.parent_session_id.as_deref() == Some(inst.id.as_str()))
        .peekable();
    if children.peek().is_some() {
        lines.push("  Children:".to_string());
        lines.extend(children.map(|child| format!("    {} ({})", child.title, child.id)));
    }
    lines
}

async fn capture_session(profile: &str, args: CaptureArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());

        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    crate::session::config::profile_config::resolve_config_or_warn(profile);

    let tmux_session = crate::tmux::Session::new(&inst.id, &inst.title)?;

    let (content, status) = if !tmux_session.exists() {
        (String::new(), "stopped".to_string())
    } else {
        let raw = tmux_session.capture_pane(args.lines)?;
        let hook_alias =
            crate::tmux::status_rules::effective_detect_as(profile, &inst.tool, &inst.detect_as);
        let manifest_tool: &str = if hook_alias.is_empty() {
            &inst.tool
        } else {
            &hook_alias
        };
        let rules_tool =
            crate::tmux::status_rules::detection_tool(profile, &inst.tool, &inst.detect_as);
        let hook = crate::hooks::read_hook_status(&inst.id).map(|status| {
            crate::tmux::detect::HookObservation {
                status,
                age: crate::hooks::read_hook_status_age(&inst.id),
            }
        });
        let status = if crate::tmux::detect::has_manifest(manifest_tool) {
            let status_raw;
            let status_content = if args.lines >= 50 {
                raw.as_str()
            } else {
                status_raw = tmux_session
                    .capture_pane(50)
                    .unwrap_or_else(|_| raw.clone());
                status_raw.as_str()
            };
            let osc_title = crate::tmux::utils::pane_title(tmux_session.name()).unwrap_or_default();
            crate::tmux::detect_with_rules(
                profile,
                &rules_tool,
                manifest_tool,
                &crate::tmux::utils::strip_ansi(status_content),
                &osc_title,
                hook,
            )
            .and_then(|d| d.status)
            .unwrap_or_default()
        } else {
            let hook = hook.filter(|_| !crate::tmux::status_rules::has_rules(profile, &rules_tool));
            match hook {
                Some(hook) => hook.status,
                None => tmux_session
                    .detect_status(profile, &rules_tool)
                    .unwrap_or_default(),
            }
        };
        let content = if args.strip_ansi {
            crate::tmux::utils::strip_ansi(&raw)
        } else {
            raw
        };
        (content, format!("{:?}", status).to_lowercase())
    };

    if args.json {
        let output = CaptureOutput {
            id: inst.id.clone(),
            title: inst.title.clone(),
            status,
            tool: inst.tool.clone(),
            content,
            lines: args.lines,
        };
        super::output::print_json(&output)?;
    } else {
        print!("{}", content);
    }

    Ok(())
}

fn rename_success_message(
    persisted_old_title: &str,
    committed_title: &str,
    title_requested: bool,
) -> String {
    if title_requested && persisted_old_title != committed_title {
        format!("✓ Renamed session: {persisted_old_title} → {committed_title}")
    } else {
        format!("✓ Updated session: {committed_title}")
    }
}

async fn rename_session(profile: &str, args: RenameArgs) -> Result<()> {
    if args.title.is_none() && args.group.is_none() && args.branch.is_none() {
        bail!("At least one of --title, --group or --branch must be specified");
    }

    let storage = Storage::open_unwatched(profile)?;

    let (instances, _groups) = storage.load_with_groups()?;
    let inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?.clone()
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());

        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let id = inst.id.clone();
    let title_requested = args.title.is_some();
    let session_lock_required = title_requested || args.rename_branch || args.branch.is_some();

    let _identity_lock = acquire_session_identity_lock()?;
    let _session_title_lock = if session_lock_required {
        Some(
            crate::session::acquire_session_title_lock(&id)
                .context("failed to acquire session title lock")?,
        )
    } else {
        None
    };
    let _lifecycle_lock = if session_lock_required {
        Some(
            storage
                .acquire_instance_lifecycle_lock(&id)
                .context("failed to acquire session lifecycle lock")?,
        )
    } else {
        None
    };
    let (authoritative_instances, _groups) = storage.load_with_groups()?;
    let inst = authoritative_instances
        .iter()
        .find(|instance| instance.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
    let mut inst = inst.clone();
    if let Err(error) = crate::session::worktree_reconcile::reconcile_and_persist(
        &storage,
        &mut inst,
        &mut Default::default(),
    ) {
        tracing::warn!(target: "cli.session", session = %id, "worktree path reconciliation skipped: {error}");
    }
    let old_title = inst.title.clone();
    let effective_title = args
        .title
        .clone()
        .unwrap_or_else(|| old_title.clone())
        .trim()
        .to_string();
    let new_group = args.group.as_ref().map(|g| g.trim().to_string());
    let title_changed = old_title != effective_title;

    let config = crate::session::config::profile_config::resolve_config_or_warn(profile);
    let tied = inst.tie_workdir_applies(config.session.tie_workdir_to_name);
    let tied_edit = args.branch.is_none() && tied && (args.title.is_some() || args.rename_branch);
    let duplicate_path = if tied_edit {
        crate::session::worktree_edit::derived_worktree_path(
            std::path::Path::new(&inst.project_path),
            &effective_title,
        )
    } else {
        inst.project_path.clone()
    };
    let pair_changed = title_changed
        || duplicate_path.trim_end_matches('/') != inst.project_path.trim_end_matches('/');
    if pair_changed
        && is_duplicate_session(
            authoritative_instances.iter(),
            &effective_title,
            &duplicate_path,
            Some(&id),
        )
    {
        return Err(duplicate_session_error(&effective_title));
    }

    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;
    if let Some(branch) = &args.branch {
        let info = inst
            .worktree_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Branch-only rename requires a managed worktree"))?;
        if branch != &info.branch {
            let path = std::path::Path::new(&inst.project_path).canonicalize()?;
            let repo = std::path::Path::new(&info.main_repo_path).canonicalize()?;
            let same_branch = |main_repo: &str, name: &str| {
                name == info.branch
                    && std::path::Path::new(main_repo).canonicalize().ok().as_ref() == Some(&repo)
            };
            for other_profile in crate::session::list_profiles()? {
                let rows = Storage::open_unwatched(&other_profile)?.load()?;
                if rows.iter().any(|other| {
                    other.id != id
                        && !other.is_trashed()
                        && (std::path::Path::new(&other.project_path)
                            .canonicalize()
                            .ok()
                            .as_ref()
                            == Some(&path)
                            || other
                                .worktree_info
                                .as_ref()
                                .is_some_and(|wt| same_branch(&wt.main_repo_path, &wt.branch))
                            || other.workspace_info.as_ref().is_some_and(|workspace| {
                                workspace
                                    .repos
                                    .iter()
                                    .any(|wt| same_branch(&wt.main_repo_path, &wt.branch))
                            }))
                }) {
                    bail!("Another session shares this branch or worktree in profile {other_profile}; rename is not isolated");
                }
            }
        }
        if crate::session::worktree_edit::rename_worktree_branch(
            info,
            std::path::Path::new(&inst.project_path),
            branch,
        )? {
            new_branch = Some(branch.clone());
        }
    } else if tied_edit {
        let current_path = inst.project_path.clone();
        let worktree_info = inst
            .worktree_info
            .clone()
            .expect("tie_workdir_applies implies worktree_info is Some");
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
        let moves_worktree = crate::session::worktree_edit::worktree_move_required(
            std::path::Path::new(&current_path),
            &leaf,
        );
        let renames_branch = crate::session::worktree_edit::worktree_branch_rename_required(
            &worktree_info,
            &leaf,
            args.rename_branch,
        );
        let is_sandboxed = inst.is_sandboxed();
        if moves_worktree || renames_branch {
            let mut live = inst.clone();
            live.source_profile = profile.to_string();
            crate::tmux::refresh_session_cache();
            live.update_status_with_metadata(None, None);
            let container_holds = !live.status.blocks_worktree_edit()
                && moves_worktree
                && crate::session::worktree_edit::ensure_sandbox_container_released(
                    &id,
                    is_sandboxed,
                );
            if live.status.blocks_worktree_edit() || container_holds {
                bail!("Stop the session before renaming its worktree directory or branch. Disable session.tie_workdir_to_name to relabel a running session.");
            }
        }
        match crate::session::worktree_edit::edit_worktree_workdir(
            crate::session::worktree_edit::WorktreeEditRequest {
                worktree_info: &worktree_info,
                current_path: std::path::Path::new(&current_path),
                new_name: &leaf,
                rename_branch: args.rename_branch,
            },
        ) {
            Ok(outcome) => {
                if outcome.new_path != std::path::Path::new(&current_path) {
                    crate::session::worktree_edit::discard_sandbox_container_after_move(
                        &id,
                        is_sandboxed,
                    );
                }
                new_path = Some(outcome.new_path.to_string_lossy().to_string());
                new_branch = outcome.new_branch;
            }
            Err(crate::session::worktree_edit::WorktreeEditError::Unchanged) => {}
            Err(e) => return Err(e.into()),
        }
    } else if args.rename_branch {
        bail!("--rename-branch only applies to a tied aoe-managed worktree session (session.tie_workdir_to_name)");
    }

    let persist = storage.update(|instances, groups| {
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
        let persisted_old_title = inst.title.clone();
        if title_requested {
            inst.title = effective_title.clone();
        }
        if let Some(path) = &new_path {
            inst.project_path = path.clone();
        }
        if let Some(branch) = &new_branch {
            if let Some(wt) = inst.worktree_info.as_mut() {
                wt.branch = branch.clone();
            }
        }
        if let Some(group) = &new_group {
            inst.group_path = group.clone();
        }
        let committed_title = inst.title.clone();
        let group_path = inst.group_path.clone();
        if !group_path.is_empty() {
            let mut group_tree = GroupTree::new_with_groups(instances, groups);
            group_tree.create_group(&group_path);
            *groups = group_tree.get_all_groups();
        }
        Ok((persisted_old_title, committed_title))
    });
    let (persisted_old_title, committed_title) = match persist {
        Ok(titles) => titles,
        Err(error) => {
            if args.branch.is_some() {
                if let Some(branch) = &new_branch {
                    let info = inst
                        .worktree_info
                        .as_ref()
                        .expect("branch rename checked worktree metadata");
                    let stored = storage.load().map_err(|_| anyhow::anyhow!("Session metadata write failed ({error}) and its state cannot be read. Branch is {branch}; directory unchanged. Inspect before retrying."))?;
                    let persisted_branch = stored
                        .iter()
                        .find(|row| row.id == id)
                        .and_then(|row| row.worktree_info.as_ref())
                        .map(|wt| wt.branch.as_str());
                    if persisted_branch == Some(info.branch.as_str()) {
                        if let Err(rollback) =
                            crate::session::worktree_edit::rollback_worktree_branch(
                                info,
                                std::path::Path::new(&inst.project_path),
                                branch,
                            )
                        {
                            bail!("Session metadata failed: {error}; branch rollback also failed: {rollback}. The directory is unchanged; inspect Git and session metadata before continuing.");
                        }
                    } else if persisted_branch != Some(branch.as_str()) {
                        bail!("Session metadata failed: {error}; its branch changed concurrently. Directory unchanged; inspect before continuing.");
                    }
                }
            }
            if let Some(path) = &new_path {
                bail!("Worktree was moved on disk to {path}, but persisting the new session metadata failed: {error}. Re-run to retry.");
            }
            return Err(error);
        }
    };
    drop(_identity_lock);

    let committed_title_changed = title_requested && persisted_old_title != committed_title;
    if committed_title_changed {
        let rekey_id = id.clone();
        let rekey_old_title = persisted_old_title.clone();
        let rekey_new_title = committed_title.clone();
        match tokio::task::spawn_blocking(move || {
            crate::tmux::rekey_session(&rekey_id, &rekey_old_title, &rekey_new_title)
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("Warning: failed to rename tmux session: {error}"),
            Err(error) => eprintln!("Warning: tmux rename task failed: {error}"),
        }
    }

    if args.branch.is_some() {
        if let Some(branch) = &new_branch {
            println!("✓ Branch renamed to: {branch} (worktree directory unchanged)");
        }
    }
    if let Some(path) = &new_path {
        println!("✓ Worktree moved to: {}", path);
        if let Some(branch) = &new_branch {
            println!("  Branch renamed to: {}", branch);
        }
    }
    println!(
        "{}",
        rename_success_message(&persisted_old_title, &committed_title, title_requested,)
    );

    Ok(())
}

#[cfg(test)]
mod rename_tests {
    use super::{rename_session, rename_success_message, RenameArgs};
    use crate::session::{Instance, Status, Storage};
    use serial_test::serial;

    fn args(
        id: &str,
        title: Option<&str>,
        group: Option<&str>,
        branch: Option<&str>,
    ) -> RenameArgs {
        RenameArgs {
            identifier: Some(id.to_string()),
            title: title.map(str::to_owned),
            group: group.map(str::to_owned),
            rename_branch: false,
            branch: branch.map(str::to_owned),
        }
    }

    #[tokio::test]
    #[serial]
    async fn branch_rename_preserves_worktree_and_updates_metadata() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("agent-fixed");
        std::fs::create_dir(&repo).unwrap();
        let git = |path: &std::path::Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(path)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&repo, &["init", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "agent-old",
                worktree.to_str().unwrap(),
            ],
        );
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        std::fs::write(worktree.join("uncommitted.txt"), "keep me").unwrap();
        let storage = Storage::new_unwatched("branch-only").unwrap();
        let mut target = Instance::new("Old Title", worktree.to_str().unwrap());
        target.status = Status::Running;
        target.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "agent-old".into(),
            main_repo_path: repo.to_str().unwrap().into(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: Some("main".into()),
        });
        let id = target.id.clone();
        storage
            .update(|instances, _| {
                instances.push(target);
                Ok(())
            })
            .unwrap();
        let external = dir.path().join("external");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--force",
                external.to_str().unwrap(),
                "agent-old",
            ],
        );
        std::fs::write(external.join("external.txt"), "keep external").unwrap();
        let before = serde_json::to_value(storage.load().unwrap()).unwrap();
        let shared = rename_session(
            "branch-only",
            args(&id, Some("Must not apply"), None, Some("blocked-shared")),
        )
        .await
        .unwrap_err();
        assert!(shared.to_string().contains("another Git worktree"));
        assert_eq!(
            serde_json::to_value(storage.load().unwrap()).unwrap(),
            before
        );
        for path in [&worktree, &external] {
            assert_eq!(git(path, &["branch", "--show-current"]), "agent-old");
            assert_eq!(git(path, &["rev-parse", "HEAD"]), head);
        }
        assert_eq!(
            std::fs::read_to_string(worktree.join("uncommitted.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            std::fs::read_to_string(external.join("external.txt")).unwrap(),
            "keep external"
        );
        assert!(git(&repo, &["branch", "--list", "blocked-shared"]).is_empty());
        git(
            &repo,
            &["worktree", "remove", "--force", external.to_str().unwrap()],
        );
        for branch in ["olof/bemlo-123-task", "olof/bemlo-123-task"] {
            rename_session("branch-only", args(&id, Some(branch), None, Some(branch)))
                .await
                .unwrap();
        }
        let target = storage.load().unwrap().pop().unwrap();
        assert_eq!(target.project_path, worktree.to_str().unwrap());
        assert_eq!(target.title, "olof/bemlo-123-task");
        assert_eq!(target.worktree_info.unwrap().branch, "olof/bemlo-123-task");
        assert_eq!(
            git(&worktree, &["branch", "--show-current"]),
            "olof/bemlo-123-task"
        );
        assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(worktree.join("uncommitted.txt")).unwrap(),
            "keep me"
        );
        for branch in ["bad name", "-option", "bad..name", "refs/heads/"] {
            assert!(rename_session(
                "branch-only",
                args(&id, Some("Must not apply"), None, Some(branch)),
            )
            .await
            .is_err());
        }
        git(&repo, &["remote", "add", "origin", "."]);
        git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/olof/bemlo-123-task",
            ],
        );
        for title in [Some("Updated title"), None, Some("olof/bemlo-123-task")] {
            rename_session(
                "branch-only",
                RenameArgs {
                    identifier: Some(id.clone()),
                    title: title.map(str::to_owned),
                    group: None,
                    rename_branch: false,
                    branch: Some("olof/bemlo-123-task".into()),
                },
            )
            .await
            .unwrap();
            let current = storage.load().unwrap().pop().unwrap();
            assert_eq!(current.title, title.unwrap_or("Updated title"));
            assert_eq!(current.project_path, worktree.to_str().unwrap());
            assert_eq!(current.worktree_info.unwrap().branch, "olof/bemlo-123-task");
            assert_eq!(
                git(&worktree, &["branch", "--show-current"]),
                "olof/bemlo-123-task"
            );
            assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                std::fs::read_to_string(worktree.join("uncommitted.txt")).unwrap(),
                "keep me"
            );
        }
        let protected = rename_session(
            "branch-only",
            args(&id, None, None, Some("blocked-default")),
        )
        .await
        .unwrap_err();
        assert!(protected.to_string().contains("default branch"));
        git(
            &repo,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        let peer_storage = Storage::new_unwatched("branch-peer").unwrap();
        for with_worktree_info in [true, false] {
            let mut peer = Instance::new("Peer", worktree.join(".").to_str().unwrap());
            if with_worktree_info {
                peer.worktree_info = storage.load().unwrap()[0].worktree_info.clone();
                peer.worktree_info.as_mut().unwrap().managed_by_aoe = false;
            }
            peer_storage
                .update(|rows, _| {
                    *rows = vec![peer];
                    Ok(())
                })
                .unwrap();
            let shared = rename_session(
                "branch-only",
                args(&id, Some("Must not apply"), None, Some("would-change-peer")),
            )
            .await
            .unwrap_err();
            assert!(shared.to_string().contains("profile branch-peer"));
            assert_eq!(
                git(&worktree, &["branch", "--show-current"]),
                "olof/bemlo-123-task"
            );
            assert_eq!(storage.load().unwrap()[0].title, "olof/bemlo-123-task");
        }
        peer_storage
            .update(|rows, _| {
                rows.clear();
                Ok(())
            })
            .unwrap();
        let error = rename_session(
            "branch-only",
            args(&id, Some("collision"), None, Some("main")),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(storage.load().unwrap()[0].title, "olof/bemlo-123-task");
        assert_eq!(
            git(&worktree, &["branch", "--show-current"]),
            "olof/bemlo-123-task"
        );

        let info = storage.load().unwrap()[0].worktree_info.clone().unwrap();
        git(&worktree, &["branch", "-m", "rollback-source"]);
        git(&worktree, &["checkout", "-b", "external-checkout"]);
        let refs = git(&repo, &["show-ref", "--heads"]);
        let error = crate::session::worktree_edit::rollback_worktree_branch(
            &info,
            &worktree,
            "rollback-source",
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed concurrently"));
        assert_eq!(git(&repo, &["show-ref", "--heads"]), refs);
        assert_eq!(
            git(&worktree, &["branch", "--show-current"]),
            "external-checkout"
        );
        assert_eq!(
            storage.load().unwrap()[0]
                .worktree_info
                .as_ref()
                .unwrap()
                .branch,
            info.branch
        );
        git(&worktree, &["checkout", "rollback-source"]);
        crate::session::worktree_edit::rollback_worktree_branch(
            &info,
            &worktree,
            "rollback-source",
        )
        .unwrap();
        assert_eq!(git(&worktree, &["branch", "--show-current"]), info.branch);
        assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(worktree.join("uncommitted.txt")).unwrap(),
            "keep me"
        );
    }

    #[tokio::test]
    #[serial]
    async fn rename_rejects_duplicate_pair_but_allows_group_only_change() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("rename-duplicate").unwrap();
        let existing = Instance::new("main branch", "/tmp/repo/");
        let target = Instance::new("throwaway", "/tmp/repo");
        let target_id = target.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![existing, target];
                Ok(())
            })
            .unwrap();

        let error = rename_session(
            "rename-duplicate",
            args(&target_id, Some("main branch"), None, None),
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Session already exists with same title and path"));

        rename_session(
            "rename-duplicate",
            args(&target_id, None, Some("work"), None),
        )
        .await
        .unwrap();

        let instances = storage.load().unwrap();
        let target = instances
            .iter()
            .find(|instance| instance.id == target_id)
            .unwrap();
        assert_eq!(target.title, "throwaway");
        assert_eq!(target.group_path, "work");
        let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let existing = Instance::new("main branch", "/tmp/worktrees/main-branch");
        let mut tied = Instance::new("main branch", "/tmp/worktrees/drifted");
        tied.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "drifted".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let tied_id = tied.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![existing, tied];
                Ok(())
            })
            .unwrap();

        let error = rename_session(
            "rename-duplicate",
            args(&tied_id, Some("main branch"), None, None),
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Session already exists with same title and path"));
        let tied = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|instance| instance.id == tied_id)
            .unwrap();
        assert_eq!(tied.title, "main branch");
        assert_eq!(tied.project_path, "/tmp/worktrees/drifted");

        let mut active = Instance::new("Main Branch", "/tmp/worktrees/main-branch");
        active.status = Status::Running;
        active.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "main-branch".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let active_id = active.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![active];
                Ok(())
            })
            .unwrap();

        rename_session(
            "rename-duplicate",
            args(&active_id, Some("Main Branch"), None, None),
        )
        .await
        .expect("active cwd-stable title no-op must succeed");
        let active = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|instance| instance.id == active_id)
            .unwrap();
        assert_eq!(active.title, "Main Branch");
        assert_eq!(active.project_path, "/tmp/worktrees/main-branch");
    }

    #[tokio::test]
    #[serial]
    async fn worktree_edit_consults_profile_status_rules() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("Skipping: tmux not available");
            return;
        }
        const PROFILE: &str = "worktree-edit-profile-rules";
        const AGENT: &str = "worktree-edit-rules-agent";
        let _guard = crate::session::test_support::isolate_app_dir();
        let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(false);
        crate::session::config::update_config(|config| {
            config.default_profile = "main".to_string();
        })
        .unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(PROFILE);
        let profile_config =
            crate::session::config::profile_config::get_profile_config_path(PROFILE).unwrap();
        std::fs::create_dir_all(profile_config.parent().unwrap()).unwrap();
        std::fs::write(
            &profile_config,
            format!(
                "[[agents.{AGENT}.status_rules]]\nstatus = \"running\"\ncontains = \"agent-busy\"\n"
            ),
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("agent-old");
        std::fs::create_dir(&worktree).unwrap();
        let mut target = Instance::new("Busy", worktree.to_str().unwrap());
        target.tool = AGENT.to_string();
        target.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "agent-old".into(),
            main_repo_path: dir.path().join("repo").to_str().unwrap().into(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let id = target.id.clone();
        let tmux_name = crate::tmux::Session::generate_name(&target.id, &target.title);
        let storage = Storage::new_unwatched(PROFILE).unwrap();
        storage
            .update(|instances, _| {
                instances.push(target);
                Ok(())
            })
            .unwrap();

        let _kill = crate::tmux::test_helpers::TmuxTestSession::from_name(tmux_name.clone());
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &tmux_name,
                "printf 'agent-busy\\n'; sleep 300",
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let captured = crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", &tmux_name])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&captured.stdout).contains("agent-busy") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "pane never painted");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let error = super::set_worktree_name(
            PROFILE,
            super::SetWorktreeNameArgs {
                identifier: Some(id),
                name: "agent-new".into(),
                rename_branch: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("while the session is active"),
            "{error}"
        );
        assert!(worktree.is_dir());
        assert_eq!(
            storage.load().unwrap()[0].project_path,
            worktree.to_str().unwrap()
        );
    }

    #[test]
    fn group_only_success_uses_authoritative_committed_title() {
        assert_eq!(
            rename_success_message("stale resolver title", "peer committed title", false),
            "✓ Updated session: peer committed title"
        );
    }
}

async fn set_worktree_name(profile: &str, args: SetWorktreeNameArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let inst = if let Some(id) = &args.identifier {
        super::resolve_session(id, &instances)?
    } else {
        let current_session = std::env::var("TMUX_PANE")
            .ok()
            .and_then(|_| crate::tmux::get_current_session_name());
        if let Some(session_name) = current_session {
            instances
                .iter()
                .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                .ok_or_else(|| {
                    anyhow::anyhow!("Current tmux session is not an Agent of Empires session")
                })?
        } else {
            bail!("Not in a tmux session. Specify a session ID or run inside tmux.");
        }
    };

    let id = inst.id.clone();
    let _identity_lock = acquire_session_identity_lock()?;
    let _lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&id)
        .context("failed to acquire worktree rename lifecycle lock")?;
    let authoritative_instances = storage.load()?;
    let inst = authoritative_instances
        .iter()
        .find(|instance| instance.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
    let mut inst = inst.clone();
    if let Err(error) = crate::session::worktree_reconcile::reconcile_and_persist(
        &storage,
        &mut inst,
        &mut Default::default(),
    ) {
        tracing::warn!(target: "cli.session", session = %id, "worktree path reconciliation skipped: {error}");
    }
    let current_path = inst.project_path.clone();
    let Some(worktree_info) = inst.worktree_info.clone() else {
        bail!("Session does not use a worktree");
    };
    if inst.tie_workdir_applies(
        crate::session::config::profile_config::resolve_config_or_warn(profile)
            .session
            .tie_workdir_to_name,
    ) {
        bail!("Renaming is unified while session.tie_workdir_to_name is on; use 'aoe session rename --title <name>' instead, and the worktree directory follows. Disable the setting to edit the directory independently.");
    }
    let duplicate_path = crate::session::worktree_edit::target_worktree_path(
        std::path::Path::new(&current_path),
        args.name.trim(),
    )
    .unwrap_or_else(|| std::path::PathBuf::from(&current_path))
    .to_string_lossy()
    .into_owned();
    if duplicate_path.trim_end_matches('/') != current_path.trim_end_matches('/')
        && is_duplicate_session(
            authoritative_instances.iter(),
            &inst.title,
            &duplicate_path,
            Some(&id),
        )
    {
        return Err(duplicate_session_error(&inst.title));
    }
    let mut live = inst.clone();
    live.source_profile = profile.to_string();
    crate::tmux::refresh_session_cache();
    live.update_status_with_metadata(None, None);
    let moves_worktree = crate::session::worktree_edit::worktree_move_required(
        std::path::Path::new(&current_path),
        args.name.trim(),
    );
    if live.status.blocks_worktree_edit()
        || (moves_worktree
            && crate::session::worktree_edit::ensure_sandbox_container_released(
                &id,
                live.is_sandboxed(),
            ))
    {
        bail!("Cannot edit the workdir name while the session is active; stop it first");
    }

    let outcome = crate::session::worktree_edit::edit_worktree_workdir(
        crate::session::worktree_edit::WorktreeEditRequest {
            worktree_info: &worktree_info,
            current_path: std::path::Path::new(&current_path),
            new_name: args.name.trim(),
            rename_branch: args.rename_branch,
        },
    )?;
    if outcome.new_path != std::path::Path::new(&current_path) {
        crate::session::worktree_edit::discard_sandbox_container_after_move(
            &id,
            live.is_sandboxed(),
        );
    }
    let new_path = outcome.new_path.to_string_lossy().to_string();
    let new_branch = outcome.new_branch.clone();

    storage
        .update(|instances, _groups| {
            let inst = instances
                .iter_mut()
                .find(|i| i.id == id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;
            inst.project_path = new_path.clone();
            if let Some(branch) = &new_branch {
                if let Some(wt) = inst.worktree_info.as_mut() {
                    wt.branch = branch.clone();
                }
            }
            Ok(())
        })
        .map_err(|e| {
            anyhow::anyhow!(
                "Worktree was moved on disk to {new_path}, but persisting the new session metadata failed: {e}. Re-run to retry."
            )
        })?;
    drop(_identity_lock);

    println!("✓ Worktree moved to: {}", new_path);
    if let Some(branch) = &new_branch {
        println!("  Branch renamed to: {}", branch);
    }
    Ok(())
}

async fn current_session(args: CurrentArgs) -> Result<()> {
    let current_session = std::env::var("TMUX_PANE")
        .ok()
        .and_then(|_| crate::tmux::get_current_session_name());

    let session_name = current_session.ok_or_else(|| anyhow::anyhow!("Not in a tmux session"))?;

    let profiles = crate::session::list_profiles()?;

    for profile_name in &profiles {
        if let Ok(storage) = Storage::open_unwatched(profile_name) {
            if let Ok((instances, _)) = storage.load_with_groups() {
                if let Some(inst) = instances
                    .iter()
                    .find(|i| crate::tmux::agent_session_belongs_to(&session_name, &i.id))
                {
                    if args.json {
                        #[derive(Serialize)]
                        struct CurrentInfo {
                            session: String,
                            profile: String,
                            id: String,
                        }
                        let info = CurrentInfo {
                            session: inst.title.clone(),
                            profile: profile_name.clone(),
                            id: inst.id.clone(),
                        };
                        super::output::print_json(&info)?;
                    } else if args.quiet {
                        println!("{}", inst.title);
                    } else {
                        println!("Session: {}", inst.title);
                        println!("Profile: {}", profile_name);
                        println!("ID:      {}", inst.id);
                    }
                    return Ok(());
                }
            }
        }
    }

    bail!("Current tmux session is not an Agent of Empires session")
}

async fn set_session_id(profile: &str, args: SetSessionIdArgs) -> Result<()> {
    let new_intent = if args.session_id.trim().is_empty() {
        crate::session::ResumeIntent::Cleared
    } else {
        let trimmed = args.session_id.trim().to_string();
        if !crate::session::is_valid_session_id(&trimmed) {
            bail!(
                "Invalid session ID {:?}: must be 1-256 ASCII alphanumeric, dash, underscore, or dot characters",
                trimmed
            );
        }
        crate::session::ResumeIntent::Use(trimmed)
    };

    let storage = Storage::open_unwatched(profile)?;
    let target_id = {
        let instances = storage.load()?;
        super::resolve_session(&args.identifier, &instances)?
            .id
            .clone()
    };
    let lifecycle_lock = storage
        .acquire_instance_lifecycle_lock(&target_id)
        .context("failed to acquire instance resume-target lock")?;
    let title = storage.update(|instances, _groups| {
        super::patch_instance(instances, &target_id, |inst| {
            inst.source_profile = storage.profile().to_string();
            if inst.is_structured() {
                anyhow::ensure!(args.store.is_some() && matches!((&new_intent, inst.acp_session_id.as_deref()), (crate::session::ResumeIntent::Use(sid), Some(acp_sid)) if sid == acp_sid),
                    "ACP manages its own conversation; a native handoff assertion requires its current ID and an explicit --store");
            }
            let binding = match &new_intent {
                crate::session::ResumeIntent::Use(sid) => Some(inst.asserted_resume_binding(sid, args.store.as_deref())?),
                _ => None,
            };
            anyhow::ensure!(!inst.is_structured() || binding.as_ref().and_then(|binding| binding.execution.as_ref()).is_some_and(|execution| execution.agent == "claude"),
                "ACP terminal handoff is supported only for an explicitly bound Claude conversation");
            inst.resume_binding = binding;
            inst.resume_intent = new_intent.clone();
            inst.resume_probe_failed_sid = None;
            Ok(inst.title.clone())
        })
    })?;
    drop(lifecycle_lock);

    match &new_intent {
        crate::session::ResumeIntent::Use(id) => {
            println!("✓ Set resume target for '{}': {}", title, id);
        }
        crate::session::ResumeIntent::Cleared => {
            println!(
                "✓ Cleared resume intent for '{}' (next launches will be fresh)",
                title
            );
        }
        crate::session::ResumeIntent::Default | crate::session::ResumeIntent::Fork { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

async fn add_project(profile: &str, args: AddProjectArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let instances = storage.load()?;
    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();
    let is_sandboxed = inst.is_sandboxed();

    if inst.status.blocks_worktree_edit() {
        bail!(
            "'{title}' has a turn in flight and attaching restarts the agent. Wait for it to \
             finish, or stop the session first."
        );
    }

    let repo_path = if std::path::Path::new(&args.project).exists()
        || args.project.contains(std::path::MAIN_SEPARATOR)
    {
        std::path::PathBuf::from(&args.project)
    } else {
        let resolved =
            crate::session::projects::resolve_names(profile, std::slice::from_ref(&args.project))?;
        match resolved.into_iter().next() {
            Some(p) => std::path::PathBuf::from(p.path),
            None => bail!("Project '{}' is not in the registry", args.project),
        }
    };

    let on_existing = if args.attach_existing_branch {
        crate::session::attach_project::ExistingBranch::Attach
    } else {
        crate::session::attach_project::ExistingBranch::Refuse
    };

    let plan = crate::session::attach_project::plan(inst, profile, &repo_path, on_existing)?;
    let restarts = crate::session::attach_project::needs_restart(&plan, is_sandboxed);
    let quiesced = if restarts {
        println!("Stopping '{title}' so its working directory can move...");
        crate::session::attach_project::quiesce_for_conversion(&storage, inst)?
    } else {
        crate::session::attach_project::Quiesced::default()
    };

    let outcome = match crate::session::attach_project::attach_planned(&storage, &id, inst, plan) {
        Ok(outcome) => outcome,
        Err(e) => {
            crate::session::attach_project::resume_after_conversion(&storage, &id, quiesced);
            return Err(e);
        }
    };

    println!("Attached '{}' to session '{}'", outcome.repo.name, title);
    println!("  Worktree: {}", outcome.repo.worktree_path);
    println!(
        "  Branch:   {} ({})",
        outcome.repo.branch,
        if outcome.repo.branch_preexisting {
            "existing, aoe will not delete it"
        } else {
            "created"
        }
    );
    if let Some(moved_to) = &outcome.moved_to {
        println!("  Workspace: {moved_to}");
        println!(
            "  This session is now a multi-repo workspace; its working directory moved to the \
             path above."
        );
    }
    for warning in &outcome.warnings {
        println!("  Warning:  {warning}");
    }

    if restarts {
        println!("Restarting the session so it comes up with the new repo.");
    } else {
        println!("The agent is already working in this directory, so nothing was restarted.");
    }
    for warning in crate::session::attach_project::resume_after_conversion(&storage, &id, quiesced)
    {
        println!("  Warning:  {warning}");
    }

    Ok(())
}

async fn set_base(profile: &str, args: SetBaseArgs) -> Result<()> {
    if !args.clear && args.branch.is_none() {
        bail!("Provide a branch ref or pass --clear to remove the override.");
    }
    let storage = Storage::open_unwatched(profile)?;
    let instances = storage.load()?;

    let inst = super::resolve_session(&args.identifier, &instances)?;
    let id = inst.id.clone();
    let title = inst.title.clone();

    let target = resolve_base_target(inst, args.repo.as_deref())?;

    let new_value = if args.clear {
        None
    } else {
        let trimmed = args.branch.as_deref().unwrap_or("").trim().to_string();
        if trimmed.is_empty() {
            bail!("Branch name is empty. Pass --clear to remove the override.");
        }
        if let Err(e) =
            crate::git::diff::validate_ref(std::path::Path::new(&target.validate_path), &trimmed)
        {
            bail!(
                "Branch '{}' does not resolve in {}: {}",
                trimmed,
                target.validate_path,
                e
            );
        }
        Some(trimmed)
    };

    let repo_name = target.repo_name.clone();
    storage.update(|instances, _groups| {
        let stored = instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", args.identifier))?;
        match repo_name.as_deref() {
            Some(name) => {
                let repo = stored
                    .workspace_info
                    .as_mut()
                    .and_then(|ws| ws.repos.iter_mut().find(|r| r.name == name))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Repo '{}' is no longer part of this session; nothing was changed",
                            name
                        )
                    })?;
                repo.base_branch_override = new_value.clone();
            }
            None => stored.base_branch_override = new_value.clone(),
        }
        Ok(())
    })?;

    let label = match target.repo_name {
        Some(ref name) => format!("'{title}' / '{name}'"),
        None => format!("'{title}'"),
    };
    match new_value {
        Some(ref v) => println!("✓ Set diff base for {}: {}", label, v),
        None => println!("✓ Cleared diff base override for {}", label),
    }
    Ok(())
}

#[derive(Debug)]
struct BaseTarget {
    repo_name: Option<String>,
    validate_path: String,
}

fn resolve_base_target(inst: &crate::session::Instance, repo: Option<&str>) -> Result<BaseTarget> {
    let names = || {
        inst.all_repos()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match repo {
        Some(name) => match inst.all_repos().iter().find(|r| r.name == name) {
            Some(r) => Ok(BaseTarget {
                repo_name: Some(r.name.clone()),
                validate_path: r.worktree_path.clone(),
            }),
            None if inst.all_repos().is_empty() => bail!(
                "This session has no workspace repos, so --repo does not apply. Drop it to set \
                 the session's own diff base."
            ),
            None => bail!("Unknown repo '{}'. This session has: {}", name, names()),
        },
        None if inst.workspace_info.is_some() => bail!(
            "This session is a multi-repo workspace, and each repo has its own diff base.\nPass \
             --repo <name> to pick one. Available: {}",
            names()
        ),
        None => Ok(BaseTarget {
            repo_name: None,
            validate_path: inst.project_path.clone(),
        }),
    }
}

#[cfg(test)]
mod restart_args_tests {
    use super::{supervise_attach_capture, SessionCommands};
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: SessionCommands,
    }

    #[test]
    #[serial_test::serial]
    fn attach_supervision_starts_before_attach_and_flushes_every_return() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "attach-capture-supervision";
        let mut inst = crate::session::Instance::new("attach", "/tmp/attach");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("d38740e4-bd1f-43d7-8727-485652e4678e".to_string());
        inst.mark_pi_extension_launched_for_test();
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _| {
                *instances = vec![inst.clone()];
                Ok(())
            })
            .unwrap();

        let first = "01a053b6-c470-78de-9d8f-bc00ef05332a";
        supervise_attach_capture(&mut inst, |live| {
            assert!(
                live.session_id_poller_is_running(),
                "capture must be supervised before the blocking attach call"
            );
            crate::session::publish_host_pi_transcript(&live.id, first, home.path());
            Ok(())
        })
        .unwrap();
        assert!(inst.session_id_poller.is_none());
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(first)
        );

        let second = "01a053b6-c470-78de-9d8f-bc00ef05332b";
        let result = supervise_attach_capture(&mut inst, |live| {
            crate::session::publish_host_pi_transcript(&live.id, second, home.path());
            Err(anyhow::anyhow!("fake attach failure"))
        });

        assert_eq!(result.unwrap_err().to_string(), "fake attach failure");
        assert!(
            inst.session_id_poller.is_none(),
            "an immediate nested attach return must not orphan its poller"
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(second),
            "the final /new identity must be durable even when attach returns an error"
        );
    }

    #[test]
    fn restart_parses_identifier_all_and_parallel() {
        let restart = |argv: &[&str]| {
            Cli::try_parse_from(argv).map(|cli| match cli.cmd {
                SessionCommands::Restart(args) => (args.all, args.identifier, args.parallel),
                _ => panic!("wrong subcommand"),
            })
        };
        assert_eq!(
            restart(&["aoe", "restart", "claude-3"]).unwrap(),
            (false, Some("claude-3".to_string()), 3)
        );
        assert_eq!(
            restart(&["aoe", "restart", "--all"]).unwrap(),
            (true, None, 3)
        );
        assert_eq!(
            restart(&["aoe", "restart", "--all", "--parallel", "5"]).unwrap(),
            (true, None, 5)
        );
        assert!(restart(&["aoe", "restart", "claude-3", "--all"]).is_err());
    }

    #[test]
    fn unarchive_parses_allow_duplicate_opt_in() {
        let cli = Cli::try_parse_from(["aoe", "unarchive", "abc"]).expect("unarchive must parse");
        match cli.cmd {
            SessionCommands::Unarchive(args) => {
                assert_eq!(args.identifier, "abc");
                assert!(!args.allow_duplicate);
            }
            _ => panic!("expected unarchive"),
        }
        let cli = Cli::try_parse_from(["aoe", "unarchive", "abc", "--allow-duplicate"])
            .expect("unarchive --allow-duplicate must parse");
        match cli.cmd {
            SessionCommands::Unarchive(args) => assert!(args.allow_duplicate),
            _ => panic!("expected unarchive"),
        }
    }

    #[test]
    fn add_project_parses_its_identifier_project_and_branch_opt_in() {
        let cases = [
            (vec!["aoe", "add-project", "claude-3", "../frontend"], false),
            (
                vec![
                    "aoe",
                    "add-project",
                    "claude-3",
                    "../frontend",
                    "--attach-existing-branch",
                ],
                true,
            ),
        ];
        for (argv, attach_existing) in cases {
            let cli = Cli::try_parse_from(&argv).expect("add-project must parse");
            match cli.cmd {
                SessionCommands::AddProject(args) => {
                    assert_eq!(args.identifier, "claude-3");
                    assert_eq!(args.project, "../frontend");
                    assert_eq!(args.attach_existing_branch, attach_existing, "{argv:?}");
                }
                _ => panic!("wrong subcommand"),
            }
        }
    }

    #[test]
    fn set_base_parses_branch_or_clear_but_not_both() {
        let set_base = |argv: &[&str]| {
            Cli::try_parse_from(argv).map(|cli| match cli.cmd {
                SessionCommands::SetBase(args) => (args.identifier, args.branch, args.clear),
                _ => panic!("wrong subcommand"),
            })
        };
        assert_eq!(
            set_base(&["aoe", "set-base", "claude-3", "upstream/main"]).unwrap(),
            (
                "claude-3".to_string(),
                Some("upstream/main".to_string()),
                false
            )
        );
        assert_eq!(
            set_base(&["aoe", "set-base", "claude-3", "--clear"]).unwrap(),
            ("claude-3".to_string(), None, true)
        );
        assert!(set_base(&["aoe", "set-base", "claude-3", "main", "--clear"]).is_err());
    }

    #[test]
    fn set_model_parses_identifier_and_model() {
        let cli = Cli::try_parse_from(["aoe", "set-model", "claude-3", "claude-fable-5"])
            .expect("set-model must parse");
        match cli.cmd {
            SessionCommands::SetModel(args) => {
                assert_eq!(args.identifier, "claude-3");
                assert_eq!(args.model, "claude-fable-5");
                assert!(!args.no_restart);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn set_model_no_restart_flag_parses() {
        let cli = Cli::try_parse_from(["aoe", "set-model", "claude-3", "opus", "--no-restart"])
            .expect("set-model --no-restart must parse");
        match cli.cmd {
            SessionCommands::SetModel(args) => {
                assert!(args.no_restart);
                assert_eq!(args.model, "opus");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn set_model_empty_model_clears() {
        // Empty-string model is the documented CLEAR path; it must parse as a
        // positional (not be swallowed / error).
        let cli = Cli::try_parse_from(["aoe", "set-model", "claude-3", ""])
            .expect("set-model with empty model must parse");
        match cli.cmd {
            SessionCommands::SetModel(args) => {
                assert_eq!(args.identifier, "claude-3");
                assert!(args.model.is_empty());
            }
            _ => panic!("wrong subcommand"),
        }
    }
}

#[cfg(test)]
mod queue_command_tests {
    use super::{format_queue_row, humanize_age, queue_hold_label, QueueAction, SessionCommands};
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: SessionCommands,
    }

    #[test]
    fn queue_list_parses_identifier() {
        let cli = Cli::try_parse_from(["aoe", "queue", "probe-target", "--json"])
            .expect("queue <id> must parse");
        match cli.cmd {
            SessionCommands::Queue(args) => {
                assert!(args.action.is_none());
                assert_eq!(args.identifier.as_deref(), Some("probe-target"));
                assert!(args.json);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn queue_drop_parses_identifier_and_qid() {
        let cli =
            Cli::try_parse_from(["aoe", "queue", "drop", "probe-target", "send-0123456789ab"])
                .expect("queue drop must parse");
        match cli.cmd {
            SessionCommands::Queue(args) => match args.action {
                Some(QueueAction::Drop { identifier, qid }) => {
                    assert_eq!(identifier, "probe-target");
                    assert_eq!(qid, "send-0123456789ab");
                }
                _ => panic!("expected drop"),
            },
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn queue_release_parses_identifier_and_qid() {
        let cli = Cli::try_parse_from([
            "aoe",
            "queue",
            "release",
            "for-Productivity",
            "send-8bcbf22386ea",
        ])
        .expect("queue release must parse");
        match cli.cmd {
            SessionCommands::Queue(args) => match args.action {
                Some(QueueAction::Release { identifier, qid }) => {
                    assert_eq!(identifier, "for-Productivity");
                    assert_eq!(qid, "send-8bcbf22386ea");
                }
                _ => panic!("expected release"),
            },
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn hold_column_reads_the_receipt() {
        assert_eq!(queue_hold_label(None), "-");
        assert_eq!(queue_hold_label(Some("claimed")), "review");
        assert_eq!(queue_hold_label(Some("legacy_uncertain")), "review");
        assert_eq!(queue_hold_label(Some("released")), "released");
        assert_eq!(queue_hold_label(Some("released:2")), "released");
        let spent = format!(
            "released:{}",
            crate::acp::event_store::terminal_queue::MAX_AUTOMATIC_ATTEMPTS
        );
        assert_eq!(queue_hold_label(Some(&spent)), "exhausted");
        assert_eq!(queue_hold_label(Some("delivered")), "retiring");
        assert_eq!(queue_hold_label(Some("dropped")), "retiring");
        let entry = crate::daemon::QueuedPromptEntry {
            id: "send-8bcbf22386ea".to_string(),
            seq: 1,
            text: "STATUS: shipped".to_string(),
            attachments: Vec::new(),
            created_at: "2026-09-20T07:58:00Z".to_string(),
            origin_device: Some("aoe send".to_string()),
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-20T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let row = format_queue_row(&entry, now, Some("review"));
        assert!(row.contains("  review    aoe send"), "{row}");
    }

    #[test]
    fn queue_row_shows_sender_age_and_first_80_chars() {
        let long = "STATUS: shipped ".repeat(10);
        let entry = crate::daemon::QueuedPromptEntry {
            id: "send-0123456789ab".to_string(),
            seq: 3,
            text: format!("{long}\n\nEVIDENCE: build 9f221e45"),
            attachments: Vec::new(),
            created_at: "2026-09-04T08:00:00Z".to_string(),
            origin_device: Some("aoe send from for-dev".to_string()),
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-04T08:05:30Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let row = format_queue_row(&entry, now, None);
        assert!(row.starts_with("send-0123456789ab "), "{row}");
        assert!(row.contains("    5m  "), "{row}");
        assert!(row.contains("aoe send from for-dev"), "{row}");
        let text = row.rsplit("  ").next().unwrap();
        // 80 chars + the ellipsis, whitespace collapsed (no newline survives).
        assert_eq!(text.chars().count(), 81, "{text}");
        assert!(text.ends_with('…') && !text.contains('\n'));
    }

    #[test]
    fn age_humanizes_by_magnitude() {
        assert_eq!(humanize_age(42), "42s");
        assert_eq!(humanize_age(330), "5m");
        assert_eq!(humanize_age(7_200), "2h");
        assert_eq!(humanize_age(200_000), "2d");
    }
}

#[cfg(test)]
mod apply_model_arg_tests {
    use super::apply_model_arg;

    #[test]
    fn appends_when_no_existing_model() {
        assert_eq!(apply_model_arg("", "opus"), "--model opus");
        assert_eq!(
            apply_model_arg("--verbose", "claude-fable-5"),
            "--verbose --model claude-fable-5"
        );
    }

    #[test]
    fn replaces_existing_space_form_in_place_preserving_other_args() {
        // Existing `--model X` is stripped, new one appended at the end; other
        // flags are preserved and never duplicated.
        assert_eq!(
            apply_model_arg("--model sonnet --verbose", "opus"),
            "--verbose --model opus"
        );
    }

    #[test]
    fn replaces_existing_equals_form() {
        assert_eq!(
            apply_model_arg("--model=sonnet --verbose", "opus"),
            "--verbose --model opus"
        );
    }

    #[test]
    fn empty_model_clears_the_pin() {
        assert_eq!(apply_model_arg("--model opus --verbose", ""), "--verbose");
        assert_eq!(apply_model_arg("--model=opus", ""), "");
        assert_eq!(apply_model_arg("", ""), "");
    }

    #[test]
    fn idempotent_on_repeat() {
        let once = apply_model_arg("--verbose", "opus");
        assert_eq!(apply_model_arg(&once, "opus"), once);
    }
}

#[cfg(test)]
mod set_base_target_tests {
    use super::resolve_base_target;
    use crate::session::{Instance, WorkspaceInfo, WorkspaceRepo};

    fn workspace_instance() -> Instance {
        let mut inst = Instance::new("ws", "/ws");
        inst.workspace_info = Some(WorkspaceInfo {
            branch: "feature/x".to_string(),
            workspace_dir: "/ws".to_string(),
            repos: ["api", "web"]
                .iter()
                .map(|n| WorkspaceRepo {
                    name: n.to_string(),
                    source_path: format!("/src/{n}"),
                    branch: "feature/x".to_string(),
                    worktree_path: format!("/ws/{n}"),
                    main_repo_path: format!("/src/{n}"),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                })
                .collect(),
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });
        inst
    }

    #[test]
    fn workspace_requires_a_known_repo_and_targets_its_own_worktree() {
        let inst = workspace_instance();
        let target = resolve_base_target(&inst, Some("web")).expect("named repo resolves");
        assert_eq!(target.repo_name.as_deref(), Some("web"));
        assert_eq!(target.validate_path, "/ws/web");

        let err = resolve_base_target(&inst, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("api, web"),
            "should list the repos, got: {err}"
        );

        let err = resolve_base_target(&inst, None).unwrap_err().to_string();
        assert!(
            err.contains("--repo") && err.contains("api, web"),
            "should demand a repo and list them, got: {err}"
        );
    }

    #[test]
    fn single_repo_session_targets_its_own_checkout() {
        let inst = Instance::new("solo", "/tmp/solo");
        let target = resolve_base_target(&inst, None).expect("single repo resolves");
        assert_eq!(target.repo_name, None);
        assert_eq!(target.validate_path, "/tmp/solo");

        let err = resolve_base_target(&inst, Some("api"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no workspace repos"),
            "should explain --repo does not apply, got: {err}"
        );
    }
}

#[cfg(test)]
mod target_filter_tests {
    use super::pick_targets_for_restart_all;
    use crate::session::{Instance, Status};

    #[test]
    fn restart_all_skips_deleting_and_creating() {
        let instance = |id: &str, status: Status| {
            let mut inst = Instance::new(id, "/tmp");
            inst.id = id.to_string();
            inst.status = status;
            inst
        };
        let instances = vec![
            instance("running", Status::Running),
            instance("idle", Status::Idle),
            instance("stopped", Status::Stopped),
            instance("error", Status::Error),
            instance("waiting", Status::Waiting),
            instance("starting", Status::Starting),
            instance("unknown", Status::Unknown),
            instance("deleting", Status::Deleting),
            instance("creating", Status::Creating),
        ];
        let mut picked = pick_targets_for_restart_all(&instances);
        picked.sort();
        assert_eq!(
            picked,
            ["error", "idle", "running", "starting", "stopped", "unknown", "waiting"]
        );
        assert!(pick_targets_for_restart_all(&[]).is_empty());
    }
}

#[cfg(test)]
mod move_lookup_tests {
    use super::find_session_across_profiles;
    use crate::session::{Instance, Storage};
    use serial_test::serial;
    use tempfile::tempdir;

    fn seed(profile: &str, id: &str, title: &str, path: &str) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new(id, path);
        inst.id = id.to_string();
        inst.title = title.to_string();
        inst.source_profile = profile.to_string();
        let on_disk = inst.clone();
        storage
            .update(|i, _g| {
                i.push(on_disk.clone());
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn finds_owner_exact_id_wins_and_title_collision_is_ambiguous() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        // Two profiles, identical title in each, distinct ids.
        seed("mvfind-a", "aaaa1111", "shared-title", "/tmp/a");
        seed("mvfind-b", "bbbb2222", "shared-title", "/tmp/b");

        // Exact id resolves to its owning profile even though the title
        // collides across profiles (exact-id short-circuit).
        let (owner, inst) = find_session_across_profiles("aaaa1111").unwrap();
        assert_eq!(owner, "mvfind-a");
        assert_eq!(inst.id, "aaaa1111");

        // Title alone matches in both profiles -> ambiguous -> refuse to guess.
        let err = find_session_across_profiles("shared-title").unwrap_err();
        assert!(
            err.to_string().contains("Ambiguous"),
            "expected ambiguity error, got: {err}"
        );

        // No match anywhere -> error.
        assert!(find_session_across_profiles("nope-nope-nope").is_err());
    }
}

#[cfg(test)]
mod move_divergence_tests {
    use super::{config_dir_diverged, extract_config_dir, move_incomplete_message};

    const FORIT_MAIN: &str = "/Users/me/.claude-accounts/forit-main";
    const FORIT_BACKUP: &str = "/Users/me/.claude-accounts/forit-backup";

    #[test]
    fn destination_trust_seeds_the_pinned_config_dir_on_the_canonical_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let account = temp.path().join("accounts").join("bsc-main");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(
            account.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"keep-me"},"projects":{"/other":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        let env = vec![
            "FOO=bar".to_string(),
            format!("CLAUDE_CONFIG_DIR={}", account.display()),
        ];
        let home = temp.path().join("home");

        let (json_path, key) =
            super::seed_destination_folder_trust(&env, &home, repo.to_str().unwrap()).unwrap();

        assert_eq!(json_path, account.join(".claude.json"));
        assert_eq!(key, std::fs::canonicalize(&repo).unwrap().to_string_lossy());
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(json["projects"][&key]["hasTrustDialogAccepted"], true);
        assert_eq!(json["projects"]["/other"]["hasTrustDialogAccepted"], true);
        assert_eq!(json["oauthAccount"]["accountUuid"], "keep-me");
        assert!(
            !home.join(".claude.json").exists(),
            "home must not be touched when a config dir is pinned"
        );
    }

    #[test]
    fn destination_trust_falls_back_to_home_claude_json_when_unpinned() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let home = temp.path().join("home");
        let env = vec!["FOO=bar".to_string()];

        let (json_path, key) =
            super::seed_destination_folder_trust(&env, &home, repo.to_str().unwrap()).unwrap();

        assert_eq!(json_path, home.join(".claude.json"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(json["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn destination_trust_refuses_loudly_when_the_config_dir_is_unwritable() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let blocker = temp.path().join("not-a-dir");
        std::fs::write(&blocker, "file").unwrap();
        let env = vec![format!("CLAUDE_CONFIG_DIR={}", blocker.display())];

        let err = super::seed_destination_folder_trust(&env, temp.path(), repo.to_str().unwrap())
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("creating Claude config dir"), "{msg}");
    }

    #[test]
    fn extract_config_dir_plucks_the_value() {
        let env = vec![
            "PATH=/usr/bin".to_string(),
            format!("CLAUDE_CONFIG_DIR={FORIT_MAIN}"),
            "TERM=xterm".to_string(),
        ];
        assert_eq!(extract_config_dir(&env).as_deref(), Some(FORIT_MAIN));
        assert_eq!(extract_config_dir(&["PATH=/usr/bin".to_string()]), None);
    }

    // THE BUG: registry label == target (`forit-main`), but the live pane is
    // bound to a diverged, weekly-capped account (`forit-backup`). The old
    // `owner == target` no-op left this un-relocatable. The divergence check
    // must report it diverged so a single `move <id> forit-main` triggers the
    // restart+rebind instead of "nothing to move". (con-assistant / for-maint.)
    #[test]
    fn diverged_live_account_is_detected() {
        // These paths don't exist on disk, so canonicalize fails-soft to the
        // raw strings and the comparison is on the literal values.
        assert!(
            config_dir_diverged(Some(FORIT_BACKUP), Some(FORIT_MAIN)),
            "live forit-backup vs profile forit-main must read as diverged"
        );
    }

    #[test]
    fn matching_live_account_is_not_diverged() {
        assert!(
            !config_dir_diverged(Some(FORIT_MAIN), Some(FORIT_MAIN)),
            "live == expected must be a genuine no-op"
        );
    }

    #[test]
    fn unreadable_live_binding_fails_safe_to_no_divergence() {
        // Stopped session / un-inspectable pane => no spurious restart.
        assert!(!config_dir_diverged(None, Some(FORIT_MAIN)));
        assert!(!config_dir_diverged(Some(FORIT_BACKUP), None));
        assert!(!config_dir_diverged(None, None));
    }

    // WO#1669: a move whose re-bind did not take must say so in the terms
    // that matter (which account is still being spent) and name the exact
    // recovery command, never a bare "session no longer exists".
    #[test]
    fn move_incomplete_message_names_the_source_account_and_the_recovery_command() {
        let msg = move_incomplete_message(
            "for-Probe",
            "0656af429bce4028",
            "p9-main",
            "forit-backup",
            Some(FORIT_MAIN),
            "restart failed: session 0656af429bce4028 no longer exists",
        );
        assert!(msg.starts_with("MOVE INCOMPLETE"), "{msg}");
        assert!(msg.contains("STILL running under 'p9-main'"), "{msg}");
        assert!(msg.contains(FORIT_MAIN), "{msg}");
        assert!(
            msg.contains("aoe -p forit-backup session restart 0656af429bce4028"),
            "{msg}"
        );
        let no_live = move_incomplete_message("t", "id1", "a", "b", None, "why");
        assert!(!no_live.contains("live CLAUDE_CONFIG_DIR="), "{no_live}");
    }
}

#[cfg(test)]
mod set_session_id_tests {
    use super::{set_session_id, SetSessionIdArgs};
mod session_mutation_tests {
    use super::{set_color_session, set_session_id, SetColorArgs, SetSessionIdArgs};
    use crate::session::{Instance, ResumeIntent, Storage};
    use serial_test::serial;
    use tempfile::tempdir;

    const SID_A: &str = "11111111-1111-1111-1111-111111111111";
    const SID_B: &str = "22222222-2222-2222-2222-222222222222";

    fn seed(profile: &str, inst: Instance) -> (Storage, String) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let id = inst.id.clone();
        storage
            .update(|rows, groups| {
                *rows = vec![inst.clone()];
                *groups =
                    crate::session::GroupTree::new_with_groups(std::slice::from_ref(&inst), &[])
                        .get_all_groups();
                Ok(())
            })
            .unwrap();
        (storage, id)
    }

    fn stored(storage: &Storage, id: &str) -> Instance {
        storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn set_session_id_replaces_intent_and_clears_the_resume_probe_marker() {
        let temp = tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _claude = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );

        let mut inst = Instance::new("marked_session", "/tmp/x");
        inst.agent_session_id = Some(SID_A.to_string());
        inst.resume_probe_failed_sid = Some(SID_A.to_string());
        let (storage, id) = seed("set-sid-clear-marker", inst);

        set_session_id(
            "set-sid-clear-marker",
            SetSessionIdArgs {
                identifier: id.clone(),
                session_id: SID_B.to_string(),
                store: None,
            },
        )
        .await
        .unwrap();

        let row = stored(&storage, &id);
        assert_eq!(row.resume_intent, ResumeIntent::Use(SID_B.to_string()));
        assert_eq!(row.resume_probe_failed_sid, None);
    }

    #[tokio::test]
    #[serial]
    async fn set_session_id_rejects_structured_view_session() {
        let temp = tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());

        let mut inst = Instance::new("acp_session", "/tmp/x");
        inst.view = crate::session::View::Structured;
        let (storage, id) = seed("acp-reject", inst);

        let _err = set_session_id(
            "acp-reject",
            SetSessionIdArgs {
                identifier: id.clone(),
                session_id: SID_A.to_string(),
                store: None,
            },
        )
        .await
        .expect_err("set-session-id must reject structured view-mode sessions");

        let row = stored(&storage, &id);
        assert_eq!(
            row.resume_intent,
            ResumeIntent::Default,
            "rejected call must not mutate intent"
        );
        assert_eq!(
            row.agent_session_id, None,
            "rejected call must not mutate sid"
        );
    }

    #[tokio::test]
    #[serial]
    async fn set_color_normalizes_clears_and_rejects_unknown_values() {
        let temp = tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());

        let (storage, id) = seed("set-color", Instance::new("color_session", "/tmp/x"));
        let set = |color: &str| {
            set_color_session(
                "set-color",
                SetColorArgs {
                    identifier: id.clone(),
                    color: color.to_string(),
                },
            )
        };

        set("Red")
            .await
            .expect("palette names are case-insensitive");
        assert_eq!(stored(&storage, &id).color.as_deref(), Some("red"));

        set("chartreuse")
            .await
            .expect_err("unknown color must error");
        assert_eq!(stored(&storage, &id).color.as_deref(), Some("red"));

        set("none").await.unwrap();
        assert_eq!(stored(&storage, &id).color, None);
    }

    #[tokio::test]
    #[serial]
    async fn set_color_refuses_unknown_profile_without_vivifying_it() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let profiles = crate::session::get_app_dir().unwrap().join("profiles");
        std::fs::create_dir_all(profiles.join("real")).unwrap();

        let msg = set_color_session(
            "ghost-profile",
            SetColorArgs {
                identifier: "whatever".to_string(),
                color: "red".to_string(),
            },
        )
        .await
        .expect_err("unknown profile must error")
        .to_string();
        assert!(
            msg.contains("does not exist"),
            "expected the unknown-profile error, got: {msg}"
        );
        assert!(
            !profiles.join("ghost-profile").exists(),
            "set-color must not mint profiles/ghost-profile"
        );
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::session::claude_import::ClaudeSessionSummary;

    fn summary(id: &str, cwd: &str, title: Option<&str>) -> ClaudeSessionSummary {
        ClaudeSessionSummary {
            session_id: id.to_string(),
            config_dir: std::path::PathBuf::from("/claude-import-store"),
            cwd: cwd.to_string(),
            title: title.map(str::to_string),
            last_modified_ms: 0,
            cwd_exists: true,
        }
    }

    #[test]
    fn build_import_instance_pins_the_replay_target_for_each_view() {
        let terminal = build_import_instance(
            &summary("abc123-def456", "/home/me/proj", Some("Fix bug")),
            false,
            "",
        );
        assert_eq!(terminal.tool, "claude");
        assert_eq!(terminal.project_path, "/home/me/proj");
        assert_eq!(terminal.title, "Fix bug");
        assert_eq!(
            terminal.resume_intent,
            ResumeIntent::Use("abc123-def456".to_string())
        );

        let untitled = build_import_instance(
            &summary("abcdef12-3456-7890", "/home/me/proj", None),
            false,
            "team/imports",
        );
        assert_eq!(untitled.title, "Claude import abcdef12");
        assert_eq!(untitled.group_path, "team/imports");

        let structured =
            build_import_instance(&summary("sid-1", "/home/me/proj", Some("x")), true, "");
        assert!(structured.is_structured());
        assert_eq!(structured.acp_session_id.as_deref(), Some("sid-1"));
        assert_eq!(structured.import_pending, Some(true));
        assert_eq!(structured.resume_intent, ResumeIntent::Default);
    }

    #[test]
    fn already_imported_matches_every_spelling_of_a_claim() {
        let mut by_resume = Instance::new("a", "/p");
        by_resume.resume_intent = ResumeIntent::Use("id-1".to_string());
        let mut by_observed = Instance::new("b", "/p");
        by_observed.agent_session_id = Some("id-2".to_string());
        let mut by_structured = Instance::new("c", "/p");
        by_structured.acp_session_id = Some("id-3".to_string());
        let fresh = Instance::new("d", "/p");
        let instances = vec![by_resume, by_observed, by_structured, fresh];

        for (id, claimed) in [
            ("id-1", true),
            ("id-2", true),
            ("id-3", true),
            ("id-4", false),
        ] {
            assert_eq!(already_imported(&instances, id), claimed, "{id}");
        }
    }
}

#[cfg(test)]
mod show_json_tests {
    use super::*;

    #[test]
    fn relationship_lines_name_parent_and_children() {
        let parent = Instance::new("orchestrator", "/repo");
        let mut child = Instance::new("worker", "/repo");
        child.parent_session_id = Some(parent.id.clone());
        let mut orphan = Instance::new("stray", "/repo");
        orphan.parent_session_id = Some("gone".to_string());
        let instances = vec![parent.clone(), child.clone(), orphan.clone()];

        for (inst, expected) in [
            (
                &parent,
                vec![
                    "  Children:".to_string(),
                    format!("    worker ({})", child.id),
                ],
            ),
            (
                &child,
                vec![format!("  Parent:  orchestrator ({})", parent.id)],
            ),
            (&orphan, vec!["  Parent:  gone".to_string()]),
        ] {
            assert_eq!(
                relationship_lines(inst, &instances),
                expected,
                "{}",
                inst.title
            );
        }
    }

    #[test]
    fn show_json_reports_state_and_only_the_timestamps_that_apply() {
        let plain = Instance::new("z", "/repo");
        let serialized = serde_json::to_string(&session_details(&plain, "p")).unwrap();
        assert!(!serialized.contains("trashed_at"), "{serialized}");
        assert!(!serialized.contains("archived_at"), "{serialized}");
        assert!(serialized.contains("\"state\":\"live\""), "{serialized}");

        let mut archived = Instance::new("z", "/repo");
        archived.archive();
        let details = session_details(&archived, "p");
        assert_eq!(details.state, "archived");
        assert!(details.archived_at.is_some());
        assert!(details.trashed_at.is_none());

        let mut trashed = archived;
        trashed.trash();
        let details = session_details(&trashed, "p");
        assert_eq!(
            details.state, "trashed",
            "trash outranks an earlier archive"
        );
        assert!(details.trashed_at.is_some());
        assert!(details.archived_at.is_some());
    }

    #[test]
    fn show_json_mirrors_the_api_snooze_and_pin_keys() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::minutes(15);
        let past = now - chrono::Duration::minutes(15);
        let row = |f: &dyn Fn(&mut Instance)| {
            let mut inst = Instance::new("z", "/repo");
            f(&mut inst);
            inst
        };
        let check = |label: &str, f: &dyn Fn(&mut Instance), snooze: bool, pin: bool, state| {
            let value = serde_json::to_value(session_details(&row(f), "p")).unwrap();
            let seen = (
                value.get("snoozed_until").is_some(),
                value.get("pinned_at").is_some(),
                value["state"].as_str(),
            );
            assert_eq!(seen, (snooze, pin, Some(state)), "{label}: {value}");
        };

        check("plain row", &|_| {}, false, false, "live");
        check(
            "active snooze",
            &|i| i.snoozed_until = Some(future),
            true,
            false,
            "live",
        );
        check(
            "expired snooze",
            &|i| i.snoozed_until = Some(past),
            false,
            false,
            "live",
        );
        check("pinned", &|i| i.pinned_at = Some(now), false, true, "live");
        check(
            "snoozed and archived",
            &|i| {
                i.archived_at = Some(now);
                i.snoozed_until = Some(future);
            },
            true,
            false,
            "archived",
        );
        check(
            "pinned and snoozed",
            &|i| {
                i.pinned_at = Some(now);
                i.snoozed_until = Some(future);
            },
            true,
            true,
            "live",
        );
        check(
            "trashed and snoozed",
            &|i| {
                i.snooze(30);
                i.trash();
            },
            true,
            false,
            "trashed",
        );
        check(
            "trashed and pinned",
            &|i| {
                i.pin();
                i.trash();
            },
            false,
            true,
            "trashed",
        );
        check(
            "pinned and archived",
            &|i| {
                i.archived_at = Some(now);
                i.pinned_at = Some(now);
            },
            false,
            true,
            "archived",
        );

        let active = serde_json::to_value(session_details(
            &row(&|i| i.snoozed_until = Some(future)),
            "p",
        ))
        .unwrap();
        assert_eq!(
            active["snoozed_until"],
            serde_json::to_value(future).unwrap()
        );
    }
}

/// WO#1953 — `aoe session keep <id> [--off]` and the CLI-side refusals.
#[cfg(test)]
mod keep_command_tests {
    use super::{
        archive_session, keep_session, snooze_session, ArchiveArgs, KeepArgs, SessionCommands,
        SnoozeArgs,
    };
    use crate::session::{Instance, Storage};
    use clap::Parser;
    use serial_test::serial;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: SessionCommands,
    }

    #[test]
    fn keep_parses_identifier_and_off_flag() {
        let cli = Cli::try_parse_from(["aoe", "keep", "abc123"]).expect("keep must parse");
        match cli.cmd {
            SessionCommands::Keep(args) => {
                assert_eq!(args.identifier, "abc123");
                assert!(!args.off);
            }
            _ => panic!("wrong subcommand"),
        }
        let cli = Cli::try_parse_from(["aoe", "keep", "abc123", "--off"]).expect("--off parses");
        match cli.cmd {
            SessionCommands::Keep(args) => assert!(args.off),
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn keep_has_no_force_flag_anywhere() {
        // The contract: there is no --force on keep, archive, snooze or remove.
        assert!(Cli::try_parse_from(["aoe", "keep", "x", "--force"]).is_err());
        assert!(Cli::try_parse_from(["aoe", "archive", "x", "--force"]).is_err());
        assert!(Cli::try_parse_from(["aoe", "snooze", "x", "--force"]).is_err());
    }

    fn seed(profile: &str, kept: bool) -> (Storage, String) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("kept-cli", "/tmp/kept-cli");
        if kept {
            inst.keep(Some("test:seed"));
        }
        let id = inst.id.clone();
        storage
            .update(|instances, _groups| {
                *instances = vec![inst];
                Ok(())
            })
            .unwrap();
        (storage, id)
    }

    #[tokio::test]
    #[serial]
    async fn keep_sets_and_clears_the_flag_without_a_daemon() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (storage, id) = seed("keep-cli", false);

        keep_session(KeepArgs {
            identifier: id.clone(),
            off: false,
        })
        .await
        .unwrap();
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert!(row.is_kept());
        let by = row.kept_by.clone().expect("kept_by recorded");
        assert!(by.starts_with("cli:"), "by = {by}");

        keep_session(KeepArgs {
            identifier: id.clone(),
            off: true,
        })
        .await
        .unwrap();
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert!(!row.is_kept());
    }

    #[tokio::test]
    #[serial]
    async fn archive_refuses_a_kept_session_and_names_the_flag() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (storage, id) = seed("keep-archive", true);
        let err = archive_session(
            "keep-archive",
            ArchiveArgs {
                identifier: id.clone(),
                no_kill: true,
                confirm_kept: false,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("kept"), "{err}");
        assert!(err.contains("archive"), "{err}");
        assert!(
            err.contains(&format!("aoe session keep --off {id}")),
            "{err}"
        );
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert!(!row.is_archived() && row.is_kept());
    }

    #[tokio::test]
    #[serial]
    async fn snooze_refuses_a_kept_session() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (storage, id) = seed("keep-snooze", true);
        let err = snooze_session(
            "keep-snooze",
            SnoozeArgs {
                identifier: id.clone(),
                minutes: Some(30),
                confirm_kept: false,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("kept") && err.contains("snooze"), "{err}");
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert!(!row.is_snoozed() && row.is_kept());
    }

    /// WO#1980-1: the refusal prints the exact one-line override.
    #[tokio::test]
    #[serial]
    async fn archive_refusal_prints_the_confirm_kept_override() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (_storage, id) = seed("keep-archive-hint", true);
        let err = archive_session(
            "keep-archive-hint",
            ArchiveArgs {
                identifier: id.clone(),
                no_kill: true,
                confirm_kept: false,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(&format!("aoe session archive {id} --confirm-kept")),
            "{err}"
        );
    }

    /// WO#1980-1: `--confirm-kept` clears the flag (logged who) and archives
    /// in ONE command.
    #[tokio::test]
    #[serial]
    async fn archive_with_confirm_kept_clears_the_flag_and_archives() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (storage, id) = seed("keep-archive-ok", true);
        archive_session(
            "keep-archive-ok",
            ArchiveArgs {
                identifier: id.clone(),
                no_kill: true,
                confirm_kept: true,
            },
        )
        .await
        .unwrap();
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert!(row.is_archived(), "archived in one step");
        assert!(!row.is_kept(), "keep flag cleared by the override");
    }

    #[tokio::test]
    #[serial]
    async fn snooze_with_confirm_kept_clears_the_flag_and_snoozes() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (storage, id) = seed("keep-snooze-ok", true);
        snooze_session(
            "keep-snooze-ok",
            SnoozeArgs {
                identifier: id.clone(),
                minutes: Some(30),
                confirm_kept: true,
            },
        )
        .await
        .unwrap();
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert!(row.is_snoozed() && !row.is_kept());
    }

    #[test]
    fn confirm_kept_parses_on_archive_snooze_and_rm() {
        let cli = Cli::try_parse_from(["aoe", "archive", "x", "--confirm-kept"]).unwrap();
        match cli.cmd {
            SessionCommands::Archive(a) => assert!(a.confirm_kept),
            _ => panic!("wrong subcommand"),
        }
        let cli = Cli::try_parse_from(["aoe", "snooze", "x", "--minutes", "5", "--confirm-kept"])
            .unwrap();
        match cli.cmd {
            SessionCommands::Snooze(a) => assert!(a.confirm_kept),
            _ => panic!("wrong subcommand"),
        }
        // and it is NOT a --force: that name stays rejected
        assert!(Cli::try_parse_from(["aoe", "archive", "x", "--force"]).is_err());
    }

    #[test]
    fn show_json_and_list_json_carry_the_keep_fields() {
        let mut inst = Instance::new("k", "/tmp/k");
        let bare = serde_json::to_value(super::session_details(&inst, "p")).unwrap();
        assert_eq!(bare["kept"], false);
        assert!(bare.get("kept_at").is_none());
        let bare = serde_json::to_value(crate::cli::list::session_json(&inst, "p")).unwrap();
        assert_eq!(bare["kept"], false);
        assert!(bare.get("kept_by").is_none());

        inst.keep(Some("tui"));
        let v = serde_json::to_value(super::session_details(&inst, "p")).unwrap();
        assert_eq!(v["kept"], true);
        assert_eq!(v["kept_by"], "tui");
        assert!(v["kept_at"].is_string());
        let v = serde_json::to_value(crate::cli::list::session_json(&inst, "p")).unwrap();
        assert_eq!(v["kept"], true);
        assert_eq!(v["kept_by"], "tui");
    }
}
