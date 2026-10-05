//! The ingress of provider webhooks: `POST /webhooks/{provider_webhook_id}`, where Amazon SES
//! (through SNS), SendGrid, Mailgun and the managed MTA post their signed events about our mail.
//!
//! The api serves the route in both of its modes; a deployment points providers at the
//! processes of its ingress mode, on the public host, so a flood of callbacks never competes
//! with the product API.
//!
//! # A request
//!
//! 1. **The webhook.** The URL names one provider webhook, bound to one connection. Its
//!    workspace is resolved by `provider_webhook_workspace()` before any tenant query (the
//!    lookup function sees only the webhook's id and status), then its provider, connection and
//!    sealed key are read inside that workspace. An unknown or disabled webhook is `404`; one
//!    whose key the customer has not pasted yet (SendGrid shows its key only once our URL is
//!    configured) is `401`, which providers retry, so nothing is lost while it is configured.
//! 2. **Verification**, per provider, by the mail crate, before the body is trusted: Mailgun's
//!    HMAC over its timestamp and token, SendGrid's ECDSA signature, Amazon SNS's message
//!    signature (the signing certificate fetched under its own bounded permit, shared by the
//!    whole process) and the managed MTA's Standard Webhooks signature, each with its own
//!    freshness window. An SNS subscription confirmation is confirmed at once (the confirmation
//!    URL is rebuilt from the configured topic, never taken from the message).
//! 3. **The receipt.** Each provider event's key `(provider webhook, event id)` and its receipt
//!    (the body's SHA-256, the verified bytes, at most 1 MiB) are committed before anything else
//!    happens, together with the `receipts.normalize` job that will turn them into evidence. A
//!    key that already exists is a replay: it is answered with success and stores nothing, on
//!    the same day, across days (keys live three days, longer than any provider retries) and
//!    from two concurrent deliveries (the key is one global row). A replay whose body differs
//!    from the first one's is stored `quarantined`, its body kept for review and never
//!    normalised: the first body stands.
//! 4. **The answer.** `200` once the receipt is committed, which every provider takes as success.
//!    Refusals are problems (`401` signature, `400` body, `413` size), except for Mailgun, which
//!    treats `406` as "do not retry" and every other failure as "retry": a refused Mailgun
//!    callback is answered `406`. `503` only when neither the database nor the local spool could
//!    take the receipt, or when the SNS certificate could not be fetched: the provider retries.
//!
//! # Micro-batches
//!
//! Requests hand their receipts to one batcher task, which commits what has arrived together
//! (up to [`BATCH_MAX`] receipts and 1 MiB of their bodies) in one transaction per workspace, then
//! answers each request. It never waits for more to arrive: when it is idle a request is
//! committed alone at once, and under load the receipts that arrived during a commit form the
//! next one, so the database sees one commit per batch rather than one per callback, well inside
//! 200 ms.
//!
//! # The spool
//!
//! When the database cannot take a batch (unavailable, its pool saturated), its receipts are
//! written to the process's local spool ([`crate::spool`], `INGRESS_SPOOL_DIR`) and the request is
//! answered `200`; the [`drain`] task stores them once the database is back, through the same
//! path (so replays are still recognised then). Mailgun does not retry delivery notifications,
//! which is why a callback must be kept rather than refused. A webhook's key is read from the
//! database on every request; when the lookup itself fails, the last resolution of that webhook
//! (kept in memory for an hour) verifies the callback, so an outage does not refuse the webhooks
//! that were active before it.

use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use norbelys_mail::webhooks::{Receipt, VerifyError, mailgun, norbelys, sendgrid, ses};
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use super::normalize::Normalize;
use crate::crypto::{self, Keys};
use crate::db::Database;
use crate::domain::ids::{Id, ProviderWebhook, WorkspaceId};
use crate::domain::retry;
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::Path;
use crate::jobs::{self, Queue};
use crate::problem::{Code, Problem};
use crate::process::Shutdown;
use crate::senders::credentials;
use crate::spool::{self, Spool};

