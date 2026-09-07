//! CLI command implementations

pub mod acp;
pub mod add;
pub mod agents;
pub mod cityhall;
pub mod definition;
pub mod extract_session_id;
pub mod graft;
pub mod group;
pub mod init;
pub mod killall;
pub mod list;
pub mod log_level;
pub mod logs;
pub mod mcp;
pub mod migrate;
pub mod output;
pub mod plugin;
pub mod profile;
pub mod project;
pub mod ps;
pub mod relay;
pub mod remove;
pub mod sandbox;
pub mod send;
pub mod serve;
pub mod session;
pub mod settings;
pub mod skill;
pub mod sounds;
pub mod status;
pub mod telemetry;
pub mod theme;
pub mod tmux;
pub mod uninstall;
pub mod update;
pub mod url;
pub mod worktree;

pub use definition::{command_name, Cli, Commands, CLI_COMMAND_NAMES};

/// Whether CLI stdout should contain ANSI color. Color is terminal-only and
/// follows the NO_COLOR convention (only a non-empty value disables it).
pub(crate) fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// One rendered lifecycle-notice line for CLI listings. Amber on a color-
/// capable terminal; plain text otherwise, so pipes and CI logs stay clean.
pub(crate) fn lifecycle_notice_line(indent: &str, notice: &str) -> String {
    if color_enabled() {
        format!("{indent}\x1b[33m⚠ {notice}\x1b[0m")
    } else {
        format!("{indent}⚠ {notice}")
    }
}

use crate::session::Instance;
use anyhow::{bail, Result};

pub fn resolve_session<'a>(identifier: &str, instances: &'a [Instance]) -> Result<&'a Instance> {
    // Try exact ID match. Exact matches always win over prefix matches and
    // can never be ambiguous (IDs are unique).
    if let Some(inst) = instances.iter().find(|i| i.id == identifier) {
        return Ok(inst);
    }

    // Try ID prefix match. If more than one session has an ID starting with
    // `identifier`, fail loudly instead of silently mutating the first one.
    // Mutating commands (archive, kill, snooze) could otherwise act on the
    // wrong session when the user provides a too-short prefix.
    let prefix_matches: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.id.starts_with(identifier))
        .collect();
    match prefix_matches.len() {
        0 => {}
        1 => return Ok(prefix_matches[0]),
        _ => {
            let mut candidates: Vec<String> = prefix_matches
                .iter()
                .map(|i| format!("  {} ({})", i.id, i.title))
                .collect();
            candidates.sort();
            bail!(
                "Ambiguous session identifier {:?} matches {} sessions:\n{}\nUse a longer prefix or the full ID.",
                identifier,
                prefix_matches.len(),
                candidates.join("\n")
            );
        }
    }

    // Try exact title match. A live session wins over a trashed twin with
    // the same title: storage order is not a tie-break anyone intends, and
    // a trashed row is only the answer when nothing live carries the name.
    if let Some(inst) = prefer_live(instances.iter().filter(|i| i.title == identifier)) {
        return Ok(inst);
    }

    // Try path match, same live-first rule.
    if let Some(inst) = prefer_live(instances.iter().filter(|i| i.project_path == identifier)) {
        return Ok(inst);
    }

    bail!("Session not found: {}", identifier)
}

/// First live (untrashed) match, else the first match of any kind.
fn prefer_live<'a>(
    mut matches: impl Iterator<Item = &'a Instance> + Clone,
) -> Option<&'a Instance> {
    matches
        .clone()
        .find(|i| !i.is_trashed())
        .or_else(|| matches.next())
}

/// Whether the invocation named a profile (`-p`/`--profile`, or the
/// `AGENT_OF_EMPIRES_PROFILE` environment variable clap reads for it). `main`
/// records it once before any subcommand runs; the profile-scoped id verbs use
/// it to choose between the one-profile lookup (`resolve_session`) and the
/// every-profile census (`resolve_scope`). Unset — library callers, tests —
/// reads as explicit, so the per-profile behaviour is the default.
static PROFILE_EXPLICIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub fn set_profile_explicit(explicit: bool) {
    let _ = PROFILE_EXPLICIT.set(explicit);
}

pub fn profile_explicit() -> bool {
    PROFILE_EXPLICIT.get().copied().unwrap_or(true)
}

