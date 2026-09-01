//! `aoe relay` — send a message to a peer board's daemon.
//!
//! The outbound half of the cross-board relay: reads the peer's URL and
//! ingress secret from `[[relay.boards]]` in config.toml and posts to the
//! peer's `POST /api/relay`. No local daemon is involved — this is a plain
//! HTTP client, so it works from any machine that can reach the peer's URL
//! (typically a tailnet hostname). The receiving board decides the target:
//! by default its commander session; `--to` requires the peer to have
//! `allow_worker_targets` on.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::session::config::Config;

#[derive(Args)]
pub struct RelayArgs {
    /// Peer board name, from `[[relay.boards]]` in config.toml
    board: String,

    /// Message to deliver
    message: String,

    /// Explicit target session title on the peer board (default: the peer's
    /// commander session; the peer must allow worker targets)
    #[arg(long)]
    to: Option<String>,

    /// Sending session id/title to announce in the provenance prefix
    #[arg(long)]
    from_session: Option<String>,
}

pub async fn run(args: RelayArgs) -> Result<()> {
    let config = Config::load_or_warn();
    let relay = &config.relay;

    let Some(board) = relay.boards.iter().find(|b| b.name == args.board) else {
        if relay.boards.is_empty() {
            bail!(
                "no peer boards configured; add a [[relay.boards]] entry \
                 (name, url, secret_file) to config.toml"
            );
        }
        let known: Vec<&str> = relay.boards.iter().map(|b| b.name.as_str()).collect();
        bail!(
            "unknown board '{}'; configured boards: {}",
            args.board,
            known.join(", ")
        );
    };

    let secret = std::fs::read_to_string(&board.secret_file)
        .with_context(|| format!("cannot read secret_file {}", board.secret_file))?
        .trim()
        .to_string();
    if secret.is_empty() {
        bail!("secret_file {} is empty", board.secret_file);
    }

    let url = format!("{}/api/relay", board.url.trim_end_matches('/'));
    let body = serde_json::json!({
        "message": args.message,
        "from_board": relay.board_name.clone().unwrap_or_else(|| "unnamed-board".to_string()),
        "from_session": args.from_session,
        "to": args.to,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = client
        .post(&url)
        .bearer_auth(&secret)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url} failed"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        println!("relayed to '{}': {}", args.board, text.trim());
        Ok(())
    } else {
        bail!(
            "relay to '{}' failed: HTTP {status}: {}",
            args.board,
            text.trim()
        );
    }
}
