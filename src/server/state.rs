//! `AppState`, the shared handle every request and background loop reads,
//! plus the caches hanging off it.

use crate::acp::protocol::AcpBroadcastFrame;
use crate::file_watch::{FileWatchService, SubscriptionHandle};
use crate::server::push::{PushState, StatusChange};
use crate::server::rate_limit::RateLimiter;
use crate::session::Instance;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;

use super::serve_snapshot::{
    FormFactorCounters, ReportedServeSignals, StructuredTelemetryCounters,
};
use super::token::TokenManager;
use crate::server::{login, session_service};

pub(super) const ACP_CHANNEL_CAPACITY: usize = 256;

/// Per-profile cleanup defaults with a refresh timestamp.
pub struct CleanupDefaultsCache {
    pub refreshed_at: std::time::Instant,
    pub entries: std::collections::HashMap<String, crate::daemon::CleanupDefaults>,
}

pub const CLEANUP_DEFAULTS_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long attachment bytes buffered for a queued prompt live before the hourly sweep
/// reclaims them.
pub(super) const PENDING_ATTACHMENT_TTL: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

impl CleanupDefaultsCache {
    pub fn stale(&self) -> bool {
        self.refreshed_at.elapsed() >= CLEANUP_DEFAULTS_TTL
    }
}

/// A cached branch-diff scan (`compute_changed_files`) with its refresh timestamp.
pub(super) struct ChangedFilesEntry {
    refreshed_at: std::time::Instant,
    files: Vec<crate::git::diff::DiffFile>,
}

pub const CHANGED_FILES_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

/// Per-profile entry tracking a live `FileWatchService` subscription and the
/// `tokio::spawn`ed forwarder that drains its receiver into `AppState::disk_changed`.
pub(crate) struct DiskWatchEntry {
    /// RAII guard from `subscribe_channel`.
    pub(super) handle: SubscriptionHandle,
    /// Abort handle for the forwarder task that drains the per-profile
    /// receiver into `disk_changed`.
    pub(super) forwarder: tokio::task::AbortHandle,
}

/// Whether the caller has applied tmux scrape (and suppression) to `fresh.status`.
/// `status_poll_loop` passes `TmuxApplied`; the watcher consumer passes `DiskOnly`.
#[derive(Copy, Clone, Debug)]
pub(crate) enum StatusSource {
    /// Caller already scraped tmux into `fresh.status` and applied `recently_restarted`
    /// suppression.
    TmuxApplied,
    /// `fresh` was loaded from disk only.
    DiskOnly,
}

/// Shared application state accessible by all request handlers.
/// Terminal record of one daemon-owned restart cascade (see
/// `AppState::restart_outcomes`).
#[derive(Debug, Clone)]
pub struct RestartOutcomeRecord {
    pub ok: bool,
    pub error: Option<String>,
    /// Unix seconds at which the cascade reached its terminal state.
    pub finished_at: i64,
    /// Profile the cascade persisted into.
    pub profile: String,
}

