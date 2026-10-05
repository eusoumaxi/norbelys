//! Enrollments: one person's progress through one campaign, and the ways it ends early.
//!
//! # Enrolling
//!
//! People are enrolled by id, by address, as a group or as a segment. Up to [`INLINE_MAX`]
//! people are enrolled inside the request ([`enroll`]); more are enrolled by the job
//! `enrollment.add` ([`EnrollmentAdd`]), in chunks of [`CHUNK`] people, each chunk one
//! transaction with its checkpoint. Either way each chunk is one statement that skips, without
//! failing the rest:
//!
//! - a person who does not exist (an unknown id or address);
//! - a suppressed address: it may not be mailed, so enrolling it would only stop it later;
//! - a person with a live (`active` or `paused`) enrollment in the campaign already, which the
//!   database keeps unique (`enrollments_active_once`). A person whose earlier enrollment ended
//!   may be enrolled again.
//!
//! A new enrollment's first step is due at once, unless the campaign's `start_at` is later, or
//! the person received a campaign message from the workspace less than the campaign's
//! `cooldown_hours` ago: then it is due when the cooldown ends. People enrolled into an `active`
//! campaign get their first messages from the campaign's job within seconds; a `completed`
//! campaign that receives people is `active` again.
//!
//! # Ending early
//!
//! An enrollment ends early when a person stops it (`POST /enrollments/{id}/stop`), when the
//! person replies ([`stop_for_reply`], called by the inbox, as the campaign's stop rules say),
//! when the address is suppressed ([`stop_suppressed`]), or when its sender leaves the pool of
//! a campaign that stops such conversations. Ending it cancels its current message while that
//! is still `queued`; a message a sender already claimed is returned to the queue by its Start
//! (the enrollment is no longer active) and cancelled by the next `enrollment.advance` pass; one
//! whose submission started finishes as it is.
//!
//! # What an enrollment shows
//!
//! Its person, campaign, position, next run and status; `sender_identity_id`, the sender its
//! conversation keeps; and `waiting_for`, that same sender while it is unavailable (its
//! connection paused, disconnected, its breaker open or its daily budget spent): the next step
//! waits for it and continues by itself when it is back.
//!
//! # What a campaign shows of them
//!
//! A campaign answered alone carries its enrollments counted ([`summary`], [`EnrollmentSummary`]):
//! how many are in each status, how many live ones are at each of its steps, and when its next
//! email may go, so a client never counts enrollments page by page to show where everyone is.
//! One statement counts them by status and position, exactly: it reads the campaign's range of
//! `enrollments_by_campaign` and its rows, so its cost grows with the campaign, which is why a
//! list of campaigns leaves the summary out. Nothing is locked, and nothing is stored: the
//! figures are those of the statement's snapshot.
//!
//! Lock order: the campaign's row (enrolling), then enrollment rows in id order, then queue
//! rows and messages in message order.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::{CampaignObject, Error, materialise};
use crate::db::Tx;
use crate::domain::campaigns::{CampaignStatus, EnrollmentStatus, StopOnReply, next_email};
use crate::domain::ids::{
    Campaign, Enrollment, Id, Message, Person, Segment, SenderIdentity, Step, WorkspaceId,
};
use crate::domain::schedule::Window;
use crate::domain::time::Timestamp;
use crate::jobs::{self, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::people::{PeopleFilters, Selection};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// The most people enrolled inside a request; more go to `enrollment.add`.
pub const INLINE_MAX: usize = 100;
/// The most ids or addresses one request may list.
pub const LIST_MAX: usize = 1_000;
/// People enrolled per chunk of `enrollment.add`.
const CHUNK: i64 = 2_000;

/// An enrollment as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EnrollmentObject {
    pub id: Id<Enrollment>,
    pub campaign_id: Id<Campaign>,
    pub person: PersonRef,
    pub status: EnrollmentStatus,
    /// Why it ended, when it ended early.
    pub status_detail: Option<String>,
    /// The step it is at, 1 for the first.
    pub position: i32,
    /// When its current step is due; `null` while the step's message is on its way, or once
    /// it ended.
    pub next_run_at: Option<Timestamp>,
    /// The current step's message, while it is on its way.
    pub message_id: Option<Id<Message>>,
    /// The sender its conversation keeps, once one was assigned.
    pub sender_identity_id: Option<Id<SenderIdentity>>,
    /// The sender the next step waits for, while that sender is unavailable.
    pub waiting_for: Option<Id<SenderIdentity>>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The person an enrollment is for.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PersonRef {
    pub id: Id<Person>,
    pub email: String,
    /// The given and family names, when the person has either.
    pub name: Option<String>,
}

/// A campaign's enrollments as they stand: how many are in each status, how many live ones are
/// at each step, and when the campaign's next email may go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct EnrollmentSummary {
    /// Live enrollments whose steps run.
    pub active: i64,
    /// Live enrollments held for now: their steps wait.
    pub paused: i64,
    /// Enrollments that went through the campaign's last step.
    pub completed: i64,
    /// Enrollments that ended because the person replied.
    pub replied: i64,
    /// Enrollments stopped by a person, a stop rule, a suppression or a sender that left the
    /// pool.
    pub stopped: i64,
    /// Enrollments that ended because a step could not be sent.
    pub failed: i64,
    /// Every step of the campaign, in order, with its live enrollments (`active` or `paused`):
    /// those whose email from this step is due or on its way. A live enrollment whose step was
    /// removed counts in its status but at no step, until the next pass completes it.
    #[schema(max_items = 50)]
    pub steps: Vec<StepEnrollments>,
    /// When the campaign's next email may go: the earliest time an `active` enrollment's next
    /// step is due, never before now, moved to the next opening of the campaign's send window;
    /// an enrollment whose email is already on its way counts as due now. `null` when the
    /// campaign is not sending (`draft`, `paused`, `completed` or `archived`) or no active
    /// enrollment has a step to come. The sender's pacing, daily limit and own send window may
    /// send it later.
    pub next_run_at: Option<Timestamp>,
}

