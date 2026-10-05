//! A campaign's steps and their variants: how they are read inside the campaign, and how an
//! update replaces the ordered list.
//!
//! # The model
//!
//! A step is one email position of the campaign. What it sends is its **current revision**, an
//! immutable configuration (`step_revisions`): the delay after the previous step, whether it
//! continues the conversation's thread, its allocation and winner rule, its personalisation
//! prompt, and the variants it offers with their weights (`step_revision_variants`, one row per
//! variant and the exact version of its content). A variant's content is versioned too
//! (`variant_revisions`): a change of a subject or a body is a new version, never an edit, so a
//! message created from a revision keeps pointing at exactly the content it was created from
//! and never holds a body of its own.
//!
//! # Replacing the list
//!
//! An update's `steps` is the whole ordered list, positions following the order. Each element
//! is merged with the step it names:
//!
//! - a step given by `id` alone stays exactly as it is (only its position may change);
//! - a step given by `id` with fields takes those fields, and keeps the ones it is not given;
//! - a step without `id` is new: it needs a `name` and at least one variant;
//! - a step left out is removed, which the database refuses once it has sent mail (its
//!   messages point at it): `409 invalid_state`.
//!
//! Variants inside a step given with `variants` follow the same rules: by `id` alone a variant
//! is kept, with fields it takes them (a content change is a new version), without `id` it is
//! new (it needs a `subject` and an `html` body), and one left out is retired: it stays for the
//! messages that point at it but is no longer offered.
//!
//! A step whose configuration changed (any of the fields above, a variant's version or weight,
//! a variant added or retired) gets a **new revision**; messages created from then on use it,
//! and messages already created keep theirs. A step whose configuration did not change keeps
//! its revision, so saving the same campaign twice publishes nothing. A new revision starts a
//! new test: it has no winner unless the update names one. Name and position are not part of
//! the revision.
//!
//! Variants are listed in the order they were created, which is also the order ties are broken
//! in by the allocation and by winner selection.
//!
//! # Templates
//!
//! A variant's subject, preheader and bodies are templates. Their syntax is checked when they
//! are saved; whether they render (a value a person lacks, for example) is known only per
//! person, when the person's message is created.
//!
//! # Bounds
//!
//! A campaign has at most [`STEPS_MAX`] steps of at most [`VARIANTS_MAX`] variants, and its
//! steps hold at most [`CONTENT_MAX`] bytes of content, checked on what the campaign holds after
//! each replacement (a request alone does not bound it: steps and variants named by `id` alone
//! are kept). Together they keep a campaign one bounded object, answered whole within 2 MiB.
//!
//! Lock order: the caller holds the campaign's row; steps and variants are written after it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::Acquire as _;
use uuid::Uuid;

use super::{Error, nullable};
use crate::db::Tx;
use crate::domain::allocation::{Allocation, Objective};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Campaign, Id, Step, Variant, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::rendering::templates::{self, Part};

/// The most steps a campaign has: fifty, room for the longest sequences people write.
///
/// The count is not what keeps a campaign small; its content is ([`CONTENT_MAX`]). The count
/// bounds what the content does not show: the fixed members of each step's object in an answer
/// (ids, numbers, member names, a winner: at most about 450 bytes), and the work a replacement
/// does per step (a few statements each, its variants written together). The creation pass reads
/// only the steps its due enrollments are at, so its work follows where people are, not how long
/// the campaign is.
pub const STEPS_MAX: usize = 50;
/// The most variants a step offers: fifty.
///
/// As with steps, the content bound keeps the campaign small; the count bounds the fixed members
/// of each variant's object, about 130 bytes. Nothing does work per variant that grows with
/// their number: a step's variants are written in a fixed number of statements, the allocation
/// and the winner selection compare each variant once per choice, and the creation pass counts
/// a step's assignments in one grouped read, whatever the number of variants. Measured against
/// PostgreSQL with fifty steps of fifty variants, a debug build creates such a campaign in about
/// 0.5 s and saves it whole and unchanged in about 0.2 s, which is how long an update holds the
/// campaign's row.
pub const VARIANTS_MAX: usize = 50;
/// The longest body template, in bytes.
pub const BODY_MAX: usize = 256 * 1024;
/// The most content a campaign's steps hold, in bytes as the API's JSON writes it: the steps'
/// names and prompts, and the names, subjects, preheaders, bodies and copied addresses of the
/// variants their current revisions offer. 1.5 MiB, what one request may carry, so every
/// campaign one request can write is accepted.
///
/// This is the bound that makes a campaign one bounded object. A request is bounded by its body
/// limit, but an update keeps every step and variant it names by `id` alone, so content would
/// otherwise grow from update to update (fifty steps of fifty 256 KiB bodies are 625 MiB). It is
/// checked after every replacement of the steps, on what the campaign then holds.
///
/// With it every campaign answers within 2 MiB, the MCP's bound on one answer: 1.5 MiB of
/// content, then the fixed members of at most 2,500 variants and 50 steps, about 0.31 MiB
/// (measured at both limits), and what the content does not count at its largest (a winner on
/// every step, long version numbers, 500 named identities, 20 tags, the enrollment summary),
/// under 0.07 MiB more.
pub const CONTENT_MAX: i64 = 3 << 19;
/// The default observation window of a winner rule: a week, long enough for replies.
const WINDOW_DEFAULT: i32 = 7 * 24 * 3_600;
/// The default minimum sample of a winner rule.
const SAMPLE_DEFAULT: i32 = 100;

/// A step of a campaign as the API shows it, with its current revision.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct StepObject {
    pub id: Id<Step>,
    /// 1 for the first step.
    pub position: i32,
    pub name: String,
    /// The current revision: what messages created now are made from.
    pub revision: i32,
    /// How long after the previous step's message was sent this step is due; the first step
    /// runs when a person is enrolled, so its delay is not used.
    pub delay_seconds: i32,
    /// Whether the step's message continues the conversation's thread (a reply to the previous
    /// message) or starts a new one.
    pub same_thread: bool,
    pub allocation: Allocation,
    pub winner_rule: WinnerRuleObject,
    /// The variant this revision now sends to everyone, once chosen.
    pub winner: Option<WinnerObject>,
    /// What the AI writes each message's snippets from (the `variables` namespace); `null`: the
    /// templates alone.
    pub personalisation_prompt: Option<String>,
    /// The variants this revision offers.
    #[schema(max_items = 50)]
    pub variants: Vec<VariantObject>,
}

