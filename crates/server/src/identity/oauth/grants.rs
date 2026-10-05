//! The authorization server's rows: grants, authorization codes, refresh tokens and device codes.
//!
//! # Grants
//!
//! A grant is one person's consent for one client to act in one workspace, for one resource
//! (the MCP server, or the API for the command-line client), with a set of scopes. It records the
//! workspace authentication proof of the session that consented (its method, SSO connection and
//! policy version, and when the person authenticated), which an enforcing workspace checks on
//! every use exactly as it checks a session's. It ends 90 days after consent, or when revoked
//! (`POST /oauth/revoke`, `DELETE /v1/me/grants/{id}`, or refresh token reuse).
//!
//! # Codes and tokens
//!
//! Authorization codes, refresh tokens and device codes are random secrets of which only the
//! keyed hash is stored (`Keys::hash_token`), so a copy of the tables redeems nothing.
//!
//! - An **authorization code** is bound to its grant, its redirect URI and an S256 code challenge,
//!   lives five minutes and is consumed by the first exchange that presents it, whatever the
//!   outcome; a second presentation finds it consumed.
//! - A **refresh token** belongs to a chain hanging from its grant: one root per grant, one child
//!   per parent, every parent of the same grant (the schema enforces all three). A refresh takes
//!   the grant's row lock first and the token's second, the order every writer of these rows
//!   keeps, so two refreshes of one grant serialize and a token is consumed exactly once.
//! - A **device code** (RFC 8628) waits ten minutes for a person to approve or deny its user
//!   code; each poll is recorded so a client polling faster than its interval is told to slow
//!   down; the approval creates the grant, and the one poll that sees it consumes the code.
//!
//! Every function here runs inside the caller's transaction and commits nothing itself.

use uuid::Uuid;

use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::domain::identity::Proof;
use crate::domain::ids::{Grant, Id, User, Workspace, WorkspaceId};
use crate::domain::oauth::{self, DeviceState, RefreshState};
use crate::domain::scope::ScopeSet;
use crate::domain::time::Timestamp;
use crate::identity::audit::{self, Action, AuditActor};

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// The random source failed.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// A grant as authentication and the token endpoint read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    /// The grant.
    pub id: Id<Grant>,
    /// The client it was given to.
    pub client_id: String,
    /// The person who consented.
    pub user: Id<User>,
    /// The workspace it acts in.
    pub workspace: Id<Workspace>,
    /// The resource it is for.
    pub resource: String,
    /// Its scopes.
    pub scopes: ScopeSet,
    /// The consenting session's workspace authentication proof.
    pub proof: Proof,
    /// When it ends.
    pub expires_at: Timestamp,
    /// Whether it was revoked.
    pub revoked: bool,
    /// False when its user is suspended.
    pub user_active: bool,
}

impl GrantRow {
    /// Whether it still authorizes at `now`.
    #[must_use]
    pub fn active(&self, now: Timestamp) -> bool {
        !self.revoked && self.user_active && now < self.expires_at
    }
}

/// Reads a grant with its user's status; `None` when it does not exist.
///
/// # Errors
///
/// The database failed.
pub async fn by_id(tx: &mut Tx, grant: Id<Grant>) -> Result<Option<GrantRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT g.id AS "id: Id<Grant>", g.client_id, g.user_id AS "user_id: Id<User>",
                  g.workspace_id AS "workspace_id: Id<Workspace>", g.resource, g.scopes,
                  g.auth_method, g.sso_connection_id, g.sso_policy_version,
                  g.authenticated_at AS "authenticated_at: Timestamp",
                  g.expires_at AS "expires_at: Timestamp", g.revoked_at IS NOT NULL AS "revoked!",
                  u.status = 'active' AS "user_active!"
             FROM oauth_grants g JOIN users u ON u.id = g.user_id
            WHERE g.id = $1"#,
        grant.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|row| {
        Some(GrantRow {
            id: row.id,
            client_id: row.client_id,
            user: row.user_id,
            workspace: row.workspace_id,
            resource: row.resource,
            scopes: ScopeSet::parse(row.scopes.iter().map(String::as_str)).ok()?,
            proof: Proof {
                method: row.auth_method.parse().ok()?,
                connection: row.sso_connection_id,
                policy_version: row.sso_policy_version,
                authenticated_at: row.authenticated_at.0,
            },
            expires_at: row.expires_at,
            revoked: row.revoked,
            user_active: row.user_active,
        })
    }))
}

