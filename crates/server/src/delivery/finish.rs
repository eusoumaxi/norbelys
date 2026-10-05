//! The Finish: what one connection's claimed messages came to, recorded in one micro-batch.
//!
//! The sender reports each message's ending ([`Report`]): the transport's answer, a message that
//! will never be submitted (preflight found no way to reach it, its content cannot be prepared),
//! or a message returned unstarted (preflight could not ask DNS, the process is stopping). It
//! flushes a connection's reports every 2 seconds or 50 outcomes, so an accepted message is never
//! held back by the slowest of its wave, and [`finish`] records them in one transaction:
//!
//! 1. the quota scope's row, first, only when the batch changes the scope's health, decided from a
//!    read taken before any lock (a scoped failure, the scope's probe among the reports, or an
//!    acceptance while the scope's count is above zero and its breaker closed); a failure that
//!    read did not see stays counted until the next success;
//! 2. the connection's row;
//! 3. the queue rows and their messages, in message order, and only those this owner still
//!    leases in the generation it reports: a report whose lease was lost (recovered, claimed
//!    again) records nothing and is counted as lost;
//! 4. the pure decisions (`domain::policy::delivery`): the message's next state and its
//!    reservation's fate, its recipients' evidence, the connection's and the scope's breakers;
//! 5. the attempts closed where they are still `reserved`, and the ledgers settled from exactly
//!    the rows that closed, each against its own reservation day and scope: a batch may span
//!    midnight, and a report delivered twice settles nothing the second time;
//! 6. the evidence ([`super::evidence::record`]), the counters, the health of the connection and
//!    the scope, the messages' states, the queue rows deleted (a final state) or put back with
//!    their next due time (a retry), the customer's `message.*` events, the Message-ID directory
//!    of a provider that replaced ours; a release clears the connection's budget wait, and an
//!    `uncertain` message on a mailbox asks its `connection.check` to read the Sent folder.
//!
//! Lock order, the same for every delivery path (claim, Start, finish, recovery, cancel,
//! reconciliation): policy rows (workspace, campaign, enrollment) → the quota scope's row → the
//! connection's row → the sender identity's row → queue rows, then message rows, in message
//! order → attempts → the connection's ledger rows, then the scope's, each in day order. A path
//! that does not need an earlier lock may omit it, but never takes it later.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use serde::Serialize;
use uuid::Uuid;

use super::evidence::{self, Evidence};
use crate::db::{Database, Tx};
use crate::domain::ids::{Attempt, Connection, Id, Message, WorkspaceId};
use crate::domain::messages::State as MessageState;
use crate::domain::policy::delivery::{
    self as policy, Answer, Breaker, BreakerChange, Category, Confidence, ConnectionEffect,
    Enhanced, EventKind, Failure, Health, Metric, Next, Outcome, Phase, Probe, Quota, RecipientRef,
    Refusal, RefusalScope, Source,
};
use crate::domain::senders::{HealthEvent, Provider, Status};
use crate::domain::time::{Date, Timestamp};
use crate::jobs::{self, Queue};
use crate::senders::check::ConnectionCheck;
use crate::senders::health;
use crate::webhooks::EventType;

static PAUSES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_delivery_pauses_total")
        .with_description(
            "Breakers a finish opened, pausing a connection or a quota scope, by provider and \
             scope (connection, quota_scope).",
        )
        .build()
});

/// A recipient the server refused at `RCPT TO` while the submission went on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedRecipient {
    /// The envelope address.
    pub recipient: String,
    /// The reply code.
    pub code: u16,
    /// The reply's enhanced status.
    pub status: Option<Enhanced>,
    /// The reply, bounded.
    pub diagnostic: String,
}

/// What the transport answered, with what the finish records of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Answered {
    /// The answer, as the policy reads it.
    pub answer: Answer,
    /// Who answered: the SMTP session, or a provider's HTTP API.
    pub source: Source,
    /// The Start's submission marker; `None` when the session could not be acquired, before any
    /// Start.
    pub started: Option<Timestamp>,
    /// The provider's text or our reason, bounded and on one line; never a credential.
    pub diagnostic: String,
    /// The provider's id of the accepted message, when it returns one.
    pub provider_message_id: Option<String>,
    /// The envelope's recipients, in `RCPT TO` order.
    pub recipients: Vec<String>,
    /// Recipients refused while others were accepted, or every refusal read before the
    /// submission stopped.
    pub refused: Vec<RefusedRecipient>,
}

/// How one claimed message ended, as the sender reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum Reported {
    /// The transport answered, or a session could not be acquired (a refusal of the connection
    /// before the Start, which the breaker counts).
    Answered(Box<Answered>),
    /// Never to be submitted: the message fails with `category`, and `evidence` (preflight's
    /// findings) is recorded.
    Skipped {
        category: Category,
        detail: String,
        evidence: Vec<Evidence>,
    },
    /// Back to the queue unstarted, due at `run_at` (unchanged when `None`): a deferral before
    /// the Start, or a stopping sender.
    Released { run_at: Option<Timestamp> },
}