/// Where a profile-scoped id verb runs: the profile that owns the session and
/// the session's full id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub profile: String,
    pub identifier: String,
}

/// Resolve the target of a profile-scoped id verb (`send`, `rm`, `session
/// stop|restart|snooze|…`).
///
/// With an explicit profile nothing changes: the verb runs in that profile and
/// `resolve_session` there still accepts a full id, a unique id prefix, a
/// title or a project path. Without one, `identifier` is looked up across
/// EVERY profile's session registry (the census `session move` walks), and
/// only two shapes count: a full session id, or a title exactly one session in
/// the whole census carries. Everything else fails closed: an id registered in
/// two profiles or a title two sessions share lists the candidates and errors
/// without acting; an id prefix, a project path or an unknown name is
/// "Session not found". Nothing is ever picked by proximity.
pub fn resolve_scope(profile: &str, identifier: &str) -> Result<Scope> {
    resolve_scope_with(profile, profile_explicit(), identifier)
}

pub fn resolve_scope_with(profile: &str, explicit: bool, identifier: &str) -> Result<Scope> {
    if explicit {
        return Ok(Scope {
            profile: profile.to_string(),
            identifier: identifier.to_string(),
        });
    }
    let profiles = crate::session::list_profiles()?;
    resolve_scope_in(identifier, &profiles)
}

/// The census half of `resolve_scope`, over an explicit profile list so tests
/// need no process-global state.
pub fn resolve_scope_in(identifier: &str, profiles: &[String]) -> Result<Scope> {
    let mut by_id: Vec<(String, Instance)> = Vec::new();
    let mut by_title: Vec<(String, Instance)> = Vec::new();
    for p in profiles {
        let instances = match crate::session::Storage::new_unwatched(p).and_then(|s| s.load()) {
            Ok(v) => v,
            Err(err) => {
                // A registry that will not load cannot vote; say so rather
                // than silently narrowing the census.
                eprintln!("warning: profile '{p}' skipped: {err}");
                continue;
            }
        };
        for inst in instances {
            if inst.id == identifier {
                by_id.push((p.clone(), inst));
            } else if inst.title == identifier {
                by_title.push((p.clone(), inst));
            }
        }
    }

    let describe = |hits: &[(String, Instance)]| -> String {
        let mut lines: Vec<String> = hits
            .iter()
            .map(|(p, i)| format!("  {}: {} ({})", p, i.id, i.title))
            .collect();
        lines.sort();
        lines.join("\n")
    };

    match by_id.len() {
        1 => {
            let (profile, inst) = by_id.remove(0);
            return Ok(Scope {
                profile,
                identifier: inst.id,
            });
        }
        0 => {}
        n => bail!(
            "Ambiguous session id {:?}: registered in {} profiles:\n{}\nPass -p <profile> to choose one. No action taken.",
            identifier,
            n,
            describe(&by_id)
        ),
    }

    match by_title.len() {
        1 => {
            let (profile, inst) = by_title.remove(0);
            Ok(Scope {
                profile,
                identifier: inst.id,
            })
        }
        0 => bail!(
            "Session not found: {} (searched {} profiles for a full session id or an exact title; pass -p <profile> to match an id prefix or a project path within one profile)",
            identifier,
            profiles.len()
        ),
        n => bail!(
            "Ambiguous session title {:?}: {} sessions carry it:\n{}\nUse the full session id, or -p <profile> where the title is unique. No action taken.",
            identifier,
            n,
            describe(&by_title)
        ),
    }
}

/// Best-effort deletion of a structured-view session's durable transcript
/// (the ACP event-store rows under `<app_dir>/acp_events.db`) during a CLI
/// permanent purge (`aoe rm --purge`, `aoe session empty-trash`). The serve
/// daemon does this through its supervisor; the CLI has no live worker, so it
/// opens the event store directly. It cannot send the adapter `session/delete`
/// RPC the daemon sends (that needs a running worker), but deleting the local
/// UI transcript stops purged rows from orphaning. No-op when the store does
/// not exist; a failure to open or write it returns `Err` so callers keep the
/// session row rather than orphan its transcript. See #2489, #2524.
///
/// The delete is idempotent and deliberately does NOT gate on
/// `Instance::is_structured()`: deleting zero rows for a terminal session is
/// harmless, and the old guard orphaned transcripts (#2524).
pub(crate) fn purge_acp_transcript(inst: &Instance) -> Result<()> {
    let app_dir = crate::session::get_app_dir()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: resolve app dir: {e}"))?;
    let db_path = app_dir.join("acp_events.db");
    if !db_path.exists() {
        return Ok(());
    }
    purge_acp_transcript_rows(&db_path, &inst.id)
}

