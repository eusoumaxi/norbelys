//! Users: the people who sign in.
//!
//! There is no separate sign-up: the first successful sign-in with an email code, or the first
//! OpenID Connect sign-in whose provider vouches for an address no user holds, creates the user.
//! A user is keyed by the comparison key of their email address (`users.email_key`, ASCII
//! lowercase), so one address is one person however it is typed. An email code proves the
//! inbox, so a user created or found by one has `email_verified_at` set.
//!
//! A suspended user (an operator's decision) cannot sign in; their existing sessions are
//! revoked with the suspension.
//!
//! # The welcome
//!
//! The sign-in that creates a user also welcomes them by email ([`welcome`]): the platform's
//! transactional mail, accepted in the sign-in's own transaction, so a sign-in that does not
//! commit welcomes nobody and a person is welcomed once, by whichever sign-in created them
//! (an email code or link, OpenID Connect, a workspace's single sign-on). A platform without a
//! transactional sender skips the welcome and still signs the person in: the welcome helps, the
//! sign-in is what they came for.

use std::time::Duration;

use serde::Serialize;
use url::Url;

use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::accept::{self, Transactional};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, User};
use crate::domain::time::Timestamp;
use crate::http::versioning;

/// How long a welcome stays worth delivering: three days. It greets a person who has just
/// signed up and points at their first steps; held back longer (the transactional relay paused
/// or failing over a weekend), it would arrive after they found their way and tell them nothing.
/// Once first submitted, the sender's retry window bounds its tries like any message's.
const WELCOME_USEFUL_FOR: Duration = Duration::from_secs(3 * 24 * 3_600);

/// A user as `GET /v1/me` shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UserObject {
    pub id: Id<User>,
    /// The address, as first written.
    pub email: String,
    /// When the address was proven (a code or a link reached it, or a provider vouched for it).
    pub email_verified_at: Option<Timestamp>,
    /// The display name.
    pub name: Option<String>,
    /// The preferred language of mail and the dashboard (a BCP 47 tag, `en` by default).
    pub locale: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The version `If-Match` names (the row's `updated_at` in microseconds).
    pub version: i64,
}

/// A user's identity and standing, as sign-in needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// The user.
    pub id: Id<User>,
    /// Their address.
    pub email: String,
    /// False when an operator suspended them.
    pub active: bool,
}

/// The user who holds `email`, if any.
///
/// # Errors
///
/// The database failed.
pub async fn by_email(tx: &mut Tx, email: &EmailAddress) -> Result<Option<Standing>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id AS "id: Id<User>", email, status FROM users WHERE email_key = ascii_lower($1)"#,
        email.as_str()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| Standing {
        id: row.id,
        email: row.email,
        active: row.status == "active",
    }))
}

/// The standing of `user`.
///
/// # Errors
///
/// The database failed.
pub async fn standing(tx: &mut Tx, user: Id<User>) -> Result<Option<Standing>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id AS "id: Id<User>", email, status FROM users WHERE id = $1"#,
        user.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| Standing {
        id: row.id,
        email: row.email,
        active: row.status == "active",
    }))
}

/// The user who holds `email`, created when nobody does; either way the address counts as
/// verified from now on (the caller proved it). Returns the user and whether it was created.
///
/// # Errors
///
/// The database failed.
pub async fn find_or_create(
    tx: &mut Tx,
    email: &EmailAddress,
) -> Result<(Standing, bool), sqlx::Error> {
    let row = sqlx::query!(
        r#"INSERT INTO users (email, email_verified_at) VALUES ($1, now())
           ON CONFLICT (email_key) DO UPDATE
              SET email_verified_at = coalesce(users.email_verified_at, now()), last_seen_at = now()
           RETURNING id AS "id: Id<User>", email, status, (xmax = 0) AS "created!""#,
        email.as_str()
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok((
        Standing {
            id: row.id,
            email: row.email,
            active: row.status == "active",
        },
        row.created,
    ))
}

/// Accepts the welcome of `user`, whom the caller's sign-in transaction has just created
/// ([`find_or_create`] answered `true`), to `email`, the address they proved, with a button to
/// `dashboard` (see the module). It commits with the sign-in; the caller wakes the sender after
/// the commit ([`accept::wake`]). Answers whether the welcome was queued: without a transactional
/// sender it is skipped, logged at warning level, and the sign-in goes on.
///
/// # Errors
///
/// The message could not be accepted for another reason: the database, or a programming error
/// (a template or an envelope rule the welcome breaks).
pub async fn welcome(
    tx: &mut Tx,
    keys: &Keys,
    user: Id<User>,
    email: &EmailAddress,
    dashboard: &Url,
) -> Result<bool, accept::Error> {
    let welcome = Transactional::Welcome {
        to: email,
        dashboard: dashboard.as_str(),
        expires_at: crate::process::now().plus(WELCOME_USEFUL_FOR),
    };
    match accept::transactional(tx, keys, &welcome).await {
        Ok(_) => Ok(true),
        Err(accept::Error::NoTransactionalSender) => {
            tracing::warn!(
                user_id = %user,
                "no transactional sender is configured: a new user was signed in without a welcome email"
            );
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Notes that `user` signed in now.
///
/// # Errors
///
/// The database failed.
pub async fn seen(tx: &mut Tx, user: Id<User>) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE users SET last_seen_at = now() WHERE id = $1",
        user.uuid()
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Reads `user` as the API shows it.
///
/// # Errors
///
/// The database failed.
pub async fn read(tx: &mut Tx, user: Id<User>) -> Result<Option<UserObject>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id AS "id: Id<User>", email, email_verified_at AS "email_verified_at: Timestamp", name,
                  locale, created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM users WHERE id = $1"#,
        user.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| UserObject {
        id: row.id,
        email: row.email,
        email_verified_at: row.email_verified_at,
        name: row.name,
        locale: row.locale,
        created_at: row.created_at,
        version: versioning::of(row.updated_at),
        updated_at: row.updated_at,
    }))
}

/// What `PATCH /v1/me` may change.
#[derive(Debug, Clone, Default)]
pub struct Changes<'a> {
    /// A new display name; an empty one clears it.
    pub name: Option<&'a str>,
    /// A new preferred language.
    pub locale: Option<&'a str>,
}

/// Locks `user`'s row and answers its current version, for an update's `If-Match`.
///
/// # Errors
///
/// The database failed.
pub async fn lock_version(tx: &mut Tx, user: Id<User>) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM users WHERE id = $1 FOR UPDATE"#,
        user.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// Applies `changes` to `user` (the caller holds the row's lock).
///
/// # Errors
///
/// The database failed.
pub async fn update(tx: &mut Tx, user: Id<User>, changes: &Changes<'_>) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE users
            SET name = CASE WHEN $2::text IS NULL THEN name ELSE nullif($2, '') END,
                locale = coalesce($3, locale)
          WHERE id = $1",
        user.uuid(),
        changes.name,
        changes.locale,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}
