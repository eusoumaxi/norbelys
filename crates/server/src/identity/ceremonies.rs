//! Ceremonies: the server-side state of a multi-step flow that leaves for a provider and comes
//! back, and the one `GET /v1/auth/callback` every redirect-based ceremony returns to.
//!
//! # Design
//!
//! A ceremony's state (the OpenID Connect `nonce`, the PKCE verifier, the return path, what the
//! flow is for) lives in `auth_ceremonies`, sealed with the deployment key and bound to the row,
//! never in a cookie: two api replicas can finish one ceremony, and nothing the browser holds can
//! be edited into another flow. The provider's `state` parameter is the ceremony's id.
//!
//! The browser that starts a ceremony receives `__Host-nb_ceremony`, a random secret whose SHA-256
//! is stored on the row (`HttpOnly`, `Secure`, `SameSite=Lax` so the provider's top-level redirect
//! back carries it, no `Domain`, path `/`). The callback must present it: a valid callback URL
//! carried to another browser cannot finish the ceremony, which is what stops an attacker from
//! making a victim's browser complete the attacker's consent. A ceremony is consumed once, by an
//! update that checks the cookie's hash, that it is unconsumed and that its 10 minutes have not
//! passed, so a wrong cookie consumes nothing and a replayed callback finds nothing.
//!
//! The callback dispatches on the ceremony's kind, the purpose it was started for; each kind's
//! module re-checks at the finish what the start assumed. The redirect kinds finish there:
//! `mailbox_oauth` by [`crate::senders::oauth::finish`], and `oidc`, `sso` and `identity_link` by
//! [`crate::identity::oidc::finish`]. The other kinds (an email code, a passkey's registration or
//! authentication) finish at their own API operation, and the callback refuses them without
//! consuming them.

use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::domain::ids::{Challenge, Id, User};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::Query;
use crate::problem::Problem;

/// The cookie that binds a ceremony to the browser that started it.
pub const COOKIE: &str = "__Host-nb_ceremony";
/// How long a ceremony may take.
const LIFETIME: Duration = Duration::from_secs(600);

/// What a ceremony is for: the purpose the callback dispatches on (`auth_ceremonies.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Kind {
    /// An email-code sign-in: finished by `POST /v1/auth/sessions` with the code from the same
    /// browser, or with the link from any browser.
    EmailCode,
    /// Registering a passkey for a signed-in user: finished by `POST /v1/me/passkeys`.
    PasskeyRegistration,
    /// A password-less, email-less sign-in with a discoverable passkey: finished by
    /// `POST /v1/auth/sessions`.
    PasskeyAuthentication,
    /// A sign-in through an OpenID Connect provider offered to everyone (Google).
    Oidc,
    /// A sign-in through a workspace's SSO connection, routed by email domain.
    Sso,
    /// Linking an external identity to a signed-in user.
    IdentityLink,
    /// Connecting a mailbox through OAuth, or reconnecting one whose grant was lost.
    MailboxOauth,
}

impl Kind {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why a ceremony could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum CeremonyError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("the ceremony's state is not a JSON document")]
    Shape,
}

/// A started ceremony.
#[derive(Debug, Clone)]
pub struct Started {
    /// Its id: the provider's `state`.
    pub id: Id<Challenge>,
    /// The `Set-Cookie` value that binds it to the browser.
    pub cookie: String,
    /// When it expires.
    pub expires_at: Timestamp,
}

/// A consumed ceremony.
#[derive(Debug, Clone)]
pub struct Consumed {
    /// What it was for.
    pub kind: Kind,
    /// The signed-in user who started it, for the kinds a user starts.
    pub user: Option<Id<User>>,
    /// Its opened state.
    pub state: serde_json::Value,
}

/// The associated data a ceremony's state is sealed with: its table and id. `admin secrets
/// rotate` re-seals with it.
pub(crate) fn context(id: Id<Challenge>) -> String {
    format!("auth_ceremonies.state:{}", id.uuid())
}

