//! Host-side Claude Code folder-trust seeding for a launch.
//!
//! Claude Code shows a workspace-trust dialog the first time it starts in a
//! directory whose record is missing from the config tree it reads, and a
//! headless tmux pane answers that dialog by dying: the record shows Error or
//! Idle, the pane is dead, and anything typed into the session is lost with no
//! banner. Sandboxed launches already seed the record into the staged config
//! dir (`session::container_config`); this is the host-launch counterpart.
//!
//! The record is keyed on the *canonical* workspace path (`process.cwd()`
//! inside the pane reports the physical path) and written into the
//! `.claude.json` of the config tree `CLAUDE_CONFIG_DIR` selects for THIS
//! launch. That is what makes a profile move safe: the destination account is
//! seeded before its first pane exists, on every launch path (create, restart,
//! a staged re-bind firing days later).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Instance;

/// Where Claude Code keeps `.claude.json` for a launch: inside the
/// `CLAUDE_CONFIG_DIR` the pane will see, else next to (not inside) the default
/// `~/.claude`, i.e. `~/.claude.json`. Mirrors the transcript probe in
/// `session::capture` so the seed side and the read side cannot drift.
pub(crate) fn claude_json_path_for(config_dir: Option<&str>, home: &Path) -> PathBuf {
    match config_dir {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => home.join(".claude.json"),
    }
}

