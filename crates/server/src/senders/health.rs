//! A connection's health: moving its status by the one health table, and telling customers.
//!
//! [`crate::domain::senders::transition`] decides; [`apply`] writes the new status and its
//! detail on a row the caller has locked and records `connection.health_changed` in the same
//! transaction, so the event exists exactly when the change commits. Every change of a
//! connection's `status` or `paused` is told, whoever made it (a person, a check, a provisioning
//! job), with the connection's id, its status, its detail and whether it is paused: consumers
//! filter what they care about (authorization required, disabled, archived, recovered).
//!
//! Becoming `active` also sets a paced sender's pacing clock: its next cold send is the first
//! phase instant from now, or later if the clock already points later (the clock only ever
//! moves forward). A rate-paced connection's clock is never moved.
//!
//! # Telling people by email
//!
//! A move of the table to a status people are told about (`domain::senders::told`: the
//! connection stops working, or works) also enqueues the [`HealthEmail`] job of the workspace's
//! current 15-minute window (UTC, starting on the quarter hour), keyed by the window, so every
//! move of the window coalesces into that one job. The job waits until the window has closed and
//! the longest transaction that could still record a move in it has ended (90 seconds later;
//! transactions are cut at 60), reads the window's `connection.health_changed` events from the
//! outbox, and sends one email per person, listing each connection as the window left it (its
//! latest event there), recoveries included. The workspace's active owners and admins are told
//! about every listed connection, an active member who created one about theirs. A workspace
//! administrator who blocks the app breaks hundreds of mailboxes at once, and that is one email
//! per person, not hundreds. Changes that are not moves of the table (pausing, creating,
//! archiving) enqueue nothing, though a pause in a window that has a job shows in its list.
//!
//! The window is computed by the database from the transaction's `now()`, the instant the
//! event's row records as `created_at`, so an event and its job always agree on the window. The
//! job accepts every email of the window as transactional mail of the `system` workspace in its
//! one chunk, so a run that fails sends nothing and its retry sends each email once.
//!
//! Lock order: the caller's connection row, then the job's lane and row, taken last as by every
//! enqueue.

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::accept::{self, HealthLine, Transactional};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Connection, Id, WorkspaceId};
use crate::domain::senders::{HealthEvent, Status, Transition, told, transition};
use crate::domain::time::Timestamp;
use crate::identity::memberships;
use crate::jobs::{self, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// Records `connection.health_changed` for a connection whose status or pause just changed,
/// inside the caller's transaction.
///
/// # Errors
///
/// The database refused the row.
pub async fn changed(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    status: Status,
    detail: Option<&str>,
    paused: bool,
) -> Result<(), sqlx::Error> {
    outbox::record(
        tx,
        workspace,
        Event {
            kind: EventType::ConnectionHealthChanged,
            subject_type: "connection",
            subject_id: connection.uuid(),
            data: json!({
                "connection_id": connection,
                "status": status.as_str(),
                "status_detail": detail,
                "paused": paused,
            }),
        },
    )
    .await?;
    Ok(())
}

/// Applies `event` to a connection in `current` status, whose row the caller has locked: when
/// the table moves it, writes the new status with `detail`, sets a paced sender's clock when it
/// becomes `active`, records the change, and enqueues the window's [`HealthEmail`] when people
/// are told about the new status. Writes nothing when the status stays.
///
/// # Errors
///
/// The database refused.
pub async fn apply(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    current: Status,
    paused: bool,
    event: HealthEvent,
    detail: Option<&str>,
) -> Result<Transition, sqlx::Error> {
    let decided = transition(current, event);
    let Transition::To(next) = decided else {
        return Ok(decided);
    };
    sqlx::query!(
        "UPDATE connections SET status = $3, status_detail = $4,
                next_send_at = CASE WHEN $3 = 'active' AND send_interval_minutes IS NOT NULL
                                    THEN greatest(next_send_at, next_phase_at(now(), send_phase_seconds::int))
                                    ELSE next_send_at END
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        connection.uuid(),
        next.as_str(),
        detail,
    )
    .execute(&mut **tx)
    .await?;
    changed(tx, workspace, connection, next, detail, paused).await?;
    if told(next) {
        tell(tx, workspace).await?;
    }
    Ok(decided)
}

/// Enqueues the [`HealthEmail`] of the transaction's 15-minute window, which coalesces with the
/// window's job when it exists (see the module).
async fn tell(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), sqlx::Error> {
    let window = sqlx::query!(
        r#"SELECT date_bin('15 minutes', now(), '1970-01-01T00:00:00Z') AS "start!: Timestamp",
                  date_bin('15 minutes', now(), '1970-01-01T00:00:00Z')
                    + interval '16 minutes 30 seconds' AS "run_at!: Timestamp""#
    )
    .fetch_one(&mut **tx)
    .await?;
    jobs::enqueue(
        tx,
        workspace,
        &HealthEmail {
            window: window.start,
        },
        Some(window.run_at),
    )
    .await?;
    Ok(())
}

/// `connection.health_email`: tells people about the moves of a workspace's connections in one
/// 15-minute window (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthEmail {
    /// The window's start: a quarter hour, UTC.
    pub window: Timestamp,
}

