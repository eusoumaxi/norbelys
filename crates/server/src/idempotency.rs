//! Idempotency of effectful requests: a client may retry a request whose response it never
//! received, and the effect happens once.
//!
//! The model follows Stripe's and the IETF draft's
//! (<https://datatracker.ietf.org/doc/draft-ietf-httpapi-idempotency-key-header/>).
//! The middleware runs after authentication (so the key's namespace is known) and before
//! validation (so a `422` is stored like any other terminal answer). `Idempotency-Key` is
//! required on every effectful `POST` under `/v1` (the `/v1/auth` ceremonies carry their own
//! single-use tokens), optional on `PATCH`, ignored otherwise; `POST /v1/preflight` checks
//! addresses and stores nothing, so it is not an effectful request and takes no key. The key's
//! namespace is the credential's workspace, or, for the session operations a signed-in browser
//! makes before it holds any workspace token (creating a workspace, accepting an invitation,
//! registering a passkey), the session's user; its fingerprint is the method, the path with its
//! query and the SHA-256 of the body, which is buffered up to 16 MiB for an uploaded file (an
//! import's CSV file as `text/csv`, an image as `image/png`, `image/jpeg`, `image/gif` or
//! `image/webp`) and up to 1.5 MiB otherwise. The first terminal `2xx` or `4xx`
//! response is stored for 24 hours and replayed; a
//! `429` or `5xx` is never stored, so a retry re-executes. The same key with another
//! fingerprint is `422 idempotency_mismatch`; while the first request runs it is
//! `409 idempotency_in_progress`. A lock older than 60 seconds belonged to a process that
//! died, and is taken over.

use axum::body::{Body, to_bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::crypto;
use crate::db::Database;
use crate::db::Tx;
use crate::domain::ids::{Id, User, WorkspaceId};
use crate::identity::authority::Principal;
use crate::identity::sessions::SignedIn;
use crate::problem::{Code, Problem};

const HEADER: HeaderName = HeaderName::from_static("idempotency-key");
const REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");
/// The largest body an idempotent request may carry (a campaign with its steps).
const BODY_LIMIT: usize = 3 << 19;
/// The largest uploaded file: an import's CSV file, an image.
const UPLOAD_LIMIT: usize = 16 << 20;
/// Headers stored with a response and replayed with it: a replay answers what the first request
/// did, so a created resource's `ETag` comes back with the `version` its body carries.
const KEPT_HEADERS: [&str; 5] = [
    "content-type",
    "location",
    "etag",
    "ratelimit",
    "ratelimit-policy",
];

enum Claim {
    Owned(Uuid),
    Replay(Response),
}

/// Whose keys a request's key is among: the credential's workspace, or a signed-in user's own.
#[derive(Debug, Clone, Copy)]
enum Namespace {
    Workspace(WorkspaceId),
    User(Id<User>),
}

/// A transaction that sees the namespace's keys (the workspace policy, or the user policy).
async fn begin(db: &Database, namespace: Namespace) -> Result<Tx, sqlx::Error> {
    match namespace {
        Namespace::Workspace(workspace) => db.begin_in(workspace).await,
        Namespace::User(user) => db.begin_as_user(user).await,
    }
}

/// Whether this full API path and method support a caller-stable key (required for POST,
/// optional for PATCH). The middleware, API
/// document and derived clients share this rule so a retry never becomes a second effect.
#[must_use]
pub fn takes_key(method: &str, path: &str) -> bool {
    matches!(method, "POST" | "PATCH") && !path.starts_with("/v1/auth/") && path != "/v1/preflight"
}

/// Middleware: enforces and applies idempotency for the authenticated request.
pub async fn layer(State(db): State<Database>, request: Request, next: Next) -> Response {
    // Inside `/v1` the router strips the prefix from the request's URI; the original keeps it.
    let path = request
        .extensions()
        .get::<OriginalUri>()
        .map_or(request.uri().path(), |original| original.path())
        .to_owned();
    let namespace = match (
        request.extensions().get::<Principal>(),
        request.extensions().get::<SignedIn>(),
    ) {
        (Some(principal), _) => Namespace::Workspace(principal.workspace),
        (None, Some(signed_in)) => Namespace::User(signed_in.user),
        (None, None) => return next.run(request).await,
    };
    if !takes_key(request.method().as_str(), &path) {
        return next.run(request).await;
    }
    let is_upload = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let essence = value.split(';').next().unwrap_or_default().trim();
            essence.eq_ignore_ascii_case("text/csv")
                || crate::domain::images::Format::from_media_type(essence).is_some()
        });
    let key = match request.headers().get(&HEADER).map(HeaderValue::to_str) {
        None if request.method() == Method::PATCH => return next.run(request).await,
        None => {
            return Problem::bad_request("Effectful requests need an `Idempotency-Key` header.")
                .into_response();
        }
        Some(Ok(key))
            if !key.is_empty() && key.len() <= 128 && key.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            key.to_owned()
        }
        Some(_) => {
            return Problem::bad_request("`Idempotency-Key` is 1 to 128 visible ASCII characters.")
                .into_response();
        }
    };

    let (parts, body) = request.into_parts();
    let limit = if is_upload { UPLOAD_LIMIT } else { BODY_LIMIT };
    let Ok(bytes) = to_bytes(body, limit).await else {
        return Problem::new(
            Code::PayloadTooLarge,
            "The body is larger than the operation accepts.",
        )
        .into_response();
    };
    let target = parts
        .uri
        .path_and_query()
        .map_or(parts.uri.path(), |target| target.as_str());
    let mut material = format!("{} {target}\n", parts.method).into_bytes();
    material.extend_from_slice(&bytes);
    let fingerprint = crypto::sha256(&material);

    let claim = match claim(&db, namespace, &key, &fingerprint).await {
        Ok(claim) => claim,
        Err(problem) => return problem.into_response(),
    };
    let row = match claim {
        Claim::Replay(response) => return response,
        Claim::Owned(row) => row,
    };

    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    let status = response.status();
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        release(&db, namespace, row).await;
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(body) = to_bytes(body, usize::MAX).await else {
        release(&db, namespace, row).await;
        return Problem::internal(&"a response body could not be buffered").into_response();
    };
    store(&db, namespace, row, status, &parts.headers, &body).await;
    Response::from_parts(parts, Body::from(body))
}

