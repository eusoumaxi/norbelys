//! `me`: the signed-in user's account. `GET /v1/me` shows the user with their sessions, passkeys,
//! OAuth grants, linked identities and first memberships, each bounded; `PATCH /v1/me` changes the
//! name and the language; the sub-resources are revoked or deleted one by one (`DELETE
//! /v1/me/sessions/current` signs out); `POST /v1/me/passkeys` finishes a passkey registration and
//! `POST /v1/me/memberships` accepts an invitation. All take the session cookie, with the CSRF
//! token on every change.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use webauthn_rs::prelude::RegisterPublicKeyCredential;

use super::{MembershipList, ceremony_secret, first_memberships, set_cookies};
use crate::domain::identity::{Lasting, may_make};
use crate::domain::ids::{Challenge, Grant, Id, IdentityLink, Passkey, Session, User, Workspace};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Path};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::audit::{self, Action, AuditActor};
use crate::identity::invitations;
use crate::identity::memberships::{self, MembershipObject};
use crate::identity::passkeys::{self, PasskeyObject, Registration};
use crate::identity::sessions::{self, CLEAR_COOKIE, SessionObject, SignedIn};
use crate::identity::users;
use crate::problem::{ApiResult, Problem};

/// The routes of `me`.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(retrieve, update))
        .routes(routes!(delete_session))
        .routes(routes!(create_passkey))
        .routes(routes!(delete_passkey))
        .routes(routes!(delete_grant))
        .routes(routes!(delete_identity))
        .routes(routes!(create_membership))
}

/// An OAuth grant of the user (an MCP client or the CLI acting for them in one workspace).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct GrantObject {
    pub id: Id<Grant>,
    /// The client: an `https` URL for an MCP client, or the CLI's id.
    pub client_id: String,
    /// The client's name, as shown at consent.
    pub client_name: String,
    /// The workspace it acts in.
    pub workspace_id: Id<Workspace>,
    /// The scopes it holds.
    pub scopes: Vec<String>,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    /// When it ends, whatever its use (90 days after consent).
    pub expires_at: Timestamp,
}

/// An external identity linked to the user.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct LinkedIdentityObject {
    pub id: Id<IdentityLink>,
    /// The OpenID Connect issuer.
    pub issuer: String,
    /// The address the provider gave, as last seen.
    pub email: Option<String>,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

/// The signed-in user's account.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MeObject {
    pub id: Id<User>,
    pub email: String,
    /// When the address was proven.
    pub email_verified_at: Option<Timestamp>,
    pub name: Option<String>,
    /// The preferred language (a BCP 47 tag).
    pub locale: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The version `If-Match` names.
    pub version: i64,
    /// The session of this request.
    pub session_id: Id<Session>,
    /// This session's CSRF token, for the `X-CSRF-Token` header of every change.
    pub csrf_token: String,
    /// The live sessions, newest first (at most 50).
    #[schema(max_items = 50)]
    pub sessions: Vec<SessionObject>,
    /// The passkeys (at most 20).
    #[schema(max_items = 20)]
    pub passkeys: Vec<PasskeyObject>,
    /// The live OAuth grants (at most 50).
    #[schema(max_items = 50)]
    pub grants: Vec<GrantObject>,
    /// The linked identities (at most 10).
    #[schema(max_items = 10)]
    pub identities: Vec<LinkedIdentityObject>,
    /// The first 100 memberships.
    pub memberships: MembershipList,
}

/// Reads the account of `signed_in`.
async fn me(app: &AppState, signed_in: &SignedIn) -> ApiResult<MeObject> {
    let user = signed_in.user;
    let mut tx = app.db.begin_as_user(user).await?;
    let found = users::read(&mut tx, user)
        .await?
        .ok_or_else(Problem::unauthorized)?;
    let sessions = sessions::list(&mut tx, user, signed_in.session).await?;
    let passkeys = passkeys::list(&mut tx, user).await?;
    let grants = sqlx::query_as!(
        GrantObject,
        r#"SELECT g.id AS "id: Id<Grant>", g.client_id, c.name AS client_name,
                  g.workspace_id AS "workspace_id: Id<Workspace>", g.scopes,
                  g.created_at AS "created_at: Timestamp", g.last_used_at AS "last_used_at: Timestamp",
                  g.expires_at AS "expires_at: Timestamp"
             FROM oauth_grants g JOIN oauth_clients c ON c.client_id = g.client_id
            WHERE g.user_id = $1 AND g.revoked_at IS NULL AND g.expires_at > now()
            ORDER BY g.id DESC LIMIT 50"#,
        user.uuid()
    )
    .fetch_all(&mut *tx)
    .await?;
    let identities = sqlx::query_as!(
        LinkedIdentityObject,
        r#"SELECT id AS "id: Id<IdentityLink>", issuer, email, created_at AS "created_at: Timestamp",
                  last_used_at AS "last_used_at: Timestamp"
             FROM identity_links WHERE user_id = $1 ORDER BY created_at DESC LIMIT 10"#,
        user.uuid()
    )
    .fetch_all(&mut *tx)
    .await?;
    let memberships = first_memberships(&mut tx, user).await?;
    tx.commit().await?;
    Ok(MeObject {
        id: found.id,
        email: found.email,
        email_verified_at: found.email_verified_at,
        name: found.name,
        locale: found.locale,
        created_at: found.created_at,
        updated_at: found.updated_at,
        version: found.version,
        session_id: signed_in.session,
        csrf_token: sessions::csrf_token(&app.keys, signed_in.session),
        sessions,
        passkeys,
        grants,
        identities,
        memberships,
    })
}

