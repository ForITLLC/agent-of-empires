//! Terminal delivery receipts outlive queue snapshots and event retention.
//! A claim grants one paste attempt, never permission to retry it.

use super::*;

pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS terminal_queue_receipts (
            session_id TEXT NOT NULL,
            prompt_id TEXT NOT NULL,
            disposition TEXT NOT NULL,
            PRIMARY KEY (session_id, prompt_id)
        );",
    )?;
    Ok(())
}

impl EventStore {
    /// Commit before any terminal input. An existing receipt, including a
    /// dropped row resurrected by stale sessions.json, never grants a paste.
    pub(crate) fn claim_terminal_prompt(&self, session: &str, qid: &str) -> Result<bool> {
        self.write_terminal_receipt(session, qid, "claimed", false)
    }

    pub(crate) fn drop_terminal_prompt(&self, session: &str, qid: &str) -> Result<()> {
        self.write_terminal_receipt(session, qid, "dropped", true)?;
        Ok(())
    }

    pub(crate) fn complete_terminal_prompt(&self, session: &str, qid: &str) -> Result<()> {
        self.write_terminal_receipt(session, qid, "delivered", true)?;
        Ok(())
    }

    pub(crate) fn terminal_prompt_receipt(
        &self,
        session: &str,
        qid: &str,
    ) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("queue receipt lock poisoned"))?;
        Ok(conn.query_row(
            "SELECT disposition FROM terminal_queue_receipts WHERE session_id = ?1 AND prompt_id = ?2",
            params![session, qid],
            |row| row.get(0),
        ).optional()?)
    }

    fn write_terminal_receipt(
        &self,
        session: &str,
        qid: &str,
        disposition: &str,
        replace: bool,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("queue receipt lock poisoned"))?;
        // NORMAL WAL commits can be lost on power failure. Only these rare
        // delivery/drop writes pay FULL; the event stream keeps its own policy.
        let previous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let sql = if replace {
            "INSERT INTO terminal_queue_receipts VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id, prompt_id) DO UPDATE SET disposition = excluded.disposition"
        } else {
            "INSERT INTO terminal_queue_receipts VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id, prompt_id) DO NOTHING"
        };
        let result = conn.execute(sql, params![session, qid, disposition]);
        let restored = conn.pragma_update(None, "synchronous", previous);
        let changed = result?;
        restored?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_receipts_survive_restart_drop_replay_and_competing_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let first = EventStore::open(&path, 10).unwrap();
        let peer = EventStore::open(&path, 10).unwrap();
        assert!(first.claim_terminal_prompt("s", "q").unwrap());
        assert!(!peer.claim_terminal_prompt("s", "q").unwrap());
        first.drop_terminal_prompt("s", "dropped").unwrap();
        first.claim_terminal_prompt("s", "done").unwrap();
        first.complete_terminal_prompt("s", "done").unwrap();
        drop(first);
        drop(peer);
        let restarted = EventStore::open(&path, 10).unwrap();
        for qid in ["q", "dropped", "done"] {
            assert!(!restarted.claim_terminal_prompt("s", qid).unwrap());
        }
        assert!(restarted
            .claim_terminal_prompt("other-session", "q")
            .unwrap());
        assert!(restarted.claim_terminal_prompt("s", "new-qid").unwrap());
    }

    #[test]
    fn terminal_receipt_failure_never_grants_permission_to_paste() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("events.db"), 10).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .pragma_update(None, "query_only", true)
            .unwrap();
        assert!(store.claim_terminal_prompt("s", "q").is_err());
        assert!(store.drop_terminal_prompt("s", "q").is_err());
        assert_eq!(store.terminal_prompt_receipt("s", "q").unwrap(), None);
    }

    #[test]
    fn terminal_attempt_stays_consumed_after_ambiguous_submit_abort_or_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        for outcome in ["submit_unconfirmed", "aborted", "panic"] {
            let attempt = std::panic::catch_unwind(|| {
                let store = EventStore::open(&path, 10).unwrap();
                assert!(store.claim_terminal_prompt("s", outcome).unwrap());
                if outcome == "panic" {
                    panic!("crashed after paste, before retirement");
                }
                Err::<(), _>(outcome)
            });
            assert!(attempt.is_err() || attempt.unwrap().is_err());
            let restarted = EventStore::open(&path, 10).unwrap();
            assert!(!restarted.claim_terminal_prompt("s", outcome).unwrap());
        }
    }
}
