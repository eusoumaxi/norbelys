//! Webhook endpoints: the customer URLs subscribed to event types, their sealed signing
//! secrets, and their health.
//!
//! # Secrets
//!
//! A secret is `whsec_` followed by the base64 of 32 random bytes, as the Standard Webhooks
//! specification describes; the bytes key the HMAC. It is shown once, when the endpoint is
//! created or its secret rotated, and stored sealed (AES-256-GCM under the deployment key,
//! bound to the table, the workspace and the endpoint, so a sealed value copied to another row
//! does not open). The one sealed column holds a small JSON document: the current secret and,
//! for 24 hours after a rotation, the previous one with its expiry. While both are valid every
//! attempt carries both signatures, so a consumer can switch at its own pace. The worker reads
//! the sealed value only through `webhook_endpoint_secret()`, an accessor that answers inside
//! the endpoint's own workspace only.
//!
//! # URLs
//!
//! A URL is checked for shape when it is written: an absolute `http` or `https` URL with a
//! host, without credentials or fragment, at most 2,048 characters. Where it may point (public
//! addresses, `https` only) is checked by the worker at every attempt, where DNS can be checked
//! at connection time and the deployment's development switch is known.
//!
//! # Disabling
//!
//! An endpoint is disabled by a person (`manual`), by answering `410 Gone` (`gone`) or by
//! failing for 5 days without a success (`failing`). Disabling stops its pending deliveries
//! (`disabled`) and writes a `webhook_endpoint.disabled` event, which the relay delivers to the
//! endpoints that still work. Re-enabling clears the failure record; a replay then reopens what
//! the endpoint was owed.
//!
//! Lock order: the endpoint row, then its delivery rows, as recording an attempt also takes them.