/// One message's report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The message.
    pub message: Id<Message>,
    /// The lease generation it was claimed in: the fence.
    pub generation: i64,
    /// How it ended.
    pub reported: Reported,
}

/// What one finish recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Finished {
    /// Attempts closed and settled by this batch.
    pub settled: usize,
    /// Reports whose lease this owner no longer held: nothing recorded for them.
    pub lost: usize,
    /// Reservations released.
    pub released: usize,
    /// Reservations consumed.
    pub consumed: usize,
    /// A limit of Norbelys's own project or app answered: this replica stops submitting through
    /// it until then.
    pub platform_until: Option<Timestamp>,
}

/// Why a finish could not be recorded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The connection as the finish reads it under its lock.
struct ConnectionRow {
    provider: String,
    status: String,
    paused: bool,
    breaker: Breaker,
}

/// A queue row this owner still leases, and its message.
struct Owned {
    message_id: Uuid,
    lease_generation: i64,
    deadline_at: Option<Timestamp>,
    thread_id: Option<Uuid>,
    attempt_number: i32,
    prior_transients: i64,
    recipients: Vec<String>,
}

/// One owned report and what the policy decided for it.
struct Decided<'a> {
    owned: &'a Owned,
    report: &'a Report,
    next: Next,
    category: Option<Category>,
    phase: Option<Phase>,
    code: Option<i16>,
    status: Option<String>,
    diagnostic: Option<String>,
    detail: Option<String>,
}

