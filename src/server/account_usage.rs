//! Daemon-side account identity + usage: the `/status` Status and Usage tabs
//! of every live agent, read from the process that is actually running and
//! served on `/api/sessions` rows and `GET /api/accounts`.
//!
//! One supervised loop refreshes a process-wide cache every
//! [`POLL_INTERVAL`]; request handlers only read the cache. Each account's
//! usage endpoint is read at most once per [`USAGE_CACHE_TTL`], the bearer
//! token is read from disk at fetch time and dropped, and no error string
//! ever carries it.
//!
//! Two accounts are never read at all:
//!
//! * a **credential-file-only** account — one the `[accounts]` section of
//!   `config.toml` classes as management (exact name or prefix). Its
//!   identity comes from the credential file's expiry and nothing else: no
//!   token read, no request. The pass logs which accounts it skipped.
//! * an account **backing off** — the endpoint answered 429 and asked for a
//!   pause (`Retry-After`, at least [`USAGE_BACKOFF_MIN`]). Until the pause
//!   ends the account keeps its last good meters, with their own `read_at`
//!   as the age, and the row says why (`usage_error`, `usage_backoff_until`).
//!   A 429 is a wait, never a cap.
//!
//! Every request the loop makes is logged under the `server.accounts`
//! target, one line before and one after, naming the account and the
//! outcome: the request log is how "this account was never polled" is
//! proven, by the absence of its line.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, Json};
use serde::Serialize;

use super::state::AppState;
use crate::session::account::{
    self, AccountIdentity, SessionAccount, UsageSnapshot, USAGE_CACHE_TTL,
};
use crate::session::config::AccountsConfig;

pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// `usage_policy` of an account whose meters the daemon reads.
pub const USAGE_POLICY_METERS: &str = "meters";
/// `usage_policy` of a management account: the credential file's expiry is
/// the whole reading; no request is ever made for it.
pub const USAGE_POLICY_CREDENTIAL_FILE_ONLY: &str = "credential-file-only";

const LOG_TARGET: &str = "server.accounts";

/// One account dir as `GET /api/accounts` reports it — reported even with
/// zero live sessions, straight from its credentials dir.
#[derive(Debug, Clone, Serialize)]
pub struct AccountRow {
    #[serde(flatten)]
    pub identity: AccountIdentity,
    /// Sessions whose LIVE process runs on this account.
    pub live_sessions: usize,
    /// [`USAGE_POLICY_METERS`] or [`USAGE_POLICY_CREDENTIAL_FILE_ONLY`].
    pub usage_policy: &'static str,
    /// The last good meters (or, for a non-429 failure, the failed read).
    /// Always `None` for a credential-file-only account.
    pub usage: Option<UsageSnapshot>,
    /// Unix seconds until which the endpoint is not polled for this account,
    /// set while a 429's pause is running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_backoff_until: Option<u64>,
    /// What the most recent read attempt returned when it failed; `None`
    /// after a success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_error: Option<String>,
}

/// What the last pass learned about one session's process.
#[derive(Debug, Clone)]
pub struct LiveBinding {
    pub pane_pid: Option<u32>,
    /// The `CLAUDE_CONFIG_DIR` the running agent carries, canonical.
    pub live_dir: Option<PathBuf>,
}

