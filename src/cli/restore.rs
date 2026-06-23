//! `agent-of-empires restore` — recover an archived session.
//!
//! Top-level, discoverable alias for `aoe session unarchive`: with `aoe remove`
//! now archive-preferred, `restore` is the obvious recovery verb (pairs with
//! `aoe list --archived`). Flips `archived_at` back off, returns the row to its
//! tier in the Attention sort, and lands an audit line.

use anyhow::Result;
use clap::Args;

use crate::session::{audit, Storage};

#[derive(Args)]
pub struct RestoreArgs {
    /// Session ID or title to restore (unarchive)
    identifier: String,
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: RestoreArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

    // Resolve + flip archived_at off atomically under the lock, capturing the
    // instance + whether it was actually archived for the audit/report below.
    let (inst, was_archived) = storage.update(|instances, _groups| {
        let id = super::resolve_session(&args.identifier, instances)?
            .id
            .clone();
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .expect("resolve_session returned an id that is no longer in instances");
        let was_archived = inst.is_archived();
        inst.unarchive();
        Ok((inst.clone(), was_archived))
    })?;

    audit::record(
        audit::Event::Unarchive,
        "cli-restore",
        &inst,
        storage.profile(),
        false,
        false,
    );

    if was_archived {
        println!("Restored (unarchived): {}", inst.title);
    } else {
        println!(
            "Session {} was not archived; nothing to restore.",
            inst.title
        );
    }
    Ok(())
}