/// Records one micro-batch of `reports` of `connection` in `workspace`, for the lease `owner`
/// (see the module). Nothing is written for a report whose lease this owner lost.
///
/// # Errors
///
/// The database refused; the whole batch is rolled back and its leases expire into recovery.
pub async fn finish(
    db: &Database,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    owner: &str,
    reports: &[Report],
) -> Result<Finished, Error> {
    let started = Instant::now();
    let mut tx = db.begin_in(workspace).await?;
    let now = crate::process::now();

    // 0. The scope, read before any lock: lock it only when this batch changes its health.
    let scope = sqlx::query!(
        r#"SELECT s.id, s.consecutive_failures, s.breaker_opened_at AS "opened_at: Timestamp",
                  s.probe_message_id, s.probe_generation
             FROM connections c JOIN quota_scopes s ON s.workspace_id = c.workspace_id AND s.id = c.quota_scope_id
            WHERE c.workspace_id = $1 AND c.id = $2"#,
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let lock_scope = scope.as_ref().is_some_and(|scope| {
        reports.iter().any(|report| match &report.reported {
            Reported::Answered(answered) => {
                let scoped = matches!(
                    answered.answer,
                    Answer::Refused(Refusal {
                        scope: RefusalScope::QuotaScope,
                        ..
                    })
                );
                let probe = scope.probe_message_id == Some(report.message.uuid())
                    && scope.probe_generation == Some(report.generation);
                let reset = answered.answer == Answer::Accepted
                    && scope.consecutive_failures > 0
                    && scope.opened_at.is_none();
                scoped || probe || reset
            }
            Reported::Skipped { .. } | Reported::Released { .. } => false,
        })
    });
    let mut scope_breaker = None;
    if let (true, Some(scope)) = (lock_scope, &scope) {
        let row = sqlx::query!(
            r#"SELECT consecutive_failures, paused_until AS "paused_until: Timestamp",
                      breaker_opened_at AS "opened_at: Timestamp", probe_message_id, probe_generation
                 FROM quota_scopes WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
            workspace.uuid(),
            scope.id,
        )
        .fetch_one(&mut *tx)
        .await?;
        scope_breaker = Some((
            scope.id,
            breaker_of(
                row.consecutive_failures,
                row.paused_until,
                row.opened_at,
                row.probe_message_id,
                row.probe_generation,
            ),
        ));
    }

    // 1. The connection.
    let Some(conn) = lock_connection(&mut tx, workspace, connection).await? else {
        tx.rollback().await?;
        return Ok(Finished {
            lost: reports.len(),
            ..Finished::default()
        });
    };

    // 2. The rows this owner still leases, queue rows then messages, in message order.
    let ids: Vec<Uuid> = reports.iter().map(|report| report.message.uuid()).collect();
    let generations: Vec<i64> = reports.iter().map(|report| report.generation).collect();
    let owned: Vec<Owned> = sqlx::query_as!(
        Owned,
        r#"SELECT q.message_id, q.lease_generation, q.deadline_at AS "deadline_at: Timestamp",
                  m.thread_id, m.attempt_number,
                  (SELECT count(*) FROM attempts a WHERE a.workspace_id = q.workspace_id AND a.message_id = q.message_id
                      AND a.outcome = 'transient') AS "prior_transients!",
                  m.to_addresses || m.cc || m.bcc AS "recipients!"
             FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
            WHERE q.workspace_id = $1 AND q.connection_id = $2 AND q.lease_owner = $3
              AND (q.message_id, q.lease_generation) IN (SELECT * FROM unnest($4::uuid[], $5::bigint[]))
            ORDER BY q.message_id
              FOR UPDATE OF q, m"#,
        workspace.uuid(),
        connection.uuid(),
        owner,
        &ids,
        &generations,
    )
    .fetch_all(&mut *tx)
    .await?;
    let lost = reports.len().saturating_sub(owned.len());

    // The policy streak reads the connection's latest outcomes before this batch closes its own.
    let any_policy = reports.iter().any(|report| match &report.reported {
        Reported::Answered(answered) => {
            policy::connection_effect(&answered.answer) == ConnectionEffect::Policy
        }
        Reported::Skipped { .. } | Reported::Released { .. } => false,
    });
    let mut recent: Vec<Category> = Vec::new();
    if any_policy {
        let rows = sqlx::query_scalar!(
            "SELECT category FROM attempts
              WHERE workspace_id = $1 AND connection_id = $2 AND finished_at IS NOT NULL
                AND outcome IN ('accepted', 'transient', 'permanent', 'uncertain')
                AND reserved_day >= $3
              ORDER BY finished_at DESC LIMIT 2",
            workspace.uuid(),
            connection.uuid(),
            Date::utc_day(now.minus(std::time::Duration::from_secs(86_400))) as _,
        )
        .fetch_all(&mut *tx)
        .await?;
        recent = rows
            .into_iter()
            .rev()
            .map(|category| {
                category
                    .and_then(|text| text.parse::<Category>().ok())
                    .unwrap_or(Category::Rejected)
            })
            .collect();
    }

    // 3. Pure decisions, in the order the reports came.
    let decided: Vec<Decided<'_>> = reports
        .iter()
        .filter_map(|report| {
            let owned = owned.iter().find(|row| {
                row.message_id == report.message.uuid() && row.lease_generation == report.generation
            })?;
            Some(decide(owned, report, now))
        })
        .collect();

    // 4. Close the attempts still reserved, and settle exactly those.
    let closed = close_attempts(&mut tx, workspace, connection, &decided).await?;
    let reservations: Vec<Reservation> = closed.iter().map(Reservation::from).collect();
    settle(&mut tx, workspace, connection, &reservations).await?;
    let attempt_of = |message: Uuid| {
        closed
            .iter()
            .find(|row| row.message_id == message)
            .map(|row| Id::<Attempt>::from_uuid(row.id))
    };

    // 5. Evidence, counters, health, states, queue rows, directory, the customer.
    let mut observations: Vec<Evidence> = Vec::new();
    for decision in &decided {
        let attempt = attempt_of(decision.owned.message_id);
        observations.extend(observations_of(decision, attempt, now));
    }
    evidence::record(&mut tx, workspace, &observations).await?;

    let provider = conn.provider.parse::<Provider>().ok();
    let mut breaker = conn.breaker;
    let mut categories = recent;
    let mut credential_lost = None;
    let mut blocked = None;
    let mut platform_until: Option<Timestamp> = None;
    let mut openings = 0_u64;
    let mut scope_openings = 0_u64;
    for decision in &decided {
        let Reported::Answered(answered) = &decision.report.reported else {
            continue;
        };
        let draw = jobs::draw();
        let probe = Some(Probe {
            message: decision.owned.message_id,
            generation: decision.owned.lease_generation,
        });
        let health = match policy::connection_effect(&answered.answer) {
            ConnectionEffect::None => None,
            ConnectionEffect::Success => Some(Health::Success {
                probe,
                started: answered.started.map(|at| at.0),
            }),
            ConnectionEffect::Failure { throttled, wait } => {
                Some(Health::Failure { throttled, wait })
            }
            ConnectionEffect::Policy => Some(Health::Failure {
                throttled: false,
                wait: None,
            }),
            ConnectionEffect::CredentialLost => {
                credential_lost = Some(decision.diagnostic.clone().unwrap_or_default());
                None
            }
            ConnectionEffect::AccountBlocked => {
                blocked = Some(decision.diagnostic.clone().unwrap_or_default());
                None
            }
        };
        if let Some(health) = health {
            let change = policy::breaker_after(&breaker, health, now.0, draw);
            openings += u64::from(matches!(change, BreakerChange::Open { .. }));
            breaker = applied(breaker, change, now.0);
        }
        if let Some(category) = decision.category {
            categories.push(category);
        }
        if let Some((_, scope)) = scope_breaker.as_mut()
            && let Some(health) = policy::scope_effect(&answered.answer)
        {
            let health = match health {
                Health::Success { .. } => Health::Success {
                    probe,
                    started: answered.started.map(|at| at.0),
                },
                failure @ Health::Failure { .. } => failure,
            };
            let change = policy::breaker_after(scope, health, now.0, draw);
            let opened = u64::from(matches!(change, BreakerChange::Open { .. }));
            openings += opened;
            scope_openings += opened;
            *scope = applied(*scope, change, now.0);
        }
        if let Some(until) = policy::platform_backoff(&answered.answer, 0, now.0, draw) {
            let until = Timestamp(until);
            platform_until = Some(platform_until.map_or(until, |earlier| earlier.max(until)));
        }
    }
    if any_policy && policy::policy_streak(&categories) && blocked.is_none() {
        blocked = Some(
            "The provider refused the last three messages by policy, naming the account or its address; check the account's standing with the provider, then verify the connection."
                .to_owned(),
        );
    }
    let released = decided
        .iter()
        .any(|decision| decision.next.quota == Quota::Released);
    write_connection(
        &mut tx,
        workspace,
        connection,
        &conn.breaker,
        &breaker,
        released,
    )
    .await?;
    if let Some((scope_id, scope)) = &scope_breaker {
        write_scope(&mut tx, workspace, *scope_id, scope, &decided).await?;
    }
    let status = conn.status.parse::<Status>().unwrap_or(Status::Active);
    if let Some(detail) = credential_lost {
        let detail = format!(
            "The provider refused the connection's credential ({detail}); reconnect it or save a new one."
        );
        health::apply(
            &mut tx,
            workspace,
            connection,
            status,
            conn.paused,
            HealthEvent::CredentialLost,
            Some(&detail),
        )
        .await?;
    } else if let Some(detail) = blocked {
        health::apply(
            &mut tx,
            workspace,
            connection,
            status,
            conn.paused,
            HealthEvent::AccountBlocked,
            Some(&detail),
        )
        .await?;
    }

    write_messages(&mut tx, workspace, &decided).await?;
    write_queue(&mut tx, workspace, &decided).await?;
    expire_holds(&mut tx, workspace, &decided).await?;
    if provider == Some(Provider::Ses) {
        write_directory(&mut tx, workspace, &decided).await?;
    }
    let sent: Vec<Id<Message>> = decided
        .iter()
        .filter(|d| d.next.state == MessageState::Sent)
        .map(|d| Id::from_uuid(d.owned.message_id))
        .collect();
    evidence::increment(&mut tx, workspace, &sent, Metric::Sent).await?;
    for decision in &decided {
        let kind = match decision.next.state {
            MessageState::Sent => EventType::MessageSent,
            MessageState::Failed => EventType::MessageFailed,
            MessageState::Uncertain => EventType::MessageUncertain,
            MessageState::Queued
            | MessageState::Claimed
            | MessageState::InFlight
            | MessageState::Cancelled
            | MessageState::Suppressed => continue,
        };
        evidence::tell(
            &mut tx,
            workspace,
            kind,
            Id::from_uuid(decision.owned.message_id),
            attempt_of(decision.owned.message_id),
            decision.category,
            now,
        )
        .await?;
    }

    // 6. An uncertain message on a mailbox asks its one maintenance job to read the Sent folder.
    let mailbox = provider.is_some_and(Provider::is_mailbox);
    let check = mailbox
        && decided
            .iter()
            .any(|decision| decision.next.state == MessageState::Uncertain);
    if check {
        jobs::enqueue(&mut tx, workspace, &ConnectionCheck { connection }, None).await?;
    }
    tx.commit().await?;
    for decision in &decided {
        evidence::report_failure(
            workspace,
            Id::from_uuid(decision.owned.message_id),
            decision.next.state,
            decision.category,
        );
    }
    if check {
        jobs::wake(db, Queue::Maintenance).await;
    }

    let consumed = closed
        .iter()
        .filter(|row| row.quota_state == Quota::Consumed.as_str())
        .count();
    let finished = Finished {
        settled: closed.len(),
        lost,
        released: closed.len().saturating_sub(consumed),
        consumed,
        platform_until,
    };
    if let Some(provider) = provider {
        for (scope, pauses) in [
            ("connection", openings.saturating_sub(scope_openings)),
            ("quota_scope", scope_openings),
        ] {
            if pauses > 0 {
                PAUSES.add(
                    pauses,
                    &[
                        KeyValue::new("provider", provider.as_str()),
                        KeyValue::new("scope", scope),
                    ],
                );
            }
        }
    }
    crate::telemetry::unit(crate::telemetry::Event::DeliverySettle);
    tracing::info!(
        event = "delivery.settle",
        workspace_id = %workspace,
        connection_id = %connection,
        settled = finished.settled,
        lost = finished.lost,
        released = finished.released,
        consumed = finished.consumed,
        pauses = openings,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "delivery.settle"
    );
    Ok(finished)
}

/// A breaker as stored.
pub(crate) fn breaker_of(
    failures: i32,
    paused_until: Option<Timestamp>,
    opened_at: Option<Timestamp>,
    probe_message: Option<Uuid>,
    probe_generation: Option<i64>,
) -> Breaker {
    Breaker {
        failures,
        paused_until: paused_until.map(|at| at.0),
        opened_at: opened_at.map(|at| at.0),
        probe: match (probe_message, probe_generation) {
            (Some(message), Some(generation)) => Some(Probe {
                message,
                generation,
            }),
            _ => None,
        },
    }
}

/// `breaker` after `change` at `now`.
fn applied(breaker: Breaker, change: BreakerChange, now: jiff::Timestamp) -> Breaker {
    match change {
        BreakerChange::Unchanged => breaker,
        BreakerChange::Reset => Breaker {
            failures: 0,
            ..breaker
        },
        BreakerChange::Count { failures } => Breaker {
            failures,
            ..breaker
        },
        BreakerChange::Open { failures, until } => Breaker {
            failures,
            paused_until: Some(until),
            opened_at: Some(now),
            probe: None,
        },
        BreakerChange::Close => Breaker {
            failures: 0,
            paused_until: None,
            opened_at: None,
            probe: None,
        },
    }
}

/// Locks the connection's row and reads what the finish needs; `None` when it is gone.
async fn lock_connection(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<Option<ConnectionRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT provider, status, paused, consecutive_failures, paused_until AS "paused_until: Timestamp",
                  breaker_opened_at AS "opened_at: Timestamp", probe_message_id, probe_generation
             FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| ConnectionRow {
        provider: row.provider,
        status: row.status,
        paused: row.paused,
        breaker: breaker_of(
            row.consecutive_failures,
            row.paused_until,
            row.opened_at,
            row.probe_message_id,
            row.probe_generation,
        ),
    }))
}

