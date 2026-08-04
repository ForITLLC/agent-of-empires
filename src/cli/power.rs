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

use anyhow::Result;

use crate::acp::client::{discovery, http::HttpClient};
use crate::server::power::{power_file_path, PowerRegistry};

fn daemon_client() -> Option<HttpClient> {
    let endpoint = discovery::discover_local().ok()?;
    HttpClient::new(endpoint).ok()
}

fn print_power_value(v: &serde_json::Value) {
    let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("?");
    let live = v.get("live_wakes").and_then(|n| n.as_u64()).unwrap_or(0);
    println!("power: {state} (live wakes: {live})");
    if let Some(cancelled) = v.get("cancelled_wakes").and_then(|c| c.as_array()) {
        if cancelled.is_empty() {
            println!("cancelled wakes: none");
        } else {
            println!("cancelled wakes ({}):", cancelled.len());
            for id in cancelled {
                println!("  - {}", id.as_str().unwrap_or("?"));
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
