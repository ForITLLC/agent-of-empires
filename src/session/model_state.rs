//! Per-session MODEL state on every row (WO#1933, Ben-direct 2026-09-05
//! "nothing can be on OPUS"): what model the session is REALLY on, what it
//! was pinned to, and whether the two disagree.
//!
//! Claude Code 2.1.258 silently switches a session's model after a flagged
//! message ("automatically switched from claude-fable-5-1 …") and shows it
//! only on `/status` and `/model`; the launch flag, the record and the pane
//! footer all stay quiet. The Commander's census found a session that had
//! answered 286 turns on claude-opus-4-8 that way. The daemon's row is the
//! API-first answer: a consumer reads `live_model` / `model_pin` /
//! `model_drift` instead of driving `/status` in every pane.
//!
//! * `live_model` — the model of the LAST assistant turn in the session's
//!   Claude transcript (`message.model`; source `transcript`), or — when a
//!   `/model <arg>` local command was run AFTER that turn — that argument
//!   (source `transcript-command`: the next turn will answer on it). The
//!   transcript is the only surface the automatic switch writes to.
//!   `live_model_at` is the evidence's timestamp (unix seconds): the reading
//!   is as stale as the session's last turn, by construction — a session
//!   idle for a day reports the model of yesterday's last answer, and
//!   `model_drift` is judged on that. Read from the file's tail every daemon
//!   pass (60 s); absent for non-Claude tools and before the first turn.
//! * `model_pin` — the model the session is SUPPOSED to be on: the record's
//!   own model (`aoe session set-model`, source `record`), else the profile's
//!   `session.agent_extra_args.<tool>` `--model` flag (source `profile`).
//! * `model_drift` — both known and not the same model. An alias
//!   (`opus`, `fable`, …) matches any full id of that family; two full ids
//!   must match exactly (a `[1m]` suffix is ignored).
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many bytes of the transcript tail are read per pass. A turn's
/// assistant record is a few KB; the model field is on every one of them.
pub const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LiveModelSource {
    /// `message.model` of the last assistant record.
    Transcript,
    /// A `/model <arg>` local command recorded after the last assistant turn.
    TranscriptCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinSource {
    /// The session record's own model (`aoe session set-model`).
    Record,
    /// The profile's `session.agent_extra_args.<tool>` `--model` flag.
    Profile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveModel {
    pub model: String,
    pub source: LiveModelSource,
    /// Unix seconds of the transcript record that is the evidence.
    pub at: Option<u64>,
}

/// The flattened view on a session row (`/api/sessions`, `aoe list --json`,
/// `aoe status --json`). Every field is absent/false without evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_model_source: Option<LiveModelSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_model_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_pin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_pin_source: Option<PinSource>,
    /// True only when `live_model` and `model_pin` are both known and name
    /// different models.
    pub model_drift: bool,
}

/// Extract the value of a `--model` / `-m` flag from a whitespace-split
/// extra-args string (`session.agent_extra_args.<agent>`, a record's
/// `extra_args`). Handles the spaced form (`--model X`, `-m X`) and the
/// joined form (`--model=X`, `-m=X`). A dangling flag — no following value,
/// or the next token is another option (`--model --verbose`) — yields `None`
/// rather than a bogus model.
pub fn parse_model_flag(args: &str) -> Option<String> {
    let toks: Vec<&str> = args.split_whitespace().collect();
    for (i, tok) in toks.iter().enumerate() {
        if let Some(v) = tok
            .strip_prefix("--model=")
            .or_else(|| tok.strip_prefix("-m="))
        {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        } else if *tok == "--model" || *tok == "-m" {
            if let Some(v) = toks
                .get(i + 1)
                .filter(|v| !v.is_empty() && !v.starts_with('-'))
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The model a session is supposed to be on. The record's own model wins
/// (`set-model` writes both `agent_model` and a `--model` into the record's
/// `extra_args`; either counts), else the profile's flag for the tool.
pub fn model_pin(
    record_agent_model: Option<&str>,
    record_extra_args: &str,
    profile_extra_args: Option<&str>,
) -> Option<(String, PinSource)> {
    if let Some(m) = record_agent_model.map(str::trim).filter(|m| !m.is_empty()) {
        return Some((m.to_string(), PinSource::Record));
    }
    if let Some(m) = parse_model_flag(record_extra_args) {
        return Some((m, PinSource::Record));
    }
    profile_extra_args
        .and_then(parse_model_flag)
        .map(|m| (m, PinSource::Profile))
}

fn normalize(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// `opus` for `claude-opus-4-8`, `opus`, `opusplan`; `fable` for
/// `claude-fable-5-1`; the whole token for anything unfamiliar.
fn family(norm: &str) -> &str {
    let body = norm.strip_prefix("claude-").unwrap_or(norm);
    let head = body.split('-').next().unwrap_or(body);
    for f in ["opus", "sonnet", "haiku", "fable"] {
        if head.starts_with(f) {
            return f;
        }
    }
    head
}

fn is_alias(norm: &str) -> bool {
    !norm.starts_with("claude-")
}

/// Same model? Exact after normalisation; otherwise an alias on either side
/// matches by family (`opus` ≡ `claude-opus-4-8`). Two full ids of the same
/// family but different versions are NOT the same model.
pub fn same_model(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    if a == b {
        return true;
    }
    (is_alias(&a) || is_alias(&b)) && family(&a) == family(&b)
}

pub fn model_drift(live: Option<&str>, pin: Option<&str>) -> bool {
    match (live, pin) {
        (Some(l), Some(p)) => !same_model(l, p),
        _ => false,
    }
}

/// `<config_dir>/projects/<encoded cwd>/<session id>.jsonl` — where Claude
/// Code writes the session's transcript.
pub fn transcript_path(config_dir: &Path, project_path: &str, agent_session_id: &str) -> PathBuf {
    let canonical = crate::session::capture::canonicalize_or_raw(project_path);
    let dir_name =
        crate::session::capture::encode_claude_project_path(&canonical.to_string_lossy());
    config_dir
        .join("projects")
        .join(dir_name)
        .join(format!("{agent_session_id}.jsonl"))
}

fn record_secs(rec: &serde_json::Value) -> Option<u64> {
    let ts = rec.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp().max(0) as u64)
}

fn tag_value<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim())
}

/// The model evidence in one transcript record, if it carries any.
fn record_evidence(rec: &serde_json::Value) -> Option<LiveModel> {
    let at = record_secs(rec);
    match rec.get("type").and_then(|t| t.as_str())? {
        "assistant" => {
            let model = rec.get("message")?.get("model")?.as_str()?.trim();
            if model.is_empty() || model.starts_with('<') {
                return None; // "<synthetic>" placeholder turns carry no model
            }
            Some(LiveModel {
                model: model.to_string(),
                source: LiveModelSource::Transcript,
                at,
            })
        }
        "user" => {
            let content = rec.get("message")?.get("content")?.as_str()?;
            if tag_value(content, "command-name") != Some("/model") {
                return None;
            }
            let arg = tag_value(content, "command-args")?;
            if arg.is_empty() {
                return None; // bare `/model` opens the picker; nothing chosen here
            }
            Some(LiveModel {
                model: arg.to_string(),
                source: LiveModelSource::TranscriptCommand,
                at,
            })
        }
        _ => None,
    }
}

/// The LAST model evidence in a chunk of transcript lines. The file is
/// append-only, so file order is time order: the last record wins. A
/// leading partial line (a tail read that cut a record) is skipped.
pub fn scan_transcript_tail(chunk: &[u8], cut: bool) -> Option<LiveModel> {
    let text = String::from_utf8_lossy(chunk);
    let mut lines = text.lines();
    if cut {
        lines.next();
    }
    let mut last = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(ev) = record_evidence(&rec) {
            last = Some(ev);
        }
    }
    last
}

/// Read the transcript's tail and return its last model evidence.
pub fn read_live_model(path: &Path) -> Option<LiveModel> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    scan_transcript_tail(&buf, start > 0)
}