pub struct AppState {
    pub profile: String,
    pub read_only: bool,
    /// CityHall client mode, resolved once at launch from `AOE_CITYHALL_MODE`.
    pub cityhall_mode: bool,
    pub instances: Arc<RwLock<Vec<Instance>>>,
    /// Session-domain service handle sharing `instances`, `instance_locks`, `file_watch`,
    /// the telemetry create counter, and the ACP supervisor with the fields on this struct,
    /// so a non-HTTP caller (the plugin host, #2897) can drive session create/turn without
    /// holding `AppState`.
    pub session_service: Arc<session_service::SessionService>,
    pub token_manager: Arc<TokenManager>,
    pub login_manager: Arc<login::LoginManager>,
    pub rate_limiter: Arc<RateLimiter>,
    pub behind_tunnel: bool,
    /// Coarse auth mode resolved once at launch (`"token"` / `"passphrase"` / `"none"`).
    pub auth_mode: &'static str,
    /// Coarse exposure mode resolved once at launch from the active transport (`"tunnel"` /
    /// `"tailscale"` / `"local"`), fed to the telemetry snapshot.
    pub serve_mode: &'static str,
    /// DNS-rebinding gate.
    pub allowed_hosts: Vec<String>,
    /// DNS-rebinding gate.
    pub allowed_origins: Vec<String>,
    /// Per-instance mutex guarding mutations that must not interleave (e.g.
    pub instance_locks: Arc<RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Per-`idempotency_key` mutex serializing `POST /api/sessions` create requests that
    /// share a key, so two concurrent retries with the same key can't both scan-miss the
    /// existing-instance check and both create a session.
    pub idempotency_locks:
        Arc<RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Disk config resolutions performed by `list_sessions`, one per unique `(profile,
    /// project_path)` per request, accumulated monotonically.
    pub list_sessions_resolver_misses: std::sync::atomic::AtomicUsize,
    /// Session ids with an in-flight smart-rename one-shot, so a burst of rapid first
    /// prompts cannot spawn concurrent title generators for the same session.
    pub smart_rename_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Session ids that have already had a smart-rename one-shot attempt this process
    /// lifetime (success or failure).
    pub smart_rename_attempted: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Global cap on concurrent smart-rename one-shots so a burst of new sessions cannot
    /// fan out into N host processes each holding a slot for up to `ONESHOT_TIMEOUT`.
    pub smart_rename_semaphore: tokio::sync::Semaphore,
    /// Session ids with an in-flight conversation-summary one-shot, so the automatic
    /// trigger and the on-demand endpoint cannot spawn concurrent summaries for the same
    /// session (which would also race on the last-summary seq).
    pub summary_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Global cap on concurrent conversation-summary one-shots.
    pub summary_semaphore: tokio::sync::Semaphore,
    /// Suppression set for the startup-recovery cascade.
    pub recently_restarted: crate::session::recovery::RecentlyRestarted,
    /// Session ids with an in-flight daemon-owned restart cascade
    /// (`POST /api/sessions/{id}/restart`), each with the instant its mark
    /// was taken. A repeat POST while the detached cascade is still running
    /// returns 202 `already_restarting` instead of racing a second kill+start
    /// against the first; a mark older than the admission TTL is treated as
    /// leaked (a cascade that died without its finish path) and replaced, so
    /// a vanished cascade can never wedge the endpoint into silent
    /// `already_restarting` forever. Synchronous mutex: critical sections
    /// are tiny and never span an `await`.
    pub restart_inflight: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// Terminal outcome of the LAST daemon-owned restart cascade per session
    /// id. The `status` a poller samples from the list is overwritten within
    /// ~500ms by the hook poller, so a CLI watching it can read a cascade
    /// that never ran as restarted, or a transient blip as failure.
    /// `GET /api/sessions/{id}/restart-status` serves this record instead:
    /// written exactly once per cascade, at its true end, and never touched
    /// by status detection. Synchronous mutex: tiny critical sections, never
    /// spans an `await`.
    pub restart_outcomes: std::sync::Mutex<std::collections::HashMap<String, RestartOutcomeRecord>>,
    /// Bumped under the `instances` write lock by any change an earlier disk snapshot
    /// would not carry, so a reload holding that snapshot drops itself.
    pub mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Ids whose startup-recovery cascade is scheduled but not yet complete.
    pub recovery_pending: crate::session::recovery::RecoveryPending,
    /// Host and per-agent resource sampler behind the system-health endpoint.
    /// Held across requests because CPU is a delta against the previous
    /// sample: a fresh sampler reports CPU as unknown until its second tick.
    pub(crate) metrics_sampler: tokio::sync::Mutex<crate::process::metrics::MetricsSampler>,
    /// Cached per-profile cleanup defaults for the delete dialog, with a timestamp so we
    /// re-resolve after config changes (see `CLEANUP_DEFAULTS_TTL`).
    pub cleanup_defaults_cache: RwLock<CleanupDefaultsCache>,
    /// Cached (owner, host-scoped key) per repo path.
    pub remote_owner_cache: RwLock<std::collections::HashMap<String, Option<(String, String)>>>,
    /// Short-TTL cache of `compute_changed_files` keyed by `(repo_path,
    /// base_branch)`, shared by the file-list and per-file diff endpoints so a
    /// burst of file switches reuses one branch scan. See `ChangedFilesEntry`.
    pub(super) changed_files_cache:
        std::sync::RwLock<std::collections::HashMap<(String, String), ChangedFilesEntry>>,
    /// Broadcasts session status transitions to consumers (currently the push-notification
    /// module).
    pub status_tx: broadcast::Sender<StatusChange>,
    /// Web Push state.
    pub push: Option<Arc<PushState>>,
    /// Cached value of `web.notifications_enabled` at startup.
    pub push_enabled: bool,
    /// Snapshot of the resolved WebConfig at startup.
    pub web_config: crate::session::config::WebConfig,
    /// Cross-board relay ingress, resolved once at launch from the `[relay]`
    /// config section. `None` = unconfigured (no secret file, unreadable, or
    /// empty after trimming); `POST /api/relay` then returns 404. Carries the
    /// shared ingress secret — compared constant-time in the handler and
    /// never logged.
    pub relay: Option<RelayIngress>,
    /// Broadcasts acp events to subscribed WebSocket clients.
    pub acp_events_tx: broadcast::Sender<AcpBroadcastFrame>,
    /// Disk-backed acp event log.
    pub acp_event_store: Arc<crate::acp::event_store::EventStore>,
    /// Live control-state projection per session, folded at the publish choke point and
    /// shared with `ChannelSink`.
    pub acp_control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
    /// Owns the per-session ACP agent subprocesses.
    pub acp_supervisor:
        Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    /// The Tier 1 plugin worker host.
    pub plugin_host: Option<Arc<crate::plugin::host::PluginHost>>,
    /// Tracks in-flight web plugin install / update / uninstall jobs so the dashboard can
    /// tail their host-side log.
    pub plugin_jobs: Arc<crate::server::api::plugins::PluginJobRegistry>,
    /// Per-browser foreground dashboard presence.
    pub web_presence: std::sync::Mutex<std::collections::HashMap<[u8; 32], i64>>,
    /// Packed sleep-inhibit reconciler snapshot for read-only status reporting.
    pub sleep_inhibit_snapshot: std::sync::atomic::AtomicU8,
    /// Allowlisted usage-signal counters.
    pub telemetry_usage_seen: crate::telemetry::usage_signals::UsageSeenCounters,
    /// Per-form-factor open counts for the web dashboard / acp, layered on the `usage_seen`
    /// registry counts above so the snapshot can report which client classes (desktop /
    /// mobile / PWA) used each surface.
    pub telemetry_web_clients: FormFactorCounters,
    pub telemetry_structured_clients: FormFactorCounters,
    /// Sessions created since the last opt-in telemetry snapshot.
    pub telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
    /// Aggregate structured-interaction tallies for the next opt-in snapshot (approvals
    /// decision mix, agent/substrate switches, plan-mode, queued prompts).
    pub telemetry_structured: StructuredTelemetryCounters,
    /// What the most recent serve snapshot reported, held until its send is confirmed so
    /// the originating signals (the `usage_seen` counts and the create counter) are cleared
    /// only on success.
    pub(super) telemetry_last_reported: std::sync::Mutex<Option<ReportedServeSignals>>,
    /// Resolved when the daemon receives SIGINT/SIGTERM/SIGHUP.
    pub shutdown: CancellationToken,
    /// Live account identity + usage per config dir and per session, refreshed
    /// by `account_usage::spawn_account_usage_loop`. Read by `/api/sessions`
    /// and `/api/accounts`. See WO#1852.
    pub account_cache: Arc<RwLock<super::account_usage::AccountCache>>,
    /// Process-wide file-watch primitive.
    pub(crate) file_watch: Arc<FileWatchService>,
    /// Wakeup signal for `disk_watcher_consumer`.
    pub(crate) disk_changed: Arc<tokio::sync::Notify>,
    /// Per-profile disk-watch subscriptions plus their forwarder tasks.
    pub(crate) disk_watch_handles:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, DiskWatchEntry>>>,
}