/// The live enrollments at one step of a campaign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct StepEnrollments {
    pub step_id: Id<Step>,
    /// Live enrollments (`active` or `paused`) at this step now.
    pub live: i64,
}

impl EnrollmentSummary {
    /// Adds `count` enrollments in `status` to that status's figure. The match names every
    /// status, so a status added to the lifecycle does not compile until it has its figure.
    fn add(&mut self, status: EnrollmentStatus, count: i64) {
        let figure = match status {
            EnrollmentStatus::Active => &mut self.active,
            EnrollmentStatus::Paused => &mut self.paused,
            EnrollmentStatus::Completed => &mut self.completed,
            EnrollmentStatus::Replied => &mut self.replied,
            EnrollmentStatus::Stopped => &mut self.stopped,
            EnrollmentStatus::Failed => &mut self.failed,
        };
        *figure = figure.saturating_add(count);
    }
}

/// The filters of the enrollment list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnrollmentFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_id: Option<Id<Campaign>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<Id<Person>>,
    /// Conversations kept by this sender: what removing it from a pool would touch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_identity_id: Option<Id<SenderIdentity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EnrollmentStatus>,
}

/// Who to enroll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// People by id.
    People(Vec<Uuid>),
    /// People by address, as keys (ASCII lowercase).
    Emails(Vec<String>),
    /// A group's members.
    Group(Uuid),
    /// The people a segment matches when each chunk runs.
    Segment(Uuid),
}

/// What an inline enrollment did.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Enrolled {
    /// The enrollments created.
    #[schema(max_items = 100)]
    pub data: Vec<EnrollmentObject>,
    /// The people that were not enrolled, and why.
    #[schema(max_items = 100)]
    pub skipped: Vec<Skipped>,
}

/// A person an enrollment skipped.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Skipped {
    /// The person, when known.
    pub person_id: Option<Id<Person>>,
    /// The address, when the request named the person by it.
    pub email: Option<String>,
    /// `not_found`, `suppressed` or `already_enrolled`.
    pub reason: SkipReason,
}

/// Why a person was not enrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// No such person in the workspace.
    NotFound,
    /// The address is suppressed.
    Suppressed,
    /// The person has a live enrollment in the campaign.
    AlreadyEnrolled,
}

