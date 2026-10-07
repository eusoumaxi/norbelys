//! The creation pass: turning due enrollments into step messages.
//!
//! `enrollment.advance` (every workspace, every 5 minutes) and `campaign.materialise` (one
//! campaign, right after it starts or receives people) both run [`pass`] over a chunk of due
//! enrollments; `message.generate` finishes the enrollments whose step asks for AI content
//! ([`create_generated`]). Every message is created through the delivery's one creation
//! contract (`delivery::accept::step`), which locks the enrollment and creates the step's
//! message only while the enrollment is active, at the expected position and revision, and has
//! no message yet: two passes over the same enrollment create one message.
//!
//! # One enrollment
//!
//! 1. **The step**: the campaign's step at the enrollment's position, with its current revision.
//!    No step there (steps were removed) ends the enrollment `completed`.
//! 2. **When**: the later of its due time and now, moved to the campaign window's next opening
//!    when the window is closed then (`domain::campaigns::admit`). Beyond this pass's horizon
//!    (the mark after next), the due time is moved there and the enrollment waits.
//! 3. **The sender**: the conversation's own (its affinity) while it is still in the pool; while
//!    that sender is unavailable the enrollment waits for it. A conversation whose sender left
//!    the pool follows the campaign's `on_sender_removed`: `reassign` drops the affinity and the
//!    thread's root, so the new sender starts a new thread; `stop` ends the enrollment. A
//!    conversation without a sender takes the pool's next usable one by rotation; when none is
//!    usable it waits, and the campaign's `last_error` says `no_sender` until a sender is back.
//! 4. **The variant**: the person's assignment for this revision if there is one, else the one
//!    the step's allocation chooses (`domain::allocation`), recorded in `step_assignments`. An
//!    `automatic` step whose winner rule is met names its winner first.
//! 5. **The message**: created now; or, for a step with a personalisation prompt, a
//!    `message.generate` job writes the snippets first and creates it in its checkpoint, so the
//!    message row is never written after it is inserted.
//!
//! A suppressed address stops the enrollment; templates that do not render for the person fail
//! it, with the reason. Rotation, affinity and assignment are recorded only for a message that
//! was created (or handed to its generation).
//!
//! # Locks
//!
//! A chunk locks its campaigns' rows (`FOR UPDATE`, in id order) before anything else: the
//! pool's rotation is serialised on that lock, so two passes never give two conversations the
//! same "least recently assigned" sender, and an update of the campaign (a new revision, a pool
//! change) waits for the chunk or is seen whole by it. Then each enrollment's row (taken by the
//! creation contract, or here before an affinity or assignment is written), then threads, then
//! the rows the message inserts. Chunks are kept small ([`CHUNK`]): a Start of the campaign's
//! mail share-locks the same campaign row and waits for the chunk to commit.

use std::collections::HashMap;
use std::time::Duration;

use jiff::Timestamp as Instant;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::enrollments;
use super::generate::MessageGenerate;
use super::steps;
use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::accept::{self, StepMessage, StepOutcome};
use crate::domain::allocation::{self, Allocation, Objective, Observed, Offered, WinnerRule};
use crate::domain::campaigns::{
    self as policy, Admitted, Candidate, EnrollmentStatus, OnSenderRemoved, Rotation,
};
use crate::domain::ids::{Campaign, Enrollment, Id, SenderIdentity, Step, Variant, WorkspaceId};
use crate::domain::schedule::Window;
use crate::domain::senders::SendWindow;
use crate::domain::time::Timestamp;
use crate::jobs;

/// Enrollments one creation chunk takes: small, because the chunk holds its campaigns' rows,
/// which the Starts of their mail share-lock.
pub const CHUNK: i64 = 200;

/// Where a pass stopped, in the order it walks due enrollments: `(next_run_at, id)`.
pub type Cursor = (Timestamp, Uuid);

/// What one chunk did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// Due enrollments it looked at.
    pub examined: usize,
    /// Messages it created.
    pub created: usize,
    /// Messages handed to `message.generate`.
    pub generating: usize,
    /// Where the next chunk continues; `None` when this chunk was the last.
    pub cursor: Option<Cursor>,
}

/// A due enrollment.
struct Due {
    id: Uuid,
    campaign: Uuid,
    person: Uuid,
    position: i32,
    next_run_at: Timestamp,
}