/// Runtime half of the `[relay]` config section: the resolved ingress
/// policy for `POST /api/relay`. Built once at daemon launch by
/// [`RelayIngress::from_config`]; immutable for the daemon's lifetime, like
/// `auth_mode`.
pub struct RelayIngress {
    /// Shared bearer secret peers must present. Loaded from
    /// `[relay] secret_file`, whitespace-trimmed. Scoped to `/api/relay`
    /// only — it opens no other endpoint.
    pub secret: String,
    /// Title of the session that receives untargeted relays. Already
    /// defaulted: empty config resolves to "AoE-Commander" here.
    pub commander_title: String,
    /// Whether inbound relays may name an explicit non-commander target.
    pub allow_worker_targets: bool,
}

/// Default target title for untargeted relays when `[relay] commander_title`
/// is unset.
pub const DEFAULT_COMMANDER_TITLE: &str = "AoE-Commander";

impl RelayIngress {
    /// Resolve the ingress policy from config. Fail-closed: any problem with
    /// the secret file (unset, unreadable, empty after trimming) disables
    /// ingress with a warning rather than starting an unauthenticated or
    /// misconfigured relay surface.
    pub fn from_config(cfg: &crate::session::config::RelayConfig) -> Option<Self> {
        let path = cfg.secret_file.as_deref()?;
        let secret = match std::fs::read_to_string(path) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                tracing::warn!(
                    target: "http.relay",
                    "relay disabled: cannot read secret_file {path}: {e}"
                );
                return None;
            }
        };
        if secret.is_empty() {
            tracing::warn!(
                target: "http.relay",
                "relay disabled: secret_file {path} is empty"
            );
            return None;
        }
        let commander_title = if cfg.commander_title.is_empty() {
            DEFAULT_COMMANDER_TITLE.to_string()
        } else {
            cfg.commander_title.clone()
        };
        Some(Self {
            secret,
            commander_title,
            allow_worker_targets: cfg.allow_worker_targets,
        })
    }
}

