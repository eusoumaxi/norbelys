//! The dashboard surface: the handlers of sign-in, the account, workspaces, members,
//! invitations, API keys, SSO connections and the audit log.
//!
//! Every operation here is tagged `Dashboard` (the document marks it `x-surface: dashboard`, so
//! the public reference, the SDK, the CLI and the MCP catalogue leave it out), and each accepts
//! one form of a browser session only:
//!
//! - **anonymous**: `GET /v1/auth/config`, a read with nothing to forge, and, with the login-CSRF
//!   guard (an allowed `Origin` and the `X-CSRF-Token` header), `POST /v1/auth/challenges` for
//!   the sign-in methods and `POST /v1/auth/sessions` ([`auth`]);
//! - **the session cookie**, with the CSRF token on every change: `/v1/me` and its
//!   sub-resources, `POST /v1/auth/tokens`, a passkey registration or identity link challenge,
//!   and listing and creating workspaces ([`me`], [`team`]);
//! - **a workspace token** minted from the cookie (`nbs_`): everything under
//!   `/v1/workspaces/{id}` ([`team`], [`sso`]).
//!
//! An API key, a CLI token or an MCP token gets `403 session_required` on all of them, so no
//! credential a program holds can create another credential or a member.
//!
//! Effectful `POST`s take an `Idempotency-Key` (the middleware enforces it): in the workspace for
//! a workspace token, in the user's own namespace for the cookie operations. The `/v1/auth`
//! ceremonies carry their own single-use proofs instead.

mod auth;
mod me;
mod sso;
mod team;

use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use utoipa_axum::router::OpenApiRouter;

use super::authority::Principal;
use super::ceremonies;
use super::codes::CodeError;
use super::invitations::InvitationError;
use super::oidc::OidcError;
use super::passkeys::PasskeyError;
use super::sso::SsoError;
use super::workspaces::CreateWorkspaceError;
use crate::delivery::accept;
use crate::domain::ids::{Id, Workspace};
use crate::http::AppState;
use crate::problem::{Code, Problem};

/// The routes of the dashboard surface.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(auth::routes())
        .merge(me::routes())
        .merge(team::routes())
        .merge(sso::routes())
}

/// The checks of every `/v1/workspaces/{id}/…` operation: a workspace token (`403
/// session_required` for any other credential), for the workspace the path names (`404`
/// otherwise, as for an absent one).
fn dashboard(principal: &Principal, workspace: Id<Workspace>) -> Result<(), Problem> {
    principal.require_session()?;
    if workspace == principal.workspace.id() {
        Ok(())
    } else {
        Err(Problem::not_found("workspace"))
    }
}

/// The ceremony cookie of the request, if any.
fn ceremony_secret(headers: &HeaderMap) -> Option<String> {
    axum_extra::extract::CookieJar::from_headers(headers)
        .get(ceremonies::COOKIE)
        .map(|cookie| cookie.value().to_owned())
        .filter(|value| !value.is_empty())
}

/// Appends `cookies` to `response` as `Set-Cookie` headers.
fn set_cookies(response: &mut Response, cookies: &[String]) -> Result<(), Problem> {
    for cookie in cookies {
        let value =
            HeaderValue::from_str(cookie).map_err(|_| Problem::internal(&"an invalid cookie"))?;
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(())
}

/// The `Set-Cookie` value that removes the ceremony cookie.
const CLEAR_CEREMONY: &str =
    "__Host-nb_ceremony=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax";

/// The first memberships of a user, as `GET /v1/me` and a sign-in show them: the first 100, and
/// where the rest are listed.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct MembershipList {
    /// The first 100 active memberships, by workspace.
    #[schema(max_items = 100)]
    pub data: Vec<super::memberships::MembershipObject>,
    /// Whether there are more.
    pub has_more: bool,
    /// Where every workspace of the user is listed: `/v1/workspaces`.
    pub url: String,
}

/// The first memberships list of `user`, read under the user policy.
async fn first_memberships(
    tx: &mut crate::db::Tx,
    user: Id<crate::domain::ids::User>,
) -> Result<MembershipList, sqlx::Error> {
    const FIRST: i64 = 100;
    let mut data = super::memberships::of_user(tx, user, None, true, FIRST + 1).await?;
    let has_more = i64::try_from(data.len()).unwrap_or(i64::MAX) > FIRST;
    data.truncate(usize::try_from(FIRST).unwrap_or(100));
    Ok(MembershipList {
        data,
        has_more,
        url: "/v1/workspaces".to_owned(),
    })
}