/// When an `automatic` step names its winner.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WinnerRuleObject {
    /// What the variants are ranked by.
    pub objective: Objective,
    /// How long after the revision was published the results are read.
    pub observation_window_seconds: i32,
    /// The messages every variant must have sent first.
    pub minimum_sample: i32,
}

/// A revision's winner.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WinnerObject {
    pub variant_id: Id<Variant>,
    pub selected_at: Timestamp,
    /// `automatic`, or the user who chose it (`usr_…`).
    pub selected_by: String,
}

/// A variant as its step offers it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct VariantObject {
    pub id: Id<Variant>,
    pub name: String,
    /// The version of its content the step's revision offers.
    pub version: i32,
    /// Its share under `weighted` allocation, 1 to 100.
    pub weight: i32,
    /// The subject template.
    pub subject: String,
    /// The hidden preview text template.
    pub preheader: Option<String>,
    /// The HTML body template: shown on a campaign retrieved alone, absent from a list (bodies
    /// can be large, and a list shows many variants).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub html: Option<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// A step in a create or an update (see the module for the merge rules).
#[derive(Debug, Clone, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StepInput {
    /// The step to keep or change (`stp_…`); absent for a new step.
    #[garde(skip)]
    pub id: Option<Id<Step>>,
    /// 1 to 200 characters; required for a new step.
    #[garde(length(chars, min = 1, max = 200))]
    pub name: Option<String>,
    /// 0 to 31,536,000 seconds after the previous step's message was sent (default 0).
    #[garde(range(min = 0, max = 31_536_000))]
    pub delay_seconds: Option<i32>,
    /// Continue the conversation's thread (default `true`).
    #[garde(skip)]
    pub same_thread: Option<bool>,
    /// `balanced` (default), `weighted` or `automatic`.
    #[garde(skip)]
    pub allocation: Option<Allocation>,
    /// The rule of an `automatic` test (default: replies, a week, 100 messages per variant).
    #[garde(dive)]
    pub winner_rule: Option<WinnerRuleInput>,
    /// The variant the step now sends to everyone (`var_…`), one of its variants; `null` clears
    /// it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    pub winner_variant_id: Option<Option<Id<Variant>>>,
    /// What the AI writes each message's snippets from, 1 to 4,000 characters; `null` removes
    /// it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    pub personalisation_prompt: Option<Option<String>>,
    /// 1 to 50 variants; required for a new step.
    #[garde(length(min = 1, max = VARIANTS_MAX), dive)]
    pub variants: Option<Vec<VariantInput>>,
}

/// A winner rule in a create or an update; a field left out keeps its value.
#[derive(Debug, Clone, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WinnerRuleInput {
    #[garde(skip)]
    pub objective: Option<Objective>,
    /// 1 to 31,536,000 seconds.
    #[garde(range(min = 1, max = 31_536_000))]
    pub observation_window_seconds: Option<i32>,
    /// 1 to 1,000,000 messages per variant.
    #[garde(range(min = 1, max = 1_000_000))]
    pub minimum_sample: Option<i32>,
}

/// A variant in a create or an update (see the module for the merge rules).
#[derive(Debug, Clone, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VariantInput {
    /// The variant to keep or change (`var_…`); absent for a new variant.
    #[garde(skip)]
    pub id: Option<Id<Variant>>,
    /// 1 to 200 characters (default: a letter by position).
    #[garde(length(chars, min = 1, max = 200))]
    pub name: Option<String>,
    /// The subject template, 1 to 1,000 characters; required for a new variant.
    #[garde(length(chars, min = 1, max = 1_000))]
    pub subject: Option<String>,
    /// The hidden preview text template, at most 1,000 characters; `null` removes it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    pub preheader: Option<Option<String>>,
    /// The HTML body template, at most 256 KiB; required for a new variant.
    #[garde(length(min = 1, max = BODY_MAX))]
    pub html: Option<String>,
    /// Copied on every message of the variant, at most 20.
    #[garde(length(max = 20))]
    #[schema(value_type = Option<Vec<String>>)]
    pub cc: Option<Vec<EmailAddress>>,
    /// Blind-copied on every message of the variant, at most 20.
    #[garde(length(max = 20))]
    #[schema(value_type = Option<Vec<String>>)]
    pub bcc: Option<Vec<EmailAddress>>,
    /// The share under `weighted` allocation, 1 to 100 (default: the current weight, else 1).
    #[garde(range(min = 1, max = 100))]
    pub weight: Option<i32>,
}

/// A revision's configuration: what decides whether a change publishes a new revision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    delay_seconds: i32,
    same_thread: bool,
    allocation: Allocation,
    objective: Objective,
    window: i32,
    sample: i32,
    prompt: Option<String>,
    /// `(variant, version, weight)`, sorted by variant (creation order).
    options: Vec<(Uuid, i32, i32)>,
}

/// A variant's content at one version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Content {
    subject: String,
    preheader: Option<String>,
    html: String,
    text: Option<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
}

/// A step as it stands, for a replacement.
struct Existing {
    id: Id<Step>,
    position: i32,
    name: String,
    revision: Option<i32>,
    config: Option<Config>,
    winner: Option<(Uuid, i32)>,
}

/// A variant as it stands, for a replacement.
struct ExistingVariant {
    step: Uuid,
    name: String,
    version: i32,
    content: Content,
}

