//! Suppressions: addresses the workspace never mails again.
//!
//! A suppression excludes one address (by its key, the address in ASCII lowercase) from every
//! message of the workspace, whoever the person is. Only two things create one: evidence that
//! authenticates the refusal and names the recipient (a bounce read in the SMTP session, a
//! signed provider event, a complaint, an unsubscribe), recorded by the modules that read that
//! evidence; or a person, through `POST /suppressions`, whose reason is always `manual`. An
//! address is suppressed once (`UNIQUE (workspace_id, email_key)`), and every creation writes
//! the `suppression.created` event for the customer's webhooks.
//!
//! Every suppression is written by one statement ([`create`] for one address, [`create_all`]
//! for a person's list of up to 1,000): `INSERT … SELECT unnest(…) ON CONFLICT DO NOTHING`, so
//! an address suppressed already, or by a concurrent request, is skipped rather than failing
//! the rest, and the events of what it wrote are recorded together in one more statement.
//!
//! # Lifting
//!
//! Only a `manual` suppression may be removed (the rule and its reason are
//! `domain::suppressions`), and the removal is written to the audit log.

use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::Error;
use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Suppression, WorkspaceId};
use crate::domain::suppressions::{Reason, Source};
use crate::domain::time::Timestamp;
use crate::identity::audit::{self, Action, AuditActor};
use crate::identity::authority::Actor;
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// A suppression as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SuppressionObject {
    pub id: Id<Suppression>,
    /// The address as it was suppressed; it matches its key ignoring ASCII case.
    pub email: String,
    /// Why the address is suppressed. New values may be added.
    #[schema(value_type = Reason)]
    pub reason: String,
    /// Who suppressed it: `manual`, `unsubscribe`, or the source of the evidence. New values may
    /// be added.
    #[schema(value_type = Source)]
    pub source: String,
    /// A summary of the evidence that created it; null for a manual suppression.
    #[schema(value_type = Option<Object>)]
    pub evidence: Option<Value>,
    pub created_at: Timestamp,
}

/// The filters of the suppression list.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct SuppressionFilters {
    /// The suppression of this address (ignoring ASCII case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

/// One page of `workspace`'s suppressions matching `filters`, in id order after `cursor`.
/// Fetches `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &SuppressionFilters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<SuppressionObject>, sqlx::Error> {
    let reason = filters.reason.map(Reason::as_str);
    let source = filters.source.map(Source::as_str);
    // Two statements, one per direction, so each walks the primary key in its own order.
    if ascending {
        sqlx::query_as!(
            SuppressionObject,
            r#"SELECT id AS "id: Id<Suppression>", email, reason, source, evidence, created_at AS "created_at: Timestamp"
                 FROM suppressions
                WHERE workspace_id = $1 AND ($2::uuid IS NULL OR id > $2)
                  AND ($3::text IS NULL OR email_key = ascii_lower($3))
                  AND ($4::text IS NULL OR reason = $4) AND ($5::text IS NULL OR source = $5)
                ORDER BY id LIMIT $6"#,
            workspace.uuid(),
            cursor,
            filters.email,
            reason,
            source,
            limit,
        )
        .fetch_all(&mut **tx)
        .await
    } else {
        sqlx::query_as!(
            SuppressionObject,
            r#"SELECT id AS "id: Id<Suppression>", email, reason, source, evidence, created_at AS "created_at: Timestamp"
                 FROM suppressions
                WHERE workspace_id = $1 AND ($2::uuid IS NULL OR id < $2)
                  AND ($3::text IS NULL OR email_key = ascii_lower($3))
                  AND ($4::text IS NULL OR reason = $4) AND ($5::text IS NULL OR source = $5)
                ORDER BY id DESC LIMIT $6"#,
            workspace.uuid(),
            cursor,
            filters.email,
            reason,
            source,
            limit,
        )
        .fetch_all(&mut **tx)
        .await
    }
}

/// Counts `workspace`'s suppressions matching `filters`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &SuppressionFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM suppressions
                WHERE workspace_id = $1 AND ($2::text IS NULL OR email_key = ascii_lower($2))
                  AND ($3::text IS NULL OR reason = $3) AND ($4::text IS NULL OR source = $4)
                LIMIT $5) counted"#,
        workspace.uuid(),
        filters.email,
        filters.reason.map(Reason::as_str),
        filters.source.map(Source::as_str),
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Reads one suppression of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Suppression>,
) -> Result<Option<SuppressionObject>, sqlx::Error> {
    sqlx::query_as!(
        SuppressionObject,
        r#"SELECT id AS "id: Id<Suppression>", email, reason, source, evidence, created_at AS "created_at: Timestamp"
             FROM suppressions WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await
}

/// A suppression to record.
pub struct NewSuppression<'a> {
    pub email: &'a EmailAddress,
    pub reason: Reason,
    pub source: Source,
    /// The delivery event that proved it, if any.
    pub source_event: Option<Uuid>,
    /// A summary of that evidence, kept with the suppression.
    pub evidence: Option<Value>,
    /// The actor's id, or `system`.
    pub created_by: String,
}

