//! The authorization server's clients: registered clients, and MCP clients identified by a
//! Client ID Metadata Document.
//!
//! # Registered clients
//!
//! Rows of `oauth_clients` with `kind = 'registered'`: ours. The command-line client
//! `norbelys-cli` is the one the schema creates: public (it holds no secret, `auth_method =
//! 'none'`), without redirect URIs, and the only client allowed the device authorization grant.
//! A confidential registered client authenticates at the token and revocation endpoints with
//! HTTP Basic (`client_secret_basic`, RFC 6749 §2.3.1), its secret checked against the stored
//! keyed hash.
//!
//! # Client ID Metadata Documents
//!
//! An MCP client that has no registration with us names itself by an `https` URL: its
//! `client_id` is the address of a JSON document describing it
//! (<https://datatracker.ietf.org/doc/draft-ietf-oauth-client-id-metadata-document/>), which the
//! MCP specification recommends over dynamic registration. The document is fetched through the
//! bounded identity fetcher (5 seconds, 64 KiB, no redirects, no private addresses), and accepted
//! only when its own `client_id` equals the URL it was fetched from, it lists at least one
//! redirect URI, and it asks for no client authentication (a document is public, so such a client
//! can hold no secret). The accepted document is kept in `oauth_clients` (`kind = 'cimd'`) for the
//! lifetime its `Cache-Control: max-age` gives, at most 24 hours (none with `no-store` or
//! `no-cache`), and fetched again sooner when a request names a redirect URI the kept copy does not
//! list, since the client may have added one. Dynamic client registration is not offered.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Network attempts are globally bounded and deduplicated before DNS or HTTP work.
static FETCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(32);
static ATTEMPTS: LazyLock<Mutex<Attempts>> = LazyLock::new(Mutex::default);
const CLIENT_LIMIT: i64 = 10_000;
const ATTEMPT_LIMIT: usize = 1_024;
const FAILED_WAIT: Duration = Duration::from_secs(60);

/// A bounded reservation remains busy while its request runs and briefly after a failed read.
#[derive(Default)]
struct Attempts(HashMap<String, Option<Instant>>);
impl Attempts {
    fn reserve(&mut self, client_id: &str, now: Instant) -> bool {
        self.0.retain(|_, failed| {
            failed.is_none_or(|failed| now.saturating_duration_since(failed) < FAILED_WAIT)
        });
        if self.0.contains_key(client_id) || self.0.len() >= ATTEMPT_LIMIT {
            return false;
        }
        self.0.insert(client_id.to_owned(), None);
        true
    }

    fn finish(&mut self, client_id: &str, succeeded: bool, now: Instant) {
        if succeeded {
            self.0.remove(client_id);
        } else if let Some(failed) = self.0.get_mut(client_id) {
            *failed = Some(now);
        }
    }
}

struct Attempt {
    client_id: String,
    succeeded: bool,
}
impl Attempt {
    fn begin(client_id: &str) -> Result<Self, ClientError> {
        let mut attempts = ATTEMPTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !attempts.reserve(client_id, Instant::now()) {
            return Err(ClientError::Document(
                "metadata fetch is busy or temporarily refused".into(),
            ));
        }
        Ok(Self {
            client_id: client_id.to_owned(),
            succeeded: false,
        })
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        // Failure, panic and cancellation all get a short negative cache. A successful
        // attempt removes its entry while holding the same lock as failed attempts.
        let mut attempts = ATTEMPTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attempts.finish(&self.client_id, self.succeeded, Instant::now());
    }
}

use axum::http::{HeaderMap, Request, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use crate::crypto::Keys;
use crate::db::Database;
use crate::domain::oauth::{redirect_allowed, valid_redirect_uri};
use crate::domain::time::Timestamp;
use crate::identity::fetch::{FetchError, Fetcher};

/// The command-line client's id.
pub const CLI: &str = "norbelys-cli";
/// The longest a fetched metadata document is kept.
const MAX_CACHE_SECONDS: i64 = 24 * 3600;

/// How a client authenticates at the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// A public client: no secret.
    None,
    /// A confidential client: HTTP Basic with its secret.
    ClientSecretBasic,
}

