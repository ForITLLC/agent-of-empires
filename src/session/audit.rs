//! Append-only audit trail for session lifecycle events that destroy or hide
//! a session's record (archive, unarchive, remove, hard-delete).
//!
//! WHY THIS EXISTS: a session ("for-Jamf") once vanished from `sessions.json`
//! with no record of *who* removed it, *when*, or whether its worktree was
//! kept — there was no audit surface at all. Every record-affecting event now
//! leaves a durable, append-only JSON line here AND a structured `tracing`
//! event in the daemon log, so a disappearance is always explainable after the
//! fact. The file carries exactly the fields the incident needed: timestamp,
//! id, title, project_path, actor, and worktree kept-vs-deleted.
//!
//! Best-effort by contract: a write failure here must NEVER fail the caller's
//! operation. The `tracing` event always fires (so the record survives in the
//! daemon log even if the app dir is read-only); the file append is
//! fire-and-forget with a warning on failure.

use serde::Serialize;

use crate::session::Instance;

/// File name (under the aoe app dir) of the append-only audit log.
pub const AUDIT_LOG_NAME: &str = "session-audit.jsonl";

/// Window during which an explicitly-audited removal (`Archive` / `Remove` /
/// `HardDelete`) suppresses a `RegistryPrune` disappearance audit for the same
/// id. A normal `aoe remove`/archive drops the sessions.json row and tears the
/// pane down; the daemon's reload-diff detector would otherwise also log that
/// departure as a phantom prune. Generous enough to cover the gap between the
/// remove's disk write and the next reload (poll = 2s, disk-watch debounce =
/// 75ms), short enough not to mask a genuine later prune of a reused id.
const RECENT_REMOVAL_TTL: std::time::Duration = std::time::Duration::from_secs(10);

fn recent_removals(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static R: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Mark `id` as explicitly removed just now, so `was_recently_removed` can tell
/// an intentional removal from a silent registry loss. Opportunistically evicts
/// entries past the TTL to keep the map bounded.
fn mark_removed(id: &str) {
    if let Ok(mut m) = recent_removals().lock() {
        let now = std::time::Instant::now();
        m.retain(|_, t| now.duration_since(*t) < RECENT_REMOVAL_TTL);
        m.insert(id.to_string(), now);
    }
}

/// True if `id` was explicitly removed/archived within `RECENT_REMOVAL_TTL`.
/// The daemon's registry-prune detector calls this to suppress a phantom prune
/// audit for a session a normal remove path already accounted for.
pub fn was_recently_removed(id: &str) -> bool {
    match recent_removals().lock() {
        Ok(m) => m
            .get(id)
            .is_some_and(|t| std::time::Instant::now().duration_since(*t) < RECENT_REMOVAL_TTL),
        Err(_) => false,
    }
}

/// What happened to the session record.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Event {
    /// Record preserved, sunk in the Attention sort + tmux torn down.
    /// Restorable via `aoe restore`. This is the new default for `aoe remove`.
    Archive,
    /// Record restored from the archived state (`aoe restore` / `unarchive`).
    Unarchive,
    /// Record dropped from `sessions.json` with on-disk artifacts preserved
    /// (the legacy no-flag `remove` behaviour; reachable only via `--hard`
    /// without `--delete-worktree`).
    Remove,
    /// Record dropped AND on-disk artifacts (worktree/branch/container)
    /// deleted per flags — the destructive, non-restorable path (`--hard`).
    HardDelete,
    /// The session's tmux pane exited/crashed (pane reported dead by the
    /// daemon poll). `exit_code` carries the dead pane's exit status when tmux
    /// reports it. The session was witnessed alive in a prior poll and is now
    /// gone — this is the "it DIED" signal (vs. an explicit remove).
    PaneExit,
    /// The session's tmux pane/window is gone entirely (vanished from tmux
    /// without a captured exit status). Witnessed-alive → gone transition;
    /// distinct from a clean `aoe remove`, which tears the pane down
    /// deliberately and is recorded as `archive`/`remove`/`hard-delete`.
    TmuxGone,
    /// The session record disappeared from `sessions.json` while its tmux pane
    /// still exists — a genuine registry loss (prune/corruption) under a LIVE
    /// session, NOT an explicit remove. The forensic "deleted vs died" signal
    /// for registry-side disappearance.
    RegistryPrune,
}