async fn claim(
    db: &Database,
    namespace: Namespace,
    key: &str,
    fingerprint: &[u8],
) -> Result<Claim, Problem> {
    let (workspace, user) = match namespace {
        Namespace::Workspace(workspace) => (Some(workspace.uuid()), None),
        Namespace::User(user) => (None, Some(user.uuid())),
    };
    let mut tx = begin(db, namespace).await?;
    // One insert per namespace: each has its own unique index, and a conflict names one arbiter.
    let inserted = match namespace {
        Namespace::Workspace(_) => {
            sqlx::query_scalar!(
            "INSERT INTO idempotency_keys (workspace_id, key, fingerprint, locked_at, expires_at)
                 VALUES ($1, $2, $3, now(), now() + interval '24 hours')
                 ON CONFLICT (workspace_id, key) WHERE workspace_id IS NOT NULL DO NOTHING
                 RETURNING id",
            workspace,
            key,
            fingerprint
        )
            .fetch_optional(&mut *tx)
            .await?
        }
        Namespace::User(_) => {
            sqlx::query_scalar!(
                "INSERT INTO idempotency_keys (user_id, key, fingerprint, locked_at, expires_at)
                 VALUES ($1, $2, $3, now(), now() + interval '24 hours')
                 ON CONFLICT (user_id, key) WHERE user_id IS NOT NULL DO NOTHING
                 RETURNING id",
                user,
                key,
                fingerprint
            )
            .fetch_optional(&mut *tx)
            .await?
        }
    };
    if let Some(id) = inserted {
        tx.commit().await?;
        return Ok(Claim::Owned(id));
    }
    let row = sqlx::query!(
        r#"SELECT id, fingerprint, response_status, response_headers, response_body,
                  locked_at IS NOT NULL AND locked_at < now() - interval '60 seconds' AS "stale!",
                  expires_at <= now() AS "expired!"
             FROM idempotency_keys WHERE key = $3 AND (workspace_id = $1 OR user_id = $2) FOR UPDATE"#,
        workspace,
        user,
        key
    )
    .fetch_one(&mut *tx)
    .await?;
    if row.expired || (row.response_status.is_none() && row.stale) {
        sqlx::query!(
            "UPDATE idempotency_keys SET fingerprint = $2, locked_at = now(), response_status = NULL, response_headers = NULL,
                    response_body = NULL, expires_at = now() + interval '24 hours'
              WHERE id = $1",
            row.id,
            fingerprint
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Claim::Owned(row.id));
    }
    tx.commit().await?;
    if row.fingerprint != fingerprint {
        return Err(Problem::new(
            Code::IdempotencyMismatch,
            "This `Idempotency-Key` was used with a different request.",
        ));
    }
    let Some(status) = row.response_status else {
        return Err(Problem {
            retry_after: Some(1),
            ..Problem::new(
                Code::IdempotencyInProgress,
                "The first request with this key is still running.",
            )
        });
    };
    Ok(Claim::Replay(replay(
        status,
        row.response_headers,
        row.response_body,
    )))
}

fn replay(
    status: i16,
    headers: Option<serde_json::Value>,
    body: Option<serde_json::Value>,
) -> Response {
    let status = u16::try_from(status)
        .ok()
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(StatusCode::OK);
    let bytes = match body {
        Some(serde_json::Value::Null) | None => Vec::new(),
        Some(value) => serde_json::to_vec(&value).unwrap_or_default(),
    };
    let mut response = (status, bytes).into_response();
    if let Some(serde_json::Value::Object(stored)) = headers {
        for (name, value) in stored {
            if let (Ok(name), Some(Ok(value))) = (
                HeaderName::try_from(name.as_str()),
                value.as_str().map(HeaderValue::from_str),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
    }
    response
        .headers_mut()
        .insert(REPLAYED, HeaderValue::from_static("true"));
    response
}

async fn store(
    db: &Database,
    namespace: Namespace,
    row: Uuid,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) {
    let kept: serde_json::Map<String, serde_json::Value> = KEPT_HEADERS
        .iter()
        .filter_map(|name| {
            headers
                .get(*name)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    (
                        (*name).to_owned(),
                        serde_json::Value::String(value.to_owned()),
                    )
                })
        })
        .collect();
    let json_body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
    };
    let result = async {
        let mut tx = begin(db, namespace).await?;
        sqlx::query!(
            "UPDATE idempotency_keys SET response_status = $2, response_headers = $3, response_body = $4, locked_at = NULL WHERE id = $1",
            row,
            i16::try_from(status.as_u16()).unwrap_or(500),
            serde_json::Value::Object(kept),
            json_body
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, "idempotent response not stored");
    }
}

async fn release(db: &Database, namespace: Namespace, row: Uuid) {
    let result = async {
        let mut tx = begin(db, namespace).await?;
        sqlx::query!("DELETE FROM idempotency_keys WHERE id = $1", row)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, "idempotency lock not released");
    }
}
