//! Sealing of the Sending area's secrets: connection credentials and provider webhook keys.
//!
//! A connection's credential is a small JSON document (a password, with the API credential a
//! relay's account checks and reconciliation use when the customer gave one; or an OAuth grant:
//! refresh token, access token, its expiry and the granted scopes) sealed with the deployment key
//! (AES-256-GCM, [`crate::crypto::Keys::seal`]) and bound to its row: the associated data names
//! the table, the workspace and the connection, so a sealed value copied onto another row, or
//! into another workspace, does not open. A provider webhook's verification material (the
//! secret Norbelys generates for its own MTA, a relay's signing key, verification key or SNS
//! topic) is sealed the same way, bound to the webhook's row.
//!
//! Nothing here is logged or shown: the API's objects carry no credential, only whether a
//! webhook's key is set.

use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

use crate::crypto::{CryptoError, Keys};
use crate::domain::ids::{Connection, Id, ProviderWebhook, WorkspaceId};
use crate::domain::time::Timestamp;

/// A connection's secret.
#[derive(Clone)]
pub enum Credential {
    /// A password: a mailbox's login (SMTP and IMAP), a relay's SMTP credential, a login of
    /// the managed MTA.
    Password(SecretString),
    /// An OAuth grant at Google or Microsoft.
    OAuth(Grant),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Password(_) => "Credential::Password(..)",
            Self::OAuth(_) => "Credential::OAuth(..)",
        })
    }
}

/// An OAuth grant: what refreshing needs and what the last refresh gave.
#[derive(Clone)]
pub struct Grant {
    /// The refresh token; Microsoft rotates it on every refresh.
    pub refresh_token: SecretString,
    /// The current access token.
    pub access_token: SecretString,
    /// When the access token expires.
    pub expires_at: Timestamp,
    /// The scopes granted, space-separated, as the provider returned them.
    pub scope: String,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Grant")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// A relay's API credential, given besides its SMTP credential for the account checks and the
/// reconciliation of its events: an AWS access key for Amazon SES (`id` and `secret`), a private
/// API key for SendGrid or Mailgun (`secret` alone). It is sealed in the same document as the
/// connection's password, so it is bound to the same row, replaced with the same version bump
/// every check fences on, erased with it on archive, and re-sealed with it by a key rotation.
#[derive(Clone)]
pub struct ApiCredential {
    /// The access key id (`AKIA…`) of an AWS key; `None` for a SendGrid or Mailgun key.
    pub id: Option<String>,
    /// The secret: the AWS secret access key, or the API key.
    pub secret: SecretString,
}

impl std::fmt::Debug for ApiCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiCredential")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The sealed document.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Stored {
    Password {
        password: String,
        /// A relay's API credential; absent in documents sealed without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api: Option<StoredApi>,
    },
    Oauth {
        refresh_token: String,
        access_token: String,
        expires_at: Timestamp,
        scope: String,
    },
}

/// The sealed form of an [`ApiCredential`].
#[derive(Serialize, Deserialize)]
struct StoredApi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    secret: String,
}

/// Why a sealed value could not be used.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("the sealed credential is not a credential document")]
    Shape,
}

/// The associated data a connection's credential is sealed with: its table, workspace and row, so
/// a sealed value copied to another row never opens there. `admin secrets rotate` re-seals with it.
pub(crate) fn connection_context(workspace: WorkspaceId, connection: Id<Connection>) -> String {
    format!(
        "connections.credential:{}:{}",
        workspace.uuid(),
        connection.uuid()
    )
}

/// The associated data a provider webhook's verification material is sealed with: its table,
/// workspace and row. `admin secrets rotate` re-seals with it.
pub(crate) fn webhook_context(workspace: WorkspaceId, webhook: Id<ProviderWebhook>) -> String {
    format!(
        "provider_webhooks.signing_secret:{}:{}",
        workspace.uuid(),
        webhook.uuid()
    )
}

/// Seals `credential` for `connection` of `workspace`: a new credential, replacing the whole
/// document (a password sealed here carries no API credential; [`seal_relay`] keeps one).
///
/// # Errors
///
/// The system random source failed.
pub fn seal(
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    credential: &Credential,
) -> Result<Vec<u8>, CredentialError> {
    let stored = match credential {
        Credential::Password(password) => Stored::Password {
            password: password.expose_secret().to_owned(),
            api: None,
        },
        Credential::OAuth(grant) => Stored::Oauth {
            refresh_token: grant.refresh_token.expose_secret().to_owned(),
            access_token: grant.access_token.expose_secret().to_owned(),
            expires_at: grant.expires_at,
            scope: grant.scope.clone(),
        },
    };
    seal_stored(keys, workspace, connection, &stored)
}

/// Seals a relay's `password` with its API credential `api`, when it has one, for `connection`
/// of `workspace`.
///
/// # Errors
///
/// The system random source failed.
pub fn seal_relay(
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    password: &SecretString,
    api: Option<&ApiCredential>,
) -> Result<Vec<u8>, CredentialError> {
    let stored = Stored::Password {
        password: password.expose_secret().to_owned(),
        api: api.map(|api| StoredApi {
            id: api.id.clone(),
            secret: api.secret.expose_secret().to_owned(),
        }),
    };
    seal_stored(keys, workspace, connection, &stored)
}

fn seal_stored(
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    stored: &Stored,
) -> Result<Vec<u8>, CredentialError> {
    let plaintext = serde_json::to_vec(stored).map_err(|_| CredentialError::Shape)?;
    Ok(keys.seal(
        &plaintext,
        connection_context(workspace, connection).as_bytes(),
    )?)
}

