//! Terminal delivery receipts outlive queue snapshots and event retention.
//! A claim grants one paste attempt, never permission to retry it. Only a
//! `released` receipt grants another: the drain writes one when a withheld
//! Enter's paste was verifiably removed (`released:N`, N attempts so far),
//! an operator writes one after inspecting the pane (`released`).

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

/// Attempts so far encoded in a `released` receipt: `released` is 0 (an
/// operator's release), `released:N` is N automatic attempts, each withheld
/// with its paste verifiably removed. Anything else is not a release.
pub(crate) fn released_attempts(disposition: &str) -> Option<u32> {
    match disposition.strip_prefix("released") {
        Some("") => Some(0),
        Some(rest) => rest.strip_prefix(':').and_then(|n| n.parse().ok()),
        None => None,
    }
}

pub(crate) fn released_disposition(attempts: u32) -> String {
    if attempts == 0 {
        "released".to_string()
    } else {
        format!("released:{attempts}")
    }
}

/// Which existing receipt a write may overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    /// Insert or replace unconditionally.
    Always,
    /// Insert, or replace a `released` receipt: the one grant of another attempt.
    Released,
}

impl EventStore {
    /// Commit before any terminal input. An existing receipt, including a
    /// dropped row resurrected by stale sessions.json, never grants a paste
    /// unless it is a `released` one, which grants exactly one more.
    pub(crate) fn claim_terminal_prompt(&self, session: &str, qid: &str) -> Result<bool> {
        self.write_terminal_receipt(session, qid, "claimed", Overwrite::Released)
    }

    pub(crate) fn drop_terminal_prompt(&self, session: &str, qid: &str) -> Result<()> {
        self.write_terminal_receipt(session, qid, "dropped", Overwrite::Always)?;
        Ok(())
    }

    pub(crate) fn complete_terminal_prompt(&self, session: &str, qid: &str) -> Result<()> {
        self.write_terminal_receipt(session, qid, "delivered", Overwrite::Always)?;
        Ok(())
    }

    /// Release a held row (`claimed` or `legacy_uncertain`) for one more
    /// attempt, recording `attempts` automatic attempts so far (0 for an
    /// operator's release). `Ok(false)` when there is no held receipt to
    /// release: none at all, delivered, dropped, or already released. A
    /// delivered or dropped row can never come back this way.
    pub(crate) fn release_terminal_prompt(
        &self,
        session: &str,
        qid: &str,
        attempts: u32,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("queue receipt lock poisoned"))?;
        let previous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let result = conn.execute(
            "UPDATE terminal_queue_receipts SET disposition = ?3
             WHERE session_id = ?1 AND prompt_id = ?2
               AND disposition IN ('claimed', 'legacy_uncertain')",
            params![session, qid, released_disposition(attempts)],
        );
        let restored = conn.pragma_update(None, "synchronous", previous);
        let changed = result?;
        restored?;
        Ok(changed == 1)
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
        overwrite: Overwrite,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("queue receipt lock poisoned"))?;
        // NORMAL WAL commits can be lost on power failure. Only these rare
        // delivery/drop writes pay FULL; the event stream keeps its own policy.
        let previous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let sql = match overwrite {
            Overwrite::Always => {
                "INSERT INTO terminal_queue_receipts VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, prompt_id) DO UPDATE SET disposition = excluded.disposition"
            }
            Overwrite::Released => {
                "INSERT INTO terminal_queue_receipts VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, prompt_id) DO UPDATE SET disposition = excluded.disposition
                 WHERE terminal_queue_receipts.disposition = 'released'
                    OR terminal_queue_receipts.disposition LIKE 'released:%'"
            }
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
    fn released_receipts_grant_exactly_one_more_claim() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("events.db"), 10).unwrap();
        let receipt = |qid: &str| store.terminal_prompt_receipt("s", qid).unwrap();
        assert!(store.claim_terminal_prompt("s", "q").unwrap());
        assert!(!store.claim_terminal_prompt("s", "q").unwrap());
        // Enter withheld, paste verifiably removed: one more attempt.
        assert!(store.release_terminal_prompt("s", "q", 1).unwrap());
        assert_eq!(receipt("q").as_deref(), Some("released:1"));
        assert!(store.claim_terminal_prompt("s", "q").unwrap());
        assert_eq!(receipt("q").as_deref(), Some("claimed"));
        assert!(!store.claim_terminal_prompt("s", "q").unwrap());
        // Releasing a released row is not a second grant.
        assert!(store.release_terminal_prompt("s", "q", 2).unwrap());
        assert!(!store.release_terminal_prompt("s", "q", 3).unwrap());
        assert_eq!(receipt("q").as_deref(), Some("released:2"));
        // Delivered and dropped rows never come back.
        store.claim_terminal_prompt("s", "q").unwrap();
        store.complete_terminal_prompt("s", "q").unwrap();
        assert!(!store.release_terminal_prompt("s", "q", 0).unwrap());
        assert_eq!(receipt("q").as_deref(), Some("delivered"));
        store.drop_terminal_prompt("s", "d").unwrap();
        assert!(!store.release_terminal_prompt("s", "d", 0).unwrap());
        assert_eq!(receipt("d").as_deref(), Some("dropped"));
        assert!(!store.claim_terminal_prompt("s", "d").unwrap());
        // Nothing to release without a receipt, and nothing is written.
        assert!(!store.release_terminal_prompt("s", "none", 0).unwrap());
        assert_eq!(receipt("none"), None);
        // A v029 quarantine row is released by an operator.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO terminal_queue_receipts VALUES ('s', 'legacy', 'legacy_uncertain')",
                [],
            )
            .unwrap();
        assert!(!store.claim_terminal_prompt("s", "legacy").unwrap());
        assert!(store.release_terminal_prompt("s", "legacy", 0).unwrap());
        assert_eq!(receipt("legacy").as_deref(), Some("released"));
        assert!(store.claim_terminal_prompt("s", "legacy").unwrap());
    }

    #[test]
    fn released_attempts_parse_only_release_receipts() {
        assert_eq!(released_attempts("released"), Some(0));
        assert_eq!(released_attempts("released:1"), Some(1));
        assert_eq!(released_attempts("released:12"), Some(12));
        assert_eq!(released_attempts("released:"), None);
        assert_eq!(released_attempts("released:x"), None);
        assert_eq!(released_attempts("claimed"), None);
        assert_eq!(released_attempts("legacy_uncertain"), None);
        assert_eq!(released_attempts("delivered"), None);
        assert_eq!(released_disposition(0), "released");
        assert_eq!(released_disposition(2), "released:2");
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
