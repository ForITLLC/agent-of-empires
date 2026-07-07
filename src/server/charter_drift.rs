//! Scope/charter-drift detection for the pane watchdog (Commander WO
//! 2026-07-07).
//!
//! A fleet session's title prefix is its charter: `personal-ContactSync`
//! works the personal-ContactSync repo, `for-Forms` works the for-Forms
//! repo, and neither has business inside another product's repo or on
//! another product's appliance host. Drift out of charter has burned the
//! fleet before (a personal-ContactSync session mutating Home Assistant),
//! and nothing surfaced it: the status pipeline sees activity, not scope.
//!
//! Detection is deterministic string matching, never semantic guessing:
//!
//! - **Foreign repo**: a `GitProjects/<repo>` path in the pane tail whose
//!   first component is not the session's own registered repo, not a
//!   prefix-relative of its title, and not a shared-docs repo. Requires
//!   [`REPO_MIN_LINES`] distinct tail lines so a one-off prose mention or
//!   a pasted directory listing does not fire.
//! - **Foreign host**: a host from the [`HOST_OWNERS`] table appearing in
//!   the tail of a session whose charter does not own it. The table is an
//!   explicit fleet-topology map (like [`super::pane_watchdog::DRAW_ORDER`]),
//!   so unmapped hosts can never false-positive.
//!
//! Sessions whose title carries no tenant prefix (the AoE-Commander,
//! default civ names) have no charter and are exempt.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// Tenant prefixes that define a charter. A title outside these namespaces
/// is unchartered and never checked.
const TENANT_PREFIXES: [&str; 8] = [
    "personal-", "per-", "forit-", "for-", "gna-", "wma-", "xce-", "ras-",
];

/// Appliance/remote hosts with a known owning charter repo. Only hosts in
/// this table can produce a host-drift hit.
const HOST_OWNERS: [(&str, &str); 1] = [("homeassistant.local", "per-Home")];

/// Shared repos every fleet session legitimately reads (docs, briefs).
const ALLOW_REPOS: [&str; 2] = ["for-Common", "forit-Common"];

/// Distinct tail lines a foreign repo path must appear on before it counts
/// as work rather than a mention.
const REPO_MIN_LINES: usize = 3;

/// Non-empty tail lines in scope; matches the widest built-in rule window.
const TAIL_LINES: usize = 30;

static REPO_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"GitProjects/([A-Za-z0-9._-]+)").expect("static regex"));

/// A confirmed drift: what the session is chartered for and what it is
/// actually touching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriftHit {
    /// The session's charter: title plus its registered repo.
    pub charter: String,
    /// The out-of-charter thing observed in the pane.
    pub observed: String,
}