/// Receipts committed in one micro-batch at most.
pub const BATCH_MAX: usize = 500;
/// Bytes of receipt bodies committed in one micro-batch at most: a batch is one transaction
/// whose bodies are held in memory until it commits, and a receipt alone may hold 1 MiB.
const BATCH_BYTES: usize = 1 << 20;
/// Requests waiting for the batcher at most; beyond it requests wait (backpressure).
const QUEUE: usize = 1_024;
/// How long a webhook's last resolution may stand in for the database during an outage.
const RESOLVED_TTL: Duration = Duration::from_secs(3_600);
/// Webhooks whose resolution is kept at most.
const RESOLVED_MAX: u64 = 10_000;
/// Concurrent fetches of SNS signing certificates, for the whole process.
const CERTIFICATE_FETCHES: usize = 4;
/// How long confirming an SNS subscription may take.
const CONFIRM_BUDGET: Duration = Duration::from_secs(10);
/// Spooled requests the drain stores per pass.
const DRAIN_BATCH: usize = 20;
/// The drain's wait when the spool is empty.
const DRAIN_IDLE: Duration = Duration::from_secs(1);
/// The format of the spool's records.
const SPOOL_VERSION: u8 = 1;

static FLUSH_DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_receipts_flush_duration_seconds")
        .with_unit("s")
        .with_description("How long committing one micro-batch of provider receipts took.")
        .build()
});

static SPOOLED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_receipts_spooled_total")
        .with_description(
            "Provider callbacks written to the ingress spool because the database could not take them, by provider.",
        )
        .build()
});

static QUARANTINED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_receipts_quarantined_total")
        .with_description(
            "Provider events replayed with a body that differs from the first one's, kept for review, by provider.",
        )
        .build()
});

static SPOOL_BYTES: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_ingress_spool_bytes")
        .with_unit("By")
        .with_description("Live bytes of the ingress spool: callbacks not yet stored.")
        .build()
});

/// The route; the api serves it in both modes.
pub fn routes() -> Router<AppState> {
    Router::new().route("/webhooks/{provider_webhook_id}", post(receive))
}

/// One request's verified receipts, waiting to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    /// The webhook's workspace, from the lookup function.
    pub workspace: WorkspaceId,
    /// The webhook the URL named.
    pub webhook: Id<ProviderWebhook>,
    /// Its provider.
    pub provider: Provider,
    /// When the request arrived: the receipts' `received_at`, also when they are stored later
    /// from the spool.
    pub received_at: Timestamp,
    /// The provider's events, as verified.
    pub receipts: Vec<Receipt>,
}

/// An [`Arrival`] as the spool writes it: versioned JSON, the bodies in base64.
#[derive(Serialize, Deserialize)]
struct Spooled {
    v: u8,
    workspace: Uuid,
    webhook: Uuid,
    provider: Provider,
    received_at: Timestamp,
    receipts: Vec<(String, String)>,
}

impl spool::Record for Arrival {
    const NAME: &'static str = "ingress";

    fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&Spooled {
            v: SPOOL_VERSION,
            workspace: self.workspace.uuid(),
            webhook: self.webhook.uuid(),
            provider: self.provider,
            received_at: self.received_at,
            receipts: self
                .receipts
                .iter()
                .map(|receipt| (receipt.event_id.clone(), STANDARD.encode(&receipt.raw)))
                .collect(),
        })
        .map_err(|error| error.to_string())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let spooled: Spooled = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if spooled.v != SPOOL_VERSION {
            return Err(format!("the record's version {} is unknown", spooled.v));
        }
        let receipts = spooled
            .receipts
            .into_iter()
            .map(|(event_id, raw)| {
                STANDARD
                    .decode(raw)
                    .map(|raw| Receipt { event_id, raw })
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            // The webhook's lookup named this workspace before its callback was verified and
            // spooled.
            workspace: WorkspaceId::trusted(spooled.workspace),
            webhook: Id::from_uuid(spooled.webhook),
            provider: spooled.provider,
            received_at: spooled.received_at,
            receipts,
        })
    }
}

/// What storing one arrival did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stored {
    /// New receipts, waiting for the normaliser.
    pub received: usize,
    /// Replays: nothing stored.
    pub duplicates: usize,
    /// Replays with another body, kept for review.
    pub quarantined: usize,
}

/// Why a callback could not be kept: answered `503`, so the provider retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("neither the database nor the spool could take the callback")]
pub struct Unavailable;