/// An enrollment's row as read.
struct Row {
    id: Id<Enrollment>,
    campaign_id: Id<Campaign>,
    person_id: Id<Person>,
    email: String,
    given_name: Option<String>,
    family_name: Option<String>,
    status: String,
    status_detail: Option<String>,
    current_position: i32,
    next_run_at: Option<Timestamp>,
    message_id: Option<Id<Message>>,
    sender_identity_id: Option<Id<SenderIdentity>>,
    waiting_for: Option<Id<SenderIdentity>>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl From<Row> for EnrollmentObject {
    fn from(row: Row) -> Self {
        let name = [row.given_name, row.family_name]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            id: row.id,
            campaign_id: row.campaign_id,
            person: PersonRef {
                id: row.person_id,
                email: row.email,
                name: (!name.is_empty()).then_some(name),
            },
            status: row.status.parse().unwrap_or(EnrollmentStatus::Stopped),
            status_detail: row.status_detail,
            position: row.current_position,
            next_run_at: row.next_run_at,
            message_id: row.message_id,
            sender_identity_id: row.sender_identity_id,
            waiting_for: row.waiting_for,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// One page of `workspace`'s enrollments matching `filters`, in id order after `cursor`, or
/// exactly the enrollments `ids` when given.
async fn rows(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EnrollmentFilters,
    ids: Option<&[Uuid]>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<EnrollmentObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT e.id AS "id: Id<Enrollment>", e.campaign_id AS "campaign_id: Id<Campaign>",
                  e.person_id AS "person_id: Id<Person>", p.email, p.given_name, p.family_name, e.status,
                  e.status_detail, e.current_position, e.next_run_at AS "next_run_at: Timestamp",
                  e.message_id AS "message_id: Id<Message>",
                  a.sender_identity_id AS "sender_identity_id?: Id<SenderIdentity>",
                  CASE WHEN e.status = 'active' AND i.enabled AND i.archived_at IS NULL
                        AND NOT (c.status = 'active' AND NOT c.paused AND coalesce(c.paused_until <= now(), true)
                                 AND coalesce(c.next_claim_at <= now(), true))
                       THEN a.sender_identity_id END AS "waiting_for?: Id<SenderIdentity>",
                  e.created_at AS "created_at: Timestamp", e.updated_at AS "updated_at: Timestamp"
             FROM enrollments e
             JOIN people p ON p.workspace_id = e.workspace_id AND p.id = e.person_id
             LEFT JOIN campaign_sender_affinity a
               ON a.workspace_id = e.workspace_id AND a.campaign_id = e.campaign_id AND a.person_id = e.person_id
             LEFT JOIN sender_identities i ON i.workspace_id = a.workspace_id AND i.id = a.sender_identity_id
             LEFT JOIN connections c ON c.workspace_id = i.workspace_id AND c.id = i.connection_id
            WHERE e.workspace_id = $1
              AND ($2::uuid[] IS NULL OR e.id = ANY($2))
              AND ($3::uuid IS NULL OR e.campaign_id = $3)
              AND ($4::uuid IS NULL OR e.person_id = $4)
              AND ($5::uuid IS NULL OR a.sender_identity_id = $5)
              AND ($6::text IS NULL OR e.status = $6)
              AND ($7::uuid IS NULL OR CASE WHEN $8 THEN e.id > $7 ELSE e.id < $7 END)
            ORDER BY CASE WHEN $8 THEN e.id END, e.id DESC
            LIMIT $9"#,
        workspace.uuid(),
        ids,
        filters.campaign_id.map(|id| id.uuid()),
        filters.person_id.map(|id| id.uuid()),
        filters.sender_identity_id.map(|id| id.uuid()),
        filters.status.map(EnrollmentStatus::as_str),
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(EnrollmentObject::from).collect())
}

/// Reads one enrollment.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Enrollment>,
) -> Result<Option<EnrollmentObject>, sqlx::Error> {
    let ids = [id.uuid()];
    Ok(rows(
        tx,
        workspace,
        &EnrollmentFilters::default(),
        Some(&ids),
        None,
        false,
        1,
    )
    .await?
    .pop())
}

/// One page of `workspace`'s enrollments matching `filters`, in id order after `cursor`; fetches
/// `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EnrollmentFilters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<EnrollmentObject>, sqlx::Error> {
    rows(tx, workspace, filters, None, cursor, ascending, limit).await
}

/// Counts `workspace`'s enrollments matching `filters`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EnrollmentFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
             SELECT 1 FROM enrollments e
               LEFT JOIN campaign_sender_affinity a
                 ON a.workspace_id = e.workspace_id AND a.campaign_id = e.campaign_id AND a.person_id = e.person_id
              WHERE e.workspace_id = $1 AND ($2::uuid IS NULL OR e.campaign_id = $2)
                AND ($3::uuid IS NULL OR e.person_id = $3) AND ($4::uuid IS NULL OR a.sender_identity_id = $4)
                AND ($5::text IS NULL OR e.status = $5)
              LIMIT $6) counted"#,
        workspace.uuid(),
        filters.campaign_id.map(|id| id.uuid()),
        filters.person_id.map(|id| id.uuid()),
        filters.sender_identity_id.map(|id| id.uuid()),
        filters.status.map(EnrollmentStatus::as_str),
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Counts the enrollments of `campaign` (an object just read, with its status, steps and
/// schedule) by status and by step, and finds when its next email may go as of now
/// (`domain::campaigns::next_email`, through the campaign's send window as the creation pass
/// reads it); see the module. One statement, in the caller's transaction; it writes nothing.
///
/// # Errors
///
/// The database is unavailable.
pub async fn summary(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: &CampaignObject,
) -> Result<EnrollmentSummary, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT status AS "status!", current_position AS "position!", count(*) AS "count!",
                  min(next_run_at) FILTER (WHERE status = 'active') AS "waiting?: Timestamp",
                  count(*) FILTER (WHERE status = 'active' AND message_id IS NOT NULL) AS "on_its_way!"
             FROM enrollments
            WHERE workspace_id = $1 AND campaign_id = $2
            GROUP BY status, current_position"#,
        workspace.uuid(),
        campaign.id.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut summary = EnrollmentSummary::default();
    let mut live: HashMap<i32, i64> = HashMap::new();
    let mut waiting: Option<Timestamp> = None;
    let mut on_its_way = false;
    for row in rows {
        // The table's CHECK admits the lifecycle's names only.
        let Ok(status) = row.status.parse::<EnrollmentStatus>() else {
            continue;
        };
        summary.add(status, row.count);
        if status.is_live() {
            let at = live.entry(row.position).or_default();
            *at = at.saturating_add(row.count);
        }
        waiting = waiting.into_iter().chain(row.waiting).min();
        on_its_way |= row.on_its_way > 0;
    }
    summary.steps = campaign
        .steps
        .iter()
        .map(|step| StepEnrollments {
            step_id: step.id,
            live: live.get(&step.position).copied().unwrap_or(0),
        })
        .collect();
    let window = campaign
        .schedule
        .send_window
        .as_ref()
        .and_then(|window| Window::new(window, &campaign.schedule.timezone).ok());
    summary.next_run_at = next_email(
        campaign.status,
        waiting.map(|at| at.0),
        on_its_way,
        jiff::Timestamp::now(),
        window.as_ref(),
    )
    .map(Timestamp);
    Ok(summary)
}