/// The policy's decision for one owned report.
fn decide<'a>(owned: &'a Owned, report: &'a Report, now: Timestamp) -> Decided<'a> {
    let base = |next| Decided {
        owned,
        report,
        next,
        category: None,
        phase: None,
        code: None,
        status: None,
        diagnostic: None,
        detail: None,
    };
    match &report.reported {
        Reported::Answered(answered) => {
            let next = policy::after_submission(
                &answered.answer,
                u32::try_from(owned.prior_transients).unwrap_or(u32::MAX),
                owned.deadline_at.map(|at| at.0),
                now.0,
                jobs::draw(),
            );
            let category = if next.expired {
                Category::Expired
            } else {
                policy::category(&answered.answer)
            };
            let (phase, code, status) = match &answered.answer {
                Answer::Accepted => (
                    Some(match answered.source {
                        Source::ProviderApi => Phase::Api,
                        _ => Phase::Data,
                    }),
                    None,
                    None,
                ),
                Answer::Refused(refusal) => (
                    Some(refusal.phase),
                    refusal.code.and_then(|code| i16::try_from(code).ok()),
                    refusal.status.map(status_text),
                ),
            };
            let detail = match next.state {
                MessageState::Sent => None,
                MessageState::Failed if next.expired => {
                    Some("It expired before the provider would take it.".to_owned())
                }
                MessageState::Uncertain => Some(
                    "The provider's final answer was lost: it may have been sent, and it is never resent automatically."
                        .to_owned(),
                ),
                _ => Some(answered.diagnostic.clone()),
            };
            Decided {
                category: Some(category),
                phase,
                code,
                status,
                diagnostic: Some(answered.diagnostic.clone()),
                detail,
                ..base(next)
            }
        }
        Reported::Skipped {
            category, detail, ..
        } => Decided {
            category: Some(*category),
            detail: Some(detail.clone()),
            ..base(Next {
                state: MessageState::Failed,
                outcome: Outcome::Skipped,
                quota: Quota::Released,
                retry_at: None,
                expired: false,
            })
        },
        Reported::Released { run_at } => base(Next {
            state: MessageState::Queued,
            outcome: Outcome::Released,
            quota: Quota::Released,
            retry_at: run_at.map(|at| at.0),
            expired: false,
        }),
    }
}

