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
    "personal-",
    "per-",
    "forit-",
    "for-",
    "gna-",
    "wma-",
    "xce-",
    "ras-",
];

/// Appliance/remote hosts with a known owning charter repo. Only hosts in
/// this table can produce a host-drift hit.
const HOST_OWNERS: [(&str, &str); 1] = [("homeassistant.local", "per-Home")];

/// Shared repos every fleet session legitimately reads (docs, briefs).
const ALLOW_REPOS: [&str; 2] = ["for-Common", "forit-Common"];

/// Charter-map: repos a specific charter legitimately works beyond its own
/// prefix-relatives. Keyed by the session's own repo (case-insensitive).
/// per-dev is the fleet-infra lane; its `claude-hooks/` and `mcp-servers/`
/// are symlinks into the extracted per-hooks and per-mcp repos (and the repo
/// was named personal-dev before 2026-06), so work there prints those repo
/// paths by design. The grant is per-charter, never fleet-wide.
const CHARTER_ALLIES: [(&str, &[&str]); 1] =
    [("per-dev", &["per-mcp", "per-hooks", "personal-dev"])];

/// Distinct tail lines a foreign repo path must appear on before it counts
/// as work rather than a mention.
const REPO_MIN_LINES: usize = 3;

/// Non-empty tail lines in scope; matches the widest built-in rule window.
const TAIL_LINES: usize = 30;

static REPO_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"GitProjects/([A-Za-z0-9._-]+)").expect("static regex"));

/// Compaction / hook-plumbing that Claude Code injects into EVERY session's
/// pane by design. A `/compact` block prints each PreCompact/PostToolUse hook's
/// script path, and those hooks live under `.../claude-hooks/...` (the per-dev
/// repo), so the block names per-dev on every session regardless of its actual
/// charter, plus the "Compacted", "Skills restored", and "Referenced file"
/// markers. None of it is the session's own work, so it must never count toward
/// drift. Matching on the `/claude-hooks/` path segment (not a repo name) is the
/// discriminator: it catches both `per-dev/claude-hooks/` and the legacy
/// `personal-dev/claude-hooks/` regardless of which profile printed it.
static HOOK_PLUMBING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)/claude-hooks/|(?:PreCompact|PostToolUse|PreToolUse|Stop)\s*\[|Compacted \(|Skills restored|Referenced file",
    )
    .expect("static regex")
});

