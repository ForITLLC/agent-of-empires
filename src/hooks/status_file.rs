//! Status file I/O for hooks-based agent status detection.
//!
//! Public reader/writer surface that delegates to `dir_guard` for every
//! file operation. The four readers (`read_hook_status`, `read_hook_session_id`,
//! `read_hook_urgent`, `cleanup_hook_status_dir`) and `hook_status_dir` are
//! the stable contract; their internals all ride `*at`-anchored I/O on a
//! verified host base directory (`/tmp/aoe-hooks-<euid>`).

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use uuid::Uuid;

use crate::session::Status;

use super::dir_guard;

/// Maximum age before a sidecar `session_id` file is considered stale.
pub(crate) const SESSION_ID_SIDECAR_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// Maximum age of a `running` status file before it is treated as stale.
///
/// The hook writes `running` on PreToolUse and only resets it to `idle` on
/// Stop/SubagentStop. If Stop never fires (kill -9, crash, compaction mid-tool,
/// or a hook error) the file stays `running` forever, so the FleetView would
/// pin a blue "active" bar on a session that is actually idle. Past this age we
/// stop trusting a `running` value and return `None`, letting callers fall back
/// to live pane-content detection — which still reports Running for a genuine
/// long tool call (the pane shows the running spinner) but Idle for a parked
/// prompt. This self-heals a missed Stop WITHOUT false-idling a busy session,
/// and is hook-independent (the renderer recovers even if the hook never runs).
/// Only `running` is aged; the other states are stable terminal values whose
/// last write remains authoritative. An active session keeps this fresh because
/// every PreToolUse rewrites the file, and the attention hook stamps it each
/// tick — so 60s comfortably exceeds the inter-tool gap of a working turn.
pub(crate) const HOOK_RUNNING_STALE_MAX_AGE: Duration = Duration::from_secs(60);

/// Cap for an urgent flag that carries no `urgent_expires_at` stamp. Every
/// first-party writer stamps an expiry (15 min default, 60 min ceiling), so an
/// unstamped flag is foreign or hand-written. The reader ages it out at the
/// writers' default TTL, measured from the file mtime, so a bare flag can
/// never pin a row red forever.
pub(crate) const URGENT_NO_EXPIRY_MAX_AGE: Duration = Duration::from_secs(900);

/// Cap used when reading a status file. The legitimate values are short
/// tokens; an attacker-planted larger payload is irrelevant either way.
const STATUS_FILE_READ_CAP: usize = 64;
const SESSION_ID_FILE_READ_CAP: usize = 128;
const ATTENTION_FILE_READ_CAP: usize = 16 * 1024;

/// `<host base>/<instance_id>`. The base is the per-user directory
/// `/tmp/aoe-hooks-<euid>` resolved by `dir_guard::hook_base_path()`.
/// `Err` if `instance_id` fails `validate_instance_id`.
///
/// The path is informational (used by the sandbox bind-mount source string and
/// by debug logs); production I/O goes through `dir_guard` and never path-joins.
pub fn hook_status_dir(instance_id: &str) -> Result<PathBuf> {
    crate::session::validate_instance_id(instance_id)?;
    Ok(dir_guard::hook_base_path().join(instance_id))
}

/// Read the hook-written status file for the given instance.
///
/// Returns `None` if the file doesn't exist, the symlink is forbidden, or
/// initialization of the per-user base failed (squatted or wrong-mode dir).
pub fn read_hook_status(instance_id: &str) -> Option<Status> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let bytes = dir_guard::read_file_at(dir.as_fd(), "status", STATUS_FILE_READ_CAP).ok()??;
    let status = parse_status(&bytes)?;

    // Staleness self-heal: a `running` value is only trustworthy while the hook
    // keeps heartbeating it. If Stop never fired (kill -9/crash/compaction/hook
    // error) the file is pinned at `running` — past HOOK_RUNNING_STALE_MAX_AGE
    // we drop it (return None) so callers fall back to live pane-content
    // detection instead of pinning a blue bar on an idle session. Fail-safe: a
    // stat error or future-dated mtime keeps the current value (no flicker).
    if status == Status::Running {
        if let Ok(Some(meta)) = dir_guard::metadata_at(dir.as_fd(), "status") {
            let stale = meta
                .modified()
                .ok()
                .and_then(|mtime| mtime.elapsed().ok())
                .map(|age| age > HOOK_RUNNING_STALE_MAX_AGE)
                .unwrap_or(false);
            if stale {
                return None;
            }
        }
    }
    Some(status)
}

