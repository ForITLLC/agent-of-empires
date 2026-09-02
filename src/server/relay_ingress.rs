//! Runtime half of the `[relay]` config section (WO#1584 relay, ported to
//! this stack's monolithic `server/mod.rs` layout — upstream keeps it in `state.rs`).

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