/// The key Claude Code looks the workspace up under: the canonical path when
/// the directory resolves, else the path as configured (a missing workspace
/// fails the launch on its own terms a moment later; a phantom key is harmless).
pub(crate) fn trust_key_for(project_path: &Path) -> String {
    std::fs::canonicalize(project_path)
        .unwrap_or_else(|_| project_path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Mark `project_path` trusted in `claude_json`, creating the config dir and
/// the file when the account has never been opened. Every other key in the
/// file is preserved and an unchanged file is not rewritten
/// (`hooks::trust_claude_project`). Returns the key written.
pub(crate) fn seed_claude_folder_trust_at(
    claude_json: &Path,
    project_path: &Path,
) -> Result<String> {
    if let Some(parent) = claude_json.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating Claude config dir {}", parent.display()))?;
    }
    let key = trust_key_for(project_path);
    crate::hooks::trust_claude_project(claude_json, &key, crate::hooks::SymlinkPolicy::Follow)
        .with_context(|| {
            format!(
                "writing folder trust for {} into {}",
                key,
                claude_json.display()
            )
        })?;
    Ok(key)
}

impl Instance {
    /// Seed the folder-trust record for this session's workspace into the
    /// config tree its pane is about to read. Host Claude launches only: a
    /// sandboxed launch is seeded by `container_config` into the staged dir,
    /// and other agents keep no such record.
    ///
    /// An error here aborts the launch. Launching anyway would land the pane
    /// on the trust prompt, which kills it silently; a loud launch failure
    /// names the file and the workspace instead.
    pub(super) fn seed_host_folder_trust(&self) -> Result<()> {
        if self.is_sandboxed() || self.capture_agent_name() != Some("claude") {
            return Ok(());
        }
        let home =
            dirs::home_dir().context("cannot seed Claude folder trust: no home directory")?;
        let config_dir = crate::hooks::resolve_config_dir_override(
            "CLAUDE_CONFIG_DIR",
            &self.resolved_host_environment(),
        );
        let claude_json = claude_json_path_for(config_dir.as_deref(), &home);
        let key = seed_claude_folder_trust_at(&claude_json, Path::new(&self.project_path))
            .with_context(|| {
                format!(
                    "refusing to launch {}: could not seed Claude folder trust for {} in {} \
                     (the pane would die on the workspace-trust prompt)",
                    self.id,
                    self.project_path,
                    claude_json.display()
                )
            })?;
        tracing::debug!(
            target: "session.store",
            instance = %self.id,
            key,
            config = %claude_json.display(),
            "Claude folder trust seeded for launch"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn config_dir_override_puts_claude_json_inside_it() {
        let home = Path::new("/home/someone");
        assert_eq!(
            claude_json_path_for(Some("/accounts/alpha"), home),
            PathBuf::from("/accounts/alpha/.claude.json")
        );
    }

    #[test]
    fn no_override_uses_home_claude_json_next_to_dot_claude() {
        let home = Path::new("/home/someone");
        assert_eq!(
            claude_json_path_for(None, home),
            PathBuf::from("/home/someone/.claude.json")
        );
    }

    #[test]
    fn seed_writes_record_keyed_by_canonical_path_and_preserves_other_keys() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = temp.path().join("account");
        std::fs::create_dir_all(&config_dir).unwrap();
        let claude_json = config_dir.join(".claude.json");
        std::fs::write(
            &claude_json,
            r#"{"oauthAccount":{"accountUuid":"abc"},"projects":{"/elsewhere":{"hasTrustDialogAccepted":true,"allowedTools":["Bash"]}}}"#,
        )
        .unwrap();

        let key = seed_claude_folder_trust_at(&claude_json, &project).unwrap();

        let canonical = std::fs::canonicalize(&project).unwrap();
        assert_eq!(key, canonical.to_string_lossy());
        let json = read_json(&claude_json);
        assert_eq!(json["projects"][&key]["hasTrustDialogAccepted"], true);
        assert_eq!(json["oauthAccount"]["accountUuid"], "abc");
        assert_eq!(json["projects"]["/elsewhere"]["allowedTools"][0], "Bash");
        assert_eq!(
            json["projects"]["/elsewhere"]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn seed_is_idempotent_and_leaves_a_seeded_file_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let claude_json = temp.path().join("account").join(".claude.json");

        seed_claude_folder_trust_at(&claude_json, &project).unwrap();
        let first = std::fs::read_to_string(&claude_json).unwrap();
        let first_mtime = std::fs::metadata(&claude_json).unwrap().modified().unwrap();
        seed_claude_folder_trust_at(&claude_json, &project).unwrap();

        assert_eq!(std::fs::read_to_string(&claude_json).unwrap(), first);
        assert_eq!(
            std::fs::metadata(&claude_json).unwrap().modified().unwrap(),
            first_mtime,
            "an already-trusted workspace must not rewrite the file"
        );
    }

    #[test]
    fn seed_creates_a_never_opened_account_config_dir() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let claude_json = temp.path().join("fresh-account").join(".claude.json");
        assert!(!claude_json.parent().unwrap().exists());

        let key = seed_claude_folder_trust_at(&claude_json, &project).unwrap();

        let json = read_json(&claude_json);
        assert_eq!(json["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[cfg(unix)]
    #[test]
    fn seed_keys_a_symlinked_workspace_on_its_realpath() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-repo");
        std::fs::create_dir_all(&real).unwrap();
        let alias = temp.path().join("alias-repo");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let claude_json = temp.path().join("account").join(".claude.json");

        let key = seed_claude_folder_trust_at(&claude_json, &alias).unwrap();

        let canonical = std::fs::canonicalize(&real).unwrap();
        assert_eq!(key, canonical.to_string_lossy());
        let json = read_json(&claude_json);
        assert_eq!(json["projects"][&key]["hasTrustDialogAccepted"], true);
        assert!(json["projects"]
            .get(alias.to_string_lossy().as_ref())
            .is_none());
    }

    #[test]
    fn seed_keeps_the_configured_path_when_the_workspace_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("not-yet-cloned");
        let claude_json = temp.path().join("account").join(".claude.json");

        let key = seed_claude_folder_trust_at(&claude_json, &missing).unwrap();

        assert_eq!(key, missing.to_string_lossy());
    }

    #[test]
    fn seed_fails_loudly_when_the_config_dir_cannot_be_created() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let blocker = temp.path().join("account");
        std::fs::write(&blocker, "not a directory").unwrap();
        let claude_json = blocker.join(".claude.json");

        let err = seed_claude_folder_trust_at(&claude_json, &project).unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("creating Claude config dir"), "{msg}");
        assert!(msg.contains(&blocker.display().to_string()), "{msg}");
    }
}
