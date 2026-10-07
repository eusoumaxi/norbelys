//! `campaign.materialise`: a started campaign becomes `active`, and the messages its people are
//! already due get created now rather than at the next 5-minute pass.
//!
//! `POST /campaigns/{id}/start` makes the campaign `materialising` and enqueues this job, unique
//! per campaign. The job checks the campaign can send (every step offers a variant: a step
//! removed meanwhile could leave none) and makes it `active`, telling
//! `campaign.status_changed`; a campaign that cannot send goes back to `draft` (or `paused`, if
//! it sent before) with `last_error` saying why. Then it runs the creation pass over the
//! campaign's due enrollments in chunks, each chunk committed with its checkpoint (the cursor
//! it reached), so a run that yields or crashes continues where it was and never creates a
//! message twice: the creation contract creates one message per step whatever the replays.
//!
//! Enrolling people into an `active` campaign enqueues it too (coalescing with one queued), so
//! their first messages exist within seconds; the job then only creates.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::creator::{self, CHUNK, Cursor};
use super::steps;
use crate::crypto::Keys;
use crate::domain::campaigns::CampaignStatus;
use crate::domain::ids::{Campaign, Id};
use crate::domain::time::Timestamp;
use crate::jobs::{self, Effect, Job, JobContext, JobError, Outcome, Queue};

/// `campaign.materialise`: see the module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignMaterialise {
    /// The campaign.
    pub campaign: Uuid,
}

impl Job for CampaignMaterialise {
    const KIND: &'static str = "campaign.materialise";
    const QUEUE: Queue = Queue::Enrollment;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.campaign.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let campaign: Id<Campaign> = Id::from_uuid(self.campaign);
        let keys = cx.env::<Keys>()?.clone();
        let validation_required = super::recipients::validator(cx).enabled();
        if !super::recipients::prepare(cx, campaign).await? {
            return Ok(Outcome::Yield {
                after: Duration::from_secs(1),
            });
        }
        let mut cursor: Option<Cursor> = cx.progress().and_then(|progress| {
            let at = progress.get("at")?.as_str()?.parse().ok()?;
            let id = progress.get("id")?.as_str()?.parse().ok()?;
            Some((Timestamp(at), id))
        });
        if cursor.is_none() {
            let mut chunk = cx.begin().await?;
            let status = sqlx::query_scalar!(
                "SELECT status FROM campaigns WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
                workspace.uuid(),
                campaign.uuid(),
            )
            .fetch_optional(&mut **chunk.tx())
            .await?
            .and_then(|status| status.parse::<CampaignStatus>().ok());
            // Enrollment takes this lock too, so activation cannot race with new addresses.
            if validation_required
                && matches!(
                    status,
                    Some(CampaignStatus::Materialising | CampaignStatus::Active)
                )
                && !super::recipients::ready(chunk.tx(), workspace, campaign).await?
            {
                return Ok(Outcome::Yield {
                    after: Duration::from_secs(1),
                });
            }
            match status {
                Some(CampaignStatus::Materialising) => {
                    if steps::sendable(chunk.tx(), workspace, campaign).await? {
                        super::set_status(chunk.tx(), workspace, campaign, CampaignStatus::Active)
                            .await?;
                    } else {
                        let sent = sqlx::query_scalar!(
                            r#"SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND campaign_id = $2) AS "sent!""#,
                            workspace.uuid(),
                            campaign.uuid(),
                        )
                        .fetch_one(&mut **chunk.tx())
                        .await?;
                        let back = if sent {
                            CampaignStatus::Paused
                        } else {
                            CampaignStatus::Draft
                        };
                        super::set_status(chunk.tx(), workspace, campaign, back).await?;
                        sqlx::query!(
                            "UPDATE campaigns SET last_error = $3 WHERE workspace_id = $1 AND id = $2",
                            workspace.uuid(),
                            campaign.uuid(),
                            json!({
                                "code": "invalid",
                                "detail": "Every step needs at least one variant before the campaign starts.",
                                "at": crate::process::now(),
                            }),
                        )
                        .execute(&mut **chunk.tx())
                        .await?;
                        cx.checkpoint(chunk, json!({ "started": false })).await?;
                        return Ok(Outcome::Done);
                    }
                }
                Some(CampaignStatus::Active) => {}
                _ => {
                    cx.checkpoint(chunk, json!({ "started": false })).await?;
                    return Ok(Outcome::Done);
                }
            }
            cx.checkpoint(chunk, json!({ "started": true })).await?;
        }
        loop {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let mut chunk = cx.begin().await?;
            let pass = creator::pass(
                chunk.tx(),
                &keys,
                workspace,
                Some(campaign),
                jiff::Timestamp::now(),
                cursor,
                CHUNK,
                validation_required,
            )
            .await?;
            cursor = pass.cursor;
            cx.checkpoint(
                chunk,
                json!({
                    "at": cursor.map(|(at, _)| at),
                    "id": cursor.map(|(_, id)| id),
                    "created": pass.created,
                }),
            )
            .await?;
            if pass.created > 0 {
                crate::delivery::accept::wake(cx.db()).await;
            }
            if pass.generating > 0 {
                jobs::wake(cx.db(), Queue::Ai).await;
            }
            if cursor.is_none() {
                return Ok(Outcome::Done);
            }
        }
    }
}