/// Retrieve the signed-in user.
///
/// With their live sessions, passkeys, OAuth grants, linked identities and first 100
/// memberships, and this session's CSRF token.
#[utoipa::path(
    get,
    path = "/me",
    tag = "Dashboard",
    operation_id = "me.retrieve",
    responses(
        (status = 200, description = "The account.", body = MeObject,
         headers(("ETag" = String, description = "The user's version, for `If-Match`."))),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "A bearer credential (`session_required`)."),
    ),
    security(("session" = []))
)]
async fn retrieve(State(app): State<AppState>, signed_in: SignedIn) -> ApiResult<Tagged<MeObject>> {
    let me = me(&app, &signed_in).await?;
    Ok(Tagged {
        version: me.version,
        body: me,
    })
}

/// The body of `PATCH /me`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateMe {
    /// The display name; an empty string clears it.
    #[garde(length(chars, max = 200))]
    name: Option<String>,
    /// The preferred language, a BCP 47 tag (`en`, `es-ES`).
    #[garde(
        length(min = 2, max = 35),
        pattern(r"^[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*$")
    )]
    locale: Option<String>,
}

/// Update the signed-in user.
///
/// Their display name or preferred language.
#[utoipa::path(
    patch,
    path = "/me",
    tag = "Dashboard",
    operation_id = "me.update",
    params(IfMatch),
    request_body(content = UpdateMe, example = json!({"name": "Ada Lovelace", "locale": "en"})),
    responses(
        (status = 200, description = "The account.", body = MeObject,
         headers(("ETag" = String, description = "The user's new version."))),
        (status = 400, description = "`If-Match` is malformed."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current; nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("session" = []))
)]
async fn update(
    State(app): State<AppState>,
    signed_in: SignedIn,
    if_match: IfMatch,
    Json(body): Json<UpdateMe>,
) -> ApiResult<Tagged<MeObject>> {
    let mut tx = app.db.begin_as_user(signed_in.user).await?;
    let current = users::lock_version(&mut tx, signed_in.user)
        .await?
        .ok_or_else(Problem::unauthorized)?;
    if_match.check(current)?;
    users::update(
        &mut tx,
        signed_in.user,
        &users::Changes {
            name: body.name.as_deref(),
            locale: body.locale.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    let me = me(&app, &signed_in).await?;
    Ok(Tagged {
        version: me.version,
        body: me,
    })
}

/// A session named in a path: its id, or `current` for the session of the request.
#[derive(Debug, Clone, Copy)]
enum SessionRef {
    Current,
    Id(Id<Session>),
}

impl<'de> Deserialize<'de> for SessionRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "current" {
            Ok(Self::Current)
        } else {
            value
                .parse()
                .map(Self::Id)
                .map_err(serde::de::Error::custom)
        }
    }
}

impl utoipa::PartialSchema for SessionRef {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Type};
        OneOfBuilder::new()
            .item(<Id<Session> as utoipa::PartialSchema>::schema())
            .item(
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .enum_values(Some(["current"])),
            )
            .into()
    }
}

impl utoipa::ToSchema for SessionRef {}

