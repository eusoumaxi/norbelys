//! The Start: the last decision before one message's submission, and its marker.
//!
//! Immediately before a message is handed to its transport, in one short transaction:
//!
//! 1. **The deadline first.** The sender fixes the submission's deadline on its own monotonic
//!    clock before it asks for the Start: that instant plus the transport's budget (SMTP 300
//!    seconds, an HTTP API 20). Every later delay (the transaction, its acknowledgement, a paused
//!    process) is spent from that same deadline, which ends at least 30 seconds before the lease
//!    the Start renews, so a submission never outlives the lease that protects it.
//! 2. **The locks every removal also takes, before reading what they guard**: the campaign's row
//!    `FOR SHARE` (campaign mail), the connection's `FOR UPDATE`, the sender identity's
//!    `FOR SHARE`, then the queue row. A removal (an identity disabled, untagged or taken out of a
//!    pool, a connection archived, a campaign paused) committed before these locks makes the Start
//!    see it; one that comes after waits for the Start, and finds the message in flight. The
//!    transaction is bounded (`transaction_timeout` 10 s), so a Start never holds them long.
//! 3. **The final checks** (`domain::policy::delivery::start_verdict`): the deadline, the
//!    workspace, the recipients' suppressions and holds, the connection, the identity, the
//!    campaign, its enrollment and pool, the send windows (campaign mail only: mail created
//!    through the API is not held by them), a cold message's pacing clock, the breakers. A check
//!    that fails returns the message to the queue unstarted (its reservation released once, the
//!    connection's budget wait cleared), or ends it without a submission.
//! 4. **The marker, last.** A cold message on a paced sender moves the clock once, to
//!    `next_phase_at(max(scheduled + interval, started + interval − 30 s), phase)`; the message
//!    becomes `in_flight`; and, as the transaction's last statement, the queue row's lease is
//!    renewed against the statement's own clock (never the transaction's start) to that clock plus
//!    the budget plus 30 seconds, fenced by owner, generation, state and an unexpired lease, with
//!    the submission marker, the first submission's instant and the deadline it implies (the
//!    earlier of the message's own expiry and its first submission plus the retry window). No row:
//!    the claim was lost or replaced, the transaction rolls back, and nothing is submitted.
//!
//! The workspace's mode is read here too: a workspace in test mode submits through the fake
//! transport, so a switch to test mode after the claim never reaches a provider.
//!
//! Lock order, as every delivery path: the campaign's row → the connection's row → the identity's
//! row → the queue row, then the message's row → its attempt → the connection's ledger row, then
//! the scope's.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;
use uuid::Uuid;

use super::evidence;
use super::finish::{self, Reservation};
use crate::db::{Database, Tx};
use crate::domain::ids::{Connection, Id, Message, WorkspaceId};
use crate::domain::messages::{Kind as MessageKind, State as MessageState};
use crate::domain::policy::delivery::{
    self as policy, BreakerState, Category, Held, Probe, StartFacts, StartVerdict,
};
use crate::domain::schedule::{self, Window};
use crate::domain::senders::{SendWindow, Status};
use crate::domain::time::{Date, Timestamp};
use crate::webhooks::EventType;

/// How long the lease outlives the submission's budget.
const LEASE_MARGIN: Duration = Duration::from_secs(30);
/// How long the Start's transaction may hold the locks every removal also takes.
const BOUND: Duration = Duration::from_secs(10);

/// One Start's request.
#[derive(Debug, Clone, Copy)]
pub struct Start<'a> {
    /// The message's workspace.
    pub workspace: WorkspaceId,
    /// The connection it was claimed on.
    pub connection: Id<Connection>,
    /// The message.
    pub message: Id<Message>,
    /// The lease generation it was claimed in.
    pub generation: i64,
    /// The lease owner.
    pub owner: &'a str,
    /// The transport's budget for the whole submission.
    pub budget: Duration,
    /// How long after its first submission a message keeps being tried.
    pub retry_window: Duration,
}

