//! The campaign resources under `/v1`: `campaigns` and `enrollments`.
//!
//! Reads need `campaigns:read`; writes need `campaigns:write`. Every effectful `POST` takes an
//! `Idempotency-Key` (the idempotency middleware enforces it before these handlers run).
//!
//! A campaign can be updated, so it carries `version`, answered as `ETag` wherever it is
//! returned alone; an update takes an optional `If-Match`, checked under the campaign's row lock
//! before anything is written (`http::versioning`). Its `steps` replace the ordered list (see
//! `campaigns::steps`). `start` and `pause` answer the campaign; `DELETE` answers `204` for a
//! campaign that never sent (it is removed) and `200` with the archived campaign otherwise.
//!
//! Enrolling up to 100 people answers `201` with the enrollments and the people skipped; more
//! answer `202` with the `enrollment.add` job and its `Location`.

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::enrollments::{
    self, Audience, Enrolled, EnrollmentAdd, EnrollmentFilters, EnrollmentObject, INLINE_MAX,
    LIST_MAX,
};
use super::steps::{STEPS_MAX, StepInput};
use super::{CampaignFilters, CampaignObject, Changes, Deleted, nullable};
use crate::domain::campaigns::{CampaignStatus, EnrollmentStatus, OnSenderRemoved, StopOnReply};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{
    Campaign, Enrollment, Group, Id, Person, Segment, SenderIdentity, SendingDomain,
};
use crate::domain::scope::Scope;
use crate::domain::senders::SendWindow;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::authority::Principal;
use crate::jobs::{self, Queue};
use crate::pagination::{COUNT_CAP, Include, ListQuery, Order, Page, PageParams};
use crate::problem::{ApiResult, Problem};

/// The largest campaign body, 1.5 MiB, as the idempotency layer buffers: the most content a
/// campaign holds (`steps::CONTENT_MAX`), so one request can write any campaign.
const BODY_LIMIT: usize = 3 << 19;

/// The routes of campaigns and enrollments.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(
            OpenApiRouter::new()
                .routes(routes!(list_campaigns, create_campaign))
                .routes(routes!(retrieve_campaign, update_campaign, delete_campaign))
                .layer(DefaultBodyLimit::max(BODY_LIMIT)),
        )
        .routes(routes!(start_campaign))
        .routes(routes!(pause_campaign))
        .routes(routes!(list_enrollments, create_enrollments))
        .routes(routes!(retrieve_enrollment))
        .routes(routes!(stop_enrollment))
}

/// Who names a winner: the person behind the credential (`usr_…`).
fn actor(principal: &Principal) -> String {
    principal.actor.user().to_string()
}

// ───────────────────────────── campaigns ─────────────────────────────

/// A campaign's pool in a create or an update; a field left out keeps its value.
#[derive(Debug, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct SendersInput {
    /// The identities the campaign names (`sid_…`), at most 500; replaces the list.
    #[garde(length(max = 500))]
    #[schema(value_type = Option<Vec<String>>)]
    identity_ids: Option<Vec<Id<SenderIdentity>>>,
    /// Tags whose enabled identities join the pool, at most 20, each 1 to 50 characters.
    #[garde(length(max = 20), inner(inner(length(chars, min = 1, max = 50))))]
    tags: Option<Vec<String>>,
    /// `reassign` (default) or `stop`.
    #[garde(skip)]
    on_sender_removed: Option<OnSenderRemoved>,
}

/// A campaign's schedule in a create or an update; a field left out keeps its value.
#[derive(Debug, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ScheduleInput {
    /// An IANA time zone, such as `Europe/Madrid` (default `UTC`).
    #[garde(length(min = 1, max = 64))]
    timezone: Option<String>,
    /// The days (1 Monday to 7 Sunday) and hours (`HH:MM`, on 5-minute marks) campaign mail may
    /// be sent; `null` removes it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<SendWindow>)]
    send_window: Option<Option<SendWindow>>,
    /// No step runs before this instant; `null` removes it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    start_at: Option<Option<Timestamp>>,
}

/// A campaign's tracking in a create or an update; a field left out keeps its value.
#[derive(Debug, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct TrackingInput {
    /// A sending domain whose tracking host the links use (`dom_…`); `null`: the platform's.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    domain_id: Option<Option<Id<SendingDomain>>>,
    #[garde(skip)]
    opens: Option<bool>,
    #[garde(skip)]
    clicks: Option<bool>,
}