/// How a callback was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// Committed to the database.
    Stored(Stored),
    /// Written to the local spool, for the drain.
    Spooled,
}

struct Pending {
    arrival: Arrival,
    reply: oneshot::Sender<Result<Kept, Unavailable>>,
}

/// A webhook as a request needs it.
#[derive(Debug)]
struct Resolved {
    workspace: WorkspaceId,
    provider: Provider,
    /// The verification material, opened; `None` until the customer has set it.
    key: Option<SecretString>,
}

/// The ingress's state, shared by every request of the process; cheap to clone.
#[derive(Clone)]
pub struct Ingress {
    batcher: mpsc::Sender<Pending>,
    certificates: Arc<ses::SnsCertificates>,
    resolved: moka::future::Cache<Uuid, Arc<Resolved>>,
}

impl std::fmt::Debug for Ingress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Ingress").finish_non_exhaustive()
    }
}

impl Ingress {
    /// Starts the batcher on the current runtime, committing to `db` and falling back to `spool`
    /// (none: a batch the database refuses is answered `503`).
    #[must_use]
    pub fn start(db: Database, spool: Option<Spool<Arrival>>) -> Self {
        let (batcher, pending) = mpsc::channel(QUEUE);
        tokio::spawn(batch(db, spool, pending));
        Self {
            batcher,
            certificates: Arc::new(ses::SnsCertificates::new(CERTIFICATE_FETCHES)),
            resolved: moka::future::Cache::builder()
                .max_capacity(RESOLVED_MAX)
                .time_to_live(RESOLVED_TTL)
                .build(),
        }
    }

    /// The SNS signing certificates of this process.
    #[cfg(test)]
    pub(crate) fn certificates(&self) -> &ses::SnsCertificates {
        &self.certificates
    }

    /// Hands `arrival` to the batcher and waits until it is committed or spooled.
    ///
    /// # Errors
    ///
    /// [`Unavailable`]: neither the database nor the spool took it.
    pub async fn keep(&self, arrival: Arrival) -> Result<Kept, Unavailable> {
        let (reply, answer) = oneshot::channel();
        self.batcher
            .send(Pending { arrival, reply })
            .await
            .map_err(|_| Unavailable)?;
        answer.await.map_err(|_| Unavailable)?
    }

    /// The webhook `id` names, from the database, else (the database failed) from its last
    /// resolution. `Ok(None)`: no such active webhook.
    async fn resolve(
        &self,
        db: &Database,
        keys: &Keys,
        id: Id<ProviderWebhook>,
    ) -> Result<Option<Arc<Resolved>>, sqlx::Error> {
        match lookup(db, keys, id).await {
            Ok(Some(resolved)) => {
                let resolved = Arc::new(resolved);
                self.resolved.insert(id.uuid(), Arc::clone(&resolved)).await;
                Ok(Some(resolved))
            }
            Ok(None) => {
                self.resolved.invalidate(&id.uuid()).await;
                Ok(None)
            }
            Err(error) => match self.resolved.get(&id.uuid()).await {
                Some(resolved) => Ok(Some(resolved)),
                None => Err(error),
            },
        }
    }
}

/// Reads webhook `id`: its workspace through the lookup function, then its row inside that
/// workspace.
async fn lookup(
    db: &Database,
    keys: &Keys,
    id: Id<ProviderWebhook>,
) -> Result<Option<Resolved>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let workspace = sqlx::query_scalar!(
        r#"SELECT provider_webhook_workspace($1) AS "workspace""#,
        id.uuid()
    )
    .fetch_one(&mut *tx)
    .await?;
    let Some(workspace) = workspace.map(WorkspaceId::trusted) else {
        return Ok(None);
    };
    crate::db::set_workspace(&mut tx, workspace).await?;
    let row = sqlx::query!(
        "SELECT provider, signing_secret FROM provider_webhooks
          WHERE workspace_id = $1 AND id = $2 AND status = 'active'",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let Ok(provider) = Provider::from_str(&row.provider) else {
        return Ok(None);
    };
    let key = row.signing_secret.and_then(|sealed| {
        credentials::open_webhook_key(keys, workspace, id, &sealed)
            .inspect_err(|error| {
                tracing::error!(provider_webhook_id = %id, error = %error,
                                "a provider webhook's key does not open");
            })
            .ok()
    });
    Ok(Some(Resolved {
        workspace,
        provider,
        key,
    }))
}

