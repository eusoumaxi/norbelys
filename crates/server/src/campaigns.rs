//! Campaigns: an audience enrolled person by person, an ordered list of steps with their
//! variants, a pool of sender identities and a send window; and the background work that turns
//! them into messages.
//!
//! # What lives here
//!
//! - The `campaigns` resource ([`create`], [`read`], [`list`], [`update`], [`delete`],
//!   [`start`], [`pause`]); its steps, variants and revisions ([`steps`]).
//! - The `enrollments` resource and the stop rules ([`enrollments`]), including
//!   [`enrollments::stop_for_reply`], which the inbox calls when a person answers.
//! - The creation of step messages ([`creator`]): the one pass that picks a sender
//!   from the pool, a variant by the step's allocation, and creates the message through the
//!   delivery's creation contract (`delivery::accept::step`). Its callers are the job kinds
//!   `campaign.materialise` ([`materialise`]), `enrollment.advance` ([`advance`]) and
//!   `message.generate` ([`generate`]).
//! - Losing a sender: `senders.removed` ([`removal`]).
//!
//! # Design
//!
//! A campaign is configuration; an enrollment is one person's progress through it (a position,
//! a due time, the message of the current step). Nothing is precomputed for an audience: a
//! step's message is created when the step falls due (within one slot of the 5-minute grid),
//! from the step's current revision, so an edit reaches every message not yet created and never
//! one already created. Messages never hold a campaign body: they point at the exact variant
//! version they were made from.
//!
//! The pool is read when a sender is assigned, never copied: the identities the campaign names
//! plus every enabled identity carrying one of its tags. A conversation keeps its sender (its
//! affinity) for its follow-ups; what happens when the sender leaves the pool is the campaign's
//! `on_sender_removed` (`domain::campaigns`).
//!
//! # Versions
//!
//! A campaign carries `version` (see `http::versioning`). Its object shows rows of other tables
//! (steps, variants, revisions, the named identities, a revision's winner), so every write of
//! those rows also updates the campaign's row in the same transaction: an update does it with
//! its own `UPDATE`, and the background writes (a winner chosen automatically, `last_error`)
//! update the row they hold locked. Counters (`stats`) and the enrollment summary of a campaign
//! answered alone (`enrollments`, [`enrollments::summary`]) are computed when read and are not
//! part of the version.
//!
//! # Lock order
//!
//! The campaign's row first (an update, `start`, `pause`, `delete`, and every creation pass,
//! which serialises the pool's rotation on it), then enrollment rows in id order, then threads,
//! queue rows and messages, as every delivery path orders them. A path that does not need the
//! campaign's row (stopping an enrollment, settling a finished message, a removal) starts at
//! the enrollments and never takes the campaign's row after them.

pub mod advance;
pub mod creator;
pub mod enrollments;
pub mod generate;
pub mod http;
pub mod materialise;
mod recipients;
pub mod removal;
#[cfg(test)]
mod rotation_tests;
pub mod steps;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use uuid::Uuid;

use self::enrollments::EnrollmentSummary;
use self::steps::{StepInput, StepObject};
use crate::db::Tx;
use crate::domain::campaigns::{CampaignStatus, OnSenderRemoved, StopOnReply};
use crate::domain::ids::{Campaign, Id, SenderIdentity, SendingDomain, WorkspaceId};
use crate::domain::senders::SendWindow;
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::jobs;
use crate::problem::Problem;
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// Why a campaign operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A referenced resource does not exist in the workspace (`404`).
    #[error("no such {0}")]
    NotFound(&'static str),
    /// A field breaks a rule, at its JSON pointer (`422 validation_failed`).
    #[error("{pointer}: {detail}")]
    Invalid { pointer: String, detail: String },
    /// The resource's state forbids it (`409 invalid_state`).
    #[error("{0}")]
    InvalidState(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl Error {
    /// A field error at `pointer`.
    pub(crate) fn invalid(pointer: &str, detail: impl Into<String>) -> Self {
        Self::Invalid {
            pointer: pointer.to_owned(),
            detail: detail.into(),
        }
    }
}

impl From<Error> for Problem {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(what) => Problem::not_found(what),
            Error::Invalid { pointer, detail } => {
                Problem::invalid_field(&pointer, "invalid", detail)
            }
            Error::InvalidState(detail) => Problem::invalid_state(detail),
            Error::Db(error) => Problem::from(error),
        }
    }
}

