//! Daemon-side account identity + usage: the `/status` Status and Usage tabs
//! of every live agent, read from the process that is actually running and
//! served on `/api/sessions` rows and `GET /api/accounts`.
//!
//! One supervised loop refreshes a process-wide cache every
//! [`POLL_INTERVAL`]; request handlers only read the cache. Each account's
//! usage endpoint is read at most once per [`USAGE_CACHE_TTL`], the bearer
//! token is read from disk at fetch time and dropped, and no error string
//! ever carries it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, Json};
use serde::Serialize;

use super::state::AppState;
use crate::session::account::{
    self, AccountIdentity, SessionAccount, UsageSnapshot, USAGE_CACHE_TTL,
};

pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// One account dir as `GET /api/accounts` reports it — reported even with
/// zero live sessions, straight from its credentials dir.
#[derive(Debug, Clone, Serialize)]
pub struct AccountRow {
    #[serde(flatten)]
    pub identity: AccountIdentity,
    /// Sessions whose LIVE process runs on this account.
    pub live_sessions: usize,
    pub usage: Option<UsageSnapshot>,
}

/// What the last pass learned about one session's process.
#[derive(Debug, Clone)]
pub struct LiveBinding {
    pub pane_pid: Option<u32>,
    /// The `CLAUDE_CONFIG_DIR` the running agent carries, canonical.
    pub live_dir: Option<PathBuf>,
}

#[derive(Default)]
pub struct AccountCache {
    /// Keyed by canonical config dir.
    pub accounts: HashMap<String, AccountRow>,
    /// Last successful-or-failed usage read per canonical config dir.
    pub fetched_at: HashMap<String, Instant>,
    /// Per session id.
    pub live: HashMap<String, LiveBinding>,
    /// Per profile name: the config dir its `environment` binds (canonical).
    pub record: HashMap<String, Option<PathBuf>>,
    /// Per session id: live model vs pin (WO#1933), read on the same pass.
    pub models: HashMap<String, crate::session::model_state::SessionModel>,
    pub updated_at: Option<u64>,
}

#[derive(Serialize)]
pub struct AccountsEnvelope {
    pub updated_at: Option<u64>,
    pub poll_interval_secs: u64,
    pub accounts: Vec<AccountRow>,
}

