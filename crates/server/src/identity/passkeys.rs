//! Passkeys: WebAuthn credentials (<https://www.w3.org/TR/webauthn-3/>) through `webauthn-rs`.
//!
//! - **Registration** needs a signed-in session: `POST /v1/auth/challenges { method:
//!   "passkey_registration" }` starts a `passkey_registration` ceremony naming the user and
//!   answers the browser's creation options; `POST /v1/me/passkeys { challenge_id, credential,
//!   name }` finishes it from the same browser. The user's existing credentials are excluded, so
//!   one authenticator is not registered twice; a user keeps at most 20 passkeys; a credential id
//!   belongs to one passkey (`passkeys.credential_id` is unique), so a credential cannot be
//!   attached to two accounts.
//! - **Authentication** is discoverable: `POST /v1/auth/challenges { method: "passkey" }`
//!   starts a `passkey_authentication` ceremony with no user and no email, and the authenticator
//!   says whose credential it is (its user handle, our user's id). `POST /v1/auth/sessions {
//!   challenge_id, credential }` finishes it from the same browser: the credential is found by its
//!   id and user, the assertion verified, and the stored credential updated as the library's
//!   contract asks (its signature counter, its backup state).
//!
//! The library's ceremony states and credentials are stored as it defines them: a state sealed
//! in `auth_ceremonies` (never in a cookie), a `Passkey` as JSON in `passkeys.credential`. A
//! passkey ceremony is consumed by the finish whatever its outcome, so a failed assertion starts
//! again. User verification is required at registration and at authentication.
//!
//! The relying party is the dashboard's registrable domain (`WEBAUTHN_RP_ID`), and the
//! dashboard's origins are the only origins accepted.

use serde::Serialize;
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, CredentialID, DiscoverableAuthentication, DiscoverableKey, Passkey,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Webauthn, WebauthnBuilder,
};

use super::SettingsError;
use super::ceremonies::{self, CeremonyError, Kind, Started};
use crate::crypto::Keys;
use crate::db::Tx;
use crate::domain::ids::{Challenge, Id, Passkey as PasskeyId, User};
use crate::domain::time::Timestamp;

/// The most passkeys a user keeps.
pub const MAX_PER_USER: i64 = 20;
/// The relying party's name shown by authenticators.
const RP_NAME: &str = "Norbelys";

/// Why a passkey ceremony failed.
#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    /// The user has 20 passkeys already.
    #[error("a user keeps at most 20 passkeys")]
    TooMany,
    /// The browser's answer does not complete the ceremony (another browser, an expired or used
    /// ceremony, an assertion or attestation the library refuses).
    #[error("the passkey ceremony could not be completed")]
    Refused,
    /// The credential is registered already.
    #[error("this passkey is registered already")]
    Registered,
    /// The library could not start a ceremony.
    #[error("the passkey library failed: {0}")]
    Library(String),
    #[error(transparent)]
    Ceremony(#[from] CeremonyError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The relying party of the dashboard at `origins` under `rp_id`.
///
/// # Errors
///
/// `rp_id` is not the registrable domain of every origin.
pub fn relying_party(rp_id: &str, origins: &[Url]) -> Result<Webauthn, SettingsError> {
    let invalid = |detail: String| SettingsError::Invalid(detail);
    let first = origins
        .first()
        .ok_or_else(|| invalid("DASHBOARD_ORIGINS needs at least one origin".to_owned()))?;
    let mut builder = WebauthnBuilder::new(rp_id, first)
        .map_err(|error| {
            invalid(format!(
                "WEBAUTHN_RP_ID does not fit the dashboard: {error}"
            ))
        })?
        .rp_name(RP_NAME);
    for origin in origins.iter().skip(1) {
        builder = builder.append_allowed_origin(origin);
    }
    builder
        .build()
        .map_err(|error| invalid(format!("the passkey relying party is invalid: {error}")))
}

/// A passkey as `GET /v1/me` lists it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PasskeyObject {
    pub id: Id<PasskeyId>,
    /// The name its user gave it.
    pub name: String,
    pub created_at: Timestamp,
    /// When it last signed in.
    pub last_used_at: Option<Timestamp>,
}

