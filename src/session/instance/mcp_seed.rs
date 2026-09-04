//! Host-side Claude Code user-scope MCP server seeding for a launch.
//!
//! Claude Code keeps its user-scope MCP servers in the `mcpServers` map of
//! `.claude.json`, inside the config tree `CLAUDE_CONFIG_DIR` selects. A
//! brand-new account dir (or a `session move` destination) has no such map,
//! so every session that lands there starts with zero MCP tools and nothing
//! on screen says so — the fleet found out when a moved session could not
//! reach its task board (WO#1894). Folder trust already has a per-launch seed
//! (`folder_trust`); this is its MCP counterpart, fed from ONE template file
//! named by `session.claude_mcp_servers_seed` rather than from a copy of
//! whichever account was handy.
//!
//! The seed only ever fills an EMPTY map. An account that lists servers keeps
//! them untouched, so an operator's edits survive every launch.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::folder_trust::claude_json_path_for;
use super::Instance;

/// Load the servers to seed from the template file: either a bare map of
/// server name → definition, or a `.claude.json`-shaped object whose
/// `mcpServers` map is taken. A template with no servers is a configuration
/// error, not an empty seed — it would silently disable the feature.
pub(crate) fn load_mcp_servers_template(
    path: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading MCP servers seed template {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing MCP servers seed template {}", path.display()))?;
    let root = value.as_object().with_context(|| {
        format!(
            "MCP servers seed template {} is not a JSON object",
            path.display()
        )
    })?;
    let servers = match root.get("mcpServers") {
        Some(inner) => inner
            .as_object()
            .with_context(|| {
                format!(
                    "MCP servers seed template {}: `mcpServers` is not an object",
                    path.display()
                )
            })?
            .clone(),
        None => root.clone(),
    };
    if servers.is_empty() {
        anyhow::bail!(
            "MCP servers seed template {} lists no servers",
            path.display()
        );
    }
    if let Some((name, bad)) = servers.iter().find(|(_, v)| !v.is_object()) {
        anyhow::bail!(
            "MCP servers seed template {}: server {name:?} is not an object ({bad})",
            path.display()
        );
    }
    Ok(servers)
}

/// Expand a leading `~/` against `home`; other paths pass through.
pub(crate) fn expand_seed_path(configured: &str, home: &Path) -> PathBuf {
    match configured.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if configured == "~" => home.to_path_buf(),
        None => PathBuf::from(configured),
    }
}

/// Seed `servers` into `claude_json` when its `mcpServers` map is missing or
/// empty, creating the config dir and the file for a never-opened account.
/// Returns `true` when the file was written.
pub(crate) fn seed_claude_mcp_servers_at(
    claude_json: &Path,
    servers: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool> {
    if let Some(parent) = claude_json.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating Claude config dir {}", parent.display()))?;
    }
    crate::hooks::seed_claude_mcp_servers(claude_json, servers, crate::hooks::SymlinkPolicy::Follow)
        .with_context(|| {
            format!(
                "seeding {} MCP server(s) into {}",
                servers.len(),
                claude_json.display()
            )
        })
}

/// Seed the user-scope MCP servers of the account at `claude_json` from the
/// template `configured` names (`None`/empty = feature off). Returns the
/// template path and whether the file was written, or `None` when off.
pub(crate) fn seed_mcp_servers_from_template(
    configured: Option<&str>,
    claude_json: &Path,
    home: &Path,
) -> Result<Option<(PathBuf, bool)>> {
    let Some(configured) = configured.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let template = expand_seed_path(configured, home);
    let servers = load_mcp_servers_template(&template)?;
    let written = seed_claude_mcp_servers_at(claude_json, &servers)?;
    Ok(Some((template, written)))
}

