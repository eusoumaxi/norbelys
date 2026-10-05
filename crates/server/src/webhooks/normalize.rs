//! The normaliser: the `receipts.normalize` job, which turns the receipts the ingress stored into
//! delivery evidence.
//!
//! The ingress answers a provider as soon as a verified receipt is committed (see
//! `webhooks::ingress`); understanding it waits for this job, which the same transaction
//! enqueued with the ids of the receipts it stored. For each receipt still `received`, under its
//! row lock, the job:
//!
//! 1. **Parses** it with the provider's parser in the mail crate. A receipt that is not the
//!    provider's format is `quarantined` (its body kept for review) and has no effect.
//! 2. **Matches** each event to one of our messages: by our message id in the provider's
//!    metadata (an SES message tag, a SendGrid unique argument, a Mailgun variable), else by the
//!    `Message-ID` we composed, whose tag must verify (the managed MTA's events carry it). The
//!    message must exist in the workspace and have been sent through the webhook's own
//!    connection: anything else (another connection's message, one archived since, mail the
//!    customer sent outside Norbelys through the same account) is stored unmatched, with no
//!    message, for review.
//! 3. **Records** the events as evidence ([`crate::delivery::evidence::record`]) with the kind,
//!    category and confidence `domain::receipts` decides. The evidence rules do the rest:
//!    an `uncertain` message the event proves the provider took becomes `sent` (reconciliation
//!    by a provider's later event), recipients are held or suppressed as far as the confidence
//!    allows, counters move and `delivery_event.recorded` is told. A review an event would
//!    propose (a status notification the managed MTA matched only by its VERP address) has no
//!    inbound message to be attached to: the event itself, listed with its confidence, is the
//!    record a person reviews.
//! 4. **Marks** the receipt `normalized` (its body dropped: the events keep what it said) and
//!    notes the webhook's `last_event_at`.
//!
//! All of it commits in one chunk transaction with the job's checkpoint, so a receipt is
//! normalised exactly once: a rerun (a retry, a recovered lease) finds it no longer `received`.
//!
//! Then, in a short transaction of its own, the campaign enrollments of every address the chunk
//! suppressed are stopped (`campaigns::enrollments::stop_suppressed`): stopping locks enrollment
//! rows before queue rows and messages, so it never runs where evidence already holds message
//! rows. Should the job stop in between, those people still receive nothing (every Start re-reads
//! suppressions) and their enrollment ends at its next step.
//!
//! Lock order: the receipt rows, then what evidence locks (message rows, holds, suppressions),
//! then the webhook's row.

use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use norbelys_mail::webhooks::{self as mail, ParseError, mailgun, norbelys, sendgrid, ses};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::accept;
use crate::delivery::evidence::{self, Evidence};
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::policy::delivery::{RecipientEffect, RecipientRef, Source};
use crate::domain::receipts;
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue};

/// Receipts normalised per chunk: a receipt is at most 1 MiB, so a chunk stays within memory
/// and well inside the job's lease.
const CHUNK: usize = 50;

static LAG: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_gauge("norbelys_receipts_lag_seconds")
        .with_unit("s")
        .with_description(
            "Age of the oldest receipt a normalisation chunk turned into evidence: from its arrival \
             at the ingress to its evidence, by provider.",
        )
        .build()
});

/// The `receipts.normalize` job: the receipts one ingress micro-batch stored for one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Normalize {
    /// The receipts' ids, at most one micro-batch (500).
    pub receipts: Vec<Uuid>,
}

impl Job for Normalize {
    const KIND: &'static str = "receipts.normalize";
    const QUEUE: Queue = Queue::Receipts;
    const EFFECT: Effect = Effect::Idempotent;

    /// The batch's identity: its first receipt's id, unique to that batch.
    fn unique_key(&self) -> Option<String> {
        self.receipts.first().map(Uuid::to_string)
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let keys = cx.env::<Keys>()?.clone();
        for ids in self.receipts.chunks(CHUNK) {
            if cx.should_yield() {
                // The receipts already normalised are no longer `received`: the next run skips
                // them, so no progress needs to be kept.
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let started = Instant::now();
            let workspace = cx.workspace();
            let mut chunk = cx.begin().await?;
            let done = normalize(chunk.tx(), workspace, &keys, ids).await?;
            cx.checkpoint(chunk, json!({})).await?;
            stop_enrollments(cx, &done.suppressed).await?;
            done.report(started.elapsed());
        }
        Ok(Outcome::Done)
    }
}

/// What normalising one chunk did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Normalized {
    /// Receipts turned into evidence.
    pub receipts: u64,
    /// Receipts that could not be parsed, set aside.
    pub quarantined: u64,
    /// Events recorded.
    pub events: u64,
    /// Events that held a recipient.
    pub holds: u64,
    /// Events that suppressed an address.
    pub suppressions: u64,
    /// Events that name none of our messages.
    pub unmatched: u64,
    /// The addresses the events suppressed, whose enrollments stop after the commit.
    pub suppressed: Vec<String>,
    /// The provider of the oldest receipt and how long ago it arrived.
    lag: Option<(Provider, Duration)>,
}