/// A step's current revision, as the pass uses it.
struct StepNow {
    id: Uuid,
    revision: i32,
    published_at: Timestamp,
    allocation: Allocation,
    rule: WinnerRule,
    prompt: bool,
    winner: Option<(Uuid, i32)>,
    /// `(variant, version, weight)` in creation order, with the people assigned so far.
    options: Vec<((Uuid, i32), Offered)>,
}

/// One locked campaign of a chunk, with what its enrollments need.
struct Context {
    id: Uuid,
    rule: OnSenderRemoved,
    window: Option<Window>,
    last_error: Option<String>,
    rotation: Rotation,
    picked: Vec<Uuid>,
    steps: HashMap<i32, StepNow>,
    affinity: HashMap<Uuid, Uuid>,
    assigned: HashMap<(Uuid, i32, Uuid), (Uuid, i32)>,
    no_sender: bool,
    created: usize,
}

/// Creates the messages of up to `limit` due enrollments of `workspace` (of `campaign` alone
/// when given), after `cursor`, in the caller's transaction (a job's chunk): see the module.
/// `now` is the pass's clock; enrollments due before its horizon are taken.
///
/// # Errors
///
/// The database refused; the job runs the chunk again.
#[allow(
    clippy::too_many_arguments,
    reason = "the pass's scope, cursor and configured validation gate"
)]
pub async fn pass(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    campaign: Option<Id<Campaign>>,
    now: Instant,
    cursor: Option<Cursor>,
    limit: i64,
    validation_required: bool,
) -> Result<Pass, sqlx::Error> {
    let horizon = policy::horizon(now);
    let due: Vec<Due> = sqlx::query!(
        r#"SELECT e.id, e.campaign_id, e.person_id, e.current_position, e.next_run_at AS "next_run_at!: Timestamp"
             FROM enrollments e JOIN campaigns c ON c.workspace_id = e.workspace_id AND c.id = e.campaign_id
            WHERE e.workspace_id = $1 AND e.status = 'active' AND e.next_run_at IS NOT NULL AND e.next_run_at < $2
              AND e.message_id IS NULL AND c.status = 'active'
              AND ($3::uuid IS NULL OR e.campaign_id = $3)
              AND ($4::timestamptz IS NULL OR (e.next_run_at, e.id) > ($4, $5))
            ORDER BY e.next_run_at, e.id LIMIT $6"#,
        workspace.uuid(),
        Timestamp(horizon) as _,
        campaign.map(|id| id.uuid()),
        cursor.map(|(at, _)| at) as _,
        cursor.map(|(_, id)| id),
        limit,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| Due {
        id: row.id,
        campaign: row.campaign_id,
        person: row.person_id,
        position: row.current_position,
        next_run_at: row.next_run_at,
    })
    .collect();
    let mut result = Pass {
        examined: due.len(),
        cursor: due
            .last()
            .filter(|_| i64::try_from(due.len()).unwrap_or(i64::MAX) >= limit)
            .map(|last| (last.next_run_at, last.id)),
        ..Pass::default()
    };
    if due.is_empty() {
        return Ok(result);
    }
    let mut campaigns: Vec<Uuid> = due.iter().map(|due| due.campaign).collect();
    campaigns.sort_unstable();
    campaigns.dedup();
    let mut contexts = load(tx, workspace, &campaigns, &due).await?;
    if validation_required {
        // Keep the addresses this chunk may snapshot stable until its messages commit.
        let people = due.iter().map(|row| row.person).collect::<Vec<_>>();
        sqlx::query(
            "SELECT id FROM people WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR SHARE",
        )
        .bind(workspace.uuid())
        .bind(&people)
        .fetch_all(&mut **tx)
        .await?;
        let mut ready = Vec::new();
        for context in contexts {
            let campaign = Id::from_uuid(context.id);
            if super::recipients::ready_or_enqueue(tx, workspace, campaign).await? {
                ready.push(context);
            }
        }
        contexts = ready;
    }
    for due in &due {
        let Some(context) = contexts
            .iter_mut()
            .find(|context| context.id == due.campaign)
        else {
            // The campaign stopped being active between the read and its lock.
            continue;
        };
        match one(tx, keys, workspace, context, due, now, horizon).await? {
            Made::Created => {
                result.created = result.created.saturating_add(1);
                context.created = context.created.saturating_add(1);
            }
            Made::Generating => {
                result.generating = result.generating.saturating_add(1);
                context.created = context.created.saturating_add(1);
            }
            Made::Nothing => {}
        }
    }
    for context in &contexts {
        record(tx, workspace, context).await?;
    }
    Ok(result)
}