/// The steps of `campaigns`, in order, with their current revisions; the variants' bodies only
/// when `bodies` is true.
///
/// # Errors
///
/// The database is unavailable.
pub async fn of_campaigns(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaigns: &[Uuid],
    bodies: bool,
) -> Result<HashMap<Uuid, Vec<StepObject>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT s.campaign_id, s.id AS "id: Id<Step>", s.position, s.name, r.revision, r.delay_seconds,
                  r.same_thread, r.allocation, r.ranking_objective, r.observation_window_seconds, r.minimum_sample,
                  r.personalisation_prompt, r.winner_variant_id AS "winner_variant_id: Id<Variant>",
                  r.winner_selected_at AS "winner_selected_at: Timestamp",
                  (SELECT w.selected_by FROM step_winner_selections w
                    WHERE w.workspace_id = r.workspace_id AND w.step_id = r.step_id AND w.step_revision = r.revision
                      AND w.variant_id = r.winner_variant_id
                    ORDER BY w.created_at DESC LIMIT 1) AS winner_selected_by
             FROM steps s
             JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = s.current_revision
            WHERE s.workspace_id = $1 AND s.campaign_id = ANY($2)
            ORDER BY s.campaign_id, s.position"#,
        workspace.uuid(),
        campaigns,
    )
    .fetch_all(&mut **tx)
    .await?;
    let variants = sqlx::query!(
        r#"SELECT o.step_id, o.variant_id AS "id: Id<Variant>", o.variant_version, o.weight, v.name, c.subject,
                  c.preheader, CASE WHEN $3 THEN c.html END AS html, CASE WHEN $3 THEN c.text END AS text, c.cc, c.bcc
             FROM steps s
             JOIN step_revision_variants o ON o.workspace_id = s.workspace_id AND o.step_id = s.id AND o.step_revision = s.current_revision
             JOIN variants v ON v.workspace_id = o.workspace_id AND v.id = o.variant_id
             JOIN variant_revisions c ON c.workspace_id = o.workspace_id AND c.variant_id = o.variant_id AND c.version = o.variant_version
            WHERE s.workspace_id = $1 AND s.campaign_id = ANY($2)
            ORDER BY o.step_id, o.variant_id"#,
        workspace.uuid(),
        campaigns,
        bodies,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut offered: HashMap<Uuid, Vec<VariantObject>> = HashMap::new();
    for row in variants {
        offered.entry(row.step_id).or_default().push(VariantObject {
            id: row.id,
            name: row.name,
            version: row.variant_version,
            weight: row.weight,
            subject: row.subject,
            preheader: row.preheader,
            html: row.html,
            cc: row.cc,
            bcc: row.bcc,
        });
    }
    let mut steps: HashMap<Uuid, Vec<StepObject>> = HashMap::new();
    for row in rows {
        let winner = match (row.winner_variant_id, row.winner_selected_at) {
            (Some(variant_id), Some(selected_at)) => Some(WinnerObject {
                variant_id,
                selected_at,
                selected_by: row
                    .winner_selected_by
                    .unwrap_or_else(|| "automatic".to_owned()),
            }),
            _ => None,
        };
        steps.entry(row.campaign_id).or_default().push(StepObject {
            id: row.id,
            position: row.position,
            name: row.name,
            revision: row.revision,
            delay_seconds: row.delay_seconds,
            same_thread: row.same_thread,
            allocation: row.allocation.parse().unwrap_or(Allocation::Balanced),
            winner_rule: WinnerRuleObject {
                objective: row.ranking_objective.parse().unwrap_or(Objective::Replies),
                observation_window_seconds: row.observation_window_seconds,
                minimum_sample: row.minimum_sample,
            },
            winner,
            personalisation_prompt: row.personalisation_prompt,
            variants: offered.remove(&row.id.uuid()).unwrap_or_default(),
        });
    }
    Ok(steps)
}

/// Whether every step of `campaign` can send: at least one step, each with a revision that
/// offers at least one variant. A campaign that cannot is refused at `start`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn sendable(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM steps WHERE workspace_id = $1 AND campaign_id = $2)
              AND NOT EXISTS (SELECT 1 FROM steps s
                               WHERE s.workspace_id = $1 AND s.campaign_id = $2
                                 AND NOT EXISTS (SELECT 1 FROM step_revision_variants o
                                                  WHERE o.workspace_id = s.workspace_id AND o.step_id = s.id
                                                    AND o.step_revision = s.current_revision)) AS "sendable!""#,
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Replaces the ordered steps of `campaign` with `inputs` (see the module), in the caller's
/// transaction, which holds the campaign's row. `actor` is who names a winner (`usr_…`). What
/// the campaign then holds is checked against [`CONTENT_MAX`]; a refusal leaves the caller's
/// transaction to be rolled back.
///
/// # Errors
///
/// [`Error::Invalid`] for more than [`STEPS_MAX`] steps, an id that is not the campaign's (or
/// the step's), a new step or variant without its required fields, a template that does not
/// parse, a winner that is not offered, or steps that would hold more than [`CONTENT_MAX`];
/// [`Error::InvalidState`] for a removed step that has sent mail; or the database.
pub async fn replace(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    inputs: &[StepInput],
    actor: &str,
) -> Result<(), Error> {
    if inputs.len() > STEPS_MAX {
        return Err(Error::invalid(
            "/steps",
            format!("A campaign has at most {STEPS_MAX} steps."),
        ));
    }
    let existing = existing_steps(tx, workspace, campaign).await?;
    let variants = existing_variants(tx, workspace, campaign).await?;
    let mut seen: Vec<Id<Step>> = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        if let Some(id) = input.id {
            if !existing.iter().any(|step| step.id == id) {
                return Err(Error::invalid(
                    &format!("/steps/{index}/id"),
                    "No such step in this campaign.",
                ));
            }
            if seen.contains(&id) {
                return Err(Error::invalid(
                    &format!("/steps/{index}/id"),
                    "A step is listed twice.",
                ));
            }
            seen.push(id);
        }
    }
    for step in existing.iter().filter(|step| !seen.contains(&step.id)) {
        remove_unsent(tx, workspace, step.id).await?;
    }
    for (index, input) in inputs.iter().enumerate() {
        let position = i32::try_from(index).unwrap_or(i32::MAX).saturating_add(1);
        let pointer = format!("/steps/{index}");
        let current = input
            .id
            .and_then(|id| existing.iter().find(|step| step.id == id));
        let step = match current {
            Some(step) => {
                let name = input.name.as_deref().unwrap_or(&step.name);
                if step.position != position || name != step.name {
                    sqlx::query!(
                        "UPDATE steps SET position = $3, name = $4 WHERE workspace_id = $1 AND id = $2",
                        workspace.uuid(),
                        step.id.uuid(),
                        position,
                        name,
                    )
                    .execute(&mut **tx)
                    .await?;
                }
                step.id
            }
            None => {
                let Some(name) = input.name.as_deref() else {
                    return Err(Error::invalid(
                        &format!("{pointer}/name"),
                        "A new step needs a name.",
                    ));
                };
                if input.variants.is_none() {
                    return Err(Error::invalid(
                        &format!("{pointer}/variants"),
                        "A new step needs at least one variant.",
                    ));
                }
                sqlx::query_scalar!(
                    r#"INSERT INTO steps (workspace_id, campaign_id, position, name) VALUES ($1, $2, $3, $4)
                       RETURNING id AS "id: Id<Step>""#,
                    workspace.uuid(),
                    campaign.uuid(),
                    position,
                    name,
                )
                .fetch_one(&mut **tx)
                .await?
            }
        };
        publish(
            tx, workspace, step, current, &variants, input, &pointer, actor,
        )
        .await?;
    }
    let held = content(tx, workspace, campaign).await?;
    if held > CONTENT_MAX {
        return Err(Error::invalid(
            "/steps",
            format!(
                "A campaign's steps hold at most 1.5 MiB of names, prompts and templates; these would hold {held} bytes. Make bodies shorter or remove variants."
            ),
        ));
    }
    Ok(())
}

