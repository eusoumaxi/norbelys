//! `provider.reconcile`: reading the SendGrid and Mailgun events APIs, hourly, into receipts, for
//! the events their webhooks did not deliver.
//!
//! # Why
//!
//! - Mailgun retries a failed webhook for about eight hours, except its delivery notifications,
//!   which it never retries: a `delivered` event lost on the way (our ingress unreachable, a
//!   network fault) would be lost for good, though the local spool keeps every callback the
//!   ingress received.
//! - A submission whose answer was lost after the content went out leaves its message
//!   `uncertain`; for a relay, the provider's own later event about the message settles it
//!   (the normaliser applies it). SendGrid retries its webhook for a day, so its events are
//!   rarely lost, but an `uncertain` message whose event never came is looked up in its Email
//!   Activity.
//!
//! # How
//!
//! `provider.reconcile_due` ([`ProviderReconcileDue`], at the top of every hour) enqueues one
//! `provider.reconcile` per active provider webhook, read from the scheduler's view of the
//! webhooks (ids and status only). Each `provider.reconcile` ([`ProviderReconcile`]), inside its
//! workspace:
//!
//! 1. reads its webhook, its connection and the connection's credentials; a webhook of another
//!    provider (SES and the managed MTA name our message in their own events, which the
//!    normaliser settles), an archived connection, or one without an API credential is done at
//!    once;
//! 2. reads the provider's API outside any transaction, within [`BUDGET`]: Mailgun's recorded
//!    events of the connection's domain from two and a half hours ago to half an hour ago
//!    (Mailgun's guidance trusts only events older than about half an hour; hourly runs read each
//!    event twice, so a late or missed run loses nothing), or SendGrid's Email Activity of each
//!    message of the connection still `uncertain`, at most [`UNCERTAIN_PER_RUN`];
//! 3. drops the events whose key exists already: a webhook delivered them, or an earlier run
//!    read them. The API renders an event differently from the webhook, and the ingress keeps a
//!    known key with another body as a quarantined replay, which a reconciled event is not;
//! 4. stores the rest through the ingress's own path (`webhooks::ingress::store`): their
//!    provider event keys, the receipts and the `receipts.normalize` job, in one transaction, so
//!    the normaliser turns them into evidence as it does every callback, and two runs, or a run
//!    and a late webhook, store each key once.
//!
//! A refused API key ends the run quietly (logged): nothing changes until a person saves a new
//! one. Any other failure fails the run, retried on the runner's backoff; reading again is
//! harmless, since the keys make every store idempotent.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use norbelys_mail::http::ApiError;
use norbelys_mail::relays::{mailgun, sendgrid};
use norbelys_mail::webhooks::Receipt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use super::ingress::{self, Arrival};
use crate::crypto::Keys;
use crate::domain::ids::{Connection, Id, ProviderWebhook, WorkspaceId};
use crate::domain::senders::{Provider, Status};
use crate::jobs::{self, Class, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::senders::Env;
use crate::senders::connections::SmtpSettings;
use crate::senders::credentials::{self, ApiCredential};

/// How long one run's provider reads may take in all: well inside the job's lease.
pub const BUDGET: Duration = Duration::from_secs(45);
/// The `uncertain` messages of a SendGrid connection one run looks up; the rest wait an hour.
pub const UNCERTAIN_PER_RUN: i64 = 25;
/// Mailgun pages one run reads at most (300 events each).
const MAILGUN_PAGES: usize = 20;
/// How old Mailgun's events must be to be read: its guidance trusts only events older than about
/// half an hour, since some reach its storage after newer ones.
const MAILGUN_SETTLED: Duration = Duration::from_secs(30 * 60);
/// How far back each run reads Mailgun's events: two hours, so hourly runs overlap by one.
const MAILGUN_SPAN: Duration = Duration::from_secs(2 * 3_600);
/// Webhooks the fan-out enqueues per run; the rest wait for the next hour.
const DUE_PER_RUN: i64 = 10_000;

/// `provider.reconcile_due`: at the top of every hour, enqueues `provider.reconcile` for each
/// active provider webhook, read from the scheduler's view (ids and status only); it calls no
/// provider itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderReconcileDue {}

impl Job for ProviderReconcileDue {
    const KIND: &'static str = "provider.reconcile_due";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("0 * * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut directory = cx.directory().await?;
        let due = sqlx::query!(
            "SELECT workspace_id, id FROM provider_webhooks
              WHERE status = 'active'
              ORDER BY workspace_id, id LIMIT $1",
            DUE_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        let mut by_workspace: HashMap<Uuid, Vec<ProviderReconcile>> = HashMap::new();
        for row in due {
            by_workspace
                .entry(row.workspace_id)
                .or_default()
                .push(ProviderReconcile {
                    webhook: Id::from_uuid(row.id),
                });
        }
        let mut enqueued = cx
            .progress()
            .and_then(|progress| progress.get("enqueued"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        for (workspace, reconciles) in by_workspace {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let workspace = WorkspaceId::trusted(workspace);
            let mut chunk = cx.begin_in(workspace).await?;
            let added = jobs::enqueue_many(chunk.tx(), workspace, &reconciles, None).await?;
            enqueued = enqueued.saturating_add(added);
            cx.checkpoint(chunk, json!({ "enqueued": enqueued }))
                .await?;
        }
        if enqueued > 0 {
            jobs::wake(cx.db(), Queue::Maintenance).await;
        }
        Ok(Outcome::Done)
    }
}

/// `provider.reconcile`: reads one SendGrid or Mailgun webhook's events API into receipts (see
/// the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderReconcile {
    /// The provider webhook whose events are read.
    pub webhook: Id<ProviderWebhook>,
}

impl Job for ProviderReconcile {
    const KIND: &'static str = "provider.reconcile";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.webhook.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let keys = cx.env::<Keys>()?.clone();
        let env = cx.env::<Env>()?.clone();
        let Some(target) = target(cx, &keys, workspace, self.webhook).await? else {
            return Ok(Outcome::Done);
        };
        let deadline = Instant::now() + BUDGET;
        let read = match target.provider {
            Provider::Mailgun => read_mailgun(&env, &target, deadline).await,
            Provider::Sendgrid => read_sendgrid(cx, &env, workspace, &target, deadline).await?,
            Provider::Smtp
            | Provider::Google
            | Provider::Microsoft
            | Provider::Ses
            | Provider::Norbelys => return Ok(Outcome::Done),
        };
        let receipts = match read {
            Ok(receipts) => receipts,
            Err(ApiError::Unauthorized | ApiError::Forbidden { .. }) => {
                tracing::warn!(
                    webhook = %self.webhook,
                    provider = target.provider.as_str(),
                    "the provider refused the connection's API key; its events are read again once a new key is saved"
                );
                return Ok(Outcome::Done);
            }
            Err(error) => {
                return Err(JobError::Failed(format!(
                    "the {} events API could not be read: {error}",
                    target.provider.as_str()
                )));
            }
        };
        let fresh = unseen(cx, self.webhook, receipts).await?;
        if fresh.is_empty() {
            return Ok(Outcome::Done);
        }
        let arrival = Arrival {
            workspace,
            webhook: self.webhook,
            provider: target.provider,
            received_at: crate::process::now(),
            receipts: fresh,
        };
        let stored = ingress::store(cx.db(), workspace, &[&arrival]).await?;
        tracing::info!(
            webhook = %self.webhook,
            provider = target.provider.as_str(),
            received = stored.iter().map(|stored| stored.received).sum::<usize>(),
            "provider events reconciled"
        );
        Ok(Outcome::Done)
    }
}

/// What a run reads with: the webhook's provider, its connection, the connection's SMTP settings
/// (Mailgun's domain and region) and its API credential.
struct Target {
    provider: Provider,
    connection: Id<Connection>,
    smtp: SmtpSettings,
    api: ApiCredential,
}

/// Reads the webhook, its connection and the API credential sealed beside the connection's
/// password, inside the workspace; `None` when there is nothing to read: the webhook is gone or
/// disabled, its connection archived or without SMTP settings, or no API credential was given.
async fn target(
    cx: &JobContext,
    keys: &Keys,
    workspace: WorkspaceId,
    webhook: Id<ProviderWebhook>,
) -> Result<Option<Target>, JobError> {
    let mut tx = cx.db().begin_in(workspace).await?;
    let row = sqlx::query!(
        "SELECT w.provider, w.connection_id, c.status, c.smtp,
                connection_credential(c.workspace_id, c.id) AS credential
           FROM provider_webhooks w
           JOIN connections c ON c.workspace_id = w.workspace_id AND c.id = w.connection_id
          WHERE w.workspace_id = $1 AND w.id = $2 AND w.status = 'active'",
        workspace.uuid(),
        webhook.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (Ok(provider), Ok(status)) = (
        row.provider.parse::<Provider>(),
        row.status.parse::<Status>(),
    ) else {
        return Ok(None);
    };
    if status == Status::Archived || !matches!(provider, Provider::Sendgrid | Provider::Mailgun) {
        return Ok(None);
    }
    let connection = Id::from_uuid(row.connection_id);
    let Some(smtp) = row
        .smtp
        .and_then(|smtp| serde_json::from_value::<SmtpSettings>(smtp).ok())
    else {
        return Ok(None);
    };
    let Some(sealed) = row.credential else {
        return Ok(None);
    };
    let (_, api) = credentials::open_parts(keys, workspace, connection, &sealed)
        .map_err(|error| JobError::Failed(format!("the credential cannot be opened: {error}")))?;
    Ok(api.map(|api| Target {
        provider,
        connection,
        smtp,
        api,
    }))
}

/// Mailgun's recorded events of the connection's domain over the run's range (see the module).
async fn read_mailgun(
    env: &Env,
    target: &Target,
    deadline: Instant,
) -> Result<Vec<Receipt>, ApiError> {
    let Some(domain) = mailgun::domain_of(&target.smtp.username) else {
        return Err(ApiError::InvalidResponse(
            "the Mailgun SMTP login names no domain".to_owned(),
        ));
    };
    let end = crate::process::now().minus(MAILGUN_SETTLED);
    let begin = end.minus(MAILGUN_SPAN);
    let read = mailgun::events(
        &env.settings.http,
        &target.api.secret,
        mailgun::api_base(&target.smtp.host),
        &domain,
        (begin.0, end.0),
        MAILGUN_PAGES,
        deadline,
    )
    .await?;
    if !read.complete {
        tracing::warn!(
            connection = %target.connection,
            "Mailgun's events of the range exceed one run's pages; the rest is read by the next run's overlap"
        );
    }
    Ok(read.receipts)
}

/// SendGrid's Email Activity of the connection's `uncertain` messages, at most
/// [`UNCERTAIN_PER_RUN`] of them, oldest first, until the deadline.
async fn read_sendgrid(
    cx: &JobContext,
    env: &Env,
    workspace: WorkspaceId,
    target: &Target,
    deadline: Instant,
) -> Result<Result<Vec<Receipt>, ApiError>, JobError> {
    let mut tx = cx.db().begin_in(workspace).await?;
    let uncertain = sqlx::query_scalar!(
        "SELECT id FROM messages
          WHERE workspace_id = $1 AND connection_id = $2 AND state = 'uncertain'
          ORDER BY id LIMIT $3",
        workspace.uuid(),
        target.connection.uuid(),
        UNCERTAIN_PER_RUN,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut receipts = Vec::new();
    for message in uncertain {
        if Instant::now() >= deadline {
            break;
        }
        match sendgrid::activity(
            &env.settings.http,
            &target.api.secret,
            &target.smtp.host,
            message,
            deadline,
        )
        .await
        {
            Ok(found) => receipts.extend(found),
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(receipts))
}

/// The receipts among `receipts` whose key `webhook` has not stored yet, each key once.
async fn unseen(
    cx: &JobContext,
    webhook: Id<ProviderWebhook>,
    receipts: Vec<Receipt>,
) -> Result<Vec<Receipt>, JobError> {
    let ids: Vec<String> = receipts
        .iter()
        .map(|receipt| receipt.event_id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let stored: HashSet<String> = sqlx::query_scalar!(
        "SELECT event_id FROM provider_event_keys WHERE provider_webhook_id = $1 AND event_id = ANY($2)",
        webhook.uuid(),
        &ids,
    )
    .fetch_all(cx.db().pool())
    .await?
    .into_iter()
    .collect();
    let mut seen = HashSet::new();
    Ok(receipts
        .into_iter()
        .filter(|receipt| {
            !stored.contains(&receipt.event_id) && seen.insert(receipt.event_id.clone())
        })
        .collect())
}

#[cfg(test)]
mod tests;
