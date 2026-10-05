//! The automation resources of customer webhooks: `events`, `webhook_endpoints` and
//! `webhook_deliveries`.
//!
//! - `events` lists and retrieves outbox events (the CLI's stream long-polls the list with
//!   `after` and `wait`), and creates synthetic ones: `POST /events {type}` publishes a sample
//!   event of that type to every subscribed endpoint, and `{type, webhook_endpoint_id}`
//!   addresses it to one endpoint whatever its subscriptions, which is how an endpoint is
//!   tested.
//! - `webhook_endpoints` holds the customer's URLs with their subscribed types; the secret is
//!   shown once, on creation and on `rotate_secret`; `enabled` is a reversible switch; `replay`
//!   reopens the endpoint's deliveries for events since an instant.
//! - `webhook_deliveries` shows one delivery per event and endpoint with its latest attempt;
//!   `retry` reopens one now.
//!
//! Reads need `automation:read`; writes need `automation:manage`. Every effectful `POST` takes
//! an `Idempotency-Key` (the idempotency middleware enforces it before these handlers run).
//!
//! An endpoint can be updated, so it carries `version` and answers it as `ETag` wherever it is
//! returned alone (create, retrieve, update, `replay`, `rotate_secret`); its update takes an
//! optional `If-Match`, checked under the row's lock before anything is written
//! (`http::versioning`).

use crate::domain::webhooks::{self as policy, Filters};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::EventType;
use super::deliver::{self, DeliveryFilters, DeliveryObject, DeliveryState, Reopen, ReopenError};
use super::endpoints::{self, EndpointChanges, EndpointError, EndpointObject, NewEndpoint};
use super::outbox::{self, EventFilters, EventObject};
use crate::crypto::Keys;
use crate::db::Database;
use crate::domain::ids::{Id, OutboxEvent, WebhookDelivery, WebhookEndpoint};
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::authority::Principal;
use crate::jobs::{self, Queue};
use crate::pagination::{self, After, COUNT_CAP, Include, Key, ListQuery, Order, Page, PageParams};
use crate::problem::{ApiResult, Problem};

/// The longest a list of events waits for a first event (`wait`), in seconds.
const WAIT_MAX: u64 = 25;
/// How often a waiting list of events looks again.
const WAIT_POLL: Duration = Duration::from_secs(1);

/// The routes of the three resources.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_events, create_event))
        .routes(routes!(retrieve_event))
        .routes(routes!(list_endpoints, create_endpoint))
        .routes(routes!(retrieve_endpoint, update_endpoint, delete_endpoint))
        .routes(routes!(replay_endpoint))
        .routes(routes!(rotate_endpoint_secret))
        .routes(routes!(list_deliveries))
        .routes(routes!(retrieve_delivery))
        .routes(routes!(retry_delivery))
}

// ───────────────────────────── events ─────────────────────────────

/// The filters of `GET /events`.
#[derive(Debug, Deserialize)]
struct EventQuery {
    #[serde(rename = "type")]
    kind: Option<EventType>,
    after: Option<Id<OutboxEvent>>,
    wait: Option<u64>,
}