/// How many people `audience` names, counting at most `cap + 1`: what decides between an
/// inline enrollment and the job.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown group or segment; the database.
pub async fn size(
    tx: &mut Tx,
    workspace: WorkspaceId,
    audience: &Audience,
    cap: i64,
) -> Result<i64, Error> {
    match audience {
        Audience::People(ids) => Ok(i64::try_from(ids.len()).unwrap_or(i64::MAX)),
        Audience::Emails(keys) => Ok(i64::try_from(keys.len()).unwrap_or(i64::MAX)),
        Audience::Group(group) => {
            let exists = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM groups WHERE workspace_id = $1 AND id = $2) AS "exists!""#,
                workspace.uuid(),
                group,
            )
            .fetch_one(&mut **tx)
            .await?;
            if !exists {
                return Err(Error::NotFound("group"));
            }
            Ok(sqlx::query_scalar!(
                r#"SELECT count(*) AS "count!" FROM (
                     SELECT 1 FROM group_people WHERE workspace_id = $1 AND group_id = $2 LIMIT $3) counted"#,
                workspace.uuid(),
                group,
                cap.saturating_add(1),
            )
            .fetch_one(&mut **tx)
            .await?)
        }
        Audience::Segment(segment) => {
            let selection = segment_selection(tx, workspace, *segment).await?;
            Ok(crate::people::count(tx, workspace, &selection, cap).await?)
        }
    }
}

/// A segment's people as a people-list selection.
async fn segment_selection(
    tx: &mut Tx,
    workspace: WorkspaceId,
    segment: Uuid,
) -> Result<Selection, Error> {
    let id: Id<Segment> = Id::from_uuid(segment);
    let compiled = crate::people::segments::compiled(tx, workspace, id)
        .await
        .map_err(|error| match error {
            crate::people::Error::NotFound(what) => Error::NotFound(what),
            crate::people::Error::Db(error) => Error::Db(error),
            other => Error::InvalidState(other.to_string()),
        })?;
    Ok(Selection::new(
        &PeopleFilters {
            segment_id: Some(id),
            ..PeopleFilters::default()
        },
        Some(compiled),
    ))
}

/// The campaign enrolling people, locked: enrolling waits for a creation pass or a completion
/// of the same campaign and is waited for by them, so a campaign is never marked `completed`
/// while people are being added.
struct Target {
    status: CampaignStatus,
    start_at: Option<Timestamp>,
    cooldown_hours: i32,
}

async fn target(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<Target, Error> {
    let row = sqlx::query!(
        r#"SELECT status, start_at AS "start_at: Timestamp", cooldown_hours
             FROM campaigns WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("campaign"))?;
    let status: CampaignStatus = row.status.parse().unwrap_or(CampaignStatus::Archived);
    if !status.enrolls() {
        return Err(Error::InvalidState(
            "An archived campaign takes no new people.".to_owned(),
        ));
    }
    Ok(Target {
        status,
        start_at: row.start_at,
        cooldown_hours: row.cooldown_hours,
    })
}

/// Enrolls the people `ids` (existing or not) into the locked campaign: one statement that
/// skips the unknown, the suppressed and the already enrolled (see the module). Returns the new
/// enrollments' ids with their people.
async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    target: &Target,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    let rows = sqlx::query!(
        "INSERT INTO enrollments (workspace_id, campaign_id, person_id, next_run_at)
         SELECT p.workspace_id, $2, p.id,
                greatest(now(), $4::timestamptz,
                         (SELECT max(m.sent_at) FROM messages m
                           WHERE m.workspace_id = p.workspace_id AND m.person_id = p.id AND m.kind = 'campaign')
                         + make_interval(hours => $5))
           FROM people p
          WHERE p.workspace_id = $1 AND p.id = ANY($3)
            AND NOT EXISTS (SELECT 1 FROM suppressions s WHERE s.workspace_id = p.workspace_id AND s.email_key = p.email_key)
          ORDER BY p.id
         ON CONFLICT (workspace_id, campaign_id, person_id) WHERE status IN ('active', 'paused') DO NOTHING
         RETURNING id, person_id",
        workspace.uuid(),
        campaign.uuid(),
        ids,
        target.start_at as _,
        target.cooldown_hours,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.person_id))
        .collect())
}