/// A client of the authorization server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// Its id: a name for registered clients, the document's URL for metadata-document clients.
    pub client_id: String,
    /// Its display name, shown at consent.
    pub name: String,
    /// The redirect URIs it may use.
    pub redirect_uris: Vec<String>,
    /// How it authenticates.
    pub auth_method: AuthMethod,
    /// The keyed hash of its secret, for confidential clients.
    secret_hash: Option<Vec<u8>>,
    /// Whether it is described by a metadata document.
    cimd: bool,
}

impl Client {
    /// Whether this is the command-line client.
    #[must_use]
    pub fn is_cli(&self) -> bool {
        self.client_id == CLI && !self.cimd
    }
}

/// Why a client could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// The metadata document could not be fetched or is not acceptable.
    #[error("the client metadata document is not usable: {0}")]
    Document(String),
}

impl From<FetchError> for ClientError {
    fn from(error: FetchError) -> Self {
        Self::Document(error.to_string())
    }
}

/// Resolves `client_id`: a registered client, or a metadata-document client (fetched when it is
/// not kept, its copy is stale, or, with `redirect_uri`, the copy does not allow that redirect);
/// `None` for an unknown registered id.
///
/// # Errors
///
/// The database failed, or the metadata document cannot be fetched or is refused.
pub async fn find(
    db: &Database,
    fetcher: &Fetcher,
    client_id: &str,
    redirect_uri: Option<&str>,
) -> Result<Option<Client>, ClientError> {
    if client_id.len() > 2048 {
        return Err(ClientError::Document(
            "client identifier is too long".into(),
        ));
    }
    let row = sqlx::query!(
        r#"SELECT client_id, kind, name, redirect_uris, auth_method, secret_hash,
                  coalesce((metadata->>'cache_seconds')::bigint, 0) AS "cache_seconds!",
                  fetched_at AS "fetched_at: Timestamp"
             FROM oauth_clients WHERE client_id = $1"#,
        client_id
    )
    .fetch_optional(db.pool())
    .await?;
    let is_document = client_id.starts_with("https://");
    if let Some(row) = row {
        if row.redirect_uris.iter().any(|uri| !valid_redirect_uri(uri)) {
            return Err(ClientError::Document(
                "unsafe registered redirect URI".into(),
            ));
        }
        let client = Client {
            auth_method: if row.auth_method == "client_secret_basic" {
                AuthMethod::ClientSecretBasic
            } else {
                AuthMethod::None
            },
            cimd: row.kind == "cimd",
            client_id: row.client_id,
            name: row.name,
            redirect_uris: row.redirect_uris,
            secret_hash: row.secret_hash,
        };
        let fresh = row.fetched_at.is_some_and(|fetched| {
            fetched.plus(Duration::from_secs(
                u64::try_from(row.cache_seconds.clamp(0, MAX_CACHE_SECONDS)).unwrap_or(0),
            )) > crate::process::now()
        });
        let allows = redirect_uri.is_none_or(|uri| redirect_allowed(&client.redirect_uris, uri));
        if !client.cimd || (fresh && allows) {
            return Ok(Some(client));
        }
    } else if !is_document {
        return Ok(None);
    }
    fetch(db, fetcher, client_id).await.map(Some)
}