/// Starts a ceremony of `kind` for `user` with `state`, sealed; returns its id and the cookie
/// for the browser.
///
/// # Errors
///
/// The random source failed or the database refused the row.
pub async fn start(
    tx: &mut Tx,
    keys: &Keys,
    kind: Kind,
    user: Id<User>,
    state: &serde_json::Value,
) -> Result<Started, CeremonyError> {
    start_for(tx, keys, kind, Some(user), state).await
}

/// Starts a ceremony of `kind` with `state`, sealed, for `user` when a signed-in user starts it
/// and for nobody yet when it is a sign-in; returns its id and the cookie for the browser.
///
/// # Errors
///
/// The random source failed, or the database refused the row (a kind a signed-in user starts
/// needs `user`).
pub async fn start_for(
    tx: &mut Tx,
    keys: &Keys,
    kind: Kind,
    user: Option<Id<User>>,
    state: &serde_json::Value,
) -> Result<Started, CeremonyError> {
    let id = Id::<Challenge>::new();
    let secret = crypto::random_token(32)?;
    let plaintext = serde_json::to_vec(state).map_err(|_| CeremonyError::Shape)?;
    let sealed = keys.seal(&plaintext, context(id).as_bytes())?;
    let expires_at = sqlx::query_scalar!(
        r#"INSERT INTO auth_ceremonies (id, user_id, kind, browser_hash, state, expires_at)
           VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))
           RETURNING expires_at AS "expires_at: Timestamp""#,
        id.uuid(),
        user.map(|user| user.uuid()),
        kind.as_str(),
        crypto::sha256(secret.as_bytes()),
        sealed,
        LIFETIME.as_secs_f64(),
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(Started {
        id,
        cookie: format!(
            "{COOKIE}={secret}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
            LIFETIME.as_secs()
        ),
        expires_at,
    })
}

/// Consumes ceremony `id` for the browser holding `secret`: only an unconsumed, unexpired
/// ceremony whose stored hash matches the browser's cookie; `None` otherwise, and then nothing
/// is consumed.
///
/// # Errors
///
/// The database failed, or the state does not open.
pub async fn consume(
    tx: &mut Tx,
    keys: &Keys,
    id: Id<Challenge>,
    secret: &str,
) -> Result<Option<Consumed>, CeremonyError> {
    let row = sqlx::query!(
        r#"UPDATE auth_ceremonies SET consumed_at = now()
            WHERE id = $1 AND consumed_at IS NULL AND expires_at > now() AND browser_hash = $2
           RETURNING kind, user_id AS "user_id: Id<User>", state"#,
        id.uuid(),
        crypto::sha256(secret.as_bytes()),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind = row.kind.parse::<Kind>().map_err(|_| CeremonyError::Shape)?;
    let plaintext = keys.open(&row.state, context(id).as_bytes())?;
    let state = serde_json::from_slice(&plaintext).map_err(|_| CeremonyError::Shape)?;
    Ok(Some(Consumed {
        kind,
        user: row.user_id,
        state,
    }))
}

/// Opens ceremony `id` without consuming it and locks its row until the transaction ends: a
/// finish that may fail and be tried again (a mistyped code) reads the state, decides, and consumes
/// the ceremony with [`consume_opened`] only on success. With `secret`, only the browser holding it
/// opens the ceremony; without, any browser does, which only the email code's link may use (it
/// works on any device by design, behind a page that names the account). `None` for a ceremony
/// that is consumed, expired, unknown or another browser's.
///
/// # Errors
///
/// The database failed, or the state does not open.
pub async fn open(
    tx: &mut Tx,
    keys: &Keys,
    id: Id<Challenge>,
    secret: Option<&str>,
) -> Result<Option<Consumed>, CeremonyError> {
    let row = sqlx::query!(
        r#"SELECT kind, user_id AS "user_id: Id<User>", state, browser_hash FROM auth_ceremonies
            WHERE id = $1 AND consumed_at IS NULL AND expires_at > now()
              FOR UPDATE"#,
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if let Some(secret) = secret
        && aws_lc_rs::constant_time::verify_slices_are_equal(
            &row.browser_hash,
            &crypto::sha256(secret.as_bytes()),
        )
        .is_err()
    {
        return Ok(None);
    }
    let kind = row.kind.parse::<Kind>().map_err(|_| CeremonyError::Shape)?;
    let plaintext = keys.open(&row.state, context(id).as_bytes())?;
    let state = serde_json::from_slice(&plaintext).map_err(|_| CeremonyError::Shape)?;
    Ok(Some(Consumed {
        kind,
        user: row.user_id,
        state,
    }))
}

/// Consumes a ceremony this transaction opened with [`open`], so it can never finish again.
///
/// # Errors
///
/// The database failed.
pub async fn consume_opened(tx: &mut Tx, id: Id<Challenge>) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE auth_ceremonies SET consumed_at = now() WHERE id = $1 AND consumed_at IS NULL",
        id.uuid()
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The routes of the ceremonies: the one callback.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(callback))
}