/// After people were enrolled into the locked campaign: a `completed` campaign is `active`
/// again, and an `active` one has its job create their first messages now rather than at the
/// next pass.
async fn after_enrolling(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    target: &Target,
) -> Result<(), sqlx::Error> {
    if target.status == CampaignStatus::Completed {
        super::set_status(tx, workspace, campaign, CampaignStatus::Active).await?;
    }
    if matches!(
        target.status,
        CampaignStatus::Completed | CampaignStatus::Active
    ) {
        jobs::enqueue(
            tx,
            workspace,
            &materialise::CampaignMaterialise {
                campaign: campaign.uuid(),
            },
            None,
        )
        .await?;
    }
    Ok(())
}

/// Enrolls `audience` (at most [`INLINE_MAX`] people) into `campaign` inside the caller's
/// transaction, and reports each person it skipped.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown campaign, group or segment; [`Error::InvalidState`] for an
/// archived campaign; the database.
pub async fn enroll(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    audience: &Audience,
) -> Result<Enrolled, Error> {
    let target = target(tx, workspace, campaign).await?;
    let limit = i64::try_from(INLINE_MAX).unwrap_or(i64::MAX);
    let (ids, emails): (Vec<Uuid>, Vec<(Uuid, String)>) = match audience {
        Audience::People(ids) => (ids.clone(), Vec::new()),
        Audience::Emails(keys) => {
            let found = sqlx::query!(
                "SELECT id, email_key FROM people WHERE workspace_id = $1 AND email_key = ANY($2)",
                workspace.uuid(),
                keys,
            )
            .fetch_all(&mut **tx)
            .await?;
            let pairs: Vec<(Uuid, String)> = found
                .into_iter()
                .map(|row| (row.id, row.email_key))
                .collect();
            (pairs.iter().map(|(id, _)| *id).collect(), pairs)
        }
        Audience::Group(_) | Audience::Segment(_) => (
            members(tx, workspace, audience, None, limit).await?,
            Vec::new(),
        ),
    };
    let created = insert(tx, workspace, campaign, &target, &ids).await?;
    if !created.is_empty() {
        after_enrolling(tx, workspace, campaign, &target).await?;
    }
    let mut skipped = Vec::new();
    if let Audience::Emails(keys) = audience {
        for key in keys {
            if !emails.iter().any(|(_, found)| found == key) {
                skipped.push(Skipped {
                    person_id: None,
                    email: Some(key.clone()),
                    reason: SkipReason::NotFound,
                });
            }
        }
    }
    let left: Vec<Uuid> = ids
        .iter()
        .filter(|id| !created.iter().any(|(_, person)| person == *id))
        .copied()
        .collect();
    if !left.is_empty() {
        let known = sqlx::query!(
            r#"SELECT p.id, p.email_key,
                      EXISTS (SELECT 1 FROM suppressions s WHERE s.workspace_id = p.workspace_id AND s.email_key = p.email_key) AS "suppressed!"
                 FROM people p WHERE p.workspace_id = $1 AND p.id = ANY($2)"#,
            workspace.uuid(),
            &left,
        )
        .fetch_all(&mut **tx)
        .await?;
        for id in left {
            let row = known.iter().find(|row| row.id == id);
            let email = emails
                .iter()
                .find(|(person, _)| *person == id)
                .map(|(_, key)| key.clone());
            skipped.push(Skipped {
                person_id: row.map(|_| Id::from_uuid(id)),
                email,
                reason: match row {
                    None => SkipReason::NotFound,
                    Some(row) if row.suppressed => SkipReason::Suppressed,
                    Some(_) => SkipReason::AlreadyEnrolled,
                },
            });
        }
    }
    let ids: Vec<Uuid> = created.iter().map(|(id, _)| *id).collect();
    let data = rows(
        tx,
        workspace,
        &EnrollmentFilters::default(),
        Some(&ids),
        None,
        true,
        limit,
    )
    .await?;
    Ok(Enrolled { data, skipped })
}