/// Locks the active campaigns among `campaigns` (in id order) and reads what their due
/// enrollments need: the window, the pool with its rotation, the steps, the affinities and the
/// assignments of these people.
async fn load(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaigns: &[Uuid],
    due: &[Due],
) -> Result<Vec<Context>, sqlx::Error> {
    let locked = sqlx::query!(
        "SELECT id, on_sender_removed, timezone, send_window, sender_tags, last_error ->> 'code' AS last_error
           FROM campaigns WHERE workspace_id = $1 AND id = ANY($2) AND status = 'active'
          ORDER BY id FOR UPDATE",
        workspace.uuid(),
        campaigns,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut contexts = Vec::with_capacity(locked.len());
    for campaign in locked {
        let people: Vec<Uuid> = due
            .iter()
            .filter(|due| due.campaign == campaign.id)
            .map(|due| due.person)
            .collect();
        let mut positions: Vec<i32> = due
            .iter()
            .filter(|due| due.campaign == campaign.id)
            .map(|due| due.position)
            .collect();
        positions.sort_unstable();
        positions.dedup();
        let window = campaign
            .send_window
            .and_then(|window| serde_json::from_value::<SendWindow>(window).ok())
            .and_then(|window| Window::new(&window, &campaign.timezone).ok());
        let pool = sqlx::query!(
            r#"SELECT i.id, (c.status = 'active' AND NOT c.paused AND coalesce(c.paused_until <= now(), true)
                             AND coalesce(c.next_claim_at <= now(), true)) AS "usable!",
                      r.last_assigned_at AS "last_assigned_at: Timestamp"
                 FROM sender_identities i
                 JOIN connections c ON c.workspace_id = i.workspace_id AND c.id = i.connection_id
                 LEFT JOIN campaign_sender_rotation r
                   ON r.workspace_id = i.workspace_id AND r.campaign_id = $2 AND r.sender_identity_id = i.id
                WHERE i.workspace_id = $1 AND i.enabled AND i.archived_at IS NULL AND c.status <> 'archived'
                  AND (i.tags && $3::text[]
                       OR EXISTS (SELECT 1 FROM campaign_senders s
                                   WHERE s.workspace_id = i.workspace_id AND s.campaign_id = $2 AND s.sender_identity_id = i.id))"#,
            workspace.uuid(),
            campaign.id,
            &campaign.sender_tags,
        )
        .fetch_all(&mut **tx)
        .await?;
        let rotation = Rotation::new(
            pool.into_iter()
                .map(|row| Candidate {
                    id: row.id,
                    usable: row.usable,
                    last_assigned_at: row.last_assigned_at.map(|at| at.0),
                })
                .collect(),
        );
        let steps = steps_now(tx, workspace, campaign.id, &positions).await?;
        let affinity = sqlx::query!(
            "SELECT person_id, sender_identity_id FROM campaign_sender_affinity
              WHERE workspace_id = $1 AND campaign_id = $2 AND person_id = ANY($3)",
            workspace.uuid(),
            campaign.id,
            &people,
        )
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|row| (row.person_id, row.sender_identity_id))
        .collect();
        let step_ids: Vec<Uuid> = steps.values().map(|step| step.id).collect();
        let assigned = sqlx::query!(
            "SELECT step_id, step_revision, person_id, variant_id, variant_version FROM step_assignments
              WHERE workspace_id = $1 AND step_id = ANY($2) AND person_id = ANY($3)",
            workspace.uuid(),
            &step_ids,
            &people,
        )
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|row| {
            (
                (row.step_id, row.step_revision, row.person_id),
                (row.variant_id, row.variant_version),
            )
        })
        .collect();
        contexts.push(Context {
            id: campaign.id,
            rule: campaign
                .on_sender_removed
                .parse()
                .unwrap_or(OnSenderRemoved::Reassign),
            window,
            last_error: campaign.last_error,
            rotation,
            picked: Vec::new(),
            steps,
            affinity,
            assigned,
            no_sender: false,
            created: 0,
        });
    }
    Ok(contexts)
}