/// Time since the hook status file was last written, i.e. how long the current
/// value has been standing.
///
/// The running-mapped hooks (`PreToolUse`, `UserPromptSubmit`, `ElicitationResult`)
/// rewrite the file on every fire, so a fresh mtime means the last write is
/// recent. `reconcile_claude_hook_status` uses this to tell a genuinely fresh
/// `running` (a turn that just started, spinner not yet rendered) from a stale
/// one that a missed idle hook left standing after the turn ended.
///
/// Returns `None` when the file is absent or its mtime can't be read.
pub fn read_hook_status_age(instance_id: &str) -> Option<std::time::Duration> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let meta = dir_guard::metadata_at(dir.as_fd(), "status").ok()??;
    meta.modified().ok()?.elapsed().ok()
}

fn parse_status(bytes: &[u8]) -> Option<Status> {
    let trimmed = std::str::from_utf8(bytes).ok()?.trim();
    match trimmed {
        "running" => Some(Status::Running),
        "waiting" => Some(Status::Waiting),
        "idle" => Some(Status::Idle),
        "error" => Some(Status::Error),
        other => {
            tracing::warn!(target: "hooks.status", "Unexpected hook status value: {:?}", other);
            None
        }
    }
}

/// Read a Claude session UUID from the hook-written `session_id` sidecar.
///
/// Returns `None` when the file is absent, malformed (non-UUID), or older
/// than `SESSION_ID_SIDECAR_MAX_AGE`.
pub fn read_hook_session_id(instance_id: &str) -> Option<String> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let meta = dir_guard::metadata_at(dir.as_fd(), "session_id").ok()??;
    let mtime = meta.modified().ok()?;
    if mtime.elapsed().ok()? > SESSION_ID_SIDECAR_MAX_AGE {
        return None;
    }
    let bytes =
        dir_guard::read_file_at(dir.as_fd(), "session_id", SESSION_ID_FILE_READ_CAP).ok()??;
    let id = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    if Uuid::parse_str(&id).is_ok() {
        Some(id)
    } else {
        None
    }
}

/// Read the urgent flag from the hook-written `attention.json`.
///
/// See the `attention-urgent` cx-script for the writer contract: `urgent`
/// boolean plus optional `urgent_expires_at` epoch seconds.
pub fn read_hook_urgent(instance_id: &str) -> bool {
    let Ok(Some(dir)) = dir_guard::open_instance_dir_read_only(instance_id) else {
        return false;
    };
    let Ok(Some(bytes)) =
        dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
    else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    if !value
        .get("urgent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    if let Some(exp) = value.get("urgent_expires_at").and_then(|v| v.as_i64()) {
        let now = crate::util::now_secs() as i64;
        if now > exp {
            return false;
        }
    } else if let Ok(Some(meta)) = dir_guard::metadata_at(dir.as_fd(), "attention.json") {
        // No expiry stamp: age the flag out at the writers' default TTL so a
        // bare `urgent: true` can never pin a row red forever. Fail-safe
        // mirrors read_hook_status: a stat error or future-dated mtime keeps
        // the flag live (no flicker on clock skew).
        let stale = meta
            .modified()
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .map(|age| age > URGENT_NO_EXPIRY_MAX_AGE)
            .unwrap_or(false);
        if stale {
            return false;
        }
    }
    true
}

/// Merge a pane-watchdog urgent flag into the instance's `attention.json`
/// without disturbing hook-written fields (tier/reason/tool survive; only
/// the `urgent_*` family is stamped). The watchdog re-detects every tick,
/// so a still-blocked pane keeps its flag fresh and `urgent_expires_at`
/// clears the row after recovery. A corrupt or non-object file is replaced
/// wholesale — the urgent flag must not be lost to junk on disk.
pub fn merge_watchdog_urgent(
    instance_id: &str,
    reason: &str,
    kind: &str,
    ttl: Duration,
) -> Result<()> {
    let dir = dir_guard::open_instance_dir(instance_id)?;
    let mut value = dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let obj = value.as_object_mut().expect("filtered to object above");
    obj.insert("urgent".into(), true.into());
    obj.insert(
        "urgent_reason".into(),
        reason.chars().take(280).collect::<String>().into(),
    );
    obj.insert("urgent_kind".into(), kind.into());
    obj.insert("urgent_source".into(), "pane-watchdog".into());
    obj.insert("urgent_set_at".into(), now.into());
    obj.insert(
        "urgent_expires_at".into(),
        (now + ttl.as_secs() as i64).into(),
    );
    dir_guard::write_atomic(dir.as_fd(), "attention.json", value.to_string().as_bytes())
}

