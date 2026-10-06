//! The claim: which connections are due, and which of their queued messages a sender takes, under
//! a lease, with their budget reserved.
//!
//! # Turns and sweeps
//!
//! Workspaces take turns: a sender takes the turn of the workspace whose `sender_turn_at` is
//! oldest among those with due work ([`turn`]), moves it to the back, and claims for up to a page
//! of eight of its due connections ([`page`]), one transaction each ([`claim`]). Two scans find
//! them, read without a lock as the scheduler role (the routing columns of every workspace,
//! nothing else):
//!
//! - **the clock scan**: connections whose pacing clock is due (a paced sender with nothing in
//!   progress), or rate-paced connections with a due row (their clock never moves, so they sort
//!   first), by `(next_send_at, id)`;
//! - **the API scan**: connections with mail created through the API due, by `id`, whatever their
//!   clock, with nothing in progress on a paced sender.
//!
//! Both skip what cannot send: paused, an open breaker, a paused quota scope, a spent budget's
//! wait. Each scan pages after this replica's cursor for the workspace, kept in memory
//! ([`Cursors`]), and fixes a cutoff when its sweep starts: only connections that existed and were
//! due by then are members, so a connection the sweep pushes (a clock moved, a newcomer) leaves it
//! and every sweep ends; a page shorter than eight ends the sweep, and the next turn starts a new
//! one with a new cutoff. A replica that dies loses only its cursors.
//!
//! # One claim
//!
//! In one transaction, in the lock order of every delivery path:
//!
//! 1. As the scheduler: the quota scope's row, only when it is half-open (a probe may have to be
//!    admitted), then the connection's row, both `FOR UPDATE SKIP LOCKED`: a row another replica
//!    holds is skipped, never waited for. The eligibility is checked again under the locks, and a
//!    half-open breaker admits one message only if no probe is live (the queue row and lease
//!    generation it names still leased).
//! 2. As the worker, inside the workspace: the budgets (`domain::policy::budget`): the daily
//!    budget against today's UTC bucket, the provider cap and the scope's units against today's
//!    and yesterday's. A spent budget records the connection's wait (`next_claim_at`) and the
//!    claim ends, its clock untouched.
//! 3. The rows: on a paced sender at most one message, its oldest due mail created through the
//!    API first, else one cold message when its clock is due and the campaign's and the
//!    connection's send windows are open; on a rate-paced connection, due rows in order up to the
//!    allowance, a campaign row whose windows are closed pushed to their next opening rather than
//!    claimed (so it is not taken and returned at every sweep). Each row is charged to the
//!    in-process limiters it falls under; a refusal ends the wave there, and a claim that takes
//!    nothing writes nothing.
//! 4. The writes: the rows `claimed` under a new lease generation (120 seconds), their messages
//!    `claimed` with their next attempt number, one attempt each with its reservation day and the
//!    scope it reserves on (frozen), the connection's bucket of the day grown, then the scope's
//!    units checked and reserved in one statement over both buckets, locked in day order, so two
//!    connections of one scope never both take its last units: no row and the whole claim rolls
//!    back. A probe is recorded on the breaker that admitted it.
//! 5. A paced sender with nothing in progress and no cold message it may send now has its clock
//!    moved forward only: to its next phase instant (an idle mailbox is looked at once a slot), or
//!    to the first phase instant at which its due cold mail's windows open.
//!
//! Every day the ledgers name is a UTC day, computed once per claim.
//!
//! Lock order, as every delivery path: the quota scope's row → the connection's row → queue rows,
//! then message rows → attempts → the connection's ledger row, then the scope's, in day order.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use serde_json::Value;
use uuid::Uuid;

use crate::db::{self, Database, Tx};
use crate::domain::ids::{Connection, Id, Message, WorkspaceId};
use crate::domain::policy::budget::{self, Bucket, Budgets, ScopeBudget, Take};
use crate::domain::policy::delivery::{self as policy, Admission};
use crate::domain::schedule::{self, Window};
use crate::domain::senders::{Provider, SendWindow};
use crate::domain::time::{Date, Timestamp};

/// A claim's lease before its Start renews it: long enough for preflight, rendering and a
/// session, short enough that a lost sender's unstarted work returns quickly.
pub const LEASE: Duration = Duration::from_secs(120);
/// Candidates per page of a scan.
pub const PAGE: i64 = 8;
/// Cold rows a paced sender's claim looks at to find one whose windows are open.
const COLD_LOOKAHEAD: i64 = 20;
/// The most rows one claim of a rate-paced connection takes.
const WAVE_MAX: i64 = 200;

static CLAIMS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_delivery_claims_total")
        .with_description("Claims that took a wave of messages, by provider.")
        .build()
});

static PACING_WAIT: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_delivery_pacing_wait_seconds")
        .with_unit("s")
        .with_description(
            "How long a claimed message waited after it was due (its clock, budgets, limiters, \
             breakers, a free slot), by provider.",
        )
        .with_boundaries(vec![
            1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1_800.0, 3_600.0, 14_400.0, 86_400.0,
        ])
        .build()
});