/// Delete a session's rows from the ACP event store at `db_path`, removing both
/// the event rows and their attachment blobs (mirrors
/// `crate::events::delete_topic`'s cascade so no orphaned bytes are left).
/// A missing table means the store predates it: nothing to purge.
fn purge_acp_transcript_rows(db_path: &std::path::Path, session_id: &str) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db_path)
        .map_err(|e| anyhow::anyhow!("acp transcript purge: open event store: {e}"))?;
    // A running daemon may hold the store open; wait briefly rather than fail.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("acp transcript purge: set busy_timeout: {e}"))?;
    // Both deletes run in one transaction so the purge is all-or-nothing: if the
    // attachments delete fails after the events delete, the dropped `tx` rolls
    // both back and the caller keeps the session row for retry rather than
    // leaving the transcript half removed.
    let tx = conn
        .transaction()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: begin transaction: {e}"))?;
    // Table names come from the same `crate::events::Schema` the store is
    // opened with, so they cannot drift; `session_id` is bound, so the
    // `format!` only interpolates a validated constant.
    let schema = crate::events::Schema::new("acp")
        .map_err(|e| anyhow::anyhow!("acp transcript purge: schema: {e}"))?;
    for table in [schema.events_table(), schema.attachments_table()] {
        match tx.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            rusqlite::params![session_id],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "acp transcript purge: delete from {table}: {e}"
                ))
            }
        }
    }
    tx.commit()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: commit: {e}"))?;
    Ok(())
}

/// Aggregated `empty-trash` outcome across per-session purge transactions.
/// Named rather than positional because every field is a `usize`.
pub(crate) struct EmptyTrashOutcome {
    /// Successfully-purged rows dropped from storage.
    pub removed: usize,
    /// Rows a peer restored AFTER our teardown began (orphan-risk; the caller
    /// warns). Kept, not dropped.
    pub restored_after_teardown: usize,
    /// Rows WE claimed whose teardown/transcript purge failed and are still
    /// trashed: genuinely kept for retry (distinct from peer restores).
    pub kept_for_retry: usize,
}

pub fn truncate(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else if max <= 3 {
        s.chars().take(max).collect()
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{}...", truncated)
    }
}

pub fn truncate_id(id: &str, max_len: usize) -> &str {
    match id.char_indices().nth(max_len) {
        Some((byte_pos, _)) => &id[..byte_pos],
        None => id,
    }
}

