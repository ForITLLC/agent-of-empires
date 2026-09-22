//! Resolving an instance to its tmux session, and the options applied to it.

use super::*;

pub(super) fn tmux_env_session_name_for_instance_id(instance_id: &str) -> Option<String> {
    let live = crate::tmux::probe_live_sessions()?;
    crate::tmux::live_any_kind_name_for_id(
        crate::tmux::marked_names(&live),
        instance_id,
        crate::tmux::utils::is_pane_dead,
    )
}

/// Live-scan result for seeding a session-id poller.
/// Only an empty or unavailable scan permits a title-derived fallback.
pub(crate) enum AgentSeed {
    /// The unique live agent session for the id.
    Agent(String),
    /// Live panes exist, but no unique eligible agent can be selected.
    NoUniqueAgent,
    /// Nothing live for the id, or the tmux server could not be reached.
    NothingLive,
}

pub(super) fn live_agent_seed_for_instance_id(instance_id: &str) -> AgentSeed {
    let Some(live) = crate::tmux::probe_live_sessions() else {
        return AgentSeed::NothingLive;
    };
    if let Some(name) = crate::tmux::live_agent_name_for_id(
        crate::tmux::marked_names(&live),
        instance_id,
        crate::tmux::utils::is_pane_dead,
    ) {
        return AgentSeed::Agent(name);
    }
    if crate::tmux::live_any_kind_name_for_id(
        crate::tmux::marked_names(&live),
        instance_id,
        crate::tmux::utils::is_pane_dead,
    )
    .is_some()
    {
        return AgentSeed::NoUniqueAgent;
    }
    AgentSeed::NothingLive
}

/// Find another session that owns the exact title and normalized path.
pub(crate) fn find_duplicate_session<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    title: &str,
    path: &str,
    exclude_id: Option<&str>,
) -> Option<&'a Instance> {
    let normalized_path = path.trim_end_matches('/');
    instances.into_iter().find(|inst| {
        exclude_id != Some(inst.id.as_str())
            && inst.project_path.trim_end_matches('/') == normalized_path
            && inst.title == title
    })
}

pub(crate) fn is_duplicate_session<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    title: &str,
    path: &str,
    exclude_id: Option<&str>,
) -> bool {
    find_duplicate_session(instances, title, path, exclude_id).is_some()
}

pub(crate) fn duplicate_session_error(title: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Session already exists with same title and path: {}\n\
         Tip: use a different title or remove the existing session first",
        title
    )
}

/// [`find_duplicate_session`] restricted to rows that are neither archived
/// nor trashed. Restart and profile-move paths use this so a shelved corpse
/// holding the same title and path can never block a live row: on
/// 2026-09-22 a `for-Jamf` restart was refused by an archived twin in the
/// target profile (WO#2205).
pub(crate) fn find_live_duplicate_session<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    title: &str,
    path: &str,
    exclude_id: Option<&str>,
) -> Option<&'a Instance> {
    find_duplicate_session(
        instances
            .into_iter()
            .filter(|inst| !inst.is_archived() && !inst.is_trashed()),
        title,
        path,
        exclude_id,
    )
}

pub(crate) fn is_live_duplicate_session<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    title: &str,
    path: &str,
    exclude_id: Option<&str>,
) -> bool {
    find_live_duplicate_session(instances, title, path, exclude_id).is_some()
}

/// A board-wide title collision: another non-trashed row (archived included,
/// in any profile) already carries the title, compared case-insensitively
/// after trimming whitespace. The path is deliberately ignored: the board is
/// keyed by what a human reads, and two rows reading `for-Jamf` are a
/// duplicate no matter where they check out (WO#2205).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TitleCollision {
    pub profile: String,
    pub id: String,
    pub title: String,
    pub archived: bool,
}

impl TitleCollision {
    pub fn state(&self) -> &'static str {
        if self.archived {
            "archived"
        } else {
            "live"
        }
    }
}

pub(crate) fn find_title_collision<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a Instance)>,
    title: &str,
    exclude_id: Option<&str>,
) -> Option<TitleCollision> {
    let wanted = title.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    rows.into_iter().find_map(|(profile, inst)| {
        if inst.is_trashed() || exclude_id == Some(inst.id.as_str()) {
            return None;
        }
        if inst.title.trim().to_lowercase() != wanted {
            return None;
        }
        Some(TitleCollision {
            profile: profile.to_string(),
            id: inst.id.clone(),
            title: inst.title.clone(),
            archived: inst.is_archived(),
        })
    })
}