/// A replica's place in each workspace's sweeps, in memory.
#[derive(Debug, Default)]
pub struct Cursors {
    clock: HashMap<WorkspaceId, ClockSweep>,
    api: HashMap<WorkspaceId, ApiSweep>,
}

#[derive(Debug, Clone, Copy)]
struct ClockSweep {
    after_at: Timestamp,
    after_id: Uuid,
    sweep_at: Timestamp,
}

#[derive(Debug, Clone, Copy)]
struct ApiSweep {
    after_id: Uuid,
    sweep_at: Timestamp,
}

/// Which scan found a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    /// The clock scan.
    Clock,
    /// The API scan.
    Api,
}

/// A connection one of the scans found due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// The connection.
    pub connection: Id<Connection>,
    /// The scan that found it.
    pub scan: Scan,
    /// Its key in that scan, for the cursor.
    key: (Timestamp, Uuid),
    /// Its quota scope is half-open: the claim locks the scope's row first.
    scope_half_open: bool,
}

/// One page of each scan, and whether each page ended its sweep.
#[derive(Debug, Clone, Default)]
pub struct Page {
    /// The clock scan's candidates, then the API scan's, in their orders.
    pub candidates: Vec<Candidate>,
    /// The clock scan's page was short: its sweep ends once its candidates are reached.
    pub clock_ended: bool,
    /// The API scan's page was short.
    pub api_ended: bool,
}

impl Cursors {
    /// Records that the turn reached `candidate`: the next page of its scan starts after it.
    pub fn reached(&mut self, candidate: &Candidate) {
        match candidate.scan {
            Scan::Clock => {
                if let Some(sweep) = self.clock.get_mut(&candidate.workspace) {
                    sweep.after_at = candidate.key.0;
                    sweep.after_id = candidate.key.1;
                }
            }
            Scan::Api => {
                if let Some(sweep) = self.api.get_mut(&candidate.workspace) {
                    sweep.after_id = candidate.key.1;
                }
            }
        }
    }

    /// Ends a workspace's sweep of `scan`: its next turn starts a new one, with a new cutoff.
    pub fn end(&mut self, workspace: WorkspaceId, scan: Scan) {
        match scan {
            Scan::Clock => self.clock.remove(&workspace),
            Scan::Api => {
                self.api.remove(&workspace);
                None
            }
        };
    }
}