/// The content `campaign`'s steps hold, as [`CONTENT_MAX`] counts it: each string and list the
/// API shows of a step and of the variants its current revision offers, measured as JSON
/// encodes it (quotes and escapes included; PostgreSQL's `to_json` escapes exactly the
/// characters `serde_json` does), so the figure is the bytes those values take in an answer.
///
/// # Errors
///
/// The database is unavailable.
pub(crate) async fn content(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT (coalesce(sum(octet_length(to_json(s.name)::text)
                                + octet_length(coalesce(to_json(r.personalisation_prompt)::text, 'null'))), 0)
                   + coalesce((SELECT sum(octet_length(to_json(v.name)::text) + octet_length(to_json(c.subject)::text)
                                          + octet_length(coalesce(to_json(c.preheader)::text, 'null'))
                                          + octet_length(to_json(c.html)::text)
                                          + octet_length(to_json(c.cc)::text) + octet_length(to_json(c.bcc)::text))
                                 FROM steps t
                                 JOIN step_revision_variants o
                                   ON o.workspace_id = t.workspace_id AND o.step_id = t.id AND o.step_revision = t.current_revision
                                 JOIN variants v ON v.workspace_id = o.workspace_id AND v.id = o.variant_id
                                 JOIN variant_revisions c
                                   ON c.workspace_id = o.workspace_id AND c.variant_id = o.variant_id AND c.version = o.variant_version
                                WHERE t.workspace_id = $1 AND t.campaign_id = $2), 0))::bigint AS "bytes!"
             FROM steps s
             LEFT JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = s.current_revision
            WHERE s.workspace_id = $1 AND s.campaign_id = $2"#,
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_one(&mut **tx)
    .await
}

/// The steps of `campaign` with their current configurations.
async fn existing_steps(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<Vec<Existing>, sqlx::Error> {
    let steps = sqlx::query!(
        r#"SELECT s.id AS "id: Id<Step>", s.position, s.name, s.current_revision,
                  r.delay_seconds AS "delay_seconds?", r.same_thread AS "same_thread?", r.allocation AS "allocation?",
                  r.ranking_objective AS "ranking_objective?", r.observation_window_seconds AS "window?",
                  r.minimum_sample AS "sample?", r.personalisation_prompt, r.winner_variant_id, r.winner_variant_version
             FROM steps s
             LEFT JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = s.current_revision
            WHERE s.workspace_id = $1 AND s.campaign_id = $2
            ORDER BY s.position"#,
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let options = sqlx::query!(
        "SELECT o.step_id, o.variant_id, o.variant_version, o.weight
           FROM steps s
           JOIN step_revision_variants o ON o.workspace_id = s.workspace_id AND o.step_id = s.id AND o.step_revision = s.current_revision
          WHERE s.workspace_id = $1 AND s.campaign_id = $2
          ORDER BY o.step_id, o.variant_id",
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(steps
        .into_iter()
        .map(|row| {
            let config = match (
                row.delay_seconds,
                row.same_thread,
                row.allocation,
                row.ranking_objective,
                row.window,
                row.sample,
            ) {
                (
                    Some(delay_seconds),
                    Some(same_thread),
                    Some(allocation),
                    Some(objective),
                    Some(window),
                    Some(sample),
                ) => Some(Config {
                    delay_seconds,
                    same_thread,
                    allocation: allocation.parse().unwrap_or(Allocation::Balanced),
                    objective: objective.parse().unwrap_or(Objective::Replies),
                    window,
                    sample,
                    prompt: row.personalisation_prompt,
                    options: options
                        .iter()
                        .filter(|option| option.step_id == row.id.uuid())
                        .map(|option| (option.variant_id, option.variant_version, option.weight))
                        .collect(),
                }),
                _ => None,
            };
            Existing {
                id: row.id,
                position: row.position,
                name: row.name,
                revision: row.current_revision,
                config,
                winner: row.winner_variant_id.zip(row.winner_variant_version),
            }
        })
        .collect())
}

/// Every variant of `campaign`'s steps, retired ones included, with its latest content.
async fn existing_variants(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
) -> Result<HashMap<Uuid, ExistingVariant>, sqlx::Error> {
    let rows = sqlx::query!(
        "SELECT v.id, v.step_id, v.name, v.version, c.subject, c.preheader, c.html, c.text, c.cc, c.bcc
           FROM variants v
           JOIN steps s ON s.workspace_id = v.workspace_id AND s.id = v.step_id
           JOIN variant_revisions c ON c.workspace_id = v.workspace_id AND c.variant_id = v.id AND c.version = v.version
          WHERE s.workspace_id = $1 AND s.campaign_id = $2",
        workspace.uuid(),
        campaign.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.id,
                ExistingVariant {
                    step: row.step_id,
                    name: row.name,
                    version: row.version,
                    content: Content {
                        subject: row.subject,
                        preheader: row.preheader,
                        html: row.html,
                        text: row.text,
                        cc: row.cc,
                        bcc: row.bcc,
                    },
                },
            )
        })
        .collect())
}

