//! Sender identities: the From addresses a connection sends as, with their display name,
//! reply-to address, signature and tags.
//!
//! A person writes an identity's signature once, as HTML (`signature_html`) or as plain text
//! (`signature_text`), or both; when only one is set, the other part of each message derives its
//! signature from it when the message is rendered (`rendering::footer::Signature`).
//!
//! Several identities share one connection's budget and pacing clock, so adding aliases never
//! multiplies capacity. An address is an identity of one **live** connection in a workspace (the
//! database keeps the folded address unique among identities that are not archived), so a
//! message's sender is never ambiguous.
//!
//! Archiving a connection archives its identities with it ([`archive`]): they stay, because its
//! messages and threads point at them, but they no longer hold their addresses, so another
//! account may send as the same address. Connecting the same account again restores them with
//! its row ([`restore`]), unless a live identity holds one of their addresses meanwhile: that
//! restore is refused, naming the live connection, and archiving that one first lets it come
//! back.
//!
//! `verified` says the address may be sent as: confirmed by the provider where it can say (a
//! Gmail mailbox's send-as list, read by `connection.check`; the address an OAuth consent
//! proved), and otherwise the person's attestation when adding the identity.
//!
//! A connection's identities come inside it, at most 50 (the request bodies bound the list); a write replaces the
//! list whole: an identity given with its `id` is replaced by what is given (an absent field is
//! cleared, `enabled` defaults to true, and `verified` is kept while the address stays), one
//! without an `id` is added, one left out is removed. An identity with history (messages, threads) cannot be removed, only disabled, since
//! its history keeps pointing at it.
//!
//! Lock order: the caller holds the connection's row; identities are written after it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::Acquire as _;
use uuid::Uuid;

use super::Error;
use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Connection, Id, SenderIdentity, WorkspaceId};
use crate::domain::time::Timestamp;

/// A sender identity as the API shows it, inside its connection.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct IdentityObject {
    pub id: Id<SenderIdentity>,
    /// The From address.
    pub email: String,
    /// The display name.
    pub name: Option<String>,
    /// The `Reply-To` address.
    pub reply_to: Option<String>,
    /// The signature that ends the HTML part of its mail. Set one or both: when only one is set,
    /// the other part's signature is derived from it.
    pub signature_html: Option<String>,
    /// The signature that ends the text part of its mail. Set one or both: when only one is set,
    /// the other part's signature is derived from it.
    pub signature_text: Option<String>,
    /// Tags a campaign selects senders by.
    pub tags: Vec<String>,
    /// Whether campaigns may send from it.
    pub enabled: bool,
    /// Whether the address may be sent as: confirmed by the provider where it can say, else the
    /// person's word.
    pub verified: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// An identity in a request: without `id` it is added; with the `id` of one of the
/// connection's identities, that one is updated.
#[derive(Debug, Clone, Serialize, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IdentityInput {
    /// The identity to update (`sid_…`); absent for a new one.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    pub id: Option<Id<SenderIdentity>>,
    /// The From address.
    #[garde(skip)]
    #[schema(value_type = String, format = "email")]
    pub email: EmailAddress,
    /// The display name, 1 to 200 characters.
    #[garde(length(chars, min = 1, max = 200))]
    pub name: Option<String>,
    /// The `Reply-To` address.
    #[garde(skip)]
    #[schema(value_type = Option<String>, format = "email")]
    pub reply_to: Option<EmailAddress>,
    /// The signature that ends the HTML part of its mail, at most 64 KiB. Set one or both: when
    /// only one is set, the other part's signature is derived from it (the HTML's plain text, or
    /// the text with its line breaks kept).
    #[garde(length(max = 65_536))]
    pub signature_html: Option<String>,
    /// The signature that ends the text part of its mail, at most 64 KiB. Set one or both: when
    /// only one is set, the other part's signature is derived from it (the HTML's plain text, or
    /// the text with its line breaks kept).
    #[garde(length(max = 65_536))]
    pub signature_text: Option<String>,
    /// Up to 20 tags of 1 to 64 characters.
    #[garde(length(max = 20), inner(inner(length(chars, min = 1, max = 64))))]
    pub tags: Option<Vec<String>>,
    /// Whether campaigns may send from it (default true).
    #[garde(skip)]
    pub enabled: Option<bool>,
    /// The person attests the address may be sent as (default false); a Gmail mailbox's check
    /// replaces it with what Gmail says.
    #[garde(skip)]
    pub verified: Option<bool>,
}

impl IdentityInput {
    /// A new identity for `email` with nothing else set.
    #[must_use]
    pub fn address(email: EmailAddress, verified: bool) -> Self {
        Self {
            id: None,
            email,
            name: None,
            reply_to: None,
            signature_html: None,
            signature_text: None,
            tags: None,
            enabled: None,
            verified: Some(verified),
        }
    }
}

