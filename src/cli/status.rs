//! `agent-of-empires status` command implementation

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::session::account::{
    daemon_usage_map, discover_account_dirs, local_session_accounts, now_ms, read_identity,
    record_config_dir, tool_has_config_dir, AccountIdentity, SessionAccount, UsageSnapshot,
};
use crate::session::{Status, Storage};

#[derive(Args)]
pub struct StatusArgs {
    /// Show detailed session list
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Only output waiting count (for scripts)
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Default)]
struct StatusCounts {
    running: usize,
    waiting: usize,
    idle: usize,
    stopped: usize,
    error: usize,
    total: usize,
}

#[derive(Serialize)]
struct StatusJson {
    waiting: usize,
    running: usize,
    idle: usize,
    stopped: usize,
    error: usize,
    total: usize,
    /// Every account this profile can reach — the bound config dir, its
    /// sibling account dirs and the default dir — with the identity its
    /// `/status` tab would show and the daemon's cached usage meters. An
    /// account with no sessions still reports, so a mover can see where
    /// there is room. (WO#1852)
    accounts: Vec<AccountStatusJson>,
    /// One row per session: status plus the same flattened account view
    /// `aoe list --json` and `/api/sessions` carry.
    sessions: Vec<SessionStatusJson>,
}

#[derive(Serialize)]
struct AccountStatusJson {
    #[serde(flatten)]
    identity: AccountIdentity,
    /// Sessions in this profile on this account: live process binding
    /// when there is one, else the profile's recorded binding.
    live_sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageSnapshot>,
}

#[derive(Serialize)]
struct SessionStatusJson {
    id: String,
    title: String,
    tool: String,
    status: &'static str,
    #[serde(flatten)]
    account: SessionAccount,
}

/// Account rows for `dirs`, counting the sessions on each by canonical
/// config dir. `identity` and `usage` answer by dir so this stays pure.
fn account_rows(
    dirs: Vec<PathBuf>,
    sessions: &HashMap<String, SessionAccount>,
    usage: &HashMap<String, UsageSnapshot>,
    mut identity: impl FnMut(&Path) -> AccountIdentity,
) -> Vec<AccountStatusJson> {
    dirs.into_iter()
        .map(|dir| {
            let id = identity(&dir);
            let live_sessions = sessions
                .values()
                .filter(|a| a.account_config_dir.as_deref() == Some(id.config_dir.as_str()))
                .count();
            AccountStatusJson {
                live_sessions,
                usage: usage.get(&id.config_dir).cloned(),
                identity: id,
            }
        })
        .collect()
}

fn session_rows(
    instances: &[crate::session::Instance],
    accounts: &mut HashMap<String, SessionAccount>,
) -> Vec<SessionStatusJson> {
    instances
        .iter()
        .map(|inst| SessionStatusJson {
            id: inst.id.clone(),
            title: inst.title.clone(),
            tool: inst.tool.clone(),
            status: inst.status.as_str(),
            account: accounts.remove(&inst.id).unwrap_or_default(),
        })
        .collect()
}

/// The config dirs to discover accounts from: the profile's binding for
/// every account-bearing tool in use, and always claude's, so a profile
/// with no sessions still reports its accounts.
fn bound_dirs(profile: &str, instances: &[crate::session::Instance]) -> Vec<PathBuf> {
    let mut tools: Vec<&str> = instances
        .iter()
        .map(|i| i.tool.as_str())
        .filter(|t| tool_has_config_dir(t))
        .collect();
    tools.push("claude");
    tools.sort_unstable();
    tools.dedup();
    tools
        .into_iter()
        .filter_map(|t| record_config_dir(profile, t))
        .collect()
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: StatusArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    for inst in &mut instances {
        inst.source_profile = storage.profile().to_string();
    }

    // `--json` falls through: zero sessions still has accounts to report.
    if instances.is_empty() && !args.json {
        if args.quiet {
            println!("0");
        } else {
            println!("No sessions in profile '{}'.", storage.profile());
        }
        return Ok(());
    }

    // Resolving the profile config installs the declarative status-rule
    // registry (`[[agents.<name>.status_rules]]`); the per-instance poll
    // below never loads config itself.
    crate::session::config::profile_config::resolve_config_or_warn(profile);

    // Refresh tmux session cache
    crate::tmux::refresh_session_cache();

    let contended = crate::session::Instance::contended_capture_cwds(&instances);
    for inst in &mut instances {
        inst.update_status_once(None, None);
        inst.self_heal_session_id(profile, &contended);
    }

    let counts = count_by_status(&instances);

    if args.json {
        let usage = daemon_usage_map().await;
        let mut per_session = local_session_accounts(&instances, profile, &usage);
        let now = now_ms();
        let accounts = account_rows(
            discover_account_dirs(bound_dirs(profile, &instances)),
            &per_session,
            &usage,
            |dir| read_identity(dir, now),
        );
        let status_json = StatusJson {
            waiting: counts.waiting,
            running: counts.running,
            idle: counts.idle,
            stopped: counts.stopped,
            error: counts.error,
            total: counts.total,
            accounts,
            sessions: session_rows(&instances, &mut per_session),
        };
        println!("{}", serde_json::to_string(&status_json)?);
    } else if args.quiet {
        println!("{}", counts.waiting);
    } else if args.verbose {
        print_status_group("WAITING", "⠃", Status::Waiting, &instances);
        print_status_group("RUNNING", "⠋", Status::Running, &instances);
        print_status_group("IDLE", "⠒", Status::Idle, &instances);
        print_status_group("STOPPED", "⠒", Status::Stopped, &instances);
        print_status_group("ERROR", "✕", Status::Error, &instances);
        println!(
            "Total: {} sessions in profile '{}'",
            counts.total,
            storage.profile()
        );
    } else if counts.stopped > 0 {
        println!(
            "{} waiting • {} running • {} idle • {} stopped",
            counts.waiting, counts.running, counts.idle, counts.stopped
        );
    } else {
        println!(
            "{} waiting • {} running • {} idle",
            counts.waiting, counts.running, counts.idle
        );
    }

    // Show update notice if available (skip for JSON/quiet output)
    if !args.json && !args.quiet {
        crate::update::print_update_notice().await;
    }

    Ok(())
}

