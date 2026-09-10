//! Retain terminal queue attempt/drop identities independently of snapshots.
//!
//! Fork migration. It shipped as v029 on the pre-1.17 fork, so hosts that ran
//! it record schema version 29 and would skip upstream's own v029
//! (`fold_pending_initial_turn`). That fold is idempotent (it only rewrites the
//! legacy string form), so it runs again here to cover those hosts.

use anyhow::Result;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    super::v029_fold_pending_initial_turn::run_in(&app_dir)?;
    let path = app_dir.join("acp_events.db");
    let conn = rusqlite::Connection::open(&path)?;
    crate::acp::event_store::terminal_queue::initialize(&conn)?;
    tracing::info!(path = %path.display(), "terminal queue receipt schema installed");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn preserves_existing_event_rows_and_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (seq INTEGER); INSERT INTO acp_events VALUES (7);",
        )
        .unwrap();
        for _ in 0..2 {
            crate::acp::event_store::terminal_queue::initialize(&conn).unwrap();
        }
        assert_eq!(
            conn.query_row("SELECT seq FROM acp_events", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            7
        );
        conn.execute(
            "INSERT INTO terminal_queue_receipts VALUES ('s', 'q', 'claimed')",
            [],
        )
        .unwrap();
    }
}