/// Fetches, checks and keeps the metadata document at `client_id`.
async fn fetch(db: &Database, fetcher: &Fetcher, client_id: &str) -> Result<Client, ClientError> {
    let _slot = FETCHES
        .try_acquire()
        .map_err(|_| ClientError::Document("metadata fetch capacity exhausted".into()))?;
    let mut attempt = Attempt::begin(client_id)?;
    let url = url::Url::parse(client_id).map_err(|_| ClientError::Document("not a URL".into()))?;
    let request = Request::get(url.as_str())
        .header(header::ACCEPT, "application/json")
        .body(Vec::new())
        .map_err(|_| ClientError::Document("not a URL".into()))?;
    let response = fetcher.execute(request).await?;
    if !response.status().is_success() {
        return Err(ClientError::Document(format!(
            "the document answered {}",
            response.status()
        )));
    }
    let document: Value = serde_json::from_slice(response.body())
        .map_err(|_| ClientError::Document("the document is not JSON".into()))?;
    let accepted = accept_document(client_id, &document).map_err(ClientError::Document)?;
    let cache_seconds = cache_seconds(response.headers());
    let mut tx = db.pool().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(734286292)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM oauth_clients c WHERE kind = 'cimd' AND fetched_at < now() - interval '1 day' AND NOT EXISTS (SELECT 1 FROM oauth_grants g WHERE g.client_id = c.client_id)")
        .execute(&mut *tx).await?;
    let full: bool = sqlx::query_scalar("SELECT (SELECT count(*) FROM oauth_clients WHERE kind = 'cimd') >= $1 AND NOT EXISTS(SELECT 1 FROM oauth_clients WHERE client_id = $2)")
        .bind(CLIENT_LIMIT).bind(client_id).fetch_one(&mut *tx).await?;
    if full {
        return Err(ClientError::Document(
            "metadata client capacity exhausted".into(),
        ));
    }
    sqlx::query!(
        "INSERT INTO oauth_clients (client_id, kind, name, redirect_uris, auth_method, metadata, fetched_at)
         VALUES ($1, 'cimd', $2, $3, 'none', $4, now())
         ON CONFLICT (client_id) DO UPDATE
            SET name = excluded.name, redirect_uris = excluded.redirect_uris,
                metadata = excluded.metadata, fetched_at = excluded.fetched_at
          WHERE oauth_clients.kind = 'cimd'",
        client_id,
        accepted.name,
        &accepted.redirect_uris,
        serde_json::json!({ "document": document, "cache_seconds": cache_seconds }),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    attempt.succeeded = true;
    Ok(accepted)
}

/// The client a metadata document fetched from `client_id` describes.
///
/// # Errors
///
/// Why the document is refused: its `client_id` is not the URL it came from, it lists no redirect
/// URI, or it asks for client authentication.
fn accept_document(client_id: &str, document: &Value) -> Result<Client, String> {
    if document.get("client_id").and_then(Value::as_str) != Some(client_id) {
        return Err("its `client_id` is not the URL it was fetched from".to_owned());
    }
    if document
        .get("redirect_uris")
        .and_then(Value::as_array)
        .is_some_and(|uris| uris.iter().any(|uri| !uri.is_string()))
    {
        return Err("redirect URIs must all be strings".to_owned());
    }
    let redirect_uris: Vec<String> = document
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|uris| {
            uris.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if redirect_uris.is_empty()
        || redirect_uris.len() > 10
        || redirect_uris.iter().any(|uri| !valid_redirect_uri(uri))
    {
        return Err("it must list 1 to 10 safe HTTPS or HTTP loopback redirect URIs".to_owned());
    }
    let method = document
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if method != "none" {
        return Err(format!(
            "it asks for `{method}`, but a public document can hold no secret"
        ));
    }
    let host = url::Url::parse(client_id)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    let name = document
        .get("client_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or(host, |name| name.chars().take(100).collect());
    Ok(Client {
        client_id: client_id.to_owned(),
        name,
        redirect_uris,
        auth_method: AuthMethod::None,
        secret_hash: None,
        cimd: true,
    })
}

/// How long an answer with `headers` may be kept: its `max-age`, at most 24 hours; nothing with
/// `no-store` or `no-cache`; 24 hours without a `Cache-Control`.
fn cache_seconds(headers: &HeaderMap) -> i64 {
    let Some(control) = headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
    else {
        return MAX_CACHE_SECONDS;
    };
    let mut seconds = MAX_CACHE_SECONDS;
    for directive in control.split(',').map(str::trim) {
        let directive = directive.to_ascii_lowercase();
        if directive == "no-store" || directive == "no-cache" {
            return 0;
        }
        if let Some(age) = directive.strip_prefix("max-age=") {
            seconds = age.trim_matches('"').parse::<i64>().unwrap_or(0);
        }
    }
    seconds.clamp(0, MAX_CACHE_SECONDS)
}

/// Why a client failed to authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unauthenticated {
    /// Whether it tried HTTP Basic (the answer then challenges for it).
    pub basic: bool,
}

/// Authenticates the client of a token or revocation request: the `client_id` of the form (or of
/// HTTP Basic) must name `client`, and a confidential client must present its secret with HTTP
/// Basic; a public client presents none.
///
/// # Errors
///
/// [`Unauthenticated`] for a missing, mismatched or wrong credential.
pub fn authenticate(
    client: &Client,
    keys: &Keys,
    headers: &HeaderMap,
    form_client_id: Option<&str>,
) -> Result<(), Unauthenticated> {
    let basic = basic_credentials(headers);
    match (client.auth_method, basic) {
        (AuthMethod::None, None) if form_client_id == Some(client.client_id.as_str()) => Ok(()),
        (AuthMethod::ClientSecretBasic, Some((id, secret)))
            if id == client.client_id
                && form_client_id.is_none_or(|form| form == id)
                && client.secret_hash.as_deref().is_some_and(|hash| {
                    aws_lc_rs::constant_time::verify_slices_are_equal(
                        hash,
                        &keys.hash_token(&secret),
                    )
                    .is_ok()
                }) =>
        {
            Ok(())
        }
        (_, basic) => Err(Unauthenticated {
            basic: basic.is_some(),
        }),
    }
}

/// The client id of a request: the form's `client_id`, or HTTP Basic's user name.
#[must_use]
pub fn client_id_of(headers: &HeaderMap, form_client_id: Option<&str>) -> Option<String> {
    form_client_id
        .map(str::to_owned)
        .or_else(|| basic_credentials(headers).map(|(id, _)| id))
}

/// HTTP Basic credentials, form-decoded as RFC 6749 §2.3.1 requires.
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    let decode = |part: &str| {
        url::form_urlencoded::parse(format!("v={part}").as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
    };
    Some((decode(id)?, decode(secret)?))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    const URL: &str = "https://client.example/oauth/metadata.json";

    /// Duplicate anonymous fetches and a spray cannot expand memory or occupy extra network
    /// permits. Successful reads release their slot; failed reads become reusable after a minute.
    #[test]
    fn metadata_attempts_are_bounded_deduplicated_and_recover_after_failure() {
        let now = Instant::now();
        let mut attempts = Attempts::default();
        for index in 0..ATTEMPT_LIMIT {
            assert!(attempts.reserve(&format!("https://client{index}.example/metadata.json"), now));
        }
        assert!(!attempts.reserve("https://overflow.example/metadata.json", now));
        let first = "https://client0.example/metadata.json";
        assert!(!attempts.reserve(first, now));
        attempts.finish(first, false, now);
        assert!(!attempts.reserve(first, now + Duration::from_secs(59)));
        assert!(attempts.reserve(first, now + FAILED_WAIT));
        attempts.finish(first, true, now);
        assert!(attempts.reserve("https://new.example/metadata.json", now));
        assert_eq!(attempts.0.len(), ATTEMPT_LIMIT);
    }

    /// An abandoned fetch retains a negative cache entry even if its future is cancelled.
    #[test]
    fn abandoned_metadata_attempts_are_temporarily_refused() {
        let client_id = format!("https://{}.example/metadata.json", uuid::Uuid::now_v7());
        let attempt = Attempt::begin(&client_id).unwrap();
        assert!(Attempt::begin(&client_id).is_err());
        drop(attempt);
        assert!(Attempt::begin(&client_id).is_err());
        ATTEMPTS.lock().unwrap().0.remove(&client_id);
    }

    /// A metadata document is accepted only when it names itself by the URL it came from, lists a
    /// redirect URI and asks for no secret, so a document cannot impersonate another client or
    /// claim a credential it cannot keep; its name falls back to its host.
    #[test]
    fn metadata_documents_must_describe_themselves() {
        let good = json!({ "client_id": URL, "client_name": "Example", "redirect_uris": ["http://127.0.0.1/cb"] });
        let client = accept_document(URL, &good).unwrap();
        assert_eq!(
            (
                client.name.as_str(),
                client.redirect_uris.len(),
                client.cimd
            ),
            ("Example", 1, true)
        );
        let unnamed = json!({ "client_id": URL, "redirect_uris": ["https://client.example/cb"] });
        assert_eq!(
            accept_document(URL, &unnamed).unwrap().name,
            "client.example"
        );
        for refused in [
            json!({ "client_id": "https://other.example/m.json", "redirect_uris": ["https://x/cb"] }),
            json!({ "client_id": URL, "redirect_uris": [] }),
            json!({ "client_id": URL, "redirect_uris": ["not a url"] }),
            json!({ "client_id": URL, "redirect_uris": ["https://x/cb"], "token_endpoint_auth_method": "private_key_jwt" }),
        ] {
            assert!(accept_document(URL, &refused).is_err(), "{refused}");
        }
    }

    /// A document is kept for its `max-age`, never more than a day, and not at all when the
    /// answer forbids caching, so a client's change reaches us when it says it may.
    #[test]
    fn documents_are_kept_as_their_cache_headers_say() {
        for (control, expected) in [
            (None, MAX_CACHE_SECONDS),
            (Some("public, max-age=600"), 600),
            (Some("max-age=999999"), MAX_CACHE_SECONDS),
            (Some("no-store"), 0),
            (Some("max-age=600, no-cache"), 0),
            (Some("max-age=nonsense"), 0),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(control) = control {
                headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(control));
            }
            assert_eq!(cache_seconds(&headers), expected, "{control:?}");
        }
    }

    /// A public client authenticates by naming itself and presenting no secret; a confidential
    /// one only with its own secret over HTTP Basic, so neither can pass for the other.
    #[test]
    fn clients_authenticate_by_their_method() {
        let keys = crate::testing::keys();
        let public = Client {
            client_id: CLI.to_owned(),
            name: "CLI".to_owned(),
            redirect_uris: Vec::new(),
            auth_method: AuthMethod::None,
            secret_hash: None,
            cimd: false,
        };
        let confidential = Client {
            client_id: "partner".to_owned(),
            auth_method: AuthMethod::ClientSecretBasic,
            secret_hash: Some(keys.hash_token("s3cret")),
            ..public.clone()
        };
        let basic = |id: &str, secret: &str| {
            let mut headers = HeaderMap::new();
            let value = format!("Basic {}", STANDARD.encode(format!("{id}:{secret}")));
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&value).unwrap(),
            );
            headers
        };
        let none = HeaderMap::new();
        assert!(authenticate(&public, &keys, &none, Some(CLI)).is_ok());
        assert!(authenticate(&public, &keys, &none, Some("other")).is_err());
        assert!(authenticate(&public, &keys, &none, None).is_err());
        assert_eq!(
            authenticate(&public, &keys, &basic(CLI, "x"), Some(CLI)),
            Err(Unauthenticated { basic: true })
        );
        assert!(authenticate(&confidential, &keys, &basic("partner", "s3cret"), None).is_ok());
        assert!(authenticate(&confidential, &keys, &basic("partner", "wrong"), None).is_err());
        assert!(authenticate(&confidential, &keys, &none, Some("partner")).is_err());
        assert_eq!(
            client_id_of(&basic("partner", "s3cret"), None).as_deref(),
            Some("partner")
        );
    }
}
