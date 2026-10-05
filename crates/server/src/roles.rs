//! Server command dispatch and shared startup dependencies.
//! Runtime roles compose features and supervise shutdown; operator commands and probes exit
//! after their requested operation completes.

mod admin;
mod analytics;
mod api;
pub(crate) mod healthcheck;
mod inbox;
mod sender;
mod tracking;
mod worker;

use std::time::Duration;

use anyhow::Context as _;

use crate::config::{Common, KeyRole, Role};
use crate::crypto::{Keys, Subkey};
use crate::db::{Database, PoolSettings};

/// Runs `role` until it finishes or the process is asked to stop.
///
/// # Errors
///
/// An environment the role refuses (`config::check_environment`), or whatever stops the role.
pub async fn run(role: Role) -> anyhow::Result<()> {
    crate::config::check_environment(&role)?;
    match role {
        Role::Api(args) => api::run(args).await,
        Role::Sender(args) => sender::run(args).await,
        Role::Inbox(args) => inbox::run(args).await,
        Role::Worker(args) => worker::run(args).await,
        Role::Tracking(args) => tracking::run(args).await,
        Role::Analytics(args) => analytics::run(args).await,
        Role::Admin(args) => admin::run(args).await,
        Role::Healthcheck(args) => healthcheck::probe(&args),
    }
}

/// Connects a role's pool.
async fn connect(common: &Common, settings: PoolSettings) -> anyhow::Result<Database> {
    let url = common
        .database_url
        .as_ref()
        .context("DATABASE_URL is required by this role")?;
    Database::connect(url, settings).await.with_context(|| {
        format!(
            "cannot connect {} to the database",
            settings.application_name
        )
    })
}

/// The subkeys of the deployment key each role's work uses, and so all it holds (see `crypto`):
/// the tracking role never holds the sealing key, and no background role (sender, inbox, worker)
/// holds the identity secrets (the session token, sign-in code and CSRF subkeys) or the cursor and
/// address subkeys, which only requests use.
pub(crate) fn subkeys(role: KeyRole) -> &'static [Subkey] {
    use Subkey::{Address, Code, Csrf, Cursor, Link, MessageId, Seal, Token, Tracking};
    match role {
        KeyRole::Api => &[
            Seal, Cursor, Token, Code, Tracking, Link, MessageId, Csrf, Address,
        ],
        // Provider webhooks' verification keys are sealed; unsubscribe links are signed.
        KeyRole::Ingress => &[Seal, Link],
        // Credentials and refreshed grants are sealed; rendered mail carries tracking and
        // unsubscribe links. Message-IDs are minted when a message is accepted, not here.
        KeyRole::Sender => &[Seal, Tracking, Link],
        // Credentials and refreshed grants are sealed; replies are correlated by our Message-IDs.
        KeyRole::Inbox => &[Seal, MessageId],
        // Credentials, webhook endpoints' and SSO connections' secrets are sealed; exports are
        // download links; campaign and transactional mail is accepted (Message-IDs) and rendered
        // with links; receipts are correlated by Message-ID.
        KeyRole::Worker => &[Seal, Tracking, Link, MessageId],
        // Open and click tokens, the unsubscribe tokens they share a format with, and the hashed
        // client address of each event.
        KeyRole::Tracking => &[Tracking, Link, Address],
    }
}

/// The keys of `role`: the deployment key kept to the role's subkeys, with the previous key
/// during a rotation, or the role key made for it; refused at start when a subkey the role uses
/// is missing.
fn role_keys(common: &Common, role: KeyRole) -> anyhow::Result<Keys> {
    let needed = subkeys(role);
    let keys = match (&common.deployment_key, &common.role_key) {
        (Some(current), _) => {
            let keys = Keys::from_deployment_key(current)
                .context("NORBELYS_DEPLOYMENT_KEY is not a deployment key")?;
            let keys = match &common.previous_deployment_key {
                Some(previous) => keys
                    .with_previous(previous)
                    .context("NORBELYS_PREVIOUS_DEPLOYMENT_KEY is not a deployment key")?,
                None => keys,
            };
            keys.only(needed)
        }
        (None, Some(role_key)) => Keys::from_role_key(role_key)
            .context("NORBELYS_ROLE_KEY is not a role key")?
            .only(needed),
        (None, None) => {
            anyhow::bail!("NORBELYS_DEPLOYMENT_KEY or NORBELYS_ROLE_KEY is required by this role")
        }
    };
    keys.require(needed)
        .with_context(|| format!("NORBELYS_ROLE_KEY was made for another role than {role:?}"))?;
    Ok(keys)
}

/// The deployment's keys, every subkey: what the operator's `admin` commands hold.
fn keys(common: &Common) -> anyhow::Result<Keys> {
    role_keys(common, KeyRole::Api)
}

/// The pool settings of a background role.
fn background_pool(
    application_name: &'static str,
    max_connections: u32,
    statement_timeout: Duration,
) -> PoolSettings {
    PoolSettings {
        application_name,
        max_connections,
        statement_timeout,
        acquire_timeout: Duration::from_secs(5),
        min_connections: 1,
        request_deadline: None,
    }
}

/// A spool's directory: `configured` (the role's `variable`), else, in development only, `name`
/// under the system's temporary directory, which a restart of the machine may empty.
fn spool_dir(
    configured: Option<&std::path::Path>,
    common: &Common,
    variable: &str,
    name: &str,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = configured {
        return Ok(dir.to_path_buf());
    }
    anyhow::ensure!(
        common.environment == "development",
        "{variable} is required outside development"
    );
    let dir = std::env::temp_dir().join(name);
    tracing::warn!(dir = %dir.display(), "{variable} is not set: the spool is kept in a temporary directory (development only)");
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use secrecy::SecretString;
    use strum::IntoEnumIterator as _;

    use super::subkeys;
    use crate::config::KeyRole;
    use crate::crypto::{self, Keys, Subkey};

    /// Each role's key holds what its work uses and nothing an intruder on its host could turn
    /// against the rest: the api holds every subkey; the tracking role, on the public host, no
    /// sealing key; no other role an identity secret (session tokens, sign-in codes, CSRF tokens).
    /// The role key `admin deployment-key --for <role>` makes passes that role's own start check,
    /// so the operator's command and the role always agree. A new role fails here until its keys
    /// are decided.
    #[test]
    fn each_role_holds_only_the_subkeys_its_work_uses() {
        let deployment = SecretString::from(STANDARD.encode([5_u8; 32]));
        let identity = [Subkey::Token, Subkey::Code, Subkey::Csrf];
        for role in KeyRole::iter() {
            let held = subkeys(role);
            match role {
                KeyRole::Api => assert_eq!(held.len(), Subkey::iter().count()),
                KeyRole::Tracking => assert!(!held.contains(&Subkey::Seal)),
                KeyRole::Ingress | KeyRole::Sender | KeyRole::Inbox | KeyRole::Worker => {
                    assert!(held.contains(&Subkey::Seal), "{role:?}");
                }
            }
            if role != KeyRole::Api {
                assert!(
                    identity.iter().all(|subkey| !held.contains(subkey)),
                    "{role:?} holds an identity secret"
                );
            }
            let encoded = crypto::role_key(&deployment, None, held).unwrap();
            let keys = Keys::from_role_key(&SecretString::from(encoded)).unwrap();
            assert!(keys.require(held).is_ok(), "{role:?}");
        }
    }
}
