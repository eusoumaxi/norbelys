//! Optional bulk recipient checks before campaign messages are created.
//!
//! Reuses Address check's syntax, DNS and remote SMTP checks. Each batch reads and commits,
//! checks the network without an open transaction, then saves with the job's fenced checkpoint.
//! Results belong to the enrollment and its current email: a restart resumes unchecked rows,
//! and an address change makes its old finding inapplicable. Invalid findings end only that
//! enrollment; they are never bounce evidence or workspace-wide suppressions.
//!
//! Both message creation paths also check readiness under the campaign lock, which enrollment
//! takes too. The gate includes future first-step recipients and unfinished audience imports.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::db::{Database, Tx};
use crate::delivery::preflight;
use crate::delivery::validation::{BATCH, MailboxFinding, MailboxStatus, Validation};
use crate::domain::campaigns::EnrollmentStatus;
use crate::domain::ids::{Campaign, Id, WorkspaceId};
use crate::jobs::{self, JobContext, JobError};

/// The same configured client as Address check. Job harnesses may omit the optional dependency.
pub(crate) fn validator(cx: &JobContext) -> Validation {
    cx.env::<crate::senders::Env>()
        .map(|env| env.settings.recipient_validation.clone())
        .unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct Recipient {
    id: Uuid,
    email: String,
    email_key: String,
}

/// One predicate for both the bulk reader and the gate: a future send is still part of the
/// audience. Suppressed recipients and test workspaces never need a network check.
async fn unchecked(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    limit: i64,
) -> Result<Vec<Recipient>, sqlx::Error> {
    sqlx::query_as(
        "SELECT e.id, p.email, p.email_key FROM enrollments e
         JOIN people p ON p.workspace_id = e.workspace_id AND p.id = e.person_id
         JOIN campaigns c ON c.workspace_id = e.workspace_id AND c.id = e.campaign_id
         JOIN workspaces w ON w.id = e.workspace_id
          WHERE e.workspace_id = $1 AND e.campaign_id = $2 AND w.mode = 'live'
            AND c.status IN ('materialising', 'active')
            AND e.status = 'active' AND e.current_position = 1 AND e.message_id IS NULL
            AND e.recipient_validation ->> 'email_key' IS DISTINCT FROM p.email_key
            AND NOT EXISTS (SELECT 1 FROM suppressions s
                             WHERE s.workspace_id = e.workspace_id AND s.email_key = p.email_key)
          ORDER BY e.id LIMIT $3",
    )
    .bind(workspace.uuid())
    .bind(campaign.uuid())
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
}

/// Caller holds the campaign lock. Unknown and skipped findings count as checked; a temporary
/// lack of capacity does not. An audience still being enrolled must finish before sending.
pub(crate) async fn ready(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<bool, sqlx::Error> {
    if !unchecked(tx, workspace, campaign, 1).await?.is_empty() {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT NOT EXISTS (
             SELECT 1 FROM jobs j JOIN workspaces w ON w.id = j.workspace_id
              WHERE j.workspace_id = $1 AND w.mode = 'live'
                AND j.kind = 'enrollment.add' AND j.state IN ('available', 'running')
                AND j.payload ->> 'campaign' = $2::uuid::text)",
    )
    .bind(workspace.uuid())
    .bind(campaign.uuid())
    .fetch_one(&mut **tx)
    .await
}

/// Creation can discover a changed address or newly enrolled audience after materialisation.
pub(crate) async fn ready_or_enqueue(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<bool, sqlx::Error> {
    if ready(tx, workspace, campaign).await? {
        return Ok(true);
    }
    jobs::enqueue(
        tx,
        workspace,
        &super::materialise::CampaignMaterialise {
            campaign: campaign.uuid(),
        },
        None,
    )
    .await?;
    Ok(false)
}

async fn pending(
    db: &Database,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<Vec<Recipient>, sqlx::Error> {
    let mut tx = db.begin_in(workspace).await?;
    let rows = unchecked(
        &mut tx,
        workspace,
        campaign,
        i64::try_from(BATCH).unwrap_or(8),
    )
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Runs bounded batches until every address is checked, or the job should yield. `false` asks
/// materialisation to yield without activating the campaign or creating messages.
pub(crate) async fn prepare(cx: &mut JobContext, campaign: Id<Campaign>) -> Result<bool, JobError> {
    let validation = validator(cx);
    if !validation.enabled() {
        return Ok(true);
    }
    let resolver = cx.env::<crate::senders::Env>()?.resolver.clone();
    loop {
        if cx.should_yield() {
            return Ok(false);
        }
        let workspace = cx.workspace();
        let recipients = pending(cx.db(), workspace, campaign).await?;
        if recipients.is_empty() {
            return Ok(true);
        }
        let emails = recipients
            .iter()
            .map(|row| row.email.clone())
            .collect::<Vec<_>>();
        let mut findings = preflight::check(cx.db(), &resolver, workspace, &emails, false).await?;
        if validation.preflight(&mut findings, false).await.is_err() {
            return Ok(false);
        }
        let checked = recipients
            .into_iter()
            .zip(findings.into_iter().map(result))
            .collect::<Vec<_>>();
        let mut chunk = cx.begin().await?;
        save(chunk.tx(), workspace, campaign, &checked).await?;
        // Preserve the creator's cursor if a resumed job was already creating messages.
        let mut progress = cx
            .progress()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        progress.insert("recipient_validation".to_owned(), json!({
            "checked": checked.len(),
            "invalid": checked.iter().filter(|(_, finding)| finding.status == MailboxStatus::Invalid).count(),
        }));
        cx.checkpoint(chunk, Value::Object(progress)).await?;
    }
}

/// Store the effective result, retaining DNS refusals even when SMTP could not be consulted.
fn result(finding: preflight::Finding) -> MailboxFinding {
    match finding.status {
        "invalid" | "unknown" => MailboxFinding {
            status: if finding.status == "invalid" {
                MailboxStatus::Invalid
            } else {
                MailboxStatus::Unknown
            },
            detail: finding
                .detail
                .unwrap_or_else(|| format!("DNS preflight: {}", finding.reason)),
        },
        _ => finding
            .smtp
            .unwrap_or_else(|| MailboxFinding::skipped("SMTP was not checked.")),
    }
}

/// The campaign lock serializes with creation and enrollment. Lock people too so an email
/// cannot change between the conditional write and ending an explicitly invalid enrollment.
async fn save(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    checked: &[(Recipient, MailboxFinding)],
) -> Result<(), sqlx::Error> {
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM campaigns WHERE workspace_id = $1 AND id = $2
          AND status IN ('materialising', 'active') FOR UPDATE",
    )
    .bind(workspace.uuid())
    .bind(campaign.uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if active.is_none() {
        return Ok(());
    }
    let enrollments = checked
        .iter()
        .map(|(recipient, _)| recipient.id)
        .collect::<Vec<_>>();
    sqlx::query(
        "SELECT p.id FROM people p JOIN enrollments e ON e.workspace_id = p.workspace_id AND e.person_id = p.id
          WHERE e.workspace_id = $1 AND e.campaign_id = $2 AND e.id = ANY($3) ORDER BY p.id FOR SHARE OF p",
    )
    .bind(workspace.uuid()).bind(campaign.uuid()).bind(&enrollments).fetch_all(&mut **tx).await?;
    for (recipient, finding) in checked {
        let result = json!({
            "email_key": recipient.email_key,
            "status": finding.status,
            "checked_at": crate::process::now(),
            "detail": finding.detail,
        });
        let stored = sqlx::query(
            "UPDATE enrollments e SET recipient_validation = $4
               FROM people p
              WHERE e.workspace_id = $1 AND e.campaign_id = $2 AND e.id = $3
                AND p.workspace_id = e.workspace_id AND p.id = e.person_id AND p.email_key = $5
                AND e.status = 'active' AND e.current_position = 1 AND e.message_id IS NULL
                AND e.recipient_validation ->> 'email_key' IS DISTINCT FROM p.email_key",
        )
        .bind(workspace.uuid())
        .bind(campaign.uuid())
        .bind(recipient.id)
        .bind(result)
        .bind(&recipient.email_key)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if stored > 0 && finding.status == MailboxStatus::Invalid {
            let detail = format!("Recipient validation: {}", finding.detail);
            super::enrollments::end(
                tx,
                workspace,
                &[recipient.id],
                EnrollmentStatus::Failed,
                Some(&detail),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
