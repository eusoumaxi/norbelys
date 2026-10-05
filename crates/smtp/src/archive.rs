//! Transfers captured evidence to Turso in bounded batches, without downloading history.
//!
//! The local queue owns capture durability and source checkpoints. Turso owns the event,
//! its route and delivery state. A remote transaction inserts events by their stable ids and
//! advances confirmed checkpoints in queue order. Only a confirmed commit allows local
//! removal. A lost commit response therefore leaves every record available for replay;
//! duplicate inserts never reset a delivered event. No queue lock is held during network I/O.

use std::time::Duration;

use crate::db::{Connection, Db, params};
use crate::queue::{Queue, Record};
use crate::serve::Shutdown;
use crate::telemetry;

/// A canonical database operation or local durability failure preserves the pending batch.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Turso did not confirm the transaction; it may already have committed.
    #[error("remote archival: {0}")]
    Database(#[from] crate::db::Error),
    /// The local batch could not be read or acknowledged.
    #[error("pending evidence: {0}")]
    Queue(#[from] crate::queue::Error),
    /// The blocking operation failed before returning a result.
    #[error("archival task: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Transfers one bounded batch, returning the number of acknowledged records.
/// A failed remote write or commit preserves the entire local batch for retry.
pub fn transfer(conn: &mut Connection, queue: &Queue) -> Result<usize, Error> {
    let pending = queue.pending()?;
    if pending.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut events = Vec::new();
    for item in &pending {
        if let Record::Event {
            id,
            route,
            payload,
            created,
        } = &item.record
        {
            events.push(vec![
                id.clone().into(),
                route.clone().into(),
                payload.clone().into(),
                (*created).into(),
            ]);
        }
    }
    tx.execute_many("INSERT INTO events (id, provider_webhook_id, payload, created) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (id) DO NOTHING", events)?;
    for item in &pending {
        if let Record::Cursor {
            node,
            inode,
            position,
        } = &item.record
        {
            tx.execute("INSERT INTO cursors (node, inode, position) VALUES (?1, ?2, ?3) ON CONFLICT (node) DO UPDATE SET inode = excluded.inode, position = excluded.position", params![node, inode, position])?;
        }
    }
    tx.commit()?;
    queue.acknowledge(&pending.iter().map(|row| row.sequence).collect::<Vec<_>>())?;
    Ok(pending.len())
}

/// Drains the local queue until shutdown, retrying failures with its records intact.
pub async fn run(db: Db, queue: Queue, mut shutdown: Shutdown) -> anyhow::Result<()> {
    loop {
        let capture = queue.clone();
        let result = db.call(move |conn| transfer(conn, &capture)).await;
        let wait = match result {
            Ok(0) => Duration::from_millis(500),
            Ok(records) => {
                telemetry::unit(telemetry::Event::Archive);
                tracing::info!(
                    event = "mta.archive",
                    records,
                    outcome = "confirmed",
                    "mta.archive"
                );
                Duration::ZERO
            }
            Err(error) => {
                telemetry::unit(telemetry::Event::Archive);
                tracing::error!(event = "mta.archive", error = %error, outcome = "retry", "mta.archive");
                Duration::from_secs(10)
            }
        };
        if !shutdown.sleep(wait).await {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{SqlServer, TempDir};
    use std::sync::atomic::Ordering;

    /// A lost commit acknowledgement preserves local evidence and cursor for retry, while
    /// replay leaves the canonical event and its completed callback state untouched.
    #[test]
    fn retries_uncertain_commits_without_losing_or_reopening_evidence() {
        let server = SqlServer::new();
        let dir = TempDir::new();
        let queue = Queue::open(&dir.join("pending.sqlite"), "mail-a", 4 * 1024 * 1024).unwrap();
        let mut conn = Connection::remote(&server.args, "mail-a", None).unwrap();
        queue
            .append(vec![
                Record::Event {
                    id: "e1".to_owned(),
                    route: "r1".to_owned(),
                    payload: r#"{"event_id":"e1"}"#.to_owned(),
                    created: 1.0,
                },
                Record::Cursor {
                    node: "mail-a".to_owned(),
                    inode: "42".to_owned(),
                    position: 128,
                },
            ])
            .unwrap();
        server.lose_commit.store(true, Ordering::SeqCst);
        assert!(transfer(&mut conn, &queue).is_err());
        assert_eq!(queue.pending().unwrap().len(), 2);
        conn.execute(
            "UPDATE events SET done = 1, attempts = 3 WHERE id = 'e1'",
            [],
        )
        .unwrap();
        assert_eq!(transfer(&mut conn, &queue).unwrap(), 2);
        assert!(queue.pending().unwrap().is_empty());
        assert_eq!(
            conn.query_row("SELECT count(*), done, attempts FROM events", [], |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?
            )))
            .unwrap(),
            (1, 1, 3)
        );
        assert_eq!(conn.cursor("mail-a").unwrap(), Some(("42".to_owned(), 128)));
    }

    /// A remote outage preserves every pending record; recovery commits and removes them,
    /// and two SMTP hosts sharing one endpoint cannot see or overwrite each other's tables.
    #[test]
    fn remote_outages_preserve_records_and_nodes_are_isolated() {
        let server = SqlServer::new();
        let dir = TempDir::new();
        let queue = Queue::open(&dir.join("pending.sqlite"), "mail-a", 4 * 1024 * 1024).unwrap();
        let mut a = Connection::remote(&server.args, "mail-a", None).unwrap();
        let b = Connection::remote(&server.args, "mail-b", None).unwrap();
        queue
            .append(vec![Record::Event {
                id: "same-id".to_owned(),
                route: "r1".to_owned(),
                payload: "{}".to_owned(),
                created: 1.0,
            }])
            .unwrap();
        server.available.store(false, Ordering::SeqCst);
        assert!(transfer(&mut a, &queue).is_err());
        assert_eq!(queue.pending().unwrap().len(), 1);
        server.available.store(true, Ordering::SeqCst);
        transfer(&mut a, &queue).unwrap();
        assert_eq!(
            a.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            b.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        b.execute(
            "INSERT INTO events (id,payload,created) VALUES ('same-id','{}',1)",
            [],
        )
        .unwrap();
        assert_eq!(
            b.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            a.query_row(
                "SELECT provider_webhook_id FROM events WHERE id = 'same-id'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "r1"
        );
    }
}