/// Check one session's pane tail against its charter. `None` means in
/// charter, unchartered, or not enough evidence.
pub(crate) fn detect(title: &str, project_path: &str, pane: &str) -> Option<DriftHit> {
    let title_lc = title.to_lowercase();
    if !TENANT_PREFIXES.iter().any(|p| title_lc.starts_with(p)) {
        return None;
    }
    let own_repo = project_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let charter = format!("{title} (repo {own_repo})");
    let in_charter = |name: &str| {
        let name_lc = name.to_lowercase();
        let own_lc = own_repo.to_lowercase();
        name_lc.starts_with(&title_lc)
            || title_lc.starts_with(&name_lc)
            || (!own_lc.is_empty()
                && (name_lc.starts_with(&own_lc) || own_lc.starts_with(&name_lc)))
            || ALLOW_REPOS.iter().any(|a| a.eq_ignore_ascii_case(name))
    };

    let stripped = crate::tmux::utils::strip_ansi(pane);
    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(TAIL_LINES)..];

    for (host, owner) in HOST_OWNERS {
        if !in_charter(owner)
            && tail
                .iter()
                .any(|l| l.to_lowercase().contains(host))
        {
            return Some(DriftHit {
                charter,
                observed: format!("host {host} (owned by {owner})"),
            });
        }
    }

    let mut repo_lines: HashMap<&str, usize> = HashMap::new();
    for line in tail {
        let mut seen_on_line: Vec<&str> = Vec::new();
        for cap in REPO_PATH.captures_iter(line) {
            let repo = cap.get(1).expect("group 1").as_str();
            if !seen_on_line.contains(&repo) {
                seen_on_line.push(repo);
                *repo_lines.entry(repo).or_insert(0) += 1;
            }
        }
    }
    let mut foreign: Vec<(&str, usize)> = repo_lines
        .into_iter()
        .filter(|(repo, hits)| *hits >= REPO_MIN_LINES && !in_charter(repo))
        .collect();
    foreign.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    foreign.first().map(|(repo, hits)| DriftHit {
        charter,
        observed: format!("repo {repo} ({hits} tail lines)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the acceptance case: ContactSync touching Home Assistant ───────

    #[test]
    fn contactsync_touching_homeassistant_fires() {
        let pane = "\
⏺ Bash(curl -s http://homeassistant.local:8123/api/states/light.kitchen)
  ⎿  {\"state\": \"on\", \"entity_id\": \"light.kitchen\"}
";
        let hit = detect(
            "personal-ContactSync",
            "/Users/ben/GitProjects/personal-ContactSync",
            pane,
        )
        .expect("must fire");
        assert!(hit.observed.contains("homeassistant.local"));
        assert!(hit.observed.contains("per-Home"));
        assert!(hit.charter.contains("personal-ContactSync"));
    }

    #[test]
    fn per_home_session_on_homeassistant_is_in_charter() {
        let pane = "⏺ Bash(ssh root@homeassistant.local 'ha core check')\n";
        assert_eq!(
            detect("per-Home", "/Users/ben/GitProjects/per-Home", pane),
            None
        );
    }

    #[test]
    fn unmapped_local_host_never_fires() {
        let pane = "⏺ Bash(ping -c1 printer.local)\n  ⎿  64 bytes from printer.local\n";
        assert_eq!(
            detect(
                "personal-ContactSync",
                "/Users/ben/GitProjects/personal-ContactSync",
                pane
            ),
            None
        );
    }

    // ── foreign repo work ───────────────────────────────────────────────

    #[test]
    fn for_forms_working_directory_repo_fires() {
        let pane = "\
⏺ Read(/Users/ben/GitProjects/for-Directory/src/api/people.ts)
⏺ Edit(/Users/ben/GitProjects/for-Directory/src/api/people.ts)
⏺ Bash(cd /Users/ben/GitProjects/for-Directory && npm test)
";
        let hit = detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane)
            .expect("must fire");
        assert!(hit.observed.contains("repo for-Directory"));
    }

    #[test]
    fn gna_session_in_for_repo_fires() {
        let pane = "\
⏺ Read(~/GitProjects/for-Support/src/tickets.ts)
⏺ Edit(~/GitProjects/for-Support/src/tickets.ts)
⏺ Bash(git -C ~/GitProjects/for-Support status)
";
        let hit = detect("gna-Assistant", "/Users/ben/GitProjects/gna-Assistant", pane)
            .expect("must fire");
        assert!(hit.observed.contains("repo for-Support"));
    }

    #[test]
    fn own_repo_including_nested_paths_never_fires() {
        let pane = "\
⏺ Bash(git -C ~/GitProjects/per-dev/forks/agent-of-empires log -1)
⏺ Read(~/GitProjects/per-dev/forks/agent-of-empires/src/server/mod.rs)
⏺ Edit(~/GitProjects/per-dev/cx-scripts/aoe-rebuild)
⏺ Bash(ls ~/GitProjects/per-dev/docs/plans)
";
        assert_eq!(
            detect("per-dev", "/Users/ben/GitProjects/per-dev", pane),
            None
        );
    }

    #[test]
    fn single_prose_mention_of_foreign_repo_is_below_threshold() {
        let pane = "\
the fix mirrors what for-Directory does in ~/GitProjects/for-Directory
⏺ Edit(~/GitProjects/for-Forms/src/invites.ts)
";
        assert_eq!(
            detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane),
            None
        );
    }

    #[test]
    fn repeated_mentions_on_one_line_count_once() {
        let pane = "\
diff ~/GitProjects/for-Directory/a.ts ~/GitProjects/for-Directory/b.ts ~/GitProjects/for-Directory/c.ts
";
        assert_eq!(
            detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane),
            None
        );
    }

    #[test]
    fn shared_docs_repo_reads_are_allowed() {
        let pane = "\
⏺ Read(~/GitProjects/for-Common/docs/AI-START-HERE.md)
⏺ Read(~/GitProjects/for-Common/docs/api-conventions.md)
⏺ Bash(grep -rn tenant ~/GitProjects/for-Common/docs)
";
        assert_eq!(
            detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane),
            None
        );
    }

    #[test]
    fn title_suffix_variants_of_own_product_are_in_charter() {
        // A session titled with a task suffix stays chartered for its repo.
        let pane = "\
⏺ Edit(~/GitProjects/for-Christine/src/loop.ts)
⏺ Bash(cd ~/GitProjects/for-Christine && npm test)
⏺ Read(~/GitProjects/for-Christine/README.md)
";
        assert_eq!(
            detect(
                "for-Christine-Loop",
                "/Users/ben/GitProjects/for-Christine",
                pane
            ),
            None
        );
    }

    // ── exemptions ──────────────────────────────────────────────────────

    #[test]
    fn unchartered_titles_are_exempt() {
        let pane = "\
⏺ Edit(~/GitProjects/for-Support/src/a.ts)
⏺ Edit(~/GitProjects/for-Support/src/b.ts)
⏺ Bash(curl http://homeassistant.local:8123/api/)
⏺ Bash(cd ~/GitProjects/for-Support && npm test)
";
        assert_eq!(detect("AoE-Commander", "/Users/ben/GitProjects/aoe-commander", pane), None);
        assert_eq!(detect("Byzantines", "/Users/ben/GitProjects/scratch", pane), None);
    }

    #[test]
    fn foreign_repo_deep_in_scrollback_is_out_of_window() {
        let mut pane = String::new();
        for _ in 0..3 {
            pane.push_str("⏺ Edit(~/GitProjects/for-Directory/src/api.ts)\n");
        }
        for i in 0..30 {
            pane.push_str(&format!("⏺ in-charter output line {i}\n"));
        }
        assert_eq!(
            detect("for-Forms", "/Users/ben/GitProjects/for-Forms", &pane),
            None
        );
    }
}