use crate::domain::webhooks::Filters;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::EventType;
use super::outbox::{self, Event};
use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::domain::ids::{Id, WebhookEndpoint, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::http::versioning;

/// How long a rotated-out secret keeps signing.
const PREVIOUS_SECRET_HOURS: u64 = 24;
/// The longest URL an endpoint takes.
pub const URL_MAX: usize = 2_048;

/// Why an endpoint was disabled (`webhook_endpoints.disabled_reason`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = EndpointDisabledReason, rename_all = "snake_case")]
pub enum DisabledReason {
    /// The consumer answered `410 Gone`: it is no longer interested.
    Gone,
    /// Five days of failures without a success.
    Failing,
    /// A person disabled it.
    Manual,
}

impl DisabledReason {
    /// The reason as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Disables `endpoint` when it is enabled: stops its pending deliveries and records the
/// `webhook_endpoint.disabled` event. Returns false when it was already disabled (or absent).
///
/// # Errors
///
/// The database refused.
pub async fn disable(
    tx: &mut Tx,
    workspace: WorkspaceId,
    endpoint: Uuid,
    reason: DisabledReason,
    last_error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let disabled = sqlx::query_scalar!(
        "UPDATE webhook_endpoints SET enabled = false, disabled_reason = $3 WHERE workspace_id = $1 AND id = $2 AND enabled RETURNING id",
        workspace.uuid(),
        endpoint,
        reason.as_str(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if disabled.is_none() {
        return Ok(false);
    }
    sqlx::query!(
        "UPDATE webhook_deliveries SET state = 'disabled' WHERE workspace_id = $1 AND endpoint_id = $2 AND state = 'pending'",
        workspace.uuid(),
        endpoint,
    )
    .execute(&mut **tx)
    .await?;
    outbox::record(
        tx,
        workspace,
        Event {
            kind: EventType::WebhookEndpointDisabled,
            subject_type: "webhook_endpoint",
            subject_id: endpoint,
            data: json!({
                "webhook_endpoint_id": Id::<WebhookEndpoint>::from_uuid(endpoint),
                "disabled_reason": reason.as_str(),
                "last_error": last_error,
            }),
        },
    )
    .await?;
    Ok(true)
}

/// The signing secrets of an endpoint, as sealed in its row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secrets {
    /// The secret every attempt is signed with.
    current: String,
    /// Custom credentials, sealed with the signing keys and never returned.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The secret replaced by the last rotation, while it still signs.
    #[serde(default)]
    previous: Option<Previous>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Previous {
    secret: String,
    expires_at: Timestamp,
}

impl Secrets {
    /// A fresh secret.
    ///
    /// # Errors
    ///
    /// The system random source failed.
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self {
            current: new_secret()?,
            previous: None,
            headers: BTreeMap::new(),
        })
    }

    /// The current secret, to show once.
    #[must_use]
    pub fn current(&self) -> &str {
        &self.current
    }

    /// A new current secret; the old one keeps signing for 24 hours.
    ///
    /// # Errors
    ///
    /// The system random source failed.
    pub fn rotate(self, now: Timestamp) -> Result<Self, CryptoError> {
        Ok(Self {
            current: new_secret()?,
            headers: self.headers,
            previous: Some(Previous {
                secret: self.current,
                expires_at: now.plus(std::time::Duration::from_secs(
                    PREVIOUS_SECRET_HOURS * 3_600,
                )),
            }),
        })
    }

    /// The HMAC keys an attempt at `now` is signed with: the current secret's, then the previous
    /// one's while it has not expired.
    #[must_use]
    pub fn signing_keys(&self, now: Timestamp) -> Vec<Vec<u8>> {
        let previous = self
            .previous
            .as_ref()
            .filter(|previous| previous.expires_at > now)
            .map(|previous| previous.secret.as_str());
        std::iter::once(self.current.as_str())
            .chain(previous)
            .filter_map(super::deliver::secret_bytes)
            .collect()
    }
}

fn new_secret() -> Result<String, CryptoError> {
    Ok(format!(
        "whsec_{}",
        STANDARD.encode(crypto::random_bytes(32)?)
    ))
}

/// The context a sealed secret is bound to.
pub(crate) fn context(workspace: WorkspaceId, endpoint: Uuid) -> String {
    format!("webhook_endpoints.secret:{}:{endpoint}", workspace.uuid())
}

/// Why a sealed secret could not be used.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("the sealed secret is not a secrets document")]
    Shape,
}

/// Seals `secrets` for `endpoint` of `workspace`.
///
/// # Errors
///
/// The system random source failed.
pub fn seal_secrets(
    keys: &Keys,
    workspace: WorkspaceId,
    endpoint: Uuid,
    secrets: &Secrets,
) -> Result<Vec<u8>, SecretError> {
    let plaintext = serde_json::to_vec(secrets).map_err(|_| SecretError::Shape)?;
    Ok(keys.seal(&plaintext, context(workspace, endpoint).as_bytes())?)
}

/// Opens the sealed secrets of `endpoint` of `workspace`.
///
/// # Errors
///
/// The value was sealed under another key or context, or it is not a secrets document.
pub fn open_secrets(
    keys: &Keys,
    workspace: WorkspaceId,
    endpoint: Uuid,
    sealed: &[u8],
) -> Result<Secrets, SecretError> {
    let plaintext = keys.open(sealed, context(workspace, endpoint).as_bytes())?;
    serde_json::from_slice(&plaintext).map_err(|_| SecretError::Shape)
}

/// Checks the shape of an endpoint's URL; returns why it is refused.
///
/// # Errors
///
/// A reason fit for a `validation_failed` problem.
pub fn check_url(text: &str) -> Result<url::Url, &'static str> {
    if text.len() > URL_MAX {
        return Err("The URL is longer than 2,048 characters.");
    }
    let url = url::Url::parse(text).map_err(|_| "The URL is not a valid absolute URL.")?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err("The URL uses `https`.");
    }
    if url.host().is_none() {
        return Err("The URL names a host.");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The URL carries no credentials; authenticate with the signature.");
    }
    if url.fragment().is_some() {
        return Err("The URL has no fragment.");
    }
    Ok(url)
}

/// An endpoint as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EndpointObject {
    pub id: Id<WebhookEndpoint>,
    pub url: String,
    /// The event types the endpoint receives.
    #[schema(value_type = Vec<EventType>)]
    pub event_types: Vec<String>,
    /// Resource filters applied together with the event types.
    pub filters: Filters,
    /// Configured header names; their values are never returned.
    pub header_names: Vec<String>,
    /// False once disabled, by a person or by its failures.
    pub enabled: bool,
    /// Why it was disabled: it answered `410` (`gone`), it failed for 5 days without a success
    /// (`failing`), or a person disabled it (`manual`). Null while enabled. New values may be added.
    #[schema(value_type = Option<DisabledReason>)]
    pub disabled_reason: Option<String>,
    /// The first failed attempt since the last success.
    pub failing_since: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The endpoint's version, also the response's `ETag`: `updated_at` in microseconds since
    /// the Unix epoch. An update sent with it in `If-Match` applies only to this version, so a
    /// replaced list of event types never undoes a change made since it was read. A failure or
    /// a disabling by the worker moves it too.
    pub version: i64,
    /// The signing secret, shown only when the endpoint is created or its secret rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