/// Every key the `attention-urgent` writer stamps alongside `urgent`. An ack
/// strips all of them so a later [`read_hook_urgent`] can never see a
/// half-cleared flag.
const URGENT_KEYS: [&str; 5] = [
    "urgent",
    "urgent_reason",
    "urgent_set_at",
    "urgent_expires_at",
    "urgent_kind",
];

/// Urgent kinds that resolve OUT-OF-BAND — a browser sign-in (`auth`), a
/// rate-cap reset (`cap`), an API overload that auto-resumes (`overload`), a
/// gateway restart (`mcp`) — rather than by the agent receiving its next
/// message. Machine traffic (fleet dispatches, loop ticks, harness
/// continuations) must not wipe them before a human has seen the row; only a
/// message that reads as genuine human input, or expiry, clears them. Mirrors
/// the `claude-attention-signal-hook.py` `_clear_urgent` contract.
const STICKY_URGENT_KINDS: [&str; 4] = ["auth", "cap", "overload", "mcp"];

/// A message starting with one of these is a harness continuation, not a
/// human prompt.
const HARNESS_PREFIXES: [&str; 7] = [
    "<system-reminder>",
    "Stop hook feedback:",
    "<command-name>",
    "<local-command",
    "<task-notification",
    "# Autonomous loop check",
    "<<autonomous-loop",
];

/// Fleet-protocol markers: a message carrying one was machine-composed.
const FLEET_MARKERS: [&str; 2] = ["-- FLEET-AUTH v1 --", "SELF-HARDENING PROBE"];

/// Peer/agent signature line, e.g. `— AoE-Commander (e284618842464176)`: a
/// message that ends a line with one was composed by another agent.
static FLEET_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*[—–-]{1,2}\s*\S[^\n(]{0,60}\(([0-9a-f]{6,32})\)\s*$")
        .expect("fleet signature regex is valid")
});

/// True when `text` reads as a genuine human prompt rather than harness or
/// fleet traffic. Conservative: empty or unclassifiable → `false`, so sticky
/// urgents survive.
pub fn is_human_prompt(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    if HARNESS_PREFIXES.iter().any(|p| text.starts_with(p)) {
        return false;
    }
    if FLEET_MARKERS.iter().any(|m| text.contains(m)) {
        return false;
    }
    if FLEET_SIG_RE.is_match(text) {
        return false;
    }
    true
}

/// Outcome of [`ack_hook_urgent_on_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrgentAck {
    /// No urgent flag was set (or no attention file exists).
    Absent,
    /// A sticky urgent kind is unexpired and the message is machine traffic:
    /// the flag was left intact.
    Kept,
    /// The urgent fields were stripped.
    Cleared,
}

impl UrgentAck {
    /// Wire form for API responses.
    pub fn as_str(self) -> &'static str {
        match self {
            UrgentAck::Absent => "absent",
            UrgentAck::Kept => "kept",
            UrgentAck::Cleared => "cleared",
        }
    }
}