/// Removes a step that has never sent: its revisions, options, assignments and variants with
/// it. A step whose messages point at it is refused by their references, answered as
/// [`Error::InvalidState`]; the savepoint keeps the transaction usable for that answer. The
/// caller holds the campaign's row.
///
/// # Errors
///
/// [`Error::InvalidState`] for a step that has sent, or the database.
pub(crate) async fn remove_unsent(
    tx: &mut Tx,
    workspace: WorkspaceId,
    step: Id<Step>,
) -> Result<(), Error> {
    let mut savepoint = tx.begin().await?;
    let removed: Result<(), sqlx::Error> = async {
        let (ws, id) = (workspace.uuid(), step.uuid());
        sqlx::query!(
            "UPDATE steps SET current_revision = NULL WHERE workspace_id = $1 AND id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "DELETE FROM step_assignments WHERE workspace_id = $1 AND step_id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "DELETE FROM step_winner_selections WHERE workspace_id = $1 AND step_id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "UPDATE step_revisions SET winner_variant_id = NULL, winner_variant_version = NULL, winner_selected_at = NULL
              WHERE workspace_id = $1 AND step_id = $2 AND winner_variant_id IS NOT NULL",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "DELETE FROM step_revisions WHERE workspace_id = $1 AND step_id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "DELETE FROM variant_revisions r USING variants v
              WHERE r.workspace_id = $1 AND v.workspace_id = r.workspace_id AND v.id = r.variant_id AND v.step_id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "DELETE FROM variants WHERE workspace_id = $1 AND step_id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "DELETE FROM steps WHERE workspace_id = $1 AND id = $2",
            ws,
            id
        )
        .execute(&mut *savepoint)
        .await?;
        Ok(())
    }
    .await;
    match removed {
        Ok(()) => {
            savepoint.commit().await?;
            Ok(())
        }
        Err(sqlx::Error::Database(error))
            if matches!(error.code().as_deref(), Some("23503" | "23001")) =>
        {
            savepoint.rollback().await?;
            Err(Error::InvalidState(format!(
                "The step `{step}` has sent mail, so it cannot be removed; keep it in the list."
            )))
        }
        Err(error) => Err(error.into()),
    }
}