struct EndpointRow {
    id: Id<WebhookEndpoint>,
    url: String,
    event_types: Vec<String>,
    enabled: bool,
    disabled_reason: Option<String>,
    failing_since: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl From<EndpointRow> for EndpointObject {
    fn from(row: EndpointRow) -> Self {
        Self {
            id: row.id,
            url: row.url,
            event_types: row.event_types,
            filters: Filters::default(),
            header_names: Vec::new(),
            enabled: row.enabled,
            disabled_reason: row.disabled_reason,
            failing_since: row.failing_since,
            created_at: row.created_at,
            updated_at: row.updated_at,
            version: versioning::of(row.updated_at),
            secret: None,
        }
    }
}

/// Locks an endpoint's row for an update and returns its version, for the update's `If-Match`
/// to be checked against before anything is written; `None` when the workspace has no such
/// endpoint. The endpoint's row comes before its deliveries' rows, as in every path.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<WebhookEndpoint>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM webhook_endpoints WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// Reads one endpoint of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<WebhookEndpoint>,
) -> Result<Option<EndpointObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        EndpointRow,
        r#"SELECT id AS "id: Id<WebhookEndpoint>", url, event_types, enabled, disabled_reason,
                  failing_since AS "failing_since: Timestamp", created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM webhook_endpoints WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let mut endpoint = row.map(EndpointObject::from);
    if let Some(endpoint) = &mut endpoint {
        enrich(tx, workspace, endpoint).await?;
    }
    Ok(endpoint)
}

/// One page of `workspace`'s endpoints by id, after `cursor` when given.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    enabled: Option<bool>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<EndpointObject>, sqlx::Error> {
    // A workspace has few endpoints: one statement serves both orders.
    let rows = sqlx::query_as!(
        EndpointRow,
        r#"SELECT id AS "id: Id<WebhookEndpoint>", url, event_types, enabled, disabled_reason,
                  failing_since AS "failing_since: Timestamp", created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM webhook_endpoints
            WHERE workspace_id = $1 AND ($2::boolean IS NULL OR enabled = $2)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN id > $3 ELSE id < $3 END)
            ORDER BY CASE WHEN $4 THEN id END, id DESC LIMIT $5"#,
        workspace.uuid(),
        enabled,
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut endpoints: Vec<_> = rows.into_iter().map(EndpointObject::from).collect();
    for endpoint in &mut endpoints {
        enrich(tx, workspace, endpoint).await?;
    }
    Ok(endpoints)
}

/// Counts `workspace`'s endpoints, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    enabled: Option<bool>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM webhook_endpoints WHERE workspace_id = $1 AND ($2::boolean IS NULL OR enabled = $2) LIMIT $3) counted"#,
        workspace.uuid(),
        enabled,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// A new endpoint.
pub struct NewEndpoint<'a> {
    pub url: &'a str,
    pub event_types: &'a [EventType],
    pub enabled: bool,
}

/// Creates an endpoint with a fresh secret; returns it with the secret, shown once.
///
/// # Errors
///
/// The random source failed, or the database refused the row.
pub async fn create(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    new: &NewEndpoint<'_>,
) -> Result<EndpointObject, EndpointError> {
    let id = Id::<WebhookEndpoint>::new();
    let secrets = Secrets::generate()?;
    let sealed = seal_secrets(keys, workspace, id.uuid(), &secrets)?;
    let types = type_names(new.event_types);
    let row = sqlx::query_as!(
        EndpointRow,
        r#"INSERT INTO webhook_endpoints (workspace_id, id, url, secret, event_types, enabled, disabled_reason)
           VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $6 THEN NULL ELSE 'manual' END)
           RETURNING id AS "id: Id<WebhookEndpoint>", url, event_types, enabled, disabled_reason,
                     failing_since AS "failing_since: Timestamp", created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp""#,
        workspace.uuid(),
        id.uuid(),
        new.url,
        sealed,
        &types,
        new.enabled,
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(EndpointObject {
        secret: Some(secrets.current().to_owned()),
        ..EndpointObject::from(row)
    })
}