/// A grant about to be created at consent.
#[derive(Debug, Clone)]
pub struct NewGrant<'a> {
    /// The client.
    pub client_id: &'a str,
    /// The consenting person.
    pub user: Id<User>,
    /// The workspace they chose.
    pub workspace: WorkspaceId,
    /// The resource.
    pub resource: &'a str,
    /// The scopes, already narrowed to the person's role.
    pub scopes: ScopeSet,
    /// The consenting session's proof; never a break-glass session's (the caller refuses those).
    pub proof: Proof,
}

/// The most live grants one person keeps; `GET /v1/me` lists them all.
pub const MAX_GRANTS: i64 = 50;

/// Creates a grant that ends 90 days from now. A person keeps at most [`MAX_GRANTS`] live grants:
/// beyond them the oldest end, with their refresh chains, as a sign-in beyond the session cap ends
/// the oldest session, each recorded as `grant.revoked` (reason `replaced`) in its workspace's
/// audit log. Their access tokens stop within the authority's minute, like any revocation's.
///
/// # Errors
///
/// The database failed or refused the row (a break-glass proof breaks its check).
pub async fn create(tx: &mut Tx, new: &NewGrant<'_>) -> Result<Id<Grant>, sqlx::Error> {
    let expires_at = Timestamp(oauth::after(crate::process::now().0, oauth::GRANT_LIFETIME));
    let id = sqlx::query_scalar!(
        r#"INSERT INTO oauth_grants (client_id, user_id, workspace_id, resource, scopes, auth_method,
                                     sso_connection_id, sso_policy_version, authenticated_at, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           RETURNING id AS "id: Id<Grant>""#,
        new.client_id,
        new.user.uuid(),
        new.workspace.uuid(),
        new.resource,
        &new.scopes.to_strings(),
        new.proof.method.as_str(),
        new.proof.connection,
        new.proof.policy_version,
        Timestamp(new.proof.authenticated_at) as _,
        expires_at as _,
    )
    .fetch_one(&mut **tx)
    .await?;
    let ended = sqlx::query!(
        r#"UPDATE oauth_grants SET revoked_at = now()
            WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()
              AND id NOT IN (SELECT id FROM oauth_grants
                              WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()
                              ORDER BY id DESC LIMIT $2)
           RETURNING id AS "id: Id<Grant>", workspace_id"#,
        new.user.uuid(),
        MAX_GRANTS,
    )
    .fetch_all(&mut **tx)
    .await?;
    for grant in &ended {
        sqlx::query!(
            "UPDATE oauth_refresh_tokens SET revoked_at = now() WHERE grant_id = $1 AND revoked_at IS NULL",
            grant.id.uuid(),
        )
        .execute(&mut **tx)
        .await?;
        let workspace = WorkspaceId::trusted(grant.workspace_id);
        let previous = crate::db::switch_workspace(tx, workspace).await?;
        audit::record(
            tx,
            workspace,
            AuditActor::User(new.user),
            Action::GrantRevoked,
            Some(grant.id.to_string()),
            serde_json::json!({ "reason": "replaced" }),
            None,
        )
        .await?;
        crate::db::restore_workspace(tx, previous).await?;
    }
    Ok(id)
}