/// Reads a member that may be absent (`None`), `null` (`Some(None)`) or a value, so an update
/// can tell "keep" from "clear".
pub(crate) fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A campaign as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct CampaignObject {
    pub id: Id<Campaign>,
    pub name: String,
    pub status: CampaignStatus,
    /// Why the campaign cannot send right now, while that lasts: `no_sender` (no sender of the
    /// pool is usable; its conversations wait and continue by themselves), `invalid` (it could
    /// not start).
    pub last_error: Option<LastError>,
    /// The steps in order, with their variants (a list leaves out the variants' bodies).
    #[schema(max_items = 50)]
    pub steps: Vec<StepObject>,
    pub senders: SendersObject,
    pub schedule: ScheduleObject,
    pub tracking: TrackingObject,
    pub stop_rules: StopRulesObject,
    pub stats: StatsObject,
    /// Where the campaign's people are now and when its next email may go, counted whenever one
    /// campaign is answered (retrieved, created, updated, started, paused, or archived by a
    /// deletion); `null` in a list. Like `stats`, it is not part of `version`.
    pub enrollments: Option<EnrollmentSummary>,
    /// The version an update's `If-Match` names.
    pub version: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What stops a campaign from sending.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = CampaignLastError)]
pub struct LastError {
    /// `no_sender` or `invalid`. New codes may be added.
    pub code: String,
    pub detail: String,
    pub at: Timestamp,
}

/// The campaign's pool of senders.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SendersObject {
    /// The identities the campaign names (`sid_…`).
    pub identity_ids: Vec<Id<SenderIdentity>>,
    /// Every enabled identity carrying one of these tags joins the pool, as it is when a sender
    /// is assigned.
    pub tags: Vec<String>,
    /// What happens to a conversation whose sender leaves the pool: `reassign` (another sender
    /// of the pool takes the next step, in a new thread) or `stop`.
    pub on_sender_removed: OnSenderRemoved,
}

/// When the campaign sends.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ScheduleObject {
    /// The IANA time zone of the send window.
    pub timezone: String,
    /// The days and hours campaign mail may be sent; `null`: any time.
    pub send_window: Option<SendWindow>,
    /// No step runs before this instant.
    pub start_at: Option<Timestamp>,
}

/// What the campaign's messages track.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TrackingObject {
    /// The sending domain whose tracking host the links use (`dom_…`); `null`: the platform's.
    pub domain_id: Option<Id<SendingDomain>>,
    pub opens: bool,
    pub clicks: bool,
}

/// When an enrollment stops before its last step.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct StopRulesObject {
    /// Whose enrollments a person's reply stops: `all` (the person's live enrollments in every
    /// campaign), `campaign` (this one only) or `none`.
    pub on_reply: StopOnReply,
    /// Whether a reply also stops this campaign's enrollments of people at the same email
    /// domain (the same company).
    pub company_on_reply: bool,
    /// A new enrollment's first step waits until this many hours have passed since the person's
    /// last campaign message from the workspace.
    pub cooldown_hours: i32,
}

/// The campaign's counters, from the analytics rollup.
#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema)]
pub struct StatsObject {
    pub sent: i64,
    pub opened: i64,
    pub clicked: i64,
    pub replied: i64,
    pub bounced: i64,
    pub unsubscribed: i64,
    /// How far the rollup had counted when these were read; `null` before its first run.
    pub computed_at: Option<Timestamp>,
}

/// The settings a create or an update gives; `None` keeps the current value (or the default, on
/// create). `Some(None)` clears a nullable one.
#[derive(Debug, Default)]
pub struct Changes {
    pub name: Option<String>,
    pub identity_ids: Option<Vec<Id<SenderIdentity>>>,
    pub tags: Option<Vec<String>>,
    pub on_sender_removed: Option<OnSenderRemoved>,
    pub timezone: Option<String>,
    pub send_window: Option<Option<SendWindow>>,
    pub start_at: Option<Option<Timestamp>>,
    pub tracking_domain: Option<Option<Id<SendingDomain>>>,
    pub track_opens: Option<bool>,
    pub track_clicks: Option<bool>,
    pub stop_on_reply: Option<StopOnReply>,
    pub stop_company_on_reply: Option<bool>,
    pub cooldown_hours: Option<i32>,
    /// The whole ordered list of steps (see [`steps`]).
    pub steps: Option<Vec<StepInput>>,
}

/// The filters of the campaign list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CampaignFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CampaignStatus>,
    /// A prefix of the name, ignoring case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

