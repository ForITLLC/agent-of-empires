//! Durable fleet message log: every `aoe send` (CLI direct-to-tmux) and
//! daemon `POST /api/sessions/{id}/send` is recorded here so message
//! history survives session restarts and is queryable across the fleet
//! (`GET /api/messages`).
//!
//! Storage rides the protocol-agnostic [`crate::events`] substrate: one
//! SQLite database at `<app_dir>/messages.db`, topic = target session id,
//! payload = a JSON [`MessageRecord`]. The CLI process and the daemon both
//! open the same file; WAL mode plus a busy timeout make the two writers
//! coexist. Logging is best-effort at every call site — a send must never
//! fail because its audit row couldn't be written.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::events::{self, Order, Schema, SeqBound};

/// One logged send. Serialized as the event payload; field names are the
/// wire contract for `GET /api/messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Unix seconds when the send happened.
    pub ts: i64,
    /// Which path performed the send: "cli" or "api".
    pub source: String,
    /// Caller identity when known (e.g. the X-Caller-Session header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    pub target_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_title: Option<String>,
    pub message: String,
    /// "sent" or an error summary when the tmux delivery failed.
    pub outcome: String,
}

/// Durable message log over the [`crate::events`] substrate. Topic = target
/// session id, one event per send. Safe for the CLI and the daemon to open
/// concurrently: WAL plus a busy timeout on the shared database file.
pub struct MessageLog {
    conn: Connection,
    schema: Schema,
    retention: usize,
}

/// Default per-target retention cap for production call sites.
pub const DEFAULT_RETENTION: usize = 1000;

/// Database path under the app dir shared by the CLI and the daemon.
pub fn default_db_path() -> Result<std::path::PathBuf> {
    Ok(crate::session::get_app_dir()?.join("messages.db"))
}

impl MessageLog {
    pub fn open(db_path: &Path, retention: usize) -> Result<Self> {
        let schema = Schema::new("messages")?;
        let conn = events::open(db_path, &schema)?;
        conn.busy_timeout(Duration::from_secs(3))?;
        Ok(Self {
            conn,
            schema,
            retention,
        })
    }

    /// Append one record; returns its seq (monotonic per target session).
    pub fn record(&mut self, rec: &MessageRecord) -> Result<u64> {
        let topic = rec.target_session.as_str();
        let seq = events::highest_seq(&self.conn, &self.schema, topic) + 1;
        let json = serde_json::to_string(rec)?;
        events::insert_event(&self.conn, &self.schema, topic, seq, &json, rec.ts)?;
        events::prune_retention(&self.conn, &self.schema, topic, self.retention, &[]);
        Ok(seq)
    }

    /// Newest-first history for one target session.
    pub fn for_session(&self, session_id: &str, limit: usize) -> Vec<(u64, MessageRecord)> {
        events::scan(
            &self.conn,
            &self.schema,
            session_id,
            SeqBound::Before(u64::MAX),
            Order::Desc,
            Some(limit),
        )
        .into_iter()
        .filter_map(|(seq, json)| decode(seq, &json))
        .collect()
    }

    /// Newest-first history across all target sessions. Cross-topic, so it
    /// queries the substrate table directly rather than through `scan`.
    pub fn recent(&self, limit: usize) -> Vec<(u64, MessageRecord)> {
        let sql = format!(
            "SELECT seq, event_json FROM {}
             ORDER BY created_at DESC, seq DESC LIMIT ?1",
            self.schema.events_table()
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "messages", "prepare recent: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map([limit as i64], |row| {
            let seq: i64 = row.get(0)?;
            let json: String = row.get(1)?;
            Ok((seq as u64, json))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "messages", "query recent: {e}");
                return Vec::new();
            }
        };
        rows.filter_map(|row| match row {
            Ok((seq, json)) => decode(seq, &json),
            Err(e) => {
                warn!(target: "messages", "row error: {e}");
                None
            }
        })
        .collect()
    }
}