/// Records that a grant was just used to issue tokens.
///
/// # Errors
///
/// The database failed.
pub async fn touch(tx: &mut Tx, grant: Id<Grant>) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE oauth_grants SET last_used_at = now() WHERE id = $1",
        grant.uuid()
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Revokes a live grant and every refresh token of its chain, grant first (the lock order of
/// the module); with `user`, only a grant of that person. Answers the grant's workspace, or
/// `None` when there was no such live grant.
///
/// # Errors
///
/// The database failed.
pub async fn revoke(
    tx: &mut Tx,
    grant: Id<Grant>,
    user: Option<Id<User>>,
) -> Result<Option<WorkspaceId>, sqlx::Error> {
    let workspace: Option<Uuid> = sqlx::query_scalar!(
        "UPDATE oauth_grants SET revoked_at = now()
          WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2) AND revoked_at IS NULL
          RETURNING workspace_id",
        grant.uuid(),
        user.map(|user| user.uuid()),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if workspace.is_some() {
        sqlx::query!(
            "UPDATE oauth_refresh_tokens SET revoked_at = now() WHERE grant_id = $1 AND revoked_at IS NULL",
            grant.uuid(),
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(workspace.map(WorkspaceId::trusted))
}

/// Issues an authorization code for `grant`, bound to `redirect_uri` and `challenge`; returns
/// the code, shown once.
///
/// # Errors
///
/// The random source or the database failed.
pub async fn issue_code(
    tx: &mut Tx,
    keys: &Keys,
    grant: Id<Grant>,
    redirect_uri: &str,
    challenge: &str,
) -> Result<String, StoreError> {
    let code = crypto::random_token(32)?;
    let expires_at = crate::process::now().plus(oauth::CODE_LIFETIME);
    sqlx::query!(
        "INSERT INTO oauth_codes (code_hash, grant_id, redirect_uri, code_challenge, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
        keys.hash_token(&code),
        grant.uuid(),
        redirect_uri,
        challenge,
        expires_at as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(code)
}

/// An authorization code as its exchange reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRow {
    /// The grant it was issued for.
    pub grant: Id<Grant>,
    /// The redirect URI it was issued to.
    pub redirect_uri: String,
    /// Its S256 code challenge.
    pub code_challenge: String,
    /// When it expires.
    pub expires_at: Timestamp,
    /// Whether an earlier exchange already presented it.
    pub replayed: bool,
}

/// Takes the code `code` for an exchange: locks it, marks it consumed, and answers it with
/// whether it had been presented before; `None` for a code that was never issued.
///
/// # Errors
///
/// The database failed.
pub async fn take_code(
    tx: &mut Tx,
    keys: &Keys,
    code: &str,
) -> Result<Option<CodeRow>, sqlx::Error> {
    let hash = keys.hash_token(code);
    let row = sqlx::query!(
        r#"SELECT grant_id AS "grant_id: Id<Grant>", redirect_uri, code_challenge,
                  expires_at AS "expires_at: Timestamp", consumed_at IS NOT NULL AS "consumed!"
             FROM oauth_codes WHERE code_hash = $1 FOR UPDATE"#,
        hash
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if !row.consumed {
        sqlx::query!(
            "UPDATE oauth_codes SET consumed_at = now() WHERE code_hash = $1",
            hash
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(Some(CodeRow {
        grant: row.grant_id,
        redirect_uri: row.redirect_uri,
        code_challenge: row.code_challenge,
        expires_at: row.expires_at,
        replayed: row.consumed,
    }))
}

/// Issues a refresh token of `grant`'s chain: its root without `parent`, else the one child of
/// `parent` (the hash of the token it replaces); returns the token, shown once.
///
/// # Errors
///
/// The random source or the database failed (a second root or a second child breaks a unique
/// index).
pub async fn issue_refresh(
    tx: &mut Tx,
    keys: &Keys,
    grant: Id<Grant>,
    parent: Option<&[u8]>,
    expires_at: Timestamp,
) -> Result<String, StoreError> {
    let token = crypto::random_token(32)?;
    sqlx::query!(
        "INSERT INTO oauth_refresh_tokens (token_hash, grant_id, rotated_from, expires_at)
         VALUES ($1, $2, $3, $4)",
        keys.hash_token(&token),
        grant.uuid(),
        parent,
        expires_at as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(token)
}

/// A refresh token and its grant, read under both locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRow {
    /// The token's hash.
    pub hash: Vec<u8>,
    /// Its grant.
    pub grant: Id<Grant>,
    /// The grant's client.
    pub client_id: String,
    /// The grant's expiry.
    pub grant_expires_at: Timestamp,
    /// The token's state and its grant's, for the refresh decision (with `same_client` false
    /// until the caller compares the client).
    pub state: RefreshState,
}

/// Locks the refresh token `token` and its grant (grant first; see the module); `None` for a
/// token that was never issued.
///
/// # Errors
///
/// The database failed.
pub async fn lock_refresh(
    tx: &mut Tx,
    keys: &Keys,
    token: &str,
) -> Result<Option<RefreshRow>, sqlx::Error> {
    let hash = keys.hash_token(token);
    let Some(grant) = sqlx::query_scalar!(
        r#"SELECT grant_id AS "grant_id: Id<Grant>" FROM oauth_refresh_tokens WHERE token_hash = $1"#,
        hash
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let grant_row = sqlx::query!(
        r#"SELECT client_id, expires_at AS "expires_at: Timestamp", revoked_at IS NOT NULL AS "revoked!"
             FROM oauth_grants WHERE id = $1 FOR UPDATE"#,
        grant.uuid()
    )
    .fetch_one(&mut **tx)
    .await?;
    let token_row = sqlx::query!(
        r#"SELECT expires_at AS "expires_at: Timestamp", consumed_at IS NOT NULL AS "consumed!",
                  revoked_at IS NOT NULL AS "revoked!"
             FROM oauth_refresh_tokens WHERE token_hash = $1 FOR UPDATE"#,
        hash
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(Some(RefreshRow {
        hash,
        grant,
        client_id: grant_row.client_id,
        grant_expires_at: grant_row.expires_at,
        state: RefreshState {
            same_client: false,
            consumed: token_row.consumed,
            revoked: token_row.revoked,
            expires_at: token_row.expires_at.0,
            grant_revoked: grant_row.revoked,
            grant_expires_at: grant_row.expires_at.0,
        },
    }))
}

/// Marks a locked refresh token used.
///
/// # Errors
///
/// The database failed.
pub async fn consume_refresh(tx: &mut Tx, hash: &[u8]) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE oauth_refresh_tokens SET consumed_at = now() WHERE token_hash = $1",
        hash
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The grant a refresh token belongs to, without locking; `None` for an unknown token.
///
/// # Errors
///
/// The database failed.
pub async fn grant_of_refresh(
    tx: &mut Tx,
    keys: &Keys,
    token: &str,
) -> Result<Option<Id<Grant>>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT grant_id AS "grant_id: Id<Grant>" FROM oauth_refresh_tokens WHERE token_hash = $1"#,
        keys.hash_token(token)
    )
    .fetch_optional(&mut **tx)
    .await
}

/// A started device authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStart {
    /// The device code the client polls with, shown once.
    pub device_code: String,
    /// The user code a person types, stored form (eight letters).
    pub user_code: String,
    /// When both expire.
    pub expires_at: Timestamp,
}

/// Starts a device authorization for `client_id` and `resource` with `scopes`.
///
/// # Errors
///
/// The random source or the database failed.
pub async fn start_device(
    tx: &mut Tx,
    keys: &Keys,
    client_id: &str,
    resource: &str,
    scopes: ScopeSet,
) -> Result<DeviceStart, StoreError> {
    let device_code = crypto::random_token(32)?;
    let expires_at = crate::process::now().plus(oauth::DEVICE_LIFETIME);
    // Eight letters of a 20-letter alphabet are 2.56e10 codes: a collision with a live code is
    // improbable, and a few draws make it practically impossible.
    for _ in 0..4 {
        let user_code = oauth::user_code(crypto::random_bytes(32)?).unwrap_or_default();
        if user_code.is_empty() {
            continue;
        }
        let inserted = sqlx::query!(
            "INSERT INTO oauth_device_codes (device_code_hash, user_code, client_id, resource, scopes,
                                             interval_seconds, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (user_code) DO NOTHING",
            keys.hash_token(&device_code),
            user_code,
            client_id,
            resource,
            &scopes.to_strings(),
            oauth::DEVICE_INTERVAL,
            expires_at as _,
        )
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(DeviceStart {
                device_code,
                user_code,
                expires_at,
            });
        }
    }
    Err(StoreError::Crypto(CryptoError::Random))
}

/// A device code as a poll or an approval reads it, locked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// The device code's hash.
    pub hash: Vec<u8>,
    /// The user code, stored form.
    pub user_code: String,
    /// The client that started it.
    pub client_id: String,
    /// The resource asked for.
    pub resource: String,
    /// The scopes asked for.
    pub scopes: ScopeSet,
    /// The grant its approval created.
    pub grant: Option<Id<Grant>>,
    /// Its state, for the poll decision.
    pub state: DeviceState,
}

async fn lock_device(
    tx: &mut Tx,
    hash: Option<&[u8]>,
    user_code: Option<&str>,
) -> Result<Option<DeviceRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT device_code_hash, user_code, client_id, resource, scopes, interval_seconds,
                  expires_at AS "expires_at: Timestamp", last_polled_at AS "last_polled_at: Timestamp",
                  approved_grant_id AS "approved_grant_id: Id<Grant>",
                  denied_at IS NOT NULL AS "denied!", consumed_at IS NOT NULL AS "consumed!"
             FROM oauth_device_codes
            WHERE device_code_hash = $1 OR user_code = $2
            FOR UPDATE"#,
        hash,
        user_code,
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| DeviceRow {
        hash: row.device_code_hash,
        user_code: row.user_code,
        client_id: row.client_id,
        resource: row.resource,
        scopes: ScopeSet::parse(row.scopes.iter().map(String::as_str)).unwrap_or_default(),
        grant: row.approved_grant_id,
        state: DeviceState {
            expires_at: row.expires_at.0,
            last_polled_at: row.last_polled_at.map(|at| at.0),
            interval_seconds: row.interval_seconds,
            denied: row.denied,
            approved: row.approved_grant_id.is_some(),
            consumed: row.consumed,
        },
    }))
}