/// One audit line. Field order matches the incident's requirement:
/// ts, id, title, project_path, actor, worktree kept-vs-deleted.
#[derive(Serialize)]
struct Record<'a> {
    ts: chrono::DateTime<chrono::Utc>,
    event: Event,
    actor: &'a str,
    session_id: &'a str,
    title: &'a str,
    project_path: &'a str,
    profile: &'a str,
    /// Dead pane exit status for `pane-exit` events; omitted when not known
    /// (tmux-gone, registry-prune) or not applicable (record-destroying ops).
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    /// `true` => the worktree directory was deleted; `false` => preserved.
    /// Omitted for disappearance events (pane-exit / tmux-gone / registry-prune
    /// do not act on the worktree).
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_deleted: Option<bool>,
    /// `true` => the git branch was deleted; `false` => preserved.
    /// Omitted for disappearance events.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_deleted: Option<bool>,
}

/// Record a session lifecycle event to the audit log + the daemon trace.
///
/// `actor` is a short provenance tag for the caller (e.g. `"cli-remove"`,
/// `"cli-remove-hard"`, `"cli-restore"`, `"session-archive"`, `"tui"`).
/// `worktree_deleted` / `branch_deleted` record the kept-vs-deleted outcome.
pub fn record(
    event: Event,
    actor: &str,
    inst: &Instance,
    profile: &str,
    worktree_deleted: bool,
    branch_deleted: bool,
) {
    // Record-dropping events explain a future registry absence; mark the id so
    // the daemon's prune detector does not double-log it as a silent loss.
    if matches!(event, Event::Archive | Event::Remove | Event::HardDelete) {
        mark_removed(&inst.id);
    }
    emit(&Record {
        ts: chrono::Utc::now(),
        event,
        actor,
        session_id: &inst.id,
        title: &inst.title,
        project_path: &inst.project_path,
        profile,
        exit_code: None,
        worktree_deleted: Some(worktree_deleted),
        branch_deleted: Some(branch_deleted),
    });
}

/// Record a session *disappearance* — a session leaving the live set WITHOUT
/// an explicit `aoe remove`/archive. Covers `PaneExit` (pane crashed/exited,
/// `exit_code` set when tmux reports it), `TmuxGone` (pane vanished, no exit
/// status), and `RegistryPrune` (record dropped from `sessions.json` while the
/// pane was still alive). This is the "it DIED / was pruned, nobody removed it"
/// half of the deleted-vs-died forensic answer.
///
/// Field-based rather than `&Instance` because by the time the daemon notices
/// a session is gone it usually holds only the prior snapshot's fields, not a
/// live `Instance`.
pub fn record_disappearance(
    event: Event,
    actor: &str,
    session_id: &str,
    title: &str,
    project_path: &str,
    profile: &str,
    exit_code: Option<i32>,
) {
    emit(&Record {
        ts: chrono::Utc::now(),
        event,
        actor,
        session_id,
        title,
        project_path,
        profile,
        exit_code,
        worktree_deleted: None,
        branch_deleted: None,
    });
}

/// Emit one audit record: a structured `tracing` event first (the durable path
/// — it lands in the daemon debug log even if the file append fails), then a
/// best-effort append to the on-disk audit log.
fn emit(rec: &Record) {
    tracing::info!(
        target: "session.audit",
        event = ?rec.event,
        actor = %rec.actor,
        session_id = %rec.session_id,
        title = %rec.title,
        project_path = %rec.project_path,
        profile = %rec.profile,
        exit_code = ?rec.exit_code,
        worktree_deleted = ?rec.worktree_deleted,
        branch_deleted = ?rec.branch_deleted,
        "session lifecycle audit",
    );

    if let Err(e) = append(rec) {
        tracing::warn!(
            target: "session.audit",
            session_id = %rec.session_id,
            error = %e,
            "failed to append session audit record (event still in tracing log)",
        );
    }
}

