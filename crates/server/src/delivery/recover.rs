//! Recovery of lost delivery leases: a sender that died, stalled or lost the database leaves its
//! claimed and in-flight queue rows leased until their leases expire; this sweep takes them back.
//!
//! A lease cannot fence a remote server, so what recovery does depends on how far the message
//! got, and the submission marker (`submission_started_at`, set by the Start immediately before
//! the submission) is what tells:
//!
//! - **No marker**: nothing was handed to a socket. The row goes back to `queued` (due when it
//!   was), its attempt closes `released` and its reservation is released, once: the attempt
//!   leaves `reserved` in the same statement that returns it, so a second recovery settles
//!   nothing. The connection's budget wait is cleared, since budget was freed.
//! - **Marker set**: the phase the submission reached is unknown, so the provider may have the
//!   message. It becomes `uncertain`, its reservation is consumed (conservatively), it is never
//!   resent automatically, `message.uncertain` is told, and on a mailbox the connection's
//!   `connection.check` is asked to read the Sent folder for it, as a finish of an `uncertain`
//!   answer does.
//!
//! A probe of a half-open breaker is a queue row in one lease generation, so a recovered probe
//! frees the breaker's slot by itself: its lease is no longer live.
//!
//! The sweep runs on every sender every few seconds. It finds expired leases as the scheduler
//! role (the lease columns of every workspace, nothing else), then recovers each connection's
//! rows in one tenant transaction that takes the connection's row first and re-checks under the
//! queue rows' locks that each lease is still the expired one it read: a row finished, recovered
//! or claimed again meanwhile is left alone.
//!
//! Lock order, as every delivery path: the connection's row → queue rows, then message rows, in
//! message order → attempts → the connection's ledger rows, then the scope's, each in day order.

use std::collections::BTreeMap;
use std::time::Instant;

use uuid::Uuid;

use super::evidence;
use super::finish::{self, Reservation};
use crate::db::{self, Database};
use crate::domain::ids::{Attempt, Connection, Id, Message, WorkspaceId};
use crate::domain::messages::State as MessageState;
use crate::domain::policy::delivery::{self as policy, Category, Quota};
use crate::domain::senders::Provider;
use crate::domain::time::Date;
use crate::jobs::{self, Queue};
use crate::senders::check::ConnectionCheck;
use crate::webhooks::EventType;

/// Expired leases read per sweep; the rest wait for the next one, seconds later.
const PER_SWEEP: i64 = 500;

/// What one sweep recovered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Rows put back in the queue (no marker).
    pub requeued: usize,
    /// Messages made `uncertain` (marker set).
    pub uncertain: usize,
}

/// One expired lease as the scheduler reads it.
struct Expired {
    workspace_id: Uuid,
    connection_id: Uuid,
    message_id: Uuid,
    lease_generation: i64,
}