#[derive(Default, Clone)]
pub struct AccountCache {
    /// Keyed by canonical config dir.
    pub accounts: HashMap<String, AccountRow>,
    /// Last successful-or-failed usage read per canonical config dir.
    pub fetched_at: HashMap<String, Instant>,
    /// Per canonical config dir: no read before this (a 429's pause).
    pub backoff_until: HashMap<String, Instant>,
    /// Per canonical config dir: the most recent failed read's error, kept
    /// until a read succeeds.
    pub last_error: HashMap<String, String>,
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

/// Read one account's usage from `endpoint`. Every failure becomes a
/// snapshot with `error` set to the HTTP status or transport class — never
/// the request. A 429 also carries the wait its `Retry-After` asked for.
pub async fn fetch_usage(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    read_at: u64,
) -> UsageSnapshot {
    let res = client
        .get(endpoint)
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
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok());
        let mut snap = account::usage_error(read_at, "http 429");
        snap.retry_after_secs = Some(account::retry_after_secs(retry_after));
        return snap;
    }
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
/// each session's live model vs pin (WO#1933), plus the `[accounts]` policy
/// as configured right now.
struct Pass {
    live: HashMap<String, LiveBinding>,
    record: HashMap<String, Option<PathBuf>>,
    identities: Vec<AccountIdentity>,
    models: HashMap<String, crate::session::model_state::SessionModel>,
    policy: AccountsConfig,
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
    // The policy is re-read every pass so an edit to `[accounts]` takes
    // effect at the next poll, without a restart.
    let policy = crate::session::Config::load_or_warn().accounts;
    Pass {
        live,
        record,
        identities,
        models,
        policy,
    }
}

/// One account's read this pass, as the merge step consumes it.
pub(crate) struct ReadOutcome {
    pub snapshot: UsageSnapshot,
    pub at: Instant,
    /// Set by a 429: no read of this account before this instant.
    pub backoff_until: Option<Instant>,
}

fn names(list: &mut [&str]) -> String {
    list.sort_unstable();
    list.join(",")
}

fn pct(v: Option<u32>) -> String {
    v.map_or_else(|| "?".to_string(), |p| p.to_string())
}

/// The usage reads one pass makes, given the identities the blocking pass
/// found and the cache as it stood: nothing for a credential-file-only
/// account (skipped before any token read), nothing for an account inside
/// a 429's pause, nothing inside the per-account TTL, one request for each
/// of the rest. `read_token` is the credential-file read, injected so a
/// test can prove which dirs were asked. Logs one line per request, one
/// per outcome, and one per pass naming what was skipped and why.
pub(crate) async fn read_due(
    client: &reqwest::Client,
    endpoint: &str,
    identities: &[AccountIdentity],
    policy: &AccountsConfig,
    prior: &AccountCache,
    now: Instant,
    read_token: &(dyn Fn(&Path) -> Option<String> + Sync),
) -> Vec<(String, ReadOutcome)> {
    if !policy.is_configured() {
        tracing::warn!(
            target: LOG_TARGET,
            "no [accounts] management list configured: every account dir is usage-polled"
        );
    }
    let mut management_skipped: Vec<&str> = Vec::new();
    let mut backing_off: Vec<&str> = Vec::new();
    let mut read: Vec<&str> = Vec::new();
    let mut due: Vec<&AccountIdentity> = Vec::new();
    for id in identities {
        if policy.is_management(&id.account) {
            management_skipped.push(&id.account);
            continue;
        }
        if prior
            .backoff_until
            .get(&id.config_dir)
            .is_some_and(|until| *until > now)
        {
            backing_off.push(&id.account);
            continue;
        }
        let fresh = prior
            .fetched_at
            .get(&id.config_dir)
            .is_some_and(|t| now.duration_since(*t) < USAGE_CACHE_TTL);
        if fresh {
            continue;
        }
        due.push(id);
    }

    let mut outcomes = Vec::with_capacity(due.len());
    for id in due {
        read.push(&id.account);
        let token = if id.credential_state == account::CRED_OK
            || id.credential_state == account::CRED_EXPIRED
        {
            read_token(Path::new(&id.config_dir))
        } else {
            None
        };
        let read_at = account::now_secs();
        tracing::info!(
            target: LOG_TARGET,
            "usage read: account={} request=GET usage endpoint",
            id.account
        );
        let snapshot = match token {
            Some(t) => fetch_usage(client, endpoint, &t, read_at).await,
            None => account::usage_error(read_at, format!("credentials {}", id.credential_state)),
        };
        let at = Instant::now();
        let mut backoff_until = None;
        match (&snapshot.error, snapshot.retry_after_secs) {
            (None, _) => tracing::info!(
                target: LOG_TARGET,
                "usage read: account={} http=200 session={} week={} fable={}",
                id.account,
                pct(snapshot.session_pct),
                pct(snapshot.week_pct),
                pct(snapshot.fable_pct)
            ),
            (Some(err), Some(wait)) => {
                let until = at + Duration::from_secs(wait);
                backoff_until = Some(until);
                let last_good_kept = prior
                    .accounts
                    .get(&id.config_dir)
                    .and_then(|r| r.usage.as_ref())
                    .is_some_and(|u| u.error.is_none());
                tracing::info!(
                    target: LOG_TARGET,
                    "usage read: account={} error=\"{}\" retry_after={}s backoff_until={} last_good_kept={}",
                    id.account,
                    err,
                    wait,
                    read_at + wait,
                    last_good_kept
                );
            }
            (Some(err), None) => tracing::info!(
                target: LOG_TARGET,
                "usage read: account={} error=\"{}\"",
                id.account,
                err
            ),
        }
        outcomes.push((
            id.config_dir.clone(),
            ReadOutcome {
                snapshot,
                at,
                backoff_until,
            },
        ));
    }
    tracing::info!(
        target: LOG_TARGET,
        "usage pass: read=[{}] backing_off=[{}] management_skipped=[{}] (credential-file-only, no request)",
        names(&mut read),
        names(&mut backing_off),
        names(&mut management_skipped)
    );
    outcomes
}