/// What a verified request asks for.
enum Verified {
    /// Store these receipts.
    Receipts(Vec<Receipt>),
    /// Confirm the subscription of our URL to this SNS topic.
    Confirm(ses::SnsTopic, ses::SubscriptionConfirmation),
    /// Nothing (SNS confirms an unsubscription).
    Nothing,
}

/// Receive a provider's callback (see the module).
async fn receive(
    State(state): State<AppState>,
    Path(id): Path<Id<ProviderWebhook>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let received_at = crate::process::now();
    let resolved = match state.ingress.resolve(&state.db, &state.keys, id).await {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return Problem::not_found("provider webhook").into_response(),
        Err(error) => {
            tracing::warn!(provider_webhook_id = %id, error = %error,
                           "a provider webhook could not be resolved");
            return Problem::unavailable(30).into_response();
        }
    };
    let Some(key) = resolved.key.as_ref() else {
        return Problem::new(
            Code::Unauthorized,
            "This webhook's verification key is not set yet: callbacks are refused until it is.",
        )
        .into_response();
    };
    let verified = match verify(&state, resolved.provider, key, &headers, &body, received_at).await
    {
        Ok(verified) => verified,
        Err(error) => return refusal(resolved.provider, &error),
    };
    match verified {
        Verified::Receipts(receipts) if receipts.is_empty() => StatusCode::OK.into_response(),
        Verified::Receipts(receipts) => {
            let arrival = Arrival {
                workspace: resolved.workspace,
                webhook: id,
                provider: resolved.provider,
                received_at,
                receipts,
            };
            match state.ingress.keep(arrival).await {
                Ok(_) => StatusCode::OK.into_response(),
                Err(Unavailable) => Problem::unavailable(30).into_response(),
            }
        }
        Verified::Confirm(topic, confirmation) => confirm(&state, id, &topic, &confirmation).await,
        Verified::Nothing => StatusCode::OK.into_response(),
    }
}

/// Verifies a callback of `provider` with its `key`, as of `now`.
async fn verify(
    state: &AppState,
    provider: Provider,
    key: &SecretString,
    headers: &HeaderMap,
    body: &[u8],
    now: Timestamp,
) -> Result<Verified, VerifyError> {
    let unusable = |_| VerifyError::Unauthorized("the webhook's key is not usable");
    match provider {
        Provider::Mailgun => {
            let key = mailgun::MailgunKey::new(key).map_err(unusable)?;
            Ok(Verified::Receipts(vec![mailgun::verify(
                &key, body, now.0,
            )?]))
        }
        Provider::Sendgrid => {
            let key = sendgrid::SendgridKey::new(key.expose_secret()).map_err(unusable)?;
            Ok(Verified::Receipts(sendgrid::verify(
                &key, headers, body, now.0,
            )?))
        }
        Provider::Norbelys => {
            let secret = super::deliver::secret_bytes(key.expose_secret())
                .ok_or(VerifyError::Unauthorized("the webhook's key is not usable"))?;
            let key = norbelys::NorbelysKey::new(&secret).map_err(unusable)?;
            Ok(Verified::Receipts(norbelys::verify(
                &key, headers, body, now.0,
            )?))
        }
        Provider::Ses => {
            let topic = ses::SnsTopic::new(key.expose_secret()).map_err(unusable)?;
            let message = ses::verify(
                &topic,
                &state.ingress.certificates,
                &state.settings.senders.http,
                body,
                now.0,
            )
            .await?;
            Ok(match message {
                ses::SnsMessage::Notification(receipt) => Verified::Receipts(vec![receipt]),
                ses::SnsMessage::SubscriptionConfirmation(confirmation) => {
                    Verified::Confirm(topic, confirmation)
                }
                ses::SnsMessage::UnsubscribeConfirmation => Verified::Nothing,
            })
        }
        Provider::Smtp | Provider::Google | Provider::Microsoft => Err(VerifyError::Unauthorized(
            "this provider posts no callbacks",
        )),
    }
}