/// `status` as stored (`5.1.1`).
pub(crate) fn status_text(status: Enhanced) -> String {
    format!("{}.{}.{}", status.class, status.subject, status.detail)
}

/// The evidence one decision records: an acceptance names each recipient that was not refused;
/// each recipient refused at `RCPT TO` is its own refusal; a refusal of the whole message names
/// its single recipient, or none; a skipped message brings its own (preflight's findings).
fn observations_of(
    decision: &Decided<'_>,
    attempt: Option<Id<Attempt>>,
    now: Timestamp,
) -> Vec<Evidence> {
    let message = Some(Id::<Message>::from_uuid(decision.owned.message_id));
    let attempt_number = Some(decision.owned.attempt_number);
    let source_event_id = attempt.map_or_else(
        || {
            format!(
                "message:{}:{}",
                decision.owned.message_id, decision.owned.attempt_number
            )
        },
        |attempt| format!("attempt:{}", attempt.uuid()),
    );
    let answered = match &decision.report.reported {
        Reported::Answered(answered) => answered,
        Reported::Skipped { evidence, .. } => return evidence.clone(),
        Reported::Released { .. } => return Vec::new(),
    };
    let base = |recipient: Option<String>, reference, kind, category, phase, status, diagnostic| {
        Evidence {
            message,
            thread: decision.owned.thread_id,
            attempt_number,
            recipient,
            recipient_ref: reference,
            source: answered.source,
            source_event_id: source_event_id.clone(),
            received_via: None,
            kind,
            action: None,
            phase,
            enhanced_status: status,
            category,
            diagnostic,
            confidence: Confidence::Authenticated,
            receipt: None,
            observed_at: now,
        }
    };
    let mut observed = Vec::new();
    let refused: HashSet<String> = answered
        .refused
        .iter()
        .map(|refusal| refusal.recipient.to_ascii_lowercase())
        .collect();
    for refusal in &answered.refused {
        let failure = if refusal.code >= 500 {
            Failure::Permanent
        } else {
            Failure::Transient
        };
        let category = policy::category(&Answer::Refused(Refusal {
            failure,
            phase: Phase::RcptTo,
            scope: RefusalScope::Recipient,
            cause: policy::Cause::Refused,
            code: Some(refusal.code),
            status: refusal.status,
            retry_after: None,
        }));
        observed.push(base(
            Some(refusal.recipient.clone()),
            RecipientRef::Named,
            EventKind::Rejected,
            category,
            Some(Phase::RcptTo),
            refusal.status.map(status_text),
            Some(refusal.diagnostic.clone()),
        ));
    }
    match decision.next.state {
        MessageState::Sent => {
            let recipients = if answered.recipients.is_empty() {
                &decision.owned.recipients
            } else {
                &answered.recipients
            };
            for recipient in recipients {
                if refused.contains(&recipient.to_ascii_lowercase()) {
                    continue;
                }
                observed.push(base(
                    Some(recipient.clone()),
                    RecipientRef::Named,
                    EventKind::Accepted,
                    Category::Accepted,
                    decision.phase,
                    None,
                    None,
                ));
            }
        }
        MessageState::Failed if answered.refused.is_empty() && !decision.next.expired => {
            let single = decision.owned.recipients.len() == 1;
            let recipient = single
                .then(|| decision.owned.recipients.first().cloned())
                .flatten();
            observed.push(base(
                recipient,
                if single {
                    RecipientRef::SingleEnvelope
                } else {
                    RecipientRef::Unknown
                },
                EventKind::Rejected,
                decision.category.unwrap_or(Category::Rejected),
                decision.phase,
                decision.status.clone(),
                decision.diagnostic.clone(),
            ));
        }
        MessageState::Failed
        | MessageState::Queued
        | MessageState::Claimed
        | MessageState::InFlight
        | MessageState::Cancelled
        | MessageState::Uncertain
        | MessageState::Suppressed => {}
    }
    observed
}