/// List the workspace's events, newest first by default.
///
/// With `after`, only later events, in ascending order unless `order` says otherwise; with `wait`,
/// an empty page waits up to that many seconds (at most 25) for an event to arrive.
#[utoipa::path(
    get,
    path = "/events",
    tag = "Automation",
    operation_id = "events.list",
    params(
        ("limit" = Option<i64>, Query, description = "Events per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` or `asc`; `asc` by default with `after`, `desc` otherwise."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("type" = Option<EventType>, Query, description = "Only events of this type."),
        ("after" = Option<Id<OutboxEvent>>, Query, description = "Only events after this one."),
        ("wait" = Option<u64>, Query, description = "Seconds to wait for a first event when the page would be empty, 0 to 25."),
    ),
    responses(
        (status = 200, description = "A page of events.", body = Page<EventObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_events(
    principal: Principal,
    State(db): State<Database>,
    State(keys): State<Keys>,
    Query(mut list): Query<ListQuery>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Page<EventObject>>> {
    principal.require(Scope::AutomationRead)?;
    let wait = query.wait.unwrap_or(0);
    if wait > WAIT_MAX {
        return Err(Problem::invalid_field(
            "?wait",
            "range",
            "`wait` is between 0 and 25 seconds.",
        ));
    }
    if query.after.is_some() && list.order.is_none() {
        list.order = Some(Order::Asc);
    }
    let filters = EventFilters {
        kind: query.kind.map(|kind| kind.as_str().to_owned()),
        after: query.after.map(|after| after.uuid()),
    };
    let params =
        PageParams::from_query(&keys, principal.workspace, "events", "id", &filters, &list)?;
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        let mut tx = db.begin_in(principal.workspace).await?;
        let rows = outbox::list(
            &mut tx,
            principal.workspace,
            &filters,
            params.after_id(),
            params.ascending(),
            params.fetch(),
        )
        .await?;
        let total = match params.include_total {
            true => Some(outbox::count(&mut tx, principal.workspace, &filters, COUNT_CAP).await?),
            false => None,
        };
        tx.commit().await?;
        if rows.is_empty() && Instant::now() + WAIT_POLL <= deadline {
            tokio::time::sleep(WAIT_POLL).await;
            continue;
        }
        let page = Page::new(&keys, &params, rows, |event| {
            pagination::by_id(event.id.uuid())
        });
        return Ok(Json(match total {
            Some(total) => page.with_total(total),
            None => page,
        }));
    }
}

/// Retrieve an event.
#[utoipa::path(
    get,
    path = "/events/{id}",
    tag = "Automation",
    operation_id = "events.retrieve",
    params(("id" = Id<OutboxEvent>, Path, description = "The event id (`evt_…`).")),
    responses(
        (status = 200, description = "The event.", body = EventObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 404, description = "No such event in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_event(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<OutboxEvent>>,
) -> ApiResult<Json<EventObject>> {
    principal.require(Scope::AutomationRead)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let event = outbox::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("event"))?;
    tx.commit().await?;
    Ok(Json(event))
}

/// The body of `POST /events`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateEvent {
    /// The type of the synthetic event.
    #[serde(rename = "type")]
    #[garde(skip)]
    kind: EventType,
    /// Address the event to this endpoint only, whatever its subscriptions.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    webhook_endpoint_id: Option<Id<WebhookEndpoint>>,
}

/// Create a synthetic event with sample data of its type.
///
/// It is delivered like any other event, to every subscribed endpoint, or only to
/// `webhook_endpoint_id` when given.
#[utoipa::path(
    post,
    path = "/events",
    tag = "Automation",
    operation_id = "events.create",
    request_body(content = CreateEvent, example = json!({"type": "endpoint.test", "webhook_endpoint_id": "whe_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"})),
    responses(
        (status = 201, description = "The synthetic event; its deliveries appear within seconds.", body = EventObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such endpoint in this workspace."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_event(
    principal: Principal,
    State(db): State<Database>,
    Json(body): Json<CreateEvent>,
) -> ApiResult<(StatusCode, Json<EventObject>)> {
    principal.require(Scope::AutomationManage)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    if let Some(endpoint) = body.webhook_endpoint_id
        && endpoints::read(&mut tx, principal.workspace, endpoint)
            .await?
            .is_none()
    {
        return Err(Problem::not_found("webhook endpoint"));
    }
    let id = outbox::record_synthetic(
        &mut tx,
        principal.workspace,
        body.kind,
        body.webhook_endpoint_id,
    )
    .await?;
    let event = outbox::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("event"))?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(event)))
}

// ───────────────────────────── webhook endpoints ─────────────────────────────

/// The filters of `GET /webhook_endpoints`.
#[derive(Debug, Default, Deserialize, serde::Serialize)]
struct EndpointQuery {
    enabled: Option<bool>,
}