/// Assemble the row view.
pub fn session_model(pin: Option<(String, PinSource)>, live: Option<LiveModel>) -> SessionModel {
    let (model_pin, model_pin_source) = match pin {
        Some((m, s)) => (Some(m), Some(s)),
        None => (None, None),
    };
    let (live_model, live_model_source, live_model_at) = match live {
        Some(l) => (Some(l.model), Some(l.source), l.at),
        None => (None, None, None),
    };
    let model_drift = model_drift(live_model.as_deref(), model_pin.as_deref());
    SessionModel {
        live_model,
        live_model_source,
        live_model_at,
        model_pin,
        model_pin_source,
        model_drift,
    }
}

/// The model view of every Claude session in one profile, resolved LOCALLY
/// (the CLI path): pin from the record + profile config, live from the
/// transcript under the record's bound config dir (or the default).
pub fn local_session_models<'a>(
    instances: impl IntoIterator<Item = &'a crate::session::Instance>,
    profile: &str,
) -> std::collections::HashMap<String, SessionModel> {
    let config = crate::session::config::profile_config::resolve_config_or_warn(profile);
    let mut out = std::collections::HashMap::new();
    for inst in instances {
        if !crate::session::account::tool_has_config_dir(&inst.tool) {
            continue;
        }
        let profile_args = config
            .session
            .agent_extra_args
            .get(&inst.tool)
            .map(String::as_str);
        let pin = model_pin(inst.agent_model.as_deref(), &inst.extra_args, profile_args);
        let config_dir = crate::session::account::record_config_dir(profile, &inst.tool)
            .unwrap_or_else(crate::session::account::default_config_dir);
        let live = inst
            .agent_session_id
            .as_deref()
            .map(|sid| transcript_path(&config_dir, &inst.project_path, sid))
            .and_then(|p| read_live_model(&p));
        out.insert(inst.id.clone(), session_model(pin, live));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // VERBATIM shapes from the WO#1933 induced test (throwaway 49ea251d,
    // 2026-09-05) and the for-Common census (be576c43): an assistant record
    // carries `message.model`; a `/model X` local command is a user record
    // whose content holds `<command-name>/model</command-name>` and
    // `<command-args>X</command-args>`.
    fn assistant(ts: &str, model: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","model":"{model}","content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
    }

    fn model_command(ts: &str, arg: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args>{arg}</command-args>"}}}}"#
        )
    }

    fn lines(v: &[String]) -> Vec<u8> {
        let mut s = v.join("\n");
        s.push('\n');
        s.into_bytes()
    }

    #[test]
    fn last_assistant_turn_is_the_live_model() {
        let chunk = lines(&[
            assistant("2026-09-03T08:20:00.000Z", "claude-fable-5-1"),
            r#"{"type":"user","timestamp":"2026-09-03T08:25:00.000Z","message":{"role":"user","content":"carry on"}}"#.to_string(),
            assistant("2026-09-03T08:25:47.000Z", "claude-opus-4-8"),
            r#"{"type":"progress","timestamp":"2026-09-03T08:26:00.000Z"}"#.to_string(),
        ]);
        let live = scan_transcript_tail(&chunk, false).expect("evidence");
        assert_eq!(live.model, "claude-opus-4-8");
        assert_eq!(live.source, LiveModelSource::Transcript);
        assert_eq!(live.at, Some(1_788_423_947));
    }

    #[test]
    fn a_later_model_command_overrides_the_last_turn() {
        let chunk = lines(&[
            assistant("2026-09-05T05:17:00.000Z", "claude-opus-5"),
            model_command("2026-09-05T05:21:00.513Z", "claude-fable-5-1"),
        ]);
        let live = scan_transcript_tail(&chunk, false).unwrap();
        assert_eq!(live.model, "claude-fable-5-1");
        assert_eq!(live.source, LiveModelSource::TranscriptCommand);
        assert_eq!(live.at, Some(1_788_585_660));
    }

    #[test]
    fn a_turn_after_the_command_wins_again() {
        let chunk = lines(&[
            model_command("2026-09-05T05:16:55.550Z", "opus"),
            assistant("2026-09-05T05:18:00.000Z", "claude-opus-5"),
        ]);
        let live = scan_transcript_tail(&chunk, false).unwrap();
        assert_eq!(live.model, "claude-opus-5");
        assert_eq!(live.source, LiveModelSource::Transcript);
    }

    #[test]
    fn bare_model_command_and_synthetic_turns_are_not_evidence() {
        let chunk = lines(&[
            assistant("2026-09-05T05:17:00.000Z", "claude-fable-5-1"),
            model_command("2026-09-05T05:18:00.000Z", ""),
            assistant("2026-09-05T05:19:00.000Z", "<synthetic>"),
        ]);
        let live = scan_transcript_tail(&chunk, false).unwrap();
        assert_eq!(live.model, "claude-fable-5-1");
    }

    #[test]
    fn a_cut_leading_line_is_skipped_and_garbage_is_ignored() {
        let mut chunk = b"ge\":{\"model\":\"claude-opus-4-8\"}}\n".to_vec();
        chunk.extend(lines(&[
            "not json".to_string(),
            assistant("2026-09-05T05:17:00.000Z", "claude-fable-5-1"),
        ]));
        assert_eq!(
            scan_transcript_tail(&chunk, true).unwrap().model,
            "claude-fable-5-1"
        );
        assert!(scan_transcript_tail(b"", false).is_none());
    }

    #[test]
    fn read_live_model_reads_only_the_tail_of_a_big_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut body = Vec::new();
        body.extend(lines(&[assistant(
            "2026-09-05T05:00:00.000Z",
            "claude-opus-4-8",
        )]));
        // pad past the tail window with records that carry no model
        let filler = r#"{"type":"progress","timestamp":"2026-09-05T05:01:00.000Z","data":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#;
        while (body.len() as u64) < TRANSCRIPT_TAIL_BYTES + 4096 {
            body.extend(filler.as_bytes());
            body.push(b'\n');
        }
        body.extend(lines(&[assistant(
            "2026-09-05T05:30:00.000Z",
            "claude-fable-5-1",
        )]));
        std::fs::write(&path, &body).unwrap();
        let live = read_live_model(&path).unwrap();
        assert_eq!(live.model, "claude-fable-5-1");
        assert!(read_live_model(&dir.path().join("missing.jsonl")).is_none());
    }

    #[test]
    fn parse_model_flag_extracts_the_pinned_model() {
        assert_eq!(
            parse_model_flag("--model claude-fable-5-1"),
            Some("claude-fable-5-1".into())
        );
        assert_eq!(
            parse_model_flag("--verbose --model=opus"),
            Some("opus".into())
        );
        assert_eq!(parse_model_flag("-m sonnet"), Some("sonnet".into()));
        assert_eq!(parse_model_flag("--model --verbose"), None);
        assert_eq!(parse_model_flag("--model"), None);
        assert_eq!(parse_model_flag(""), None);
    }

    #[test]
    fn model_pin_prefers_the_record_over_the_profile() {
        assert_eq!(
            model_pin(
                Some("opus"),
                "--model opus",
                Some("--model claude-fable-5-1")
            ),
            Some(("opus".into(), PinSource::Record))
        );
        assert_eq!(
            model_pin(None, "--model opus", Some("--model claude-fable-5-1")),
            Some(("opus".into(), PinSource::Record))
        );
        assert_eq!(
            model_pin(None, "", Some("--model claude-fable-5-1")),
            Some(("claude-fable-5-1".into(), PinSource::Profile))
        );
        assert_eq!(model_pin(None, "", None), None);
        assert_eq!(model_pin(Some("  "), "--verbose", Some("--verbose")), None);
    }

    #[test]
    fn same_model_matches_aliases_by_family_and_full_ids_exactly() {
        assert!(same_model("claude-fable-5-1", "claude-fable-5-1"));
        assert!(same_model("claude-fable-5-1", "claude-fable-5-1[1m]"));
        assert!(same_model("fable", "claude-fable-5-1"));
        assert!(same_model("claude-opus-5", "opus"));
        assert!(same_model("opusplan", "claude-opus-4-8"));
        assert!(!same_model("claude-opus-4-8", "claude-fable-5-1"));
        assert!(!same_model("claude-fable-5", "claude-fable-5-1"));
        assert!(!same_model("sonnet", "claude-opus-5"));
    }

    #[test]
    fn drift_needs_both_sides() {
        assert!(model_drift(
            Some("claude-opus-4-8"),
            Some("claude-fable-5-1")
        ));
        assert!(!model_drift(
            Some("claude-fable-5-1"),
            Some("claude-fable-5-1")
        ));
        assert!(!model_drift(None, Some("claude-fable-5-1")));
        assert!(!model_drift(Some("claude-opus-4-8"), None));
    }

    #[test]
    fn session_model_flattens_to_the_wire_shape() {
        let row = session_model(
            Some(("claude-fable-5-1".into(), PinSource::Profile)),
            Some(LiveModel {
                model: "claude-opus-4-8".into(),
                source: LiveModelSource::Transcript,
                at: Some(1_788_423_947),
            }),
        );
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["live_model"], "claude-opus-4-8");
        assert_eq!(v["live_model_source"], "transcript");
        assert_eq!(v["live_model_at"], 1_788_423_947);
        assert_eq!(v["model_pin"], "claude-fable-5-1");
        assert_eq!(v["model_pin_source"], "profile");
        assert_eq!(v["model_drift"], true);

        let empty = serde_json::to_value(session_model(None, None)).unwrap();
        assert_eq!(empty, serde_json::json!({"model_drift": false}));
    }

    #[test]
    fn transcript_path_uses_claude_code_encoding() {
        let p = transcript_path(
            Path::new("/home/x/.claude-accounts/forit-main"),
            "/home/x/GitProjects/for-Common",
            "be576c43-b742-4741-a3a9-28f7dccf230b",
        );
        assert_eq!(
            p,
            PathBuf::from("/home/x/.claude-accounts/forit-main/projects/-home-x-GitProjects-for-Common/be576c43-b742-4741-a3a9-28f7dccf230b.jsonl")
        );
    }
}