/// A campaign's stop rules in a create or an update; a field left out keeps its value.
#[derive(Debug, Default, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct StopRulesInput {
    /// `all` (default), `campaign` or `none`.
    #[garde(skip)]
    on_reply: Option<StopOnReply>,
    #[garde(skip)]
    company_on_reply: Option<bool>,
    /// 0 to 8,760 hours (default 72).
    #[garde(range(min = 0, max = 8_760))]
    cooldown_hours: Option<i32>,
}

/// The body of `POST /campaigns`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateCampaign {
    /// 1 to 200 characters.
    #[garde(length(chars, min = 1, max = 200))]
    name: String,
    /// The steps in order, at most 50, holding at most 1.5 MiB of names, prompts and templates.
    #[garde(length(max = STEPS_MAX), dive)]
    steps: Option<Vec<StepInput>>,
    #[garde(dive)]
    senders: Option<SendersInput>,
    #[garde(dive)]
    schedule: Option<ScheduleInput>,
    #[garde(dive)]
    tracking: Option<TrackingInput>,
    #[garde(dive)]
    stop_rules: Option<StopRulesInput>,
}

/// The body of `PATCH /campaigns/{id}`: every member optional; `steps` replaces the ordered list.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateCampaign {
    #[garde(length(chars, min = 1, max = 200))]
    name: Option<String>,
    /// The whole ordered list, at most 50: a step by `id` alone stays unchanged, by `id` with
    /// fields takes them, without `id` is new, and one left out is removed (refused once it has
    /// sent). Together the campaign's steps then, kept ones included, hold at most 1.5 MiB of
    /// names, prompts and templates.
    #[garde(length(max = STEPS_MAX), dive)]
    steps: Option<Vec<StepInput>>,
    #[garde(dive)]
    senders: Option<SendersInput>,
    #[garde(dive)]
    schedule: Option<ScheduleInput>,
    #[garde(dive)]
    tracking: Option<TrackingInput>,
    #[garde(dive)]
    stop_rules: Option<StopRulesInput>,
}

/// The parts of a create or an update, checked and gathered.
fn changes(
    name: Option<String>,
    steps: Option<Vec<StepInput>>,
    senders: Option<SendersInput>,
    schedule: Option<ScheduleInput>,
    tracking: Option<TrackingInput>,
    stop_rules: Option<StopRulesInput>,
) -> Result<Changes, Problem> {
    let senders = senders.unwrap_or_default();
    let schedule = schedule.unwrap_or_default();
    let tracking = tracking.unwrap_or_default();
    let stop_rules = stop_rules.unwrap_or_default();
    if let Some(zone) = &schedule.timezone
        && jiff::tz::TimeZone::get(zone).is_err()
    {
        return Err(Problem::invalid_field(
            "/schedule/timezone",
            "format",
            "The time zone is an IANA name, such as `Europe/Madrid`.",
        ));
    }
    let send_window = match schedule.send_window {
        Some(Some(window)) => Some(Some(
            SendWindow::parse(&window.days, &window.start, &window.end).map_err(|error| {
                Problem::invalid_field("/schedule/send_window", "invalid", error.to_string())
            })?,
        )),
        other => other,
    };
    let identity_ids = senders.identity_ids.map(|mut ids| {
        ids.sort_by_key(|id| id.uuid());
        ids.dedup();
        ids
    });
    let tags = senders.tags.map(|tags| {
        let mut tags: Vec<String> = tags.into_iter().map(|tag| tag.trim().to_owned()).collect();
        tags.sort();
        tags.dedup();
        tags
    });
    Ok(Changes {
        name,
        identity_ids,
        tags,
        on_sender_removed: senders.on_sender_removed,
        timezone: schedule.timezone,
        send_window,
        start_at: schedule.start_at,
        tracking_domain: tracking.domain_id,
        track_opens: tracking.opens,
        track_clicks: tracking.clicks,
        stop_on_reply: stop_rules.on_reply,
        stop_company_on_reply: stop_rules.company_on_reply,
        cooldown_hours: stop_rules.cooldown_hours,
        steps,
    })
}