/// `user`'s passkeys, newest first (at most 20 exist).
///
/// # Errors
///
/// The database failed.
pub async fn list(tx: &mut Tx, user: Id<User>) -> Result<Vec<PasskeyObject>, sqlx::Error> {
    sqlx::query_as!(
        PasskeyObject,
        r#"SELECT id AS "id: Id<PasskeyId>", name, created_at AS "created_at: Timestamp",
                  last_used_at AS "last_used_at: Timestamp"
             FROM passkeys WHERE user_id = $1 ORDER BY id DESC LIMIT $2"#,
        user.uuid(),
        MAX_PER_USER,
    )
    .fetch_all(&mut **tx)
    .await
}

/// Deletes `user`'s passkey `id`; true when one was deleted.
///
/// # Errors
///
/// The database failed.
pub async fn delete(tx: &mut Tx, user: Id<User>, id: Id<PasskeyId>) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query!(
        "DELETE FROM passkeys WHERE id = $1 AND user_id = $2",
        id.uuid(),
        user.uuid()
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(deleted > 0)
}

/// Starts registering a passkey for `user` (whose address is `email`, shown by authenticators):
/// the ceremony for the browser, and the creation options it passes to
/// `navigator.credentials.create`.
///
/// # Errors
///
/// [`PasskeyError::TooMany`], or the library or the database failed.
pub async fn start_registration(
    tx: &mut Tx,
    keys: &Keys,
    webauthn: &Webauthn,
    user: Id<User>,
    email: &str,
    display_name: &str,
) -> Result<(Started, CreationChallengeResponse), PasskeyError> {
    let existing = sqlx::query_scalar!(
        "SELECT credential_id FROM passkeys WHERE user_id = $1",
        user.uuid()
    )
    .fetch_all(&mut **tx)
    .await?;
    if i64::try_from(existing.len()).unwrap_or(i64::MAX) >= MAX_PER_USER {
        return Err(PasskeyError::TooMany);
    }
    let exclude: Vec<CredentialID> = existing.into_iter().map(CredentialID::from).collect();
    let (options, registration) = webauthn
        .start_passkey_registration(
            user.uuid(),
            email,
            display_name,
            (!exclude.is_empty()).then_some(exclude),
        )
        .map_err(|error| PasskeyError::Library(error.to_string()))?;
    let state = serde_json::to_value(&registration)
        .map_err(|error| PasskeyError::Library(error.to_string()))?;
    let started =
        ceremonies::start_for(tx, keys, Kind::PasskeyRegistration, Some(user), &state).await?;
    Ok((started, options))
}

/// A registration a browser finishes.
#[derive(Debug, Clone, Copy)]
pub struct Registration<'a> {
    /// The signed-in user.
    pub user: Id<User>,
    /// The ceremony.
    pub challenge: Id<Challenge>,
    /// The ceremony cookie of the browser.
    pub secret: &'a str,
    /// The authenticator's answer.
    pub credential: &'a RegisterPublicKeyCredential,
    /// The name its user gives it.
    pub name: &'a str,
}