/// The answer to a callback that did not verify.
fn refusal(provider: Provider, error: &VerifyError) -> Response {
    if let VerifyError::Unavailable(reason) = error {
        tracing::warn!(provider = provider.as_str(), reason = %reason,
                       "a provider callback could not be verified now");
        return Problem::unavailable(30).into_response();
    }
    if provider == Provider::Mailgun {
        // Mailgun retries everything but `406`, which it takes as a final refusal.
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let problem = match error {
        VerifyError::TooLarge => Problem::new(Code::PayloadTooLarge, error.to_string()),
        VerifyError::InvalidPayload(_) => Problem::new(Code::InvalidRequest, error.to_string()),
        VerifyError::Unauthorized(_) | VerifyError::Stale | VerifyError::Unavailable(_) => {
            Problem::new(Code::Unauthorized, error.to_string())
        }
    };
    problem.into_response()
}

/// Confirms the subscription of webhook `id`'s URL to `topic`: `200` once SNS accepted it,
/// `503` otherwise (SNS sends the confirmation again).
async fn confirm(
    state: &AppState,
    id: Id<ProviderWebhook>,
    topic: &ses::SnsTopic,
    confirmation: &ses::SubscriptionConfirmation,
) -> Response {
    let deadline = Instant::now()
        .checked_add(CONFIRM_BUDGET)
        .unwrap_or_else(Instant::now);
    match ses::confirm_subscription(&state.settings.senders.http, topic, confirmation, deadline)
        .await
    {
        Ok(()) => {
            tracing::info!(provider_webhook_id = %id, topic = topic.arn(),
                           "an SNS subscription was confirmed");
            StatusCode::OK.into_response()
        }
        Err(error) => {
            tracing::warn!(provider_webhook_id = %id, topic = topic.arn(), error = %error,
                           "an SNS subscription could not be confirmed");
            Problem::unavailable(60).into_response()
        }
    }
}

/// The batcher: commits what has arrived together, then answers (see the module). A batch takes
/// what has arrived while it stays within [`BATCH_MAX`] receipts and [`BATCH_BYTES`] of bodies;
/// the arrival that would pass either bound opens the next batch, and an arrival larger than a
/// bound on its own is committed alone.
async fn batch(db: Database, spool: Option<Spool<Arrival>>, mut pending: mpsc::Receiver<Pending>) {
    let mut held: Option<Pending> = None;
    loop {
        let first = match held.take() {
            Some(first) => first,
            None => match pending.recv().await {
                Some(first) => first,
                None => break,
            },
        };
        let mut size = Size::of(&first.arrival);
        let mut group = vec![first];
        while let Ok(next) = pending.try_recv() {
            let grown = size.and(&next.arrival);
            if !grown.fits() {
                held = Some(next);
                break;
            }
            size = grown;
            group.push(next);
        }
        flush(&db, spool.as_ref(), group).await;
    }
}

/// The size of a micro-batch: its receipts and the bytes of their bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Size {
    receipts: usize,
    bytes: usize,
}

impl Size {
    /// The size of `arrival` alone.
    fn of(arrival: &Arrival) -> Self {
        Self {
            receipts: arrival.receipts.len(),
            bytes: arrival
                .receipts
                .iter()
                .map(|receipt| receipt.raw.len())
                .fold(0, usize::saturating_add),
        }
    }

    /// This size with `arrival` added.
    fn and(self, arrival: &Arrival) -> Self {
        let added = Self::of(arrival);
        Self {
            receipts: self.receipts.saturating_add(added.receipts),
            bytes: self.bytes.saturating_add(added.bytes),
        }
    }

    /// Whether a batch of this size stays within both bounds.
    fn fits(self) -> bool {
        self.receipts <= BATCH_MAX && self.bytes <= BATCH_BYTES
    }
}