/// Suppresses an address and records the `suppression.created` event, in the caller's
/// transaction.
///
/// # Errors
///
/// [`Error::Conflict`] naming the existing suppression when the address is suppressed already,
/// or the database refused.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    new: &NewSuppression<'_>,
) -> Result<SuppressionObject, Error> {
    let written = insert(
        tx,
        workspace,
        &[new.email.as_str().to_owned()],
        &Shared {
            reason: new.reason,
            source: new.source,
            source_event: new.source_event,
            evidence: new.evidence.as_ref(),
            created_by: &new.created_by,
        },
    )
    .await?;
    let Some(created) = written.into_iter().next() else {
        let existing = sqlx::query_scalar!(
            r#"SELECT id AS "id: Id<Suppression>" FROM suppressions WHERE workspace_id = $1 AND email_key = $2"#,
            workspace.uuid(),
            new.email.key(),
        )
        .fetch_optional(&mut **tx)
        .await?;
        return Err(Error::Conflict(match existing {
            Some(id) => format!("This address is suppressed already ({id})."),
            None => "This address is suppressed already.".to_owned(),
        }));
    };
    Ok(created)
}

/// Suppresses each of `addresses` by hand (reason and source `manual`, created by
/// `created_by`) in the caller's transaction, skipping each address suppressed already, and
/// records `suppression.created` for each suppression written: a person's list, one decision.
/// Returns the suppressions written.
///
/// The rows are written in the order of the addresses' keys, so two lists that overlap wait
/// for each other's rows in one order and never deadlock; an address given twice is written
/// once.
///
/// # Errors
///
/// The database refused.
pub async fn create_all(
    tx: &mut Tx,
    workspace: WorkspaceId,
    addresses: &[EmailAddress],
    created_by: &str,
) -> Result<Vec<SuppressionObject>, sqlx::Error> {
    let mut addresses: Vec<&EmailAddress> = addresses.iter().collect();
    addresses.sort_by_cached_key(|address| address.key());
    let emails: Vec<String> = addresses
        .iter()
        .map(|address| address.as_str().to_owned())
        .collect();
    insert(
        tx,
        workspace,
        &emails,
        &Shared {
            reason: Reason::Manual,
            source: Source::Manual,
            source_event: None,
            evidence: None,
            created_by,
        },
    )
    .await
}

/// What every suppression one statement writes has alike: all but its address.
struct Shared<'a> {
    reason: Reason,
    source: Source,
    source_event: Option<Uuid>,
    evidence: Option<&'a Value>,
    created_by: &'a str,
}

/// Writes a suppression of each of `emails`, in that order, with what `shared` says of all, in
/// one statement; an address suppressed already (by key, or earlier in `emails`) is skipped.
/// Records `suppression.created` for each suppression written, in one more statement, and
/// returns them: the one write of every suppression, whoever makes it.
async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    emails: &[String],
    shared: &Shared<'_>,
) -> Result<Vec<SuppressionObject>, sqlx::Error> {
    let written = sqlx::query_as!(
        SuppressionObject,
        r#"INSERT INTO suppressions (workspace_id, email, reason, source, source_event, evidence, created_by)
           SELECT $1, address.email, $3, $4, $5, $6, $7
             FROM unnest($2::text[]) WITH ORDINALITY AS address(email, position)
            ORDER BY address.position
           ON CONFLICT (workspace_id, email_key) DO NOTHING
           RETURNING id AS "id: Id<Suppression>", email, reason, source, evidence, created_at AS "created_at: Timestamp""#,
        workspace.uuid(),
        emails,
        shared.reason.as_str(),
        shared.source.as_str(),
        shared.source_event,
        shared.evidence,
        shared.created_by,
    )
    .fetch_all(&mut **tx)
    .await?;
    let events: Vec<Event> = written
        .iter()
        .map(|created| Event {
            kind: EventType::SuppressionCreated,
            subject_type: "suppression",
            subject_id: created.id.uuid(),
            data: json!({ "suppression_id": created.id, "reason": created.reason, "source": created.source }),
        })
        .collect();
    outbox::record_all(tx, workspace, &events).await?;
    Ok(written)
}

/// Lifts a `manual` suppression and writes the removal to the audit log with `actor`.
///
/// # Errors
///
/// [`Error::NotFound`], [`Error::InvalidState`] for any other reason, or the database refused.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Suppression>,
    actor: Actor,
) -> Result<(), Error> {
    let row = sqlx::query!(
        "SELECT email, reason FROM suppressions WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("suppression"))?;
    let liftable = row.reason.parse::<Reason>().is_ok_and(Reason::liftable);
    if !liftable {
        return Err(Error::InvalidState(format!(
            "A `{}` suppression records what the recipient or their provider said and cannot be removed; only a manual one can.",
            row.reason
        )));
    }
    sqlx::query!(
        "DELETE FROM suppressions WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    audit::record(
        tx,
        workspace,
        AuditActor::from(actor),
        Action::SuppressionDeleted,
        Some(id.to_string()),
        json!({ "email": row.email, "reason": row.reason }),
        None,
    )
    .await?;
    Ok(())
}