/// A campaign's row as read.
struct Row {
    id: Id<Campaign>,
    name: String,
    status: String,
    last_error: Option<serde_json::Value>,
    timezone: String,
    send_window: Option<serde_json::Value>,
    start_at: Option<Timestamp>,
    tracking_domain_id: Option<Id<SendingDomain>>,
    track_opens: bool,
    track_clicks: bool,
    stop_on_reply: String,
    sender_tags: Vec<String>,
    on_sender_removed: String,
    stop_company_on_reply: bool,
    cooldown_hours: i32,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// A campaign's row, locked for a change: what the change checks before writing.
#[derive(Debug, Clone)]
pub struct Locked {
    pub id: Id<Campaign>,
    pub status: CampaignStatus,
    pub tags: Vec<String>,
    /// The version `If-Match` is checked against.
    pub version: i64,
}

/// Reads one campaign, its variants' bodies and its enrollment summary included.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Campaign>,
) -> Result<Option<CampaignObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<Campaign>", name, status, last_error, timezone, send_window,
                  start_at AS "start_at: Timestamp", tracking_domain_id AS "tracking_domain_id: Id<SendingDomain>",
                  track_opens, track_clicks, stop_on_reply, sender_tags, on_sender_removed, stop_company_on_reply,
                  cooldown_hours, created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM campaigns WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let Some(mut campaign) = objects(tx, workspace, vec![row], true).await?.pop() else {
        return Ok(None);
    };
    campaign.enrollments = Some(enrollments::summary(tx, workspace, &campaign).await?);
    Ok(Some(campaign))
}

/// One page of the workspace's campaigns, id-descending by default, `limit` rows after the
/// cursor; the variants' bodies and the enrollment summaries are left out.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &CampaignFilters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<CampaignObject>, sqlx::Error> {
    let prefix = filters.q.as_deref().and_then(crate::people::like_prefix);
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<Campaign>", name, status, last_error, timezone, send_window,
                  start_at AS "start_at: Timestamp", tracking_domain_id AS "tracking_domain_id: Id<SendingDomain>",
                  track_opens, track_clicks, stop_on_reply, sender_tags, on_sender_removed, stop_company_on_reply,
                  cooldown_hours, created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM campaigns
            WHERE workspace_id = $1
              AND ($2::text IS NULL OR status = $2)
              AND ($3::text IS NULL OR lower(name) LIKE $3)
              AND ($4::uuid IS NULL OR CASE WHEN $5 THEN id > $4 ELSE id < $4 END)
            ORDER BY CASE WHEN $5 THEN id END, id DESC
            LIMIT $6"#,
        workspace.uuid(),
        filters.status.map(CampaignStatus::as_str),
        prefix,
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    objects(tx, workspace, rows, false).await
}

/// How many campaigns match `filters`, counting at most `cap + 1` (so a capped count is told from an exact one).
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &CampaignFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    let prefix = filters.q.as_deref().and_then(crate::people::like_prefix);
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
             SELECT 1 FROM campaigns
              WHERE workspace_id = $1 AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR lower(name) LIKE $3)
              LIMIT $4) capped"#,
        workspace.uuid(),
        filters.status.map(CampaignStatus::as_str),
        prefix,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// The objects of `rows`: their steps, named identities and counters read in one query each;
