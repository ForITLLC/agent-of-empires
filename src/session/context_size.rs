//! Authoritative per-session context size, read from the session's own
//! Claude Code transcript.
//!
//! Per-dev WO#1280 D5: the daemon exposed no context size, so fleet callers
//! hand-rolled transcript scanners and got it wrong twice in one night. The
//! two failure modes this module exists to make unrepeatable:
//!   1. keying the transcript dir off a session's *workdir* instead of its
//!      registered `project_path` (resolved zero sessions), and
//!   2. selecting records by recency alone, which picks up `<synthetic>`
//!      model rows, sidechain (subagent) rows, and zero-usage rows
//!      (produced five false zeros).
//!
//! The size reported is the total input-side token count of the LAST real
//! assistant turn: `input_tokens + cache_read_input_tokens +
//! cache_creation_input_tokens`, from the newest transcript line whose
//! message has a non-synthetic model, non-zero usage, and is not a
//! sidechain. That is what the agent itself was billed for the turn, so it
//! is the number an autocompact trigger compares against.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::capture::encode_claude_project_path;

/// Only the newest `TAIL_CAP` bytes of a transcript are scanned. A real
/// assistant turn recurs every few lines, but individual lines (tool
/// results) can run to hundreds of KB, so the bound is generous. A
/// transcript whose last real turn sits deeper than this reports nothing
/// rather than something stale.
const TAIL_CAP: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextSize {
    pub tokens: u64,
    /// Transcript timestamp of the measured turn, verbatim (RFC3339).
    pub at: Option<String>,
    pub model: String,
    pub transcript: PathBuf,
}

/// Resolve and read the context size for one session. `host_env` is the
/// session's profile environment list (`KEY=VALUE`), consulted for a
/// `CLAUDE_CONFIG_DIR` override the same way agent launch does; without one
/// the store is `~/.claude/projects`.
pub(crate) fn context_size_for(
    host_env: &[String],
    project_path: &str,
    sid: &str,
) -> Option<ContextSize> {
    let root = projects_root(host_env)?;
    let transcript = locate_transcript(&root, project_path, sid)?;
    let (tokens, at, model) = read_last_real_turn(&transcript, TAIL_CAP)?;
    Some(ContextSize {
        tokens,
        at,
        model,
        transcript,
    })
}

fn projects_root(host_env: &[String]) -> Option<PathBuf> {
    let config_dir =
        super::environment::resolve_host_environment_value(host_env, "CLAUDE_CONFIG_DIR")
            .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    Some(config_dir.join("projects"))
}

/// The transcript lives at `<root>/<encoded project_path>/<sid>.jsonl`. When
/// the encoded dir misses (workdir renamed after launch, the #132 stranding
/// class), fall back to the sid anywhere under the store: sids are UUIDs, so
/// any hit is this session's file. Read-only sibling of
/// `capture::relink_stranded_transcript`.
fn locate_transcript(root: &Path, project_path: &str, sid: &str) -> Option<PathBuf> {
    let file_name = format!("{sid}.jsonl");
    let exact = root
        .join(encode_claude_project_path(project_path))
        .join(&file_name);
    if exact.is_file() {
        return Some(exact);
    }
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(root).ok()? {
        let Ok(entry) = entry else { continue };
        let candidate = entry.path().join(&file_name);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best
            .as_ref()
            .is_none_or(|(_, best_mtime)| mtime > *best_mtime)
        {
            best = Some((candidate, mtime));
        }
    }
    best.map(|(p, _)| p)
}

/// Scan the newest `cap` bytes for the last real assistant turn. Returns
/// `(tokens, timestamp, model)`.
fn read_last_real_turn(path: &Path, cap: u64) -> Option<(u64, Option<String>, String)> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(cap);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line is almost certainly cut mid-record by the seek;
        // parsing a truncated JSON line fails harmlessly, but drop it so a
        // pathological truncation cannot half-parse into wrong numbers.
        lines.remove(0);
    }
    lines.iter().rev().find_map(|line| parse_real_turn(line))
}