/// The steps of a campaign at `positions` (where the chunk's due enrollments are), by
/// position, with their current revisions and the people each option has been assigned so far.
///
/// Only those steps are read: the assignments of a step are counted on every chunk that needs
/// it, so reading every step would make each chunk count the whole campaign's assignments. A
/// step's assignments are counted in one grouped read of the step's revision, never once per
/// variant, so the work is one pass over them whatever the number of variants.
async fn steps_now(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Uuid,
    positions: &[i32],
) -> Result<HashMap<i32, StepNow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT s.id, s.position, r.revision, r.created_at AS "created_at: Timestamp", r.allocation, r.ranking_objective,
                  r.observation_window_seconds, r.minimum_sample, r.personalisation_prompt IS NOT NULL AS "prompt!",
                  r.winner_variant_id, r.winner_variant_version
             FROM steps s
             JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = s.current_revision
            WHERE s.workspace_id = $1 AND s.campaign_id = $2 AND s.position = ANY($3)"#,
        workspace.uuid(),
        campaign,
        positions,
    )
    .fetch_all(&mut **tx)
    .await?;
    let options = sqlx::query!(
        r#"SELECT o.step_id, o.variant_id, o.variant_version, o.weight, coalesce(a.assigned, 0) AS "assigned!"
             FROM steps s
             JOIN step_revision_variants o ON o.workspace_id = s.workspace_id AND o.step_id = s.id AND o.step_revision = s.current_revision
             LEFT JOIN LATERAL (SELECT a.variant_id, count(*) AS assigned FROM step_assignments a
                                 WHERE a.workspace_id = s.workspace_id AND a.step_id = s.id AND a.step_revision = s.current_revision
                                 GROUP BY a.variant_id) a ON a.variant_id = o.variant_id
            WHERE s.workspace_id = $1 AND s.campaign_id = $2 AND s.position = ANY($3)
            ORDER BY o.step_id, o.variant_id"#,
        workspace.uuid(),
        campaign,
        positions,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let step = StepNow {
                id: row.id,
                revision: row.revision,
                published_at: row.created_at,
                allocation: row.allocation.parse().unwrap_or(Allocation::Balanced),
                rule: WinnerRule {
                    objective: row.ranking_objective.parse().unwrap_or(Objective::Replies),
                    observation_window: Duration::from_secs(
                        u64::try_from(row.observation_window_seconds).unwrap_or(0),
                    ),
                    minimum_sample: u64::try_from(row.minimum_sample).unwrap_or(1),
                },
                prompt: row.prompt,
                winner: row.winner_variant_id.zip(row.winner_variant_version),
                options: options
                    .iter()
                    .filter(|option| option.step_id == row.id)
                    .map(|option| {
                        (
                            (option.variant_id, option.variant_version),
                            Offered {
                                weight: u32::try_from(option.weight).unwrap_or(1),
                                assigned: u64::try_from(option.assigned).unwrap_or(0),
                            },
                        )
                    })
                    .collect(),
            };
            (row.position, step)
        })
        .collect())
}

/// What became of one due enrollment.
enum Made {
    Created,
    Generating,
    Nothing,
}

/// The sender of one enrollment's next message.
enum Sender {
    /// Its conversation's own sender.
    Kept(Uuid),
    /// A sender the rotation just picked; `reassigned` when it replaces one that left the pool.
    Picked(Uuid),
    /// Nothing to send from now.
    Wait,
}