struct Row {
    connection_id: Uuid,
    id: Id<SenderIdentity>,
    email: String,
    name: Option<String>,
    reply_to: Option<String>,
    signature_html: Option<String>,
    signature_text: Option<String>,
    tags: Vec<String>,
    enabled: bool,
    verified_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// The identities of `connections`, by connection, oldest first: one query for a page.
///
/// # Errors
///
/// The database is unavailable.
pub async fn of_connections(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connections: &[Uuid],
) -> Result<HashMap<Uuid, Vec<IdentityObject>>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT connection_id, id AS "id: Id<SenderIdentity>", email, name, reply_to, signature_html, signature_text,
                  tags, enabled, verified_at AS "verified_at: Timestamp",
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM sender_identities WHERE workspace_id = $1 AND connection_id = ANY($2) ORDER BY id"#,
        workspace.uuid(),
        connections,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut by_connection: HashMap<Uuid, Vec<IdentityObject>> = HashMap::new();
    for row in rows {
        by_connection
            .entry(row.connection_id)
            .or_default()
            .push(IdentityObject {
                id: row.id,
                email: row.email,
                name: row.name,
                reply_to: row.reply_to,
                signature_html: row.signature_html,
                signature_text: row.signature_text,
                tags: row.tags,
                enabled: row.enabled,
                verified: row.verified_at.is_some(),
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
    }
    Ok(by_connection)
}

/// Refuses inputs that name one address twice, and addresses that are live identities of another
/// connection of the workspace (naming it).
async fn check_addresses(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    inputs: &[IdentityInput],
) -> Result<(), Error> {
    let keys: Vec<String> = inputs.iter().map(|input| input.email.key()).collect();
    for (index, key) in keys.iter().enumerate() {
        if keys.iter().take(index).any(|earlier| earlier == key) {
            return Err(Error::invalid(
                &format!("/identities/{index}/email"),
                "The address is given twice.",
            ));
        }
    }
    let taken = sqlx::query!(
        r#"SELECT email, connection_id AS "connection_id: Id<Connection>" FROM sender_identities
            WHERE workspace_id = $1 AND email_key = ANY($2) AND connection_id <> $3 AND archived_at IS NULL LIMIT 1"#,
        workspace.uuid(),
        &keys,
        connection.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match taken {
        Some(row) => Err(Error::Conflict(format!(
            "`{}` is already an identity of the connection {}; an address sends through one connection.",
            row.email, row.connection_id
        ))),
        None => Ok(()),
    }
}

async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    input: &IdentityInput,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO sender_identities (workspace_id, connection_id, email, name, reply_to, signature_html, signature_text,
                                        tags, enabled, verified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, CASE WHEN $10 THEN now() END)",
        workspace.uuid(),
        connection.uuid(),
        input.email.as_str(),
        input.name,
        input.reply_to.as_ref().map(EmailAddress::as_str),
        input.signature_html,
        input.signature_text,
        &input.tags.clone().unwrap_or_default(),
        input.enabled.unwrap_or(true),
        input.verified.unwrap_or(false),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Adds the inputs whose address the connection does not have yet, leaving its existing
/// identities as they are: a new connection's list, or what a restored connection gains.
///
/// # Errors
///
/// An address is given twice or belongs to another connection's identity, or the database
/// refused.
pub async fn add(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    inputs: &[IdentityInput],
) -> Result<(), Error> {
    check_addresses(tx, workspace, connection, inputs).await?;
    let existing = sqlx::query_scalar!(
        "SELECT email_key FROM sender_identities WHERE workspace_id = $1 AND connection_id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    for input in inputs {
        if !existing.contains(&input.email.key()) {
            insert(tx, workspace, connection, input).await?;
        }
    }
    Ok(())
}

/// Archives the connection's identities with it: they stay for its history but no longer hold
/// their addresses.
///
/// # Errors
///
/// The database refused.
pub async fn archive(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE sender_identities SET archived_at = now()
          WHERE workspace_id = $1 AND connection_id = $2 AND archived_at IS NULL",
        workspace.uuid(),
        connection.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Restores the identities of a connection being restored, unless a live identity of another
/// connection holds one of their addresses.
///
/// # Errors
///
/// A live identity holds one of the addresses (`Conflict`, naming its connection), or the
/// database refused.
pub async fn restore(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<(), Error> {
    let held = sqlx::query!(
        r#"SELECT live.email, live.connection_id AS "connection_id: Id<Connection>"
             FROM sender_identities archived
             JOIN sender_identities live
               ON live.workspace_id = archived.workspace_id AND live.email_key = archived.email_key
            WHERE archived.workspace_id = $1 AND archived.connection_id = $2 AND archived.archived_at IS NOT NULL
              AND live.archived_at IS NULL AND live.connection_id <> $2
            LIMIT 1"#,
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = held {
        return Err(Error::Conflict(format!(
            "`{}` is now an identity of the live connection {}; archive it first to restore this connection.",
            row.email, row.connection_id
        )));
    }
    sqlx::query!(
        "UPDATE sender_identities SET archived_at = NULL WHERE workspace_id = $1 AND connection_id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Replaces the connection's identities with `inputs`: updates those given with an id, adds
/// those without, removes the rest.
///
/// # Errors
///
/// An id is not one of the connection's identities, an address is given twice or belongs to
/// another connection, an identity to remove has history, or the database refused.
pub async fn replace(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    inputs: &[IdentityInput],
) -> Result<(), Error> {
    let existing: Vec<Id<SenderIdentity>> = sqlx::query_scalar!(
        r#"SELECT id AS "id: Id<SenderIdentity>" FROM sender_identities WHERE workspace_id = $1 AND connection_id = $2"#,
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    for (index, input) in inputs.iter().enumerate() {
        if let Some(id) = input.id
            && !existing.contains(&id)
        {
            return Err(Error::invalid(
                &format!("/identities/{index}/id"),
                "No such identity on this connection.",
            ));
        }
    }
    check_addresses(tx, workspace, connection, inputs).await?;
    let removed: Vec<Uuid> = existing
        .iter()
        .filter(|id| !inputs.iter().any(|input| input.id == Some(**id)))
        .map(|id| id.uuid())
        .collect();
    if !removed.is_empty() {
        // A removed identity with history is refused by its references; the savepoint keeps the
        // transaction usable to answer that.
        let mut savepoint = tx.begin().await?;
        let deleted = sqlx::query!(
            "DELETE FROM sender_identities WHERE workspace_id = $1 AND connection_id = $2 AND id = ANY($3)",
            workspace.uuid(),
            connection.uuid(),
            &removed,
        )
        .execute(&mut *savepoint)
        .await;
        match deleted {
            Ok(_) => savepoint.commit().await?,
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23503") => {
                savepoint.rollback().await?;
                return Err(Error::InvalidState(
                    "An identity left out of the list has sent mail; keep it and set `enabled: false` instead.".to_owned(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    // What the kept identities were, to tell which ones leave the pools that selected them.
    let before = sqlx::query!(
        "SELECT id, tags, enabled FROM sender_identities WHERE workspace_id = $1 AND connection_id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    for input in inputs {
        match input.id {
            None => insert(tx, workspace, connection, input).await?,
            Some(id) => {
                sqlx::query!(
                    "UPDATE sender_identities
                        SET email = $3, name = $4, reply_to = $5, signature_html = $6, signature_text = $7,
                            tags = $8, enabled = $9,
                            verified_at = CASE WHEN $10::boolean IS TRUE AND email_key = ascii_lower($3) THEN coalesce(verified_at, now())
                                               WHEN $10::boolean IS TRUE THEN now()
                                               WHEN $10::boolean IS NULL AND email_key = ascii_lower($3) THEN verified_at END
                      WHERE workspace_id = $1 AND id = $2",
                    workspace.uuid(),
                    id.uuid(),
                    input.email.as_str(),
                    input.name,
                    input.reply_to.as_ref().map(EmailAddress::as_str),
                    input.signature_html,
                    input.signature_text,
                    &input.tags.clone().unwrap_or_default(),
                    input.enabled.unwrap_or(true),
                    input.verified,
                )
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    // An identity disabled, or that lost a tag, leaves the campaign pools that took it (by name
    // or by tag): `senders.removed` applies each campaign's rule to its conversations, and
    // re-reads the pools when it runs, so an identity still in a pool keeps its conversations.
    let leaving: Vec<Id<SenderIdentity>> = inputs
        .iter()
        .filter_map(|input| {
            let id = input.id?;
            let old = before.iter().find(|row| row.id == id.uuid())?;
            let tags = input.tags.clone().unwrap_or_default();
            let disabled = old.enabled && !input.enabled.unwrap_or(true);
            let untagged = old.tags.iter().any(|tag| !tags.contains(tag));
            (disabled || untagged).then_some(id)
        })
        .collect();
    if !leaving.is_empty() {
        crate::campaigns::removal::enqueue(
            tx,
            workspace,
            crate::campaigns::removal::Scope::Identities(leaving),
        )
        .await?;
    }
    Ok(())
}