/// Record `rec` in the default log, swallowing every failure. Send call
/// sites use this so a message that reached the pane is never reported as
/// failed because its audit row couldn't be written.
pub fn log_best_effort(rec: &MessageRecord) {
    let path = match default_db_path() {
        Ok(p) => p,
        Err(e) => {
            warn!(target: "messages", "message log unavailable (no app dir): {e}");
            return;
        }
    };
    match MessageLog::open(&path, DEFAULT_RETENTION) {
        Ok(mut log) => {
            if let Err(e) = log.record(rec) {
                warn!(target: "messages", "failed to record send to {}: {e}", rec.target_session);
            }
        }
        Err(e) => warn!(target: "messages", "failed to open message log: {e}"),
    }
}

fn decode(seq: u64, json: &str) -> Option<(u64, MessageRecord)> {
    match serde_json::from_str(json) {
        Ok(rec) => Some((seq, rec)),
        Err(e) => {
            warn!(target: "messages", "undecodable message record at seq {seq}: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(target: &str, body: &str, ts: i64) -> MessageRecord {
        MessageRecord {
            ts,
            source: "cli".to_string(),
            sender: None,
            target_session: target.to_string(),
            target_title: Some("title".to_string()),
            message: body.to_string(),
            outcome: "sent".to_string(),
        }
    }

    #[test]
    fn record_and_query_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = MessageLog::open(&tmp.path().join("messages.db"), 100).unwrap();

        log.record(&rec("session_a", "first to a", 1000)).unwrap();
        log.record(&rec("session_a", "second to a", 1001)).unwrap();
        log.record(&rec("session_b", "only to b", 1002)).unwrap();

        // Per-session query returns newest-first for that topic only.
        let a = log.for_session("session_a", 10);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].1.message, "second to a");
        assert_eq!(a[1].1.message, "first to a");

        // Cross-session recent view sees all three, newest first.
        let all = log.recent(10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].1.message, "only to b");
        assert_eq!(all[2].1.message, "first to a");
    }

    #[test]
    fn seq_is_monotonic_per_target() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = MessageLog::open(&tmp.path().join("messages.db"), 100).unwrap();

        let s1 = log.record(&rec("session_a", "one", 1)).unwrap();
        let s2 = log.record(&rec("session_a", "two", 2)).unwrap();
        let s3 = log.record(&rec("session_b", "b one", 3)).unwrap();
        assert!(s2 > s1);
        assert_eq!(s3, 1, "each target session starts its own seq space");
    }

    #[test]
    fn retention_caps_per_target_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = MessageLog::open(&tmp.path().join("messages.db"), 3).unwrap();

        for i in 0..5 {
            log.record(&rec("session_a", &format!("msg {i}"), i))
                .unwrap();
        }
        let a = log.for_session("session_a", 10);
        assert_eq!(a.len(), 3, "retention prunes oldest past the cap");
        assert_eq!(a[0].1.message, "msg 4");
        assert_eq!(a[2].1.message, "msg 2");
    }

    #[test]
    fn limit_bounds_query() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = MessageLog::open(&tmp.path().join("messages.db"), 100).unwrap();
        for i in 0..10 {
            log.record(&rec("session_a", &format!("msg {i}"), i))
                .unwrap();
        }
        assert_eq!(log.for_session("session_a", 4).len(), 4);
        assert_eq!(log.recent(4).len(), 4);
    }

    #[test]
    fn two_writers_share_one_database() {
        // The CLI and the daemon are separate processes appending to the
        // same file; two independent connections must both land rows.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("messages.db");
        let mut w1 = MessageLog::open(&db, 100).unwrap();
        let mut w2 = MessageLog::open(&db, 100).unwrap();

        w1.record(&rec("session_a", "from cli", 1)).unwrap();
        w2.record(&rec("session_a", "from daemon", 2)).unwrap();

        let a = w1.for_session("session_a", 10);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].1.message, "from daemon");
    }
}