/// Runs one due enrollment (see the module).
async fn one(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    context: &mut Context,
    due: &Due,
    now: Instant,
    horizon: Instant,
) -> Result<Made, sqlx::Error> {
    let Some((step_id, revision, prompt)) = context
        .steps
        .get(&due.position)
        .map(|step| (step.id, step.revision, step.prompt))
    else {
        enrollments::end(tx, workspace, &[due.id], EnrollmentStatus::Completed, None).await?;
        return Ok(Made::Nothing);
    };
    let send_at = match policy::admit(due.next_run_at.0, now, context.window.as_ref()) {
        Admitted::Never => return Ok(Made::Nothing),
        Admitted::At(at) if at >= horizon => {
            sqlx::query!(
                "UPDATE enrollments SET next_run_at = $3
                  WHERE workspace_id = $1 AND id = $2 AND status = 'active' AND message_id IS NULL",
                workspace.uuid(),
                due.id,
                Timestamp(at) as _,
            )
            .execute(&mut **tx)
            .await?;
            return Ok(Made::Nothing);
        }
        Admitted::At(at) => Timestamp(at),
    };
    let sender = match context.affinity.get(&due.person).copied() {
        Some(kept) if context.rotation.contains(kept) => {
            if context.rotation.usable(kept) {
                Sender::Kept(kept)
            } else {
                Sender::Wait
            }
        }
        Some(_) => match context.rule {
            OnSenderRemoved::Stop => {
                enrollments::end(
                    tx,
                    workspace,
                    &[due.id],
                    EnrollmentStatus::Stopped,
                    Some("The conversation's sender left the campaign's pool."),
                )
                .await?;
                return Ok(Made::Nothing);
            }
            OnSenderRemoved::Reassign => {
                // A new sender starts a new thread: the earlier messages were not its own.
                sqlx::query!(
                    "UPDATE enrollments SET thread_root_message_id = NULL
                      WHERE workspace_id = $1 AND id = $2 AND status = 'active' AND message_id IS NULL",
                    workspace.uuid(),
                    due.id,
                )
                .execute(&mut **tx)
                .await?;
                sqlx::query!(
                    "DELETE FROM campaign_sender_affinity WHERE workspace_id = $1 AND campaign_id = $2 AND person_id = $3",
                    workspace.uuid(),
                    context.id,
                    due.person,
                )
                .execute(&mut **tx)
                .await?;
                context.affinity.remove(&due.person);
                pick(context, now)
            }
        },
        None => pick(context, now),
    };
    let (identity, picked) = match sender {
        Sender::Kept(identity) => (identity, false),
        Sender::Picked(identity) => (identity, true),
        Sender::Wait => return Ok(Made::Nothing),
    };
    let (variant, fresh) = match context
        .assigned
        .get(&(step_id, revision, due.person))
        .copied()
    {
        Some(variant) => (variant, false),
        None => match choose(tx, workspace, context, due.position, now).await? {
            Some(variant) => (variant, true),
            None => return Ok(Made::Nothing),
        },
    };
    let message = StepMessage {
        enrollment: Id::from_uuid(due.id),
        campaign: Id::from_uuid(context.id),
        step: Id::from_uuid(step_id),
        position: due.position,
        step_revision: revision,
        variant: Id::from_uuid(variant.0),
        variant_version: variant.1,
        identity: Id::from_uuid(identity),
        variables: None,
        snippets_fallback: None,
        send_at: Some(send_at),
    };
    if prompt {
        if !lock_ready(tx, workspace, &message).await? {
            return Ok(Made::Nothing);
        }
        remember(tx, workspace, context, due, &message, picked, fresh).await?;
        jobs::enqueue(tx, workspace, &MessageGenerate::from(&message), None).await?;
        return Ok(Made::Generating);
    }
    match create_one(tx, keys, workspace, &message).await? {
        true => {
            remember(tx, workspace, context, due, &message, picked, fresh).await?;
            Ok(Made::Created)
        }
        false => Ok(Made::Nothing),
    }
}

/// The rotation's next usable sender, or [`Sender::Wait`] with the campaign marked `no_sender`.
fn pick(context: &mut Context, now: Instant) -> Sender {
    match context.rotation.pick(now) {
        Some(identity) => {
            context.picked.push(identity);
            Sender::Picked(identity)
        }
        None => {
            context.no_sender = true;
            Sender::Wait
        }
    }
}

/// The variant the next person receives at `position`: the revision's winner, or the one its
/// allocation chooses. Names an `automatic` step's winner first when its rule is met.
async fn choose(
    tx: &mut Tx,
    workspace: WorkspaceId,
    context: &mut Context,
    position: i32,
    now: Instant,
) -> Result<Option<(Uuid, i32)>, sqlx::Error> {
    let Some(step) = context.steps.get_mut(&position) else {
        return Ok(None);
    };
    if step.allocation == Allocation::Automatic && step.winner.is_none() {
        step.winner = select_winner(tx, workspace, context.id, step, now).await?;
    }
    let offered: Vec<Offered> = step.options.iter().map(|(_, offered)| *offered).collect();
    let winner = step.winner.and_then(|winner| {
        step.options
            .iter()
            .position(|(option, _)| *option == winner)
    });
    let Some(index) = allocation::choose(step.allocation, &offered, winner) else {
        return Ok(None);
    };
    let Some((variant, offered)) = step.options.get_mut(index) else {
        return Ok(None);
    };
    offered.assigned = offered.assigned.saturating_add(1);
    Ok(Some(*variant))
}