/// An attempt that closed in this batch.
struct Closed {
    id: Uuid,
    message_id: Uuid,
    reserved_day: Date,
    quota_scope_id: Option<Uuid>,
    recipient_count: i32,
    quota_state: String,
}

/// Closes the attempts of `decided` that are still `reserved`; only the rows returned settle.
async fn close_attempts(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    decided: &[Decided<'_>],
) -> Result<Vec<Closed>, sqlx::Error> {
    if decided.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = decided.iter().map(|d| d.owned.message_id).collect();
    let numbers: Vec<i32> = decided.iter().map(|d| d.owned.attempt_number).collect();
    let outcomes: Vec<&str> = decided.iter().map(|d| d.next.outcome.as_str()).collect();
    let quotas: Vec<&str> = decided.iter().map(|d| d.next.quota.as_str()).collect();
    let phases: Vec<Option<&str>> = decided.iter().map(|d| d.phase.map(Phase::as_str)).collect();
    let codes: Vec<Option<i16>> = decided.iter().map(|d| d.code).collect();
    let statuses: Vec<Option<String>> = decided.iter().map(|d| d.status.clone()).collect();
    let categories: Vec<Option<&str>> = decided
        .iter()
        .map(|d| d.category.map(Category::as_str))
        .collect();
    let diagnostics: Vec<Option<String>> = decided
        .iter()
        .map(|d| {
            d.diagnostic
                .as_deref()
                .map(|text| text.chars().take(1_000).collect())
        })
        .collect();
    let provider_ids: Vec<Option<String>> = decided
        .iter()
        .map(|d| match &d.report.reported {
            Reported::Answered(answered) => answered.provider_message_id.clone(),
            Reported::Skipped { .. } | Reported::Released { .. } => None,
        })
        .collect();
    let started: Vec<Option<Timestamp>> = decided
        .iter()
        .map(|d| match &d.report.reported {
            Reported::Answered(answered) => answered.started,
            Reported::Skipped { .. } | Reported::Released { .. } => None,
        })
        .collect();
    sqlx::query_as!(
        Closed,
        r#"UPDATE attempts a SET outcome = d.outcome, finished_at = now(), quota_state = d.quota, phase = d.phase,
                  smtp_code = d.code, enhanced_status = d.status, category = d.category, diagnostic = d.diagnostic,
                  provider_message_id = d.provider_id, smtp_started_at = coalesce(a.smtp_started_at, d.started)
             FROM unnest($3::uuid[], $4::int[], $5::text[], $6::text[], $7::text[], $8::int2[], $9::text[],
                         $10::text[], $11::text[], $12::text[], $13::timestamptz[])
                  AS d(message_id, attempt_number, outcome, quota, phase, code, status, category, diagnostic, provider_id, started)
            WHERE a.workspace_id = $1 AND a.connection_id = $2 AND a.message_id = d.message_id
              AND a.attempt_number = d.attempt_number AND a.quota_state = 'reserved'
        RETURNING a.id, a.message_id, a.reserved_day AS "reserved_day: Date", a.quota_scope_id, a.recipient_count,
                  a.quota_state"#,
        workspace.uuid(),
        connection.uuid(),
        &ids,
        &numbers,
        &outcomes as _,
        &quotas as _,
        &phases as _,
        &codes as _,
        &statuses as _,
        &categories as _,
        &diagnostics as _,
        &provider_ids as _,
        &started as _,
    )
    .fetch_all(&mut **tx)
    .await
}

/// One closed attempt's reservation, as settling needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reservation {
    /// The day it reserved on.
    pub day: Date,
    /// The scope it reserved on, frozen at the claim.
    pub scope: Option<Uuid>,
    /// Its envelope's recipients.
    pub recipients: i32,
    /// True when it is consumed, false when released.
    pub consumed: bool,
}

