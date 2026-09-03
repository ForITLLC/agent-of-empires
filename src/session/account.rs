//! Account identity + plan usage per session — the daemon-side equivalent of
//! the agent's own `/status` Status and Usage tabs.
//!
//! A session record names a *profile*; the profile's `environment` names the
//! `CLAUDE_CONFIG_DIR` the agent *should* launch with. Neither is what the
//! running process is actually using: a `session move --no-restart` changes
//! the record while the pane keeps the old account, and a wrapper can export
//! its own value. The only truth is the live process environment, so this
//! module reads both — the RECORD binding and the LIVE value — and reports a
//! drift flag when they disagree.
//!
//! Usage comes from the same OAuth endpoint the agent's Usage tab reads. The
//! bearer token is read from the config dir's `.credentials.json`, sent, and
//! never stored, logged or serialized anywhere.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `GET` endpoint behind the agent's Usage tab.
pub const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
/// Beta header the OAuth surface requires.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Minimum spacing between two usage reads of one account.
pub const USAGE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// How deep below a pane's shell we look for the agent process.
const ENV_WALK_DEPTH: usize = 4;

/// Credential-file state, from `claudeAiOauth.expiresAt` by identity:
/// `0` is the agent's logout marker, a missing field is a different state
/// from a zero, and a past timestamp is an expired ACCESS token the agent
/// refreshes on its next call (not a logout).
pub const CRED_OK: &str = "ok";
pub const CRED_EXPIRED: &str = "expired";
pub const CRED_LOGGED_OUT: &str = "logged-out";
pub const CRED_NO_FIELD: &str = "no-field";
pub const CRED_NO_FILE: &str = "no-file";

/// What `/status` shows on its Status tab, read from `<config_dir>/.claude.json`
/// (`oauthAccount`) and `<config_dir>/.credentials.json` (`claudeAiOauth`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountIdentity {
    /// Directory basename — the fleet's account name (`forit-main`).
    pub account: String,
    /// Canonical config dir the identity was read from.
    pub config_dir: String,
    pub email: Option<String>,
    pub org: Option<String>,
    /// One of the `CRED_*` states.
    pub credential_state: String,
    /// `claudeAiOauth.expiresAt` (ms since epoch) when present.
    pub expires_at: Option<u64>,
}

/// The three meters the Usage tab renders: the 5-hour session meter, the
/// all-models weekly meter, and the model-scoped weekly meter for Fable.
/// A `None` percent means the endpoint did not report that row — it is
/// unknown, never zero. `error` is set (and every percent is `None`) when
/// the read failed; the text never contains the token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct UsageSnapshot {
    pub session_pct: Option<u32>,
    pub session_resets_at: Option<String>,
    pub week_pct: Option<u32>,
    pub week_resets_at: Option<String>,
    pub fable_pct: Option<u32>,
    pub fable_resets_at: Option<String>,
    /// Unix seconds when the endpoint was read (or the read failed).
    pub read_at: u64,
    pub error: Option<String>,
}

/// Where a session's account came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountSource {
    /// Read from the running agent process's environment.
    Live,
    /// No live process; the profile's recorded binding.
    Record,
}

/// Per-session account view carried on every session projection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct SessionAccount {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_org: Option<String>,
    /// Basename of the config dir the session is (live) or would be (record) on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_config_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_source: Option<AccountSource>,
    /// True when the record's profile binds a different account than the
    /// live process is using — a staged `move --no-restart`, or a wrapper
    /// override. Always `false` without a live reading.
    pub account_drift: bool,
    /// Basename the record's profile binds to (what a restart would use).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_account_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSnapshot>,
}

pub fn credential_state(found: bool, expires_at: Option<u64>, now_ms: u64) -> &'static str {
    if !found {
        return CRED_NO_FILE;
    }
    match expires_at {
        None => CRED_NO_FIELD,
        Some(0) => CRED_LOGGED_OUT,
        Some(t) if t < now_ms => CRED_EXPIRED,
        Some(_) => CRED_OK,
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Basename used as the fleet account name; the default `~/.claude` dir
/// reports as `.claude`.
pub fn account_name(config_dir: &Path) -> String {
    config_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| config_dir.to_string_lossy().into_owned())
}