/// A Start that marked its message: submit before `deadline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Begun {
    /// The submission's deadline on the sender's monotonic clock.
    pub deadline: Instant,
    /// The submission marker, the database's clock.
    pub started: Timestamp,
    /// The workspace is in test mode: submit through the fake transport.
    pub test_mode: bool,
    /// For a cold message on a paced sender, the instant its send was scheduled for: the sender
    /// role records how long after it the Start ran in the histogram
    /// `norbelys_delivery_cold_start_lag_seconds{provider}`.
    pub scheduled: Option<Timestamp>,
}

/// How a Start ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// Submit now.
    Submit(Begun),
    /// Returned to the queue unstarted.
    Returned(Held),
    /// Ended without a submission, in this state.
    Ended(MessageState),
    /// The lease was lost (expired, recovered or claimed again): nothing is submitted.
    Lost,
}

/// The message as the Start reads it (immutable columns, no lock).
struct MessageRow {
    kind: String,
    campaign_id: Option<Uuid>,
    enrollment_id: Option<Uuid>,
    sender_identity_id: Uuid,
    attempt_number: i32,
    recipients: Vec<String>,
}

/// Starts the submission of one claimed message (see the module).
///
/// # Errors
///
/// The database refused; nothing was submitted, and the lease expires into recovery.
pub async fn start(db: &Database, request: &Start<'_>) -> Result<Started, sqlx::Error> {
    // 1. The deadline, on the sender's own clock, before anything else.
    let deadline = Instant::now() + request.budget;
    let workspace = request.workspace;
    let mut tx = db.begin_in(workspace).await?;
    crate::db::set_transaction_timeout(&mut tx, BOUND).await?;
    let Some(message) = sqlx::query_as!(
        MessageRow,
        r#"SELECT kind, campaign_id, enrollment_id, sender_identity_id, attempt_number,
                  to_addresses || cc || bcc AS "recipients!"
             FROM messages WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        request.message.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(Started::Lost);
    };
    let kind = message
        .kind
        .parse::<MessageKind>()
        .unwrap_or(MessageKind::Direct);

    // 2. The locks, in order, before reading what they guard.
    let campaign = match message.campaign_id {
        Some(campaign) => sqlx::query!(
            "SELECT status, send_window, timezone, sender_tags FROM campaigns
              WHERE workspace_id = $1 AND id = $2 FOR SHARE",
            workspace.uuid(),
            campaign,
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| (row.status, row.send_window, row.timezone, row.sender_tags)),
        None => None,
    };
    let Some(connection) = sqlx::query!(
        r#"SELECT status, paused, next_send_at AS "next_send_at: Timestamp", send_interval_minutes, send_phase_seconds,
                  paused_until AS "paused_until: Timestamp", consecutive_failures, probe_message_id, probe_generation,
                  quota_scope_id, timezone, send_window
             FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        request.connection.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(Started::Lost);
    };
    let identity = sqlx::query!(
        r#"SELECT enabled, archived_at IS NOT NULL AS "archived!", tags FROM sender_identities
            WHERE workspace_id = $1 AND id = $2 FOR SHARE"#,
        workspace.uuid(),
        message.sender_identity_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let queue = sqlx::query!(
        r#"SELECT state, lease_owner, lease_generation, lease_expires_at > clock_timestamp() AS "live!", paced,
                  deadline_at AS "deadline_at: Timestamp"
             FROM delivery_queue WHERE workspace_id = $1 AND message_id = $2 FOR UPDATE"#,
        workspace.uuid(),
        request.message.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(queue) = queue.filter(|row| {
        row.state == "claimed"
            && row.lease_owner.as_deref() == Some(request.owner)
            && row.lease_generation == request.generation
            && row.live
    }) else {
        tx.rollback().await?;
        return Ok(Started::Lost);
    };

    // 3. The final checks.
    let now: Timestamp = sqlx::query_scalar!(r#"SELECT clock_timestamp() AS "now!: Timestamp""#)
        .fetch_one(&mut *tx)
        .await?;
    let workspace_row = sqlx::query!(
        r#"SELECT mode, deleted_at IS NOT NULL AS "deleted!" FROM workspaces WHERE id = $1"#,
        workspace.uuid(),
    )
    .fetch_one(&mut *tx)
    .await?;
    let keys: Vec<String> = message
        .recipients
        .iter()
        .map(|address| address.to_ascii_lowercase())
        .collect();
    let suppressed = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM suppressions WHERE workspace_id = $1 AND email_key = ANY($2)) AS "suppressed!""#,
        workspace.uuid(),
        &keys,
    )
    .fetch_one(&mut *tx)
    .await?;
    let held_until = sqlx::query_scalar!(
        r#"SELECT max(review_after) AS "until: Timestamp" FROM recipient_holds
            WHERE workspace_id = $1 AND email_key = ANY($2) AND message_id <> $3
              AND resolved_at IS NULL AND review_after > now()"#,
        workspace.uuid(),
        &keys,
        request.message.uuid(),
    )
    .fetch_one(&mut *tx)
    .await?;
    let scope = match connection.quota_scope_id {
        Some(scope) => sqlx::query!(
            r#"SELECT paused_until AS "paused_until: Timestamp", consecutive_failures, probe_message_id, probe_generation
                 FROM quota_scopes WHERE workspace_id = $1 AND id = $2"#,
            workspace.uuid(),
            scope,
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| {
            finish::breaker_of(
                row.consecutive_failures,
                row.paused_until,
                None,
                row.probe_message_id,
                row.probe_generation,
            )
        }),
        None => None,
    };
    let this = Probe {
        message: request.message.uuid(),
        generation: request.generation,
    };
    let blocks = |breaker: &policy::Breaker| match breaker.state(now.0) {
        BreakerState::Closed => false,
        BreakerState::Open => true,
        BreakerState::HalfOpen => breaker.probe != Some(this),
    };
    let connection_breaker = finish::breaker_of(
        connection.consecutive_failures,
        connection.paused_until,
        None,
        connection.probe_message_id,
        connection.probe_generation,
    );
    let breaker_blocks = blocks(&connection_breaker) || scope.as_ref().is_some_and(blocks);
    let (identity_usable, identity_tags) = identity.map_or((false, Vec::new()), |row| {
        (row.enabled && !row.archived, row.tags)
    });
    let mut campaign_usable = true;
    let mut windows_open = true;
    if kind == MessageKind::Campaign {
        let enrollment_active = match message.enrollment_id {
            Some(enrollment) => sqlx::query_scalar!(
                "SELECT status FROM enrollments WHERE workspace_id = $1 AND id = $2",
                workspace.uuid(),
                enrollment,
            )
            .fetch_optional(&mut *tx)
            .await?
            .is_some_and(|status| status == "active"),
            None => false,
        };
        let in_pool = match (message.campaign_id, &campaign) {
            (Some(campaign_id), Some((_, _, _, tags))) => {
                tags.iter().any(|tag| identity_tags.contains(tag))
                    || sqlx::query_scalar!(
                        r#"SELECT EXISTS (SELECT 1 FROM campaign_senders
                                           WHERE workspace_id = $1 AND campaign_id = $2 AND sender_identity_id = $3) AS "named!""#,
                        workspace.uuid(),
                        campaign_id,
                        message.sender_identity_id,
                    )
                    .fetch_one(&mut *tx)
                    .await?
            }
            _ => false,
        };
        let active = campaign
            .as_ref()
            .is_some_and(|(status, ..)| status == "active");
        campaign_usable = active && enrollment_active && in_pool;
        let campaign_window = campaign
            .as_ref()
            .and_then(|(_, window, zone, _)| window.as_ref().map(|window| (window, zone.as_str())));
        windows_open = open(
            campaign_window,
            connection.send_window.as_ref(),
            &connection.timezone,
            now,
        );
    }
    let cold = queue.paced && connection.send_interval_minutes.is_some();
    let facts = StartFacts {
        now: now.0,
        deadline_at: queue.deadline_at.map(|at| at.0),
        kind,
        workspace_deleted: workspace_row.deleted,
        connection_archived: connection.status == Status::Archived.as_str(),
        connection_usable: connection.status == Status::Active.as_str() && !connection.paused,
        suppressed,
        held_until: held_until.map(|at| at.0),
        identity_usable,
        campaign_usable,
        windows_open,
        clock_due: !cold || connection.next_send_at <= now,
        breaker_blocks,
    };

    match policy::start_verdict(&facts) {
        StartVerdict::Submit => {}
        StartVerdict::Return { run_at, why } => {
            release(
                &mut tx,
                request,
                message.attempt_number,
                run_at.map(Timestamp),
                why,
            )
            .await?;
            tx.commit().await?;
            return Ok(Started::Returned(why));
        }
        StartVerdict::End { state, category } => {
            end(&mut tx, request, message.attempt_number, state, category).await?;
            tx.commit().await?;
            evidence::report_failure(workspace, request.message, state, Some(category));
            return Ok(Started::Ended(state));
        }
    }

    // 4. The marker, last.
    let started: Timestamp =
        sqlx::query_scalar!(r#"SELECT clock_timestamp() AS "now!: Timestamp""#)
            .fetch_one(&mut *tx)
            .await?;
    let scheduled = cold.then_some(connection.next_send_at);
    if cold {
        sqlx::query!(
            "UPDATE connections
                SET next_send_at = next_phase_at(greatest(next_send_at + make_interval(mins => send_interval_minutes),
                                                          $3 + make_interval(mins => send_interval_minutes) - interval '30 seconds'),
                                                 send_phase_seconds)
              WHERE workspace_id = $1 AND id = $2 AND send_interval_minutes IS NOT NULL AND next_send_at <= $3",
            workspace.uuid(),
            request.connection.uuid(),
            started as _,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "UPDATE messages SET state = 'in_flight' WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        request.message.uuid(),
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE attempts SET smtp_started_at = $4
          WHERE workspace_id = $1 AND message_id = $2 AND attempt_number = $3 AND quota_state = 'reserved'",
        workspace.uuid(),
        request.message.uuid(),
        message.attempt_number,
        started as _,
    )
    .execute(&mut *tx)
    .await?;
    let lease = request.budget.saturating_add(LEASE_MARGIN).as_secs_f64();
    let renewed = sqlx::query_scalar!(
        r#"UPDATE delivery_queue
              SET state = 'in_flight', lease_expires_at = clock_timestamp() + make_interval(secs => $5),
                  submission_started_at = $6, first_submitted_at = coalesce(first_submitted_at, $6),
                  deadline_at = least(expires_at, coalesce(first_submitted_at, $6) + make_interval(secs => $7))
            WHERE workspace_id = $1 AND message_id = $2 AND lease_owner = $3 AND lease_generation = $4
              AND state = 'claimed' AND lease_expires_at > clock_timestamp()
        RETURNING 1 AS "renewed!""#,
        workspace.uuid(),
        request.message.uuid(),
        request.owner,
        request.generation,
        lease,
        started as _,
        request.retry_window.as_secs_f64(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    if renewed.is_none() {
        tx.rollback().await?;
        return Ok(Started::Lost);
    }
    tx.commit().await?;
    Ok(Started::Submit(Begun {
        deadline,
        started,
        test_mode: workspace_row.mode == "test",
        scheduled,
    }))
}

/// True when the campaign's window (and its zone) and the connection's are open together at
/// `now`; a window that cannot be read is closed.
fn open(
    campaign: Option<(&Value, &str)>,
    connection: Option<&Value>,
    connection_zone: &str,
    now: Timestamp,
) -> bool {
    let parse = |value: &Value, zone: &str| -> Option<Window> {
        let window: SendWindow = serde_json::from_value(value.clone()).ok()?;
        Window::new(&window, zone).ok()
    };
    let mut windows = Vec::new();
    for (value, zone) in campaign
        .into_iter()
        .chain(connection.map(|value| (value, connection_zone)))
    {
        match parse(value, zone) {
            Some(window) => windows.push(window),
            None => return false,
        }
    }
    let windows: Vec<&Window> = windows.iter().collect();
    schedule::open_together(&windows, now.0)
}

/// The attempt the Start closes, for settling.
struct Closed {
    reserved_day: Date,
    quota_scope_id: Option<Uuid>,
    recipient_count: i32,
}

/// Closes the message's attempt with `outcome` and `category` and releases its reservation,
/// once: only a still-reserved attempt closes and settles.
async fn close(
    tx: &mut Tx,
    request: &Start<'_>,
    attempt_number: i32,
    outcome: policy::Outcome,
    category: Option<Category>,
    diagnostic: &str,
) -> Result<(), sqlx::Error> {
    let closed = sqlx::query_as!(
        Closed,
        r#"UPDATE attempts SET outcome = $4, finished_at = now(), quota_state = 'released', category = $5, diagnostic = $6
            WHERE workspace_id = $1 AND message_id = $2 AND attempt_number = $3 AND quota_state = 'reserved'
        RETURNING reserved_day AS "reserved_day: Date", quota_scope_id, recipient_count"#,
        request.workspace.uuid(),
        request.message.uuid(),
        attempt_number,
        outcome.as_str(),
        category.map(Category::as_str),
        diagnostic,
    )
    .fetch_all(&mut **tx)
    .await?;
    let reservations: Vec<Reservation> = closed
        .into_iter()
        .map(|row| Reservation {
            day: row.reserved_day,
            scope: row.quota_scope_id,
            recipients: row.recipient_count,
            consumed: false,
        })
        .collect();
    finish::settle(tx, request.workspace, request.connection, &reservations).await?;
    // A release frees budget: the connection's budget wait ends.
    sqlx::query!(
        "UPDATE connections SET next_claim_at = NULL WHERE workspace_id = $1 AND id = $2",
        request.workspace.uuid(),
        request.connection.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Returns the message to the queue unstarted, due at `run_at` (unchanged when `None`).
async fn release(
    tx: &mut Tx,
    request: &Start<'_>,
    attempt_number: i32,
    run_at: Option<Timestamp>,
    why: Held,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE delivery_queue
            SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL, submission_started_at = NULL,
                reserved_day = NULL, run_at = coalesce($3, run_at)
          WHERE workspace_id = $1 AND message_id = $2",
        request.workspace.uuid(),
        request.message.uuid(),
        run_at as _,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE messages SET state = 'queued' WHERE workspace_id = $1 AND id = $2",
        request.workspace.uuid(),
        request.message.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    let reason: &'static str = why.into();
    close(
        tx,
        request,
        attempt_number,
        policy::Outcome::Released,
        None,
        &format!("returned unstarted: {reason}"),
    )
    .await
}

/// Ends the message without a submission, in `state`, for `category`.
async fn end(
    tx: &mut Tx,
    request: &Start<'_>,
    attempt_number: i32,
    state: MessageState,
    category: Category,
) -> Result<(), sqlx::Error> {
    let detail = match category {
        Category::Expired => "It expired before it could be sent.",
        Category::Suppressed => "A recipient is suppressed: nothing is sent to it.",
        Category::SenderArchived => "Its sender was archived.",
        Category::WorkspaceDeleted => "Its workspace was deleted.",
        _ => "It was not sent.",
    };
    sqlx::query!(
        "DELETE FROM delivery_queue WHERE workspace_id = $1 AND message_id = $2",
        request.workspace.uuid(),
        request.message.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE messages SET state = $3, status_detail = $4 WHERE workspace_id = $1 AND id = $2",
        request.workspace.uuid(),
        request.message.uuid(),
        state.as_str(),
        detail,
    )
    .execute(&mut **tx)
    .await?;
    if category == Category::Expired {
        sqlx::query!(
            "UPDATE recipient_holds SET resolved_at = now(), resolution = 'expired'
              WHERE workspace_id = $1 AND message_id = $2 AND resolved_at IS NULL",
            request.workspace.uuid(),
            request.message.uuid(),
        )
        .execute(&mut **tx)
        .await?;
    }
    let outcome = if state == MessageState::Suppressed {
        policy::Outcome::Suppressed
    } else {
        policy::Outcome::Skipped
    };
    close(tx, request, attempt_number, outcome, Some(category), detail).await?;
    evidence::tell(
        tx,
        request.workspace,
        EventType::MessageFailed,
        request.message,
        None,
        Some(category),
        crate::process::now(),
    )
    .await
}

#[cfg(test)]
mod tests;