/// Up to `limit` member ids of a group or segment after `after`, in id order.
async fn members(
    tx: &mut Tx,
    workspace: WorkspaceId,
    audience: &Audience,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>, Error> {
    match audience {
        Audience::Group(group) => Ok(sqlx::query_scalar!(
            "SELECT person_id FROM group_people
              WHERE workspace_id = $1 AND group_id = $2 AND ($3::uuid IS NULL OR person_id > $3)
              ORDER BY person_id LIMIT $4",
            workspace.uuid(),
            group,
            after,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?),
        Audience::Segment(segment) => {
            let selection = segment_selection(tx, workspace, *segment).await?;
            let people = crate::people::list(tx, workspace, &selection, after, true, limit).await?;
            Ok(people.into_iter().map(|person| person.id.uuid()).collect())
        }
        Audience::People(_) | Audience::Emails(_) => Ok(Vec::new()),
    }
}

/// `enrollment.add`: enrolls more than [`INLINE_MAX`] people into a campaign, in chunks of
/// [`CHUNK`] (see the module). Its progress counts the people enrolled and skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentAdd {
    /// The campaign.
    pub campaign: Uuid,
    /// Who to enroll.
    pub audience: Audience,
    /// The request that asked for it: at most one live job per request.
    pub request: String,
}

impl Job for EnrollmentAdd {
    const KIND: &'static str = "enrollment.add";
    const QUEUE: Queue = Queue::Enrollment;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.request.clone())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let campaign = Id::from_uuid(self.campaign);
        let progress = cx.progress().cloned().unwrap_or_else(|| json!({}));
        let mut offset = progress
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let mut after: Option<Uuid> = progress
            .get("after")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| text.parse().ok());
        let mut enrolled = progress
            .get("enrolled")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let mut examined = progress
            .get("examined")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        loop {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let mut chunk = cx.begin().await?;
            let target = match target(chunk.tx(), workspace, campaign).await {
                Ok(target) => target,
                Err(Error::Db(error)) => return Err(error.into()),
                Err(error) => {
                    return Ok(Outcome::Discard {
                        reason: error.to_string(),
                    });
                }
            };
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let size = usize::try_from(CHUNK).unwrap_or(usize::MAX);
            let ids: Vec<Uuid> = match &self.audience {
                Audience::People(ids) => ids.iter().skip(start).take(size).copied().collect(),
                Audience::Emails(keys) => {
                    let keys: Vec<String> = keys.iter().skip(start).take(size).cloned().collect();
                    sqlx::query_scalar!(
                        "SELECT id FROM people WHERE workspace_id = $1 AND email_key = ANY($2)",
                        workspace.uuid(),
                        &keys,
                    )
                    .fetch_all(&mut **chunk.tx())
                    .await?
                }
                Audience::Group(_) | Audience::Segment(_) => {
                    match members(chunk.tx(), workspace, &self.audience, after, CHUNK).await {
                        Ok(ids) => ids,
                        Err(Error::Db(error)) => return Err(error.into()),
                        Err(error) => {
                            return Ok(Outcome::Discard {
                                reason: error.to_string(),
                            });
                        }
                    }
                }
            };
            let listed = matches!(self.audience, Audience::People(_) | Audience::Emails(_));
            let total = match &self.audience {
                Audience::People(ids) => ids.len(),
                Audience::Emails(keys) => keys.len(),
                Audience::Group(_) | Audience::Segment(_) => 0,
            };
            let done = if listed {
                start >= total
            } else {
                ids.is_empty()
            };
            if done {
                return Ok(Outcome::Done);
            }
            let created = insert(chunk.tx(), workspace, campaign, &target, &ids).await?;
            if !created.is_empty() {
                after_enrolling(chunk.tx(), workspace, campaign, &target).await?;
            }
            enrolled = enrolled.saturating_add(u64::try_from(created.len()).unwrap_or(0));
            if listed {
                offset = offset.saturating_add(u64::try_from(size).unwrap_or(0));
                examined = u64::try_from(total.min(start.saturating_add(size))).unwrap_or(0);
            } else {
                after = ids.last().copied();
                examined = examined.saturating_add(u64::try_from(ids.len()).unwrap_or(0));
            }
            cx.checkpoint(
                chunk,
                json!({
                    "offset": offset,
                    "after": after,
                    "examined": examined,
                    "enrolled": enrolled,
                    "skipped": examined.saturating_sub(enrolled),
                }),
            )
            .await?;
            jobs::wake(cx.db(), Queue::Enrollment).await;
        }
    }
}

/// Cancels the messages `messages` that are still `queued` (their queue rows deleted, the
/// messages `cancelled` with `detail`, `message.cancelled` told); a message another path holds
/// (a sender's claim) is skipped. Returns the cancelled ids. Takes queue rows, then messages, in
/// message order.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn cancel_queued(
    tx: &mut Tx,
    workspace: WorkspaceId,
    messages: &[Uuid],
    detail: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let cancelled = sqlx::query_scalar!(
        "WITH queued AS (
             SELECT q.message_id FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
              WHERE q.workspace_id = $1 AND q.message_id = ANY($2) AND q.state = 'queued'
              ORDER BY q.message_id
                FOR UPDATE OF q, m SKIP LOCKED)
         DELETE FROM delivery_queue q USING queued
          WHERE q.workspace_id = $1 AND q.message_id = queued.message_id
         RETURNING q.message_id",
        workspace.uuid(),
        messages,
    )
    .fetch_all(&mut **tx)
    .await?;
    if cancelled.is_empty() {
        return Ok(cancelled);
    }
    sqlx::query!(
        "UPDATE messages SET state = 'cancelled', status_detail = $3 WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &cancelled,
        detail,
    )
    .execute(&mut **tx)
    .await?;
    crate::delivery::evidence::tell_all(
        tx,
        workspace,
        EventType::MessageCancelled,
        &cancelled,
        None,
        crate::process::now(),
    )
    .await?;
    Ok(cancelled)
}