/// Append a single newline-delimited JSON record to the audit log under the
/// aoe app dir, creating the file if needed.
fn append(rec: &Record) -> anyhow::Result<()> {
    use std::io::Write;

    let path = crate::session::get_app_dir()?.join(AUDIT_LOG_NAME);
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use serial_test::serial;

    /// Point HOME (+ XDG) at a throwaway dir so `get_app_dir()` resolves under
    /// the temp tree and the audit file does not touch the real app dir.
    fn isolate_app_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("create temp home for audit tests");
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        tmp
    }

    fn read_audit_lines() -> Vec<Value> {
        let path = crate::session::get_app_dir().unwrap().join(AUDIT_LOG_NAME);
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("audit line must be valid JSON"))
            .collect()
    }

    #[test]
    #[serial]
    fn record_appends_one_parseable_line_with_required_fields() {
        let _tmp = isolate_app_dir();
        let mut inst = Instance::new("for-Jamf", "/tmp/for-jamf");
        inst.id = "jamf-session-id-1234".to_string();

        record(
            Event::HardDelete,
            "cli-remove-hard",
            &inst,
            "forit-work",
            true,
            true,
        );

        let lines = read_audit_lines();
        assert_eq!(lines.len(), 1, "exactly one audit line expected");
        let r = &lines[0];
        assert_eq!(r["event"], "hard-delete");
        assert_eq!(r["actor"], "cli-remove-hard");
        assert_eq!(r["session_id"], "jamf-session-id-1234");
        assert_eq!(r["title"], "for-Jamf");
        assert_eq!(r["project_path"], "/tmp/for-jamf");
        assert_eq!(r["profile"], "forit-work");
        assert_eq!(r["worktree_deleted"], true);
        assert_eq!(r["branch_deleted"], true);
        assert!(r["ts"].is_string(), "ts must be an RFC3339 string");
    }

    #[test]
    #[serial]
    fn record_is_append_only_across_calls() {
        let _tmp = isolate_app_dir();
        let inst = Instance::new("s", "/tmp/s");
        record(Event::Archive, "cli-remove", &inst, "p", false, false);
        record(Event::Unarchive, "cli-restore", &inst, "p", false, false);

        let lines = read_audit_lines();
        assert_eq!(lines.len(), 2, "audit log must accumulate, not overwrite");
        assert_eq!(lines[0]["event"], "archive");
        assert_eq!(lines[0]["worktree_deleted"], false);
        assert_eq!(lines[1]["event"], "unarchive");
    }

    #[test]
    #[serial]
    fn record_disappearance_serializes_exit_code_and_omits_worktree_fields() {
        let _tmp = isolate_app_dir();
        record_disappearance(
            Event::PaneExit,
            "daemon-poll",
            "dead-sess-id-9",
            "for-Ghost",
            "/tmp/for-ghost",
            "forit-main",
            Some(137),
        );

        let lines = read_audit_lines();
        assert_eq!(lines.len(), 1, "exactly one disappearance line expected");
        let r = &lines[0];
        assert_eq!(r["event"], "pane-exit");
        assert_eq!(r["actor"], "daemon-poll");
        assert_eq!(r["session_id"], "dead-sess-id-9");
        assert_eq!(r["title"], "for-Ghost");
        assert_eq!(r["project_path"], "/tmp/for-ghost");
        assert_eq!(r["profile"], "forit-main");
        assert_eq!(r["exit_code"], 137, "exit_code must serialize when present");
        assert!(
            r.get("worktree_deleted").is_none(),
            "disappearance line must omit worktree_deleted"
        );
        assert!(
            r.get("branch_deleted").is_none(),
            "disappearance line must omit branch_deleted"
        );
    }

    #[test]
    #[serial]
    fn record_marks_destructive_events_for_prune_suppression() {
        let _tmp = isolate_app_dir();
        let mut inst = Instance::new("for-Mark", "/tmp/for-mark");
        inst.id = "mark-suppress-id".to_string();
        assert!(!was_recently_removed("mark-suppress-id"));

        record(Event::Remove, "cli-remove", &inst, "p", false, false);
        assert!(
            was_recently_removed("mark-suppress-id"),
            "Remove must mark the id so the prune detector stays silent"
        );

        // Unarchive is a restore, not a departure; it must NOT suppress a prune.
        let mut other = Instance::new("for-Other", "/tmp/for-other");
        other.id = "unarchive-not-marked-id".to_string();
        record(Event::Unarchive, "cli-restore", &other, "p", false, false);
        assert!(
            !was_recently_removed("unarchive-not-marked-id"),
            "Unarchive must not suppress a prune"
        );
    }

    #[test]
    #[serial]
    fn record_disappearance_omits_exit_code_when_none() {
        let _tmp = isolate_app_dir();
        record_disappearance(
            Event::RegistryPrune,
            "daemon-reload",
            "pruned-id-3",
            "for-Lost",
            "/tmp/for-lost",
            "forit-work",
            None,
        );

        let lines = read_audit_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["event"], "registry-prune");
        assert!(
            lines[0].get("exit_code").is_none(),
            "exit_code must be omitted when None"
        );
    }
}
