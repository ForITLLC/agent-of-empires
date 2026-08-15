//! Instant AOE message capture with regex subscriptions (WO#1393 D1).
//!
//! Callers register regex subscriptions (`POST /api/subscriptions`); the tap
//! watches each live session's Claude transcript jsonl through the shared
//! kernel [`FileWatchService`] (zero polling) and, the moment a new transcript
//! line matches a subscription, emits a `regex_match` event onto the daemon
//! event bus, where it fans out over the existing SSE/push surfaces.
//!
//! The watchdog tick calls [`reconcile`] to keep the watched set aligned with
//! the live session list; everything between ticks is kernel-event driven.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};

use serde::{Deserialize, Serialize};

use crate::file_watch::{FileEventKind, FileMatcher, SubscriptionHandle, WatchSpec};

use super::AppState;

/// One regex subscription. `session` restricts matching to a single aoe
/// session id; `None` matches every live session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSub {
    pub id: String,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub created_at: u64,
}

struct TapWatch {
    path: PathBuf,
    /// Dropping the handle unsubscribes from the file watch, which closes the
    /// reader task's channel and ends it.
    _handle: SubscriptionHandle,
}

/// Subscription registry + the set of transcripts currently tapped.
#[derive(Default)]
pub struct MessageTap {
    subs: RwLock<Vec<MessageSub>>,
    compiled: RwLock<HashMap<String, regex::Regex>>,
    watched: Mutex<HashMap<String, TapWatch>>,
}

/// Where subscriptions persist across daemon restarts.
pub fn subs_path() -> Option<PathBuf> {
    match std::env::var("AOE_MESSAGE_SUBS_FILE") {
        Ok(p) => Some(PathBuf::from(p)),
        Err(_) => match crate::session::get_app_dir() {
            Ok(dir) => Some(dir.join("message_subs.json")),
            Err(e) => {
                tracing::warn!(target: "server.message_tap", error = %e, "no app dir; message subscriptions unavailable");
                None
            }
        },
    }
}

impl MessageTap {
    /// Load persisted subscriptions; a sub whose pattern no longer compiles
    /// is dropped with a warning rather than poisoning the registry.
    pub fn load() -> Self {
        let tap = Self::default();
        let Some(path) = subs_path() else {
            return tap;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return tap;
        };
        let subs: Vec<MessageSub> = serde_json::from_str(&raw).unwrap_or_default();
        let mut compiled = HashMap::new();
        let mut kept = Vec::new();
        for sub in subs {
            match regex::Regex::new(&sub.pattern) {
                Ok(re) => {
                    compiled.insert(sub.id.clone(), re);
                    kept.push(sub);
                }
                Err(e) => tracing::warn!(
                    target: "server.message_tap",
                    id = %sub.id, pattern = %sub.pattern, error = %e,
                    "dropping persisted subscription with invalid pattern"
                ),
            }
        }
        *tap.subs.write().expect("subs lock") = kept;
        *tap.compiled.write().expect("compiled lock") = compiled;
        tap
    }

    fn persist(&self) {
        let Some(path) = subs_path() else { return };
        let subs = self.subs.read().expect("subs lock").clone();
        match serde_json::to_string_pretty(&subs) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(target: "server.message_tap", error = %e, "failed to persist message subscriptions");
                }
            }
            Err(e) => {
                tracing::warn!(target: "server.message_tap", error = %e, "failed to serialize message subscriptions")
            }
        }
    }

    /// Register a subscription. Errors on an invalid regex.
    pub fn add(
        &self,
        pattern: &str,
        label: Option<String>,
        session: Option<String>,
        now_secs: u64,
    ) -> Result<MessageSub, String> {
        let re = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (pattern, &label, &session, now_secs).hash(&mut h);
        let sub = MessageSub {
            id: format!("{:08x}", h.finish() as u32),
            pattern: pattern.to_string(),
            label,
            session,
            created_at: now_secs,
        };
        self.compiled
            .write()
            .expect("compiled lock")
            .insert(sub.id.clone(), re);
        self.subs.write().expect("subs lock").push(sub.clone());
        self.persist();
        Ok(sub)
    }

    /// Remove a subscription by id; true when one was removed.
    pub fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut subs = self.subs.write().expect("subs lock");
            let before = subs.len();
            subs.retain(|s| s.id != id);
            subs.len() != before
        };
        if removed {
            self.compiled.write().expect("compiled lock").remove(id);
            self.persist();
        }
        removed
    }

    pub fn list(&self) -> Vec<MessageSub> {
        self.subs.read().expect("subs lock").clone()
    }

    /// True when at least one subscription could match `sid` (drives whether
    /// the session's transcript is worth tapping at all).
    fn any_sub_for(&self, sid: &str) -> bool {
        self.subs
            .read()
            .expect("subs lock")
            .iter()
            .any(|s| s.session.as_deref().is_none_or(|want| want == sid))
    }

    /// Match `text` (one transcript line's collected text) against every
    /// subscription applicable to `sid`; returns `(sub, excerpt)` pairs.
    pub fn matches(&self, sid: &str, text: &str) -> Vec<(MessageSub, String)> {
        let subs = self.subs.read().expect("subs lock");
        let compiled = self.compiled.read().expect("compiled lock");
        let mut hits = Vec::new();
        for sub in subs.iter() {
            if sub.session.as_deref().is_some_and(|want| want != sid) {
                continue;
            }
            let Some(re) = compiled.get(&sub.id) else {
                continue;
            };
            if let Some(m) = re.find(text) {
                hits.push((sub.clone(), excerpt_around(text, m.start(), m.end())));
            }
        }
        hits
    }
}