fn parse_real_turn(line: &str) -> Option<(u64, Option<String>, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("isSidechain").and_then(|s| s.as_bool()) == Some(true) {
        return None;
    }
    let msg = v.get("message")?;
    let model = msg.get("model")?.as_str()?;
    if model == "<synthetic>" {
        return None;
    }
    let usage = msg.get("usage")?;
    let tokens: u64 = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .filter_map(|k| usage.get(*k).and_then(|t| t.as_u64()))
    .sum();
    if tokens == 0 {
        return None;
    }
    let at = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    Some((tokens, at, model.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn turn(model: &str, tokens: u64, ts: &str, sidechain: bool) -> String {
        format!(
            concat!(
                r#"{{"isSidechain":{sidechain},"timestamp":"{ts}","message":"#,
                r#"{{"model":"{model}","usage":{{"input_tokens":{tokens},"#,
                r#""cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
            ),
            sidechain = sidechain,
            ts = ts,
            model = model,
            tokens = tokens,
        )
    }

    #[test]
    fn last_real_turn_skips_synthetic_sidechain_and_zero_usage() {
        // The exact record mix that produced the false zeros: the real turn is
        // OLDER than a synthetic row, a sidechain row, a zero-usage row, and a
        // non-message row, and must still win.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let lines = [
            turn("claude-fable-5", 100, "2026-08-06T01:00:00Z", false),
            turn("claude-fable-5", 193_859, "2026-08-06T02:00:00Z", false),
            turn("<synthetic>", 5, "2026-08-06T03:00:00Z", false),
            turn("claude-fable-5", 999_999, "2026-08-06T04:00:00Z", true),
            turn("claude-fable-5", 0, "2026-08-06T05:00:00Z", false),
            r#"{"type":"summary","summary":"compact"}"#.to_string(),
        ];
        fs::write(&p, lines.join("\n")).unwrap();
        let (tokens, at, model) = read_last_real_turn(&p, TAIL_CAP).unwrap();
        assert_eq!(tokens, 193_859);
        assert_eq!(at.as_deref(), Some("2026-08-06T02:00:00Z"));
        assert_eq!(model, "claude-fable-5");
    }

    #[test]
    fn tail_cap_bounds_the_scan_and_drops_the_cut_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let real = turn("claude-fable-5", 42, "2026-08-06T01:00:00Z", false);
        // Padding record long enough that a small cap seeks into the middle
        // of the real turn's line; the cut line must be dropped, not
        // half-parsed.
        let pad = format!(r#"{{"pad":"{}"}}"#, "x".repeat(512));
        fs::write(&p, format!("{real}\n{pad}")).unwrap();
        assert_eq!(read_last_real_turn(&p, 400), None, "real turn beyond cap");
        assert!(
            read_last_real_turn(&p, 10_000).is_some(),
            "generous cap finds it"
        );
    }

    #[test]
    fn locate_transcript_exact_then_stranded_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sid = "0f0e0d0c-1111-2222-3333-444455556666";
        // Stranded under an old encoded dir only -> fallback finds it.
        let old = root.join("-Users-foo-oldname");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(format!("{sid}.jsonl")), "x").unwrap();
        let found = locate_transcript(root, "/Users/foo/newname", sid).unwrap();
        assert_eq!(found, old.join(format!("{sid}.jsonl")));
        // Exact encoded dir wins once it exists.
        let new = root.join("-Users-foo-newname");
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join(format!("{sid}.jsonl")), "x").unwrap();
        let found = locate_transcript(root, "/Users/foo/newname", sid).unwrap();
        assert_eq!(found, new.join(format!("{sid}.jsonl")));
        // Absent everywhere -> None.
        assert_eq!(
            locate_transcript(root, "/Users/foo/newname", "no-such-sid"),
            None
        );
    }

    #[test]
    fn projects_root_prefers_session_env_override() {
        let env = vec!["CLAUDE_CONFIG_DIR=/tmp/acct".to_string()];
        assert_eq!(
            projects_root(&env),
            Some(PathBuf::from("/tmp/acct/projects"))
        );
    }
}