/// Canonical path when it resolves, the input otherwise — so a symlinked
/// home (`/Users/x -> /home/x`) compares equal to its target.
pub fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn same_account(a: &Path, b: &Path) -> bool {
    canonical_or_self(a) == canonical_or_self(b) || account_name(a) == account_name(b)
}

/// Read the Status-tab identity from a config dir. Never fails: a missing
/// or unreadable file yields `None` fields and a `no-file` state.
pub fn read_identity(config_dir: &Path, now_ms: u64) -> AccountIdentity {
    let canon = canonical_or_self(config_dir);
    let claude_json = read_json(&canon.join(".claude.json"));
    let oauth = claude_json.as_ref().and_then(|v| v.get("oauthAccount"));
    let email = oauth
        .and_then(|o| o.get("emailAddress"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let org = oauth
        .and_then(|o| o.get("organizationName"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let creds = read_json(&canon.join(".credentials.json"));
    let expires_at = creds
        .as_ref()
        .and_then(|v| v.get("claudeAiOauth"))
        .and_then(|o| o.get("expiresAt"))
        .and_then(|v| v.as_u64());
    AccountIdentity {
        account: account_name(&canon),
        config_dir: canon.to_string_lossy().into_owned(),
        email,
        org,
        credential_state: credential_state(creds.is_some(), expires_at, now_ms).to_string(),
        expires_at,
    }
}

/// The OAuth access token for a config dir. Returned to the caller only;
/// nothing here logs or stores it.
pub fn read_access_token(config_dir: &Path) -> Option<String> {
    let creds = read_json(&canonical_or_self(config_dir).join(".credentials.json"))?;
    creds
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn pct(v: &serde_json::Value) -> Option<u32> {
    v.as_f64().map(|f| f.round().max(0.0) as u32)
}

fn reset(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|r| r.as_str()).map(str::to_string)
}

/// Parse the usage endpoint's body. The `limits[]` rows are authoritative
/// (they are what the Usage tab renders); the legacy top-level `five_hour` /
/// `seven_day` objects fill the two shared meters when `limits` is absent.
/// A Fable row is a `weekly_scoped` limit whose scope model display name is
/// `Fable`; without one, `fable_pct` is `None` (unknown).
pub fn parse_usage(body: &serde_json::Value, read_at: u64) -> UsageSnapshot {
    let mut snap = UsageSnapshot {
        read_at,
        ..Default::default()
    };
    if let Some(rows) = body.get("limits").and_then(|l| l.as_array()) {
        for row in rows {
            let kind = row.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let percent = row.get("percent").and_then(pct);
            let resets_at = reset(row.get("resets_at"));
            match kind {
                "session" => {
                    snap.session_pct = percent;
                    snap.session_resets_at = resets_at;
                }
                "weekly_all" => {
                    snap.week_pct = percent;
                    snap.week_resets_at = resets_at;
                }
                "weekly_scoped" => {
                    let model = row
                        .get("scope")
                        .and_then(|s| s.get("model"))
                        .and_then(|m| m.get("display_name"))
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    if model.eq_ignore_ascii_case("fable") {
                        snap.fable_pct = percent;
                        snap.fable_resets_at = resets_at;
                    }
                }
                _ => {}
            }
        }
    }
    if snap.session_pct.is_none() {
        if let Some(fh) = body.get("five_hour") {
            snap.session_pct = fh.get("utilization").and_then(pct);
            snap.session_resets_at = reset(fh.get("resets_at"));
        }
    }
    if snap.week_pct.is_none() {
        if let Some(sd) = body.get("seven_day") {
            snap.week_pct = sd.get("utilization").and_then(pct);
            snap.week_resets_at = reset(sd.get("resets_at"));
        }
    }
    snap
}

/// A failed read, with a message that carries the HTTP status or transport
/// class only — never the request, never the token.
pub fn usage_error(read_at: u64, error: impl Into<String>) -> UsageSnapshot {
    UsageSnapshot {
        read_at,
        error: Some(error.into()),
        ..Default::default()
    }
}

/// One environment variable of a live process, or `None` when the process
/// is gone, unreadable, or lacks the variable.
pub fn process_env_var(pid: u32, key: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        for entry in raw.split(|b| *b == 0) {
            let entry = String::from_utf8_lossy(entry);
            if let Some((k, v)) = entry.split_once('=') {
                if k == key && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        // `ps -E` appends the environment to the command line on macOS/BSD.
        let out = std::process::Command::new("ps")
            .args(["-E", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!(" {key}=");
        let start = text.find(&needle)? + needle.len();
        let rest = &text[start..];
        let end = rest.find(' ').unwrap_or(rest.len());
        let v = rest[..end].trim();
        (!v.is_empty()).then(|| v.to_string())
    }
}

/// Direct children of a process.
pub fn child_pids(pid: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
            return text
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
        }
    }
    let out = match std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// The `CLAUDE_CONFIG_DIR` the agent under a pane is really running with:
/// the pane's shell first, then its descendants breadth-first (the pane
/// process is usually a shell that received the variable on the agent's
/// command line only).
pub fn live_config_dir(pane_pid: u32) -> Option<PathBuf> {
    let mut frontier = vec![pane_pid];
    for _ in 0..=ENV_WALK_DEPTH {
        let mut next = Vec::new();
        for pid in frontier {
            if let Some(v) = process_env_var(pid, "CLAUDE_CONFIG_DIR") {
                return Some(PathBuf::from(v));
            }
            next.extend(child_pids(pid));
        }
        if next.is_empty() {
            return None;
        }
        frontier = next;
    }
    None
}

/// The agent's default config dir when nothing overrides it.
pub fn default_config_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
}

/// The config dir a profile's `environment` binds, if any.
pub fn bound_config_dir(host_env: &[String]) -> Option<PathBuf> {
    crate::session::environment::resolve_host_environment_value(host_env, "CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Drift = record and live both known and naming different accounts.
pub fn drift(record: Option<&Path>, live: Option<&Path>) -> bool {
    match (record, live) {
        (Some(r), Some(l)) => !same_account(r, l),
        _ => false,
    }
}

/// Every account dir worth reporting, even with zero sessions: the siblings
/// of each bound config dir (an accounts root like `~/.claude-accounts`),
/// skipping `_shared`-style and dot/backup entries, plus the bound dirs
/// themselves and the default dir. Sorted, canonical, de-duplicated.
pub fn discover_account_dirs<I: IntoIterator<Item = PathBuf>>(bound: I) -> Vec<PathBuf> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in bound {
        let canon = canonical_or_self(&dir);
        if canon.is_dir() {
            if let Some(parent) = canon.parent() {
                roots.insert(parent.to_path_buf());
            }
            out.insert(canon);
        }
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('_') || name.starts_with('.') || name.contains(".bak") {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                out.insert(canonical_or_self(&path));
            }
        }
    }
    let default = canonical_or_self(&default_config_dir());
    if default.is_dir() {
        out.insert(default);
    }
    out.into_iter().collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Whether `tool` is an agent whose config dir (and so its account) is
/// selected by an env var — Claude's `CLAUDE_CONFIG_DIR`. Other tools
/// carry no account view.
pub fn tool_has_config_dir(tool: &str) -> bool {
    match crate::agents::get_agent(tool) {
        Some(def) => def
            .hook_config
            .as_ref()
            .is_some_and(|h| h.config_dir_env_var.is_some()),
        // A custom agent named after claude (`claude-wrapper`) still runs it.
        None => tool.starts_with("claude"),
    }
}

/// The config dir a session on `profile` would launch with: the profile's
/// `agent_config_dir` for `tool`, else the `CLAUDE_CONFIG_DIR` in the
/// profile's `environment`, else the default dir. Canonical.
pub fn record_config_dir(profile: &str, tool: &str) -> Option<PathBuf> {
    let config = crate::session::config::profile_config::resolve_config_or_warn(profile);
    let home = dirs::home_dir()?;
    let dir = config
        .session
        .agent_config_dir_for(tool, &home)
        .or_else(|| bound_config_dir(&config.environment))
        .unwrap_or_else(default_config_dir);
    Some(canonical_or_self(&dir))
}

/// Assemble a session's account view from its record binding and (if any)
/// its live process's config dir. `identity` and `usage` are looked up by
/// config dir so callers can answer from a cache.
pub fn session_account_from_parts(
    record: Option<&Path>,
    live: Option<&Path>,
    mut identity: impl FnMut(&Path) -> AccountIdentity,
    usage: impl Fn(&Path) -> Option<UsageSnapshot>,
) -> SessionAccount {
    let (dir, source) = match (live, record) {
        (Some(l), _) => (l, AccountSource::Live),
        (None, Some(r)) => (r, AccountSource::Record),
        (None, None) => return SessionAccount::default(),
    };
    let id = identity(dir);
    SessionAccount {
        account_email: id.email,
        account_org: id.org,
        account_dir: Some(id.account),
        account_config_dir: Some(id.config_dir),
        account_source: Some(source),
        account_drift: drift(record, live),
        record_account_dir: record.map(account_name),
        usage: usage(dir),
    }
}

/// One row of the daemon's `GET /api/accounts`, as much of it as a CLI
/// needs: which config dir, and its cached usage.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountUsageRow {
    pub config_dir: String,
    #[serde(default)]
    pub usage: Option<UsageSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountsWire {
    #[serde(default)]
    pub accounts: Vec<AccountUsageRow>,
}

/// Usage per canonical config dir from the local daemon's cache, or empty
/// when no daemon is reachable — the CLI never reads the usage endpoint
/// itself (one reader per box keeps the per-account cadence honest).
pub async fn daemon_usage_map() -> std::collections::HashMap<String, UsageSnapshot> {
    let mut out = std::collections::HashMap::new();
    let Ok(endpoint) = crate::acp::client::discovery::discover() else {
        return out;
    };
    let Ok(client) = crate::acp::client::http::HttpClient::new(endpoint) else {
        return out;
    };
    match client.accounts().await {
        Ok(wire) => {
            for row in wire.accounts {
                if let Some(u) = row.usage {
                    out.insert(row.config_dir, u);
                }
            }
        }
        Err(e) => {
            tracing::debug!(target: "session.account", error = %e, "daemon usage unavailable");
        }
    }
    out
}

/// The account view of every session in one profile, resolved LOCALLY:
/// record binding from the profile config, live dir from the pane's
/// process, identity from disk. Keyed by session id. `usage` answers by
/// canonical config dir (typically [`daemon_usage_map`]).
pub fn local_session_accounts<'a>(
    instances: impl IntoIterator<Item = &'a crate::session::Instance>,
    profile: &str,
    usage: &std::collections::HashMap<String, UsageSnapshot>,
) -> std::collections::HashMap<String, SessionAccount> {
    use std::collections::HashMap;
    let now_ms = now_ms();
    let pane_metadata = crate::tmux::batch_pane_metadata().unwrap_or_default();
    let mut record_by_tool: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut identities: HashMap<PathBuf, AccountIdentity> = HashMap::new();
    let mut out = HashMap::new();
    for inst in instances {
        if !tool_has_config_dir(&inst.tool) {
            continue;
        }
        let record = record_by_tool
            .entry(inst.tool.clone())
            .or_insert_with(|| record_config_dir(profile, &inst.tool))
            .clone();
        let live = crate::tmux::Session::new(&inst.id, &inst.title)
            .ok()
            .and_then(|s| pane_metadata.get(s.name()).and_then(|m| m.pane_pid))
            .and_then(live_config_dir)
            .map(|p| canonical_or_self(&p));
        let mut ident = |dir: &Path| -> AccountIdentity {
            identities
                .entry(dir.to_path_buf())
                .or_insert_with(|| read_identity(dir, now_ms))
                .clone()
        };
        let account =
            session_account_from_parts(record.as_deref(), live.as_deref(), &mut ident, |dir| {
                usage.get(&dir.to_string_lossy().into_owned()).cloned()
            });
        out.insert(inst.id.clone(), account);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_body() -> serde_json::Value {
        serde_json::json!({
            "five_hour": {"utilization": 62.0, "resets_at": "2026-09-03T18:09:59+00:00"},
            "seven_day": {"utilization": 47.0, "resets_at": "2026-09-09T14:59:59+00:00"},
            "limits": [
                {"kind": "session", "percent": 62, "resets_at": "2026-09-03T18:09:59+00:00", "scope": null},
                {"kind": "weekly_all", "percent": 47, "resets_at": "2026-09-09T14:59:59+00:00", "scope": null},
                {"kind": "weekly_scoped", "percent": 92, "severity": "critical",
                 "resets_at": "2026-09-09T14:59:59+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
            ]
        })
    }

    #[test]
    fn parses_the_three_meters_from_limits_rows() {
        let snap = parse_usage(&usage_body(), 1_788_450_000);
        assert_eq!(snap.session_pct, Some(62));
        assert_eq!(snap.week_pct, Some(47));
        assert_eq!(snap.fable_pct, Some(92));
        assert_eq!(
            snap.fable_resets_at.as_deref(),
            Some("2026-09-09T14:59:59+00:00")
        );
        assert_eq!(snap.read_at, 1_788_450_000);
        assert!(snap.error.is_none());
    }

    #[test]
    fn a_missing_fable_row_is_unknown_not_zero() {
        let mut body = usage_body();
        body["limits"].as_array_mut().unwrap().pop();
        let snap = parse_usage(&body, 1);
        assert_eq!(snap.fable_pct, None);
        assert_eq!(snap.session_pct, Some(62));
    }

    #[test]
    fn legacy_top_level_meters_fill_in_without_limits() {
        let mut body = usage_body();
        body.as_object_mut().unwrap().remove("limits");
        let snap = parse_usage(&body, 1);
        assert_eq!(snap.session_pct, Some(62));
        assert_eq!(snap.week_pct, Some(47));
        assert_eq!(snap.fable_pct, None);
    }

    #[test]
    fn a_scoped_row_for_another_model_is_not_fable() {
        let mut body = usage_body();
        body["limits"][2]["scope"]["model"]["display_name"] = serde_json::json!("Opus");
        assert_eq!(parse_usage(&body, 1).fable_pct, None);
    }

    #[test]
    fn credential_states_are_told_apart_by_identity() {
        assert_eq!(credential_state(false, None, 10), CRED_NO_FILE);
        assert_eq!(credential_state(true, None, 10), CRED_NO_FIELD);
        assert_eq!(credential_state(true, Some(0), 10), CRED_LOGGED_OUT);
        assert_eq!(credential_state(true, Some(5), 10), CRED_EXPIRED);
        assert_eq!(credential_state(true, Some(11), 10), CRED_OK);
    }

    #[test]
    fn identity_reads_email_org_and_state_and_never_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let acct = dir.path().join("forit-main");
        std::fs::create_dir(&acct).unwrap();
        std::fs::write(
            acct.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"b@forit.io","organizationName":"ForIT"}}"#,
        )
        .unwrap();
        std::fs::write(
            acct.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-SECRET","expiresAt":9999999999999}}"#,
        )
        .unwrap();
        let id = read_identity(&acct, 1);
        assert_eq!(id.account, "forit-main");
        assert_eq!(id.email.as_deref(), Some("b@forit.io"));
        assert_eq!(id.org.as_deref(), Some("ForIT"));
        assert_eq!(id.credential_state, CRED_OK);
        let json = serde_json::to_string(&id).unwrap();
        assert!(!json.contains("SECRET"));
        assert_eq!(read_access_token(&acct).as_deref(), Some("sk-ant-SECRET"));
        assert_eq!(
            read_identity(&dir.path().join("nope"), 1).credential_state,
            CRED_NO_FILE
        );
    }

    #[test]
    fn drift_needs_both_sides_and_compares_accounts() {
        let a = Path::new("/x/.claude-accounts/forit-main");
        let b = Path::new("/y/.claude-accounts/forit-main");
        let c = Path::new("/x/.claude-accounts/cay-main");
        assert!(!drift(None, Some(a)));
        assert!(!drift(Some(a), None));
        assert!(
            !drift(Some(a), Some(b)),
            "same basename, different root = same account"
        );
        assert!(drift(Some(a), Some(c)));
    }

    #[test]
    fn discovers_sibling_account_dirs_and_skips_shared_and_backups() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["forit-main", "cay-main", "_shared", ".backups", "x.bak-1"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("stray.txt"), "").unwrap();
        let found = discover_account_dirs([dir.path().join("forit-main")]);
        let names: Vec<String> = found
            .iter()
            .filter(|p| p.starts_with(canonical_or_self(dir.path())))
            .map(|p| account_name(p))
            .collect();
        assert_eq!(names, vec!["cay-main", "forit-main"]);
    }

    #[test]
    fn live_config_dir_walks_the_process_tree() {
        // The shell itself lacks the variable; only its forked `sleep`
        // child carries it (the trailing `true` stops sh from exec-ing
        // sleep in place), so a hit proves the descendant walk.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "CLAUDE_CONFIG_DIR=/tmp/acct-from-child sleep 5; true"])
            .env_remove("CLAUDE_CONFIG_DIR")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let found = live_config_dir(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found.as_deref(), Some(Path::new("/tmp/acct-from-child")));
        assert_eq!(live_config_dir(u32::MAX - 7), None);
    }

    #[test]
    fn usage_error_carries_no_meter_values() {
        let e = usage_error(5, "http 401");
        assert_eq!(e.error.as_deref(), Some("http 401"));
        assert_eq!(e.fable_pct, None);
        assert_eq!(e.read_at, 5);
    }

    #[test]
    fn session_account_prefers_live_and_flags_drift() {
        let ident = |p: &Path| AccountIdentity {
            account: account_name(p),
            config_dir: p.display().to_string(),
            email: Some(format!("{}@x", account_name(p))),
            org: None,
            credential_state: CRED_OK.to_string(),
            expires_at: None,
        };
        let usage = |_: &Path| {
            Some(UsageSnapshot {
                fable_pct: Some(42),
                ..Default::default()
            })
        };
        let rec = Path::new("/acc/forit-main");
        let live = Path::new("/acc/forit-work");
        let a = session_account_from_parts(Some(rec), Some(live), ident, usage);
        assert_eq!(a.account_dir.as_deref(), Some("forit-work"));
        assert_eq!(a.account_email.as_deref(), Some("forit-work@x"));
        assert_eq!(a.account_source, Some(AccountSource::Live));
        assert!(a.account_drift);
        assert_eq!(a.record_account_dir.as_deref(), Some("forit-main"));
        assert_eq!(a.usage.as_ref().and_then(|u| u.fable_pct), Some(42));

        let b = session_account_from_parts(Some(rec), None, ident, usage);
        assert_eq!(b.account_source, Some(AccountSource::Record));
        assert!(!b.account_drift);
        assert_eq!(b.account_dir.as_deref(), Some("forit-main"));

        let c = session_account_from_parts(None, None, ident, usage);
        assert_eq!(c, SessionAccount::default());
    }

    #[test]
    fn tool_gate_is_claude_only() {
        assert!(tool_has_config_dir("claude"));
        assert!(!tool_has_config_dir("codex"));
        assert!(!tool_has_config_dir("shell"));
    }
}