pub fn build_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("aoe/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// Read one account's usage. Every failure becomes a snapshot with `error`
/// set to the HTTP status or transport class — never the request.
pub async fn fetch_usage(client: &reqwest::Client, token: &str, read_at: u64) -> UsageSnapshot {
    let res = client
        .get(account::USAGE_ENDPOINT)
        .bearer_auth(token)
        .header("anthropic-beta", account::OAUTH_BETA)
        .send()
        .await;
    let res = match res {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return account::usage_error(read_at, "timeout"),
        Err(e) if e.is_connect() => return account::usage_error(read_at, "connect"),
        Err(_) => return account::usage_error(read_at, "transport"),
    };
    let status = res.status();
    if !status.is_success() {
        return account::usage_error(read_at, format!("http {}", status.as_u16()));
    }
    match res.json::<serde_json::Value>().await {
        Ok(body) => account::parse_usage(&body, read_at),
        Err(_) => account::usage_error(read_at, "bad body"),
    }
}

/// What one session contributes to the blocking pass: identity for the
/// account walk, plus the record-side model inputs (WO#1933).
pub struct PassRow {
    pub id: String,
    pub title: String,
    pub profile: String,
    pub tool: String,
    pub project_path: String,
    pub agent_session_id: Option<String>,
    pub agent_model: Option<String>,
    pub extra_args: String,
}

/// A blocking snapshot of the fleet: every claude session's record binding
/// and live config dir, plus the identity of every account dir found, plus
/// each session's live model vs pin (WO#1933).
struct Pass {
    live: HashMap<String, LiveBinding>,
    record: HashMap<String, Option<PathBuf>>,
    identities: Vec<AccountIdentity>,
    models: HashMap<String, crate::session::model_state::SessionModel>,
}

fn blocking_pass(rows: Vec<PassRow>) -> Pass {
    use crate::session::model_state;
    let now_ms = account::now_ms();
    let mut record: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut live: HashMap<String, LiveBinding> = HashMap::new();
    let mut models: HashMap<String, model_state::SessionModel> = HashMap::new();
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    // Per (profile, tool): the `session.agent_extra_args.<tool>` string,
    // whose `--model` flag is the profile pin. Resolved once per pass.
    let mut profile_args: HashMap<(String, String), Option<String>> = HashMap::new();
    let pane_metadata = crate::tmux::batch_pane_metadata().unwrap_or_default();
    for row in rows {
        if !account::tool_has_config_dir(&row.tool) {
            continue;
        }
        let bound = record
            .entry(row.profile.clone())
            .or_insert_with(|| account::record_config_dir(&row.profile, &row.tool))
            .clone();
        if let Some(b) = &bound {
            dirs.insert(b.clone());
        }
        let pane_pid = crate::tmux::Session::new(&row.id, &row.title)
            .ok()
            .and_then(|s| pane_metadata.get(s.name()).and_then(|m| m.pane_pid));
        let live_dir = pane_pid
            .and_then(account::live_config_dir)
            .map(|p| account::canonical_or_self(&p));
        if let Some(l) = &live_dir {
            dirs.insert(l.clone());
        }
        // WO#1933: pin from the record, else the profile; live from the
        // transcript under the dir the process really runs on (else the
        // record's, else the default).
        let pargs = profile_args
            .entry((row.profile.clone(), row.tool.clone()))
            .or_insert_with(|| {
                crate::session::config::profile_config::resolve_config_or_warn(&row.profile)
                    .session
                    .agent_extra_args
                    .get(&row.tool)
                    .cloned()
            })
            .clone();
        let pin = model_state::model_pin(
            row.agent_model.as_deref(),
            &row.extra_args,
            pargs.as_deref(),
        );
        let transcript_dir = live_dir
            .clone()
            .or_else(|| bound.clone())
            .unwrap_or_else(account::default_config_dir);
        let live_model = row
            .agent_session_id
            .as_deref()
            .map(|sid| model_state::transcript_path(&transcript_dir, &row.project_path, sid))
            .and_then(|p| model_state::read_live_model(&p));
        models.insert(row.id.clone(), model_state::session_model(pin, live_model));
        live.insert(row.id, LiveBinding { pane_pid, live_dir });
    }
    let identities = account::discover_account_dirs(dirs)
        .into_iter()
        .map(|d| account::read_identity(&d, now_ms))
        .collect();
    Pass {
        live,
        record,
        identities,
        models,
    }
}

/// One refresh: blocking identity/process pass, then the usage reads that
/// are due, then a single cache swap.
pub async fn refresh_once(state: &Arc<AppState>, client: &reqwest::Client) {
    let rows: Vec<PassRow> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| !i.is_trashed())
            .map(|i| PassRow {
                id: i.id.clone(),
                title: i.title.clone(),
                profile: i.effective_profile(),
                tool: i.tool.clone(),
                project_path: i.project_path.clone(),
                agent_session_id: i.agent_session_id.clone(),
                agent_model: i.agent_model.clone(),
                extra_args: i.extra_args.clone(),
            })
            .collect()
    };
    let pass = match tokio::task::spawn_blocking(move || blocking_pass(rows)).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "server.accounts", error = %e, "account pass panicked");
            return;
        }
    };

    // Usage reads due this pass (TTL per account), decided against the
    // previous cache so a slow endpoint never doubles up.
    let due: Vec<(String, Option<String>, String)> = {
        let cache = state.account_cache.read().await;
        pass.identities
            .iter()
            .filter(|id| {
                cache
                    .fetched_at
                    .get(&id.config_dir)
                    .is_none_or(|t| t.elapsed() >= USAGE_CACHE_TTL)
            })
            .map(|id| {
                let token = if id.credential_state == account::CRED_OK
                    || id.credential_state == account::CRED_EXPIRED
                {
                    account::read_access_token(std::path::Path::new(&id.config_dir))
                } else {
                    None
                };
                (id.config_dir.clone(), token, id.credential_state.clone())
            })
            .collect()
    };
    let mut fresh: HashMap<String, (UsageSnapshot, Instant)> = HashMap::new();
    for (dir, token, cred_state) in due {
        let read_at = account::now_secs();
        let snap = match token {
            Some(t) => fetch_usage(client, &t, read_at).await,
            None => account::usage_error(read_at, format!("credentials {cred_state}")),
        };
        fresh.insert(dir, (snap, Instant::now()));
    }

    let mut cache = state.account_cache.write().await;
    let mut live_counts: HashMap<String, usize> = HashMap::new();
    for b in pass.live.values() {
        if let Some(d) = &b.live_dir {
            *live_counts
                .entry(d.to_string_lossy().into_owned())
                .or_default() += 1;
        }
    }
    let mut accounts = HashMap::new();
    for identity in pass.identities {
        let key = identity.config_dir.clone();
        let usage = match fresh.remove(&key) {
            Some((snap, at)) => {
                cache.fetched_at.insert(key.clone(), at);
                Some(snap)
            }
            None => cache.accounts.get(&key).and_then(|r| r.usage.clone()),
        };
        accounts.insert(
            key.clone(),
            AccountRow {
                live_sessions: live_counts.get(&key).copied().unwrap_or(0),
                identity,
                usage,
            },
        );
    }
    cache.accounts = accounts;
    cache.live = pass.live;
    cache.record = pass.record;
    cache.models = pass.models;
    cache.updated_at = Some(account::now_secs());
}