/// the enrollment summary is left for [`read`] to add.
async fn objects(
    tx: &mut Tx,
    workspace: WorkspaceId,
    rows: Vec<Row>,
    bodies: bool,
) -> Result<Vec<CampaignObject>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let mut steps = steps::of_campaigns(tx, workspace, &ids, bodies).await?;
    let mut named: HashMap<Uuid, Vec<Id<SenderIdentity>>> = HashMap::new();
    for row in sqlx::query!(
        r#"SELECT campaign_id, sender_identity_id AS "id: Id<SenderIdentity>"
             FROM campaign_senders WHERE workspace_id = $1 AND campaign_id = ANY($2)
            ORDER BY campaign_id, sender_identity_id"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    {
        named.entry(row.campaign_id).or_default().push(row.id);
    }
    // The counters come from the one reader of the rolled-up stats, with how far the rollup had
    // counted when they were read.
    let (counters, computed_at) = crate::analytics::campaign_stats(tx, workspace, &ids).await?;
    let mut stats: HashMap<Uuid, StatsObject> = counters
        .into_iter()
        .map(|(campaign, counters)| {
            (
                campaign,
                StatsObject {
                    sent: counters.sent,
                    opened: counters.opened,
                    clicked: counters.clicked,
                    replied: counters.replied,
                    bounced: counters.bounced,
                    unsubscribed: counters.unsubscribed,
                    computed_at,
                },
            )
        })
        .collect();
    Ok(rows
        .into_iter()
        .map(|row| {
            let id = row.id.uuid();
            CampaignObject {
                id: row.id,
                name: row.name,
                status: row.status.parse().unwrap_or(CampaignStatus::Draft),
                last_error: row
                    .last_error
                    .and_then(|error| serde_json::from_value(error).ok()),
                steps: steps.remove(&id).unwrap_or_default(),
                senders: SendersObject {
                    identity_ids: named.remove(&id).unwrap_or_default(),
                    tags: row.sender_tags,
                    on_sender_removed: row
                        .on_sender_removed
                        .parse()
                        .unwrap_or(OnSenderRemoved::Reassign),
                },
                schedule: ScheduleObject {
                    timezone: row.timezone,
                    send_window: row
                        .send_window
                        .and_then(|window| serde_json::from_value(window).ok()),
                    start_at: row.start_at,
                },
                tracking: TrackingObject {
                    domain_id: row.tracking_domain_id,
                    opens: row.track_opens,
                    clicks: row.track_clicks,
                },
                stop_rules: StopRulesObject {
                    on_reply: row.stop_on_reply.parse().unwrap_or(StopOnReply::All),
                    company_on_reply: row.stop_company_on_reply,
                    cooldown_hours: row.cooldown_hours,
                },
                stats: stats.remove(&id).unwrap_or(StatsObject {
                    computed_at,
                    ..StatsObject::default()
                }),
                enrollments: None,
                version: versioning::of(row.updated_at),
                created_at: row.created_at,
                updated_at: row.updated_at,
            }
        })
        .collect())
}

/// Creates a `draft` campaign from `changes` (its `name` given), with its pool and steps.
/// `actor` is who names a winner (`usr_…`).
///
/// # Errors
///
/// See [`Error`]: an unknown identity or tracking domain (`404`), a step or variant that breaks
/// a rule, or the database.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    changes: &Changes,
    actor: &str,
) -> Result<CampaignObject, Error> {
    let send_window = match &changes.send_window {
        Some(Some(window)) => Some(serde_json::to_value(window).map_err(sqlx::Error::decode)?),
        _ => None,
    };
    let id = sqlx::query_scalar!(
        r#"INSERT INTO campaigns (workspace_id, name, timezone, send_window, start_at, tracking_domain_id, track_opens,
                                  track_clicks, stop_on_reply, sender_tags, on_sender_removed, stop_company_on_reply,
                                  cooldown_hours)
           VALUES ($1, $2, coalesce($3, 'UTC'), $4, $5, $6, coalesce($7, false), coalesce($8, false),
                   coalesce($9, 'all'), coalesce($10, '{}'::text[]), coalesce($11, 'reassign'), coalesce($12, false),
                   coalesce($13, 72))
           RETURNING id AS "id: Id<Campaign>""#,
        workspace.uuid(),
        changes.name.as_deref().unwrap_or_default(),
        changes.timezone,
        send_window,
        changes.start_at.flatten() as _,
        changes.tracking_domain.flatten().map(|id| id.uuid()),
        changes.track_opens,
        changes.track_clicks,
        changes.stop_on_reply.map(StopOnReply::as_str),
        changes.tags.as_deref(),
        changes.on_sender_removed.map(OnSenderRemoved::as_str),
        changes.stop_company_on_reply,
        changes.cooldown_hours,
    )
    .fetch_one(&mut **tx)
    .await?;
    if let Some(identities) = &changes.identity_ids {
        name_senders(tx, workspace, id, identities).await?;
    }
    if let Some(inputs) = &changes.steps {
        steps::replace(tx, workspace, id, inputs, actor).await?;
    }
    read(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("campaign"))
}

/// Locks a campaign's row for a change and returns what the change checks; `None` when it does
/// not exist.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Campaign>,
) -> Result<Option<Locked>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT status, sender_tags, updated_at AS "updated_at: Timestamp"
             FROM campaigns WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| Locked {
        id,
        status: row.status.parse().unwrap_or(CampaignStatus::Draft),
        tags: row.sender_tags,
        version: versioning::of(row.updated_at),
    }))
}