impl AppState {
    /// Read-through cache over `compute_changed_files`.
    pub fn changed_files_cached(
        &self,
        repo_path: &std::path::Path,
        base_branch: &str,
    ) -> crate::git::error::Result<Vec<crate::git::diff::DiffFile>> {
        let key = (
            repo_path.to_string_lossy().into_owned(),
            base_branch.to_string(),
        );
        if let Ok(cache) = self.changed_files_cache.read() {
            if let Some(entry) = cache.get(&key) {
                if entry.refreshed_at.elapsed() < CHANGED_FILES_TTL {
                    return Ok(entry.files.clone());
                }
            }
        }
        let files = crate::git::diff::compute_changed_files(repo_path, base_branch)?;
        if let Ok(mut cache) = self.changed_files_cache.write() {
            // Drop expired entries while we hold the write lock so the map can't
            // grow without bound across stale (repo, base) combinations.
            cache.retain(|_, e| e.refreshed_at.elapsed() < CHANGED_FILES_TTL);
            cache.insert(
                key,
                ChangedFilesEntry {
                    refreshed_at: std::time::Instant::now(),
                    files: files.clone(),
                },
            );
        }
        Ok(files)
    }

    /// Get or create the per-instance serialization mutex.
    pub async fn instance_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        instance_lock_in(&self.instance_locks, id).await
    }

    /// Get or create the per-idempotency-key serialization mutex.
    pub async fn idempotency_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        {
            let guard = self.idempotency_locks.read().await;
            if let Some(lock) = guard.get(key) {
                return lock.clone();
            }
        }
        let mut guard = self.idempotency_locks.write().await;
        // Drop keys nobody is using.
        guard.retain(|_, lock| Arc::strong_count(lock) > 1);
        guard
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Record whether one browser is currently foregrounded.
    pub fn set_web_presence(&self, client: [u8; 32], active: bool) {
        let mut presence = self.web_presence.lock().expect("web_presence poisoned");
        if active {
            presence.insert(client, crate::util::now_ms() as i64);
        } else {
            presence.remove(&client);
        }
    }

    /// Returns true if any dashboard recently reported itself visible and focused.
    pub fn web_active_within(&self, threshold: std::time::Duration) -> bool {
        let now = crate::util::now_ms() as i64;
        let max_age = threshold.as_millis() as i64;
        let mut presence = self.web_presence.lock().expect("web_presence poisoned");
        presence.retain(|_, last| now.saturating_sub(*last) < max_age);
        !presence.is_empty()
    }
}

/// Get or create the per-instance serialization mutex in `locks`.
pub(super) async fn instance_lock_in(
    locks: &RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    {
        let guard = locks.read().await;
        if let Some(lock) = guard.get(id) {
            return lock.clone();
        }
    }
    let mut guard = locks.write().await;
    guard
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(test)]
mod tests {
    use crate::server::test_support;

    /// `idempotency_locks` must not grow for the daemon's lifetime.
    #[tokio::test]
    async fn idempotency_lock_prunes_unreferenced_keys() {
        let state = test_support::build_test_app_state(vec![]);

        // A key acquired and released leaves nothing behind.
        drop(state.idempotency_lock("released-key").await);
        let _other = state.idempotency_lock("other-key").await;
        assert!(
            !state
                .idempotency_locks
                .read()
                .await
                .contains_key("released-key"),
            "an unreferenced key must be pruned rather than retained forever"
        );

        // A key still held by a live caller must NOT be pruned, or two
        // concurrent same-key creates would stop serializing.
        let held = state.idempotency_lock("held-key").await;
        let _guard = held.lock_owned().await;
        let _third = state.idempotency_lock("third-key").await;
        assert!(
            state
                .idempotency_locks
                .read()
                .await
                .contains_key("held-key"),
            "a key with a live holder must survive pruning"
        );
    }
}