/// A short, single-line window around the match for the event detail.
fn excerpt_around(text: &str, start: usize, end: usize) -> String {
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[end..]
        .find('\n')
        .map(|i| end + i)
        .unwrap_or(text.len());
    let mut lo = line_start.max(start.saturating_sub(80));
    let mut hi = line_end.min(end + 80);
    while !text.is_char_boundary(lo) {
        lo -= 1;
    }
    while !text.is_char_boundary(hi) {
        hi += 1;
    }
    text[lo..hi].trim().to_string()
}

/// Recursively collect every `"text"` string plus string-typed `content`
/// values from one transcript json line, so a probe echoed via a tool result
/// (nested `tool_result` blocks) matches just like a plain assistant message.
fn collect_text(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                match (k.as_str(), val) {
                    ("text" | "content", serde_json::Value::String(s)) => {
                        out.push_str(s);
                        out.push('\n');
                    }
                    _ => collect_text(val, out),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
}

/// Align the watched-transcript set with the live session list. Called from
/// the watchdog tick; between calls, everything is kernel-event driven.
pub async fn reconcile(state: &Arc<AppState>) {
    let tap = Arc::clone(&state.message_tap);
    // (aoe id, title, profile, transcript path) for every live claude session
    // that some subscription could match.
    let mut want: HashMap<String, (String, String, PathBuf)> = HashMap::new();
    {
        let instances = state.instances.read().await;
        for inst in instances.iter() {
            if !tap.any_sub_for(&inst.id) {
                continue;
            }
            let Some(sid) = inst.agent_session_id.as_deref() else {
                continue;
            };
            let Some(path) =
                crate::session::capture::claude_transcript_path(&inst.project_path, sid)
            else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            want.insert(
                inst.id.clone(),
                (inst.title.clone(), inst.source_profile.clone(), path),
            );
        }
    }
    let mut watched = tap.watched.lock().expect("watched lock");
    watched.retain(|id, w| want.get(id).is_some_and(|(_, _, path)| *path == w.path));
    for (id, (title, profile, path)) in want {
        if watched.contains_key(&id) {
            continue;
        }
        let Some(dir) = path.parent().map(PathBuf::from) else {
            continue;
        };
        let spec = WatchSpec {
            dir,
            matcher: FileMatcher::Exact(path.clone()),
            debounce: None,
        };
        let (rx, handle) = match state.file_watch.subscribe_channel(spec, 64) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    target: "server.message_tap",
                    session = %id, error = %e,
                    "failed to watch transcript; will retry next reconcile"
                );
                continue;
            }
        };
        // Start at EOF: subscriptions see messages from registration forward.
        let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        watched.insert(
            id.clone(),
            TapWatch {
                path: path.clone(),
                _handle: handle,
            },
        );
        tokio::spawn(run_tap(
            Arc::downgrade(state),
            id,
            title,
            profile,
            path,
            offset,
            rx,
        ));
    }
}