/// Applies `changes` to the locked campaign (see [`steps`] for the list of steps), updates its
/// row so its version moves, and, when the pool lost an identity or a tag, enqueues
/// `senders.removed` for the campaign's conversations of the senders that left.
///
/// # Errors
///
/// [`Error::InvalidState`] for an archived campaign; otherwise as [`create`].
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    locked: &Locked,
    changes: &Changes,
    actor: &str,
) -> Result<CampaignObject, Error> {
    if !locked.status.editable() {
        return Err(Error::InvalidState(
            "An archived campaign cannot be changed.".to_owned(),
        ));
    }
    let id = locked.id;
    let mut shrank = changes
        .tags
        .as_ref()
        .is_some_and(|tags| locked.tags.iter().any(|tag| !tags.contains(tag)));
    if let Some(identities) = &changes.identity_ids {
        shrank |= name_senders(tx, workspace, id, identities).await?;
    }
    if let Some(inputs) = &changes.steps {
        steps::replace(tx, workspace, id, inputs, actor).await?;
    }
    let send_window = match &changes.send_window {
        Some(Some(window)) => Some(serde_json::to_value(window).map_err(sqlx::Error::decode)?),
        _ => None,
    };
    sqlx::query!(
        "UPDATE campaigns
            SET name = coalesce($3, name), timezone = coalesce($4, timezone),
                send_window = CASE WHEN $5 THEN $6 ELSE send_window END,
                start_at = CASE WHEN $7 THEN $8 ELSE start_at END,
                tracking_domain_id = CASE WHEN $9 THEN $10 ELSE tracking_domain_id END,
                track_opens = coalesce($11, track_opens), track_clicks = coalesce($12, track_clicks),
                stop_on_reply = coalesce($13, stop_on_reply), sender_tags = coalesce($14, sender_tags),
                on_sender_removed = coalesce($15, on_sender_removed),
                stop_company_on_reply = coalesce($16, stop_company_on_reply),
                cooldown_hours = coalesce($17, cooldown_hours)
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
        changes.name,
        changes.timezone,
        changes.send_window.is_some(),
        send_window,
        changes.start_at.is_some(),
        changes.start_at.flatten() as _,
        changes.tracking_domain.is_some(),
        changes.tracking_domain.flatten().map(|id| id.uuid()),
        changes.track_opens,
        changes.track_clicks,
        changes.stop_on_reply.map(StopOnReply::as_str),
        changes.tags.as_deref(),
        changes.on_sender_removed.map(OnSenderRemoved::as_str),
        changes.stop_company_on_reply,
        changes.cooldown_hours,
    )
    .execute(&mut **tx)
    .await?;
    if changes.start_at.is_some() {
        defer_pending_enrollments(tx, workspace, id).await?;
    }
    if shrank {
        removal::enqueue(tx, workspace, removal::Scope::Campaign(id)).await?;
    }
    read(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("campaign"))
}

/// A later launch also applies to people enrolled before the date was chosen. Keep later
/// follow-up and cooldown times, and leave messages already created to the delivery lifecycle.
/// The caller holds the campaign row, as enrollment and message creation do.
async fn defer_pending_enrollments(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE enrollments e SET next_run_at = c.start_at
           FROM campaigns c
          WHERE c.workspace_id = $1 AND c.id = $2
            AND e.workspace_id = c.workspace_id AND e.campaign_id = c.id
            AND e.status IN ('active', 'paused') AND e.message_id IS NULL
            AND e.next_run_at < c.start_at",
    )
    .bind(workspace.uuid())
    .bind(campaign.uuid())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Makes `identities` the identities the campaign names; true when one it named before is no