pub(crate) fn title_collision_error(title: &str, hit: &TitleCollision) -> anyhow::Error {
    anyhow::anyhow!(
        "Title '{}' is already in use by {} session {} in profile '{}' (titled '{}')\n\
         Tip: pick another title, or pass --allow-duplicate to proceed anyway",
        title,
        hit.state(),
        hit.id,
        hit.profile,
        hit.title
    )
}

/// Every profile's persisted rows, paired with the profile name, for a
/// board-wide title check. Reads the sessions files directly, so archived
/// rows are included.
pub(crate) fn load_all_profile_rows() -> anyhow::Result<Vec<(String, Instance)>> {
    let mut rows = Vec::new();
    for profile in crate::session::list_profiles()? {
        let storage = crate::session::Storage::open_unwatched(&profile)?;
        for inst in storage.load()? {
            rows.push((profile.clone(), inst));
        }
    }
    Ok(rows)
}

impl Instance {
    pub fn tmux_session(&self) -> Result<tmux::Session> {
        tmux::Session::new(&self.id, &self.title)
    }

    pub(crate) fn tmux_env_session_name(&self) -> Option<String> {
        tmux_env_session_name_for_instance_id(&self.id)
    }

    /// Fresh agent-seed classification, including live panes with no unique eligible agent.
    pub(crate) fn live_agent_seed(&self) -> AgentSeed {
        live_agent_seed_for_instance_id(&self.id)
    }