/// Revoke a session of the signed-in user.
///
/// `current` signs this browser out and removes its cookie; another session's id ends that
/// session, on whatever device it is.
#[utoipa::path(
    delete,
    path = "/me/sessions/{id}",
    tag = "Dashboard",
    operation_id = "sessions.delete",
    params(("id" = inline(SessionRef), Path, description = "The session id (`ses_…`), or `current`.")),
    responses(
        (status = 204, description = "The session is revoked."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 404, description = "No such live session of this user."),
    ),
    security(("session" = []))
)]
async fn delete_session(
    State(app): State<AppState>,
    signed_in: SignedIn,
    client: crate::http::ratelimit::ClientAddress,
    Path(id): Path<SessionRef>,
) -> ApiResult<Response> {
    let (session, reason) = match id {
        SessionRef::Current => (signed_in.session, "signed_out"),
        SessionRef::Id(id) => (
            id,
            if id == signed_in.session {
                "signed_out"
            } else {
                "revoked"
            },
        ),
    };
    let mut tx = app.db.begin().await?;
    let ip_hash = app.keys.hash_address(&client.as_key());
    let revoked =
        sessions::revoke(&mut tx, signed_in.user, session, reason, Some(&ip_hash)).await?;
    tx.commit().await?;
    if !revoked {
        return Err(Problem::not_found("session"));
    }
    app.authority.forget_session(session).await;
    let mut response = StatusCode::NO_CONTENT.into_response();
    if session == signed_in.session {
        set_cookies(&mut response, &[CLEAR_COOKIE.to_owned()])?;
    }
    Ok(response)
}

/// The body of `POST /me/passkeys`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreatePasskey {
    /// The `passkey_registration` challenge, from this browser.
    #[garde(skip)]
    #[schema(value_type = String, example = "chl_0190f8a2b4c87a10b6d2e4f6a8c0e2f4")]
    challenge_id: Id<Challenge>,
    /// The authenticator's answer (`RegisterPublicKeyCredential` as JSON).
    #[garde(skip)]
    #[schema(value_type = Object)]
    credential: serde_json::Value,
    /// A name to recognise it by.
    #[garde(length(chars, min = 1, max = 100))]
    name: String,
}

/// Register a passkey.
///
/// Finishes the `passkey_registration` challenge this browser started.
#[utoipa::path(
    post,
    path = "/me/passkeys",
    tag = "Dashboard",
    operation_id = "passkeys.create",
    request_body(content = CreatePasskey, example = json!({"challenge_id": "chl_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "credential": {"id": "…", "rawId": "…", "type": "public-key", "response": {"attestationObject": "…", "clientDataJSON": "…"}}, "name": "MacBook"})),
    responses(
        (status = 201, description = "The passkey.", body = PasskeyObject),
        (status = 401, description = "No signed-in session, or the registration could not be completed."),
        (status = 403, description = "No CSRF token or dashboard origin, a bearer credential (`session_required`), or an operator's impersonation session."),
        (status = 409, description = "The passkey is registered already (`conflict`), or the user has 20 (`invalid_state`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("session" = []))
)]
async fn create_passkey(
    State(app): State<AppState>,
    signed_in: SignedIn,
    headers: HeaderMap,
    Json(body): Json<CreatePasskey>,
) -> ApiResult<Response> {
    // Checked again at the finish: the session that started the registration may have changed.
    if !may_make(signed_in.row.method, Lasting::Passkey) {
        return Err(Problem::forbidden(
            "An impersonation session cannot register a passkey in the person's name.",
        ));
    }
    let credential: RegisterPublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| {
            Problem::invalid_field(
                "/credential",
                "format",
                "The credential is not a WebAuthn registration (`RegisterPublicKeyCredential`).",
            )
        })?;
    let secret = ceremony_secret(&headers).ok_or_else(super::sign_in_refused)?;
    let mut tx = app.db.begin().await?;
    let created = passkeys::finish_registration(
        &mut tx,
        &app.keys,
        &app.identity.webauthn,
        &Registration {
            user: signed_in.user,
            challenge: body.challenge_id,
            secret: &secret,
            credential: &credential,
            name: &body.name,
        },
    )
    .await;
    // The ceremony is spent whatever the outcome.
    tx.commit().await?;
    let mut response = (StatusCode::CREATED, axum::Json(created?)).into_response();
    set_cookies(&mut response, &[super::CLEAR_CEREMONY.to_owned()])?;
    Ok(response)
}

/// Delete a passkey of the signed-in user.
///
/// It can no longer sign in.
#[utoipa::path(
    delete,
    path = "/me/passkeys/{id}",
    tag = "Dashboard",
    operation_id = "passkeys.delete",
    params(("id" = Id<Passkey>, Path, description = "The passkey id (`pky_…`).")),
    responses(
        (status = 204, description = "The passkey is deleted."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 404, description = "No such passkey of this user."),
    ),
    security(("session" = []))
)]
async fn delete_passkey(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Path(id): Path<Id<Passkey>>,
) -> ApiResult<StatusCode> {
    let mut tx = app.db.begin().await?;
    let deleted = passkeys::delete(&mut tx, signed_in.user, id).await?;
    tx.commit().await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("passkey"))
    }
}