/// Takes the turn of the workspace whose turn is oldest among those with due work, and moves it
/// to the back; `None` when no workspace has any. Read as the scheduler; a turn another replica is
/// taking at the same instant is skipped.
///
/// # Errors
///
/// The database is unavailable.
pub async fn turn(db: &Database) -> Result<Option<WorkspaceId>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let workspace = sqlx::query_scalar!(
        "SELECT d.workspace_id FROM dispatch_workspaces d
          WHERE EXISTS (
                SELECT 1 FROM connections c LEFT JOIN quota_scopes s ON (s.workspace_id, s.id) = (c.workspace_id, c.quota_scope_id)
                 WHERE c.workspace_id = d.workspace_id AND c.status = 'active' AND NOT c.paused
                   AND (c.next_claim_at IS NULL OR c.next_claim_at <= now())
                   AND (c.paused_until IS NULL OR c.paused_until <= now())
                   AND (s.paused_until IS NULL OR s.paused_until <= now())
                   AND ((c.next_send_at <= now()
                         AND CASE WHEN c.send_interval_minutes IS NULL
                                  THEN EXISTS (SELECT 1 FROM delivery_queue q WHERE (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
                                                                               AND q.state = 'queued' AND q.run_at <= now())
                                  ELSE NOT EXISTS (SELECT 1 FROM delivery_queue p WHERE (p.workspace_id, p.connection_id) = (c.workspace_id, c.id)
                                                                                   AND p.state <> 'queued') END)
                        OR (EXISTS (SELECT 1 FROM delivery_queue q WHERE (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
                                                                     AND q.state = 'queued' AND NOT q.paced AND q.run_at <= now())
                            AND (c.send_interval_minutes IS NULL
                                 OR NOT EXISTS (SELECT 1 FROM delivery_queue p WHERE (p.workspace_id, p.connection_id) = (c.workspace_id, c.id)
                                                                                AND p.state <> 'queued')))))
          ORDER BY d.sender_turn_at
          LIMIT 1
            FOR UPDATE OF d SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(workspace) = workspace {
        sqlx::query!(
            "UPDATE dispatch_workspaces SET sender_turn_at = now() WHERE workspace_id = $1",
            workspace,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(workspace.map(WorkspaceId::trusted))
}

/// The next page of each scan for `workspace`, after this replica's cursors (a new sweep, with its
/// cutoff now, when none is under way). Read without a lock as the scheduler; nothing is written.
///
/// # Errors
///
/// The database is unavailable.
pub async fn page(
    db: &Database,
    workspace: WorkspaceId,
    cursors: &mut Cursors,
) -> Result<Page, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let now: Timestamp = sqlx::query_scalar!(r#"SELECT now() AS "now!: Timestamp""#)
        .fetch_one(&mut *tx)
        .await?;
    let clock = *cursors.clock.entry(workspace).or_insert(ClockSweep {
        // Before every clock: a new sweep starts at the beginning. The Unix epoch rather than
        // jiff's minimum, which PostgreSQL's `timestamptz` cannot represent; no pacing clock is
        // ever set before 1970.
        after_at: Timestamp(jiff::Timestamp::UNIX_EPOCH),
        after_id: Uuid::nil(),
        sweep_at: now,
    });
    let api = *cursors.api.entry(workspace).or_insert(ApiSweep {
        after_id: Uuid::nil(),
        sweep_at: now,
    });
    let clock_rows = sqlx::query!(
        r#"SELECT c.id, c.next_send_at AS "next_send_at: Timestamp",
                  (s.id IS NOT NULL AND s.consecutive_failures > 0 AND s.paused_until <= now()) AS "scope_half_open!"
             FROM connections c LEFT JOIN quota_scopes s ON (s.workspace_id, s.id) = (c.workspace_id, c.quota_scope_id)
            WHERE c.workspace_id = $1 AND c.status = 'active' AND NOT c.paused
              AND c.created_at <= $2 AND c.next_send_at <= $2 AND (c.next_claim_at IS NULL OR c.next_claim_at <= now())
              AND (c.paused_until IS NULL OR c.paused_until <= now())
              AND (s.paused_until IS NULL OR s.paused_until <= now())
              AND CASE WHEN c.send_interval_minutes IS NULL
                       THEN EXISTS (SELECT 1 FROM delivery_queue q WHERE (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
                                                                    AND q.state = 'queued' AND q.run_at <= now())
                       ELSE NOT EXISTS (SELECT 1 FROM delivery_queue p WHERE (p.workspace_id, p.connection_id) = (c.workspace_id, c.id)
                                                                        AND p.state <> 'queued') END
              AND (c.next_send_at, c.id) > ($3, $4)
            ORDER BY c.next_send_at, c.id
            LIMIT $5"#,
        workspace.uuid(),
        clock.sweep_at as _,
        clock.after_at as _,
        clock.after_id,
        PAGE,
    )
    .fetch_all(&mut *tx)
    .await?;
    let api_rows = sqlx::query!(
        r#"SELECT c.id,
                  (s.id IS NOT NULL AND s.consecutive_failures > 0 AND s.paused_until <= now()) AS "scope_half_open!"
             FROM connections c LEFT JOIN quota_scopes s ON (s.workspace_id, s.id) = (c.workspace_id, c.quota_scope_id)
            WHERE c.workspace_id = $1 AND c.status = 'active' AND NOT c.paused AND c.created_at <= $2
              AND (c.next_claim_at IS NULL OR c.next_claim_at <= now())
              AND (c.paused_until IS NULL OR c.paused_until <= now())
              AND (s.paused_until IS NULL OR s.paused_until <= now())
              AND EXISTS (SELECT 1 FROM delivery_queue q WHERE (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
                                                           AND q.state = 'queued' AND NOT q.paced AND q.run_at <= $2)
              AND (c.send_interval_minutes IS NULL
                   OR NOT EXISTS (SELECT 1 FROM delivery_queue p WHERE (p.workspace_id, p.connection_id) = (c.workspace_id, c.id)
                                                                  AND p.state <> 'queued'))
              AND c.id > $3
            ORDER BY c.id
            LIMIT $4"#,
        workspace.uuid(),
        api.sweep_at as _,
        api.after_id,
        PAGE,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let full = usize::try_from(PAGE).unwrap_or(usize::MAX);
    let mut page = Page {
        clock_ended: clock_rows.len() < full,
        api_ended: api_rows.len() < full,
        candidates: Vec::with_capacity(clock_rows.len() + api_rows.len()),
    };
    page.candidates
        .extend(clock_rows.into_iter().map(|row| Candidate {
            workspace,
            connection: Id::from_uuid(row.id),
            scan: Scan::Clock,
            key: (row.next_send_at, row.id),
            scope_half_open: row.scope_half_open,
        }));
    page.candidates
        .extend(api_rows.into_iter().map(|row| Candidate {
            workspace,
            connection: Id::from_uuid(row.id),
            scan: Scan::Api,
            key: (now, row.id),
            scope_half_open: row.scope_half_open,
        }));
    Ok(page)
}

/// A quota scope's short-window rate, which the in-process limiters hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeWindow {
    /// The scope.
    pub scope: Uuid,
    /// Units a window.
    pub limit: i32,
    /// What one submission is charged: `requests`, `recipients` or `units`.
    pub unit: String,
    /// The window's length.
    pub seconds: i32,
}

/// What the in-process limiters are asked for one message.
#[derive(Debug, Clone, Copy)]
pub struct Charge<'a> {
    /// The connection.
    pub connection: Id<Connection>,
    /// Its provider.
    pub provider: Provider,
    /// Its scope's rate, when it has one.
    pub window: Option<&'a ScopeWindow>,
    /// The message's envelope recipients.
    pub recipients: i32,
}

/// One message a claim took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claimed {
    /// The message.
    pub message: Id<Message>,
    /// The lease generation it was claimed in: the fence of every later write.
    pub generation: i64,
    /// Its attempt number.
    pub attempt_number: i32,
    /// Cold campaign mail (on a paced sender it follows the clock).
    pub paced: bool,
}

/// The messages one claim took from one connection: a wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wave {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The connection.
    pub connection: Id<Connection>,
    /// Its provider.
    pub provider: Provider,
    /// It is a paced sender.
    pub paced_sender: bool,
    /// The one message was admitted as a breaker's probe.
    pub probe: bool,
    /// The messages, in due order.
    pub messages: Vec<Claimed>,
    /// The W3C `traceparent` of each request that created one of them, which the wave's span
    /// links to (messages created by other work have none).
    pub trace_parents: Vec<String>,
}

/// How one claim ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Messages were claimed.
    Wave(Wave),
    /// Nothing was claimed: another replica holds the connection, it is no longer eligible, a
    /// breaker admits nothing, a limiter refused, or nothing is due. Nothing was written but,
    /// for an idle paced sender, its clock moved forward.
    Nothing,
    /// A budget is spent: the connection waits until then (`next_claim_at`).
    Spent { until: Timestamp },
}

/// The connection's routing row, read as the scheduler under its lock.
struct Routing {
    daily_limit: i32,
    send_interval_minutes: Option<i32>,
    send_phase_seconds: i16,
    warmup_stage: Option<i16>,
    next_send_at: Timestamp,
    next_claim_at: Option<Timestamp>,
    paused_until: Option<Timestamp>,
    consecutive_failures: i32,
    probe_message_id: Option<Uuid>,
    probe_generation: Option<i64>,
    quota_scope_id: Option<Uuid>,
    timezone: String,
    send_window: Option<Value>,
}

/// A due row a claim looks at, locked.
struct Due {
    message_id: Uuid,
    campaign_id: Option<Uuid>,
    recipient_count: i32,
}

/// Claims for `candidate` as `owner`, with `free_slots` submission slots free in this replica,
/// charging each message to the in-process limiters through `charge` (see the module).
///
/// # Errors
///
/// The database refused; nothing of the claim is committed.
pub async fn claim(
    db: &Database,
    owner: &str,
    candidate: &Candidate,
    free_slots: u32,
    charge: &mut (dyn FnMut(&Charge<'_>) -> bool + Send),
) -> Result<Claim, sqlx::Error> {
    let started = Instant::now();
    let workspace = candidate.workspace;
    let connection = candidate.connection;
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;

    // 1. The scope (only when half-open), then the connection, both skipped when held.
    if candidate.scope_half_open {
        let locked = sqlx::query_scalar!(
            "SELECT s.id FROM connections c JOIN quota_scopes s ON (s.workspace_id, s.id) = (c.workspace_id, c.quota_scope_id)
              WHERE c.workspace_id = $1 AND c.id = $2
                FOR UPDATE OF s SKIP LOCKED",
            workspace.uuid(),
            connection.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if locked.is_none() {
            return nothing(tx).await;
        }
    }
    let Some(routing) = sqlx::query_as!(
        Routing,
        r#"SELECT daily_limit, send_interval_minutes, send_phase_seconds, warmup_stage,
                  next_send_at AS "next_send_at: Timestamp", next_claim_at AS "next_claim_at: Timestamp",
                  paused_until AS "paused_until: Timestamp", consecutive_failures, probe_message_id, probe_generation,
                  quota_scope_id, timezone, send_window
             FROM connections
            WHERE workspace_id = $1 AND id = $2 AND status = 'active' AND NOT paused
              FOR UPDATE SKIP LOCKED"#,
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return nothing(tx).await;
    };
    let now: Timestamp = sqlx::query_scalar!(r#"SELECT now() AS "now!: Timestamp""#)
        .fetch_one(&mut *tx)
        .await?;
    let paced_sender = routing.send_interval_minutes.is_some();
    let phase = i32::from(routing.send_phase_seconds);
    let in_progress = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM delivery_queue WHERE workspace_id = $1 AND connection_id = $2 AND state <> 'queued') AS "busy!""#,
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_one(&mut *tx)
    .await?;
    let waiting = routing.next_claim_at.is_some_and(|until| until > now);
    if waiting || (paced_sender && in_progress) {
        return nothing(tx).await;
    }
    let scope = match routing.quota_scope_id {
        Some(scope) => sqlx::query!(
            r#"SELECT paused_until AS "paused_until: Timestamp", consecutive_failures, probe_message_id, probe_generation
                 FROM quota_scopes WHERE workspace_id = $1 AND id = $2"#,
            workspace.uuid(),
            scope,
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| {
            (
                scope,
                super::finish::breaker_of(
                    row.consecutive_failures,
                    row.paused_until,
                    None,
                    row.probe_message_id,
                    row.probe_generation,
                ),
            )
        }),
        None => None,
    };
    let connection_breaker = super::finish::breaker_of(
        routing.consecutive_failures,
        routing.paused_until,
        None,
        routing.probe_message_id,
        routing.probe_generation,
    );
    let connection_live = live(&mut tx, workspace, &connection_breaker).await?;
    let scope_live = match &scope {
        Some((_, breaker)) => live(&mut tx, workspace, breaker).await?,
        None => false,
    };
    let admission = policy::admission(
        (&connection_breaker, connection_live),
        scope.as_ref().map(|(_, breaker)| (breaker, scope_live)),
        now.0,
    );
    if admission == Admission::Nothing {
        return nothing(tx).await;
    }

    // 2. The budgets, as the worker inside the workspace.
    db::reset_role(&mut tx).await?;
    db::set_workspace(&mut tx, workspace).await?;
    let details = sqlx::query!(
        "SELECT provider, smtp FROM connections WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_one(&mut *tx)
    .await?;
    let Ok(provider) = details.provider.parse::<Provider>() else {
        return nothing(tx).await;
    };
    let smtp_host = details
        .smtp
        .as_ref()
        .and_then(|smtp| smtp.get("host"))
        .and_then(Value::as_str);
    let day = Date::utc_day(now);
    let yesterday = Date(day.0.yesterday().unwrap_or(day.0));
    let usage = sqlx::query!(
        r#"SELECT day AS "day: Date", used, reserved FROM connection_usage
            WHERE workspace_id = $1 AND connection_id = $2 AND day >= $3 AND day <= $4"#,
        workspace.uuid(),
        connection.uuid(),
        yesterday as _,
        day as _,
    )
    .fetch_all(&mut *tx)
    .await?;
    let bucket_of = |wanted: Date| {
        usage
            .iter()
            .find(|row| row.day == wanted)
            .map_or_else(Bucket::default, |row| Bucket {
                used: i64::from(row.used),
                reserved: i64::from(row.reserved),
            })
    };
    let mut window = None;
    let mut scope_budget = None;
    if let Some((scope_id, _)) = &scope {
        let limits = sqlx::query!(
            "SELECT messages_per_day, recipients_per_day, window_limit, window_unit, window_seconds
               FROM quota_scopes WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            scope_id,
        )
        .fetch_one(&mut *tx)
        .await?;
        if let (Some(limit), Some(unit), Some(seconds)) = (
            limits.window_limit,
            limits.window_unit,
            limits.window_seconds,
        ) {
            window = Some(ScopeWindow {
                scope: *scope_id,
                limit,
                unit,
                seconds,
            });
        }
        let rows = sqlx::query!(
            r#"SELECT day AS "day: Date", messages_used, messages_reserved, recipients_used, recipients_reserved
                 FROM quota_scope_usage WHERE workspace_id = $1 AND scope_id = $2 AND day >= $3 AND day <= $4"#,
            workspace.uuid(),
            scope_id,
            yesterday as _,
            day as _,
        )
        .fetch_all(&mut *tx)
        .await?;
        let of = |wanted: Date| rows.iter().find(|row| row.day == wanted);
        let messages = |wanted| {
            of(wanted).map_or_else(Bucket::default, |row| Bucket {
                used: i64::from(row.messages_used),
                reserved: i64::from(row.messages_reserved),
            })
        };
        let recipients = |wanted| {
            of(wanted).map_or_else(Bucket::default, |row| Bucket {
                used: i64::from(row.recipients_used),
                reserved: i64::from(row.recipients_reserved),
            })
        };
        scope_budget = Some(ScopeBudget {
            messages_per_day: limits.messages_per_day.map(i64::from),
            recipients_per_day: limits.recipients_per_day.map(i64::from),
            messages: [messages(day), messages(yesterday)],
            recipients: [recipients(day), recipients(yesterday)],
        });
    }
    let budgets = Budgets {
        daily_limit: i64::from(schedule::warmed_limit(
            routing.daily_limit,
            routing.warmup_stage,
        )),
        today: bucket_of(day),
        yesterday: bucket_of(yesterday),
        provider_cap: budget::provider_cap(provider, smtp_host),
        scope: scope_budget,
    };
    let room = budget::room(&budgets);
    if room.messages == 0 {
        let Some(until) = budget::spent_until(&budgets, now.0, phase) else {
            return nothing(tx).await;
        };
        let until = Timestamp(until);
        sqlx::query!(
            "UPDATE connections SET next_claim_at = $3 WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            connection.uuid(),
            until as _,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Claim::Spent { until });
    }

    // 3. The rows.
    let connection_window = window_of(routing.send_window.as_ref(), &routing.timezone);
    let probe = matches!(admission, Admission::Probe { .. });
    let wanted = if probe {
        1
    } else {
        budget::allowance(&room, u32::MAX, free_slots, u32::MAX)
            .min(u32::try_from(WAVE_MAX).unwrap_or(u32::MAX))
    };
    if wanted == 0 {
        return nothing(tx).await;
    }
    let rows: Vec<Due>;
    let mut chosen: Vec<&Due> = Vec::new();
    let mut deferred: Vec<(Uuid, Timestamp)> = Vec::new();
    let mut clock_to: Option<Timestamp> = None;
    let mut limited = false;
    let mut recipients_left = room.recipients;
    let mut fits = |due: &Due| -> bool {
        if recipients_left.is_some_and(|left| left < i64::from(due.recipient_count)) {
            return false;
        }
        let allowed = charge(&Charge {
            connection,
            provider,
            window: window.as_ref(),
            recipients: due.recipient_count,
        });
        if allowed && let Some(left) = recipients_left.as_mut() {
            *left = left.saturating_sub(i64::from(due.recipient_count));
        }
        allowed
    };
    if paced_sender {
        let api = due_rows(&mut tx, workspace, connection, Some(false), 1).await?;
        let clock_due = routing.next_send_at <= now;
        match budget::take(true, wanted, !api.is_empty(), clock_due) {
            Take::Api => {
                rows = api;
                match rows.first() {
                    Some(first) if fits(first) => chosen.push(first),
                    Some(_) | None => limited = true,
                }
            }
            Take::Cold => {
                rows = due_rows(&mut tx, workspace, connection, Some(true), COLD_LOOKAHEAD).await?;
                let campaigns = campaign_windows(&mut tx, workspace, &rows).await?;
                let mut opens: Option<Timestamp> = None;
                for due in &rows {
                    match sendable(due, &campaigns, connection_window.as_ref(), now) {
                        Sendable::Now => {
                            if fits(due) {
                                chosen.push(due);
                            } else {
                                limited = true;
                            }
                            break;
                        }
                        Sendable::At(at) => {
                            opens = Some(opens.map_or(at, |earlier| earlier.min(at)));
                        }
                        Sendable::Inactive => {}
                    }
                }
                if rows.is_empty() {
                    clock_to = Some(idle_until(connection_window.as_ref(), now));
                } else if chosen.is_empty() && !limited {
                    clock_to = Some(opens.unwrap_or_else(|| slot_later(now)));
                }
            }
            Take::Nothing | Take::Due(_) => {
                if api.is_empty() && clock_due {
                    clock_to = Some(idle_until(connection_window.as_ref(), now));
                }
            }
        }
    } else {
        rows = due_rows(&mut tx, workspace, connection, None, i64::from(wanted)).await?;
        let campaigns = campaign_windows(&mut tx, workspace, &rows).await?;
        for due in &rows {
            match sendable(due, &campaigns, connection_window.as_ref(), now) {
                Sendable::Now => {
                    if !fits(due) {
                        break;
                    }
                    chosen.push(due);
                }
                Sendable::At(at) => deferred.push((due.message_id, at)),
                Sendable::Inactive => deferred.push((due.message_id, slot_later(now))),
            }
        }
    }
    if limited {
        // A limiter of this replica refused: the turn moves on, and nothing is written.
        return nothing(tx).await;
    }

    // 4. The writes.
    if !deferred.is_empty() {
        let ids: Vec<Uuid> = deferred.iter().map(|(id, _)| *id).collect();
        let at: Vec<Timestamp> = deferred.iter().map(|(_, at)| *at).collect();
        sqlx::query!(
            "UPDATE delivery_queue q SET run_at = d.run_at
               FROM unnest($2::uuid[], $3::timestamptz[]) AS d(message_id, run_at)
              WHERE q.workspace_id = $1 AND q.message_id = d.message_id",
            workspace.uuid(),
            &ids,
            &at as _,
        )
        .execute(&mut *tx)
        .await?;
    }
    if chosen.is_empty() {
        if let Some(at) = clock_to {
            sqlx::query!(
                "UPDATE connections SET next_send_at = greatest(next_send_at, CASE WHEN provider = 'norbelys' THEN $3 ELSE next_phase_at($3, send_phase_seconds) END)
                  WHERE workspace_id = $1 AND id = $2 AND send_interval_minutes IS NOT NULL",
                workspace.uuid(),
                connection.uuid(),
                at as _,
            )
            .execute(&mut *tx)
            .await?;
        }
        if deferred.is_empty() && clock_to.is_none() {
            return nothing(tx).await;
        }
        tx.commit().await?;
        return Ok(Claim::Nothing);
    }
    let ids: Vec<Uuid> = chosen.iter().map(|due| due.message_id).collect();
    let scope_id = scope.as_ref().map(|(id, _)| *id);
    sqlx::query!(
        "INSERT INTO connection_usage (workspace_id, connection_id, day) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        workspace.uuid(),
        connection.uuid(),
        day as _,
    )
    .execute(&mut *tx)
    .await?;
    if let Some(scope_id) = scope_id {
        sqlx::query!(
            "INSERT INTO quota_scope_usage (workspace_id, scope_id, day) VALUES ($1, $2, $3), ($1, $2, $4) ON CONFLICT DO NOTHING",
            workspace.uuid(),
            scope_id,
            yesterday as _,
            day as _,
        )
        .execute(&mut *tx)
        .await?;
    }
    let claimed = sqlx::query!(
        "UPDATE delivery_queue q SET state = 'claimed', lease_owner = $3, lease_generation = q.lease_generation + 1,
                lease_expires_at = now() + make_interval(secs => $4), reserved_day = $5
          WHERE q.workspace_id = $1 AND q.message_id = ANY($2) AND q.state = 'queued'
        RETURNING q.message_id, q.lease_generation, q.paced,
                  extract(epoch FROM now() - q.run_at)::float8 AS \"waited!\"",
        workspace.uuid(),
        &ids,
        owner,
        LEASE.as_secs_f64(),
        day as _,
    )
    .fetch_all(&mut *tx)
    .await?;
    let numbered = sqlx::query!(
        "UPDATE messages SET state = 'claimed', attempt_number = attempt_number + 1
          WHERE workspace_id = $1 AND id = ANY($2)
        RETURNING id, attempt_number, recipient_count AS \"recipient_count!\", trace_parent",
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut *tx)
    .await?;
    // The requests that created the claimed messages, for the wave's span to link to.
    let trace_parents: Vec<String> = numbered
        .iter()
        .filter_map(|row| row.trace_parent.clone())
        .collect();
    let numbers: Vec<i32> = ids
        .iter()
        .map(|id| {
            numbered
                .iter()
                .find(|row| row.id == *id)
                .map_or(1, |row| row.attempt_number)
        })
        .collect();
    let recipients: Vec<i32> = ids
        .iter()
        .map(|id| {
            numbered
                .iter()
                .find(|row| row.id == *id)
                .map_or(1, |row| row.recipient_count)
        })
        .collect();
    sqlx::query!(
        "INSERT INTO attempts (workspace_id, message_id, attempt_number, connection_id, reserved_day, quota_scope_id,
                               recipient_count, lease_owner)
         SELECT $1, a.message_id, a.attempt_number, $2, $3, $4, a.recipients, $5
           FROM unnest($6::uuid[], $7::int[], $8::int[]) AS a(message_id, attempt_number, recipients)",
        workspace.uuid(),
        connection.uuid(),
        day as _,
        scope_id,
        owner,
        &ids,
        &numbers,
        &recipients,
    )
    .execute(&mut *tx)
    .await?;
    let count = i32::try_from(ids.len()).unwrap_or(i32::MAX);
    sqlx::query!(
        "UPDATE connection_usage SET reserved = reserved + $4 WHERE workspace_id = $1 AND connection_id = $2 AND day = $3",
        workspace.uuid(),
        connection.uuid(),
        day as _,
        count,
    )
    .execute(&mut *tx)
    .await?;
    if let Some(scope_id) = scope_id {
        let total: i32 = recipients.iter().copied().fold(0, i32::saturating_add);
        sqlx::query!(
            r#"SELECT day AS "day: Date" FROM quota_scope_usage WHERE workspace_id = $1 AND scope_id = $2 AND day >= $3 ORDER BY day FOR UPDATE"#,
            workspace.uuid(),
            scope_id,
            yesterday as _,
        )
        .fetch_all(&mut *tx)
        .await?;
        let reserved = sqlx::query_scalar!(
            "UPDATE quota_scope_usage t SET messages_reserved = t.messages_reserved + $5, recipients_reserved = t.recipients_reserved + $6
               FROM quota_scope_usage y, quota_scopes s
              WHERE (t.workspace_id, t.scope_id, t.day) = ($1, $2, $3)
                AND (y.workspace_id, y.scope_id, y.day) = ($1, $2, $4)
                AND (s.workspace_id, s.id) = ($1, $2)
                AND (s.messages_per_day IS NULL
                     OR t.messages_used + t.messages_reserved + y.messages_used + y.messages_reserved + $5 <= s.messages_per_day)
                AND (s.recipients_per_day IS NULL
                     OR t.recipients_used + t.recipients_reserved + y.recipients_used + y.recipients_reserved + $6 <= s.recipients_per_day)
            RETURNING t.day AS \"day: Date\"",
            workspace.uuid(),
            scope_id,
            day as _,
            yesterday as _,
            count,
            total,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if reserved.is_none() {
            // Another connection of the scope took its last units: the next turn reads again.
            tx.rollback().await?;
            return Ok(Claim::Nothing);
        }
    }
    // How long each claimed message waited after it was due, for the pacing metric.
    let waits: Vec<f64> = claimed.iter().map(|row| row.waited.max(0.0)).collect();
    let claimed: Vec<Claimed> = ids
        .iter()
        .zip(&numbers)
        .filter_map(|(id, attempt_number)| {
            let row = claimed.iter().find(|row| row.message_id == *id)?;
            Some(Claimed {
                message: Id::from_uuid(*id),
                generation: row.lease_generation,
                attempt_number: *attempt_number,
                paced: row.paced,
            })
        })
        .collect();
    if let (
        Admission::Probe {
            connection: on_connection,
            scope: on_scope,
        },
        Some(first),
    ) = (admission, claimed.first())
    {
        if on_connection {
            sqlx::query!(
                "UPDATE connections SET probe_message_id = $3, probe_generation = $4 WHERE workspace_id = $1 AND id = $2",
                workspace.uuid(),
                connection.uuid(),
                first.message.uuid(),
                first.generation,
            )
            .execute(&mut *tx)
            .await?;
        }
        if let (true, Some(scope_id)) = (on_scope, scope_id) {
            sqlx::query!(
                "UPDATE quota_scopes SET probe_message_id = $3, probe_generation = $4 WHERE workspace_id = $1 AND id = $2",
                workspace.uuid(),
                scope_id,
                first.message.uuid(),
                first.generation,
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    let label = [KeyValue::new("provider", provider.as_str())];
    CLAIMS.add(1, &label);
    for waited in waits {
        PACING_WAIT.record(waited, &label);
    }
    crate::telemetry::unit(crate::telemetry::Event::DeliveryClaim);
    tracing::info!(
        event = "delivery.claim",
        workspace_id = %workspace,
        connection_id = %connection,
        provider = provider.as_str(),
        allowance = wanted,
        claimed = claimed.len(),
        budget_remaining = room.messages.saturating_sub(i64::from(count)),
        scope_remaining = room.recipients,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "delivery.claim"
    );
    Ok(Claim::Wave(Wave {
        workspace,
        connection,
        provider,
        paced_sender,
        probe,
        messages: claimed,
        trace_parents,
    }))
}

/// Ends a claim that writes nothing.
async fn nothing(tx: Tx) -> Result<Claim, sqlx::Error> {
    tx.rollback().await?;
    Ok(Claim::Nothing)
}

/// True when `breaker`'s probe is live: the queue row and generation it names still leased.
async fn live(
    tx: &mut Tx,
    workspace: WorkspaceId,
    breaker: &policy::Breaker,
) -> Result<bool, sqlx::Error> {
    let Some(probe) = breaker.probe else {
        return Ok(false);
    };
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM delivery_queue
                           WHERE workspace_id = $1 AND message_id = $2 AND lease_generation = $3
                             AND state <> 'queued' AND lease_expires_at > now()) AS "live!""#,
        workspace.uuid(),
        probe.message,
        probe.generation,
    )
    .fetch_one(&mut **tx)
    .await
}

/// The connection's due rows, in due order, locked (rows another path holds are skipped): only
/// cold rows (`Some(true)`), only rows created through the API (`Some(false)`), or both.
async fn due_rows(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    paced: Option<bool>,
    limit: i64,
) -> Result<Vec<Due>, sqlx::Error> {
    sqlx::query_as!(
        Due,
        "SELECT q.message_id, m.campaign_id, m.recipient_count AS \"recipient_count!\"
           FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
          WHERE q.workspace_id = $1 AND q.connection_id = $2 AND q.state = 'queued' AND q.run_at <= now()
            AND ($3::boolean IS NULL OR q.paced = $3)
          ORDER BY q.run_at, q.message_id
          LIMIT $4
            FOR UPDATE OF q SKIP LOCKED",
        workspace.uuid(),
        connection.uuid(),
        paced,
        limit,
    )
    .fetch_all(&mut **tx)
    .await
}

/// A campaign's standing as the claim weighs its rows.
struct CampaignWindow {
    active: bool,
    window: Option<Result<Window, ()>>,
}

/// The campaigns of `rows` with their windows.
async fn campaign_windows(
    tx: &mut Tx,
    workspace: WorkspaceId,
    rows: &[Due],
) -> Result<HashMap<Uuid, CampaignWindow>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().filter_map(|due| due.campaign_id).collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let campaigns = sqlx::query!(
        "SELECT id, status, send_window, timezone FROM campaigns WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(campaigns
        .into_iter()
        .map(|row| {
            let window = row
                .send_window
                .as_ref()
                .map(|value| window_of(Some(value), &row.timezone).ok_or(()));
            (
                row.id,
                CampaignWindow {
                    active: row.status == "active",
                    window,
                },
            )
        })
        .collect())
}

/// A stored send window in its zone; `None` for no window, and for one that cannot be read (which
/// the caller treats as never open).
fn window_of(value: Option<&Value>, zone: &str) -> Option<Window> {
    let window: SendWindow = serde_json::from_value(value?.clone()).ok()?;
    Window::new(&window, zone).ok()
}

/// When a due row may go.
enum Sendable {
    /// Now.
    Now,
    /// At the first instant its windows are open together.
    At(Timestamp),
    /// Its campaign is not active.
    Inactive,
}

/// When `due` may go: mail created through the API at once, campaign mail of an active campaign
/// while its campaign's and the connection's windows are open together (a window that cannot be
/// read never opens: such mail is looked at again a day later).
fn sendable(
    due: &Due,
    campaigns: &HashMap<Uuid, CampaignWindow>,
    connection_window: Option<&Window>,
    now: Timestamp,
) -> Sendable {
    let Some(campaign) = due.campaign_id else {
        return Sendable::Now;
    };
    let Some(standing) = campaigns.get(&campaign).filter(|standing| standing.active) else {
        return Sendable::Inactive;
    };
    let campaign_window = match &standing.window {
        None => None,
        Some(Ok(window)) => Some(window),
        Some(Err(())) => return Sendable::At(day_later(now)),
    };
    let windows: Vec<&Window> = campaign_window
        .into_iter()
        .chain(connection_window)
        .collect();
    match schedule::next_open_together(&windows, now.0) {
        Some(at) if at <= now.0 => Sendable::Now,
        Some(at) => Sendable::At(Timestamp(at)),
        None => Sendable::At(day_later(now)),
    }
}

/// Where an idle paced sender's clock goes: its next phase instant, or the next opening of its
/// own window when that window is closed.
fn idle_until(connection_window: Option<&Window>, now: Timestamp) -> Timestamp {
    match connection_window {
        Some(window) => window
            .next_open(now.0)
            .map_or_else(|| day_later(now), Timestamp),
        None => now,
    }
}

fn slot_later(now: Timestamp) -> Timestamp {
    now.plus(Duration::from_secs(300))
}

fn day_later(now: Timestamp) -> Timestamp {
    now.plus(Duration::from_secs(86_400))
}

#[cfg(test)]
mod tests;