/// Reads the counters of an `automatic` step's revision and names its winner when the rule is
/// met (`domain::allocation::select_winner`), recording the evidence; the campaign's row (held)
/// is updated so its version moves.
async fn select_winner(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Uuid,
    step: &StepNow,
    now: Instant,
) -> Result<Option<(Uuid, i32)>, sqlx::Error> {
    if step.options.len() < 2 {
        return Ok(None);
    }
    let elapsed = now
        .duration_since(step.published_at.0)
        .try_into()
        .unwrap_or(Duration::ZERO);
    if elapsed < step.rule.observation_window {
        return Ok(None);
    }
    let counted = sqlx::query!(
        r#"SELECT variant_id, variant_version, sum(sent)::bigint AS "sent!", sum(opened)::bigint AS "opened!",
                  sum(clicked)::bigint AS "clicked!", sum(replied)::bigint AS "replied!"
             FROM campaign_daily_stats
            WHERE workspace_id = $1 AND campaign_id = $2 AND step_id = $3 AND step_revision = $4
            GROUP BY variant_id, variant_version"#,
        workspace.uuid(),
        campaign,
        step.id,
        step.revision,
    )
    .fetch_all(&mut **tx)
    .await?;
    let observed: Vec<Observed> = step
        .options
        .iter()
        .map(|((variant, version), _)| {
            counted
                .iter()
                .find(|row| row.variant_id == *variant && row.variant_version == *version)
                .map_or_else(Observed::default, |row| Observed {
                    sent: u64::try_from(row.sent).unwrap_or(0),
                    opened: u64::try_from(row.opened).unwrap_or(0),
                    clicked: u64::try_from(row.clicked).unwrap_or(0),
                    replied: u64::try_from(row.replied).unwrap_or(0),
                })
        })
        .collect();
    let Some(index) = allocation::select_winner(&step.rule, elapsed, &observed) else {
        return Ok(None);
    };
    let Some(((variant, version), _)) = step.options.get(index) else {
        return Ok(None);
    };
    let evidence = json!({
        "objective": step.rule.objective.as_str(),
        "observation_window_seconds": step.rule.observation_window.as_secs(),
        "minimum_sample": step.rule.minimum_sample,
        "variants": step.options.iter().zip(&observed).map(|(((variant, version), _), seen)| json!({
            "variant_id": Id::<Variant>::from_uuid(*variant),
            "version": version,
            "sent": seen.sent,
            "count": seen.of(step.rule.objective),
        })).collect::<Vec<Value>>(),
    });
    steps::choose_winner(
        tx,
        workspace,
        Id::<Step>::from_uuid(step.id),
        step.revision,
        (*variant, *version),
        step.rule.objective,
        "automatic",
        &evidence,
    )
    .await?;
    touch(tx, workspace, campaign).await?;
    Ok(Some((*variant, *version)))
}

/// Locks the enrollment of `message` and checks it is still ready for this step's message
/// (active, at the position, no message yet), as the creation contract will check again at its
/// checkpoint.
async fn lock_ready(
    tx: &mut Tx,
    workspace: WorkspaceId,
    message: &StepMessage,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar!(
        r#"SELECT status = 'active' AND current_position = $3 AND message_id IS NULL AS "ready!"
             FROM enrollments WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        message.enrollment.uuid(),
        message.position,
    )
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false))
}

/// Records what a created (or generating) message decided: the conversation's sender, the
/// person's variant for the revision. The enrollment's row is held by then.
async fn remember(
    tx: &mut Tx,
    workspace: WorkspaceId,
    context: &mut Context,
    due: &Due,
    message: &StepMessage,
    picked: bool,
    fresh: bool,
) -> Result<(), sqlx::Error> {
    if picked {
        sqlx::query!(
            "INSERT INTO campaign_sender_affinity (workspace_id, campaign_id, person_id, sender_identity_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (workspace_id, campaign_id, person_id)
             DO UPDATE SET sender_identity_id = excluded.sender_identity_id, assigned_at = now()",
            workspace.uuid(),
            context.id,
            due.person,
            message.identity.uuid(),
        )
        .execute(&mut **tx)
        .await?;
        context.affinity.insert(due.person, message.identity.uuid());
    }
    if fresh {
        sqlx::query!(
            "INSERT INTO step_assignments (workspace_id, step_id, step_revision, person_id, variant_id, variant_version)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
            workspace.uuid(),
            message.step.uuid(),
            message.step_revision,
            due.person,
            message.variant.uuid(),
            message.variant_version,
        )
        .execute(&mut **tx)
        .await?;
        context.assigned.insert(
            (message.step.uuid(), message.step_revision, due.person),
            (message.variant.uuid(), message.variant_version),
        );
    }
    Ok(())
}