/// Locks the device code `device_code` for a poll; `None` for a code never issued.
///
/// # Errors
///
/// The database failed.
pub async fn lock_device_code(
    tx: &mut Tx,
    keys: &Keys,
    device_code: &str,
) -> Result<Option<DeviceRow>, sqlx::Error> {
    lock_device(tx, Some(&keys.hash_token(device_code)), None).await
}

/// Locks the device code a person typed (stored form) for an approval; `None` when no code has
/// it.
///
/// # Errors
///
/// The database failed.
pub async fn lock_user_code(
    tx: &mut Tx,
    user_code: &str,
) -> Result<Option<DeviceRow>, sqlx::Error> {
    lock_device(tx, None, Some(user_code)).await
}

/// Records a poll of a locked device code at the interval the client must now keep.
///
/// # Errors
///
/// The database failed.
pub async fn record_poll(
    tx: &mut Tx,
    hash: &[u8],
    interval_seconds: i16,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE oauth_device_codes SET last_polled_at = now(), interval_seconds = $2 WHERE device_code_hash = $1",
        hash,
        interval_seconds,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Marks a locked device code's tokens issued.
///
/// # Errors
///
/// The database failed.
pub async fn consume_device(tx: &mut Tx, hash: &[u8]) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE oauth_device_codes SET consumed_at = now(), last_polled_at = now() WHERE device_code_hash = $1",
        hash
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Records a person's decision on a locked device code: approved with the grant it created, or
/// denied without one.
///
/// # Errors
///
/// The database failed.
pub async fn decide_device(
    tx: &mut Tx,
    hash: &[u8],
    grant: Option<Id<Grant>>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE oauth_device_codes
            SET approved_grant_id = $2, denied_at = CASE WHEN $2::uuid IS NULL THEN now() END
          WHERE device_code_hash = $1",
        hash,
        grant.map(|grant| grant.uuid()),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::identity::AuthMethod;
    use crate::domain::oauth::{Poll, poll};
    use crate::identity::oauth::clients::CLI;
    use crate::testing::{TestDb, keys};

    /// A live grant of the command-line client for a new workspace's owner.
    async fn grant(test: &TestDb) -> Id<Grant> {
        let workspace = test.workspace("acme").await;
        let mut tx = test.app.begin().await.unwrap();
        let grant = create(
            &mut tx,
            &NewGrant {
                client_id: CLI,
                user: workspace.owner,
                workspace: workspace.id,
                resource: "http://127.0.0.1:3001",
                scopes: ScopeSet::all(),
                proof: Proof {
                    method: AuthMethod::EmailCode,
                    connection: None,
                    policy_version: None,
                    authenticated_at: crate::process::now().0,
                },
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        grant
    }

    /// A code is taken once even by two exchanges racing for it: the second waits on the row lock
    /// and then sees it consumed, so a code can never be redeemed twice.
    #[tokio::test]
    async fn codes_are_taken_once() {
        let test = TestDb::new().await;
        let grant = grant(&test).await;
        let mut tx = test.app.begin().await.unwrap();
        let code = issue_code(
            &mut tx,
            &keys(),
            grant,
            "https://client.test/cb",
            "challenge",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let mut first = test.app.begin().await.unwrap();
        let taken = take_code(&mut first, &keys(), &code)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((taken.grant, taken.replayed), (grant, false));
        let db = test.app.clone();
        let racing = code.clone();
        let second = tokio::spawn(async move {
            let mut tx = db.begin().await.unwrap();
            let taken = take_code(&mut tx, &keys(), &racing).await.unwrap().unwrap();
            tx.commit().await.unwrap();
            taken.replayed
        });
        first.commit().await.unwrap();
        assert!(
            second.await.unwrap(),
            "the second exchange sees the code consumed"
        );
        let mut tx = test.app.begin().await.unwrap();
        assert!(
            take_code(&mut tx, &keys(), "unknown")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A grant's refresh tokens form one chain: one root, one child per parent, so a rotated token
    /// can never fork the session; revoking the grant revokes every token of the chain and happens
    /// once.
    #[tokio::test]
    async fn refresh_tokens_form_one_chain() {
        let test = TestDb::new().await;
        let grant = grant(&test).await;
        let later = crate::process::now().plus(std::time::Duration::from_secs(3600));
        let mut tx = test.app.begin().await.unwrap();
        let root = issue_refresh(&mut tx, &keys(), grant, None, later)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = test.app.begin().await.unwrap();
        assert!(
            issue_refresh(&mut tx, &keys(), grant, None, later)
                .await
                .is_err(),
            "a second root"
        );
        drop(tx);

        let mut tx = test.app.begin().await.unwrap();
        let row = lock_refresh(&mut tx, &keys(), &root)
            .await
            .unwrap()
            .unwrap();
        assert!(!row.state.consumed && !row.state.revoked && !row.state.grant_revoked);
        consume_refresh(&mut tx, &row.hash).await.unwrap();
        let child = issue_refresh(&mut tx, &keys(), grant, Some(&row.hash), later)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = test.app.begin().await.unwrap();
        assert!(
            issue_refresh(&mut tx, &keys(), grant, Some(&row.hash), later)
                .await
                .is_err(),
            "a second child of one parent"
        );
        drop(tx);

        let mut tx = test.app.begin().await.unwrap();
        assert!(revoke(&mut tx, grant, None).await.unwrap().is_some());
        assert!(revoke(&mut tx, grant, None).await.unwrap().is_none());
        tx.commit().await.unwrap();
        let mut tx = test.app.begin().await.unwrap();
        for token in [&root, &child] {
            let row = lock_refresh(&mut tx, &keys(), token)
                .await
                .unwrap()
                .unwrap();
            assert!(row.state.grant_revoked && row.state.revoked, "{token}");
        }
        assert_eq!(
            grant_of_refresh(&mut tx, &keys(), &child).await.unwrap(),
            Some(grant)
        );
    }

    /// A device code records each poll (so a client polling too fast is told to slow down), is
    /// found by its user code, takes one decision, and is consumed once: the store gives each
    /// poll answer its state.
    #[tokio::test]
    async fn device_codes_record_polls_and_decisions() {
        let test = TestDb::new().await;
        let grant = grant(&test).await;
        let mut tx = test.app.begin().await.unwrap();
        let started = start_device(
            &mut tx,
            &keys(),
            CLI,
            "http://127.0.0.1:3001",
            ScopeSet::all(),
        )
        .await
        .unwrap();
        let denied = start_device(
            &mut tx,
            &keys(),
            CLI,
            "http://127.0.0.1:3001",
            ScopeSet::all(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_ne!(started.user_code, denied.user_code);

        let state = |device_code: String| {
            let db = test.app.clone();
            async move {
                let mut tx = db.begin().await.unwrap();
                let row = lock_device_code(&mut tx, &keys(), &device_code)
                    .await
                    .unwrap()
                    .unwrap();
                tx.commit().await.unwrap();
                row
            }
        };
        let now = || crate::process::now().0;
        let fresh = state(started.device_code.clone()).await;
        assert_eq!(poll(&fresh.state, now()), Poll::Pending);
        let mut tx = test.app.begin().await.unwrap();
        record_poll(&mut tx, &fresh.hash, 10).await.unwrap();
        tx.commit().await.unwrap();
        let polled = state(started.device_code.clone()).await;
        assert_eq!(polled.state.interval_seconds, 10);
        assert_eq!(poll(&polled.state, now()), Poll::SlowDown);

        let mut tx = test.app.begin().await.unwrap();
        let typed = lock_user_code(&mut tx, &started.user_code)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(typed.hash, fresh.hash);
        decide_device(&mut tx, &typed.hash, Some(grant))
            .await
            .unwrap();
        let other = lock_user_code(&mut tx, &denied.user_code)
            .await
            .unwrap()
            .unwrap();
        decide_device(&mut tx, &other.hash, None).await.unwrap();
        tx.commit().await.unwrap();
        let approved = state(started.device_code.clone()).await;
        assert_eq!(
            (poll(&approved.state, now()), approved.grant),
            (Poll::Issue, Some(grant))
        );
        assert_eq!(
            poll(&state(denied.device_code).await.state, now()),
            Poll::Denied
        );

        let mut tx = test.app.begin().await.unwrap();
        consume_device(&mut tx, &approved.hash).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            poll(&state(started.device_code).await.state, now()),
            Poll::Consumed
        );
    }
}