/// Writes the variants of a step given with `variants` and publishes a new revision when the
/// configuration changed; applies the winner the input names.
#[expect(
    clippy::too_many_arguments,
    reason = "one step's replacement needs the whole context of the list"
)]
async fn publish(
    tx: &mut Tx,
    workspace: WorkspaceId,
    step: Id<Step>,
    current: Option<&Existing>,
    variants: &HashMap<Uuid, ExistingVariant>,
    input: &StepInput,
    pointer: &str,
    actor: &str,
) -> Result<(), Error> {
    let base = current.and_then(|step| step.config.as_ref());
    let rule = input.winner_rule.clone().unwrap_or_default();
    let prompt = match &input.personalisation_prompt {
        Some(Some(prompt)) if prompt.chars().count() > 4_000 || prompt.trim().is_empty() => {
            return Err(Error::invalid(
                &format!("{pointer}/personalisation_prompt"),
                "The prompt is 1 to 4,000 characters.",
            ));
        }
        Some(prompt) => prompt.clone(),
        None => base.and_then(|config| config.prompt.clone()),
    };
    let options = match &input.variants {
        Some(given) => write_variants(tx, workspace, step, base, variants, given, pointer).await?,
        None => base
            .map(|config| config.options.clone())
            .unwrap_or_default(),
    };
    let config = Config {
        delay_seconds: input
            .delay_seconds
            .or(base.map(|config| config.delay_seconds))
            .unwrap_or(0),
        same_thread: input
            .same_thread
            .or(base.map(|config| config.same_thread))
            .unwrap_or(true),
        allocation: input
            .allocation
            .or(base.map(|config| config.allocation))
            .unwrap_or(Allocation::Balanced),
        objective: rule
            .objective
            .or(base.map(|config| config.objective))
            .unwrap_or(Objective::Replies),
        window: rule
            .observation_window_seconds
            .or(base.map(|config| config.window))
            .unwrap_or(WINDOW_DEFAULT),
        sample: rule
            .minimum_sample
            .or(base.map(|config| config.sample))
            .unwrap_or(SAMPLE_DEFAULT),
        prompt,
        options,
    };
    if config.prompt.is_some() {
        check_snippets(tx, workspace, &config.options, pointer).await?;
    }
    let unchanged = base == Some(&config);
    let revision = match (unchanged, current.and_then(|step| step.revision)) {
        (true, Some(revision)) => revision,
        (_, revision) => {
            let revision = revision.unwrap_or(0).saturating_add(1);
            insert_revision(tx, workspace, step, revision, &config).await?;
            revision
        }
    };
    let winner = match input.winner_variant_id {
        None if unchanged => return Ok(()),
        None => None,
        Some(winner) => winner,
    };
    let previous = if unchanged {
        current.and_then(|step| step.winner)
    } else {
        None
    };
    match winner {
        None if previous.is_some() => {
            sqlx::query!(
                "UPDATE step_revisions SET winner_variant_id = NULL, winner_variant_version = NULL, winner_selected_at = NULL
                  WHERE workspace_id = $1 AND step_id = $2 AND revision = $3",
                workspace.uuid(),
                step.uuid(),
                revision,
            )
            .execute(&mut **tx)
            .await?;
        }
        None => {}
        Some(variant) => {
            let Some(&(_, version, _)) =
                config.options.iter().find(|(id, ..)| *id == variant.uuid())
            else {
                return Err(Error::invalid(
                    &format!("{pointer}/winner_variant_id"),
                    "The winner is one of the step's variants.",
                ));
            };
            if previous != Some((variant.uuid(), version)) {
                choose_winner(
                    tx,
                    workspace,
                    step,
                    revision,
                    (variant.uuid(), version),
                    config.objective,
                    actor,
                    &serde_json::json!({ "chosen_by": "a person" }),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// One variant of a step's new list, decided before anything is written.
enum Planned {
    /// A variant the step has: the version its revision offers (a new one when its content
    /// changed, with that content to insert), its name and its weight.
    Kept {
        id: Uuid,
        version: i32,
        content: Option<Content>,
        name: String,
        weight: i32,
    },
    /// A new variant, at version 1.
    New {
        name: String,
        content: Content,
        weight: i32,
    },
}

/// Writes the variants of one step from `given` and returns the revision's options: new
/// variants inserted, changed content published as new versions, variants left out retired.
///
/// Every input is checked first, in order, so the first mistake is the one answered; then the
/// step's variants are written in a fixed number of statements (new variants, new content
/// versions, kept variants, retirements), whatever the number of variants, so a step of many
/// variants holds the campaign's row no longer than a step of one.
async fn write_variants(
    tx: &mut Tx,
    workspace: WorkspaceId,
    step: Id<Step>,
    base: Option<&Config>,
    variants: &HashMap<Uuid, ExistingVariant>,
    given: &[VariantInput],
    pointer: &str,
) -> Result<Vec<(Uuid, i32, i32)>, Error> {
    let weight_now = |id: Uuid| {
        base.and_then(|config| config.options.iter().find(|option| option.0 == id))
            .map(|option| option.2)
    };
    let mut planned: Vec<Planned> = Vec::with_capacity(given.len());
    let mut named: Vec<Uuid> = Vec::new();
    for (index, input) in given.iter().enumerate() {
        let at = format!("{pointer}/variants/{index}");
        match input.id {
            Some(id) => {
                let Some(variant) = variants
                    .get(&id.uuid())
                    .filter(|variant| variant.step == step.uuid())
                else {
                    return Err(Error::invalid(
                        &format!("{at}/id"),
                        "No such variant in this step.",
                    ));
                };
                if named.contains(&id.uuid()) {
                    return Err(Error::invalid(
                        &format!("{at}/id"),
                        "A variant is listed twice.",
                    ));
                }
                named.push(id.uuid());
                let content = merged(&variant.content, input, &at)?;
                let changed = content != variant.content;
                planned.push(Planned::Kept {
                    id: id.uuid(),
                    version: if changed {
                        variant.version.saturating_add(1)
                    } else {
                        variant.version
                    },
                    content: changed.then_some(content),
                    name: input.name.clone().unwrap_or_else(|| variant.name.clone()),
                    weight: input.weight.or(weight_now(id.uuid())).unwrap_or(1),
                });
            }
            None => {
                let (Some(subject), Some(html)) = (&input.subject, &input.html) else {
                    return Err(Error::invalid(
                        &at,
                        "A new variant needs a `subject` and an `html` body.",
                    ));
                };
                let content = merged(
                    &Content {
                        subject: subject.clone(),
                        preheader: None,
                        html: html.clone(),
                        text: None,
                        cc: Vec::new(),
                        bcc: Vec::new(),
                    },
                    input,
                    &at,
                )?;
                planned.push(Planned::New {
                    name: input.name.clone().unwrap_or_else(|| letter(index)),
                    content,
                    weight: input.weight.unwrap_or(1),
                });
            }
        }
    }
    let names: Vec<&str> = planned
        .iter()
        .filter_map(|plan| match plan {
            Planned::New { name, .. } => Some(name.as_str()),
            Planned::Kept { .. } => None,
        })
        .collect();
    // The new variants' ids are drawn in the order given and come back in that order; drawn in
    // one session, PostgreSQL's `uuidv7()` increases, so they also list in that order.
    let created: Vec<Uuid> = if names.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar!(
            r#"WITH fresh AS (SELECT uuidv7() AS id, n.name, n.ord FROM UNNEST($3::text[]) WITH ORDINALITY AS n(name, ord)),
                    inserted AS (INSERT INTO variants (workspace_id, id, step_id, name)
                                 SELECT $1, fresh.id, $2, fresh.name FROM fresh ORDER BY fresh.ord RETURNING 1)
               SELECT id AS "id!" FROM fresh ORDER BY ord"#,
            workspace.uuid(),
            step.uuid(),
            &names as _,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    let mut created = created.into_iter();
    let mut options: Vec<(Uuid, i32, i32)> = Vec::with_capacity(planned.len());
    let mut contents: Vec<(Uuid, i32, &Content)> = Vec::new();
    let mut kept: Vec<(Uuid, i32, &str)> = Vec::new();
    for plan in &planned {
        match plan {
            Planned::Kept {
                id,
                version,
                content,
                name,
                weight,
            } => {
                if let Some(content) = content {
                    contents.push((*id, *version, content));
                }
                kept.push((*id, *version, name));
                options.push((*id, *version, *weight));
            }
            Planned::New {
                content, weight, ..
            } => {
                let Some(id) = created.next() else {
                    // The insert answers one id per name; fewer would be the database's fault.
                    return Err(Error::Db(sqlx::Error::Protocol(
                        "a new variant's id was not returned".to_owned(),
                    )));
                };
                contents.push((id, 1, content));
                options.push((id, 1, *weight));
            }
        }
    }
    insert_contents(tx, workspace, &contents).await?;
    if !kept.is_empty() {
        let ids: Vec<Uuid> = kept.iter().map(|(id, ..)| *id).collect();
        let versions: Vec<i32> = kept.iter().map(|(_, version, _)| *version).collect();
        let names: Vec<&str> = kept.iter().map(|(.., name)| *name).collect();
        sqlx::query!(
            "UPDATE variants v SET version = u.version, name = u.name, retired_at = NULL
               FROM UNNEST($2::uuid[], $3::int[], $4::text[]) AS u(id, version, name)
              WHERE v.workspace_id = $1 AND v.id = u.id
                AND (v.version, v.name, v.retired_at IS NULL) IS DISTINCT FROM (u.version, u.name, true)",
            workspace.uuid(),
            &ids,
            &versions,
            &names as _,
        )
        .execute(&mut **tx)
        .await?;
    }
    let offered: Vec<Uuid> = options.iter().map(|(id, ..)| *id).collect();
    sqlx::query!(
        "UPDATE variants SET retired_at = now()
          WHERE workspace_id = $1 AND step_id = $2 AND id <> ALL($3) AND retired_at IS NULL",
        workspace.uuid(),
        step.uuid(),
        &offered,
    )
    .execute(&mut **tx)
    .await?;
    options.sort_by_key(|(id, ..)| *id);
    Ok(options)
}

/// `base` with the content fields `input` gives, each template's syntax checked.
fn merged(base: &Content, input: &VariantInput, at: &str) -> Result<Content, Error> {
    let content = Content {
        subject: input
            .subject
            .clone()
            .unwrap_or_else(|| base.subject.clone()),
        preheader: match &input.preheader {
            Some(preheader) => preheader.clone(),
            None => base.preheader.clone(),
        },
        html: input.html.clone().unwrap_or_else(|| base.html.clone()),
        text: None,
        cc: input.cc.as_ref().map_or_else(
            || base.cc.clone(),
            |cc| {
                cc.iter()
                    .map(|address| address.as_str().to_owned())
                    .collect()
            },
        ),
        bcc: input.bcc.as_ref().map_or_else(
            || base.bcc.clone(),
            |bcc| {
                bcc.iter()
                    .map(|address| address.as_str().to_owned())
                    .collect()
            },
        ),
    };
    if content
        .preheader
        .as_ref()
        .is_some_and(|preheader| preheader.chars().count() > 1_000)
    {
        return Err(Error::invalid(
            &format!("{at}/preheader"),
            "The preheader is at most 1,000 characters.",
        ));
    }
    if content
        .text
        .as_ref()
        .is_some_and(|text| text.len() > BODY_MAX)
    {
        return Err(Error::invalid(
            &format!("{at}/text"),
            "The text body is at most 256 KiB.",
        ));
    }
    let parts = [
        (Part::Subject, Some(content.subject.as_str())),
        (Part::Preheader, content.preheader.as_deref()),
        (Part::Html, Some(content.html.as_str())),
        (Part::Text, content.text.as_deref()),
    ];
    for (part, source) in parts {
        if let Some(source) = source {
            templates::check_syntax(part, source).map_err(|error| {
                Error::invalid(&format!("{at}{}", part.pointer()), error.detail)
            })?;
        }
    }
    Ok(content)
}

/// The default name of the `index`-th new variant of a step: `A`, `B`, … then `Variant 27`.
fn letter(index: usize) -> String {
    u8::try_from(index)
        .ok()
        .filter(|index| *index < 26)
        .map_or_else(
            || format!("Variant {}", index.saturating_add(1)),
            |index| char::from(b'A'.saturating_add(index)).to_string(),
        )
}

/// Inserts variants' contents, each `(variant, version, content)`, in one statement. The copied
/// addresses travel flattened, each with the position of the content it belongs to, and are
/// gathered back in their order.
async fn insert_contents(
    tx: &mut Tx,
    workspace: WorkspaceId,
    contents: &[(Uuid, i32, &Content)],
) -> Result<(), sqlx::Error> {
    if contents.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = contents.iter().map(|(id, ..)| *id).collect();
    let versions: Vec<i32> = contents.iter().map(|(_, version, _)| *version).collect();
    let subjects: Vec<&str> = contents
        .iter()
        .map(|(.., content)| content.subject.as_str())
        .collect();
    let preheaders: Vec<Option<&str>> = contents
        .iter()
        .map(|(.., content)| content.preheader.as_deref())
        .collect();
    let htmls: Vec<&str> = contents
        .iter()
        .map(|(.., content)| content.html.as_str())
        .collect();
    let texts: Vec<Option<&str>> = contents
        .iter()
        .map(|(.., content)| content.text.as_deref())
        .collect();
    let (mut cc, mut cc_owners, mut bcc, mut bcc_owners) = (
        Vec::<&str>::new(),
        Vec::<i64>::new(),
        Vec::<&str>::new(),
        Vec::<i64>::new(),
    );
    for ((.., content), owner) in contents.iter().zip(1_i64..) {
        for address in &content.cc {
            cc.push(address);
            cc_owners.push(owner);
        }
        for address in &content.bcc {
            bcc.push(address);
            bcc_owners.push(owner);
        }
    }
    sqlx::query!(
        "INSERT INTO variant_revisions (workspace_id, variant_id, version, subject, preheader, html, text, cc, bcc)
         SELECT $1, r.id, r.version, r.subject, r.preheader, r.html, r.text,
                ARRAY(SELECT c.address FROM UNNEST($8::text[], $9::bigint[]) WITH ORDINALITY AS c(address, owner, at)
                       WHERE c.owner = r.ord ORDER BY c.at),
                ARRAY(SELECT b.address FROM UNNEST($10::text[], $11::bigint[]) WITH ORDINALITY AS b(address, owner, at)
                       WHERE b.owner = r.ord ORDER BY b.at)
           FROM UNNEST($2::uuid[], $3::int[], $4::text[], $5::text[], $6::text[], $7::text[])
                WITH ORDINALITY AS r(id, version, subject, preheader, html, text, ord)",
        workspace.uuid(),
        &ids,
        &versions,
        &subjects as _,
        &preheaders as _,
        &htmls as _,
        &texts as _,
        &cc as _,
        &cc_owners,
        &bcc as _,
        &bcc_owners,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Inserts revision `revision` of `step` with `config` and its options, and makes it current.
async fn insert_revision(
    tx: &mut Tx,
    workspace: WorkspaceId,
    step: Id<Step>,
    revision: i32,
    config: &Config,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO step_revisions (workspace_id, step_id, revision, delay_seconds, same_thread, ranking_objective,
                                     observation_window_seconds, minimum_sample, allocation, personalisation_prompt)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        workspace.uuid(),
        step.uuid(),
        revision,
        config.delay_seconds,
        config.same_thread,
        config.objective.as_str(),
        config.window,
        config.sample,
        config.allocation.as_str(),
        config.prompt,
    )
    .execute(&mut **tx)
    .await?;
    let ids: Vec<Uuid> = config.options.iter().map(|(id, ..)| *id).collect();
    let versions: Vec<i32> = config
        .options
        .iter()
        .map(|(_, version, _)| *version)
        .collect();
    let weights: Vec<i32> = config.options.iter().map(|(.., weight)| *weight).collect();
    sqlx::query!(
        "INSERT INTO step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version, weight)
         SELECT $1, $2, $3, o.id, o.version, o.weight FROM UNNEST($4::uuid[], $5::int[], $6::int[]) AS o(id, version, weight)",
        workspace.uuid(),
        step.uuid(),
        revision,
        &ids,
        &versions,
        &weights,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE steps SET current_revision = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        step.uuid(),
        revision,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Names `winner` (a variant and its version, offered by the revision) the winner of `step`'s
/// `revision`, and records who chose it and on what evidence (`step_winner_selections`). The
/// caller holds the campaign's row and updates it, so the campaign's version moves.
///
/// # Errors
///
/// The database refused (a winner the revision does not offer breaks its reference).
#[expect(
    clippy::too_many_arguments,
    reason = "a selection records every fact it was made on"
)]
pub async fn choose_winner(
    tx: &mut Tx,
    workspace: WorkspaceId,
    step: Id<Step>,
    revision: i32,
    winner: (Uuid, i32),
    objective: Objective,
    selected_by: &str,
    evidence: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE step_revisions SET winner_variant_id = $4, winner_variant_version = $5, winner_selected_at = now()
          WHERE workspace_id = $1 AND step_id = $2 AND revision = $3",
        workspace.uuid(),
        step.uuid(),
        revision,
        winner.0,
        winner.1,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO step_winner_selections (workspace_id, step_id, step_revision, variant_id, variant_version,
                                             objective, selected_by, evidence)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        workspace.uuid(),
        step.uuid(),
        revision,
        winner.0,
        winner.1,
        objective.as_str(),
        selected_by,
        evidence,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The content a step sends a person who is not enrolled (the step-content form of
/// `POST /messages`): with `variant`, that variant's latest version (one of `step`'s, when a step
/// is given too); with `step` alone, its current revision's winner, else its first variant.
/// Answers the campaign, the step, the variant and its version; `None` when nothing matches.
///
/// # Errors
///
/// The database is unavailable.
pub async fn content_of(
    tx: &mut Tx,
    workspace: WorkspaceId,
    step: Option<Id<Step>>,
    variant: Option<Id<Variant>>,
) -> Result<Option<(Id<Campaign>, Id<Step>, Id<Variant>, i32)>, sqlx::Error> {
    if let Some(variant) = variant {
        let row = sqlx::query!(
            r#"SELECT s.campaign_id AS "campaign: Id<Campaign>", v.step_id AS "step: Id<Step>", v.version
                 FROM variants v JOIN steps s ON s.workspace_id = v.workspace_id AND s.id = v.step_id
                WHERE v.workspace_id = $1 AND v.id = $2 AND ($3::uuid IS NULL OR v.step_id = $3)"#,
            workspace.uuid(),
            variant.uuid(),
            step.map(|step| step.uuid()),
        )
        .fetch_optional(&mut **tx)
        .await?;
        return Ok(row.map(|row| (row.campaign, row.step, variant, row.version)));
    }
    let Some(step) = step else {
        return Ok(None);
    };
    let row = sqlx::query!(
        r#"SELECT s.campaign_id AS "campaign: Id<Campaign>", o.variant_id AS "variant: Id<Variant>", o.variant_version
             FROM steps s
             JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = s.current_revision
             JOIN step_revision_variants o ON o.workspace_id = r.workspace_id AND o.step_id = r.step_id AND o.step_revision = r.revision
            WHERE s.workspace_id = $1 AND s.id = $2
            ORDER BY o.variant_id = r.winner_variant_id DESC NULLS LAST, o.variant_id
            LIMIT 1"#,
        workspace.uuid(),
        step.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| (row.campaign, step, row.variant, row.variant_version)))
}

/// Checks that a step with a personalisation prompt asks the AI for something it can write: its
/// variants together read at least one snippet (`{{ variables.<name> }}`), and none reads more
/// than one call writes. A snippet name is lowercase letters, digits and `_`, starting with a
/// letter; a template printing another `variables` member gets no snippet for it.
async fn check_snippets(
    tx: &mut Tx,
    workspace: WorkspaceId,
    options: &[(Uuid, i32, i32)],
    pointer: &str,
) -> Result<(), Error> {
    let ids: Vec<Uuid> = options.iter().map(|(id, ..)| *id).collect();
    let versions: Vec<i32> = options.iter().map(|(_, version, _)| *version).collect();
    let contents = sqlx::query!(
        "SELECT c.subject, c.preheader, c.html, c.text
           FROM variant_revisions c
           JOIN UNNEST($2::uuid[], $3::int[]) AS o(id, version) ON c.variant_id = o.id AND c.version = o.version
          WHERE c.workspace_id = $1",
        workspace.uuid(),
        &ids,
        &versions,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut any = false;
    for content in contents {
        let names = super::generate::names(&[
            Some(content.subject.as_str()),
            content.preheader.as_deref(),
            Some(content.html.as_str()),
            content.text.as_deref(),
        ]);
        if names.len() > crate::ai::snippets::NAMES_MAX {
            return Err(Error::invalid(
                &format!("{pointer}/variants"),
                "A variant reads at most 10 snippets (`{{ variables.<name> }}`).",
            ));
        }
        any |= !names.is_empty();
    }
    if !any {
        return Err(Error::invalid(
            &format!("{pointer}/personalisation_prompt"),
            "A personalisation prompt needs a variant that reads a snippet, such as `{{ variables.opener }}`.",
        ));
    }
    Ok(())
}