/// The changes of an endpoint update; absent fields stay as they are.
pub struct EndpointChanges<'a> {
    pub url: Option<&'a str>,
    pub event_types: Option<&'a [EventType]>,
    pub enabled: Option<bool>,
}

/// Updates an endpoint. Disabling it stops its pending deliveries and records the
/// `webhook_endpoint.disabled` event; enabling it clears its failure record. Returns `None` when
/// it does not exist.
///
/// # Errors
///
/// The database refused.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<WebhookEndpoint>,
    changes: &EndpointChanges<'_>,
) -> Result<Option<EndpointObject>, sqlx::Error> {
    let types = changes.event_types.map(type_names);
    let updated = sqlx::query_scalar!(
        "UPDATE webhook_endpoints SET url = coalesce($3, url), event_types = coalesce($4, event_types)
          WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
        changes.url,
        types.as_deref(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        return Ok(None);
    }
    match changes.enabled {
        Some(false) => {
            disable(tx, workspace, id.uuid(), DisabledReason::Manual, None).await?;
        }
        Some(true) => {
            sqlx::query!(
                "UPDATE webhook_endpoints SET enabled = true, disabled_reason = NULL, failing_since = NULL, failure_notified_at = NULL
                  WHERE workspace_id = $1 AND id = $2 AND NOT enabled",
                workspace.uuid(),
                id.uuid(),
            )
            .execute(&mut **tx)
            .await?;
        }
        None => {}
    }
    read(tx, workspace, id).await
}

/// Deletes an endpoint; its deliveries go with it. Returns false when it does not exist.
///
/// # Errors
///
/// The database refused.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<WebhookEndpoint>,
) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query_scalar!(
        "DELETE FROM webhook_endpoints WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(deleted.is_some())
}

/// Rotates an endpoint's secret: the new one is returned once, the old one keeps signing for
/// 24 hours. Returns `None` when it does not exist.
///
/// # Errors
///
/// The sealed secret does not open, the random source failed, or the database refused.
pub async fn rotate_secret(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    id: Id<WebhookEndpoint>,
) -> Result<Option<EndpointObject>, EndpointError> {
    let sealed = sqlx::query_scalar!(
        "SELECT secret FROM webhook_endpoints WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(sealed) = sealed else {
        return Ok(None);
    };
    let secrets =
        open_secrets(keys, workspace, id.uuid(), &sealed)?.rotate(crate::process::now())?;
    let resealed = seal_secrets(keys, workspace, id.uuid(), &secrets)?;
    sqlx::query!(
        "UPDATE webhook_endpoints SET secret = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
        resealed,
    )
    .execute(&mut **tx)
    .await?;
    let endpoint = read(tx, workspace, id).await?;
    Ok(endpoint.map(|endpoint| EndpointObject {
        secret: Some(secrets.current().to_owned()),
        ..endpoint
    }))
}

/// Why an endpoint operation failed.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<CryptoError> for EndpointError {
    fn from(error: CryptoError) -> Self {
        Self::Secret(SecretError::Crypto(error))
    }
}