/// Every [`POLL_INTERVAL`], after an immediate first pass so the first
/// `/api/sessions` read after startup already carries identities.
pub fn spawn_account_usage_loop(state: Arc<AppState>) {
    let shutdown = state.shutdown.clone();
    crate::task_util::spawn_supervised(
        "server.account_usage",
        crate::task_util::PanicPolicy::Log,
        async move {
            let client = match build_client() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(target: "server.accounts", error = %e, "no http client; account usage disabled");
                    return;
                }
            };
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => refresh_once(&state, &client).await,
                    _ = shutdown.cancelled() => break,
                }
            }
        },
    );
}

/// The per-session account view from the cache. Record and live are
/// compared by account; `usage` is the account's cached read.
pub fn session_account(
    cache: &AccountCache,
    id: &str,
    profile: &str,
    tool: &str,
) -> SessionAccount {
    if !account::tool_has_config_dir(tool) {
        return SessionAccount::default();
    }
    let record = cache.record.get(profile).cloned().flatten();
    let live = cache.live.get(id).and_then(|b| b.live_dir.clone());
    account::session_account_from_parts(
        record.as_deref(),
        live.as_deref(),
        |dir| {
            let key = account::canonical_or_self(dir)
                .to_string_lossy()
                .into_owned();
            cache
                .accounts
                .get(&key)
                .map(|r| r.identity.clone())
                .unwrap_or_else(|| account::read_identity(dir, account::now_ms()))
        },
        |dir| {
            let key = account::canonical_or_self(dir)
                .to_string_lossy()
                .into_owned();
            cache.accounts.get(&key).and_then(|r| r.usage.clone())
        },
    )
}

/// The per-session model view from the cache (WO#1933): what the last
/// pass read from the record, the profile pin and the transcript tail.
/// Empty (drift `false`) for a session the pass has not seen yet.
pub fn session_model(cache: &AccountCache, id: &str) -> crate::session::model_state::SessionModel {
    cache.models.get(id).cloned().unwrap_or_default()
}

/// `GET /api/accounts`
pub async fn list_accounts(State(state): State<Arc<AppState>>) -> Json<AccountsEnvelope> {
    let cache = state.account_cache.read().await;
    let mut accounts: Vec<AccountRow> = cache.accounts.values().cloned().collect();
    accounts.sort_by(|a, b| a.identity.account.cmp(&b.identity.account));
    Json(AccountsEnvelope {
        updated_at: cache.updated_at,
        poll_interval_secs: POLL_INTERVAL.as_secs(),
        accounts,
    })
}