impl Normalized {
    /// Emits the chunk's canonical event and its lag.
    fn report(&self, took: Duration) {
        if let Some((provider, lag)) = self.lag {
            LAG.record(
                lag.as_secs_f64(),
                &[KeyValue::new("provider", provider.as_str())],
            );
        }
        crate::telemetry::unit(crate::telemetry::Event::ReceiptsNormalize);
        tracing::info!(
            event = "receipts.normalize",
            receipts = self.receipts,
            quarantined = self.quarantined,
            events = self.events,
            holds = self.holds,
            suppressions = self.suppressions,
            unmatched = self.unmatched,
            lag_ms = self.lag.map_or(0, |(_, lag)| u64::try_from(lag.as_millis())
                .unwrap_or(u64::MAX)),
            duration_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
            "provider receipts normalised"
        );
    }
}

/// A message an event names, as far as matching needs it.
struct Known {
    thread: Option<Uuid>,
    connection: Uuid,
    /// The envelope's one address, when it had exactly one.
    single: Option<String>,
}

/// Normalises the receipts of `ids` that are still `received` in `workspace` (see the module),
/// in the caller's transaction.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn normalize(
    tx: &mut Tx,
    workspace: WorkspaceId,
    keys: &Keys,
    ids: &[Uuid],
) -> Result<Normalized, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT r.id, r.provider_webhook_id, r.event_id, r.raw, r.received_at AS "received_at: Timestamp",
                  w.provider, w.connection_id
             FROM webhook_receipts r
             JOIN provider_webhooks w ON w.workspace_id = r.workspace_id AND w.id = r.provider_webhook_id
            WHERE r.workspace_id = $1 AND r.id = ANY($2) AND r.state = 'received'
            ORDER BY r.id
              FOR UPDATE OF r SKIP LOCKED"#,
        workspace.uuid(),
        ids,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut done = Normalized::default();
    if rows.is_empty() {
        return Ok(done);
    }
    let now = crate::process::now();
    let mut parsed = Vec::new();
    let mut quarantined = Vec::new();
    for row in &rows {
        let provider = Provider::from_str(&row.provider).ok();
        match parse(
            provider,
            row.raw.as_deref().unwrap_or_default(),
            &row.event_id,
        ) {
            Ok(events) => parsed.push((row, events)),
            Err(error) => {
                tracing::warn!(receipt_id = %row.id, provider = %row.provider, error = %error,
                               "a provider receipt was quarantined");
                quarantined.push(row.id);
            }
        }
        if done.lag.is_none()
            && let Some(provider) = provider
        {
            done.lag = Some((provider, elapsed(row.received_at, now)));
        }
    }

    // Match: our message id from the provider's metadata, else the Message-ID we composed.
    let named = |event: &mail::Event| -> Option<(Id<Message>, Option<Uuid>)> {
        event
            .message_id
            .map(|id| (Id::from_uuid(id), None))
            .or_else(|| {
                let (message, thread) =
                    accept::correlate(keys, event.internet_message_id.as_deref()?)?;
                Some((message, Some(thread.uuid())))
            })
    };
    let candidates: Vec<Uuid> = parsed
        .iter()
        .flat_map(|(_, events)| events.iter().filter_map(named))
        .map(|(message, _)| message.uuid())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let known: HashMap<Uuid, Known> = sqlx::query!(
        r#"SELECT id, thread_id, connection_id, recipient_count AS "recipient_count!", to_addresses[1] AS "first!"
             FROM messages WHERE workspace_id = $1 AND id = ANY($2)"#,
        workspace.uuid(),
        &candidates,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.id,
            Known {
                thread: row.thread_id,
                connection: row.connection_id,
                single: (row.recipient_count == 1).then_some(row.first),
            },
        )
    })
    .collect();

    let mut observed = Vec::new();
    for (row, events) in &parsed {
        for event in events {
            let matched = named(event).and_then(|(message, thread)| {
                let found = known.get(&message.uuid())?;
                (found.connection == row.connection_id).then_some((message, thread, found))
            });
            if matched.is_none() {
                done.unmatched += 1;
            }
            observed.push(evidence_of(event, row.id, matched));
        }
    }
    let recorded = evidence::record(tx, workspace, &observed).await?;
    done.events = u64::try_from(recorded.len()).unwrap_or(u64::MAX);
    for (event, observed) in recorded.iter().zip(&observed) {
        match event.effect {
            RecipientEffect::Hold(_) | RecipientEffect::HoldAndReview(..) => done.holds += 1,
            RecipientEffect::Suppress(_) => {
                done.suppressions += 1;
                done.suppressed.extend(observed.recipient.clone());
            }
            RecipientEffect::None | RecipientEffect::Review(_) => {}
        }
    }

    let all: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    sqlx::query!(
        "UPDATE webhook_receipts
            SET state = CASE WHEN id = ANY($3) THEN 'quarantined' ELSE 'normalized' END,
                raw = CASE WHEN id = ANY($3) THEN raw END,
                processed_at = now()
          WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &all,
        &quarantined,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE provider_webhooks w SET last_event_at = greatest(w.last_event_at, r.at)
           FROM (SELECT provider_webhook_id, max(received_at) AS at FROM webhook_receipts
                  WHERE workspace_id = $1 AND id = ANY($2) GROUP BY provider_webhook_id) r
          WHERE w.workspace_id = $1 AND w.id = r.provider_webhook_id",
        workspace.uuid(),
        &all,
    )
    .execute(&mut **tx)
    .await?;
    done.quarantined = u64::try_from(quarantined.len()).unwrap_or(u64::MAX);
    done.receipts = u64::try_from(all.len().saturating_sub(quarantined.len())).unwrap_or(u64::MAX);
    Ok(done)
}