/// What a provider sends back to the callback.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CallbackQuery {
    /// The ceremony's id, as the start sent it.
    pub state: Option<String>,
    /// The authorization code.
    pub code: Option<String>,
    /// The provider's error code, when the consent was not given.
    pub error: Option<String>,
    /// The provider's explanation of the error.
    pub error_description: Option<String>,
}

/// Finish a redirect-based ceremony.
///
/// The one return of every redirect-based ceremony. It finds the ceremony by `state`, requires the
/// ceremony cookie of the browser that started it, consumes the ceremony and dispatches on its
/// kind, then sends the browser on (`303`) to the ceremony's return path with the outcome.
#[utoipa::path(
    get,
    path = "/auth/callback",
    tag = "Dashboard",
    operation_id = "auth.callback",
    params(
        ("state" = Option<String>, Query, description = "The ceremony (`chl_…`)."),
        ("code" = Option<String>, Query, description = "The provider's authorization code."),
        ("error" = Option<String>, Query, description = "The provider's error code."),
        ("error_description" = Option<String>, Query, description = "The provider's explanation."),
    ),
    responses(
        (status = 303, description = "On to the ceremony's return path, with its outcome in the query."),
        (status = 400, description = "No ceremony this browser started is waiting for this callback."),
    ),
    security(())
)]
async fn callback(
    State(app): State<AppState>,
    jar: CookieJar,
    client: crate::http::ratelimit::ClientAddress,
    request_headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, Problem> {
    let refused = || {
        Problem::bad_request(
            "No ceremony started in this browser is waiting for this callback; start again.",
        )
    };
    let id: Id<Challenge> = query
        .state
        .as_deref()
        .and_then(|state| state.parse().ok())
        .ok_or_else(refused)?;
    let secret = jar.get(COOKIE).ok_or_else(refused)?.value().to_owned();
    // Only a redirect ceremony is consumed here: an email code or a passkey ceremony brought to the
    // callback is refused and left as it was.
    let mut tx = app.db.begin().await?;
    let opened = open(&mut tx, &app.keys, id, Some(&secret))
        .await
        .map_err(|error| Problem::internal(&error))?
        .filter(|opened| {
            matches!(
                opened.kind,
                Kind::MailboxOauth | Kind::Oidc | Kind::Sso | Kind::IdentityLink
            )
        });
    if opened.is_some() {
        consume_opened(&mut tx, id).await?;
    }
    tx.commit().await?;
    let consumed = opened.ok_or_else(refused)?;
    let (location, cookies) = match consumed.kind {
        Kind::MailboxOauth => (
            crate::senders::oauth::finish(&app, consumed.user, consumed.state, &query).await?,
            Vec::new(),
        ),
        Kind::Oidc | Kind::Sso | Kind::IdentityLink => {
            let finished =
                crate::identity::oidc::finish(&app, consumed, &query, &request_headers, client)
                    .await?;
            (finished.location, finished.cookies)
        }
        Kind::EmailCode | Kind::PasskeyRegistration | Kind::PasskeyAuthentication => {
            return Err(refused());
        }
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&location).map_err(|_| Problem::internal(&"an invalid location"))?,
    );
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-nb_ceremony=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax",
        ),
    );
    for cookie in cookies {
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).map_err(|_| Problem::internal(&"an invalid cookie"))?,
        );
    }
    Ok(response)
}