impl From<&Closed> for Reservation {
    fn from(closed: &Closed) -> Self {
        Self {
            day: closed.reserved_day,
            scope: closed.quota_scope_id,
            recipients: closed.recipient_count,
            consumed: closed.quota_state == Quota::Consumed.as_str(),
        }
    }
}

/// Settles the ledgers from exactly the attempts that closed: each against the connection's row
/// of its own reservation day and, when it reserved on a scope, the scope's row of that day; the
/// connection's rows first, then the scope's, each in day order. Called only with the rows an
/// `UPDATE … WHERE quota_state = 'reserved' RETURNING` closed, so a reservation settles once.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn settle(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    closed: &[Reservation],
) -> Result<(), sqlx::Error> {
    // day → (released, consumed) messages on the connection.
    let mut days: BTreeMap<Date, (i32, i32)> = BTreeMap::new();
    // (day, scope) → (messages released, consumed, recipients released, consumed).
    let mut scopes: BTreeMap<(Date, Uuid), (i32, i32, i32, i32)> = BTreeMap::new();
    for attempt in closed {
        let entry = days.entry(attempt.day).or_default();
        if attempt.consumed {
            entry.1 = entry.1.saturating_add(1);
        } else {
            entry.0 = entry.0.saturating_add(1);
        }
        if let Some(scope) = attempt.scope {
            let entry = scopes.entry((attempt.day, scope)).or_default();
            if attempt.consumed {
                entry.1 = entry.1.saturating_add(1);
                entry.3 = entry.3.saturating_add(attempt.recipients);
            } else {
                entry.0 = entry.0.saturating_add(1);
                entry.2 = entry.2.saturating_add(attempt.recipients);
            }
        }
    }
    for (day, (released, consumed)) in days {
        sqlx::query!(
            "UPDATE connection_usage SET reserved = reserved - $4 - $5, used = used + $5
              WHERE workspace_id = $1 AND connection_id = $2 AND day = $3",
            workspace.uuid(),
            connection.uuid(),
            day as _,
            released,
            consumed,
        )
        .execute(&mut **tx)
        .await?;
    }
    for ((day, scope), (released, consumed, recipients_released, recipients_consumed)) in scopes {
        sqlx::query!(
            "UPDATE quota_scope_usage
                SET messages_reserved = messages_reserved - $4 - $5, messages_used = messages_used + $5,
                    recipients_reserved = recipients_reserved - $6 - $7, recipients_used = recipients_used + $7
              WHERE workspace_id = $1 AND scope_id = $2 AND day = $3",
            workspace.uuid(),
            scope,
            day as _,
            released,
            consumed,
            recipients_released,
            recipients_consumed,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Writes the connection's breaker when the batch changed it, and clears its budget wait when a
/// reservation was released (budget freed).
async fn write_connection(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    before: &Breaker,
    after: &Breaker,
    released: bool,
) -> Result<(), sqlx::Error> {
    if before == after && !released {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE connections
            SET consecutive_failures = $3, paused_until = $4, breaker_opened_at = $5,
                probe_message_id = $6, probe_generation = $7,
                next_claim_at = CASE WHEN $8 THEN NULL ELSE next_claim_at END
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        connection.uuid(),
        after.failures,
        after.paused_until.map(Timestamp) as _,
        after.opened_at.map(Timestamp) as _,
        after.probe.map(|probe| probe.message),
        after.probe.map(|probe| probe.generation),
        released,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Writes the scope's breaker, locked by this batch; an opening names the answer that opened it.
async fn write_scope(
    tx: &mut Tx,
    workspace: WorkspaceId,
    scope: Uuid,
    breaker: &Breaker,
    decided: &[Decided<'_>],
) -> Result<(), sqlx::Error> {
    let detail = decided
        .iter()
        .rev()
        .find_map(|decision| match &decision.report.reported {
            Reported::Answered(answered)
                if matches!(
                    answered.answer,
                    Answer::Refused(Refusal {
                        scope: RefusalScope::QuotaScope,
                        ..
                    })
                ) =>
            {
                Some(answered.diagnostic.clone())
            }
            _ => None,
        });
    sqlx::query!(
        "UPDATE quota_scopes
            SET consecutive_failures = $3, paused_until = $4, breaker_opened_at = $5,
                probe_message_id = $6, probe_generation = $7,
                paused_detail = CASE WHEN $4::timestamptz IS NULL THEN NULL ELSE coalesce($8, paused_detail) END
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        scope,
        breaker.failures,
        breaker.paused_until.map(Timestamp) as _,
        breaker.opened_at.map(Timestamp) as _,
        breaker.probe.map(|probe| probe.message),
        breaker.probe.map(|probe| probe.generation),
        detail,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Writes the messages' new states (rows locked in step 2).
async fn write_messages(
    tx: &mut Tx,
    workspace: WorkspaceId,
    decided: &[Decided<'_>],
) -> Result<(), sqlx::Error> {
    if decided.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = decided.iter().map(|d| d.owned.message_id).collect();
    let states: Vec<&str> = decided.iter().map(|d| d.next.state.as_str()).collect();
    let details: Vec<Option<String>> = decided.iter().map(|d| d.detail.clone()).collect();
    sqlx::query!(
        "UPDATE messages m SET state = d.state, status_detail = d.detail,
                sent_at = CASE WHEN d.state = 'sent' THEN now() ELSE m.sent_at END
           FROM unnest($2::uuid[], $3::text[], $4::text[]) AS d(id, state, detail)
          WHERE m.workspace_id = $1 AND m.id = d.id",
        workspace.uuid(),
        &ids,
        &states as _,
        &details as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Deletes the queue rows of messages in a final state and puts the others back, unleased, due
/// at their next instant, their submission marker and reservation day cleared.
async fn write_queue(
    tx: &mut Tx,
    workspace: WorkspaceId,
    decided: &[Decided<'_>],
) -> Result<(), sqlx::Error> {
    let done: Vec<Uuid> = decided
        .iter()
        .filter(|d| policy::is_final(d.next.state))
        .map(|d| d.owned.message_id)
        .collect();
    if !done.is_empty() {
        sqlx::query!(
            "DELETE FROM delivery_queue WHERE workspace_id = $1 AND message_id = ANY($2)",
            workspace.uuid(),
            &done,
        )
        .execute(&mut **tx)
        .await?;
    }
    let again: Vec<&Decided<'_>> = decided
        .iter()
        .filter(|d| !policy::is_final(d.next.state))
        .collect();
    if again.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = again.iter().map(|d| d.owned.message_id).collect();
    let run_at: Vec<Option<Timestamp>> = again
        .iter()
        .map(|d| d.next.retry_at.map(Timestamp))
        .collect();
    sqlx::query!(
        "UPDATE delivery_queue q
            SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL, submission_started_at = NULL,
                reserved_day = NULL, run_at = coalesce(d.run_at, q.run_at)
           FROM unnest($2::uuid[], $3::timestamptz[]) AS d(message_id, run_at)
          WHERE q.workspace_id = $1 AND q.message_id = d.message_id",
        workspace.uuid(),
        &ids,
        &run_at as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A message that failed as expired resolves the holds it caused: nothing will try it again.
async fn expire_holds(
    tx: &mut Tx,
    workspace: WorkspaceId,
    decided: &[Decided<'_>],
) -> Result<(), sqlx::Error> {
    let expired: Vec<Uuid> = decided
        .iter()
        .filter(|d| d.next.expired)
        .map(|d| d.owned.message_id)
        .collect();
    if expired.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE recipient_holds SET resolved_at = now(), resolution = 'expired'
          WHERE workspace_id = $1 AND message_id = ANY($2) AND resolved_at IS NULL",
        workspace.uuid(),
        &expired,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Amazon SES replaces our Message-ID with its own: each accepted message's token goes into the
/// directory, with its thread and envelope, so a reply naming the header the recipient saw still
/// finds its thread after the message's own row is archived. The header the recipient saw is
/// SES's, whose exact form is not verified for every Region, so the thread's latest
/// `last_internet_message_id` is cleared when it names this message: a follow-up then goes
/// without an `In-Reply-To` the recipient never saw, threaded by its subject.
async fn write_directory(
    tx: &mut Tx,
    workspace: WorkspaceId,
    decided: &[Decided<'_>],
) -> Result<(), sqlx::Error> {
    for decision in decided {
        let (MessageState::Sent, Some(thread), Reported::Answered(answered)) = (
            decision.next.state,
            decision.owned.thread_id,
            &decision.report.reported,
        ) else {
            continue;
        };
        let Some(token) = &answered.provider_message_id else {
            continue;
        };
        sqlx::query!(
            "INSERT INTO message_id_directory (workspace_id, lookup_key, message_id, thread_id, recipients)
             VALUES ($1, $2, $3, $4, $5)",
            workspace.uuid(),
            token,
            decision.owned.message_id,
            thread,
            &decision.owned.recipients,
        )
        .execute(&mut **tx)
        .await?;
        sqlx::query!(
            "UPDATE threads SET last_internet_message_id = NULL
              WHERE workspace_id = $1 AND id = $2 AND last_message_id = $3",
            workspace.uuid(),
            thread,
            decision.owned.message_id,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
