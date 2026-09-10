//! Quarantine legacy terminal queues whose previous paste attempts are unknown.

use anyhow::{Context, Result};
use std::{fs, io::ErrorKind, path::Path};

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    // Parse every snapshot before committing anything. Storage::load skips
    // malformed rows, which is unsafe here: they could later be repaired and
    // delivered without a receipt for the old daemon's possible paste.
    let mut queued = Vec::new();
    read_snapshot(&app_dir.join("sessions.json"), &mut queued)?;
    let profiles = app_dir.join("profiles");
    match fs::symlink_metadata(&profiles) {
        Ok(_) => {
            for entry in fs::read_dir(&profiles)? {
                let path = entry?.path();
                if fs::metadata(&path)?.is_dir() {
                    read_snapshot(&path.join("sessions.json"), &mut queued)?;
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let path = app_dir.join("acp_events.db");
    let mut conn = rusqlite::Connection::open(&path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let transaction = conn.transaction()?;
    crate::acp::event_store::terminal_queue::initialize(&transaction)?;
    let mut quarantined = 0;
    for (session, qid) in queued {
        quarantined += transaction.execute(
            "INSERT INTO terminal_queue_receipts VALUES (?1, ?2, 'legacy_uncertain')
             ON CONFLICT(session_id, prompt_id) DO NOTHING",
            rusqlite::params![session, qid],
        )?;
    }
    transaction.commit()?;
    tracing::info!(path = %path.display(), quarantined,
        "terminal queue receipts installed; legacy rows held for review");
    Ok(())
}

fn read_snapshot(path: &Path, queued: &mut Vec<(String, String)>) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let bytes =
        fs::read(path).with_context(|| format!("read legacy queue snapshot {}", path.display()))?;
    let instances: Vec<crate::session::Instance> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse legacy queue snapshot {}", path.display()))?;
    for instance in instances {
        if !instance.is_structured() {
            queued.extend(
                instance
                    .queued_prompts
                    .into_iter()
                    .map(|prompt| (instance.id.clone(), prompt.id)),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        acp::event_store::EventStore,
        acp::state::QueuedPromptEntry,
        session::{Instance, View},
    };

    fn session(id: &str, qids: &[&str]) -> Instance {
        let mut instance = Instance::new(id, "/tmp/legacy-queue");
        instance.id = id.into();
        instance.queued_prompts = qids
            .iter()
            .enumerate()
            .map(|(seq, qid)| QueuedPromptEntry {
                id: (*qid).into(),
                seq: seq as u64,
                text: format!("preserve {qid}"),
                attachments: vec![],
                created_at: "2026-09-10T00:00:00Z".into(),
                origin_device: None,
            })
            .collect();
        instance
    }

    #[test]
    fn legacy_quarantine_survives_reopen_and_preserves_receipts_text_and_acp() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("acp_events.db");
        let store = EventStore::open(&db, 10).unwrap();
        store.complete_terminal_prompt("terminal", "done").unwrap();
        store.drop_terminal_prompt("terminal", "dropped").unwrap();
        let terminal = session("terminal", &["pending", "done", "dropped"]);
        let mut archived = session("archived", &["old"]);
        archived.archived_at = Some(chrono::Utc::now());
        let mut snoozed = session("snoozed", &["old"]);
        snoozed.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::days(1));
        let mut structured = session("structured", &["acp"]);
        structured.view = View::Structured;
        let profile = dir.path().join("profiles/example");
        fs::create_dir_all(&profile).unwrap();
        let snapshot = profile.join("sessions.json");
        let original = serde_json::to_vec(&vec![terminal, archived, snoozed, structured]).unwrap();
        fs::write(&snapshot, &original).unwrap();
        fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&vec![session("root", &["old"])]).unwrap(),
        )
        .unwrap();
        run_in(dir.path()).unwrap();
        run_in(dir.path()).unwrap();
        drop(store);
        let reopened = EventStore::open(&db, 10).unwrap();
        for (id, qid) in [
            ("terminal", "pending"),
            ("archived", "old"),
            ("snoozed", "old"),
            ("root", "old"),
        ] {
            assert_eq!(
                reopened
                    .terminal_prompt_receipt(id, qid)
                    .unwrap()
                    .as_deref(),
                Some("legacy_uncertain")
            );
            assert!(!reopened.claim_terminal_prompt(id, qid).unwrap());
        }
        assert_eq!(
            reopened
                .terminal_prompt_receipt("terminal", "done")
                .unwrap()
                .as_deref(),
            Some("delivered")
        );
        assert_eq!(
            reopened
                .terminal_prompt_receipt("terminal", "dropped")
                .unwrap()
                .as_deref(),
            Some("dropped")
        );
        assert_eq!(
            reopened
                .terminal_prompt_receipt("structured", "acp")
                .unwrap(),
            None
        );
        assert!(reopened.claim_terminal_prompt("terminal", "new").unwrap());
        assert_eq!(fs::read(snapshot).unwrap(), original);
    }

    #[test]
    fn malformed_or_unreadable_snapshots_abort_without_committing_receipts() {
        for contents in [
            "not json",
            "{}",
            "[{}]",
            "[{\"id\":\"s\",\"queued_prompts\":42}]",
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("sessions.json"), contents).unwrap();
            assert!(run_in(dir.path()).is_err(), "{contents}");
            assert!(!dir.path().join("acp_events.db").exists());
        }
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sessions.json")).unwrap();
        assert!(run_in(dir.path()).is_err());
        assert!(!dir.path().join("acp_events.db").exists());
    }
}