/// Commits `group` (one transaction per workspace), spooling what the database refuses, and
/// answers each request.
async fn flush(db: &Database, spool: Option<&Spool<Arrival>>, group: Vec<Pending>) {
    let mut by_workspace: Vec<(WorkspaceId, Vec<Pending>)> = Vec::new();
    for pending in group {
        let workspace = pending.arrival.workspace;
        match by_workspace.iter_mut().find(|(w, _)| *w == workspace) {
            Some((_, list)) => list.push(pending),
            None => by_workspace.push((workspace, vec![pending])),
        }
    }
    for (workspace, list) in by_workspace {
        let started = Instant::now();
        let arrivals: Vec<&Arrival> = list.iter().map(|pending| &pending.arrival).collect();
        match store(db, workspace, &arrivals).await {
            Ok(stored) => {
                let took = started.elapsed();
                for (pending, stored) in list.into_iter().zip(stored) {
                    report(&pending.arrival, stored, false, took);
                    let _ = pending.reply.send(Ok(Kept::Stored(stored)));
                }
            }
            Err(error) => {
                tracing::warn!(workspace_id = %workspace, error = %error,
                               "the database could not take a micro-batch of provider receipts");
                for pending in list {
                    let kept = match spool {
                        Some(spool) => spool.append(&pending.arrival).await.map_err(|error| {
                            tracing::error!(error = %error, "the ingress spool could not take a callback");
                            Unavailable
                        }),
                        None => Err(Unavailable),
                    };
                    if kept.is_ok() {
                        SPOOLED.add(
                            1,
                            &[KeyValue::new("provider", pending.arrival.provider.as_str())],
                        );
                        report(&pending.arrival, Stored::default(), true, started.elapsed());
                    }
                    let _ = pending.reply.send(kept.map(|()| Kept::Spooled));
                }
            }
        }
    }
}

/// The canonical event of one callback's receipts and the flush metrics.
fn report(arrival: &Arrival, stored: Stored, spooled: bool, took: Duration) {
    let provider = [KeyValue::new("provider", arrival.provider.as_str())];
    FLUSH_DURATION.record(took.as_secs_f64(), &provider);
    if stored.quarantined > 0 {
        QUARANTINED.add(
            u64::try_from(stored.quarantined).unwrap_or(u64::MAX),
            &provider,
        );
    }
    crate::telemetry::unit(crate::telemetry::Event::ReceiptsFlush);
    tracing::info!(
        event = "receipts.flush",
        provider_webhook_id = %arrival.webhook,
        provider = arrival.provider.as_str(),
        received = stored.received,
        duplicates = stored.duplicates,
        quarantined = stored.quarantined,
        spooled,
        duration_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        "provider receipts committed"
    );
}

/// What one receipt of a batch turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// A new key: stored `received`.
    New,
    /// A replay of the same body: nothing stored.
    Duplicate,
    /// A replay with another body: stored `quarantined`.
    Quarantined,
}