fn count_by_status(instances: &[crate::session::Instance]) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for inst in instances {
        match inst.status {
            Status::Running => counts.running += 1,
            Status::Waiting => counts.waiting += 1,
            Status::Idle => counts.idle += 1,
            Status::Unknown => counts.idle += 1,
            Status::Stopped => counts.stopped += 1,
            Status::Error => counts.error += 1,
            Status::Starting => counts.idle += 1,
            Status::Deleting => {}
            Status::Creating => {}
        }
        counts.total += 1;
    }
    counts
}

fn print_status_group(
    label: &str,
    symbol: &str,
    status: Status,
    instances: &[crate::session::Instance],
) {
    let matching: Vec<_> = instances.iter().filter(|i| i.status == status).collect();
    if matching.is_empty() {
        return;
    }

    println!("{} ({}):", label, matching.len());
    for inst in matching {
        let path = shorten_path(&inst.project_path);
        println!("  {} {:<16} {:<10} {}", symbol, inst.title, inst.tool, path);
    }
    println!();
}

fn shorten_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Some(home_str) = home.to_str() {
            if let Some(stripped) = path.strip_prefix(home_str) {
                return format!("~{}", stripped);
            }
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::account::AccountSource;

    fn identity(dir: &Path) -> AccountIdentity {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        AccountIdentity {
            account: name.clone(),
            config_dir: dir.to_string_lossy().into_owned(),
            email: Some(format!("{name}@example.test")),
            org: None,
            credential_state: "ok".into(),
            expires_at: None,
        }
    }

    fn on(dir: &str) -> SessionAccount {
        SessionAccount {
            account_dir: Some(dir.rsplit('/').next().unwrap().to_string()),
            account_config_dir: Some(dir.to_string()),
            account_source: Some(AccountSource::Live),
            ..Default::default()
        }
    }

    #[test]
    fn every_account_reports_with_its_session_count_and_usage_even_at_zero() {
        let a = "/accts/a-main";
        let b = "/accts/b-main";
        let sessions: HashMap<String, SessionAccount> = [
            ("s1".to_string(), on(a)),
            ("s2".to_string(), on(a)),
            ("s3".to_string(), SessionAccount::default()),
        ]
        .into_iter()
        .collect();
        let usage: HashMap<String, UsageSnapshot> = [(
            b.to_string(),
            UsageSnapshot {
                fable_pct: Some(97),
                read_at: 5,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let rows = account_rows(
            vec![PathBuf::from(a), PathBuf::from(b)],
            &sessions,
            &usage,
            identity,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].identity.account.as_str(), rows[0].live_sessions),
            ("a-main", 2)
        );
        assert!(
            rows[0].usage.is_none(),
            "no daemon row -> unknown, never zero"
        );
        assert_eq!(
            (rows[1].identity.account.as_str(), rows[1].live_sessions),
            ("b-main", 0)
        );
        assert_eq!(rows[1].usage.as_ref().and_then(|u| u.fable_pct), Some(97));
        let json = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(json["email"], "b-main@example.test");
        assert_eq!(json["live_sessions"], 0);
        assert_eq!(json["usage"]["fable_pct"], 97);
        assert!(json.get("token").is_none() && json.get("access_token").is_none());
    }
}