/// Resolve `identifier` and run `f` on the matching instance. Designed for
/// use inside `Storage::update`'s closure: find + mutate is atomic under
/// both lock layers. Delegates to `resolve_session`, so ambiguous prefixes
/// error rather than silently picking the first match.
pub(crate) fn patch_instance<F, R>(instances: &mut [Instance], identifier: &str, f: F) -> Result<R>
where
    F: FnOnce(&mut Instance) -> Result<R>,
{
    let id = resolve_session(identifier, instances)?.id.clone();
    let inst = instances
        .iter_mut()
        .find(|i| i.id == id)
        .expect("resolve_session returned an id that is no longer in instances");
    f(inst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::claim::purge_restored_row_must_be_kept;

    #[test]
    fn truncate_id_shorter_than_max_returns_input() {
        assert_eq!(truncate_id("abc", 8), "abc");
    }

    #[test]
    fn truncate_id_equal_to_max_returns_input() {
        assert_eq!(truncate_id("abcdefgh", 8), "abcdefgh");
    }

    #[test]
    fn truncate_id_ascii_truncates_to_max_chars() {
        assert_eq!(truncate_id("abcdefghij", 8), "abcdefgh");
    }

    #[test]
    fn truncate_id_multibyte_does_not_panic_and_respects_char_boundary() {
        // "café" is 4 chars / 5 bytes. The naive byte-slice version would have
        // panicked on max_len=4 mid-codepoint.
        assert_eq!(truncate_id("café", 3), "caf");
        assert_eq!(truncate_id("café", 4), "café");
        assert_eq!(truncate_id("café", 10), "café");
    }

    #[test]
    fn truncate_id_zero_max_returns_empty() {
        assert_eq!(truncate_id("abc", 0), "");
        assert_eq!(truncate_id("café", 0), "");
    }

    // `resolve_scope` (cross-profile id verbs): a full id or a unique title
    // resolves to its owning profile; anything ambiguous or looser fails
    // closed. Disk-backed (per-profile registries under an isolated app dir),
    // hence serial.
    fn seed_session(profile: &str, title: &str, id: Option<&str>) -> String {
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new(title, "/tmp/scope");
        if let Some(id) = id {
            inst.id = id.to_string();
        }
        let id = inst.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(inst);
                Ok(())
            })
            .unwrap();
        id
    }

    fn two_profiles() -> Vec<String> {
        vec!["scope-one".to_string(), "scope-two".to_string()]
    }

    #[test]
    #[serial_test::serial]
    fn resolve_scope_full_id_resolves_to_the_owning_profile() {
        let _guard = crate::session::test_support::isolate_app_dir();
        seed_session("scope-one", "alpha", None);
        let id = seed_session("scope-two", "beta", None);
        let scope = resolve_scope_in(&id, &two_profiles()).unwrap();
        assert_eq!(
            scope,
            Scope {
                profile: "scope-two".to_string(),
                identifier: id
            }
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_scope_unique_title_resolves_to_its_full_id() {
        let _guard = crate::session::test_support::isolate_app_dir();
        seed_session("scope-one", "alpha", None);
        let id = seed_session("scope-two", "beta", None);
        let scope = resolve_scope_in("beta", &two_profiles()).unwrap();
        assert_eq!(scope.profile, "scope-two");
        assert_eq!(scope.identifier, id);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_scope_ambiguous_title_refuses_and_lists_every_candidate() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let a = seed_session("scope-one", "twin", None);
        let b = seed_session("scope-two", "twin", None);
        let err = resolve_scope_in("twin", &two_profiles())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Ambiguous session title"), "{err}");
        assert!(err.contains("No action taken"), "{err}");
        assert!(err.contains(&format!("scope-one: {a}")), "{err}");
        assert!(err.contains(&format!("scope-two: {b}")), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_scope_same_id_in_two_profiles_refuses() {
        let _guard = crate::session::test_support::isolate_app_dir();
        seed_session("scope-one", "here", Some("dupdupdupdupdup1"));
        seed_session("scope-two", "there", Some("dupdupdupdupdup1"));
        let err = resolve_scope_in("dupdupdupdupdup1", &two_profiles())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Ambiguous session id"), "{err}");
        assert!(err.contains("2 profiles"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_scope_id_prefix_and_unknown_are_session_not_found() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let id = seed_session("scope-one", "alpha", None);
        let prefix = &id[..8];
        let err = resolve_scope_in(prefix, &two_profiles())
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(&format!("Session not found: {prefix}")),
            "{err}"
        );
        let err = resolve_scope_in("no-such-session", &two_profiles())
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Session not found: no-such-session"),
            "{err}"
        );
    }

    #[test]
    fn resolve_scope_explicit_profile_passes_the_identifier_through_untouched() {
        // No disk access at all: the caller's profile wins and the identifier
        // (even a prefix) is left for that profile's `resolve_session`.
        let scope = resolve_scope_with("named", true, "abc12345").unwrap();
        assert_eq!(
            scope,
            Scope {
                profile: "named".to_string(),
                identifier: "abc12345".to_string()
            }
        );
    }

    #[test]
    fn patch_instance_exact_id_resolves_unambiguously() {
        let mut v = vec![
            Instance::new("first", "/tmp/a"),
            Instance::new("second", "/tmp/b"),
        ];
        let target_id = v[1].id.clone();
        patch_instance(&mut v, &target_id, |i| {
            i.title = "hit".to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(v[1].title, "hit");
        assert_eq!(v[0].title, "first");
    }

    #[test]
    fn patch_instance_rejects_ambiguous_prefix() {
        let mut v = vec![
            Instance::new("first", "/tmp/a"),
            Instance::new("second", "/tmp/b"),
        ];
        v[0].id = "abcdef-1".to_string();
        v[1].id = "abcdef-2".to_string();
        let err = patch_instance(&mut v, "abcdef", |_| Ok(())).unwrap_err();
        assert!(
            err.to_string().contains("Ambiguous"),
            "expected ambiguity error, got: {err}"
        );
    }

    #[test]
    fn patch_instance_resolves_by_title() {
        let mut v = vec![
            Instance::new("alpha", "/tmp/a"),
            Instance::new("beta", "/tmp/b"),
        ];
        patch_instance(&mut v, "beta", |i| {
            i.title = "renamed".to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(v[1].title, "renamed");
    }

    // #2534: a purge keeps a targeted row only when it was trashed at snapshot
    // time but is no longer trashed (restored mid-purge); every other case
    // drops it (still trashed, or a direct live purge with no restore to lose).
    #[test]
    fn purge_keeps_only_rows_restored_after_a_trashed_snapshot() {
        assert!(purge_restored_row_must_be_kept(true, false));
        assert!(!purge_restored_row_must_be_kept(true, true));
        assert!(!purge_restored_row_must_be_kept(false, false));
        assert!(!purge_restored_row_must_be_kept(false, true));
    }

    // #2524: the purge path used to be unreachable, orphaning transcripts.
    // The row delete must drop both the event rows and their attachment blobs
    // for the target session only.
    #[test]
    fn purge_acp_transcript_rows_deletes_only_target_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("acp_events.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (session_id TEXT, seq INTEGER, event_json TEXT);
             CREATE TABLE acp_attachments (session_id TEXT, attachment_id TEXT, data BLOB);
             INSERT INTO acp_events VALUES ('keep', 0, '{}'), ('drop', 0, '{}'), ('drop', 1, '{}');
             INSERT INTO acp_attachments VALUES ('keep', 'a0', x'00'), ('drop', 'a1', x'01');",
        )
        .unwrap();
        drop(conn);

        purge_acp_transcript_rows(&db_path, "drop").unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'drop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let attachments: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_attachments WHERE session_id = 'drop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let kept_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'keep'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0, "target event rows should be deleted");
        assert_eq!(attachments, 0, "target attachment blobs should be deleted");
        assert_eq!(kept_events, 1, "other session must be untouched");
    }

    // A store that predates a table (or any expected table missing) is not an
    // error: there is simply nothing to purge.
    #[test]
    fn purge_acp_transcript_rows_tolerates_missing_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("acp_events.db");
        // Open creates an empty db with neither acp_events nor acp_attachments.
        rusqlite::Connection::open(&db_path).unwrap();
        purge_acp_transcript_rows(&db_path, "whatever").unwrap();
    }
}

#[cfg(test)]
mod resolve_session_trash_tests {
    use super::*;

    /// A title lookup must land on the live session, not a trashed twin that
    /// happens to sort first in storage. Otherwise `aoe send <title>` aims at
    /// the soft-deleted record and either refuses or, worse, revives it.
    #[test]
    fn resolve_session_title_prefers_live_over_trashed_twin() {
        let mut dead = Instance::new("per-mcp", "/tmp/per-mcp-old");
        dead.id = "dead00000000dead".to_string();
        dead.trash();
        let mut live = Instance::new("per-mcp", "/tmp/per-mcp");
        live.id = "live00000000live".to_string();
        let instances = vec![dead, live];

        let got = resolve_session("per-mcp", &instances).expect("title resolves");
        assert_eq!(got.id, "live00000000live");
    }

    /// Only a trashed match exists: still resolve it (the caller decides what
    /// a trashed target means), never "not found".
    #[test]
    fn resolve_session_title_falls_back_to_trashed_when_no_live_twin() {
        let mut dead = Instance::new("per-mcp", "/tmp/per-mcp-old");
        dead.id = "dead00000000dead".to_string();
        dead.trash();
        let instances = vec![dead];

        let got = resolve_session("per-mcp", &instances).expect("title resolves");
        assert_eq!(got.id, "dead00000000dead");
    }
}