/// Acknowledge a session's urgent flag when a message is delivered to it.
///
/// Delivering a message is the authoritative "someone is handling this":
/// a non-sticky urgent clears on any delivery; a [`STICKY_URGENT_KINDS`]
/// flag clears only when the message is genuine human input
/// ([`is_human_prompt`]) or the flag has already expired. The tier/reason
/// fields are untouched. Fail-open: a read, parse, or write failure leaves
/// the file as it was — a send must never fail on attention bookkeeping.
pub fn ack_hook_urgent_on_send(instance_id: &str, message: &str) -> UrgentAck {
    let Ok(Some(dir)) = dir_guard::open_instance_dir_read_only(instance_id) else {
        return UrgentAck::Absent;
    };
    let Ok(Some(bytes)) =
        dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
    else {
        return UrgentAck::Absent;
    };
    let Ok(serde_json::Value::Object(mut payload)) =
        serde_json::from_slice::<serde_json::Value>(&bytes)
    else {
        return UrgentAck::Absent;
    };
    if !payload
        .get("urgent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return UrgentAck::Absent;
    }
    let kind = payload
        .get("urgent_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if STICKY_URGENT_KINDS.contains(&kind) {
        let expires = payload.get("urgent_expires_at").and_then(|v| v.as_f64());
        let now = crate::util::now_secs() as f64;
        if matches!(expires, Some(exp) if exp > now) && !is_human_prompt(message) {
            return UrgentAck::Kept;
        }
    }
    for key in URGENT_KEYS {
        payload.remove(key);
    }
    let body = serde_json::Value::Object(payload).to_string();
    match dir_guard::write_atomic(dir.as_fd(), "attention.json", body.as_bytes()) {
        Ok(()) => UrgentAck::Cleared,
        Err(e) => {
            tracing::warn!(target: "hooks.status",
                "ack_hook_urgent_on_send: {} left as-is: {}", instance_id, e);
            UrgentAck::Kept
        }
    }
}