/// A stored receipt's events, by its webhook's provider; a provider that posts no callbacks
/// cannot have stored one.
fn parse(
    provider: Option<Provider>,
    raw: &[u8],
    event_id: &str,
) -> Result<Vec<mail::Event>, ParseError> {
    match provider {
        Some(Provider::Mailgun) => mailgun::events(raw),
        Some(Provider::Sendgrid) => sendgrid::events(raw),
        Some(Provider::Ses) => ses::events(raw, event_id),
        Some(Provider::Norbelys) => norbelys::events(raw),
        Some(Provider::Smtp | Provider::Google | Provider::Microsoft) | None => Err(ParseError(
            "the webhook's provider posts no callbacks".to_owned(),
        )),
    }
}

/// The evidence one provider event is, from receipt `receipt`: about `matched` (our message, its
/// thread when the Message-ID carried it, and what we know of it) or about no message of ours.
/// An event that names no recipient is about the envelope's one address when it had only one.
fn evidence_of(
    event: &mail::Event,
    receipt: Uuid,
    matched: Option<(Id<Message>, Option<Uuid>, &Known)>,
) -> Evidence {
    let (kind, category) = receipts::outcome(event.kind, event.status);
    let (recipient, recipient_ref) = match (
        &event.recipient,
        matched.and_then(|(.., known)| known.single.as_ref()),
    ) {
        (Some(named), _) => (Some(named.clone()), RecipientRef::Named),
        (None, Some(single)) => (Some(single.clone()), RecipientRef::SingleEnvelope),
        (None, None) => (None, RecipientRef::Unknown),
    };
    Evidence {
        message: matched.map(|(message, ..)| message),
        thread: matched.and_then(|(_, thread, known)| thread.or(known.thread)),
        attempt_number: None,
        recipient,
        recipient_ref,
        source: Source::ProviderWebhook,
        source_event_id: event.event_id.clone(),
        received_via: None,
        kind,
        action: None,
        phase: None,
        enhanced_status: event.status.map(|status| status.to_string()),
        category,
        diagnostic: event.diagnostic.clone(),
        confidence: receipts::confidence(event.provenance, matched.is_some()),
        receipt: Some(receipt),
        observed_at: Timestamp(event.observed_at),
    }
}

/// How long before `now` `at` was; zero for an instant not yet past (another host's clock).
fn elapsed(at: Timestamp, now: Timestamp) -> Duration {
    now.0
        .duration_since(at.0)
        .try_into()
        .unwrap_or(Duration::ZERO)
}

/// Stops the campaign enrollments of the addresses a chunk suppressed, in a transaction of its
/// own after the chunk's commit: stopping locks enrollment rows before queue rows and messages,
/// and the chunk's evidence held message rows, so the two never share a transaction. A crash in
/// between leaves them running only until their next step, whose Start re-reads suppressions
/// and sends nothing.
async fn stop_enrollments(cx: &mut JobContext, suppressed: &[String]) -> Result<(), JobError> {
    if suppressed.is_empty() {
        return Ok(());
    }
    let workspace = cx.workspace();
    let mut chunk = cx.begin().await?;
    for email in suppressed {
        crate::campaigns::enrollments::stop_suppressed(chunk.tx(), workspace, email).await?;
    }
    cx.checkpoint(chunk, json!({})).await
}