/// List the workspace's campaigns, newest first by default; the variants' bodies are left out.
///
/// The enrollment summary is left out too (`enrollments` is `null`): retrieving a campaign counts
/// its enrollments.
#[utoipa::path(
    get,
    path = "/campaigns",
    tag = "Campaigns",
    operation_id = "campaigns.list",
    params(
        ("limit" = Option<i64>, Query, description = "Campaigns per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<CampaignStatus>, Query, description = "Campaigns in this status."),
        ("q" = Option<String>, Query, description = "A prefix of the name, ignoring case."),
    ),
    responses(
        (status = 200, description = "A page of campaigns.", body = Page<CampaignObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_campaigns(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(filters): Query<CampaignFilters>,
) -> ApiResult<Json<Page<CampaignObject>>> {
    principal.require(Scope::CampaignsRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "campaigns", "id", &filters, &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = super::list(
        &mut tx,
        ws,
        &filters,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(super::count(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |campaign| campaign.id.uuid(),
    )))
}

/// Create a campaign as a `draft`, with its steps, pool, schedule, tracking and stop rules.
#[utoipa::path(
    post,
    path = "/campaigns",
    tag = "Campaigns",
    operation_id = "campaigns.create",
    request_body(content = CreateCampaign, example = json!({
        "name": "Q4 founders",
        "steps": [
            {"name": "Intro", "variants": [
                {"subject": "Quick question, {{ person.given_name | default(\"there\") }}", "html": "<p>Hi {{ person.given_name | default(\"there\") }},</p><p>Do you have ten minutes this week?</p>"},
                {"subject": "Ten minutes, {{ person.company | default(\"your team\") }}?", "html": "<p>Hello,</p><p>Could we talk this week?</p>"}
            ]},
            {"name": "Follow-up", "delay_seconds": 172800, "variants": [{"subject": "Re: quick question", "html": "<p>Any thoughts?</p>"}]}
        ],
        "senders": {"tags": ["outbound"], "on_sender_removed": "reassign"},
        "schedule": {"timezone": "Europe/Madrid", "send_window": {"days": [1, 2, 3, 4, 5], "start": "09:00", "end": "17:00"}},
        "tracking": {"opens": false, "clicks": true},
        "stop_rules": {"on_reply": "all"}
    })),
    responses(
        (status = 201, description = "The campaign.", body = CampaignObject,
         headers(("ETag" = String, description = "The campaign's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "A sender identity or the tracking domain does not exist (`not_found`)."),
        (status = 413, description = "The body is larger than 1.5 MiB (`payload_too_large`)."),
        (status = 422, description = "The body is invalid, a template does not parse, or the steps would hold more than 1.5 MiB."),
    ),
    security(("bearer" = []))
)]
async fn create_campaign(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateCampaign>,
) -> ApiResult<(StatusCode, Tagged<CampaignObject>)> {
    principal.require(Scope::CampaignsWrite)?;
    let changes = changes(
        Some(body.name),
        body.steps,
        body.senders,
        body.schedule,
        body.tracking,
        body.stop_rules,
    )?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let campaign =
        super::create(&mut tx, principal.workspace, &changes, &actor(&principal)).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: campaign.version,
            body: campaign,
        },
    ))
}

/// Retrieve a campaign with its steps and their variants' bodies.
///
/// Its enrollments are counted as it is read (`enrollments`): how many are in each status, how
/// many live ones are at each step, and when the campaign's next email may go.
#[utoipa::path(
    get,
    path = "/campaigns/{id}",
    tag = "Campaigns",
    operation_id = "campaigns.retrieve",
    params(("id" = Id<Campaign>, Path, description = "The campaign id (`cmp_…`).")),
    responses(
        (status = 200, description = "The campaign.", body = CampaignObject,
         headers(("ETag" = String, description = "The campaign's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:read`."),
        (status = 404, description = "No such campaign in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_campaign(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Campaign>>,
) -> ApiResult<Tagged<CampaignObject>> {
    principal.require(Scope::CampaignsRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let campaign = super::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("campaign"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: campaign.version,
        body: campaign,
    })
}

/// Change a campaign: its settings, its pool, or its steps.
///
/// Steps are the whole ordered list; a changed step gets a new revision for messages not yet
/// created.
#[utoipa::path(
    patch,
    path = "/campaigns/{id}",
    tag = "Campaigns",
    operation_id = "campaigns.update",
    params(("id" = Id<Campaign>, Path, description = "The campaign id (`cmp_…`)."), IfMatch),
    request_body(content = UpdateCampaign, example = json!({
        "steps": [
            {"id": "stp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"},
            {"id": "stp_0190f8a2b4c87a10b6d2e4f6a8c0e2f5", "delay_seconds": 259200}
        ],
        "senders": {"tags": ["outbound", "emea"]}
    })),
    responses(
        (status = 200, description = "The campaign.", body = CampaignObject,
         headers(("ETag" = String, description = "The campaign's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such campaign, sender identity or tracking domain in this workspace."),
        (status = 409, description = "The campaign is archived, or a removed step has sent mail (`invalid_state`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 413, description = "The body is larger than 1.5 MiB (`payload_too_large`)."),
        (status = 422, description = "The body is invalid, a template does not parse, or the steps would hold more than 1.5 MiB; nothing changed."),
    ),
    security(("bearer" = []))
)]
async fn update_campaign(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Campaign>>,
    if_match: IfMatch,
    Json(body): Json<UpdateCampaign>,
) -> ApiResult<Tagged<CampaignObject>> {
    principal.require(Scope::CampaignsWrite)?;
    let changes = changes(
        body.name,
        body.steps,
        body.senders,
        body.schedule,
        body.tracking,
        body.stop_rules,
    )?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let locked = super::lock(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("campaign"))?;
    if_match.check(locked.version)?;
    let campaign = super::update(&mut tx, ws, &locked, &changes, &actor(&principal)).await?;
    tx.commit().await?;
    jobs::wake(&state.db, Queue::Enrollment).await;
    Ok(Tagged {
        version: campaign.version,
        body: campaign,
    })
}

/// Delete a campaign.
///
/// A campaign that never sent is removed with its steps and enrollments; one that sent is archived,
/// kept for its history, and its live enrollments are stopped.
#[utoipa::path(
    delete,
    path = "/campaigns/{id}",
    tag = "Campaigns",
    operation_id = "campaigns.delete",
    params(("id" = Id<Campaign>, Path, description = "The campaign id (`cmp_…`).")),
    responses(
        (status = 200, description = "The campaign had sent: it is archived.", body = CampaignObject,
         headers(("ETag" = String, description = "The campaign's `version`, quoted."))),
        (status = 204, description = "The campaign had never sent: it is removed."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such campaign in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_campaign(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Campaign>>,
) -> ApiResult<Response> {
    principal.require(Scope::CampaignsWrite)?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let locked = super::lock(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("campaign"))?;
    let deleted = super::delete(&mut tx, ws, &locked).await?;
    tx.commit().await?;
    Ok(match deleted {
        Deleted::Removed => StatusCode::NO_CONTENT.into_response(),
        Deleted::Archived(campaign) => Tagged {
            version: campaign.version,
            body: *campaign,
        }
        .into_response(),
    })
}

/// Start a campaign from `draft` or `paused`.
///
/// It is `materialising` until its job makes it `active` and creates the messages already due.
#[utoipa::path(
    post,
    path = "/campaigns/{id}/start",
    tag = "Campaigns",
    operation_id = "campaigns.start",
    params(("id" = Id<Campaign>, Path, description = "The campaign id (`cmp_…`).")),
    responses(
        (status = 200, description = "The campaign, `materialising`.", body = CampaignObject,
         headers(("ETag" = String, description = "The campaign's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such campaign in this workspace."),
        (status = 409, description = "The campaign is not `draft` or `paused`, or has no step with a variant (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn start_campaign(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Campaign>>,
) -> ApiResult<Tagged<CampaignObject>> {
    principal.require(Scope::CampaignsWrite)?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let locked = super::lock(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("campaign"))?;
    let campaign = super::start(&mut tx, ws, &locked).await?;
    tx.commit().await?;
    jobs::wake(&state.db, Queue::Enrollment).await;
    Ok(Tagged {
        version: campaign.version,
        body: campaign,
    })
}

/// Pause a campaign: no new message is created; queued messages wait until it is started again.
#[utoipa::path(
    post,
    path = "/campaigns/{id}/pause",
    tag = "Campaigns",
    operation_id = "campaigns.pause",
    params(("id" = Id<Campaign>, Path, description = "The campaign id (`cmp_…`).")),
    responses(
        (status = 200, description = "The campaign, `paused`.", body = CampaignObject,
         headers(("ETag" = String, description = "The campaign's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such campaign in this workspace."),
        (status = 409, description = "The campaign is not `active` or `materialising` (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn pause_campaign(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Campaign>>,
) -> ApiResult<Tagged<CampaignObject>> {
    principal.require(Scope::CampaignsWrite)?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let locked = super::lock(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("campaign"))?;
    let campaign = super::pause(&mut tx, ws, &locked).await?;
    tx.commit().await?;
    Ok(Tagged {
        version: campaign.version,
        body: campaign,
    })
}

// ───────────────────────────── enrollments ─────────────────────────────

/// List the workspace's enrollments, newest first by default.
#[utoipa::path(
    get,
    path = "/enrollments",
    tag = "Campaigns",
    operation_id = "enrollments.list",
    params(
        ("limit" = Option<i64>, Query, description = "Enrollments per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000: with `sender_identity_id` and `status=active`, what removing a sender would touch."),
        ("campaign_id" = Option<Id<Campaign>>, Query, description = "Enrollments of this campaign."),
        ("person_id" = Option<Id<Person>>, Query, description = "Enrollments of this person."),
        ("sender_identity_id" = Option<Id<SenderIdentity>>, Query, description = "Conversations kept by this sender."),
        ("status" = Option<EnrollmentStatus>, Query, description = "Enrollments in this status."),
    ),
    responses(
        (status = 200, description = "A page of enrollments.", body = Page<EnrollmentObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_enrollments(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(filters): Query<EnrollmentFilters>,
) -> ApiResult<Json<Page<EnrollmentObject>>> {
    principal.require(Scope::CampaignsRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "enrollments", "id", &filters, &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = enrollments::list(
        &mut tx,
        ws,
        &filters,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(enrollments::count(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |enrollment| enrollment.id.uuid(),
    )))
}

/// The body of `POST /enrollments`: a campaign and exactly one source of people.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateEnrollments {
    /// The campaign (`cmp_…`).
    #[garde(skip)]
    #[schema(value_type = String)]
    campaign_id: Id<Campaign>,
    /// People by id (`per_…`), at most 1,000.
    #[garde(length(min = 1, max = LIST_MAX))]
    #[schema(value_type = Option<Vec<String>>)]
    person_ids: Option<Vec<Id<Person>>>,
    /// People by address, at most 1,000; an address that is no person of the workspace is
    /// skipped.
    #[garde(length(min = 1, max = LIST_MAX))]
    #[schema(value_type = Option<Vec<String>>)]
    emails: Option<Vec<EmailAddress>>,
    /// A group's members (`grp_…`).
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    group_id: Option<Id<Group>>,
    /// The people a segment matches (`seg_…`).
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    segment_id: Option<Id<Segment>>,
}

/// Enroll people into a campaign.
///
/// Up to 100 at once (`201`), more through a job (`202`). Unknown people, suppressed addresses and
/// people already enrolled are skipped.
#[utoipa::path(
    post,
    path = "/enrollments",
    tag = "Campaigns",
    operation_id = "enrollments.create",
    request_body(content = CreateEnrollments, example = json!({
        "campaign_id": "cmp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
        "group_id": "grp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"
    })),
    responses(
        (status = 201, description = "Up to 100 people: the enrollments made and the people skipped.", body = Enrolled),
        (status = 202, description = "More than 100 people: the `enrollment.add` job; `Location` names it.", body = jobs::http::JobObject,
         headers(("Location" = String, description = "The job's path."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such campaign, group or segment in this workspace."),
        (status = 409, description = "The campaign is archived (`invalid_state`)."),
        (status = 422, description = "The body is invalid, or names no source of people or more than one."),
    ),
    security(("bearer" = []))
)]
async fn create_enrollments(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateEnrollments>,
) -> ApiResult<Response> {
    principal.require(Scope::CampaignsWrite)?;
    let ws = principal.workspace;
    let sources = [
        body.person_ids.is_some(),
        body.emails.is_some(),
        body.group_id.is_some(),
        body.segment_id.is_some(),
    ];
    if sources.iter().filter(|given| **given).count() != 1 {
        return Err(Problem::invalid_field(
            "",
            "required",
            "Give exactly one of `person_ids`, `emails`, `group_id` or `segment_id`.",
        ));
    }
    let audience = if let Some(ids) = body.person_ids {
        let mut ids: Vec<Uuid> = ids.iter().map(|id| id.uuid()).collect();
        ids.sort_unstable();
        ids.dedup();
        Audience::People(ids)
    } else if let Some(emails) = body.emails {
        let mut keys: Vec<String> = emails.iter().map(EmailAddress::key).collect();
        keys.sort();
        keys.dedup();
        Audience::Emails(keys)
    } else if let Some(group) = body.group_id {
        Audience::Group(group.uuid())
    } else if let Some(segment) = body.segment_id {
        Audience::Segment(segment.uuid())
    } else {
        return Err(Problem::invalid_field(
            "",
            "required",
            "No source of people.",
        ));
    };
    let inline = i64::try_from(INLINE_MAX).unwrap_or(i64::MAX);
    let mut tx = state.db.begin_in(ws).await?;
    let size = enrollments::size(&mut tx, ws, &audience, inline).await?;
    if size <= inline {
        let enrolled = enrollments::enroll(&mut tx, ws, body.campaign_id, &audience).await?;
        tx.commit().await?;
        jobs::wake(&state.db, Queue::Enrollment).await;
        return Ok((StatusCode::CREATED, Json(enrolled)).into_response());
    }
    let status = sqlx::query_scalar!(
        "SELECT status FROM campaigns WHERE workspace_id = $1 AND id = $2",
        ws.uuid(),
        body.campaign_id.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Problem::not_found("campaign"))?;
    if status == CampaignStatus::Archived.as_str() {
        return Err(Problem::invalid_state(
            "An archived campaign takes no new people.",
        ));
    }
    // A job's payload stays small: addresses become the ids of the people they name.
    let audience = match audience {
        Audience::Emails(keys) => Audience::People(
            sqlx::query_scalar!(
                "SELECT id FROM people WHERE workspace_id = $1 AND email_key = ANY($2) ORDER BY id",
                ws.uuid(),
                &keys,
            )
            .fetch_all(&mut *tx)
            .await?,
        ),
        other => other,
    };
    let request = crate::http::context::current()
        .map_or_else(|| Uuid::now_v7().to_string(), |context| context.request_id);
    let job = jobs::enqueue(
        &mut tx,
        ws,
        &EnrollmentAdd {
            campaign: body.campaign_id.uuid(),
            audience,
            request,
        },
        None,
    )
    .await?;
    let object = jobs::http::read(&mut tx, ws, job)
        .await?
        .ok_or_else(|| Problem::internal(&"an enqueued job is not readable"))?;
    tx.commit().await?;
    jobs::wake(&state.db, Queue::Enrollment).await;
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/v1/jobs/{job}"))],
        Json(object),
    )
        .into_response())
}

/// Retrieve an enrollment.
///
/// Its person, position, next run, status and the sender its conversation keeps or waits for.
#[utoipa::path(
    get,
    path = "/enrollments/{id}",
    tag = "Campaigns",
    operation_id = "enrollments.retrieve",
    params(("id" = Id<Enrollment>, Path, description = "The enrollment id (`enr_…`).")),
    responses(
        (status = 200, description = "The enrollment.", body = EnrollmentObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:read`."),
        (status = 404, description = "No such enrollment in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_enrollment(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Enrollment>>,
) -> ApiResult<Json<EnrollmentObject>> {
    principal.require(Scope::CampaignsRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let enrollment = enrollments::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("enrollment"))?;
    tx.commit().await?;
    Ok(Json(enrollment))
}

/// Stop an enrollment: no further step runs, and its message still queued is cancelled.
#[utoipa::path(
    post,
    path = "/enrollments/{id}/stop",
    tag = "Campaigns",
    operation_id = "enrollments.stop",
    params(("id" = Id<Enrollment>, Path, description = "The enrollment id (`enr_…`).")),
    responses(
        (status = 200, description = "The enrollment, `stopped`.", body = EnrollmentObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such enrollment in this workspace."),
        (status = 409, description = "The enrollment already ended (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn stop_enrollment(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Enrollment>>,
) -> ApiResult<Json<EnrollmentObject>> {
    principal.require(Scope::CampaignsWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let enrollment = enrollments::stop(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    Ok(Json(enrollment))
}