/// Creates one step message through the creation contract and applies what it answers: a
/// suppressed address stops the enrollment, templates that do not render for the person (or an
/// envelope that breaks a rule) fail it, a stale creator or an identity that changed meanwhile
/// leaves it for the next pass. True when the message exists now.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn create_one(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    message: &StepMessage,
) -> Result<bool, sqlx::Error> {
    let ended = |status, detail: String| (status, detail);
    let outcome = match accept::step(tx, keys, workspace, message).await {
        Ok(StepOutcome::Created(_)) => return Ok(true),
        Ok(StepOutcome::Stale(_)) => return Ok(false),
        Err(accept::Error::Db(error)) => return Err(error),
        Err(accept::Error::Suppressed { .. }) => ended(
            EnrollmentStatus::Stopped,
            "The address is suppressed.".to_owned(),
        ),
        Err(accept::Error::Template(errors)) => ended(
            EnrollmentStatus::Failed,
            format!(
                "The step's templates do not render for this person: {}",
                errors
                    .first()
                    .map_or_else(|| "unknown error".to_owned(), |error| error.detail.clone())
            ),
        ),
        Err(accept::Error::Invalid { pointer, detail }) => ended(
            EnrollmentStatus::Failed,
            format!("The step's message breaks a rule at {pointer}: {detail}"),
        ),
        Err(error) => {
            tracing::warn!(error = %error, "a step message was not created; the next pass tries again");
            return Ok(false);
        }
    };
    enrollments::end(
        tx,
        workspace,
        &[message.enrollment.uuid()],
        outcome.0,
        Some(&outcome.1),
    )
    .await?;
    Ok(false)
}

