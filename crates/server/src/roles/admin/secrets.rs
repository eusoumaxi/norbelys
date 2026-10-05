//! `norbelys-server admin secrets rotate`: re-seals every stored secret under the current
//! deployment key, the step of a key rotation that lets the previous key go.
//!
//! # What is sealed, and bound to what
//!
//! | Column | Associated data |
//! |---|---|
//! | `connections.credential` | the workspace and the connection |
//! | `provider_webhooks.signing_secret` | the workspace and the webhook |
//! | `sso_connections.client_secret` | the workspace and the connection |
//! | `webhook_endpoints.secret` | the workspace and the endpoint |
//! | `signing_keys.private_key` | the key's `kid` |
//! | `auth_ceremonies.state` | the ceremony; live ceremonies only, since an expired one is never opened again and retention removes it |
//!
//! The associated data comes from each column's own module, so a re-sealed value opens exactly
//! where the original did. OAuth consent requests are sealed as well but never stored: they live
//! in the dashboard's address bar for the few minutes a consent takes, under whichever key sealed
//! them, and keeping the previous key through the rotation covers them.
//!
//! # How
//!
//! Each column is walked in key order, `batch` rows at a time, each batch in a short transaction
//! of its own, so no lock is held for long and a crash repeats at most one batch. Only rows whose
//! sealed bytes name another key are read: each is opened with the key it names and sealed again
//! under the current key with the same associated data, then written back only while the stored
//! bytes are still those read (compare and set). A secret changed meanwhile (a refreshed grant, a
//! pasted key) keeps its new value, which its writer sealed under its own current key; a
//! connection's credential version moves with the re-seal, as with every write of a credential,
//! so a refresh that read the old bytes notices. A row naming a key this process does not hold,
//! or whose bytes do not open, is left as it is and counted.
//!
//! Run it once every role holds the new key, after the rolling restart: a replica still sealing
//! under the old key would leave rows behind the walk. A run that finds nothing left to move
//! (`done`) is the moment the previous key may be dropped, as far as stored secrets go.
//!
//! The command runs as `norbelys_system`, which bypasses row security: it is the one place that
//! reads every workspace's secrets, and it writes nothing but the same secrets sealed anew.

use std::collections::BTreeMap;

use serde::Serialize;
use uuid::Uuid;

use crate::crypto::{self, CryptoError, Keys};
use crate::db::Database;
use crate::domain::ids::{Id, WorkspaceId};
use crate::identity::{ceremonies, sso, tokens};
use crate::senders::credentials;
use crate::webhooks::endpoints;

/// What one column's walk did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Resealed {
    /// Rows now sealed under the current key.
    pub resealed: u64,
    /// Rows whose bytes changed between the read and the write, left to their writer.
    pub changed: u64,
    /// Rows naming a key this process does not hold, or whose bytes do not open.
    pub unopenable: u64,
}

/// What a run did.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    /// The current deployment key's id, hex: what every re-sealed value now names.
    pub key_id: String,
    /// True when no stored secret was left under another key: none changed meanwhile, none
    /// unopenable.
    pub done: bool,
    /// Per sealed column, `table.column`.
    pub columns: BTreeMap<&'static str, Resealed>,
}

/// A sealed column of a table keyed by workspace and id.
struct TenantColumn {
    /// `table.column`.
    name: &'static str,
    /// The next batch: `$1` the current key id, `$2`, `$3` the last key read, `$4` the batch
    /// size; answers the workspace, the id and the sealed bytes of each row under another key.
    select: &'static str,
    /// The compare and set: `$1`, `$2` the row, `$3` the new bytes, `$4` the bytes read.
    update: &'static str,
    /// The associated data of a row.
    context: fn(WorkspaceId, Uuid) -> String,
}

fn connection_context(workspace: WorkspaceId, id: Uuid) -> String {
    credentials::connection_context(workspace, Id::from_uuid(id))
}

fn webhook_context(workspace: WorkspaceId, id: Uuid) -> String {
    credentials::webhook_context(workspace, Id::from_uuid(id))
}

