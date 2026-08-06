//! `aoe on` / `aoe off` / `aoe power`: the master kill switch.
//!
//! One command, no session in the loop: `aoe off` flips the daemon's
//! authoritative power state and cancels every wakeup registered with it, so
//! OFF applies to arms that already existed, not only future ones. `aoe on`
//! flips it back. `aoe power` prints the current state.
//!
//! Daemon-first: the flip goes through `POST /api/power` so the running
//! daemon and every networked consumer see it immediately. When no daemon is
//! reachable the state file is written directly (the daemon adopts it at next
//! boot), and status reporting treats "unreachable" as OFF, never ON.

use anyhow::{bail, Result};
use clap::Args;

use crate::acp::client::{discovery, http::HttpClient};
use crate::server::power::{power_file_path, PowerRegistry};
use crate::session::config::ActivityConfig;

#[derive(Args)]
pub struct ActivityArgs {
    /// Class to read or set. Omit to list every class and its state.
    pub class: Option<String>,

    /// `on` or `off`. Omit to read the named class without changing it.
    pub state: Option<String>,
}

fn daemon_client() -> Option<HttpClient> {
    let endpoint = discovery::discover_local().ok()?;
    HttpClient::new(endpoint).ok()
}

fn print_power_value(v: &serde_json::Value) {
    let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("?");
    let live = v.get("live_wakes").and_then(|n| n.as_u64()).unwrap_or(0);
    println!("power: {state} (live wakes: {live})");
    for (key, label) in [
        ("cancelled_wakes", "cancelled wakes"),
        ("stopping_sessions", "stopping sessions"),
        ("restoring_sessions", "restoring sessions"),
    ] {
        if let Some(items) = v.get(key).and_then(|c| c.as_array()) {
            if items.is_empty() {
                println!("{label}: none");
            } else {
                println!("{label} ({}):", items.len());
                for id in items {
                    println!("  - {}", id.as_str().unwrap_or("?"));
                }
            }
        }
    }
}

pub async fn set(on: bool) -> Result<()> {
    let word = if on { "ON" } else { "OFF" };
    if let Some(client) = daemon_client() {
        match client.set_power(on).await {
            Ok(v) => {
                println!("aoe is {word} (daemon acknowledged)");
                print_power_value(&v);
                return Ok(());
            }
            Err(e) => {
                eprintln!("daemon reachable but flip failed: {e}");
            }
        }
    }
    // No daemon: write the persisted state directly. The daemon reads it at
    // next boot, and every fail-closed consumer already treats an
    // unreachable daemon as OFF.
    let reg = PowerRegistry::load_from_app_dir();
    let cancelled = reg.set(on, "cli-offline");
    println!("aoe is {word} (no daemon reachable; state file written, adopted at daemon start)");
    if !cancelled.is_empty() {
        println!("cancelled wakes ({}):", cancelled.len());
        for id in &cancelled {
            println!("  - {id}");
        }
    }
    Ok(())
}

/// `aoe activity [<class> [on|off]]` — the per-class half of the kill switch.
///
/// Reading and setting go through the same config the settings UI writes, so a
/// flip here and a flip in the settings view are the same fact, and it
/// survives a daemon restart because it lives in config.toml rather than in
/// daemon memory. Setting a class OFF also asks the daemon to cancel what is
/// already armed in it; an unreachable daemon still persists the flip, and
/// every consumer already reads an unreachable daemon as OFF.
pub async fn activity(args: ActivityArgs) -> Result<()> {
    let config = crate::session::config::Config::load_or_warn();
    let Some(class) = args.class else {
        let width = ActivityConfig::CLASSES
            .iter()
            .map(|c| c.len())
            .max()
            .unwrap_or(0);
        for name in ActivityConfig::CLASSES {
            let state = if config.activity.is_on(name) {
                "on"
            } else {
                "off"
            };
            println!("{name:<width$}  {state}");
        }
        return Ok(());
    };
    if !ActivityConfig::CLASSES.contains(&class.as_str()) {
        bail!(
            "unknown activity class {class:?}\nknown classes: {}",
            ActivityConfig::CLASSES.join(", ")
        );
    }
    let Some(state) = args.state else {
        let state = if config.activity.is_on(&class) {
            "on"
        } else {
            "off"
        };
        println!("{class}: {state}");
        return Ok(());
    };
    let on = match state.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => true,
        "off" | "false" | "no" | "0" => false,
        other => bail!("state must be `on` or `off`, got {other:?}"),
    };
    crate::session::config::update_config(|c| {
        c.activity.set(&class, on);
    })?;
    println!("{class}: {}", if on { "on" } else { "off" });

    if !on {
        // An OFF that only refuses future arms is the bug this replaced. Ask
        // the daemon to cancel what this class already has armed; with no
        // daemon, do it against the state file directly.
        let cancelled = match daemon_client() {
            Some(client) => client
                .cancel_activity_class(&class)
                .await
                .unwrap_or_default(),
            None => PowerRegistry::load_from_app_dir().cancel_class(&class),
        };
        if cancelled.is_empty() {
            println!("nothing was armed in this class");
        } else {
            println!("cancelled {} armed item(s):", cancelled.len());
            for id in &cancelled {
                println!("  - {id}");
            }
        }
    }
    Ok(())
}

pub async fn status() -> Result<()> {
    if let Some(client) = daemon_client() {
        if let Ok(v) = client.get_power().await {
            print_power_value(&v);
            return Ok(());
        }
    }
    let app_dir = crate::session::get_app_dir()?;
    println!(
        "daemon unreachable: effective power state is OFF for every fail-closed consumer \
         (state file: {})",
        power_file_path(&app_dir).display()
    );
    let reg = PowerRegistry::load_from_app_dir();
    println!(
        "state file says: {}",
        if reg.is_on() { "on" } else { "off" }
    );
    Ok(())
}