/// Fold one pass's identities and reads into the cache's account rows.
///
/// A credential-file-only account gets a row with no meters and no read
/// state, whatever the cache held for it before (the policy may have just
/// changed). A successful read replaces the meters and clears the error; a
/// 429 keeps the last good meters (their `read_at` is the age), records the
/// error and the pause; any other failure replaces the meters with the
/// failed read, as before, and records the error. An account not read this
/// pass keeps what it had.
pub(crate) fn merge_reads(
    cache: &mut AccountCache,
    identities: Vec<AccountIdentity>,
    live_counts: &HashMap<String, usize>,
    policy: &AccountsConfig,
    outcomes: Vec<(String, ReadOutcome)>,
    now: Instant,
) {
    let mut fresh: HashMap<String, ReadOutcome> = outcomes.into_iter().collect();
    let now_secs = account::now_secs();
    let mut accounts = HashMap::new();
    for identity in identities {
        let key = identity.config_dir.clone();
        let live_sessions = live_counts.get(&key).copied().unwrap_or(0);
        if policy.is_management(&identity.account) {
            cache.fetched_at.remove(&key);
            cache.backoff_until.remove(&key);
            cache.last_error.remove(&key);
            accounts.insert(
                key,
                AccountRow {
                    identity,
                    live_sessions,
                    usage_policy: USAGE_POLICY_CREDENTIAL_FILE_ONLY,
                    usage: None,
                    usage_backoff_until: None,
                    usage_error: None,
                },
            );
            continue;
        }
        let prior_usage = cache.accounts.get(&key).and_then(|r| r.usage.clone());
        let usage = match fresh.remove(&key) {
            Some(outcome) => {
                cache.fetched_at.insert(key.clone(), outcome.at);
                match (&outcome.snapshot.error, outcome.backoff_until) {
                    (None, _) => {
                        cache.last_error.remove(&key);
                        cache.backoff_until.remove(&key);
                        Some(outcome.snapshot)
                    }
                    (Some(err), Some(until)) => {
                        cache.last_error.insert(key.clone(), err.clone());
                        cache.backoff_until.insert(key.clone(), until);
                        match prior_usage {
                            Some(last_good) if last_good.error.is_none() => Some(last_good),
                            _ => Some(outcome.snapshot),
                        }
                    }
                    (Some(err), None) => {
                        cache.last_error.insert(key.clone(), err.clone());
                        cache.backoff_until.remove(&key);
                        Some(outcome.snapshot)
                    }
                }
            }
            None => prior_usage,
        };
        let usage_backoff_until = match cache.backoff_until.get(&key) {
            Some(until) if *until > now => Some(now_secs + (*until - now).as_secs()),
            Some(_) => {
                cache.backoff_until.remove(&key);
                None
            }
            None => None,
        };
        accounts.insert(
            key.clone(),
            AccountRow {
                usage_error: cache.last_error.get(&key).cloned(),
                identity,
                live_sessions,
                usage_policy: USAGE_POLICY_METERS,
                usage,
                usage_backoff_until,
            },
        );
    }
    cache.accounts = accounts;
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
            tracing::warn!(target: LOG_TARGET, error = %e, "account pass panicked");
            return;
        }
    };

    // Reads are decided against a snapshot of the previous cache so a slow
    // endpoint never doubles up and never holds the cache lock.
    let prior = state.account_cache.read().await.clone();
    let outcomes = read_due(
        client,
        account::USAGE_ENDPOINT,
        &pass.identities,
        &pass.policy,
        &prior,
        Instant::now(),
        &account::read_access_token,
    )
    .await;

    let mut live_counts: HashMap<String, usize> = HashMap::new();
    for b in pass.live.values() {
        if let Some(d) = &b.live_dir {
            *live_counts
                .entry(d.to_string_lossy().into_owned())
                .or_default() += 1;
        }
    }
    let mut cache = state.account_cache.write().await;
    merge_reads(
        &mut cache,
        pass.identities,
        &live_counts,
        &pass.policy,
        outcomes,
        Instant::now(),
    );
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
                    tracing::error!(target: LOG_TARGET, error = %e, "no http client; account usage disabled");
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Mutex;
    use tracing_test::traced_test;

    const TOKEN_WMW: &str = "fake-token-aoe-wmw-never-sent";
    const TOKEN_BP: &str = "fake-token-bp-test";

    /// What the stub endpoint answers.
    #[derive(Clone, Copy)]
    enum Answer {
        Ok,
        TooMany(Option<u64>),
    }

    struct Stub {
        /// Bearer of every request received, in order.
        seen: Mutex<Vec<String>>,
        answer: Mutex<Answer>,
    }

    async fn usage_handler(
        State(stub): State<Arc<Stub>>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();
        stub.seen.lock().unwrap().push(bearer);
        match *stub.answer.lock().unwrap() {
            Answer::Ok => (
                StatusCode::OK,
                Json(serde_json::json!({"limits": [
                    {"kind": "session", "percent": 62, "resets_at": "2026-09-19T00:00:00+00:00"},
                    {"kind": "weekly_all", "percent": 47, "resets_at": "2026-09-25T00:00:00+00:00"},
                    {"kind": "weekly_scoped", "percent": 92, "resets_at": "2026-09-25T00:00:00+00:00",
                     "scope": {"model": {"display_name": "Fable"}}}
                ]})),
            )
                .into_response(),
            Answer::TooMany(Some(secs)) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                "slow down",
            )
                .into_response(),
            Answer::TooMany(None) => (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response(),
        }
    }

    async fn stub(answer: Answer) -> (Arc<Stub>, String) {
        let stub = Arc::new(Stub {
            seen: Mutex::new(Vec::new()),
            answer: Mutex::new(answer),
        });
        let router = Router::new()
            .route("/usage", get(usage_handler))
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (stub, format!("http://{address}/usage"))
    }

    /// Two account dirs with live-looking credential files: `aoe-wmw`
    /// (management) and `bp-test` (polled). Returns the root and both
    /// identities in that order.
    fn accounts_root() -> (tempfile::TempDir, Vec<AccountIdentity>) {
        let root = tempfile::tempdir().unwrap();
        let now_ms = account::now_ms();
        let expires = now_ms + 3_600_000;
        let mut ids = Vec::new();
        for (name, token) in [("aoe-wmw", TOKEN_WMW), ("bp-test", TOKEN_BP)] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":{expires}}}}}"#),
            )
            .unwrap();
            std::fs::write(
                dir.join(".claude.json"),
                r#"{"oauthAccount":{"emailAddress":"x@example.com"}}"#,
            )
            .unwrap();
            ids.push(account::read_identity(&dir, now_ms));
        }
        assert_eq!(ids[0].account, "aoe-wmw");
        assert_eq!(ids[0].credential_state, account::CRED_OK);
        assert_eq!(ids[1].account, "bp-test");
        (root, ids)
    }

    fn policy() -> AccountsConfig {
        AccountsConfig {
            management: vec!["aoe-wmw".to_string()],
            ..Default::default()
        }
    }

    /// The credential-file read, recording which dirs were asked.
    fn recording_reader(asked: Arc<Mutex<Vec<String>>>) -> impl Fn(&Path) -> Option<String> {
        move |p: &Path| {
            asked
                .lock()
                .unwrap()
                .push(p.file_name().unwrap().to_string_lossy().into_owned());
            account::read_access_token(p)
        }
    }

    fn last_good(read_at: u64) -> UsageSnapshot {
        UsageSnapshot {
            session_pct: Some(10),
            week_pct: Some(20),
            fable_pct: Some(30),
            read_at,
            ..Default::default()
        }
    }

    fn seeded_cache(identity: &AccountIdentity, snap: UsageSnapshot) -> AccountCache {
        let mut cache = AccountCache::default();
        cache.accounts.insert(
            identity.config_dir.clone(),
            AccountRow {
                identity: identity.clone(),
                live_sessions: 0,
                usage_policy: USAGE_POLICY_METERS,
                usage: Some(snap),
                usage_backoff_until: None,
                usage_error: None,
            },
        );
        cache
    }

    fn no_token_in_logs(lines: &[&str]) -> Result<(), String> {
        match lines
            .iter()
            .find(|l| l.contains(TOKEN_WMW) || l.contains(TOKEN_BP))
        {
            Some(l) => Err(format!("a log line carries a token: {l}")),
            None => Ok(()),
        }
    }

    /// A management account is never token-read and never requested; the
    /// pass says so; the other account is read exactly once.
    #[traced_test]
    #[tokio::test]
    async fn management_account_is_never_read_or_requested() {
        tracing::callsite::rebuild_interest_cache();
        let (_root, ids) = accounts_root();
        let (stub, endpoint) = stub(Answer::Ok).await;
        let client = build_client().unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let reader = recording_reader(asked.clone());
        let mut cache = AccountCache::default();
        let now = Instant::now();

        let outcomes = read_due(&client, &endpoint, &ids, &policy(), &cache, now, &reader).await;

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].0, ids[1].config_dir);
        assert_eq!(*stub.seen.lock().unwrap(), vec![TOKEN_BP.to_string()]);
        assert_eq!(*asked.lock().unwrap(), vec!["bp-test".to_string()]);
        assert!(logs_contain("management_skipped=[aoe-wmw]"));
        assert!(logs_contain("usage pass: read=[bp-test] backing_off=[] management_skipped=[aoe-wmw] (credential-file-only, no request)"));
        assert!(logs_contain(
            "usage read: account=bp-test request=GET usage endpoint"
        ));
        assert!(logs_contain(
            "usage read: account=bp-test http=200 session=62 week=47 fable=92"
        ));
        assert!(!logs_contain("account=aoe-wmw request="));
        assert!(!logs_contain("no [accounts] management list configured"));
        logs_assert(no_token_in_logs);

        merge_reads(
            &mut cache,
            ids.clone(),
            &HashMap::new(),
            &policy(),
            outcomes,
            Instant::now(),
        );
        let wmw = &cache.accounts[&ids[0].config_dir];
        assert_eq!(wmw.usage_policy, USAGE_POLICY_CREDENTIAL_FILE_ONLY);
        assert!(
            wmw.usage.is_none() && wmw.usage_error.is_none() && wmw.usage_backoff_until.is_none()
        );
        let bp = &cache.accounts[&ids[1].config_dir];
        assert_eq!(bp.usage_policy, USAGE_POLICY_METERS);
        assert_eq!(bp.usage.as_ref().unwrap().session_pct, Some(62));
        assert!(bp.usage_error.is_none());
        let json = serde_json::to_string(&cache.accounts.values().collect::<Vec<_>>()).unwrap();
        assert!(json.contains("\"usage_policy\":\"credential-file-only\""));
        assert!(!json.contains(TOKEN_WMW) && !json.contains(TOKEN_BP));
    }

    /// An empty `[accounts]` polls everything and says so once per pass.
    #[traced_test]
    #[tokio::test]
    async fn unconfigured_policy_warns_and_polls_every_account() {
        tracing::callsite::rebuild_interest_cache();
        let (_root, ids) = accounts_root();
        let (stub, endpoint) = stub(Answer::Ok).await;
        let client = build_client().unwrap();
        let cache = AccountCache::default();
        let outcomes = read_due(
            &client,
            &endpoint,
            &ids,
            &AccountsConfig::default(),
            &cache,
            Instant::now(),
            &account::read_access_token,
        )
        .await;
        assert_eq!(outcomes.len(), 2);
        assert_eq!(stub.seen.lock().unwrap().len(), 2);
        assert!(logs_contain(
            "no [accounts] management list configured: every account dir is usage-polled"
        ));
        assert!(logs_contain("management_skipped=[]"));
    }

    /// A 429 with `Retry-After: 120` pauses that account for 120 s, keeps
    /// its last good meters (their own `read_at`), records the error, and
    /// the next pass makes no request for it.
    #[traced_test]
    #[tokio::test]
    async fn a_429_backs_off_for_retry_after_and_keeps_the_last_good_meters() {
        tracing::callsite::rebuild_interest_cache();
        let (_root, ids) = accounts_root();
        let (stub, endpoint) = stub(Answer::TooMany(Some(120))).await;
        let client = build_client().unwrap();
        let mut cache = seeded_cache(&ids[1], last_good(1_000));
        let now = Instant::now();

        let outcomes = read_due(
            &client,
            &endpoint,
            &ids,
            &policy(),
            &cache,
            now,
            &account::read_access_token,
        )
        .await;
        assert_eq!(outcomes.len(), 1);
        let outcome = &outcomes[0].1;
        assert_eq!(outcome.snapshot.error.as_deref(), Some("http 429"));
        assert_eq!(outcome.snapshot.retry_after_secs, Some(120));
        let until = outcome.backoff_until.expect("a 429 sets the pause");
        let wait = until.duration_since(now).as_secs();
        assert!((119..=121).contains(&wait), "backoff {wait}s");
        assert!(logs_contain(
            "usage read: account=bp-test error=\"http 429\" retry_after=120s backoff_until="
        ));
        assert!(logs_contain("last_good_kept=true"));

        merge_reads(
            &mut cache,
            ids.clone(),
            &HashMap::new(),
            &policy(),
            outcomes,
            Instant::now(),
        );
        let bp = &cache.accounts[&ids[1].config_dir];
        let usage = bp.usage.as_ref().unwrap();
        assert_eq!(
            (usage.read_at, usage.session_pct, usage.error.as_deref()),
            (1_000, Some(10), None)
        );
        assert_eq!(bp.usage_error.as_deref(), Some("http 429"));
        let backoff = bp.usage_backoff_until.unwrap();
        let now_secs = account::now_secs();
        assert!(
            (now_secs + 118..=now_secs + 121).contains(&backoff),
            "backoff_until {backoff} vs now {now_secs}"
        );

        // The next pass, inside the pause: no request, and the pass says why.
        let again = read_due(
            &client,
            &endpoint,
            &ids,
            &policy(),
            &cache,
            Instant::now(),
            &account::read_access_token,
        )
        .await;
        assert!(again.is_empty());
        assert_eq!(stub.seen.lock().unwrap().len(), 1, "one request in total");
        assert!(logs_contain(
            "usage pass: read=[] backing_off=[bp-test] management_skipped=[aoe-wmw]"
        ));
        logs_assert(no_token_in_logs);
    }

    /// No `Retry-After` on the 429 means the minimum pause.
    #[traced_test]
    #[tokio::test]
    async fn a_429_without_retry_after_backs_off_for_the_minimum() {
        tracing::callsite::rebuild_interest_cache();
        let (_root, ids) = accounts_root();
        let (_stub, endpoint) = stub(Answer::TooMany(None)).await;
        let client = build_client().unwrap();
        let cache = AccountCache::default();
        let now = Instant::now();
        let outcomes = read_due(
            &client,
            &endpoint,
            &ids,
            &policy(),
            &cache,
            now,
            &account::read_access_token,
        )
        .await;
        let outcome = &outcomes[0].1;
        assert_eq!(
            outcome.snapshot.retry_after_secs,
            Some(account::USAGE_BACKOFF_MIN.as_secs())
        );
        let wait = outcome.backoff_until.unwrap().duration_since(now).as_secs();
        assert!((59..=61).contains(&wait), "backoff {wait}s");
        assert!(logs_contain("retry_after=60s"));
        assert!(logs_contain("last_good_kept=false"));
    }

    /// The merge alone: a 429 outcome keeps the old snapshot untouched
    /// (its `read_at` is the age the row reports), a later success replaces
    /// it and clears the error and the pause, and a non-429 failure still
    /// replaces the meters with the failed read.
    #[test]
    fn merge_keeps_last_good_on_429_and_clears_on_success() {
        let (_root, ids) = accounts_root();
        let key = ids[1].config_dir.clone();
        let mut cache = seeded_cache(&ids[1], last_good(1_000));
        let now = Instant::now();
        let mut rate_limited = account::usage_error(2_000, "http 429");
        rate_limited.retry_after_secs = Some(300);
        merge_reads(
            &mut cache,
            ids.clone(),
            &HashMap::new(),
            &policy(),
            vec![(
                key.clone(),
                ReadOutcome {
                    snapshot: rate_limited,
                    at: now,
                    backoff_until: Some(now + Duration::from_secs(300)),
                },
            )],
            now,
        );
        let row = &cache.accounts[&key];
        assert_eq!(row.usage.as_ref().unwrap().read_at, 1_000);
        assert_eq!(row.usage.as_ref().unwrap().session_pct, Some(10));
        assert_eq!(row.usage_error.as_deref(), Some("http 429"));
        assert!(row.usage_backoff_until.is_some());
        assert!(cache.backoff_until.contains_key(&key));

        merge_reads(
            &mut cache,
            ids.clone(),
            &HashMap::new(),
            &policy(),
            vec![(
                key.clone(),
                ReadOutcome {
                    snapshot: last_good(3_000),
                    at: now,
                    backoff_until: None,
                },
            )],
            now,
        );
        let row = &cache.accounts[&key];
        assert_eq!(row.usage.as_ref().unwrap().read_at, 3_000);
        assert!(row.usage_error.is_none() && row.usage_backoff_until.is_none());
        assert!(!cache.backoff_until.contains_key(&key));

        merge_reads(
            &mut cache,
            ids.clone(),
            &HashMap::new(),
            &policy(),
            vec![(
                key.clone(),
                ReadOutcome {
                    snapshot: account::usage_error(4_000, "http 401"),
                    at: now,
                    backoff_until: None,
                },
            )],
            now,
        );
        let row = &cache.accounts[&key];
        assert_eq!(
            row.usage.as_ref().unwrap().error.as_deref(),
            Some("http 401")
        );
        assert_eq!(row.usage_error.as_deref(), Some("http 401"));
        assert!(row.usage_backoff_until.is_none());

        // A management account never carries meters, even if it did before
        // the policy named it.
        let mgmt = ids[0].config_dir.clone();
        cache.accounts.insert(
            mgmt.clone(),
            AccountRow {
                identity: ids[0].clone(),
                live_sessions: 0,
                usage_policy: USAGE_POLICY_METERS,
                usage: Some(last_good(5_000)),
                usage_backoff_until: None,
                usage_error: None,
            },
        );
        cache.fetched_at.insert(mgmt.clone(), now);
        merge_reads(
            &mut cache,
            ids.clone(),
            &HashMap::new(),
            &policy(),
            Vec::new(),
            now,
        );
        let row = &cache.accounts[&mgmt];
        assert_eq!(row.usage_policy, USAGE_POLICY_CREDENTIAL_FILE_ONLY);
        assert!(row.usage.is_none());
        assert!(!cache.fetched_at.contains_key(&mgmt));
    }
}