/// Ends the live enrollments among `ids` with `status` (`stopped`, `replied`, `failed` or
/// `completed`) and `detail`, in the caller's transaction: locks them in id order, cancels their
/// current messages that are still queued (see [`cancel_queued`]), and tells
/// `enrollment.stopped` (or `enrollment.completed`). Returns the enrollments it ended.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn end(
    tx: &mut Tx,
    workspace: WorkspaceId,
    ids: &[Uuid],
    status: EnrollmentStatus,
    detail: Option<&str>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let ended = sqlx::query!(
        "WITH locked AS (
             SELECT id FROM enrollments WHERE workspace_id = $1 AND id = ANY($2) AND status IN ('active', 'paused')
              ORDER BY id FOR UPDATE)
         UPDATE enrollments e SET status = $3, status_detail = $4, next_run_at = NULL
           FROM locked WHERE e.workspace_id = $1 AND e.id = locked.id
         RETURNING e.id, e.campaign_id, e.person_id, e.message_id",
        workspace.uuid(),
        &ids,
        status.as_str(),
        detail,
    )
    .fetch_all(&mut **tx)
    .await?;
    let messages: Vec<Uuid> = ended.iter().filter_map(|row| row.message_id).collect();
    let cancelled = cancel_queued(
        tx,
        workspace,
        &messages,
        detail.unwrap_or("The enrollment ended."),
    )
    .await?;
    if !cancelled.is_empty() {
        sqlx::query!(
            "UPDATE enrollments SET message_id = NULL WHERE workspace_id = $1 AND message_id = ANY($2)",
            workspace.uuid(),
            &cancelled,
        )
        .execute(&mut **tx)
        .await?;
    }
    let kind = if status == EnrollmentStatus::Completed {
        EventType::EnrollmentCompleted
    } else {
        EventType::EnrollmentStopped
    };
    // One statement for every event, however many enrollments ended.
    let events: Vec<Event> = ended
        .iter()
        .map(|row| {
            let enrollment: Id<Enrollment> = Id::from_uuid(row.id);
            let campaign: Id<Campaign> = Id::from_uuid(row.campaign_id);
            let person: Id<Person> = Id::from_uuid(row.person_id);
            Event {
                kind,
                subject_type: "enrollment",
                subject_id: row.id,
                data: json!({
                    "enrollment_id": enrollment,
                    "campaign_id": campaign,
                    "person_id": person,
                    "status": status.as_str(),
                    "status_detail": detail,
                }),
            }
        })
        .collect();
    outbox::record_all(tx, workspace, &events).await?;
    Ok(ended.into_iter().map(|row| row.id).collect())
}

/// Stops one enrollment at a person's request (`POST /enrollments/{id}/stop`).
///
/// # Errors
///
/// [`Error::NotFound`]; [`Error::InvalidState`] when it already ended; the database.
pub async fn stop(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Enrollment>,
) -> Result<EnrollmentObject, Error> {
    let status = sqlx::query_scalar!(
        "SELECT status FROM enrollments WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("enrollment"))?;
    let ended = end(
        tx,
        workspace,
        &[id.uuid()],
        EnrollmentStatus::Stopped,
        Some("Stopped by a person."),
    )
    .await?;
    if ended.is_empty() {
        return Err(Error::InvalidState(format!(
            "Only a live enrollment can be stopped; this one is `{status}`."
        )));
    }
    read(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("enrollment"))
}

/// What a reply stopped.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "answered to the inbox, which calls `stop_for_reply` when it correlates a reply"
    )
)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stopped {
    /// The enrollments that ended `replied`: the person's own, in this campaign or, under
    /// `stop_on_reply = all`, in every campaign.
    pub replied: Vec<Uuid>,
    /// This campaign's enrollments of people at the replier's email domain that ended
    /// `stopped`, under `stop_company_on_reply`.
    pub company: Vec<Uuid>,
}