/// Per-transcript reader: waits on kernel events, reads the newly appended
/// complete lines, and emits a `regex_match` event per subscription hit.
async fn run_tap(
    state: Weak<AppState>,
    session_id: String,
    title: String,
    profile: String,
    path: PathBuf,
    mut offset: u64,
    mut rx: tokio::sync::mpsc::Receiver<crate::file_watch::FileEvent>,
) {
    while let Some(ev) = rx.recv().await {
        if ev.kind != FileEventKind::Upserted {
            continue;
        }
        let Some(state) = state.upgrade() else { return };
        let lines = match read_new_lines(&path, &mut offset) {
            Ok(lines) => lines,
            Err(e) => {
                tracing::debug!(
                    target: "server.message_tap",
                    session = %session_id, error = %e,
                    "transcript read failed"
                );
                continue;
            }
        };
        for line in lines {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let mut text = String::new();
            collect_text(&v, &mut text);
            if text.is_empty() {
                continue;
            }
            for (sub, excerpt) in state.message_tap.matches(&session_id, &text) {
                let name = sub.label.as_deref().unwrap_or(&sub.pattern);
                super::event_bus::emit_and_fan_out(
                    &state,
                    "regex_match",
                    &session_id,
                    &title,
                    &profile,
                    &format!("sub {} ({name}) matched: {excerpt}", sub.id),
                );
            }
        }
    }
}

/// Read complete lines appended since `offset`, advancing it only past the
/// last newline so a partially-flushed line is picked up whole next event.
fn read_new_lines(path: &PathBuf, offset: &mut u64) -> std::io::Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len < *offset {
        // Truncated/rewritten (transcript relink): restart from the top.
        *offset = 0;
    }
    if len == *offset {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::with_capacity((len - *offset) as usize);
    f.read_to_end(&mut buf)?;
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(Vec::new());
    };
    *offset += (last_nl + 1) as u64;
    let complete = &buf[..last_nl];
    Ok(String::from_utf8_lossy(complete)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_lifecycle_add_match_remove() {
        let tap = MessageTap::default();
        // Invalid pattern is rejected.
        assert!(tap.add("(unclosed", None, None, 100).is_err());
        let sub = tap
            .add("WO1393-PROBE-[0-9a-f]+", Some("probe".into()), None, 100)
            .unwrap();
        // Session-scoped sub only matches its session.
        let scoped = tap
            .add("deploy done", None, Some("abc123".into()), 101)
            .unwrap();
        let hits = tap.matches("abc123", "xx WO1393-PROBE-deadbeef yy deploy done zz");
        assert_eq!(hits.len(), 2, "both subs match the scoped session");
        let hits = tap.matches("other", "xx WO1393-PROBE-deadbeef yy deploy done zz");
        assert_eq!(hits.len(), 1, "scoped sub excluded on other session");
        assert_eq!(hits[0].0.id, sub.id);
        assert!(hits[0].1.contains("WO1393-PROBE-deadbeef"));
        assert!(tap.any_sub_for("anything"));
        assert!(tap.remove(&sub.id));
        assert!(!tap.remove(&sub.id));
        assert!(tap.any_sub_for("abc123"));
        assert!(!tap.any_sub_for("anything-else"), "only scoped sub remains");
        assert!(tap.remove(&scoped.id));
        assert!(tap.list().is_empty());
    }

    #[test]
    fn collect_text_reaches_nested_tool_results() {
        let cases: [(&str, &str); 3] = [
            // Plain assistant text block.
            (
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello probe"}]}}"#,
                "hello probe",
            ),
            // String-typed content (user prompt shape).
            (
                r#"{"type":"user","message":{"content":"raw string body"}}"#,
                "raw string body",
            ),
            // Nested tool_result text (a Bash echo landing in the transcript).
            (
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"WO1393-PROBE-cafe"}]}]}}"#,
                "WO1393-PROBE-cafe",
            ),
        ];
        for (raw, want) in cases {
            let v: serde_json::Value = serde_json::from_str(raw).unwrap();
            let mut out = String::new();
            collect_text(&v, &mut out);
            assert!(out.contains(want), "{raw}");
        }
    }

    #[test]
    fn read_new_lines_offset_and_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "one\ntwo\npar").unwrap();
        let mut offset = 0u64;
        let lines = read_new_lines(&path, &mut offset).unwrap();
        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
        // Partial line not consumed; completing it yields exactly that line.
        std::fs::write(&path, "one\ntwo\npartial done\n").unwrap();
        let lines = read_new_lines(&path, &mut offset).unwrap();
        assert_eq!(lines, vec!["partial done".to_string()]);
        // Truncation resets to the top.
        std::fs::write(&path, "fresh\n").unwrap();
        let lines = read_new_lines(&path, &mut offset).unwrap();
        assert_eq!(lines, vec!["fresh".to_string()]);
    }
}