/// Writes what a campaign's chunk changed outside its enrollments: the rotation of the senders
/// it picked, and `last_error` (`no_sender` while conversations wait for any sender of the pool,
/// cleared once a message is created). Writes the row only when the error changes, so a
/// waiting campaign's version stays put.
async fn record(tx: &mut Tx, workspace: WorkspaceId, context: &Context) -> Result<(), sqlx::Error> {
    if !context.picked.is_empty() {
        let assigned = context.rotation.assigned();
        let (ids, at): (Vec<Uuid>, Vec<Timestamp>) = assigned
            .into_iter()
            .filter(|(id, _)| context.picked.contains(id))
            .map(|(id, at)| (id, Timestamp(at)))
            .unzip();
        sqlx::query!(
            "INSERT INTO campaign_sender_rotation (workspace_id, campaign_id, sender_identity_id, last_assigned_at)
             SELECT $1, $2, r.id, r.at FROM UNNEST($3::uuid[], $4::timestamptz[]) AS r(id, at)
             ON CONFLICT (workspace_id, campaign_id, sender_identity_id)
             DO UPDATE SET last_assigned_at = excluded.last_assigned_at",
            workspace.uuid(),
            context.id,
            &ids,
            &at as _,
        )
        .execute(&mut **tx)
        .await?;
    }
    let waiting = context.no_sender && context.created == 0;
    let flagged = context.last_error.as_deref() == Some("no_sender");
    if waiting && !flagged {
        sqlx::query!(
            "UPDATE campaigns SET last_error = $3 WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            context.id,
            json!({
                "code": "no_sender",
                "detail": "No sender of the campaign's pool can send now; its conversations wait and continue by themselves when one is back.",
                "at": crate::process::now(),
            }),
        )
        .execute(&mut **tx)
        .await?;
    } else if flagged && context.created > 0 {
        sqlx::query!(
            "UPDATE campaigns SET last_error = NULL WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            context.id,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Updates a campaign's row so its version moves with a write of a row its object shows.
async fn touch(tx: &mut Tx, workspace: WorkspaceId, campaign: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE campaigns SET updated_at = now() WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        campaign,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Creates a generated step's message in `message.generate`'s checkpoint: the snippets become
/// the message's `variables`. The job's sender is checked again first: a sender that left the
/// pool while the content was written creates nothing, and the next pass gives the step to
/// another sender. True when the message exists now.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn create_generated(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    message: &StepMessage,
    snippets: Option<Map<String, Value>>,
    validation_required: bool,
) -> Result<bool, sqlx::Error> {
    if validation_required {
        // An address or the campaign's audience may change while AI writes the snippets.
        sqlx::query("SELECT id FROM campaigns WHERE workspace_id = $1 AND id = $2 FOR UPDATE")
            .bind(workspace.uuid())
            .bind(message.campaign.uuid())
            .fetch_optional(&mut **tx)
            .await?;
        sqlx::query("SELECT p.id FROM people p JOIN enrollments e ON e.workspace_id = p.workspace_id AND e.person_id = p.id
                     WHERE e.workspace_id = $1 AND e.id = $2 FOR SHARE OF p")
            .bind(workspace.uuid()).bind(message.enrollment.uuid()).fetch_optional(&mut **tx).await?;
        if !super::recipients::ready_or_enqueue(tx, workspace, message.campaign).await? {
            return Ok(false);
        }
    }
    let in_pool = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM sender_identities i JOIN campaigns c ON c.workspace_id = i.workspace_id AND c.id = $3
              WHERE i.workspace_id = $1 AND i.id = $2 AND i.enabled AND i.archived_at IS NULL AND c.status = 'active'
                AND (i.tags && c.sender_tags
                     OR EXISTS (SELECT 1 FROM campaign_senders s
                                 WHERE s.workspace_id = i.workspace_id AND s.campaign_id = c.id AND s.sender_identity_id = i.id))
           ) AS "in_pool!""#,
        workspace.uuid(),
        message.identity.uuid(),
        message.campaign.uuid(),
    )
    .fetch_one(&mut **tx)
    .await?;
    if !in_pool {
        return Ok(false);
    }
    let generated = StepMessage {
        variables: snippets,
        ..message.clone()
    };
    create_one(tx, keys, workspace, &generated).await
}

/// The ids a step message names, for a job's payload.
impl From<&StepMessage> for MessageGenerate {
    fn from(message: &StepMessage) -> Self {
        Self {
            enrollment: message.enrollment.uuid(),
            campaign: message.campaign.uuid(),
            step: message.step.uuid(),
            position: message.position,
            step_revision: message.step_revision,
            variant: message.variant.uuid(),
            variant_version: message.variant_version,
            identity: message.identity.uuid(),
            send_at: message.send_at,
        }
    }
}

impl MessageGenerate {
    /// The step message this job creates, its snippets not yet written.
    #[must_use]
    pub fn message(&self) -> StepMessage {
        StepMessage {
            enrollment: Id::<Enrollment>::from_uuid(self.enrollment),
            campaign: Id::<Campaign>::from_uuid(self.campaign),
            step: Id::<Step>::from_uuid(self.step),
            position: self.position,
            step_revision: self.step_revision,
            variant: Id::<Variant>::from_uuid(self.variant),
            variant_version: self.variant_version,
            identity: Id::<SenderIdentity>::from_uuid(self.identity),
            variables: None,
            snippets_fallback: None,
            send_at: self.send_at,
        }
    }
}

/// The sender a message outside the campaign's conversations goes from when none is named (a
/// step's content for a person not enrolled, a preview): the pool's usable sender assigned least
/// recently, without recording an assignment. `None` when no sender of the pool is usable.
///
/// # Errors
///
/// The database is unavailable.
pub async fn pool_sender(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<Option<Id<SenderIdentity>>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT i.id AS "id: Id<SenderIdentity>"
             FROM campaigns m
             JOIN sender_identities i ON i.workspace_id = m.workspace_id AND i.enabled AND i.archived_at IS NULL
             JOIN connections c ON c.workspace_id = i.workspace_id AND c.id = i.connection_id
             LEFT JOIN campaign_sender_rotation r
               ON r.workspace_id = m.workspace_id AND r.campaign_id = m.id AND r.sender_identity_id = i.id
            WHERE m.workspace_id = $1 AND m.id = $2
              AND c.status = 'active' AND NOT c.paused AND coalesce(c.paused_until <= now(), true)
              AND coalesce(c.next_claim_at <= now(), true)
              AND (i.tags && m.sender_tags
                   OR EXISTS (SELECT 1 FROM campaign_senders s
                               WHERE s.workspace_id = m.workspace_id AND s.campaign_id = m.id AND s.sender_identity_id = i.id))
            ORDER BY r.last_assigned_at NULLS FIRST, i.id
            LIMIT 1"#,
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await
}