/// Applies the stop rules of `campaign` to a human reply of `person` (the inbox calls it once it
/// has correlated an inbound message to a campaign thread and judged it a person's answer, in
/// its own transaction):
///
/// - `stop_on_reply = campaign`: the person's live enrollment in this campaign ends `replied`;
/// - `stop_on_reply = all`: the person's live enrollments in every campaign of the workspace
///   end `replied`;
/// - `stop_on_reply = none`: nothing ends;
/// - and with `stop_company_on_reply`, this campaign's live enrollments of other people at the
///   replier's email domain end `stopped` ("a colleague replied").
///
/// Each ended enrollment's queued message is cancelled and `enrollment.stopped` told. Calling it
/// twice for the same reply ends nothing the second time. Takes enrollment rows (in id order),
/// then queue rows and messages; it never takes the campaign's row.
///
/// # Errors
///
/// The database refused.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "called by the inbox when it correlates a reply")
)]
pub async fn stop_for_reply(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    person: Id<Person>,
) -> Result<Stopped, sqlx::Error> {
    let Some(rules) = sqlx::query!(
        "SELECT stop_on_reply, stop_company_on_reply FROM campaigns WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(Stopped::default());
    };
    let rule: StopOnReply = rules.stop_on_reply.parse().unwrap_or(StopOnReply::All);
    let own = sqlx::query_scalar!(
        "SELECT id FROM enrollments
          WHERE workspace_id = $1 AND person_id = $2 AND status IN ('active', 'paused')
            AND ($3 OR campaign_id = $4)",
        workspace.uuid(),
        person.uuid(),
        rule == StopOnReply::All,
        campaign.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let colleagues = if rules.stop_company_on_reply {
        sqlx::query_scalar!(
            "SELECT e.id FROM enrollments e
               JOIN people p ON p.workspace_id = e.workspace_id AND p.id = e.person_id
               JOIN people replier ON replier.workspace_id = e.workspace_id AND replier.id = $3
              WHERE e.workspace_id = $1 AND e.campaign_id = $2 AND e.status IN ('active', 'paused')
                AND e.person_id <> $3
                AND split_part(p.email_key, '@', 2) = split_part(replier.email_key, '@', 2)",
            workspace.uuid(),
            campaign.uuid(),
            person.uuid(),
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        Vec::new()
    };
    let replied = if rule == StopOnReply::None {
        Vec::new()
    } else {
        end(
            tx,
            workspace,
            &own,
            EnrollmentStatus::Replied,
            Some("The person replied."),
        )
        .await?
    };
    let company = end(
        tx,
        workspace,
        &colleagues,
        EnrollmentStatus::Stopped,
        Some("A colleague at the same company replied."),
    )
    .await?;
    Ok(Stopped { replied, company })
}

/// Stops every live enrollment of the person at `email` (any case), as an unsubscribe, a
/// complaint, a bounce or a person's suppression requires: the address may not be mailed, so no
/// step of any campaign may follow. Returns how many ended. A manual suppression and the inbox's
/// suppression of a sender call it in the transaction that suppresses the address; provider
/// evidence and an inbox poll's unsubscribe request call it in a transaction of their own right
/// after (their evidence holds message rows, which stopping locks in the other order). An
/// enrollment it does not reach (a crash in between, a one-click unsubscribe, a bounce seen by the
/// sender) still stops at its next step: the Start re-reads suppressions and ends the message
/// `suppressed` without sending, and the advance then settles the enrollment as stopped.
///
/// # Errors
///
/// The database refused.
pub async fn stop_suppressed(
    tx: &mut Tx,
    workspace: WorkspaceId,
    email: &str,
) -> Result<usize, sqlx::Error> {
    stop_suppressed_all(tx, workspace, &[email.to_owned()]).await
}

/// Stops every live enrollment of the people at `emails` (any case), as [`stop_suppressed`]
/// does for one address, with one lookup and one ending for them all: a person's list of
/// addresses suppressed in one request (up to 1,000) costs a few statements, not a few per
/// address. Returns how many ended.
///
/// # Errors
///
/// The database refused.
pub async fn stop_suppressed_all(
    tx: &mut Tx,
    workspace: WorkspaceId,
    emails: &[String],
) -> Result<usize, sqlx::Error> {
    if emails.is_empty() {
        return Ok(0);
    }
    let ids = sqlx::query_scalar!(
        "SELECT e.id FROM enrollments e JOIN people p ON p.workspace_id = e.workspace_id AND p.id = e.person_id
          WHERE e.workspace_id = $1 AND e.status IN ('active', 'paused')
            AND p.email_key = ANY(ARRAY(SELECT ascii_lower(email) FROM unnest($2::text[]) AS email))",
        workspace.uuid(),
        emails,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(end(
        tx,
        workspace,
        &ids,
        EnrollmentStatus::Stopped,
        Some("The address is suppressed."),
    )
    .await?
    .len())
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::EnrollmentSummary;
    use crate::domain::campaigns::EnrollmentStatus;

    /// A campaign's summary shows every status of the lifecycle under the status's own name and
    /// nothing besides them but `steps` and `next_run_at`: a count added under one status is
    /// read back under that name and under no other. A client reads these names as the
    /// enrollments' `status` values, so a figure under another name would misreport where people
    /// are. Generated over every status, so one added later fails here until the summary shows
    /// it.
    #[test]
    fn every_status_is_counted_under_its_own_name() {
        let statuses: Vec<EnrollmentStatus> = EnrollmentStatus::iter().collect();
        for status in &statuses {
            let mut summary = EnrollmentSummary::default();
            summary.add(*status, 7);
            let shown = serde_json::to_value(&summary).unwrap();
            let members = shown.as_object().unwrap();
            assert_eq!(members.len(), statuses.len() + 2, "{members:?}");
            for other in &statuses {
                let expected = if other == status { 7 } else { 0 };
                assert_eq!(
                    shown[other.as_str()],
                    expected,
                    "{status:?} read as {other:?}"
                );
            }
        }
    }
}