    /// [`Self::tmux_env_session_name`] answered from a snapshot the caller
    /// already holds, for passes that ask once per stored session.
    pub(crate) fn tmux_env_session_name_in(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> Option<String> {
        crate::tmux::live_any_kind_name_for_id_in(snapshot, &self.id)
    }

    /// [`Self::tmux_env_session_name_in`] for a one-shot pass that cannot retry.
    pub(crate) fn tmux_env_session_name_in_or_probe(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> Option<String> {
        match snapshot.names() {
            Some(_) => self.tmux_env_session_name_in(snapshot),
            None => self.tmux_env_session_name(),
        }
    }

    /// Whether this instance has a live tmux pane, answered from a snapshot the caller already
    /// holds.
    pub(crate) fn has_live_tmux_pane_in(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> bool {
        self.tmux_env_session_name_in(snapshot).is_some()
    }

    /// Whether the AGENT pane specifically is live. Poller repair gates on this.
    pub(crate) fn has_live_agent_pane_in(
        &self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> bool {
        crate::tmux::live_agent_name_for_id_in(snapshot, &self.id).is_some()
    }

    pub(super) fn sandbox_display(&self) -> Option<crate::tmux::status_bar::SandboxDisplay> {
        self.sandbox_info.as_ref().and_then(|s| {
            if s.enabled {
                Some(crate::tmux::status_bar::SandboxDisplay {
                    container_name: s.container_name.clone(),
                })
            } else {
                None
            }
        })
    }

    /// Apply all configured tmux options to a session with the given name and title.
    fn apply_session_tmux_options(&self, session_name: &str, display_title: &str) {
        let branch = self
            .worktree_info
            .as_ref()
            .map(|w| w.branch.as_str())
            .or_else(|| self.workspace_info.as_ref().map(|w| w.branch.as_str()));
        let sandbox = self.sandbox_display();
        crate::tmux::status_bar::apply_all_tmux_options(
            session_name,
            display_title,
            branch,
            sandbox.as_ref(),
            &self.effective_profile(),
        );
    }

    pub(super) fn apply_container_terminal_tmux_options(&self, index: u32) {
        let name =
            tmux::ContainerTerminalSession::resolve_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (container)", self.title));
    }

    pub(super) fn apply_terminal_tmux_options(&self, index: u32) {
        let name = tmux::TerminalSession::resolve_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (terminal)", self.title));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::test_support::EnvGuard;

    #[test]
    fn live_duplicate_ignores_archived_and_trashed_twins() {
        let live = Instance::new("for-Jamf", "/tmp/for-Jamf");
        let mut archived = Instance::new("for-Jamf", "/tmp/for-Jamf");
        archived.archive();
        let mut trashed = Instance::new("for-Jamf", "/tmp/for-Jamf/");
        trashed.trash();

        let shelved = vec![archived.clone(), trashed.clone()];
        assert!(is_duplicate_session(
            &shelved,
            "for-Jamf",
            "/tmp/for-Jamf",
            None
        ));
        assert!(!is_live_duplicate_session(
            &shelved,
            "for-Jamf",
            "/tmp/for-Jamf",
            None
        ));

        let with_live = vec![archived, trashed, live.clone()];
        assert!(is_live_duplicate_session(
            &with_live,
            "for-Jamf",
            "/tmp/for-Jamf",
            None
        ));
        assert!(!is_live_duplicate_session(
            &with_live,
            "for-Jamf",
            "/tmp/for-Jamf",
            Some(&live.id)
        ));
    }

    #[test]
    fn title_collision_spans_profiles_ignores_case_and_path_and_skips_trash() {
        let live = Instance::new("for-Jamf", "/home/a/for-Jamf");
        let mut archived = Instance::new("For-jamf ", "/home/b/for-Jamf");
        archived.archive();
        let mut trashed = Instance::new("for-Jamf", "/home/c/for-Jamf");
        trashed.trash();
        let rows = vec![
            ("gna-main", &live),
            ("forit-codex", &archived),
            ("p9-main", &trashed),
        ];

        let hit = find_title_collision(rows.clone(), "for-jamf", None).expect("live twin collides");
        assert_eq!(hit.profile, "gna-main");
        assert_eq!(hit.id, live.id);
        assert!(!hit.archived);
        assert_eq!(hit.state(), "live");

        let hit = find_title_collision(rows.clone(), "FOR-JAMF", Some(&live.id))
            .expect("archived twin still collides");
        assert_eq!(hit.profile, "forit-codex");
        assert_eq!(hit.id, archived.id);
        assert!(hit.archived);
        assert_eq!(hit.state(), "archived");

        assert!(find_title_collision(vec![("p9-main", &trashed)], "for-Jamf", None).is_none());
        assert!(find_title_collision(rows.clone(), "for-Forms", None).is_none());
        assert!(find_title_collision(rows, "  ", None).is_none());

        let msg = title_collision_error(
            "for-Jamf",
            &TitleCollision {
                profile: "forit-codex".into(),
                id: "abc123".into(),
                title: "For-jamf".into(),
                archived: true,
            },
        )
        .to_string();
        assert!(msg.contains("archived session abc123"), "{msg}");
        assert!(msg.contains("profile 'forit-codex'"), "{msg}");
        assert!(msg.contains("--allow-duplicate"), "{msg}");
    }

    #[test]
    fn duplicate_session_normalizes_path_and_excludes_self() {
        let first = Instance::new("main", "/tmp/repo/");
        let second = Instance::new("other", "/tmp/repo");
        let instances = vec![first.clone(), second.clone()];

        assert!(is_duplicate_session(&instances, "main", "/tmp/repo", None));
        assert!(!is_duplicate_session(
            &instances,
            "main",
            "/tmp/repo/",
            Some(&first.id)
        ));
        assert!(!is_duplicate_session(
            &instances,
            "other",
            "/tmp/elsewhere",
            None
        ));
    }

    /// A one-shot pass must not read an unreachable snapshot as "no live pane".
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn one_shot_name_probes_when_the_snapshot_missed_tmux() {
        let _env_read = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let inst = Instance::new("Refactor billing", "/tmp/aoe-test-one-shot-probe");
        let live_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);

        // A `tmux` that answers with one live session name, standing in for the probe that succeeds
        // after the snapshot's own `list-sessions` failed.
        let shim = temp.path().join("tmux");
        std::fs::write(
            &shim,
            format!("#!/bin/sh\necho '{live_name}|1789065184|agent'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            temp.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _guard = EnvGuard::set(&[("PATH", path)]);

        let missed = crate::tmux::LiveSessionSnapshot::from_parts(None, None);
        assert_eq!(
            inst.tmux_env_session_name_in(&missed),
            None,
            "the snapshot alone has nothing to answer an unreachable server with"
        );
        assert_eq!(
            inst.tmux_env_session_name_in_or_probe(&missed).as_deref(),
            Some(live_name.as_str()),
            "a one-shot caller falls back to the per-item probe"
        );

        // A snapshot that did reach the server is authoritative: absent from
        // its list means absent, with no probe behind it.
        let observed = crate::tmux::LiveSessionSnapshot::from_parts(Some(Vec::new()), None);
        assert_eq!(inst.tmux_env_session_name_in_or_probe(&observed), None);
    }
}