/// Finishes `registration` from the browser holding its ceremony cookie.
///
/// # Errors
///
/// [`PasskeyError::Refused`] for anything that does not complete the ceremony,
/// [`PasskeyError::Registered`] for a credential registered already; the database.
pub async fn finish_registration(
    tx: &mut Tx,
    keys: &Keys,
    webauthn: &Webauthn,
    registration: &Registration<'_>,
) -> Result<PasskeyObject, PasskeyError> {
    let Registration {
        user,
        challenge,
        secret,
        credential,
        name,
    } = *registration;
    let consumed = ceremonies::consume(tx, keys, challenge, secret)
        .await?
        .filter(|consumed| {
            consumed.kind == Kind::PasskeyRegistration && consumed.user == Some(user)
        })
        .ok_or(PasskeyError::Refused)?;
    let state: PasskeyRegistration =
        serde_json::from_value(consumed.state).map_err(|_| PasskeyError::Refused)?;
    let passkey = webauthn
        .finish_passkey_registration(credential, &state)
        .map_err(|error| {
            tracing::info!(error = %error, "a passkey registration was refused");
            PasskeyError::Refused
        })?;
    let stored =
        serde_json::to_value(&passkey).map_err(|error| PasskeyError::Library(error.to_string()))?;
    let row = sqlx::query_as!(
        PasskeyObject,
        r#"INSERT INTO passkeys (user_id, credential_id, credential, name) VALUES ($1, $2, $3, $4)
           ON CONFLICT (credential_id) DO NOTHING
           RETURNING id AS "id: Id<PasskeyId>", name, created_at AS "created_at: Timestamp",
                     last_used_at AS "last_used_at: Timestamp""#,
        user.uuid(),
        passkey.cred_id().as_ref(),
        stored,
        name,
    )
    .fetch_optional(&mut **tx)
    .await?;
    row.ok_or(PasskeyError::Registered)
}

/// Starts a discoverable authentication: the ceremony for the browser, and the request options
/// it passes to `navigator.credentials.get`.
///
/// # Errors
///
/// The library or the database failed.
pub async fn start_authentication(
    tx: &mut Tx,
    keys: &Keys,
    webauthn: &Webauthn,
) -> Result<(Started, RequestChallengeResponse), PasskeyError> {
    let (options, authentication) = webauthn
        .start_discoverable_authentication()
        .map_err(|error| PasskeyError::Library(error.to_string()))?;
    let state = serde_json::to_value(&authentication)
        .map_err(|error| PasskeyError::Library(error.to_string()))?;
    let started =
        ceremonies::start_for(tx, keys, Kind::PasskeyAuthentication, None, &state).await?;
    Ok((started, options))
}

/// Finishes a discoverable authentication with `credential` from the browser holding `secret`:
/// the user it proves, or `None` when it proves nothing. The ceremony is consumed either way.
///
/// # Errors
///
/// The database failed.
pub async fn finish_authentication(
    tx: &mut Tx,
    keys: &Keys,
    webauthn: &Webauthn,
    challenge: Id<Challenge>,
    secret: &str,
    credential: &PublicKeyCredential,
) -> Result<Option<Id<User>>, PasskeyError> {
    let Some(consumed) = ceremonies::consume(tx, keys, challenge, secret)
        .await?
        .filter(|consumed| consumed.kind == Kind::PasskeyAuthentication)
    else {
        return Ok(None);
    };
    let Ok(state) = serde_json::from_value::<DiscoverableAuthentication>(consumed.state) else {
        return Ok(None);
    };
    let Ok((user, credential_id)) = webauthn.identify_discoverable_authentication(credential)
    else {
        return Ok(None);
    };
    let row = sqlx::query!(
        "SELECT id, credential FROM passkeys WHERE credential_id = $1 AND user_id = $2 FOR UPDATE",
        credential_id,
        user,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let Ok(mut passkey) = serde_json::from_value::<Passkey>(row.credential) else {
        return Ok(None);
    };
    let key = DiscoverableKey::from(&passkey);
    let Ok(result) = webauthn.finish_discoverable_authentication(credential, state, &[key]) else {
        return Ok(None);
    };
    let changed = passkey.update_credential(&result) == Some(true);
    let stored = if changed {
        serde_json::to_value(&passkey).ok()
    } else {
        None
    };
    sqlx::query!(
        "UPDATE passkeys SET last_used_at = now(), credential = coalesce($2, credential) WHERE id = $1",
        row.id,
        stored,
    )
    .execute(&mut **tx)
    .await?;
    Ok(Some(Id::from_uuid(user)))
}