/// Commits the receipts of `arrivals` (all of `workspace`) in one transaction: their keys, the
/// new and quarantined receipts, and one `receipts.normalize` job for the new ones; wakes the
/// receipts queue after the commit. Returns what each arrival stored, in order.
///
/// # Errors
///
/// The database refused; nothing was stored.
pub(crate) async fn store(
    db: &Database,
    workspace: WorkspaceId,
    arrivals: &[&Arrival],
) -> Result<Vec<Stored>, sqlx::Error> {
    struct Item<'a> {
        arrival: usize,
        webhook: Uuid,
        event_id: &'a str,
        hash: Vec<u8>,
        raw: &'a [u8],
        received_at: Timestamp,
        fate: Option<Fate>,
    }
    let mut items: Vec<Item<'_>> = Vec::new();
    // Within the batch the first occurrence of a key stands for it; a later one is its replay.
    let mut first: HashMap<(Uuid, &str), Vec<u8>> = HashMap::new();
    for (index, arrival) in arrivals.iter().enumerate() {
        for receipt in &arrival.receipts {
            let hash = crypto::sha256(&receipt.raw);
            let key = (arrival.webhook.uuid(), receipt.event_id.as_str());
            let fate = match first.get(&key) {
                Some(seen) if *seen == hash => Some(Fate::Duplicate),
                Some(_) => Some(Fate::Quarantined),
                None => {
                    first.insert(key, hash.clone());
                    None
                }
            };
            items.push(Item {
                arrival: index,
                webhook: arrival.webhook.uuid(),
                event_id: &receipt.event_id,
                hash,
                raw: &receipt.raw,
                received_at: arrival.received_at,
                fate,
            });
        }
    }

    let mut tx = db.begin_in(workspace).await?;
    let candidates: Vec<&Item<'_>> = items.iter().filter(|item| item.fate.is_none()).collect();
    let webhooks: Vec<Uuid> = candidates.iter().map(|item| item.webhook).collect();
    let event_ids: Vec<String> = candidates
        .iter()
        .map(|item| item.event_id.to_owned())
        .collect();
    let hashes: Vec<Vec<u8>> = candidates.iter().map(|item| item.hash.clone()).collect();
    // A key that exists is a replay: the conflict leaves it as the first delivery wrote it. A
    // key a concurrent transaction is inserting makes this one wait for it, then conflict.
    let inserted: HashSet<(Uuid, String)> = sqlx::query!(
        "INSERT INTO provider_event_keys (provider_webhook_id, event_id, body_hash)
         SELECT * FROM unnest($1::uuid[], $2::text[], $3::bytea[])
         ON CONFLICT DO NOTHING
         RETURNING provider_webhook_id, event_id",
        &webhooks,
        &event_ids,
        &hashes,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| (row.provider_webhook_id, row.event_id))
    .collect();
    // The first body's hash of every replayed key, read by a statement of its own so that it
    // sees a key another transaction committed while the insert waited.
    let (replayed_webhooks, replayed_ids): (Vec<Uuid>, Vec<String>) = candidates
        .iter()
        .filter(|item| !inserted.contains(&(item.webhook, item.event_id.to_owned())))
        .map(|item| (item.webhook, item.event_id.to_owned()))
        .unzip();
    let kept: HashMap<(Uuid, String), Vec<u8>> = if replayed_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query!(
            "SELECT k.provider_webhook_id, k.event_id, k.body_hash
               FROM provider_event_keys k
               JOIN unnest($1::uuid[], $2::text[]) AS r(provider_webhook_id, event_id)
                 ON k.provider_webhook_id = r.provider_webhook_id AND k.event_id = r.event_id",
            &replayed_webhooks,
            &replayed_ids,
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| ((row.provider_webhook_id, row.event_id), row.body_hash))
        .collect()
    };
    for item in &mut items {
        if item.fate.is_some() {
            continue;
        }
        let key = (item.webhook, item.event_id.to_owned());
        item.fate = Some(if inserted.contains(&key) {
            Fate::New
        } else if kept.get(&key) == Some(&item.hash) {
            Fate::Duplicate
        } else {
            Fate::Quarantined
        });
    }

    let stored: Vec<&Item<'_>> = items
        .iter()
        .filter(|item| matches!(item.fate, Some(Fate::New | Fate::Quarantined)))
        .collect();
    let new_ids: Vec<Uuid> = if stored.is_empty() {
        Vec::new()
    } else {
        let webhooks: Vec<Uuid> = stored.iter().map(|item| item.webhook).collect();
        let event_ids: Vec<String> = stored.iter().map(|item| item.event_id.to_owned()).collect();
        let hashes: Vec<Vec<u8>> = stored.iter().map(|item| item.hash.clone()).collect();
        let raws: Vec<Vec<u8>> = stored.iter().map(|item| item.raw.to_vec()).collect();
        let states: Vec<&str> = stored
            .iter()
            .map(|item| match item.fate {
                Some(Fate::Quarantined) => "quarantined",
                _ => "received",
            })
            .collect();
        let received: Vec<Timestamp> = stored.iter().map(|item| item.received_at).collect();
        sqlx::query_scalar!(
            r#"INSERT INTO webhook_receipts (workspace_id, provider_webhook_id, event_id, body_hash, raw, state, received_at)
               SELECT $1, * FROM unnest($2::uuid[], $3::text[], $4::bytea[], $5::bytea[], $6::text[], $7::timestamptz[])
               RETURNING CASE WHEN state = 'received' THEN id END AS "id""#,
            workspace.uuid(),
            &webhooks,
            &event_ids,
            &hashes,
            &raws,
            &states as _,
            &received as _,
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .flatten()
        .collect()
    };
    if !new_ids.is_empty() {
        jobs::enqueue(&mut tx, workspace, &Normalize { receipts: new_ids }, None).await?;
    }
    tx.commit().await?;

    let mut outcomes = vec![Stored::default(); arrivals.len()];
    for item in &items {
        let Some(outcome) = outcomes.get_mut(item.arrival) else {
            continue;
        };
        match item.fate {
            Some(Fate::New) => outcome.received += 1,
            Some(Fate::Duplicate) => outcome.duplicates += 1,
            Some(Fate::Quarantined) => outcome.quarantined += 1,
            None => {}
        }
    }
    if outcomes.iter().any(|outcome| outcome.received > 0) {
        jobs::wake(db, Queue::Receipts).await;
    }
    Ok(outcomes)
}

