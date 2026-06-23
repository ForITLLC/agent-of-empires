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
    /// `true` => the worktree directory was deleted; `false` => preserved.
    worktree_deleted: bool,
    /// `true` => the git branch was deleted; `false` => preserved.
    branch_deleted: bool,
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
    // Always emit a structured tracing event first. This is the durable path:
    // it lands in the daemon debug log even if the file append below fails.
    tracing::info!(
        target: "session.audit",
        event = ?event,
        actor = %actor,
        session_id = %inst.id,
        title = %inst.title,
        project_path = %inst.project_path,
        profile = %profile,
        worktree_deleted,
        branch_deleted,
        "session lifecycle audit",
    );

    let rec = Record {
        ts: chrono::Utc::now(),
        event,
        actor,
        session_id: &inst.id,
        title: &inst.title,
        project_path: &inst.project_path,
        profile,
        worktree_deleted,
        branch_deleted,
    };

    if let Err(e) = append(&rec) {
        tracing::warn!(
            target: "session.audit",
            session_id = %inst.id,
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
}