/// longer named (the pool may have shrunk).
async fn name_senders(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    identities: &[Id<SenderIdentity>],
) -> Result<bool, sqlx::Error> {
    let ids: Vec<Uuid> = identities.iter().map(|id| id.uuid()).collect();
    let removed = sqlx::query!(
        "DELETE FROM campaign_senders WHERE workspace_id = $1 AND campaign_id = $2 AND sender_identity_id <> ALL($3)",
        workspace.uuid(),
        campaign.uuid(),
        &ids,
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    sqlx::query!(
        "INSERT INTO campaign_senders (workspace_id, campaign_id, sender_identity_id)
         SELECT $1, $2, unnest($3::uuid[]) ON CONFLICT DO NOTHING",
        workspace.uuid(),
        campaign.uuid(),
        &ids,
    )
    .execute(&mut **tx)
    .await?;
    Ok(removed > 0)
}

/// What a deletion did.
#[derive(Debug)]
pub enum Deleted {
    /// The campaign had never sent: it and its steps and enrollments are gone.
    Removed,
    /// The campaign had sent: it is archived, kept for its history.
    Archived(Box<CampaignObject>),
}

/// Deletes the locked campaign: removes it when it never sent, archives it otherwise (see the
/// module of `domain::campaigns`). An archived campaign's live enrollments are stopped and
/// their queued messages cancelled by the next `enrollment.advance` pass, in batches.
///
/// # Errors
///
/// The database refused.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    locked: &Locked,
) -> Result<Deleted, Error> {
    let id = locked.id;
    let sent = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND campaign_id = $2) AS "sent!""#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_one(&mut **tx)
    .await?;
    if sent {
        if locked.status != CampaignStatus::Archived {
            set_status(tx, workspace, id, CampaignStatus::Archived).await?;
        }
        let object = read(tx, workspace, id)
            .await?
            .ok_or(Error::NotFound("campaign"))?;
        return Ok(Deleted::Archived(Box::new(object)));
    }
    let steps = sqlx::query_scalar!(
        r#"SELECT id AS "id: crate::domain::ids::Id<crate::domain::ids::Step>" FROM steps
            WHERE workspace_id = $1 AND campaign_id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM enrollments WHERE workspace_id = $1 AND campaign_id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    for step in steps {
        steps::remove_unsent(tx, workspace, step).await?;
    }
    sqlx::query!(
        "DELETE FROM campaigns WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(Deleted::Removed)
}

/// Starts the locked campaign: from `draft` or `paused` it becomes `materialising`, and its job
/// (`campaign.materialise`) makes it `active` and creates the messages already due.
///
/// # Errors
///
/// [`Error::InvalidState`] from another status, or for a campaign without a step that can send.
pub async fn start(
    tx: &mut Tx,
    workspace: WorkspaceId,
    locked: &Locked,
) -> Result<CampaignObject, Error> {
    let Some(next) = locked.status.start() else {
        return Err(Error::InvalidState(format!(
            "A campaign starts from `draft` or `paused`; this one is `{}`.",
            locked.status.as_str()
        )));
    };
    if !steps::sendable(tx, workspace, locked.id).await? {
        return Err(Error::InvalidState(
            "A campaign needs at least one step, each with a variant, before it starts.".to_owned(),
        ));
    }
    defer_pending_enrollments(tx, workspace, locked.id).await?;
    sqlx::query!(
        "UPDATE campaigns SET status = $3, last_error = NULL WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        locked.id.uuid(),
        next.as_str(),
    )
    .execute(&mut **tx)
    .await?;
    jobs::enqueue(
        tx,
        workspace,
        &materialise::CampaignMaterialise {
            campaign: locked.id.uuid(),
        },
        None,
    )
    .await?;
    read(tx, workspace, locked.id)
        .await?
        .ok_or(Error::NotFound("campaign"))
}

/// Pauses the locked campaign: from `active` or `materialising` it becomes `paused`. No new
/// message is created; queued ones wait, returned to the queue by their Start.
///
/// # Errors
///
/// [`Error::InvalidState`] from another status.
pub async fn pause(
    tx: &mut Tx,
    workspace: WorkspaceId,
    locked: &Locked,
) -> Result<CampaignObject, Error> {
    let Some(next) = locked.status.pause() else {
        return Err(Error::InvalidState(format!(
            "A campaign pauses from `active` or `materialising`; this one is `{}`.",
            locked.status.as_str()
        )));
    };
    set_status(tx, workspace, locked.id, next).await?;
    read(tx, workspace, locked.id)
        .await?
        .ok_or(Error::NotFound("campaign"))
}

/// Writes a campaign's new `status` and tells `campaign.status_changed`, in the caller's
/// transaction, which holds the campaign's row.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn set_status(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    status: CampaignStatus,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE campaigns SET status = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        campaign.uuid(),
        status.as_str(),
    )
    .execute(&mut **tx)
    .await?;
    outbox::record(
        tx,
        workspace,
        Event {
            kind: EventType::CampaignStatusChanged,
            subject_type: "campaign",
            subject_id: campaign.uuid(),
            data: json!({ "campaign_id": campaign, "status": status.as_str() }),
        },
    )
    .await?;
    Ok(())
}