/// Revoke an OAuth grant of the signed-in user.
///
/// The client's tokens stop working within a minute and its refresh tokens at once.
#[utoipa::path(
    delete,
    path = "/me/grants/{id}",
    tag = "Dashboard",
    operation_id = "grants.delete",
    params(("id" = Id<Grant>, Path, description = "The grant id (`grt_…`).")),
    responses(
        (status = 204, description = "The grant is revoked."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 404, description = "No such live grant of this user."),
    ),
    security(("session" = []))
)]
async fn delete_grant(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Path(id): Path<Id<Grant>>,
) -> ApiResult<StatusCode> {
    let mut tx = app.db.begin().await?;
    let workspace = crate::identity::oauth::grants::revoke(&mut tx, id, Some(signed_in.user))
        .await?
        .ok_or_else(|| Problem::not_found("grant"))?;
    crate::db::set_workspace(&mut tx, workspace).await?;
    audit::record(
        &mut tx,
        workspace,
        AuditActor::User(signed_in.user),
        Action::GrantRevoked,
        Some(id.to_string()),
        serde_json::json!({}),
        None,
    )
    .await?;
    tx.commit().await?;
    app.authority.forget_grant(id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Unlink an external identity from the signed-in user.
///
/// It can no longer sign them in; signing in through it again creates nothing unless it is linked
/// again.
#[utoipa::path(
    delete,
    path = "/me/identities/{id}",
    tag = "Dashboard",
    operation_id = "identities.delete",
    params(("id" = Id<IdentityLink>, Path, description = "The linked identity's id (`idn_…`).")),
    responses(
        (status = 204, description = "The identity is unlinked."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 404, description = "No such identity of this user."),
    ),
    security(("session" = []))
)]
async fn delete_identity(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Path(id): Path<Id<IdentityLink>>,
) -> ApiResult<StatusCode> {
    let mut tx = app.db.begin().await?;
    let deleted = sqlx::query!(
        "DELETE FROM identity_links WHERE id = $1 AND user_id = $2",
        id.uuid(),
        signed_in.user.uuid(),
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if deleted > 0 {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("identity"))
    }
}

/// The body of `POST /me/memberships`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateMembership {
    /// The token of the invitation's link.
    #[garde(length(min = 1, max = 128))]
    invitation_token: String,
}

/// Accept an invitation.
///
/// The signed-in user's verified address must be the invited one. Creates the membership (or
/// revives a removed one) with the invited role; accepting twice changes nothing more.
#[utoipa::path(
    post,
    path = "/me/memberships",
    tag = "Dashboard",
    operation_id = "memberships.create",
    request_body(content = CreateMembership, example = json!({"invitation_token": "Zm9vYmFyYmF6…"})),
    responses(
        (status = 201, description = "The membership.", body = MembershipObject),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "The invitation is for another address, no CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 404, description = "No such invitation."),
        (status = 409, description = "The invitation was accepted or revoked, or has expired (`invalid_state`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("session" = []))
)]
async fn create_membership(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Json(body): Json<CreateMembership>,
) -> ApiResult<Response> {
    let mut tx = app.db.begin_as_user(signed_in.user).await?;
    let found = invitations::find(&mut tx, &app.keys, &body.invitation_token)
        .await?
        .ok_or_else(|| Problem::not_found("invitation"))?;
    let email_key = sqlx::query_scalar!(
        "SELECT email_key FROM users WHERE id = $1 AND email_verified_at IS NOT NULL",
        signed_in.user.uuid()
    )
    .fetch_optional(&mut *tx)
    .await?;
    crate::db::set_workspace(&mut tx, found.workspace).await?;
    let accepted =
        invitations::accept(&mut tx, found, signed_in.user, email_key.as_deref()).await?;
    if accepted.joined {
        audit::record(
            &mut tx,
            found.workspace,
            AuditActor::User(signed_in.user),
            Action::InvitationAccepted,
            Some(found.id.to_string()),
            serde_json::json!({ "membership": accepted.membership.to_string() }),
            None,
        )
        .await?;
    }
    let membership = memberships::membership(&mut tx, found.workspace, accepted.membership)
        .await?
        .ok_or_else(|| Problem::not_found("membership"))?;
    tx.commit().await?;
    app.authority.forget_member(found.workspace, signed_in.user);
    Ok((StatusCode::CREATED, axum::Json(membership)).into_response())
}