impl Job for HealthEmail {
    const KIND: &'static str = "connection.health_email";
    const QUEUE: Queue = Queue::Transactional;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.window.0.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        // A run whose chunk committed has sent everything: a recovered lease ends here.
        if cx.progress().is_some() {
            return Ok(Outcome::Done);
        }
        let keys = cx.env::<Keys>()?.clone();
        let workspace = cx.workspace();
        let mut chunk = cx.begin().await?;
        let sent = notify(chunk.tx(), &keys, workspace, self.window).await?;
        cx.checkpoint(chunk, json!({ "sent": sent })).await?;
        if sent > 0 {
            accept::wake(cx.db()).await;
        }
        Ok(Outcome::Done)
    }
}

/// A connection the window's email lists, and who created it.
struct Listed {
    created_by: Option<Uuid>,
    line: HealthLine,
}

/// Accepts the emails of `workspace`'s `window` inside `tx`, one per person told, and answers
/// how many (see the module).
async fn notify(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    window: Timestamp,
) -> Result<usize, JobError> {
    let Some(place) = sqlx::query!(
        r#"SELECT name, $2::timestamptz + interval '15 minutes' AS "until!: Timestamp",
                  now() + interval '1 day' AS "expires_at!: Timestamp"
             FROM workspaces WHERE id = $1 AND deleted_at IS NULL"#,
        workspace.uuid(),
        window as _,
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(0);
    };
    // The window's events by id first (the outbox is partitioned by it; an event's id is taken
    // when its row is written, at most a transaction's 60 seconds after the `now()` it records),
    // then exactly by `created_at`.
    let rows = sqlx::query!(
        r#"WITH latest AS (
               SELECT DISTINCT ON (subject_id) subject_id, payload -> 'data' AS data
                 FROM outbox_events
                WHERE workspace_id = $1 AND type = $3
                  AND id >= uuidv7_boundary($2) AND id < uuidv7_boundary($2 + interval '16 minutes 30 seconds')
                  AND created_at >= $2 AND created_at < $2 + interval '15 minutes'
                ORDER BY subject_id, id DESC)
           SELECT c.account_email, c.provider, c.created_by,
                  coalesce(l.data ->> 'status', '') AS "status!",
                  l.data ->> 'status_detail' AS detail,
                  coalesce((l.data ->> 'paused')::boolean, false) AS "paused!"
             FROM latest l
             JOIN connections c ON c.workspace_id = $1 AND c.id = l.subject_id
            ORDER BY c.account_email_key, c.id"#,
        workspace.uuid(),
        window as _,
        EventType::ConnectionHealthChanged.as_str(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let listed: Vec<Listed> = rows
        .into_iter()
        .filter_map(|row| {
            let status = row
                .status
                .parse::<Status>()
                .ok()
                .filter(|status| told(*status))?;
            Some(Listed {
                created_by: row.created_by,
                line: HealthLine {
                    account: row.account_email,
                    provider: row.provider,
                    status: label(status).to_owned(),
                    detail: row.detail,
                    paused: row.paused,
                },
            })
        })
        .collect();
    if listed.is_empty() {
        return Ok(0);
    }
    let creators: Vec<Uuid> = listed.iter().filter_map(|item| item.created_by).collect();
    let mut sent = 0;
    for person in memberships::told(tx, workspace, &creators).await? {
        let everything = person.admin();
        let lines: Vec<HealthLine> = listed
            .iter()
            .filter(|item| everything || item.created_by == Some(person.user.uuid()))
            .map(|item| item.line.clone())
            .collect();
        let Ok(to) = EmailAddress::parse(&person.email) else {
            continue;
        };
        if lines.is_empty() {
            continue;
        }
        accept::transactional(
            tx,
            keys,
            &Transactional::ConnectionHealth {
                to: &to,
                workspace_name: &place.name,
                from: window,
                until: place.until,
                connections: &lines,
                expires_at: place.expires_at,
            },
        )
        .await
        .map_err(|error| match error {
            accept::Error::Db(error) => JobError::Db(error),
            other => JobError::Failed(other.to_string()),
        })?;
        sent += 1;
    }
    Ok(sent)
}

/// What `status` means for a person reading a notice.
fn label(status: Status) -> &'static str {
    match status {
        Status::Active => "working",
        Status::AuthorizationRequired => "needs to be connected again",
        Status::Failed => "could not be set up",
        Status::Disabled => "blocked by its provider or its account's administrator",
        Status::Verifying => "being checked",
        Status::Unverified => "not checked yet",
        Status::Archived => "archived",
    }
}

#[cfg(test)]
mod tests;