/// The four tenant columns (see the module).
const TENANT_COLUMNS: [TenantColumn; 4] = [
    TenantColumn {
        name: "connections.credential",
        select: "SELECT workspace_id, id, credential FROM connections
                  WHERE credential IS NOT NULL AND substring(credential from 1 for 4) <> $1
                    AND (workspace_id, id) > ($2, $3)
                  ORDER BY workspace_id, id LIMIT $4",
        update:
            "UPDATE connections SET credential = $3, credential_version = credential_version + 1
                  WHERE workspace_id = $1 AND id = $2 AND credential = $4",
        context: connection_context,
    },
    TenantColumn {
        name: "provider_webhooks.signing_secret",
        select: "SELECT workspace_id, id, signing_secret FROM provider_webhooks
                  WHERE signing_secret IS NOT NULL AND substring(signing_secret from 1 for 4) <> $1
                    AND (workspace_id, id) > ($2, $3)
                  ORDER BY workspace_id, id LIMIT $4",
        update: "UPDATE provider_webhooks SET signing_secret = $3
                  WHERE workspace_id = $1 AND id = $2 AND signing_secret = $4",
        context: webhook_context,
    },
    TenantColumn {
        name: "sso_connections.client_secret",
        select: "SELECT workspace_id, id, client_secret FROM sso_connections
                  WHERE client_secret IS NOT NULL AND substring(client_secret from 1 for 4) <> $1
                    AND (workspace_id, id) > ($2, $3)
                  ORDER BY workspace_id, id LIMIT $4",
        update: "UPDATE sso_connections SET client_secret = $3
                  WHERE workspace_id = $1 AND id = $2 AND client_secret = $4",
        context: sso::secret_context,
    },
    TenantColumn {
        name: "webhook_endpoints.secret",
        select: "SELECT workspace_id, id, secret FROM webhook_endpoints
                  WHERE substring(secret from 1 for 4) <> $1
                    AND (workspace_id, id) > ($2, $3)
                  ORDER BY workspace_id, id LIMIT $4",
        update: "UPDATE webhook_endpoints SET secret = $3
                  WHERE workspace_id = $1 AND id = $2 AND secret = $4",
        context: endpoints::context,
    },
];

/// Re-seals every stored secret under `keys`' current key, `batch` rows per transaction (see the
/// module).
///
/// # Errors
///
/// The database failed, or the random source did while sealing.
pub(crate) async fn rotate(db: &Database, keys: &Keys, batch: u32) -> anyhow::Result<Report> {
    let batch = batch.max(1);
    let mut columns = BTreeMap::new();
    for column in &TENANT_COLUMNS {
        columns.insert(column.name, walk_tenant(db, keys, column, batch).await?);
    }
    columns.insert(
        "signing_keys.private_key",
        walk_signing_keys(db, keys).await?,
    );
    columns.insert(
        "auth_ceremonies.state",
        walk_ceremonies(db, keys, batch).await?,
    );
    let done = columns
        .values()
        .all(|walk| walk.changed == 0 && walk.unopenable == 0);
    Ok(Report {
        key_id: crypto::hex(&keys.key_id()),
        done,
        columns,
    })
}

/// `sealed` opened with the key it names and sealed again under the current key with the same
/// associated data; `None` when it does not open here.
///
/// # Errors
///
/// The random source failed while sealing.
fn reseal(keys: &Keys, sealed: &[u8], context: &str) -> Result<Option<Vec<u8>>, CryptoError> {
    let Ok(plaintext) = keys.open(sealed, context.as_bytes()) else {
        return Ok(None);
    };
    keys.seal(&plaintext, context.as_bytes()).map(Some)
}

/// Counts one row's outcome: re-sealed when the compare and set wrote it, changed otherwise.
fn count(walk: &mut Resealed, written: u64) {
    if written == 0 {
        walk.changed += 1;
    } else {
        walk.resealed += 1;
    }
}

/// Walks one tenant column (see the module).
async fn walk_tenant(
    db: &Database,
    keys: &Keys,
    column: &TenantColumn,
    batch: u32,
) -> anyhow::Result<Resealed> {
    let current = keys.key_id();
    let mut walk = Resealed::default();
    let mut after = (Uuid::nil(), Uuid::nil());
    loop {
        let mut tx = db.begin().await?;
        let rows: Vec<(Uuid, Uuid, Vec<u8>)> = sqlx::query_as(column.select)
            .bind(current.as_slice())
            .bind(after.0)
            .bind(after.1)
            .bind(i64::from(batch))
            .fetch_all(&mut *tx)
            .await?;
        for (workspace, id, sealed) in &rows {
            let context = (column.context)(WorkspaceId::trusted(*workspace), *id);
            let Some(resealed) = reseal(keys, sealed, &context)? else {
                walk.unopenable += 1;
                continue;
            };
            let written = sqlx::query(column.update)
                .bind(workspace)
                .bind(id)
                .bind(&resealed)
                .bind(sealed)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            count(&mut walk, written);
        }
        tx.commit().await?;
        let full = u32::try_from(rows.len()).is_ok_and(|read| read >= batch);
        match rows.last() {
            Some((workspace, id, _)) if full => after = (*workspace, *id),
            _ => return Ok(walk),
        }
    }
}

/// Walks the signing keys, a few dozen rows at most: one transaction.
async fn walk_signing_keys(db: &Database, keys: &Keys) -> anyhow::Result<Resealed> {
    let current = keys.key_id();
    let mut walk = Resealed::default();
    let mut tx = db.begin().await?;
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT kid, private_key FROM signing_keys
          WHERE substring(private_key from 1 for 4) <> $1 ORDER BY kid",
    )
    .bind(current.as_slice())
    .fetch_all(&mut *tx)
    .await?;
    for (kid, sealed) in &rows {
        let Some(resealed) = reseal(keys, sealed, &tokens::context(kid))? else {
            walk.unopenable += 1;
            continue;
        };
        let written = sqlx::query(
            "UPDATE signing_keys SET private_key = $2 WHERE kid = $1 AND private_key = $3",
        )
        .bind(kid)
        .bind(&resealed)
        .bind(sealed)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        count(&mut walk, written);
    }
    tx.commit().await?;
    Ok(walk)
}