/// Recovers every lease that expired, up to [`PER_SWEEP`] (see the module).
///
/// # Errors
///
/// The database refused; what was committed before stays recovered.
pub async fn sweep(db: &Database) -> Result<Recovered, sqlx::Error> {
    let started = Instant::now();
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let expired = sqlx::query_as!(
        Expired,
        "SELECT workspace_id, connection_id, message_id, lease_generation FROM delivery_queue
          WHERE state <> 'queued' AND lease_expires_at < now()
          ORDER BY lease_expires_at LIMIT $1",
        PER_SWEEP,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut groups: BTreeMap<(Uuid, Uuid), Vec<(Uuid, i64)>> = BTreeMap::new();
    for row in expired {
        groups
            .entry((row.workspace_id, row.connection_id))
            .or_default()
            .push((row.message_id, row.lease_generation));
    }
    let mut total = Recovered::default();
    for ((workspace, connection), rows) in groups {
        let recovered = recover(
            db,
            WorkspaceId::trusted(workspace),
            Id::from_uuid(connection),
            &rows,
        )
        .await?;
        total.requeued += recovered.requeued;
        total.uncertain += recovered.uncertain;
    }
    if total != Recovered::default() {
        crate::telemetry::unit(crate::telemetry::Event::DeliveryRecover);
        tracing::info!(
            event = "delivery.recover",
            requeued = total.requeued,
            uncertain = total.uncertain,
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "delivery.recover"
        );
    }
    Ok(total)
}

/// A row still holding the expired lease, under its lock.
struct Lost {
    message_id: Uuid,
    marked: bool,
    attempt_number: i32,
}

/// An attempt recovery closed.
struct Closed {
    id: Uuid,
    message_id: Uuid,
    reserved_day: Date,
    quota_scope_id: Option<Uuid>,
    recipient_count: i32,
    quota_state: String,
}

/// Recovers `rows` (message and the lease generation read) of one connection, in one tenant
/// transaction (see the module).
async fn recover(
    db: &Database,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    rows: &[(Uuid, i64)],
) -> Result<Recovered, sqlx::Error> {
    let mut tx = db.begin_in(workspace).await?;
    let Some(provider) = sqlx::query_scalar!(
        "SELECT provider FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Recovered::default());
    };
    let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    let generations: Vec<i64> = rows.iter().map(|(_, generation)| *generation).collect();
    let lost = sqlx::query_as!(
        Lost,
        r#"SELECT q.message_id, q.submission_started_at IS NOT NULL AS "marked!", m.attempt_number
             FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
            WHERE q.workspace_id = $1 AND q.connection_id = $2 AND q.state <> 'queued' AND q.lease_expires_at < now()
              AND (q.message_id, q.lease_generation) IN (SELECT * FROM unnest($3::uuid[], $4::bigint[]))
            ORDER BY q.message_id
              FOR UPDATE OF q, m"#,
        workspace.uuid(),
        connection.uuid(),
        &ids,
        &generations,
    )
    .fetch_all(&mut *tx)
    .await?;
    if lost.is_empty() {
        tx.rollback().await?;
        return Ok(Recovered::default());
    }

    let decided: Vec<(&Lost, policy::Next)> = lost
        .iter()
        .map(|row| (row, policy::after_lost_lease(row.marked)))
        .collect();
    let ids: Vec<Uuid> = decided.iter().map(|(row, _)| row.message_id).collect();
    let numbers: Vec<i32> = decided.iter().map(|(row, _)| row.attempt_number).collect();
    let outcomes: Vec<&str> = decided
        .iter()
        .map(|(_, next)| next.outcome.as_str())
        .collect();
    let quotas: Vec<&str> = decided
        .iter()
        .map(|(_, next)| next.quota.as_str())
        .collect();
    let categories: Vec<Option<&str>> = decided
        .iter()
        .map(|(_, next)| {
            (next.state == MessageState::Uncertain).then_some(Category::Uncertain.as_str())
        })
        .collect();
    let closed = sqlx::query_as!(
        Closed,
        r#"UPDATE attempts a SET outcome = d.outcome, finished_at = now(), quota_state = d.quota, category = d.category,
                  diagnostic = 'The sender lost its lease on the message.'
             FROM unnest($3::uuid[], $4::int[], $5::text[], $6::text[], $7::text[]) AS d(message_id, attempt_number, outcome, quota, category)
            WHERE a.workspace_id = $1 AND a.connection_id = $2 AND a.message_id = d.message_id
              AND a.attempt_number = d.attempt_number AND a.quota_state = 'reserved'
        RETURNING a.id, a.message_id, a.reserved_day AS "reserved_day: Date", a.quota_scope_id, a.recipient_count, a.quota_state"#,
        workspace.uuid(),
        connection.uuid(),
        &ids,
        &numbers,
        &outcomes as _,
        &quotas as _,
        &categories as _,
    )
    .fetch_all(&mut *tx)
    .await?;
    let reservations: Vec<Reservation> = closed
        .iter()
        .map(|row| Reservation {
            day: row.reserved_day,
            scope: row.quota_scope_id,
            recipients: row.recipient_count,
            consumed: row.quota_state == Quota::Consumed.as_str(),
        })
        .collect();
    finish::settle(&mut tx, workspace, connection, &reservations).await?;

    let uncertain: Vec<Uuid> = decided
        .iter()
        .filter(|(_, next)| next.state == MessageState::Uncertain)
        .map(|(row, _)| row.message_id)
        .collect();
    let requeued: Vec<Uuid> = decided
        .iter()
        .filter(|(_, next)| next.state != MessageState::Uncertain)
        .map(|(row, _)| row.message_id)
        .collect();
    if !uncertain.is_empty() {
        sqlx::query!(
            "UPDATE messages SET state = 'uncertain',
                    status_detail = 'The sender stopped while the message was being submitted: it may have been sent, and it is never resent automatically.'
              WHERE workspace_id = $1 AND id = ANY($2)",
            workspace.uuid(),
            &uncertain,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM delivery_queue WHERE workspace_id = $1 AND message_id = ANY($2)",
            workspace.uuid(),
            &uncertain,
        )
        .execute(&mut *tx)
        .await?;
    }
    if !requeued.is_empty() {
        sqlx::query!(
            "UPDATE messages SET state = 'queued' WHERE workspace_id = $1 AND id = ANY($2)",
            workspace.uuid(),
            &requeued,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE delivery_queue
                SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL, submission_started_at = NULL,
                    reserved_day = NULL
              WHERE workspace_id = $1 AND message_id = ANY($2)",
            workspace.uuid(),
            &requeued,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE connections SET next_claim_at = NULL WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            connection.uuid(),
        )
        .execute(&mut *tx)
        .await?;
    }
    let now = crate::process::now();
    for message in &uncertain {
        let attempt = closed
            .iter()
            .find(|row| row.message_id == *message)
            .map(|row| Id::<Attempt>::from_uuid(row.id));
        evidence::tell(
            &mut tx,
            workspace,
            EventType::MessageUncertain,
            Id::<Message>::from_uuid(*message),
            attempt,
            Some(Category::Uncertain),
            now,
        )
        .await?;
    }
    let check =
        !uncertain.is_empty() && provider.parse::<Provider>().is_ok_and(Provider::is_mailbox);
    if check {
        jobs::enqueue(&mut tx, workspace, &ConnectionCheck { connection }, None).await?;
    }
    tx.commit().await?;
    for message in &uncertain {
        evidence::report_failure(
            workspace,
            Id::<Message>::from_uuid(*message),
            MessageState::Uncertain,
            Some(Category::Uncertain),
        );
    }
    if check {
        jobs::wake(db, Queue::Maintenance).await;
    }
    Ok(Recovered {
        requeued: requeued.len(),
        uncertain: uncertain.len(),
    })
}

#[cfg(test)]
mod tests;