/// Welcomes `user`, whom a sign-in has just created in `tx`, at `email`, the address that sign-in
/// proved (`users::welcome`), with the dashboard's primary origin for its button. Both finishes
/// that can create a user call it: the email code or link, and the provider callback. Answers
/// whether the welcome was queued, so the caller wakes the sender once `tx` has committed.
///
/// # Errors
///
/// No dashboard origin is configured (the api refuses to start without one), or the welcome
/// could not be accepted for a reason other than a missing transactional sender, which only skips
/// it.
pub(super) async fn welcome(
    app: &AppState,
    tx: &mut crate::db::Tx,
    user: Id<crate::domain::ids::User>,
    email: &crate::domain::email::EmailAddress,
) -> Result<bool, Problem> {
    let dashboard = app
        .identity
        .settings
        .dashboard()
        .ok_or_else(|| Problem::internal(&"no dashboard origin is configured"))?;
    super::users::welcome(tx, &app.keys, user, email, dashboard)
        .await
        .map_err(|error| Problem::from(CodeError::Mail(error)))
}

/// The one answer of a sign-in finish that proves nothing: the same whatever went wrong, so it
/// tells nothing about accounts or codes.
fn sign_in_refused() -> Problem {
    Problem::new(
        Code::Unauthorized,
        "The sign-in could not be completed; start again.",
    )
}

impl From<CodeError> for Problem {
    fn from(error: CodeError) -> Self {
        match error {
            CodeError::Db(error) => error.into(),
            CodeError::Crypto(error) => error.into(),
            CodeError::Ceremony(error) => Problem::internal(&error),
            CodeError::Mail(accept::Error::NoTransactionalSender) => {
                tracing::error!(
                    "no transactional sender is configured: sign-in codes and invitations cannot be sent"
                );
                Problem::unavailable(60)
            }
            CodeError::Mail(error) => Problem::internal(&error),
        }
    }
}

impl From<PasskeyError> for Problem {
    fn from(error: PasskeyError) -> Self {
        match error {
            PasskeyError::TooMany => {
                Problem::invalid_state("A user keeps at most 20 passkeys; delete one first.")
            }
            PasskeyError::Refused => sign_in_refused(),
            PasskeyError::Registered => Problem::conflict("This passkey is registered already."),
            PasskeyError::Library(reason) => Problem::internal(&reason),
            PasskeyError::Ceremony(error) => Problem::internal(&error),
            PasskeyError::Db(error) => error.into(),
        }
    }
}

impl From<OidcError> for Problem {
    fn from(error: OidcError) -> Self {
        match error {
            OidcError::Unconfigured => Problem::invalid_field(
                "/provider",
                "invalid",
                "This sign-in method is not offered.",
            ),
            OidcError::Discovery(reason) => {
                tracing::warn!(reason = %reason, "an identity provider could not be reached");
                Problem::unavailable(5)
            }
            OidcError::Ceremony(error) => Problem::internal(&error),
            OidcError::Db(error) => error.into(),
            OidcError::Crypto(error) => error.into(),
        }
    }
}

impl From<InvitationError> for Problem {
    fn from(error: InvitationError) -> Self {
        match error {
            InvitationError::AlreadyMember => {
                Problem::conflict("The address is already a member of the workspace.")
            }
            InvitationError::NotFound => Problem::not_found("invitation"),
            InvitationError::NotPending => Problem::invalid_state(
                "The invitation was accepted or revoked, or has expired; ask for a new one.",
            ),
            InvitationError::OtherAddress => Problem::forbidden(
                "The invitation is for another address: sign in with the invited address.",
            ),
            InvitationError::Db(error) => error.into(),
            InvitationError::Crypto(error) => error.into(),
        }
    }
}

impl From<SsoError> for Problem {
    fn from(error: SsoError) -> Self {
        match error {
            SsoError::DomainInUse(domain) => Problem::conflict(format!(
                "`{domain}` belongs to another SSO connection of this workspace."
            )),
            SsoError::Db(error) => error.into(),
            SsoError::Crypto(error) => error.into(),
        }
    }
}

impl From<CreateWorkspaceError> for Problem {
    fn from(error: CreateWorkspaceError) -> Self {
        match error {
            CreateWorkspaceError::Db(error) => error.into(),
            CreateWorkspaceError::Crypto(error) => error.into(),
        }
    }
}