/// True when a pane line is Claude Code compaction/hook plumbing rather than the
/// session's actual work. Such lines name the hooks repo on every session by
/// design and are excluded from the charter-drift scan.
fn is_hook_plumbing(line: &str) -> bool {
    HOOK_PLUMBING.is_match(line)
}

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
            || CHARTER_ALLIES.iter().any(|(charter, allies)| {
                charter.eq_ignore_ascii_case(own_repo)
                    && allies.iter().any(|a| a.eq_ignore_ascii_case(name))
            })
    };

    let stripped = crate::tmux::utils::strip_ansi(pane);
    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !is_hook_plumbing(l))
        .collect();
    let tail = &lines[lines.len().saturating_sub(TAIL_LINES)..];

    for (host, owner) in HOST_OWNERS {
        if !in_charter(owner) && tail.iter().any(|l| l.to_lowercase().contains(host)) {
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
        let hit = detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane).expect("must fire");
        assert!(hit.observed.contains("repo for-Directory"));
    }

    #[test]
    fn gna_session_in_for_repo_fires() {
        let pane = "\
⏺ Read(~/GitProjects/for-Support/src/tickets.ts)
⏺ Edit(~/GitProjects/for-Support/src/tickets.ts)
⏺ Bash(git -C ~/GitProjects/for-Support status)
";
        let hit = detect(
            "gna-Assistant",
            "/Users/ben/GitProjects/gna-Assistant",
            pane,
        )
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

    // ── charter allies: per-dev's infra lane spans the extracted repos ──

    #[test]
    fn per_dev_working_per_mcp_gateway_is_in_charter() {
        // per-dev's own mcp-servers/ is a symlink into the extracted per-mcp
        // repo, so gateway work legitimately prints per-mcp paths. Live false
        // positive: the watchdog paged URGENT while per-dev read the gateway
        // for a Commander keystroke ruling (WO 2026-07-14).
        let pane = "\
⏺ Read(~/GitProjects/per-mcp/fastmcp-gateway/comms_hold.py)
⏺ Bash(git -C ~/GitProjects/per-mcp log -1 --oneline)
⏺ Grep(classify_decision, path: ~/GitProjects/per-mcp/fastmcp-gateway)
";
        assert_eq!(
            detect("per-dev", "/Users/ben/GitProjects/per-dev", pane),
            None
        );
    }

    #[test]
    fn per_dev_working_per_hooks_is_in_charter() {
        // Same class: claude-hooks/ is a symlink into the extracted per-hooks
        // repo.
        let pane = "\
⏺ Read(~/GitProjects/per-hooks/claude-guard-hook.py)
⏺ Edit(~/GitProjects/per-hooks/claude-guard-hook.py)
⏺ Bash(cd ~/GitProjects/per-hooks && ./verify.sh)
";
        assert_eq!(
            detect("per-dev", "/Users/ben/GitProjects/per-dev", pane),
            None
        );
    }

    #[test]
    fn charter_allies_do_not_leak_to_other_sessions() {
        // The ally grant is per-charter: a product session in per-mcp is
        // still drift.
        let pane = "\
⏺ Read(~/GitProjects/per-mcp/fastmcp-gateway/server.py)
⏺ Edit(~/GitProjects/per-mcp/fastmcp-gateway/server.py)
⏺ Bash(git -C ~/GitProjects/per-mcp status)
";
        let hit = detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane)
            .expect("non-ally session in per-mcp must still fire");
        assert!(hit.observed.contains("repo per-mcp"), "{hit:?}");
    }

    // ── compaction / hook plumbing is not the session's work ────────────

    #[test]
    fn compaction_hook_plumbing_does_not_trip_drift() {
        // Verbatim shape of a `/compact` block: Claude Code prints each
        // PreCompact hook's script path, which lives under
        // `.../per-dev/claude-hooks/...` and so names the per-dev repo on
        // EVERY session by design. Three such lines used to hit
        // REPO_MIN_LINES and falsely flag a non-per-dev session (live case:
        // per-finance, cwd per-Finance, doing nothing off-charter).
        let pane = "\
⏺ Done. Health check only, as ordered.
❯ /compact
  ⎿  Compacted (ctrl+o to see full summary)
     PreCompact [/Users/ben/GitProjects/per-dev/claude-hooks/claude-attention-signal-hook.py PreCompact] completed successfully
     PreCompact [/Users/ben/GitProjects/per-dev/claude-hooks/claude-precompact-size-enforce-hook.py PreCompact] completed successfully
     PreCompact [/Users/ben/GitProjects/per-dev/claude-hooks/claude-compact-reinject-hook.py PreCompact] completed successfully
  ⎿  Referenced file ../../.claude/CLAUDE.md
  ⎿  Skills restored (superpowers:verification-before-completion)
";
        assert_eq!(
            detect("per-finance", "/Users/ben/GitProjects/per-Finance", pane),
            None
        );
    }

    #[test]
    fn real_drift_still_fires_despite_a_compaction_block() {
        // The plumbing exclusion must not blind us to genuine drift that
        // happens to share the tail with a compaction block.
        let pane = "\
⏺ Read(~/GitProjects/for-Directory/src/api/people.ts)
⏺ Edit(~/GitProjects/for-Directory/src/api/people.ts)
⏺ Bash(cd ~/GitProjects/for-Directory && npm test)
❯ /compact
  ⎿  Compacted (ctrl+o to see full summary)
     PreCompact [/Users/ben/GitProjects/per-dev/claude-hooks/claude-attention-signal-hook.py PreCompact] completed successfully
";
        let hit = detect("for-Forms", "/Users/ben/GitProjects/for-Forms", pane)
            .expect("real for-Directory work must still fire");
        assert!(hit.observed.contains("repo for-Directory"), "{hit:?}");
        assert!(
            !hit.observed.contains("per-dev"),
            "plumbing must not win: {hit:?}"
        );
    }

    #[test]
    fn is_hook_plumbing_flags_plumbing_and_spares_work() {
        assert!(is_hook_plumbing(
            "PreCompact [/Users/ben/GitProjects/per-dev/claude-hooks/x.py PreCompact] completed successfully"
        ));
        assert!(is_hook_plumbing(
            "PostToolUse [/Users/ben/GitProjects/personal-dev/claude-hooks/y.py PostToolUse] completed"
        ));
        assert!(is_hook_plumbing(
            "  ⎿  Compacted (ctrl+o to see full summary)"
        ));
        assert!(is_hook_plumbing(
            "  ⎿  Skills restored (superpowers:verification-before-completion)"
        ));
        assert!(is_hook_plumbing(
            "  ⎿  Referenced file ../../.claude/CLAUDE.md"
        ));
        // real work lines must NOT be treated as plumbing
        assert!(!is_hook_plumbing(
            "⏺ Edit(~/GitProjects/for-Directory/src/api/people.ts)"
        ));
        assert!(!is_hook_plumbing(
            "⏺ Bash(cd ~/GitProjects/for-Support && npm test)"
        ));
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
        assert_eq!(
            detect(
                "AoE-Commander",
                "/Users/ben/GitProjects/aoe-commander",
                pane
            ),
            None
        );
        assert_eq!(
            detect("Byzantines", "/Users/ben/GitProjects/scratch", pane),
            None
        );
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
