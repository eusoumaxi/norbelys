//! Fresh OAuth access tokens for the mailboxes connected through Google or Microsoft: the one
//! place a role turns a connection's stored grant into a token it can call the provider with.
//!
//! Both the sender (submitting through the Gmail API or Microsoft Graph) and the inbox (reading
//! the mailbox through the same APIs) need one; each process holds one cache shared by all its
//! tasks, so a token refreshed for one request serves the next until shortly before it expires.
//!
//! # How a token is found
//!
//! 1. The cache, when it holds a token for the connection at the `credential_version` the caller
//!    read and the token is still fresh for [`TOKEN_MARGIN`].
//! 2. The stored grant's own access token, when it is still fresh.
//! 3. Otherwise a refresh at the provider's token endpoint with Norbelys's app, within the
//!    caller's deadline. The new grant is stored sealed through `set_connection_credential()`
//!    only when the connection's `credential_version` is still the one the caller read; a grant
//!    replaced meanwhile (the person reconnected, another process refreshed) stands, and the
//!    refreshed token is used once anyway, since it is valid.
//!
//! Microsoft rotates the refresh token on every refresh, which is why the new grant is stored:
//! losing it would leave the stored refresh token one rotation behind.
//!
//! # Lock order
//!
//! Storing takes the connection's row (`FOR UPDATE`) only to compare its `credential_version`,
//! then writes through the accessor; no other lock is held, and no transaction is open during
//! the network call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use norbelys_mail::http::HttpClient;
use norbelys_mail::oauth::{self, OAuthError};
use secrecy::SecretString;
use tokio::time::Instant;

use super::credentials::{self, Credential, Grant};
use super::oauth::Apps;
use crate::crypto::Keys;
use crate::db::Database;
use crate::domain::ids::{Connection, Id, WorkspaceId};
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;

/// An access token is refreshed when it expires within this margin.
pub const TOKEN_MARGIN: Duration = Duration::from_secs(120);

/// The process's fresh access tokens by connection, and what refreshing them takes. Cheap to
/// clone; clones share the cache.
#[derive(Clone)]
pub struct Tokens {
    keys: Keys,
    http: HttpClient,
    apps: Apps,
    cache: Arc<Mutex<Cache>>,
}

/// Fresh tokens by connection.
type Cache = HashMap<(WorkspaceId, Id<Connection>), Cached>;

impl std::fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Tokens").finish_non_exhaustive()
    }
}

/// A fresh access token, kept until shortly before it expires.
#[derive(Clone)]
struct Cached {
    version: i64,
    token: SecretString,
    expires_at: Timestamp,
}

/// The connection a token is wanted for, as the caller read it.
#[derive(Clone, Copy)]
pub struct Grantee<'a> {
    /// The connection's workspace.
    pub workspace: WorkspaceId,
    /// The connection.
    pub connection: Id<Connection>,
    /// Its provider: Google or Microsoft.
    pub provider: Provider,
    /// Its credential, opened.
    pub credential: Option<&'a Credential>,
    /// The `credential_version` the credential was read at.
    pub version: i64,
}

/// Why no token could be had.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The connection holds no OAuth grant: the person reconnects the mailbox.
    #[error("the connection has no OAuth grant; reconnect the mailbox")]
    NoGrant,
    /// Norbelys's OAuth app for the provider is not configured on this deployment.
    #[error("Norbelys's OAuth app for this provider is not configured on this deployment")]
    NoApp,
    /// The token endpoint refused or failed the refresh.
    #[error("the token refresh failed: {0}")]
    Refresh(#[from] OAuthError),
}

impl Tokens {
    /// An empty cache that refreshes through `http` with `apps`, sealing new grants with `keys`.
    #[must_use]
    pub fn new(keys: Keys, http: HttpClient, apps: Apps) -> Self {
        Self {
            keys,
            http,
            apps,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A fresh access token for `grantee` (see the module), refreshing it by `deadline` when it
    /// must.
    ///
    /// # Errors
    ///
    /// [`TokenError`]: no grant, no app, or a refused or failed refresh. A failure to store the
    /// refreshed grant is not an error (it is logged; the token is valid).
    pub async fn access_token(
        &self,
        db: &Database,
        grantee: &Grantee<'_>,
        deadline: Instant,
    ) -> Result<SecretString, TokenError> {
        let soon = crate::process::now().plus(TOKEN_MARGIN);
        let key = (grantee.workspace, grantee.connection);
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(&key)
            && cached.version == grantee.version
            && cached.expires_at > soon
        {
            return Ok(cached.token.clone());
        }
        let Some(Credential::OAuth(grant)) = grantee.credential else {
            return Err(TokenError::NoGrant);
        };
        if grant.expires_at > soon {
            self.remember(key, grantee.version, grant);
            return Ok(grant.access_token.clone());
        }
        let Some((identity_provider, app, scopes)) = self.apps.app(grantee.provider) else {
            return Err(TokenError::NoApp);
        };
        let tokens = oauth::refresh(
            &self.http,
            &identity_provider,
            app,
            &grant.refresh_token,
            scopes,
            deadline,
        )
        .await?;
        let refreshed = Grant {
            refresh_token: tokens
                .refresh_token
                .unwrap_or_else(|| grant.refresh_token.clone()),
            access_token: tokens.access_token,
            expires_at: Timestamp(tokens.expires_at),
            scope: tokens.scope,
        };
        let version = self
            .store(db, grantee, &refreshed)
            .await
            .unwrap_or(grantee.version);
        self.remember(key, version, &refreshed);
        Ok(refreshed.access_token)
    }

    /// Stores a refreshed grant when the connection's credential is still the one it was
    /// refreshed from; the new `credential_version`, or `None` when another writer replaced it
    /// (that writer's credential stands) or the database failed.
    async fn store(&self, db: &Database, grantee: &Grantee<'_>, grant: &Grant) -> Option<i64> {
        let sealed = credentials::seal(
            &self.keys,
            grantee.workspace,
            grantee.connection,
            &Credential::OAuth(grant.clone()),
        )
        .ok()?;
        let result: Result<Option<i64>, sqlx::Error> = async {
            let mut tx = db.begin_in(grantee.workspace).await?;
            let current = sqlx::query_scalar!(
                "SELECT credential_version FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
                grantee.workspace.uuid(),
                grantee.connection.uuid(),
            )
            .fetch_optional(&mut *tx)
            .await?;
            if current != Some(grantee.version) {
                tx.commit().await?;
                return Ok(None);
            }
            let version = sqlx::query_scalar!(
                r#"SELECT set_connection_credential($1, $2, $3) AS "version!""#,
                grantee.workspace.uuid(),
                grantee.connection.uuid(),
                sealed,
            )
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(Some(version))
        }
        .await;
        match result {
            Ok(version) => version,
            Err(error) => {
                tracing::warn!(error = %error, connection = %grantee.connection, "a refreshed grant could not be stored");
                None
            }
        }
    }

    fn remember(&self, key: (WorkspaceId, Id<Connection>), version: i64, grant: &Grant) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                key,
                Cached {
                    version,
                    token: grant.access_token.clone(),
                    expires_at: grant.expires_at,
                },
            );
        }
    }
}