/// Remove the hook status directory for a given instance (cleanup on stop/delete).
/// Symlink-safe via `dir_guard::remove_instance_dir` (`unlinkat` walk).
pub fn cleanup_hook_status_dir(instance_id: &str) {
    if let Err(e) = dir_guard::remove_instance_dir(instance_id) {
        tracing::warn!(target: "hooks.status",
            "Failed to cleanup hook status dir for {}: {}", instance_id, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::BaseGuard;
    use std::os::fd::AsFd;
    use std::time::Duration;

    fn write_status_via_guard(instance_id: &str, content: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_short(dir.as_fd(), "status", content.as_bytes()).unwrap();
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_running_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_running", "running");
        assert_eq!(read_hook_status("read_running"), Some(Status::Running));
    }

    /// Age a file in the instance dir to `now - offset` (mirrors the
    /// session_id stale-file test).
    fn age_hook_file(base: &std::path::Path, instance_id: &str, name: &str, offset: Duration) {
        let stale = std::time::SystemTime::now() - offset;
        std::fs::File::options()
            .write(true)
            .open(base.join(instance_id).join(name))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_running_status_drops_when_stale() {
        // WO #159: a `running` file pinned by a missed Stop (kill -9/crash)
        // self-heals — past HOOK_RUNNING_STALE_MAX_AGE the reader returns None so
        // callers fall back to live content detection instead of pinning blue.
        let (_g, base, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_running_stale", "running");
        age_hook_file(
            &base,
            "read_running_stale",
            "status",
            HOOK_RUNNING_STALE_MAX_AGE + Duration::from_secs(30),
        );
        assert_eq!(read_hook_status("read_running_stale"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_running_status_fresh_stays_running() {
        // A freshly-heartbeated `running` (active work) is trusted → stays blue.
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_running_fresh", "running");
        assert_eq!(
            read_hook_status("read_running_fresh"),
            Some(Status::Running)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_waiting_status_survives_stale_mtime() {
        // Only `running` is aged; a stable `waiting` value remains authoritative
        // no matter how old the file is (it is not the stuck-blue failure mode).
        let (_g, base, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_waiting_old", "waiting");
        age_hook_file(
            &base,
            "read_waiting_old",
            "status",
            HOOK_RUNNING_STALE_MAX_AGE + Duration::from_secs(600),
        );
        assert_eq!(read_hook_status("read_waiting_old"), Some(Status::Waiting));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_waiting_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_waiting", "waiting");
        assert_eq!(read_hook_status("read_waiting"), Some(Status::Waiting));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_idle_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_idle", "idle");
        assert_eq!(read_hook_status("read_idle"), Some(Status::Idle));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_error_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_err", "error");
        assert_eq!(read_hook_status("read_err"), Some(Status::Error));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_waiting_with_newline() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_nl", "waiting\n");
        assert_eq!(read_hook_status("read_nl"), Some(Status::Waiting));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_missing_file() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_status("nonexistent_instance_id"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_status_age_fresh_after_write() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("age_fresh", "running");
        let age = read_hook_status_age("age_fresh").expect("age present after write");
        assert!(
            age < Duration::from_secs(5),
            "just-written status should be fresh, got {age:?}"
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_status_age_none_when_absent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_status_age("age_absent_instance"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_dangling_symlink() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let dir = dir_guard::open_instance_dir("dangling").unwrap();
        drop(dir);
        std::os::unix::fs::symlink("/nonexistent/target", base.join("dangling").join("status"))
            .unwrap();
        assert_eq!(read_hook_status("dangling"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_unexpected_content() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_unexpected", "something_else");
        assert_eq!(read_hook_status("read_unexpected"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_cleanup_existing_dir() {
        let (_g, base, _tmp) = BaseGuard::ready();
        write_status_via_guard("cleanup_existing", "running");
        let dir = base.join("cleanup_existing");
        assert!(dir.exists());
        cleanup_hook_status_dir("cleanup_existing");
        assert!(!dir.exists());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_cleanup_nonexistent_dir() {
        let (_g, _, _tmp) = BaseGuard::ready();
        cleanup_hook_status_dir("nonexistent_cleanup_test");
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_hook_status_dir_path() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let dir = hook_status_dir("abc123").expect("test id must be allowlist-safe");
        assert_eq!(dir, base.join("abc123"));
    }

    fn write_attention_json(instance_id: &str, body: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_short(dir.as_fd(), "attention.json", body.as_bytes()).unwrap();
    }

    fn read_attention_value(instance_id: &str) -> serde_json::Value {
        let dir = dir_guard::open_instance_dir_read_only(instance_id)
            .unwrap()
            .unwrap();
        let bytes = dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
            .unwrap()
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_merge_watchdog_urgent_creates_file_and_reads_urgent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        merge_watchdog_urgent(
            "wd_urgent_new",
            "capped: usage limit reached",
            "cap",
            Duration::from_secs(3600),
        )
        .unwrap();
        assert!(read_hook_urgent("wd_urgent_new"));
        let v = read_attention_value("wd_urgent_new");
        assert_eq!(v["urgent_kind"], "cap");
        assert_eq!(v["urgent_source"], "pane-watchdog");
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_merge_watchdog_urgent_preserves_hook_fields() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json(
            "wd_urgent_merge",
            r#"{"tier":5,"reason":"tool_invoke","tool":"Bash"}"#,
        );
        merge_watchdog_urgent(
            "wd_urgent_merge",
            "waiting on a device-code sign-in",
            "auth",
            Duration::from_secs(3600),
        )
        .unwrap();
        let v = read_attention_value("wd_urgent_merge");
        assert_eq!(v["tier"], 5);
        assert_eq!(v["reason"], "tool_invoke");
        assert_eq!(v["urgent"], true);
        assert_eq!(v["urgent_kind"], "auth");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let exp = v["urgent_expires_at"].as_i64().unwrap();
        assert!(
            exp > now + 3000 && exp <= now + 3601,
            "expiry {exp} vs now {now}"
        );
        assert!(read_hook_urgent("wd_urgent_merge"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_merge_watchdog_urgent_recovers_from_corrupt_json() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("wd_urgent_corrupt", "{ this is not json");
        merge_watchdog_urgent(
            "wd_urgent_corrupt",
            "capped",
            "cap",
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(read_hook_urgent("wd_urgent_corrupt"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_merge_watchdog_urgent_rejects_bad_instance_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(merge_watchdog_urgent("../etc", "x", "cap", Duration::from_secs(60)).is_err());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_true() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_true", r#"{"urgent":true,"urgent_reason":"x"}"#);
        assert!(read_hook_urgent("urgent_true"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_flag_missing() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_missing", r#"{"tier":0}"#);
        assert!(!read_hook_urgent("urgent_missing"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_file_absent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(!read_hook_urgent("urgent_no_file"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_malformed_json() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_bad_json", "{ this is not json");
        assert!(!read_hook_urgent("urgent_bad_json"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_expires_passed() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_expired", r#"{"urgent":true,"urgent_expires_at":1}"#);
        assert!(!read_hook_urgent("urgent_expired"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_true_when_expires_future() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let future = crate::util::now_secs() + 3600;
        let body = format!(r#"{{"urgent":true,"urgent_expires_at":{}}}"#, future);
        write_attention_json("urgent_future", &body);
        assert!(read_hook_urgent("urgent_future"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_no_expiry_and_stale() {
        // Every first-party writer stamps `urgent_expires_at`, so an
        // unstamped urgent flag is a foreign or hand-written file. The reader
        // caps it at the writers' default TTL (measured from file mtime) so a
        // flag with no expiry can never pin a row red forever.
        let (_g, base, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_unstamped_stale", r#"{"urgent":true}"#);
        age_hook_file(
            &base,
            "urgent_unstamped_stale",
            "attention.json",
            URGENT_NO_EXPIRY_MAX_AGE + Duration::from_secs(5),
        );
        assert!(!read_hook_urgent("urgent_unstamped_stale"));
    }

    fn write_session_id_sidecar(instance_id: &str, content: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_atomic(dir.as_fd(), "session_id", content.as_bytes()).unwrap();
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_returns_some_when_fresh_uuid() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_session_id_sidecar("session_id_fresh", uuid);
        assert_eq!(
            read_hook_session_id("session_id_fresh").as_deref(),
            Some(uuid)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_returns_none_when_absent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(
            read_hook_session_id("nonexistent_session_id_instance"),
            None
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_rejects_non_uuid() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_session_id_sidecar("session_id_garbage", "not-a-uuid");
        assert_eq!(read_hook_session_id("session_id_garbage"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_rejects_stale_file() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_session_id_sidecar("session_id_stale", uuid);
        let stale = std::time::SystemTime::now() - Duration::from_secs(10 * 60);
        std::fs::File::options()
            .write(true)
            .open(base.join("session_id_stale").join("session_id"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
        assert_eq!(read_hook_session_id("session_id_stale"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_trims_trailing_whitespace() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_session_id_sidecar("session_id_trim", &format!("{uuid}\n"));
        assert_eq!(
            read_hook_session_id("session_id_trim").as_deref(),
            Some(uuid)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn hook_status_dir_returns_err_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(hook_status_dir("../etc").is_err());
        assert!(hook_status_dir("").is_err());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_status_returns_none_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_status("../etc"), None);
        assert_eq!(read_hook_status(""), None);
        assert_eq!(read_hook_status("foo/bar"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_session_id_returns_none_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_session_id("../etc"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_urgent_returns_false_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(!read_hook_urgent("../etc"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn cleanup_hook_status_dir_is_noop_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        cleanup_hook_status_dir("../etc");
        cleanup_hook_status_dir("");
    }

    // ── ack_hook_urgent_on_send / is_human_prompt (WO#1641 item 2) ─────────

    const COMMANDER_MSG: &str =
        "WORK ORDER #1: rebuild the thing.\n— AoE-Commander (e284618842464176)";
    const HUMAN_MSG: &str = "hey can you look at the failing deploy";

    fn read_attention_json(instance_id: &str) -> serde_json::Value {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        let bytes = dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
            .unwrap()
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn urgent_body(kind: Option<&str>, expires_at: i64) -> String {
        let kind = kind
            .map(|k| format!(",\"urgent_kind\":\"{k}\""))
            .unwrap_or_default();
        format!(
            "{{\"tier\":3,\"reason\":\"needs_input\",\"urgent\":true,\"urgent_reason\":\"r\",\
             \"urgent_set_at\":1,\"urgent_expires_at\":{expires_at}{kind}}}"
        )
    }

    fn future() -> i64 {
        crate::util::now_secs() as i64 + 600
    }

    #[test]
    fn is_human_prompt_classifies_machine_traffic() {
        assert!(is_human_prompt(HUMAN_MSG));
        assert!(is_human_prompt("call me at (555) 1234 — thanks"));
        assert!(!is_human_prompt(""));
        assert!(!is_human_prompt("   \n"));
        assert!(!is_human_prompt(
            "<system-reminder>continue</system-reminder>"
        ));
        assert!(!is_human_prompt("Stop hook feedback: open tasks remain"));
        assert!(!is_human_prompt(
            "<task-notification>done</task-notification>"
        ));
        assert!(!is_human_prompt("# Autonomous loop check\nstill going"));
        assert!(!is_human_prompt("probe body\n-- FLEET-AUTH v1 --\nsig=abc"));
        assert!(!is_human_prompt("SELF-HARDENING PROBE: run tests"));
        assert!(!is_human_prompt(COMMANDER_MSG));
        assert!(!is_human_prompt(
            "STATUS: shipped\n- for-dev (1f5a22dc28d84abd)"
        ));
        assert!(!is_human_prompt("– per-Dev (abcdef)"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_clears_plain_urgent_on_any_send() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("ack_plain", &urgent_body(None, future()));
        assert!(read_hook_urgent("ack_plain"));
        assert_eq!(
            ack_hook_urgent_on_send("ack_plain", COMMANDER_MSG),
            UrgentAck::Cleared
        );
        assert!(!read_hook_urgent("ack_plain"));
        let v = read_attention_json("ack_plain");
        assert_eq!(v["tier"], 3, "tier survives the ack: {v}");
        assert_eq!(v["reason"], "needs_input");
        for key in URGENT_KEYS {
            assert!(v.get(key).is_none(), "{key} still present: {v}");
        }
        // Idempotent: a second send finds nothing to clear.
        assert_eq!(
            ack_hook_urgent_on_send("ack_plain", HUMAN_MSG),
            UrgentAck::Absent
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_keeps_unexpired_sticky_kind_for_machine_traffic() {
        let (_g, _, _tmp) = BaseGuard::ready();
        for kind in STICKY_URGENT_KINDS {
            let id = format!("ack_sticky_{kind}");
            let body = urgent_body(Some(kind), future());
            write_attention_json(&id, &body);
            assert_eq!(
                ack_hook_urgent_on_send(&id, COMMANDER_MSG),
                UrgentAck::Kept,
                "{kind}"
            );
            assert_eq!(
                ack_hook_urgent_on_send(&id, "<system-reminder>continue</system-reminder>"),
                UrgentAck::Kept,
                "{kind}"
            );
            assert!(read_hook_urgent(&id), "{kind} flag must survive");
            let dir = dir_guard::open_instance_dir(&id).unwrap();
            let raw =
                dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
                    .unwrap()
                    .unwrap();
            assert_eq!(raw, body.as_bytes(), "{kind}: file must be byte-identical");
        }
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_clears_sticky_kind_for_human_prompt() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("ack_sticky_human", &urgent_body(Some("cap"), future()));
        assert_eq!(
            ack_hook_urgent_on_send("ack_sticky_human", HUMAN_MSG),
            UrgentAck::Cleared
        );
        assert!(!read_hook_urgent("ack_sticky_human"));
        assert!(read_attention_json("ack_sticky_human")
            .get("urgent_kind")
            .is_none());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_clears_expired_sticky_kind_for_machine_traffic() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("ack_sticky_expired", &urgent_body(Some("mcp"), 1));
        assert_eq!(
            ack_hook_urgent_on_send("ack_sticky_expired", COMMANDER_MSG),
            UrgentAck::Cleared
        );
        assert!(read_attention_json("ack_sticky_expired")
            .get("urgent")
            .is_none());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_non_sticky_kind_clears_for_machine_traffic() {
        let (_g, _, _tmp) = BaseGuard::ready();
        // "throttle" self-heals on retry; it is deliberately NOT sticky.
        write_attention_json("ack_throttle", &urgent_body(Some("throttle"), future()));
        assert_eq!(
            ack_hook_urgent_on_send("ack_throttle", COMMANDER_MSG),
            UrgentAck::Cleared
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_is_absent_without_a_flag() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(
            ack_hook_urgent_on_send("ack_missing", HUMAN_MSG),
            UrgentAck::Absent
        );
        write_attention_json("ack_tier_only", r#"{"tier":2,"reason":"idle"}"#);
        assert_eq!(
            ack_hook_urgent_on_send("ack_tier_only", HUMAN_MSG),
            UrgentAck::Absent
        );
        assert_eq!(
            read_attention_json("ack_tier_only"),
            serde_json::json!({"tier":2,"reason":"idle"})
        );
        write_attention_json("ack_false", r#"{"tier":2,"urgent":false}"#);
        assert_eq!(
            ack_hook_urgent_on_send("ack_false", HUMAN_MSG),
            UrgentAck::Absent
        );
        write_attention_json("ack_garbage", "not json");
        assert_eq!(
            ack_hook_urgent_on_send("ack_garbage", HUMAN_MSG),
            UrgentAck::Absent
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ack_rejects_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(
            ack_hook_urgent_on_send("../etc", HUMAN_MSG),
            UrgentAck::Absent
        );
    }
}