/// List the workspace's webhook endpoints, newest first by default.
#[utoipa::path(
    get,
    path = "/webhook_endpoints",
    tag = "Automation",
    operation_id = "webhook_endpoints.list",
    params(
        ("limit" = Option<i64>, Query, description = "Endpoints per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("enabled" = Option<bool>, Query, description = "Only enabled, or only disabled, endpoints."),
    ),
    responses(
        (status = 200, description = "A page of endpoints.", body = Page<EndpointObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_endpoints(
    principal: Principal,
    State(db): State<Database>,
    State(keys): State<Keys>,
    Query(list): Query<ListQuery>,
    Query(query): Query<EndpointQuery>,
) -> ApiResult<Json<Page<EndpointObject>>> {
    principal.require(Scope::AutomationRead)?;
    let params = PageParams::from_query(
        &keys,
        principal.workspace,
        "webhook_endpoints",
        "id",
        &query,
        &list,
    )?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let rows = endpoints::list(
        &mut tx,
        principal.workspace,
        query.enabled,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => {
            Some(endpoints::count(&mut tx, principal.workspace, query.enabled, COUNT_CAP).await?)
        }
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&keys, &params, rows, |endpoint| {
        pagination::by_id(endpoint.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// The body of `POST /webhook_endpoints`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateEndpoint {
    /// Resource filters; an empty object accepts every resource.
    #[garde(skip)]
    filters: Option<Filters>,
    /// Custom headers, replaced together, sealed at rest and never returned.
    #[garde(skip)]
    headers: Option<BTreeMap<String, String>>,
    /// Where events are POSTed: an `https` URL without credentials.
    #[garde(length(min = 1, max = 2048))]
    url: String,
    /// The event types the endpoint receives, 1 to 32.
    #[garde(length(min = 1, max = 32))]
    event_types: Vec<EventType>,
    /// Whether it receives events now (default true).
    #[garde(skip)]
    enabled: Option<bool>,
}

/// Create a webhook endpoint. Its signing secret (`whsec_…`) is in this response only.
#[utoipa::path(
    post,
    path = "/webhook_endpoints",
    tag = "Automation",
    operation_id = "webhook_endpoints.create",
    request_body(content = CreateEndpoint, example = json!({"url": "https://example.com/hooks/norbelys", "event_types": ["message.sent", "message.failed"]})),
    responses(
        (status = 201, description = "The endpoint, with its secret shown once.", body = EndpointObject,
         headers(("ETag" = String, description = "The endpoint's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_endpoint(
    principal: Principal,
    State(db): State<Database>,
    State(keys): State<Keys>,
    Json(body): Json<CreateEndpoint>,
) -> ApiResult<(StatusCode, Tagged<EndpointObject>)> {
    principal.require(Scope::AutomationManage)?;
    endpoints::check_url(&body.url)
        .map_err(|reason| Problem::invalid_field("/url", "format", reason))?;
    check_configuration(body.filters.as_ref())?;
    let headers = body
        .headers
        .as_ref()
        .map(policy::headers)
        .transpose()
        .map_err(|reason| Problem::invalid_field("/headers", "invalid", reason))?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let endpoint = endpoints::create(
        &mut tx,
        &keys,
        principal.workspace,
        &NewEndpoint {
            url: &body.url,
            event_types: &body.event_types,
            enabled: body.enabled.unwrap_or(true),
        },
    )
    .await
    .map_err(endpoint_problem)?;
    endpoints::configure(
        &mut tx,
        &keys,
        principal.workspace,
        endpoint.id,
        body.filters.as_ref(),
        headers.as_ref(),
    )
    .await
    .map_err(endpoint_problem)?;
    let mut configured = endpoints::read(&mut tx, principal.workspace, endpoint.id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook endpoint"))?;
    configured.secret = endpoint.secret;
    let endpoint = configured;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: endpoint.version,
            body: endpoint,
        },
    ))
}

/// Retrieve a webhook endpoint (without its secret).
#[utoipa::path(
    get,
    path = "/webhook_endpoints/{id}",
    tag = "Automation",
    operation_id = "webhook_endpoints.retrieve",
    params(("id" = Id<WebhookEndpoint>, Path, description = "The endpoint id (`whe_…`).")),
    responses(
        (status = 200, description = "The endpoint.", body = EndpointObject,
         headers(("ETag" = String, description = "The endpoint's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 404, description = "No such endpoint in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_endpoint(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<WebhookEndpoint>>,
) -> ApiResult<Tagged<EndpointObject>> {
    principal.require(Scope::AutomationRead)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let endpoint = endpoints::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook endpoint"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: endpoint.version,
        body: endpoint,
    })
}

/// The body of `PATCH /webhook_endpoints/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateEndpoint {
    /// Resource filters; an empty object accepts every resource.
    #[garde(skip)]
    filters: Option<Filters>,
    /// Custom headers, replaced together, sealed at rest and never returned.
    #[garde(skip)]
    headers: Option<BTreeMap<String, String>>,
    /// A new URL.
    #[garde(length(min = 1, max = 2048))]
    url: Option<String>,
    /// The new list of event types, 1 to 32, replacing the old one.
    #[garde(length(min = 1, max = 32))]
    event_types: Option<Vec<EventType>>,
    /// `false` disables the endpoint (its pending deliveries stop); `true` enables it again.
    #[garde(skip)]
    enabled: Option<bool>,
}

/// Update a webhook endpoint: its URL, its event types, or whether it is enabled.
#[utoipa::path(
    patch,
    path = "/webhook_endpoints/{id}",
    tag = "Automation",
    operation_id = "webhook_endpoints.update",
    params(("id" = Id<WebhookEndpoint>, Path, description = "The endpoint id (`whe_…`)."), IfMatch),
    request_body(content = UpdateEndpoint, example = json!({"enabled": false})),
    responses(
        (status = 200, description = "The endpoint.", body = EndpointObject,
         headers(("ETag" = String, description = "The endpoint's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such endpoint in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_endpoint(
    principal: Principal,
    State(db): State<Database>,
    State(keys): State<Keys>,
    Path(id): Path<Id<WebhookEndpoint>>,
    if_match: IfMatch,
    Json(body): Json<UpdateEndpoint>,
) -> ApiResult<Tagged<EndpointObject>> {
    principal.require(Scope::AutomationManage)?;
    if let Some(url) = &body.url {
        endpoints::check_url(url)
            .map_err(|reason| Problem::invalid_field("/url", "format", reason))?;
    }
    let mut tx = db.begin_in(principal.workspace).await?;
    check_configuration(body.filters.as_ref())?;
    let headers = body
        .headers
        .as_ref()
        .map(policy::headers)
        .transpose()
        .map_err(|reason| Problem::invalid_field("/headers", "invalid", reason))?;
    let current = endpoints::lock_version(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook endpoint"))?;
    if_match.check(current)?;
    endpoints::configure(
        &mut tx,
        &keys,
        principal.workspace,
        id,
        body.filters.as_ref(),
        headers.as_ref(),
    )
    .await
    .map_err(endpoint_problem)?;
    let endpoint = endpoints::update(
        &mut tx,
        principal.workspace,
        id,
        &EndpointChanges {
            url: body.url.as_deref(),
            event_types: body.event_types.as_deref(),
            enabled: body.enabled,
        },
    )
    .await?
    .ok_or_else(|| Problem::not_found("webhook endpoint"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: endpoint.version,
        body: endpoint,
    })
}

/// Delete a webhook endpoint and its deliveries.
#[utoipa::path(
    delete,
    path = "/webhook_endpoints/{id}",
    tag = "Automation",
    operation_id = "webhook_endpoints.delete",
    params(("id" = Id<WebhookEndpoint>, Path, description = "The endpoint id (`whe_…`).")),
    responses(
        (status = 204, description = "The endpoint is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such endpoint in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_endpoint(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<WebhookEndpoint>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::AutomationManage)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let deleted = endpoints::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("webhook endpoint"))
    }
}

/// The body of `POST /webhook_endpoints/{id}/replay`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ReplayEndpoint {
    /// Reopen the deliveries of events created at or after this instant. It must be within the
    /// replay window (about six days back with daily partitions and a week online).
    #[garde(skip)]
    since: Timestamp,
}

/// Replay an endpoint's deliveries.
///
/// Every delivery of its events created at or after `since` becomes pending with its next attempt
/// now. A disabled endpoint must be enabled first.
#[utoipa::path(
    post,
    path = "/webhook_endpoints/{id}/replay",
    tag = "Automation",
    operation_id = "webhook_endpoints.replay",
    params(("id" = Id<WebhookEndpoint>, Path, description = "The endpoint id (`whe_…`).")),
    request_body(content = ReplayEndpoint, example = json!({"since": "2026-10-01T00:00:00Z"})),
    responses(
        (status = 200, description = "The endpoint; its deliveries are reopened.", body = EndpointObject,
         headers(("ETag" = String, description = "The endpoint's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such endpoint in this workspace."),
        (status = 409, description = "The endpoint is disabled (`invalid_state`); enable it first."),
        (status = 422, description = "`since` is older than the replay window, or the body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn replay_endpoint(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<WebhookEndpoint>>,
    Json(body): Json<ReplayEndpoint>,
) -> ApiResult<Tagged<EndpointObject>> {
    principal.require(Scope::AutomationManage)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let endpoint = endpoints::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook endpoint"))?;
    if !endpoint.enabled {
        return Err(Problem::invalid_state(
            "The endpoint is disabled; enable it before replaying.",
        ));
    }
    deliver::reopen(
        &mut tx,
        principal.workspace,
        Reopen::Endpoint {
            endpoint: id,
            since: body.since,
        },
    )
    .await
    .map_err(|error| reopen_problem(error, "/since"))?;
    tx.commit().await?;
    jobs::wake(&db, Queue::Webhooks).await;
    Ok(Tagged {
        version: endpoint.version,
        body: endpoint,
    })
}

/// Rotate an endpoint's signing secret.
///
/// The new secret is in this response only; the old one keeps signing for 24 hours, so every
/// attempt carries both signatures meanwhile.
#[utoipa::path(
    post,
    path = "/webhook_endpoints/{id}/rotate_secret",
    tag = "Automation",
    operation_id = "webhook_endpoints.rotate_secret",
    params(("id" = Id<WebhookEndpoint>, Path, description = "The endpoint id (`whe_…`).")),
    responses(
        (status = 200, description = "The endpoint, with its new secret shown once.", body = EndpointObject,
         headers(("ETag" = String, description = "The endpoint's new `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such endpoint in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn rotate_endpoint_secret(
    principal: Principal,
    State(db): State<Database>,
    State(keys): State<Keys>,
    Path(id): Path<Id<WebhookEndpoint>>,
) -> ApiResult<Tagged<EndpointObject>> {
    principal.require(Scope::AutomationManage)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let endpoint = endpoints::rotate_secret(&mut tx, &keys, principal.workspace, id)
        .await
        .map_err(endpoint_problem)?
        .ok_or_else(|| Problem::not_found("webhook endpoint"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: endpoint.version,
        body: endpoint,
    })
}

// ───────────────────────────── webhook deliveries ─────────────────────────────

/// The filters of `GET /webhook_deliveries`.
#[derive(Debug, Deserialize)]
struct DeliveryQuery {
    webhook_endpoint_id: Option<Id<WebhookEndpoint>>,
    event_id: Option<Id<OutboxEvent>>,
    state: Option<DeliveryState>,
}

/// List the workspace's webhook deliveries, newest events first by default.
///
/// The order is by event, then by delivery.
#[utoipa::path(
    get,
    path = "/webhook_deliveries",
    tag = "Automation",
    operation_id = "webhook_deliveries.list",
    params(
        ("limit" = Option<i64>, Query, description = "Deliveries per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("webhook_endpoint_id" = Option<Id<WebhookEndpoint>>, Query, description = "Only deliveries to this endpoint."),
        ("event_id" = Option<Id<OutboxEvent>>, Query, description = "Only deliveries of this event."),
        ("state" = Option<DeliveryState>, Query, description = "Only deliveries in this state."),
    ),
    responses(
        (status = 200, description = "A page of deliveries.", body = Page<DeliveryObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_deliveries(
    principal: Principal,
    State(db): State<Database>,
    State(keys): State<Keys>,
    Query(list): Query<ListQuery>,
    Query(query): Query<DeliveryQuery>,
) -> ApiResult<Json<Page<DeliveryObject>>> {
    principal.require(Scope::AutomationRead)?;
    let filters = DeliveryFilters {
        endpoint: query.webhook_endpoint_id.map(|id| id.uuid()),
        event: query.event_id.map(|id| id.uuid()),
        state: query.state.map(|state| state.as_str().to_owned()),
    };
    let params = PageParams::from_query(
        &keys,
        principal.workspace,
        "webhook_deliveries",
        "event_id",
        &filters,
        &list,
    )?;
    let after = match &params.after {
        None => None,
        Some(after) => {
            let event = match after.key {
                Some(Key::Row(event)) => Some(event),
                Some(Key::At(_)) | None => None,
            };
            Some((
                event.ok_or_else(|| {
                    Problem::bad_request("The cursor is not valid for this request.")
                })?,
                after.id,
            ))
        }
    };
    let mut tx = db.begin_in(principal.workspace).await?;
    let rows = deliver::list(
        &mut tx,
        principal.workspace,
        &filters,
        after,
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(deliver::count(&mut tx, principal.workspace, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&keys, &params, rows, |delivery| After {
        key: Some(Key::Row(delivery.event_id.uuid())),
        id: delivery.id.uuid(),
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// Retrieve a webhook delivery with its latest attempt.
#[utoipa::path(
    get,
    path = "/webhook_deliveries/{id}",
    tag = "Automation",
    operation_id = "webhook_deliveries.retrieve",
    params(("id" = Id<WebhookDelivery>, Path, description = "The delivery id (`whd_…`), also the `webhook-id` header.")),
    responses(
        (status = 200, description = "The delivery.", body = DeliveryObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 404, description = "No such delivery in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_delivery(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<WebhookDelivery>>,
) -> ApiResult<Json<DeliveryObject>> {
    principal.require(Scope::AutomationRead)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let delivery = deliver::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook delivery"))?;
    tx.commit().await?;
    Ok(Json(delivery))
}

/// Retry a webhook delivery now.
///
/// A pending delivery's next attempt is brought forward; a finished one becomes pending again.
/// There is never a second run beside the automatic one.
#[utoipa::path(
    post,
    path = "/webhook_deliveries/{id}/retry",
    tag = "Automation",
    operation_id = "webhook_deliveries.retry",
    params(("id" = Id<WebhookDelivery>, Path, description = "The delivery id (`whd_…`).")),
    responses(
        (status = 200, description = "The delivery, pending with its next attempt now.", body = DeliveryObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such delivery in this workspace."),
        (status = 409, description = "Its endpoint is disabled (`invalid_state`); enable it first."),
        (status = 422, description = "Its event is older than the replay window."),
    ),
    security(("bearer" = []))
)]
async fn retry_delivery(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<WebhookDelivery>>,
) -> ApiResult<Json<DeliveryObject>> {
    principal.require(Scope::AutomationManage)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let delivery = deliver::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook delivery"))?;
    let endpoint = endpoints::read(&mut tx, principal.workspace, delivery.webhook_endpoint_id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook delivery"))?;
    if !endpoint.enabled {
        return Err(Problem::invalid_state(
            "The delivery's endpoint is disabled; enable it before retrying.",
        ));
    }
    deliver::reopen(&mut tx, principal.workspace, Reopen::Delivery(id))
        .await
        .map_err(|error| reopen_problem(error, ""))?;
    let delivery = deliver::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("webhook delivery"))?;
    tx.commit().await?;
    jobs::wake(&db, Queue::Webhooks).await;
    Ok(Json(delivery))
}

fn reopen_problem(error: ReopenError, pointer: &str) -> Problem {
    match error {
        ReopenError::TooOld => Problem::invalid_field(
            pointer,
            "range",
            "The events are older than the replay window; read them with `GET /events` instead.",
        ),
        ReopenError::Db(error) => Problem::from(error),
    }
}

fn endpoint_problem(error: EndpointError) -> Problem {
    match error {
        EndpointError::Db(error) => Problem::from(error),
        EndpointError::Secret(error) => Problem::internal(&error),
    }
}

/// Checks bounded filters before taking the endpoint lock.
fn check_configuration(filters: Option<&Filters>) -> ApiResult<()> {
    if let Some(filters) = filters {
        filters
            .check()
            .map_err(|reason| Problem::invalid_field("/filters", "invalid", reason))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use crate::domain::scope::{Scope, ScopeSet};
    use crate::testing::{Reply, TestApp, TestDb};
    use crate::webhooks::tests::{harness, relay};

    /// Creates an endpoint through the API with `idempotency_key`.
    async fn create(app: &TestApp, key: &str, idempotency_key: &str, body: Value) -> Reply {
        app.post("/v1/webhook_endpoints")
            .bearer(key)
            .idempotency(idempotency_key)
            .json(body)
            .send()
            .await
    }

    /// The first field error's pointer of a `validation_failed` problem.
    fn pointer(reply: &Reply) -> &str {
        reply.json["errors"][0]["pointer"]
            .as_str()
            .unwrap_or_default()
    }

    /// An RFC 3339 instant `seconds` before now.
    fn ago(seconds: u64) -> String {
        crate::process::now()
            .minus(std::time::Duration::from_secs(seconds))
            .to_string()
    }

    /// Creating an endpoint answers `201` with the endpoint and its `whsec_` secret, its event
    /// types deduplicated and sorted; the secret is never shown again, not even to its workspace.
    #[tokio::test]
    async fn creating_an_endpoint_shows_its_secret_once() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        let body = json!({ "url": "https://hooks.example.com/n", "event_types": ["message.sent", "endpoint.test", "message.sent"] });
        let created = create(&app, &acme.key, "create-1", body).await;
        assert_eq!(created.status, StatusCode::CREATED);
        let id = created.json["id"].as_str().unwrap();
        assert!(id.starts_with("whe_"));
        assert!(
            created.json["secret"]
                .as_str()
                .unwrap()
                .starts_with("whsec_")
        );
        assert_eq!(
            created.json["event_types"],
            json!(["endpoint.test", "message.sent"])
        );
        assert_eq!(
            (
                created.json["enabled"].clone(),
                created.json["disabled_reason"].clone()
            ),
            (json!(true), Value::Null)
        );
        let read = app
            .get(&format!("/v1/webhook_endpoints/{id}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(read.status, StatusCode::OK);
        assert!(read.json.get("secret").is_none());
    }

    /// The same `Idempotency-Key` with the same request replays the first answer (marked
    /// `Idempotent-Replayed: true`) without creating a second endpoint; with another body it is
    /// `422 idempotency_mismatch`; an effectful `POST` without a key is `400 invalid_request`.
    #[tokio::test]
    async fn an_idempotent_create_replays_and_refuses_another_body() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        let body = json!({ "url": "https://hooks.example.com/n", "event_types": ["message.sent"] });
        let first = create(&app, &acme.key, "same-key", body.clone()).await;
        let again = create(&app, &acme.key, "same-key", body).await;
        assert_eq!(
            (again.status, again.header("idempotent-replayed")),
            (StatusCode::CREATED, Some("true"))
        );
        assert_eq!(again.json, first.json);
        let listed = app
            .get("/v1/webhook_endpoints")
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(listed.json["data"].as_array().unwrap().len(), 1);
        let other =
            json!({ "url": "https://hooks.example.com/other", "event_types": ["message.sent"] });
        let mismatch = create(&app, &acme.key, "same-key", other.clone()).await;
        assert_eq!(
            (mismatch.status, mismatch.json["code"].as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("idempotency_mismatch")
            )
        );
        let keyless = app
            .post("/v1/webhook_endpoints")
            .bearer(&acme.key)
            .json(other)
            .send()
            .await;
        assert_eq!(
            (keyless.status, keyless.json["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_request"))
        );
    }

    /// Invalid bodies are `422 validation_failed` with an RFC 6901 pointer at the field: a URL
    /// that is not an absolute http(s) URL, an unknown event type, an empty list of types.
    #[tokio::test]
    async fn endpoint_bodies_are_validated_with_pointers() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        for (index, (body, expected)) in [
            (
                json!({ "url": "hooks.example.com", "event_types": ["message.sent"] }),
                "/url",
            ),
            (
                json!({ "url": "https://hooks.example.com", "event_types": ["message.exploded"] }),
                "/event_types/0",
            ),
            (
                json!({ "url": "https://hooks.example.com", "event_types": [] }),
                "/event_types",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let reply = create(&app, &acme.key, &format!("invalid-{index}"), body).await;
            assert_eq!(
                (reply.status, pointer(&reply)),
                (StatusCode::UNPROCESSABLE_ENTITY, expected),
                "{:?}",
                reply.json
            );
        }
    }

    /// Another workspace's endpoint does not exist for a credential: reading, changing,
    /// deleting, replaying or rotating it is `404`, exactly as for an id that exists nowhere.
    #[tokio::test]
    async fn another_workspaces_endpoint_is_not_found() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let globex = test.workspace("globex").await;
        let app = test.app();
        let (_, path) = sample_endpoint(&app, &acme.key).await;
        let replay = json!({ "since": ago(60) });
        for reply in [
            app.get(&path).bearer(&globex.key).send().await,
            app.patch(&path)
                .bearer(&globex.key)
                .json(json!({ "enabled": false }))
                .send()
                .await,
            app.delete(&path).bearer(&globex.key).send().await,
            app.post(&format!("{path}/replay"))
                .bearer(&globex.key)
                .idempotency("replay")
                .json(replay)
                .send()
                .await,
            app.post(&format!("{path}/rotate_secret"))
                .bearer(&globex.key)
                .idempotency("rotate")
                .send()
                .await,
        ] {
            assert_eq!(
                (reply.status, reply.json["code"].as_str()),
                (StatusCode::NOT_FOUND, Some("not_found"))
            );
        }
        assert_eq!(
            app.get(&path).bearer(&acme.key).send().await.status,
            StatusCode::OK
        );
    }

    /// Lists page with signed cursors: a page of `limit` with `next_cursor`, the next page from
    /// it, `total_count` on request; a cursor reused with other filters is refused
    /// (`400 invalid_cursor`) rather than read as another query's position.
    #[tokio::test]
    async fn lists_page_with_cursors_bound_to_their_filters() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        for index in 0..3 {
            let url = format!("https://hooks.example.com/{index}");
            create(
                &app,
                &acme.key,
                &format!("create-{index}"),
                json!({ "url": url, "event_types": ["message.sent"] }),
            )
            .await;
        }
        let first = app
            .get("/v1/webhook_endpoints?limit=2&include=total_count")
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(first.json["data"].as_array().unwrap().len(), 2);
        assert_eq!(
            (
                first.json["meta"]["has_more"].clone(),
                first.json["meta"]["total_count"].clone()
            ),
            (json!(true), json!(3))
        );
        let cursor = first.json["meta"]["next_cursor"].as_str().unwrap();
        let second = app
            .get(&format!("/v1/webhook_endpoints?limit=2&cursor={cursor}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(second.json["data"].as_array().unwrap().len(), 1);
        assert_eq!(second.json["meta"]["has_more"], json!(false));
        let foreign = app
            .get(&format!(
                "/v1/webhook_endpoints?limit=2&enabled=true&cursor={cursor}"
            ))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(
            (foreign.status, pointer(&foreign)),
            (StatusCode::BAD_REQUEST, "?cursor")
        );
    }

    /// An endpoint subscribed to message delivery, plus its resource path.
    async fn sample_endpoint(app: &TestApp, key: &str) -> (Reply, String) {
        let created = create(
            app,
            key,
            "create-1",
            json!({ "url": "https://a.example/n", "event_types": ["message.sent"] }),
        )
        .await;
        let path = format!(
            "/v1/webhook_endpoints/{}",
            created.json["id"].as_str().unwrap()
        );
        (created, path)
    }

    /// Disabling an endpoint records why (`manual`) and a `webhook_endpoint.disabled` event, and
    /// blocks replays (`409 invalid_state`) until it is enabled again; a replay must also stay
    /// within the replay window (`422` at `/since`).
    #[tokio::test]
    async fn disabling_an_endpoint_records_an_event_and_gates_replays() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        let (_, path) = sample_endpoint(&app, &acme.key).await;
        let disabled = app
            .patch(&path)
            .bearer(&acme.key)
            .json(json!({ "enabled": false }))
            .send()
            .await;
        assert_eq!(
            (
                disabled.json["enabled"].clone(),
                disabled.json["disabled_reason"].clone()
            ),
            (json!(false), json!("manual"))
        );
        let events = app
            .get("/v1/events?type=webhook_endpoint.disabled")
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(
            events.json["data"][0]["data"]["disabled_reason"],
            json!("manual")
        );
        let replay = |key: &'static str, since: String| {
            app.post(&format!("{path}/replay"))
                .bearer(&acme.key)
                .idempotency(key)
                .json(json!({ "since": since }))
                .send()
        };
        assert_eq!(
            replay("replay-1", ago(3_600)).await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.patch(&path)
                .bearer(&acme.key)
                .json(json!({ "enabled": true }))
                .send()
                .await
                .json["enabled"],
            json!(true)
        );
        assert_eq!(replay("replay-2", ago(3_600)).await.status, StatusCode::OK);
        let too_old = replay("replay-3", ago(30 * 86_400)).await;
        assert_eq!(
            (too_old.status, pointer(&too_old)),
            (StatusCode::UNPROCESSABLE_ENTITY, "/since")
        );
    }

    /// Rotating a secret answers the endpoint with a new secret, shown in that answer only.
    #[tokio::test]
    async fn rotating_a_secret_shows_the_new_one_once() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        let (created, path) = sample_endpoint(&app, &acme.key).await;
        let rotated = app
            .post(&format!("{path}/rotate_secret"))
            .bearer(&acme.key)
            .idempotency("rotate")
            .send()
            .await;
        assert_eq!(rotated.status, StatusCode::OK);
        let secret = rotated.json["secret"].as_str().unwrap();
        assert!(secret.starts_with("whsec_") && secret != created.json["secret"].as_str().unwrap());
        assert!(
            app.get(&path)
                .bearer(&acme.key)
                .send()
                .await
                .json
                .get("secret")
                .is_none()
        );
    }

    /// A synthetic event is created (`201`, `synthetic: true`), addressed to an endpoint of the
    /// workspace only (another workspace's endpoint is `404`), and listed: after a position in
    /// ascending order, by type, and by id; a wait beyond 25 seconds is refused.
    #[tokio::test]
    async fn synthetic_events_are_created_and_listed() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let globex = test.workspace("globex").await;
        let app = test.app();
        let own = create(
            &app,
            &acme.key,
            "own",
            json!({ "url": "https://a.example/n", "event_types": ["message.sent"] }),
        )
        .await;
        let foreign = create(
            &app,
            &globex.key,
            "foreign",
            json!({ "url": "https://b.example/n", "event_types": ["message.sent"] }),
        )
        .await;
        let own_id = own.json["id"].as_str().unwrap();
        let test_event = app
            .post("/v1/events")
            .bearer(&acme.key)
            .idempotency("event-1")
            .json(json!({ "type": "endpoint.test", "webhook_endpoint_id": own_id }))
            .send()
            .await;
        assert_eq!(test_event.status, StatusCode::CREATED);
        assert_eq!(
            (
                test_event.json["synthetic"].clone(),
                test_event.json["webhook_endpoint_id"].clone()
            ),
            (json!(true), json!(own_id))
        );
        assert_eq!(
            test_event.json["data"]["webhook_endpoint_id"],
            json!(own_id)
        );
        let elsewhere = app
            .post("/v1/events")
            .bearer(&acme.key)
            .idempotency("event-2")
            .json(json!({ "type": "endpoint.test", "webhook_endpoint_id": foreign.json["id"] }))
            .send()
            .await;
        assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
        let sent = app
            .post("/v1/events")
            .bearer(&acme.key)
            .idempotency("event-3")
            .json(json!({ "type": "message.sent" }))
            .send()
            .await;
        let first_id = test_event.json["id"].as_str().unwrap();
        let after = app
            .get(&format!("/v1/events?after={first_id}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(
            after.json["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| event["id"].clone())
                .collect::<Vec<_>>(),
            [sent.json["id"].clone()]
        );
        let by_type = app
            .get("/v1/events?type=endpoint.test")
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(by_type.json["data"].as_array().unwrap().len(), 1);
        assert_eq!(
            app.get(&format!("/v1/events/{first_id}"))
                .bearer(&acme.key)
                .send()
                .await
                .json["type"],
            json!("endpoint.test")
        );
        assert_eq!(
            app.get("/v1/events?wait=26")
                .bearer(&acme.key)
                .send()
                .await
                .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// A workspace's deliveries are listed by endpoint and by state, read by id, and retried
    /// (pending again now); a retry to a disabled endpoint is `409`, and another workspace
    /// cannot read them.
    #[tokio::test]
    async fn deliveries_are_listed_read_and_retried() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let globex = test.workspace("globex").await;
        let app = test.app();
        let created = create(
            &app,
            &acme.key,
            "create-1",
            json!({ "url": "https://a.example/n", "event_types": ["message.sent"] }),
        )
        .await;
        let endpoint = created.json["id"].as_str().unwrap();
        app.post("/v1/events")
            .bearer(&acme.key)
            .idempotency("event-1")
            .json(json!({ "type": "message.sent" }))
            .send()
            .await;
        relay(&test, &harness(&test, true)).await;

        let listed = app
            .get(&format!(
                "/v1/webhook_deliveries?webhook_endpoint_id={endpoint}"
            ))
            .bearer(&acme.key)
            .send()
            .await;
        let delivery = listed.json["data"][0].clone();
        assert_eq!(
            (
                delivery["state"].clone(),
                delivery["attempts"].clone(),
                delivery["event_type"].clone()
            ),
            (json!("pending"), json!(0), json!("message.sent"))
        );
        let none = app
            .get("/v1/webhook_deliveries?state=delivered")
            .bearer(&acme.key)
            .send()
            .await;
        assert!(none.json["data"].as_array().unwrap().is_empty());
        let path = format!(
            "/v1/webhook_deliveries/{}",
            delivery["id"].as_str().unwrap()
        );
        assert_eq!(
            app.get(&path).bearer(&acme.key).send().await.json["id"],
            delivery["id"]
        );
        assert_eq!(
            app.get(&path).bearer(&globex.key).send().await.status,
            StatusCode::NOT_FOUND
        );
        let retried = app
            .post(&format!("{path}/retry"))
            .bearer(&acme.key)
            .idempotency("retry-1")
            .send()
            .await;
        assert_eq!(
            (retried.status, retried.json["state"].clone()),
            (StatusCode::OK, json!("pending"))
        );
        app.patch(&format!("/v1/webhook_endpoints/{endpoint}"))
            .bearer(&acme.key)
            .json(json!({ "enabled": false }))
            .send()
            .await;
        let refused = app
            .post(&format!("{path}/retry"))
            .bearer(&acme.key)
            .idempotency("retry-2")
            .send()
            .await;
        assert_eq!(
            (refused.status, refused.json["code"].as_str()),
            (StatusCode::CONFLICT, Some("invalid_state"))
        );
    }

    /// The automation resources need a credential (`401` without one) and its scopes: a key
    /// without `automation:read` cannot list, one without `automation:manage` cannot create
    /// (`403 forbidden`).
    #[tokio::test]
    async fn credentials_and_scopes_are_required() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let app = test.app();
        let anonymous = app.get("/v1/webhook_endpoints").send().await;
        assert_eq!(
            (anonymous.status, anonymous.json["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("unauthorized"))
        );
        let people_only = test
            .api_key(&acme, [Scope::PeopleRead].into_iter().collect::<ScopeSet>())
            .await;
        assert_eq!(
            app.get("/v1/webhook_endpoints")
                .bearer(&people_only)
                .send()
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        let reader = test
            .api_key(
                &acme,
                [Scope::AutomationRead].into_iter().collect::<ScopeSet>(),
            )
            .await;
        assert_eq!(
            app.get("/v1/webhook_endpoints")
                .bearer(&reader)
                .send()
                .await
                .status,
            StatusCode::OK
        );
        let denied = create(
            &app,
            &reader,
            "create-1",
            json!({ "url": "https://a.example/n", "event_types": ["message.sent"] }),
        )
        .await;
        assert_eq!(
            (denied.status, denied.json["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("forbidden"))
        );
    }

    /// Every automation operation is in the OpenAPI document under its operation id, so the
    /// SDK, the CLI and the reference cannot drift from the handlers.
    #[test]
    fn the_openapi_document_lists_the_automation_operations() {
        let document = serde_json::to_value(crate::http::router::openapi()).unwrap();
        let operations: Vec<&str> = document["paths"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|path| path.as_object().unwrap().values())
            .filter_map(|operation| operation["operationId"].as_str())
            .collect();
        for expected in [
            "jobs.retrieve",
            "jobs.cancel",
            "events.list",
            "events.create",
            "events.retrieve",
            "webhook_endpoints.list",
            "webhook_endpoints.create",
            "webhook_endpoints.retrieve",
            "webhook_endpoints.update",
            "webhook_endpoints.delete",
            "webhook_endpoints.replay",
            "webhook_endpoints.rotate_secret",
            "webhook_deliveries.list",
            "webhook_deliveries.retrieve",
            "webhook_deliveries.retry",
        ] {
            assert!(operations.contains(&expected), "{expected} is missing");
        }
    }
}
