//! Tool → profile binding for the new-session dialog.
//!
//! The Profile field already snaps the engine to the profile's
//! `session.default_tool` (see `NewSessionDialog::reload_config_defaults`).
//! This module holds the pure decision for the other direction: when the
//! engine cycler lands on a tool, which profile — if any — should the dialog
//! move to so the record is created where that tool is configured?
//!
//! The decision is deliberately conservative. A user standing in `default`
//! who picks `codex` almost certainly wants the one `codex` profile (its
//! `agent_extra_args` pin, its account); a user who picks `claude` in a fleet
//! where sixteen profiles default to `claude` gave no signal about which one,
//! so the profile is left alone.

/// Decide whether landing the engine cycler on `tool` should snap the Profile
/// field to another profile.
///
/// `profiles` is the registry listing the Profile field cycles through, each
/// paired with its *resolved* `session.default_tool` (global + profile + repo,
/// the same resolution the dialog uses to pick the engine on open).
///
/// Returns `Some(profile)` only when the currently selected profile does not
/// already default to `tool` AND exactly one registered profile does. Zero or
/// several candidates return `None`: guessing between two codex profiles is
/// worse than leaving the user where they are.
pub(crate) fn profile_for_tool<'a>(
    tool: &str,
    current_profile: &str,
    profiles: &'a [(String, Option<String>)],
) -> Option<&'a str> {
    let defaults_to_tool = |default_tool: &Option<String>| default_tool.as_deref() == Some(tool);

    let current_already_bound = profiles
        .iter()
        .any(|(name, default_tool)| name == current_profile && defaults_to_tool(default_tool));
    if current_already_bound {
        return None;
    }

    let mut candidates = profiles
        .iter()
        .filter(|(_, default_tool)| defaults_to_tool(default_tool))
        .map(|(name, _)| name.as_str());
    match (candidates.next(), candidates.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::profile_for_tool;

    fn registry(entries: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        entries
            .iter()
            .map(|(name, tool)| (name.to_string(), tool.map(str::to_string)))
            .collect()
    }

    #[test]
    fn snaps_when_exactly_one_profile_defaults_to_the_tool() {
        let profiles = registry(&[("default", None), ("codex", Some("codex"))]);
        assert_eq!(
            profile_for_tool("codex", "default", &profiles),
            Some("codex")
        );
    }

    #[test]
    fn leaves_profile_when_it_already_defaults_to_the_tool() {
        let profiles = registry(&[("default", None), ("codex", Some("codex"))]);
        assert_eq!(profile_for_tool("codex", "codex", &profiles), None);
    }

    #[test]
    fn leaves_profile_when_no_profile_defaults_to_the_tool() {
        let profiles = registry(&[("default", None), ("work", Some("claude"))]);
        assert_eq!(profile_for_tool("codex", "default", &profiles), None);
    }

    #[test]
    fn leaves_profile_when_several_profiles_default_to_the_tool() {
        let profiles = registry(&[
            ("default", None),
            ("codex-a", Some("codex")),
            ("codex-b", Some("codex")),
        ]);
        assert_eq!(profile_for_tool("codex", "default", &profiles), None);
    }

    #[test]
    fn empty_registry_never_snaps() {
        assert_eq!(profile_for_tool("codex", "default", &[]), None);
    }

    #[test]
    fn current_profile_missing_from_registry_still_snaps_to_the_only_match() {
        // A stale `profile` string (registry changed under the dialog) must
        // not block the snap; the only codex profile is still the answer.
        let profiles = registry(&[("codex", Some("codex"))]);
        assert_eq!(profile_for_tool("codex", "ghost", &profiles), Some("codex"));
    }

    /// The fleet shape this exists for: a global `default_tool = "claude"`
    /// resolves every ordinary profile to claude, and one `codex` profile
    /// overrides to codex. Picking codex from anywhere lands on the codex
    /// profile; picking claude from the codex profile is ambiguous (sixteen
    /// candidates) and stays put.
    #[test]
    fn fleet_shape_codex_snaps_and_claude_stays() {
        let mut entries: Vec<(&str, Option<&str>)> = vec![
            "default",
            "forit-main",
            "forit-work",
            "gna-main",
            "wma-work",
            "xce-main",
        ]
        .into_iter()
        .map(|name| (name, Some("claude")))
        .collect();
        entries.push(("codex", Some("codex")));
        let profiles = registry(&entries);

        assert_eq!(
            profile_for_tool("codex", "forit-main", &profiles),
            Some("codex")
        );
        assert_eq!(profile_for_tool("claude", "codex", &profiles), None);
        assert_eq!(profile_for_tool("claude", "forit-main", &profiles), None);
    }
}