/// Opens the credential sealed for `connection` of `workspace`.
///
/// # Errors
///
/// The value was sealed under another key or for another row, or it is not a credential.
pub fn open(
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    sealed: &[u8],
) -> Result<Credential, CredentialError> {
    open_parts(keys, workspace, connection, sealed).map(|(credential, _)| credential)
}

/// Opens the credential sealed for `connection` of `workspace` with the API credential sealed
/// beside a relay's password, when one was given.
///
/// # Errors
///
/// The value was sealed under another key or for another row, or it is not a credential.
pub fn open_parts(
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    sealed: &[u8],
) -> Result<(Credential, Option<ApiCredential>), CredentialError> {
    let plaintext = keys.open(sealed, connection_context(workspace, connection).as_bytes())?;
    let stored: Stored = serde_json::from_slice(&plaintext).map_err(|_| CredentialError::Shape)?;
    Ok(match stored {
        Stored::Password { password, api } => (
            Credential::Password(SecretString::from(password)),
            api.map(|api| ApiCredential {
                id: api.id,
                secret: SecretString::from(api.secret),
            }),
        ),
        Stored::Oauth {
            refresh_token,
            access_token,
            expires_at,
            scope,
        } => (
            Credential::OAuth(Grant {
                refresh_token: SecretString::from(refresh_token),
                access_token: SecretString::from(access_token),
                expires_at,
                scope,
            }),
            None,
        ),
    })
}

/// Seals a provider webhook's verification material for `webhook` of `workspace`.
///
/// # Errors
///
/// The system random source failed.
pub fn seal_webhook_key(
    keys: &Keys,
    workspace: WorkspaceId,
    webhook: Id<ProviderWebhook>,
    key: &SecretString,
) -> Result<Vec<u8>, CryptoError> {
    keys.seal(
        key.expose_secret().as_bytes(),
        webhook_context(workspace, webhook).as_bytes(),
    )
}

/// Opens a provider webhook's verification material.
///
/// # Errors
///
/// The value was sealed under another key or for another row, or it is not text.
pub fn open_webhook_key(
    keys: &Keys,
    workspace: WorkspaceId,
    webhook: Id<ProviderWebhook>,
    sealed: &[u8],
) -> Result<SecretString, CredentialError> {
    let plaintext = keys.open(sealed, webhook_context(workspace, webhook).as_bytes())?;
    String::from_utf8(plaintext)
        .map(SecretString::from)
        .map_err(|_| CredentialError::Shape)
}

#[cfg(test)]
mod tests {
    use secrecy::{ExposeSecret as _, SecretString};
    use uuid::Uuid;

    use super::{ApiCredential, Credential, open, open_parts, seal, seal_relay};
    use crate::domain::ids::{Connection, Id, WorkspaceId};
    use crate::testing;

    /// A relay's API credential is sealed in its password's document and comes back with it,
    /// bound to the same row; a password sealed alone, or a document sealed before API
    /// credentials existed, opens with none. So the account checks find the key a customer gave,
    /// and no other row or workspace can open it.
    #[test]
    fn an_api_credential_is_sealed_beside_the_password() {
        let keys = testing::keys();
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        let connection = Id::<Connection>::new();
        let api = ApiCredential {
            id: Some("AKIAIOSFODNN7EXAMPLE".to_owned()),
            secret: SecretString::from("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
        };
        let sealed = seal_relay(
            &keys,
            workspace,
            connection,
            &SecretString::from("smtp-password"),
            Some(&api),
        )
        .unwrap();
        let (Credential::Password(password), Some(opened)) =
            open_parts(&keys, workspace, connection, &sealed).unwrap()
        else {
            panic!("a password with its API credential");
        };
        assert_eq!(password.expose_secret(), "smtp-password");
        assert_eq!(opened.id.as_deref(), Some("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(
            opened.secret.expose_secret(),
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        );
        assert!(open_parts(&keys, workspace, Id::new(), &sealed).is_err());

        let alone = seal(
            &keys,
            workspace,
            connection,
            &Credential::Password(SecretString::from("smtp-password")),
        )
        .unwrap();
        assert!(
            open_parts(&keys, workspace, connection, &alone)
                .unwrap()
                .1
                .is_none()
        );
        let before = keys
            .seal(
                br#"{"kind":"password","password":"old"}"#,
                super::connection_context(workspace, connection).as_bytes(),
            )
            .unwrap();
        let (Credential::Password(old), None) =
            open_parts(&keys, workspace, connection, &before).unwrap()
        else {
            panic!("a document without an API credential opens without one");
        };
        assert_eq!(old.expose_secret(), "old");
    }

    /// A sealed credential opens only for the row it was sealed for: the same bytes copied to
    /// another connection, or into another workspace, refuse to open, so a leaked or misplaced
    /// value is useless outside its row.
    #[test]
    fn a_credential_opens_only_for_its_own_row() {
        let keys = testing::keys();
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        let connection = Id::<Connection>::new();
        let sealed = seal(
            &keys,
            workspace,
            connection,
            &Credential::Password(SecretString::from("s3cret")),
        )
        .unwrap();
        let Credential::Password(password) = open(&keys, workspace, connection, &sealed).unwrap()
        else {
            panic!("a password comes back a password");
        };
        assert_eq!(password.expose_secret(), "s3cret");
        assert!(open(&keys, workspace, Id::new(), &sealed).is_err());
        assert!(
            open(
                &keys,
                WorkspaceId::trusted(Uuid::now_v7()),
                connection,
                &sealed
            )
            .is_err()
        );
    }
}