/// Stores what the spool holds into `db` until the process is asked to stop and the spool is
/// empty, through [`store`], so replays are recognised as on the request path; waits with the
/// drain backoff while the database refuses.
pub async fn drain(db: Database, spool: Spool<Arrival>, mut shutdown: Shutdown) {
    let mut failures: u32 = 0;
    loop {
        let stopping = shutdown.requested();
        let wait = match drain_once(&db, &spool).await {
            Ok(true) => {
                failures = 0;
                continue;
            }
            Ok(false) if stopping => return,
            Ok(false) => DRAIN_IDLE,
            Err(error) => {
                tracing::error!(error = %error, failures, "the ingress spool could not be drained");
                if stopping {
                    return;
                }
                let wait = retry::backoff(failures, &retry::DRAIN, jobs::draw());
                failures = failures.saturating_add(1);
                wait
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shutdown.wait() => {}
        }
    }
}

/// Why the drain stopped a pass.
#[derive(Debug, thiserror::Error)]
pub enum DrainError {
    #[error(transparent)]
    Spool(#[from] spool::SpoolError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One pass: the oldest spooled callbacks stored and acknowledged. True when it moved anything.
///
/// # Errors
///
/// The spool or the database failed; what was not acknowledged stays spooled.
pub(crate) async fn drain_once(db: &Database, spool: &Spool<Arrival>) -> Result<bool, DrainError> {
    let batch = spool.take(DRAIN_BATCH).await?;
    SPOOL_BYTES.record(spool.bytes(), &[]);
    if batch.records.is_empty() {
        return Ok(batch.rejected > 0);
    }
    for (seq, arrival) in batch.records {
        let started = Instant::now();
        let stored = store(db, arrival.workspace, &[&arrival]).await?;
        spool.ack(vec![seq]).await?;
        for stored in stored {
            report(&arrival, stored, false, started.elapsed());
        }
    }
    SPOOL_BYTES.record(spool.bytes(), &[]);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use norbelys_mail::webhooks::Receipt;
    use uuid::Uuid;

    use super::{Arrival, BATCH_BYTES, BATCH_MAX, Size};
    use crate::domain::ids::{Id, WorkspaceId};
    use crate::domain::senders::Provider;

    /// An arrival of `count` receipts whose bodies are `bytes` long each.
    fn arrival(count: usize, bytes: usize) -> Arrival {
        Arrival {
            workspace: WorkspaceId::trusted(Uuid::now_v7()),
            webhook: Id::from_uuid(Uuid::now_v7()),
            provider: Provider::Sendgrid,
            received_at: crate::process::now(),
            receipts: (0..count)
                .map(|index| Receipt {
                    event_id: index.to_string(),
                    raw: vec![b'x'; bytes],
                })
                .collect(),
        }
    }

    /// A micro-batch grows only while it stays within both of its bounds, 500 receipts and
    /// 1 MiB of bodies: a batch is one transaction holding its bodies in memory, so neither a
    /// burst of small callbacks nor a few large ones can make it unbounded. Reaching a bound
    /// exactly still fits.
    #[test]
    fn a_batch_stays_within_its_receipts_and_its_bytes() {
        let half = arrival(1, BATCH_BYTES / 2);
        let full = Size::of(&half).and(&half);
        assert!(full.fits(), "1 MiB of bodies fits");
        assert!(!full.and(&arrival(1, 1)).fits(), "one byte more does not");

        let receipts = Size::of(&arrival(BATCH_MAX, 1));
        assert!(receipts.fits(), "500 receipts fit");
        assert!(
            !receipts.and(&arrival(1, 1)).fits(),
            "one receipt more does not"
        );
    }
}
