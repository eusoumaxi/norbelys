//! Unsubscribing: what a recipient's click on the unsubscribe link of a campaign message, or their
//! mail provider's one-click request (RFC 8058, <https://www.rfc-editor.org/rfc/rfc8058>), writes.
//!
//! # Who writes it, and with which rights
//!
//! The api serves `/u/{token}`: in its ingress mode on the public host, beside the provider
//! webhooks, and in its product mode where one process serves every surface (a single-host
//! deployment). The tracking role cannot: its login writes tracking events and their rollups
//! only, and an unsubscribe changes what the workspace may send. The api's login writes it in the
//! workspace the signed token names, a workspace trusted the way a credential's is, because only
//! this deployment could have signed it.
//!
//! # What it writes
//!
//! An unsubscribe is evidence about the message, recorded through the one evidence operation
//! (`delivery::evidence::record`): a delivery event (`source = unsubscribe`, `kind =
//! unsubscribed`, authenticated by the token, naming its recipient), the suppression it implies
//! (`reason = unsubscribe`, `source = unsubscribe`, with the event's summary as its evidence),
//! the campaign's `unsubscribed` counter for campaign mail, and the customer's
//! `delivery_event.recorded` and `suppression.created` events. All of it commits in one
//! transaction, before the answer: a suppression committed before the sender's last check of a
//! message (which re-reads suppressions under its row lock) stops that message.
//!
//! # Once
//!
//! A recipient may click twice, and a provider may repeat its request: an address already
//! suppressed writes nothing more, so the event and the counter are written once. Two requests
//! for one address at the same time are ordered by a transaction-scoped advisory lock on the
//! workspace and the address key, so the second sees the first's suppression.

use crate::db::Database;
use crate::delivery::evidence::{self, Evidence};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::policy::delivery::{Category, Confidence, EventKind, RecipientRef, Source};

/// What an unsubscribe request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The address is now suppressed.
    Suppressed,
    /// The address was suppressed already; nothing was written.
    AlreadySuppressed,
}

/// Records that `email`, a recipient of `message`, unsubscribed from `workspace` (see the module).
///
/// # Errors
///
/// The database is unavailable or refused; nothing is written.
pub async fn record(
    db: &Database,
    workspace: WorkspaceId,
    message: Id<Message>,
    email: &EmailAddress,
) -> Result<Outcome, sqlx::Error> {
    let mut tx = db.begin_in(workspace).await?;
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended('unsubscribe:' || $1::uuid::text || ':' || $2, 0))",
        workspace.uuid(),
        email.key(),
    )
    .execute(&mut *tx)
    .await?;
    let suppressed = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM suppressions WHERE workspace_id = $1 AND email_key = $2) AS "exists!""#,
        workspace.uuid(),
        email.key(),
    )
    .fetch_one(&mut *tx)
    .await?;
    if suppressed {
        tx.commit().await?;
        return Ok(Outcome::AlreadySuppressed);
    }
    evidence::record(
        &mut tx,
        workspace,
        &[Evidence {
            message: Some(message),
            thread: None,
            attempt_number: None,
            recipient: Some(email.as_str().to_owned()),
            recipient_ref: RecipientRef::Named,
            source: Source::Unsubscribe,
            source_event_id: format!("unsubscribe:{message}"),
            received_via: None,
            kind: EventKind::Unsubscribed,
            action: None,
            phase: None,
            enhanced_status: None,
            category: Category::Unsubscribed,
            diagnostic: None,
            confidence: Confidence::Authenticated,
            receipt: None,
            observed_at: crate::process::now(),
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(Outcome::Suppressed)
}