impl Instance {
    /// Seed the fleet's user-scope MCP servers into the config tree this
    /// session's pane is about to read, when that tree lists none. Host
    /// Claude launches only, and only when `session.claude_mcp_servers_seed`
    /// names a template.
    ///
    /// A configured template that cannot be read or applied aborts the
    /// launch, like folder trust: the operator asked for every account to
    /// carry these servers, and a pane that starts without them fails
    /// silently at its first MCP call.
    pub(super) fn seed_host_mcp_servers(&self) -> Result<()> {
        if self.is_sandboxed() || self.capture_agent_name() != Some("claude") {
            return Ok(());
        }
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        );
        let Some(configured) = config.session.claude_mcp_servers_seed.as_deref() else {
            return Ok(());
        };
        let home = dirs::home_dir().context("cannot seed Claude MCP servers: no home directory")?;
        let config_dir = crate::hooks::resolve_config_dir_override(
            "CLAUDE_CONFIG_DIR",
            &self.resolved_host_environment(),
        );
        let claude_json = claude_json_path_for(config_dir.as_deref(), &home);
        let seeded = seed_mcp_servers_from_template(Some(configured), &claude_json, &home)
            .with_context(|| {
                format!(
                    "refusing to launch {}: could not seed Claude MCP servers into {} from {} \
                     (the pane would start with no MCP tools)",
                    self.id,
                    claude_json.display(),
                    configured
                )
            })?;
        if let Some((template, true)) = seeded {
            tracing::info!(
                target: "session.store",
                instance = %self.id,
                config = %claude_json.display(),
                template = %template.display(),
                "Claude user-scope MCP servers seeded for launch"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    const TEMPLATE: &str = r#"{"mcpServers":{"fleet-gateway":{"type":"http","url":"https://gw.example/mcp","headers":{"Authorization":"Bearer t"}},"for-mcp":{"type":"http","url":"https://for.example/mcp"}}}"#;

    #[test]
    fn template_accepts_claude_json_shape_and_bare_map() {
        let temp = tempfile::tempdir().unwrap();
        let shaped = temp.path().join("shaped.json");
        std::fs::write(&shaped, TEMPLATE).unwrap();
        let servers = load_mcp_servers_template(&shaped).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers["for-mcp"]["url"], "https://for.example/mcp");

        let bare = temp.path().join("bare.json");
        std::fs::write(&bare, r#"{"only":{"type":"http","url":"https://x/mcp"}}"#).unwrap();
        let servers = load_mcp_servers_template(&bare).unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("only"));
    }

    #[test]
    fn template_with_no_servers_or_a_bad_entry_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("empty.json");
        std::fs::write(&empty, r#"{"mcpServers":{}}"#).unwrap();
        let err = load_mcp_servers_template(&empty).unwrap_err().to_string();
        assert!(err.contains("lists no servers"), "{err}");

        let bad = temp.path().join("bad.json");
        std::fs::write(&bad, r#"{"mcpServers":{"x":"not-an-object"}}"#).unwrap();
        let err = load_mcp_servers_template(&bad).unwrap_err().to_string();
        assert!(err.contains("is not an object"), "{err}");

        let missing = temp.path().join("missing.json");
        assert!(load_mcp_servers_template(&missing).is_err());
    }

    #[test]
    fn seed_fills_an_empty_or_missing_map_and_preserves_other_keys() {
        let temp = tempfile::tempdir().unwrap();
        let template = temp.path().join("t.json");
        std::fs::write(&template, TEMPLATE).unwrap();
        let claude_json = temp.path().join("account").join(".claude.json");
        std::fs::create_dir_all(claude_json.parent().unwrap()).unwrap();
        std::fs::write(
            &claude_json,
            r#"{"oauthAccount":{"accountUuid":"abc"},"mcpServers":{},"projects":{"/w":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();

        let (used, written) = seed_mcp_servers_from_template(
            Some(template.to_str().unwrap()),
            &claude_json,
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert!(written);
        assert_eq!(used, template);
        let json = read_json(&claude_json);
        assert_eq!(
            json["mcpServers"]["for-mcp"]["url"],
            "https://for.example/mcp"
        );
        assert_eq!(
            json["mcpServers"]["fleet-gateway"]["headers"]["Authorization"],
            "Bearer t"
        );
        assert_eq!(json["oauthAccount"]["accountUuid"], "abc");
        assert_eq!(json["projects"]["/w"]["hasTrustDialogAccepted"], true);

        // A never-opened account: dir and file are created.
        let fresh = temp.path().join("fresh-account").join(".claude.json");
        let (_, written) =
            seed_mcp_servers_from_template(Some(template.to_str().unwrap()), &fresh, temp.path())
                .unwrap()
                .unwrap();
        assert!(written);
        assert_eq!(
            read_json(&fresh)["mcpServers"].as_object().unwrap().len(),
            2
        );
    }

    #[test]
    fn seed_never_overwrites_an_account_that_already_lists_servers() {
        let temp = tempfile::tempdir().unwrap();
        let template = temp.path().join("t.json");
        std::fs::write(&template, TEMPLATE).unwrap();
        let claude_json = temp.path().join("account").join(".claude.json");
        std::fs::create_dir_all(claude_json.parent().unwrap()).unwrap();
        let original = r#"{"mcpServers":{"mine":{"type":"stdio","command":"x"}}}"#;
        std::fs::write(&claude_json, original).unwrap();
        let mtime = std::fs::metadata(&claude_json).unwrap().modified().unwrap();

        let (_, written) = seed_mcp_servers_from_template(
            Some(template.to_str().unwrap()),
            &claude_json,
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert!(!written);
        assert_eq!(std::fs::read_to_string(&claude_json).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&claude_json).unwrap().modified().unwrap(),
            mtime
        );
    }

    #[test]
    fn unset_or_blank_setting_is_off_and_tilde_expands_to_home() {
        let temp = tempfile::tempdir().unwrap();
        let claude_json = temp.path().join("a").join(".claude.json");
        assert!(
            seed_mcp_servers_from_template(None, &claude_json, temp.path())
                .unwrap()
                .is_none()
        );
        assert!(
            seed_mcp_servers_from_template(Some("  "), &claude_json, temp.path())
                .unwrap()
                .is_none()
        );
        assert!(!claude_json.exists());

        assert_eq!(
            expand_seed_path(
                "~/.claude-accounts/_shared/mcp-servers.json",
                Path::new("/home/x")
            ),
            PathBuf::from("/home/x/.claude-accounts/_shared/mcp-servers.json")
        );
        assert_eq!(
            expand_seed_path("/abs/t.json", Path::new("/home/x")),
            PathBuf::from("/abs/t.json")
        );
    }

    #[test]
    fn configured_but_unreadable_template_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let claude_json = temp.path().join("a").join(".claude.json");
        let err = seed_mcp_servers_from_template(
            Some("/nonexistent/seed.json"),
            &claude_json,
            temp.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("/nonexistent/seed.json"), "{err}");
        assert!(
            !claude_json.exists(),
            "nothing is written when the template fails"
        );
    }
}