/// Walks the live ceremonies, `batch` at a time.
async fn walk_ceremonies(db: &Database, keys: &Keys, batch: u32) -> anyhow::Result<Resealed> {
    let current = keys.key_id();
    let mut walk = Resealed::default();
    let mut after = Uuid::nil();
    loop {
        let mut tx = db.begin().await?;
        let rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT id, state FROM auth_ceremonies
              WHERE expires_at > now() AND consumed_at IS NULL
                AND substring(state from 1 for 4) <> $1 AND id > $2
              ORDER BY id LIMIT $3",
        )
        .bind(current.as_slice())
        .bind(after)
        .bind(i64::from(batch))
        .fetch_all(&mut *tx)
        .await?;
        for (id, sealed) in &rows {
            let context = ceremonies::context(Id::from_uuid(*id));
            let Some(resealed) = reseal(keys, sealed, &context)? else {
                walk.unopenable += 1;
                continue;
            };
            let written =
                sqlx::query("UPDATE auth_ceremonies SET state = $2 WHERE id = $1 AND state = $3")
                    .bind(id)
                    .bind(&resealed)
                    .bind(sealed)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            count(&mut walk, written);
        }
        tx.commit().await?;
        let full = u32::try_from(rows.len()).is_ok_and(|read| read >= batch);
        match rows.last() {
            Some((id, _)) if full => after = *id,
            _ => return Ok(walk),
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use secrecy::{ExposeSecret as _, SecretString};

    use super::{Resealed, rotate};
    use crate::crypto::Keys;
    use crate::senders::credentials::{self, Credential};
    use crate::testing::{self, SenderSpec, TestDb};

    /// The deployment key made of `byte` repeated.
    fn deployment(byte: u8) -> SecretString {
        SecretString::from(STANDARD.encode([byte; 32]))
    }

    /// A rotation re-seals what the previous key sealed, batch by batch, so it opens under the new
    /// key alone with its content and binding unchanged, and a credential's version moves with
    /// it; a row sealed under a key nobody holds is counted and left, so the run is not `done`;
    /// and a second run finds nothing more to move. Without it the previous key could never be
    /// dropped.
    #[tokio::test]
    async fn a_rotation_reseals_every_secret_under_the_new_key() {
        let test = TestDb::new().await;
        let previous = testing::keys();
        test.signing_key().await;
        let acme = test.workspace("acme").await;
        let mut moved = Vec::new();
        for address in [
            "ada@acme.example",
            "grace@acme.example",
            "edsger@acme.example",
        ] {
            let sender = test.sender(acme.id, &SenderSpec::mailbox(address)).await;
            let sealed = credentials::seal(
                &previous,
                acme.id,
                sender.connection,
                &Credential::Password(SecretString::from(format!("password of {address}"))),
            )
            .unwrap();
            sqlx::query(
                "UPDATE connections SET credential = $3 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(acme.id.uuid())
            .bind(sender.connection.uuid())
            .bind(sealed)
            .execute(test.system.pool())
            .await
            .unwrap();
            moved.push((sender.connection, address));
        }
        let stranger = test
            .sender(acme.id, &SenderSpec::mailbox("stranger@acme.example"))
            .await;
        let unknown = Keys::from_deployment_key(&deployment(3)).unwrap();
        let lost = credentials::seal(
            &unknown,
            acme.id,
            stranger.connection,
            &Credential::Password(SecretString::from("lost".to_owned())),
        )
        .unwrap();
        sqlx::query("UPDATE connections SET credential = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(acme.id.uuid())
            .bind(stranger.connection.uuid())
            .bind(lost)
            .execute(test.system.pool())
            .await
            .unwrap();

        let rotated = Keys::from_deployment_key(&deployment(9))
            .unwrap()
            .with_previous(&deployment(7))
            .unwrap();
        let report = rotate(&test.system, &rotated, 1).await.unwrap();
        assert!(!report.done);
        assert_eq!(
            report.columns["connections.credential"],
            Resealed {
                resealed: 3,
                changed: 0,
                unopenable: 1
            }
        );
        assert_eq!(report.columns["signing_keys.private_key"].resealed, 1);

        let new_only = Keys::from_deployment_key(&deployment(9)).unwrap();
        for (connection, address) in moved {
            let (sealed, version): (Vec<u8>, i64) = sqlx::query_as(
                "SELECT credential, credential_version FROM connections WHERE workspace_id = $1 AND id = $2",
            )
            .bind(acme.id.uuid())
            .bind(connection.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
            let Credential::Password(password) =
                credentials::open(&new_only, acme.id, connection, &sealed).unwrap()
            else {
                panic!("a password");
            };
            assert_eq!(password.expose_secret(), format!("password of {address}"));
            assert_eq!(version, 2, "the credential's version moved");
        }
        let again = rotate(&test.system, &rotated, 100).await.unwrap();
        assert!(again.columns.values().all(|walk| walk.resealed == 0));
        assert_eq!(again.columns["connections.credential"].unopenable, 1);
    }
}
