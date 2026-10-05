//! Correlation: which of our threads, and which of our outbound messages, an inbound message
//! concerns.
//!
//! # Two ways an id is ours
//!
//! 1. **Our own Message-ID** (`<{message}.{thread}.{tag}@{domain}>`): the tag is an HMAC of the
//!    message and thread ids under the deployment's key, so a verified tag proves the id was
//!    written by us, and the thread it names is read directly, without any lookup table. This is
//!    how a reply's `In-Reply-To` or `References`, and a report's returned `Message-ID`, find
//!    their thread whatever happened to the message's own row since.
//! 2. **The directory** (`message_id_directory`): a provider that replaces our Message-ID (Amazon
//!    SES) hands back its own token, which the finish of the submission recorded with the
//!    message's thread and original envelope. An id is looked up whole and by its local part
//!    (the text before `@`, the token itself). Such an id carries no signature, so a row
//!    correlates only mail read through a binding of the thread identity's own connection, and
//!    only when the mail's sender (or the recipient a report names) is one of the original
//!    envelope's addresses; anything else stays unmatched for review, never guessed.
//!
//! Ids are tried in the order the mail gives them: `In-Reply-To` first, then `References` from
//! the newest, so the closest ancestor decides. A thread of another workspace is never found:
//! every read runs inside the binding's workspace.

use uuid::Uuid;

use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::accept;
use crate::domain::ids::{Campaign, Id, Message, Person, Thread, WorkspaceId};
use crate::domain::inbox::directory_accepts;

/// What correlation knows of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadRef {
    /// The thread.
    pub id: Id<Thread>,
    /// The person it is with, when known.
    pub person: Option<Id<Person>>,
    /// The campaign that opened it, for campaign mail.
    pub campaign: Option<Id<Campaign>>,
    /// The connection of the thread's sender identity.
    pub connection: Uuid,
}

/// How an id was found to be ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// Our Message-ID, its tag verified.
    Tag,
    /// A provider's id our directory holds.
    Directory,
}

/// What an inbound message concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The thread.
    pub thread: ThreadRef,
    /// The outbound message the id names, when its row still exists (an archived message is
    /// named by the thread alone).
    pub message: Option<Id<Message>>,
    /// The connection that sent that message, when known.
    pub sent_through: Option<Uuid>,
    /// That message's envelope (`To`, `Cc`, `Bcc`), when known.
    pub envelope: Vec<String>,
    /// How the id was found to be ours.
    pub via: Via,
}

/// The first of `ids` (angle brackets or not) that names one of our messages in `workspace`, as
/// read through a binding of `binding_connection`; `address` is the mail's sender, or the
/// recipient a report names, which a directory match must find in the original envelope.
///
/// # Errors
///
/// The database refused.
pub async fn find(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    binding_connection: Uuid,
    address: Option<&str>,
    ids: &[&str],
) -> Result<Option<Found>, sqlx::Error> {
    for id in ids {
        if let Some((message, thread)) = accept::correlate(keys, id) {
            let Some(thread) = thread_ref(tx, workspace, thread).await? else {
                continue;
            };
            let row = sqlx::query!(
                r#"SELECT connection_id, to_addresses || cc || bcc AS "envelope!"
                     FROM messages WHERE workspace_id = $1 AND id = $2"#,
                workspace.uuid(),
                message.uuid(),
            )
            .fetch_optional(&mut **tx)
            .await?;
            return Ok(Some(Found {
                thread,
                message: row.as_ref().map(|_| message),
                sent_through: row.as_ref().map(|row| row.connection_id),
                envelope: row.map(|row| row.envelope).unwrap_or_default(),
                via: Via::Tag,
            }));
        }
        if let Some(found) = directory(tx, workspace, binding_connection, address, id).await? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// A thread of `workspace`, with its identity's connection.
async fn thread_ref(
    tx: &mut Tx,
    workspace: WorkspaceId,
    thread: Id<Thread>,
) -> Result<Option<ThreadRef>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT t.person_id AS "person_id: Id<Person>", t.campaign_id AS "campaign_id: Id<Campaign>",
                  i.connection_id
             FROM threads t JOIN sender_identities i ON i.workspace_id = t.workspace_id AND i.id = t.sender_identity_id
            WHERE t.workspace_id = $1 AND t.id = $2"#,
        workspace.uuid(),
        thread.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| ThreadRef {
        id: thread,
        person: row.person_id,
        campaign: row.campaign_id,
        connection: row.connection_id,
    }))
}

/// The directory row `id` names, whole or by its local part, when it may correlate this mail.
async fn directory(
    tx: &mut Tx,
    workspace: WorkspaceId,
    binding_connection: Uuid,
    address: Option<&str>,
    id: &str,
) -> Result<Option<Found>, sqlx::Error> {
    let whole = id.trim().trim_start_matches('<').trim_end_matches('>');
    let local = whole.split_once('@').map_or(whole, |(local, _)| local);
    if local.is_empty() {
        return Ok(None);
    }
    let rows = sqlx::query!(
        r#"SELECT d.message_id AS "message_id: Id<Message>", d.thread_id AS "thread_id: Id<Thread>", d.recipients,
                  EXISTS (SELECT 1 FROM messages m WHERE m.workspace_id = d.workspace_id AND m.id = d.message_id) AS "exists!"
             FROM message_id_directory d
            WHERE d.workspace_id = $1 AND d.lookup_key IN ($2, $3)
            ORDER BY d.id DESC
            LIMIT 5"#,
        workspace.uuid(),
        whole,
        local,
    )
    .fetch_all(&mut **tx)
    .await?;
    for row in rows {
        let Some(thread) = thread_ref(tx, workspace, row.thread_id).await? else {
            continue;
        };
        if directory_accepts(
            binding_connection,
            thread.connection,
            address,
            &row.recipients,
        ) {
            return Ok(Some(Found {
                thread,
                message: row.exists.then_some(row.message_id),
                sent_through: Some(thread.connection),
                envelope: row.recipients,
                via: Via::Directory,
            }));
        }
    }
    Ok(None)
}