fn type_names(types: &[EventType]) -> Vec<String> {
    let mut names: Vec<String> = types.iter().map(|kind| kind.as_str().to_owned()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Loads public subscription configuration without opening any credential.
async fn enrich(
    tx: &mut Tx,
    workspace: WorkspaceId,
    endpoint: &mut EndpointObject,
) -> Result<(), sqlx::Error> {
    let (filters, names): (sqlx::types::Json<Filters>, Vec<String>) = sqlx::query_as(
        "SELECT filters, header_names FROM webhook_endpoints WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace.uuid())
    .bind(endpoint.id.uuid())
    .fetch_one(&mut **tx)
    .await?;
    endpoint.filters = filters.0;
    endpoint.header_names = names;
    Ok(())
}

/// Replaces the supplied subscription configuration under the endpoint's existing row lock.
/// Header values share the endpoint's sealed credential, so rotation preserves them.
///
/// # Errors
/// The database refuses the write or the credential cannot be opened or sealed.
pub async fn configure(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    id: Id<WebhookEndpoint>,
    filters: Option<&Filters>,
    headers: Option<&BTreeMap<String, String>>,
) -> Result<(), EndpointError> {
    if let Some(filters) = filters {
        sqlx::query("UPDATE webhook_endpoints SET filters=$3 WHERE workspace_id=$1 AND id=$2")
            .bind(workspace.uuid())
            .bind(id.uuid())
            .bind(sqlx::types::Json(filters))
            .execute(&mut **tx)
            .await?;
    }
    if let Some(headers) = headers {
        let sealed: Vec<u8> = sqlx::query_scalar(
            "SELECT secret FROM webhook_endpoints WHERE workspace_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(workspace.uuid())
        .bind(id.uuid())
        .fetch_one(&mut **tx)
        .await?;
        let mut secrets = open_secrets(keys, workspace, id.uuid(), &sealed)?;
        secrets.headers.clone_from(headers);
        let sealed = seal_secrets(keys, workspace, id.uuid(), &secrets)?;
        let names: Vec<_> = headers.keys().cloned().collect();
        sqlx::query("UPDATE webhook_endpoints SET secret=$3, header_names=$4 WHERE workspace_id=$1 AND id=$2")
            .bind(workspace.uuid()).bind(id.uuid()).bind(sealed).bind(names).execute(&mut **tx).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::{Secrets, check_url, open_secrets, seal_secrets};
    use crate::domain::ids::WorkspaceId;
    use crate::testing;
    use crate::webhooks::deliver::secret_bytes;

    /// An endpoint's URL is accepted only as an absolute `http` or `https` URL with a host,
    /// without credentials or a fragment, of at most 2,048 characters; where it points is judged
    /// by the worker at every attempt.
    #[test]
    fn endpoint_urls_are_checked_for_shape() {
        for good in [
            "https://hooks.example.com/norbelys",
            "http://127.0.0.1:9911/hooks?topic=a",
        ] {
            assert!(check_url(good).is_ok(), "{good}");
        }
        let long = format!("https://hooks.example.com/{}", "a".repeat(2_048));
        for bad in [
            "hooks.example.com/norbelys",
            "ftp://hooks.example.com/x",
            "mailto:hooks@example.com",
            "https://user:secret@hooks.example.com/x",
            "https://hooks.example.com/x#part",
            long.as_str(),
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    /// After a rotation the new and the previous secret both sign for 24 hours (the new one
    /// first), then only the new one: consumers switch at their own pace and the old secret dies
    /// on its own.
    #[test]
    fn a_rotated_secret_keeps_signing_for_a_day() {
        let now = crate::process::now();
        let original = Secrets::generate().unwrap();
        assert_eq!(original.signing_keys(now).len(), 1);
        let rotated = original.clone().rotate(now).unwrap();
        assert_ne!(rotated.current(), original.current());
        let during = rotated.signing_keys(now.plus(Duration::from_secs(23 * 3_600)));
        assert_eq!(during.len(), 2);
        assert_eq!(during[0], secret_bytes(rotated.current()).unwrap());
        assert_eq!(during[1], secret_bytes(original.current()).unwrap());
        assert_eq!(
            rotated
                .signing_keys(now.plus(Duration::from_secs(24 * 3_600 + 1)))
                .len(),
            1
        );
    }

    /// A sealed secret opens only for the workspace and endpoint it was sealed for, so a sealed
    /// value copied into another row is useless.
    #[test]
    fn a_sealed_secret_opens_only_for_its_endpoint() {
        let keys = testing::keys();
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        let endpoint = Uuid::now_v7();
        let secrets = Secrets::generate().unwrap();
        let sealed = seal_secrets(&keys, workspace, endpoint, &secrets).unwrap();
        assert_eq!(
            open_secrets(&keys, workspace, endpoint, &sealed)
                .unwrap()
                .current(),
            secrets.current()
        );
        assert!(open_secrets(&keys, workspace, Uuid::now_v7(), &sealed).is_err());
        assert!(
            open_secrets(
                &keys,
                WorkspaceId::trusted(Uuid::now_v7()),
                endpoint,
                &sealed
            )
            .is_err()
        );
    }
}
