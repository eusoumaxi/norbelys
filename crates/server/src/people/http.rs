//! The audience resources under `/v1`: `people`, `fields`, `groups`, `segments`,
//! `suppressions`, `imports`, `exports` and `preflight`.
//!
//! Reads need `people:read`; writes need `people:write`. Every effectful `POST` takes an
//! `Idempotency-Key` (the idempotency middleware enforces it before these handlers run);
//! `POST /preflight` checks and stores nothing, so it takes none. Creates answer `201` with the
//! resource, except imports and exports, whose work is a job: they answer `202` with the
//! resource in its first status and its `Location`; and a list of addresses suppressed in one
//! request, which answers `200` with what it did rather than up to 1,000 suppressions. Deletes
//! answer `204`.
//!
//! People, fields, groups and segments can be updated, so each carries `version` and answers it
//! as `ETag` wherever one of them is returned alone. Their updates take an optional `If-Match`,
//! checked under the row's lock before anything is written (`http::versioning`); the dashboard
//! always sends it with a person's `group_ids`, which replace the memberships whole.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use strum::IntoEnumIterator as _;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::exports::{self, ExportObject};
use super::fields::{self, FieldChanges, FieldObject, NewField};
use super::groups::{self, GroupObject};
use super::imports::{self, ImportObject, ImportStatus, Input};
use super::segments::{self, SegmentObject};
use super::suppressions::{self, NewSuppression, SuppressionFilters, SuppressionObject};
use super::{Error, GROUPS_MAX, NewPerson, PeopleFilters, PersonChanges, PersonObject, Selection};
use crate::db::Tx;
use crate::delivery::preflight::{self, Finding};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{
    Export, Field, Group, Id, Import, Person, Segment, Suppression, WorkspaceId,
};
use crate::domain::people::{
    self as attributes, Definition, FieldType, LABEL_MAX, NAME_MAX, OPTION_MAX, OPTIONS_MAX,
};
use crate::domain::scope::Scope;
use crate::domain::segments::Filter;
use crate::domain::suppressions::{LIST_MAX, Reason, Source, read_list};
use crate::http::AppState;
use crate::http::extract::{self, Json, Path, Query};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::authority::Principal;
use crate::pagination::{COUNT_CAP, Include, ListQuery, Order, Page, PageParams, Sort};
use crate::problem::{ApiResult, Code, FieldError, Problem};
use crate::storage::Links;

/// The longest group or segment name, and the longest group description, in characters.
const NAME_LONG: usize = 200;
const DESCRIPTION_MAX: usize = 1_000;

/// The routes of the audience.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_people, create_person))
        .routes(routes!(retrieve_person, update_person, delete_person))
        .routes(routes!(list_fields, create_field))
        .routes(routes!(update_field, delete_field))
        .routes(routes!(list_groups, create_group))
        .routes(routes!(retrieve_group, update_group, delete_group))
        .routes(routes!(list_segments, create_segment))
        .routes(routes!(retrieve_segment, update_segment, delete_segment))
        .routes(routes!(list_suppressions, create_suppression))
        .routes(routes!(retrieve_suppression, delete_suppression))
        .merge(
            // An import's CSV file is the request body, bounded at 16 MiB rather than the
            // default 1 MiB.
            OpenApiRouter::new()
                .routes(routes!(list_imports, create_import))
                .layer(DefaultBodyLimit::max(imports::UPLOAD_MAX)),
        )
        .routes(routes!(retrieve_import))
        .routes(routes!(list_exports, create_export))
        .routes(routes!(retrieve_export))
        .routes(routes!(create_preflight))
}

impl From<Error> for Problem {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(what) => Problem::not_found(what),
            Error::Conflict(detail) => Problem::conflict(detail),
            Error::InvalidState(detail) => Problem::invalid_state(detail),
            Error::Invalid(pointer, detail) => Problem::invalid_field(&pointer, "invalid", detail),
            Error::Db(error) => Problem::from(error),
            Error::Storage(error) => Problem::from(error),
        }
    }
}

/// The links of the api's read paths.
fn links(state: &AppState) -> Links<'_> {
    Links {
        storage: &state.storage,
        keys: &state.keys,
        public_api_url: &state.settings.public_api_url,
    }
}

/// Reads a member that may be absent (`None`), `null` (`Some(None)`) or a value.
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// An RFC 6901 pointer segment.
fn segment(text: &str) -> String {
    text.replace('~', "~0").replace('/', "~1")
}

/// Trims a name or company; an empty one is absent.
fn name(value: Option<&str>, pointer: &str) -> Result<Option<String>, Problem> {
    value
        .map(|value| {
            attributes::read_name(value)
                .map_err(|detail| Problem::invalid_field(pointer, "length", detail))
        })
        .transpose()
        .map(Option::flatten)
}

/// The workspace's definitions under the shared field lock, for a write of custom values.
async fn locked_definitions(
    tx: &mut Tx,
    workspace: WorkspaceId,
) -> Result<Vec<Definition>, Problem> {
    fields::lock_shared(tx, workspace).await?;
    Ok(fields::definitions(tx, workspace).await?)
}

/// Refuses custom values that do not fit the definitions, each at its pointer.
fn check_fields(definitions: &[Definition], values: &Map<String, Value>) -> Result<(), Problem> {
    let refused = attributes::check_fields(definitions, values);
    if refused.is_empty() {
        return Ok(());
    }
    Err(Problem::validation(
        refused
            .into_iter()
            .map(|(key, error)| FieldError {
                pointer: format!("/fields/{}", segment(&key)),
                code: "invalid".to_owned(),
                detail: error.to_string(),
            })
            .collect(),
    ))
}

// ───────────────────────────── people ─────────────────────────────

/// List the workspace's people, newest first by default.
///
/// With `segment_id`, the people the segment's filter matches now.
#[utoipa::path(
    get,
    path = "/people",
    tag = "Audience",
    operation_id = "people.list",
    params(
        ("limit" = Option<i64>, Query, description = "People per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("sort" = Option<String>, Query, description = "`id` (default, the creation order) or `updated_at` (when each person last changed); ties are ordered by id."),
        ("email" = Option<String>, Query, description = "The person with this address (ignoring ASCII case)."),
        ("group_id" = Option<Id<Group>>, Query, description = "Members of this group."),
        ("segment_id" = Option<Id<Segment>>, Query, description = "People this segment matches."),
        ("q" = Option<String>, Query, description = "A prefix of the address, a name or the company, ignoring case."),
        ("created_at[gte]" = Option<String>, Query, description = "Created at or after this instant (RFC 3339)."),
        ("created_at[gt]" = Option<String>, Query, description = "Created after this instant."),
        ("created_at[lte]" = Option<String>, Query, description = "Created at or before this instant."),
        ("created_at[lt]" = Option<String>, Query, description = "Created before this instant."),
    ),
    responses(
        (status = 200, description = "A page of people.", body = Page<PersonObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 404, description = "No such group or segment in this workspace."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_people(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(filters): Query<PeopleFilters>,
) -> ApiResult<Json<Page<PersonObject>>> {
    principal.require(Scope::PeopleRead)?;
    let ws = principal.workspace;
    let params = PageParams::sorted(
        &state.keys,
        ws,
        "people",
        &[Sort::UpdatedAt],
        &filters,
        &list,
    )?;
    let mut tx = state.db.begin_in(ws).await?;
    let selection = selection(&mut tx, ws, &filters).await?;
    let rows = match params.sort() {
        Sort::UpdatedAt => {
            super::list_by_update(
                &mut tx,
                ws,
                &selection,
                params.after_at().zip(params.after_id()),
                params.ascending(),
                params.fetch(),
            )
            .await?
        }
        Sort::Id | Sort::LastActivityAt => {
            super::list(
                &mut tx,
                ws,
                &selection,
                params.after_id(),
                params.ascending(),
                params.fetch(),
            )
            .await?
        }
    };
    let total = match params.include_total {
        true => Some(super::count(&mut tx, ws, &selection, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&state.keys, &params, rows, |person| {
        params.position(person.updated_at, person.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// The selection of `filters`: the named group and segment must exist.
async fn selection(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &PeopleFilters,
) -> Result<Selection, Problem> {
    if let Some(group) = filters.group_id
        && groups::read(tx, workspace, group).await?.is_none()
    {
        return Err(Problem::not_found("group"));
    }
    let segment = match filters.segment_id {
        Some(segment) => Some(segments::compiled(tx, workspace, segment).await?),
        None => None,
    };
    Ok(Selection::new(filters, segment))
}

/// The body of `POST /people`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreatePerson {
    /// The address; unique in the workspace ignoring ASCII case.
    #[garde(skip)]
    #[schema(value_type = String, format = Email)]
    email: EmailAddress,
    /// At most 200 characters; blank is absent.
    #[garde(length(chars, max = NAME_MAX))]
    given_name: Option<String>,
    #[garde(length(chars, max = NAME_MAX))]
    family_name: Option<String>,
    #[garde(length(chars, max = NAME_MAX))]
    company: Option<String>,
    /// Custom values by field key, each of its field's type.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    fields: Option<Map<String, Value>>,
    /// The groups the person joins, at most 100.
    #[garde(length(max = GROUPS_MAX))]
    #[schema(value_type = Option<Vec<String>>)]
    group_ids: Option<Vec<Id<Group>>>,
}

/// Create a person.
#[utoipa::path(
    post,
    path = "/people",
    tag = "Audience",
    operation_id = "people.create",
    request_body(content = CreatePerson, example = json!({"email": "ada@example.com", "given_name": "Ada", "company": "Analytical", "fields": {"industry": "Computing"}})),
    responses(
        (status = 201, description = "The person.", body = PersonObject,
         headers(("ETag" = String, description = "The person's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "A group does not exist (`not_found`)."),
        (status = 409, description = "A person with this address exists (`conflict`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_person(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreatePerson>,
) -> ApiResult<(StatusCode, Tagged<PersonObject>)> {
    principal.require(Scope::PeopleWrite)?;
    let ws = principal.workspace;
    let person = NewPerson {
        given_name: name(body.given_name.as_deref(), "/given_name")?,
        family_name: name(body.family_name.as_deref(), "/family_name")?,
        company: name(body.company.as_deref(), "/company")?,
        email: body.email,
        fields: body.fields.unwrap_or_default(),
        group_ids: body.group_ids.unwrap_or_default(),
    };
    let mut tx = state.db.begin_in(ws).await?;
    let definitions = locked_definitions(&mut tx, ws).await?;
    check_fields(&definitions, &person.fields)?;
    let person = super::create(&mut tx, ws, &person).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: person.version,
            body: person,
        },
    ))
}

/// Retrieve a person.
#[utoipa::path(
    get,
    path = "/people/{id}",
    tag = "Audience",
    operation_id = "people.retrieve",
    params(("id" = Id<Person>, Path, description = "The person id (`per_…`).")),
    responses(
        (status = 200, description = "The person.", body = PersonObject,
         headers(("ETag" = String, description = "The person's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 404, description = "No such person in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_person(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Person>>,
) -> ApiResult<Tagged<PersonObject>> {
    principal.require(Scope::PeopleRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let person = super::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("person"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: person.version,
        body: person,
    })
}

/// The body of `PATCH /people/{id}`: absent members stay as they are.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdatePerson {
    /// A new address; unique in the workspace ignoring ASCII case.
    #[garde(skip)]
    #[schema(value_type = Option<String>, format = Email)]
    email: Option<EmailAddress>,
    /// A new given name; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    given_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    family_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    company: Option<Option<String>>,
    /// Custom values merged into the person's; `null` removes one.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    fields: Option<Map<String, Value>>,
    /// The person's groups, replacing the current ones whole; at most 100.
    #[garde(length(max = GROUPS_MAX))]
    #[schema(value_type = Option<Vec<String>>)]
    group_ids: Option<Vec<Id<Group>>>,
}

/// A name member of a `PATCH`: absent, cleared, or set (trimmed and bounded).
fn name_change(
    value: Option<&Option<String>>,
    pointer: &str,
) -> Result<Option<Option<String>>, Problem> {
    match value {
        None => Ok(None),
        Some(None) => Ok(Some(None)),
        Some(Some(text)) => {
            if text.chars().count() > NAME_MAX {
                return Err(Problem::invalid_field(
                    pointer,
                    "length",
                    "at most 200 characters",
                ));
            }
            name(Some(text), pointer).map(Some)
        }
    }
}

/// Update a person: its address, names, company, custom values (merged) or groups (replaced).
#[utoipa::path(
    patch,
    path = "/people/{id}",
    tag = "Audience",
    operation_id = "people.update",
    params(("id" = Id<Person>, Path, description = "The person id (`per_…`)."), IfMatch),
    request_body(content = UpdatePerson, example = json!({"company": "Analytical Engines", "fields": {"industry": null}, "group_ids": ["grp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"]})),
    responses(
        (status = 200, description = "The person.", body = PersonObject,
         headers(("ETag" = String, description = "The person's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such person, or a group does not exist."),
        (status = 409, description = "Another person has the new address (`conflict`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_person(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Person>>,
    if_match: IfMatch,
    Json(body): Json<UpdatePerson>,
) -> ApiResult<Tagged<PersonObject>> {
    principal.require(Scope::PeopleWrite)?;
    let ws = principal.workspace;
    let changes = PersonChanges {
        given_name: name_change(body.given_name.as_ref(), "/given_name")?,
        family_name: name_change(body.family_name.as_ref(), "/family_name")?,
        company: name_change(body.company.as_ref(), "/company")?,
        email: body.email,
        fields: body.fields,
        group_ids: body.group_ids,
    };
    let mut tx = state.db.begin_in(ws).await?;
    let current = super::lock_version(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("person"))?;
    if_match.check(current)?;
    if let Some(values) = &changes.fields {
        let definitions = locked_definitions(&mut tx, ws).await?;
        check_fields(&definitions, values)?;
    }
    let person = super::update(&mut tx, ws, id, &changes).await?;
    tx.commit().await?;
    Ok(Tagged {
        version: person.version,
        body: person,
    })
}

/// Delete a person and its memberships.
///
/// A person with enrollments or messages cannot be deleted: its history refers to it.
#[utoipa::path(
    delete,
    path = "/people/{id}",
    tag = "Audience",
    operation_id = "people.delete",
    params(("id" = Id<Person>, Path, description = "The person id (`per_…`).")),
    responses(
        (status = 204, description = "The person is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such person in this workspace."),
        (status = 409, description = "The person has enrollments or messages (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn delete_person(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Person>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::PeopleWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    super::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── fields ─────────────────────────────

/// List the workspace's custom fields (at most 100).
#[utoipa::path(
    get,
    path = "/fields",
    tag = "Audience",
    operation_id = "fields.list",
    params(
        ("limit" = Option<i64>, Query, description = "Fields per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count."),
    ),
    responses(
        (status = 200, description = "A page of fields.", body = Page<FieldObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_fields(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
) -> ApiResult<Json<Page<FieldObject>>> {
    principal.require(Scope::PeopleRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "fields", "id", &(), &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = fields::list(
        &mut tx,
        ws,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(fields::count(&mut tx, ws).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |field| field.id.uuid(),
    )))
}

/// The body of `POST /fields`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateField {
    /// How values are stored and named: a lowercase letter, then up to 63 lowercase letters,
    /// digits or underscores; not a person attribute's name. It never changes.
    #[garde(skip)]
    key: String,
    /// The name people read, 1 to 100 characters.
    #[garde(length(chars, min = 1, max = LABEL_MAX))]
    label: String,
    /// The values' type; it never changes.
    #[serde(rename = "type")]
    #[garde(skip)]
    field_type: FieldType,
    /// An enum's allowed values: 1 to 100 distinct strings of 1 to 100 characters. Only for
    /// `enum`.
    #[garde(length(max = OPTIONS_MAX), inner(inner(length(chars, min = 1, max = OPTION_MAX))))]
    options: Option<Vec<String>>,
}

/// Checks an enum's options: distinct, and present exactly for an enum.
fn check_options(field_type: FieldType, options: Option<&[String]>) -> Result<(), Problem> {
    match (field_type, options) {
        (FieldType::Enum, Some(options)) if !options.is_empty() => {
            let distinct: std::collections::HashSet<&String> = options.iter().collect();
            if distinct.len() == options.len() {
                Ok(())
            } else {
                Err(Problem::invalid_field(
                    "/options",
                    "invalid",
                    "options are distinct",
                ))
            }
        }
        (FieldType::Enum, _) => Err(Problem::invalid_field(
            "/options",
            "required",
            "an enum field has 1 to 100 options",
        )),
        (_, Some(options)) if !options.is_empty() => Err(Problem::invalid_field(
            "/options",
            "invalid",
            "only an enum field has options",
        )),
        _ => Ok(()),
    }
}

/// Create a custom field.
#[utoipa::path(
    post,
    path = "/fields",
    tag = "Audience",
    operation_id = "fields.create",
    request_body(content = CreateField, example = json!({"key": "tier", "label": "Tier", "type": "enum", "options": ["gold", "silver"]})),
    responses(
        (status = 201, description = "The field.", body = FieldObject,
         headers(("ETag" = String, description = "The field's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 409, description = "The key is taken (`conflict`), or the workspace has 100 fields (`invalid_state`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_field(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateField>,
) -> ApiResult<(StatusCode, Tagged<FieldObject>)> {
    principal.require(Scope::PeopleWrite)?;
    attributes::check_key(&body.key)
        .map_err(|detail| Problem::invalid_field("/key", "format", detail))?;
    check_options(body.field_type, body.options.as_deref())?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let field = fields::create(
        &mut tx,
        principal.workspace,
        &NewField {
            key: body.key,
            label: body.label,
            field_type: body.field_type,
            options: body.options.unwrap_or_default(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: field.version,
            body: field,
        },
    ))
}

/// The body of `PATCH /fields/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateField {
    /// A new label, 1 to 100 characters.
    #[garde(length(chars, min = 1, max = LABEL_MAX))]
    label: Option<String>,
    /// An enum's options, replacing the current ones whole. An option a person holds, or a
    /// segment compares with, cannot be removed.
    #[garde(length(min = 1, max = OPTIONS_MAX), inner(inner(length(chars, min = 1, max = OPTION_MAX))))]
    options: Option<Vec<String>>,
}

/// Update a custom field's label or an enum's options. Its key and type never change.
#[utoipa::path(
    patch,
    path = "/fields/{id}",
    tag = "Audience",
    operation_id = "fields.update",
    params(("id" = Id<Field>, Path, description = "The field id (`fld_…`)."), IfMatch),
    request_body(content = UpdateField, example = json!({"options": ["gold", "silver", "bronze"]})),
    responses(
        (status = 200, description = "The field.", body = FieldObject,
         headers(("ETag" = String, description = "The field's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such field in this workspace."),
        (status = 409, description = "Options on a field that is not an enum, or a removed option still in use (`invalid_state`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_field(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Field>>,
    if_match: IfMatch,
    Json(body): Json<UpdateField>,
) -> ApiResult<Tagged<FieldObject>> {
    principal.require(Scope::PeopleWrite)?;
    if let Some(options) = &body.options {
        check_options(FieldType::Enum, Some(options))?;
    }
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let current = fields::lock_version(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("field"))?;
    if_match.check(current)?;
    let field = fields::update(
        &mut tx,
        principal.workspace,
        id,
        &FieldChanges {
            label: body.label,
            options: body.options,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Tagged {
        version: field.version,
        body: field,
    })
}

/// Delete a custom field and every person's value of it.
///
/// A field a segment uses cannot be deleted.
#[utoipa::path(
    delete,
    path = "/fields/{id}",
    tag = "Audience",
    operation_id = "fields.delete",
    params(("id" = Id<Field>, Path, description = "The field id (`fld_…`).")),
    responses(
        (status = 204, description = "The field and its values are deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such field in this workspace."),
        (status = 409, description = "A segment uses the field (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn delete_field(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Field>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::PeopleWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    fields::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── groups ─────────────────────────────

/// The filters of `GET /groups`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct GroupQuery {
    q: Option<String>,
}

/// List the workspace's groups, newest first by default, each with its people counted.
#[utoipa::path(
    get,
    path = "/groups",
    tag = "Audience",
    operation_id = "groups.list",
    params(
        ("limit" = Option<i64>, Query, description = "Groups per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("q" = Option<String>, Query, description = "A prefix of the name, ignoring case."),
    ),
    responses(
        (status = 200, description = "A page of groups.", body = Page<GroupObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_groups(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<GroupQuery>,
) -> ApiResult<Json<Page<GroupObject>>> {
    principal.require(Scope::PeopleRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "groups", "id", &query, &list)?;
    let prefix = query.q.as_deref().and_then(super::like_prefix);
    let mut tx = state.db.begin_in(ws).await?;
    let rows = groups::list(
        &mut tx,
        ws,
        prefix.as_deref(),
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(groups::count(&mut tx, ws, prefix.as_deref(), COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |group| group.id.uuid(),
    )))
}

/// The body of `POST /groups`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateGroup {
    /// 1 to 200 characters.
    #[garde(length(chars, min = 1, max = NAME_LONG))]
    name: String,
    /// At most 1,000 characters.
    #[garde(length(chars, max = DESCRIPTION_MAX))]
    description: Option<String>,
}

/// Create an empty group.
#[utoipa::path(
    post,
    path = "/groups",
    tag = "Audience",
    operation_id = "groups.create",
    request_body(content = CreateGroup, example = json!({"name": "Conference leads", "description": "Met at the 2026 summit"})),
    responses(
        (status = 201, description = "The group.", body = GroupObject,
         headers(("ETag" = String, description = "The group's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_group(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateGroup>,
) -> ApiResult<(StatusCode, Tagged<GroupObject>)> {
    principal.require(Scope::PeopleWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let group = groups::create(
        &mut tx,
        principal.workspace,
        &body.name,
        body.description.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: group.version,
            body: group,
        },
    ))
}

/// Retrieve a group with its people counted.
#[utoipa::path(
    get,
    path = "/groups/{id}",
    tag = "Audience",
    operation_id = "groups.retrieve",
    params(("id" = Id<Group>, Path, description = "The group id (`grp_…`).")),
    responses(
        (status = 200, description = "The group.", body = GroupObject,
         headers(("ETag" = String, description = "The group's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 404, description = "No such group in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_group(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Group>>,
) -> ApiResult<Tagged<GroupObject>> {
    principal.require(Scope::PeopleRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let group = groups::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("group"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: group.version,
        body: group,
    })
}

/// The body of `PATCH /groups/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateGroup {
    #[garde(length(chars, min = 1, max = NAME_LONG))]
    name: Option<String>,
    /// A new description; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    description: Option<Option<String>>,
}

/// Rename a group or change its description.
#[utoipa::path(
    patch,
    path = "/groups/{id}",
    tag = "Audience",
    operation_id = "groups.update",
    params(("id" = Id<Group>, Path, description = "The group id (`grp_…`)."), IfMatch),
    request_body(content = UpdateGroup, example = json!({"name": "Summit leads"})),
    responses(
        (status = 200, description = "The group.", body = GroupObject,
         headers(("ETag" = String, description = "The group's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such group in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_group(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Group>>,
    if_match: IfMatch,
    Json(body): Json<UpdateGroup>,
) -> ApiResult<Tagged<GroupObject>> {
    principal.require(Scope::PeopleWrite)?;
    if let Some(Some(description)) = &body.description
        && description.chars().count() > DESCRIPTION_MAX
    {
        return Err(Problem::invalid_field(
            "/description",
            "length",
            "at most 1,000 characters",
        ));
    }
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let current = groups::lock_version(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("group"))?;
    if_match.check(current)?;
    let group = groups::update(
        &mut tx,
        principal.workspace,
        id,
        body.name.as_deref(),
        body.description.as_ref().map(Option::as_deref),
    )
    .await?
    .ok_or_else(|| Problem::not_found("group"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: group.version,
        body: group,
    })
}

/// Delete a group and its memberships; its people stay.
#[utoipa::path(
    delete,
    path = "/groups/{id}",
    tag = "Audience",
    operation_id = "groups.delete",
    params(("id" = Id<Group>, Path, description = "The group id (`grp_…`).")),
    responses(
        (status = 204, description = "The group is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such group in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_group(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Group>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::PeopleWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let deleted = groups::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("group"))
    }
}

// ───────────────────────────── segments ─────────────────────────────

/// List the workspace's segments, newest first by default. A list leaves the counts out.
#[utoipa::path(
    get,
    path = "/segments",
    tag = "Audience",
    operation_id = "segments.list",
    params(
        ("limit" = Option<i64>, Query, description = "Segments per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
    ),
    responses(
        (status = 200, description = "A page of segments.", body = Page<SegmentObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_segments(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
) -> ApiResult<Json<Page<SegmentObject>>> {
    principal.require(Scope::PeopleRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "segments", "id", &(), &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = segments::list(
        &mut tx,
        ws,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(segments::count(&mut tx, ws, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |segment| segment.id.uuid(),
    )))
}

/// The body of `POST /segments`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateSegment {
    /// 1 to 200 characters.
    #[garde(length(chars, min = 1, max = NAME_LONG))]
    name: String,
    /// Who the segment means.
    #[garde(skip)]
    filter: Filter,
}

/// Checks `filter` against the definitions read under the shared field lock.
async fn check_filter(tx: &mut Tx, workspace: WorkspaceId, filter: &Filter) -> Result<(), Problem> {
    let definitions = locked_definitions(tx, workspace).await?;
    crate::domain::segments::compile(filter, &definitions)
        .map(|_| ())
        .map_err(|errors| {
            Problem::validation(
                errors
                    .into_iter()
                    .map(|error| FieldError {
                        pointer: format!("/filter{}", error.pointer),
                        code: "invalid".to_owned(),
                        detail: error.problem,
                    })
                    .collect(),
            )
        })
}

/// Create a segment; the response counts its people.
#[utoipa::path(
    post,
    path = "/segments",
    tag = "Audience",
    operation_id = "segments.create",
    request_body(content = CreateSegment, example = json!({"name": "Gold accounts", "filter": {"match": "all", "conditions": [{"field": "fields.tier", "operator": "equals", "value": "gold"}, {"field": "email_domain", "operator": "not_equals", "value": "gmail.com"}]}})),
    responses(
        (status = 201, description = "The segment.", body = SegmentObject,
         headers(("ETag" = String, description = "The segment's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 422, description = "The body or the filter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_segment(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateSegment>,
) -> ApiResult<(StatusCode, Tagged<SegmentObject>)> {
    principal.require(Scope::PeopleWrite)?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    check_filter(&mut tx, ws, &body.filter).await?;
    let segment = segments::create(&mut tx, ws, &body.name, &body.filter).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: segment.version,
            body: segment,
        },
    ))
}

/// Retrieve a segment with its people counted now (up to 10,000).
#[utoipa::path(
    get,
    path = "/segments/{id}",
    tag = "Audience",
    operation_id = "segments.retrieve",
    params(("id" = Id<Segment>, Path, description = "The segment id (`seg_…`).")),
    responses(
        (status = 200, description = "The segment.", body = SegmentObject,
         headers(("ETag" = String, description = "The segment's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 404, description = "No such segment in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_segment(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Segment>>,
) -> ApiResult<Tagged<SegmentObject>> {
    principal.require(Scope::PeopleRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let segment = segments::retrieve(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("segment"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: segment.version,
        body: segment,
    })
}

/// The body of `PATCH /segments/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateSegment {
    #[garde(length(chars, min = 1, max = NAME_LONG))]
    name: Option<String>,
    /// A new filter, replacing the current one.
    #[garde(skip)]
    filter: Option<Filter>,
}

/// Rename a segment or replace its filter.
#[utoipa::path(
    patch,
    path = "/segments/{id}",
    tag = "Audience",
    operation_id = "segments.update",
    params(("id" = Id<Segment>, Path, description = "The segment id (`seg_…`)."), IfMatch),
    request_body(content = UpdateSegment, example = json!({"name": "Gold and silver", "filter": {"conditions": [{"field": "fields.tier", "operator": "in", "value": ["gold", "silver"]}]}})),
    responses(
        (status = 200, description = "The segment.", body = SegmentObject,
         headers(("ETag" = String, description = "The segment's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such segment in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body or the filter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_segment(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Segment>>,
    if_match: IfMatch,
    Json(body): Json<UpdateSegment>,
) -> ApiResult<Tagged<SegmentObject>> {
    principal.require(Scope::PeopleWrite)?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let current = segments::lock_version(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("segment"))?;
    if_match.check(current)?;
    if let Some(filter) = &body.filter {
        check_filter(&mut tx, ws, filter).await?;
    }
    let segment =
        segments::update(&mut tx, ws, id, body.name.as_deref(), body.filter.as_ref()).await?;
    tx.commit().await?;
    Ok(Tagged {
        version: segment.version,
        body: segment,
    })
}

/// Delete a segment; its people stay.
#[utoipa::path(
    delete,
    path = "/segments/{id}",
    tag = "Audience",
    operation_id = "segments.delete",
    params(("id" = Id<Segment>, Path, description = "The segment id (`seg_…`).")),
    responses(
        (status = 204, description = "The segment is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such segment in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_segment(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Segment>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::PeopleWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let deleted = segments::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("segment"))
    }
}

// ───────────────────────────── suppressions ─────────────────────────────

/// List the workspace's suppressions, newest first by default.
#[utoipa::path(
    get,
    path = "/suppressions",
    tag = "Audience",
    operation_id = "suppressions.list",
    params(
        ("limit" = Option<i64>, Query, description = "Suppressions per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("email" = Option<String>, Query, description = "The suppression of this address (ignoring ASCII case)."),
        ("reason" = Option<Reason>, Query, description = "Only suppressions for this reason."),
        ("source" = Option<Source>, Query, description = "Only suppressions from this source."),
    ),
    responses(
        (status = 200, description = "A page of suppressions.", body = Page<SuppressionObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_suppressions(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(filters): Query<SuppressionFilters>,
) -> ApiResult<Json<Page<SuppressionObject>>> {
    principal.require(Scope::PeopleRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "suppressions", "id", &filters, &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = suppressions::list(
        &mut tx,
        ws,
        &filters,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(suppressions::count(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |suppression| suppression.id.uuid(),
    )))
}

/// The one-address form of `POST /suppressions`: answers the suppression (`201`).
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateSuppression {
    /// The address to stop mailing.
    #[garde(skip)]
    #[schema(value_type = String, format = Email)]
    email: EmailAddress,
    /// Always `manual` (the default): every other reason comes from evidence.
    #[garde(skip)]
    reason: Option<Reason>,
}

/// The list form of `POST /suppressions`: up to 1,000 addresses suppressed as one decision,
/// answered with what it did (`200`).
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateSuppressions {
    /// 1 to 1,000 addresses to stop mailing. Each entry is read on its own: one that is not an
    /// address is reported in `invalid` and the others are suppressed; an address given twice,
    /// in any case, counts once.
    #[garde(length(min = 1, max = LIST_MAX))]
    #[schema(min_items = 1, max_items = 1000)]
    emails: Vec<String>,
    /// Always `manual` (the default): every other reason comes from evidence.
    #[garde(skip)]
    reason: Option<Reason>,
}

/// The forms of `POST /suppressions`: one address or a list. Each form refuses the member the
/// other requires (`email`, `emails`), so a body is one form only.
#[derive(Debug, utoipa::ToSchema)]
#[serde(untagged)]
enum SuppressionForm {
    One(CreateSuppression),
    Many(CreateSuppressions),
}

/// What a list of addresses did (the list form of `POST /suppressions`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct SuppressedAddresses {
    /// Addresses this request suppressed.
    created: usize,
    /// Addresses that were suppressed before this request, for any reason; they stay as they
    /// were.
    already: usize,
    /// The entries that are not addresses, in the order given; nothing was done for them.
    #[schema(max_items = 1000)]
    invalid: Vec<InvalidAddress>,
}

/// An entry of `emails` that is not an address.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct InvalidAddress {
    /// Its position in `emails`, from 0 (its pointer is `/emails/{index}`).
    index: usize,
    /// The entry as given.
    value: String,
    /// Why it is not an address.
    detail: String,
}

/// Refuses a reason other than `manual`: every other reason comes from evidence.
fn manual(reason: Option<Reason>) -> Result<(), Problem> {
    if reason.is_some_and(|reason| reason != Reason::Manual) {
        return Err(Problem::invalid_field(
            "/reason",
            "invalid",
            "only `manual` is written by hand; the other reasons come from evidence",
        ));
    }
    Ok(())
}

/// Suppress an address, or a list of them: no mail of the workspace reaches them any more.
///
/// Two forms. `email` suppresses one address and answers the suppression (`201`), or `409` when
/// it is suppressed already. `emails` suppresses up to 1,000 addresses as one decision and
/// answers what it did (`200`): how many it suppressed, how many were suppressed already, and
/// each entry that is not an address. Either way the reason is `manual`, and every live
/// enrollment of an address suppressed now stops in the same transaction.
#[utoipa::path(
    post,
    path = "/suppressions",
    tag = "Audience",
    operation_id = "suppressions.create",
    // Which answer each form receives, for generated clients. The invariants test holds every
    // form and answer named here to the operation's body and responses.
    extensions(("x-norbelys-overloads" = json!([
        { "request": { "$ref": "#/components/schemas/CreateSuppression" }, "response": { "$ref": "#/components/schemas/SuppressionObject" } },
        { "request": { "$ref": "#/components/schemas/CreateSuppressions" }, "response": { "$ref": "#/components/schemas/SuppressedAddresses" } }
    ]))),
    request_body(content = SuppressionForm, examples(
        ("One address" = (summary = "Suppress one address", value = json!({"email": "no-mail@example.com"}))),
        ("A list" = (summary = "Suppress a list of addresses in one decision", value = json!({
            "emails": ["no-mail@example.com", "Former.Customer@example.org", "not an address"]
        })))
    )),
    responses(
        (status = 200, description = "For `emails`: what the request did.", body = SuppressedAddresses),
        (status = 201, description = "For `email`: the suppression.", body = SuppressionObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 409, description = "For `email`: the address is suppressed already (`conflict`)."),
        (status = 422, description = "The body is invalid: neither or both of `email` and `emails`, an `email` that is not an address, more than 1,000 `emails`, or a reason other than `manual`."),
    ),
    security(("bearer" = []))
)]
async fn create_suppression(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<extract::Object>,
) -> ApiResult<Response> {
    principal.require(Scope::PeopleWrite)?;
    // The two forms differ by `email` and `emails`. Telling them apart first names a body with
    // both or neither as such, rather than as a member the other form does not know.
    let form = match (body.0.contains_key("email"), body.0.contains_key("emails")) {
        (true, false) => SuppressionForm::One(body.parse()?),
        (false, true) => SuppressionForm::Many(body.parse()?),
        _ => {
            return Err(Problem::invalid_field(
                "",
                "required",
                "Give exactly one of `email` or `emails`.",
            ));
        }
    };
    match form {
        SuppressionForm::One(body) => suppress_one(&principal, &state, body).await,
        SuppressionForm::Many(body) => suppress_list(&principal, &state, body).await,
    }
}

/// The one-address form of [`create_suppression`].
async fn suppress_one(
    principal: &Principal,
    state: &AppState,
    body: CreateSuppression,
) -> ApiResult<Response> {
    manual(body.reason)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let suppression = suppressions::create(
        &mut tx,
        principal.workspace,
        &NewSuppression {
            email: &body.email,
            reason: Reason::Manual,
            source: Source::Manual,
            source_event: None,
            evidence: None,
            created_by: principal.actor.id(),
        },
    )
    .await?;
    // The address may not be mailed: every live enrollment of its person ends now, in the same
    // transaction, rather than at its next step.
    crate::campaigns::enrollments::stop_suppressed(
        &mut tx,
        principal.workspace,
        body.email.as_str(),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(suppression)).into_response())
}

/// The list form of [`create_suppression`]: the entries are read on their own
/// (`domain::suppressions::read_list`), then the suppressions are written and the live
/// enrollments of the addresses suppressed now are stopped, all in one transaction, so the list
/// is one decision: all of it is written, or none of it when the transaction fails.
async fn suppress_list(
    principal: &Principal,
    state: &AppState,
    body: CreateSuppressions,
) -> ApiResult<Response> {
    manual(body.reason)?;
    let ws = principal.workspace;
    let list = read_list(&body.emails);
    let created = if list.addresses.is_empty() {
        0
    } else {
        let mut tx = state.db.begin_in(ws).await?;
        let written =
            suppressions::create_all(&mut tx, ws, &list.addresses, &principal.actor.id()).await?;
        // The addresses may not be mailed: every live enrollment of their people ends now, in
        // the same transaction, rather than at its next step.
        let emails: Vec<String> = written
            .iter()
            .map(|suppression| suppression.email.clone())
            .collect();
        crate::campaigns::enrollments::stop_suppressed_all(&mut tx, ws, &emails).await?;
        tx.commit().await?;
        written.len()
    };
    let invalid = list
        .refused
        .into_iter()
        .map(|refused| InvalidAddress {
            index: refused.index,
            value: refused.value,
            detail: refused.error.to_string(),
        })
        .collect();
    Ok(Json(SuppressedAddresses {
        created,
        already: list.addresses.len().saturating_sub(created),
        invalid,
    })
    .into_response())
}

/// Retrieve a suppression.
#[utoipa::path(
    get,
    path = "/suppressions/{id}",
    tag = "Audience",
    operation_id = "suppressions.retrieve",
    params(("id" = Id<Suppression>, Path, description = "The suppression id (`sup_…`).")),
    responses(
        (status = 200, description = "The suppression.", body = SuppressionObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 404, description = "No such suppression in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_suppression(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Suppression>>,
) -> ApiResult<Json<SuppressionObject>> {
    principal.require(Scope::PeopleRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let suppression = suppressions::read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("suppression"))?;
    tx.commit().await?;
    Ok(Json(suppression))
}

/// Remove a manual suppression.
///
/// The removal is audited. Suppressions that evidence created are read-only.
#[utoipa::path(
    delete,
    path = "/suppressions/{id}",
    tag = "Audience",
    operation_id = "suppressions.delete",
    params(("id" = Id<Suppression>, Path, description = "The suppression id (`sup_…`).")),
    responses(
        (status = 204, description = "The suppression is removed."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "No such suppression in this workspace."),
        (status = 409, description = "The suppression's reason is not `manual` (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn delete_suppression(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Suppression>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::PeopleWrite)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    suppressions::delete(&mut tx, principal.workspace, id, principal.actor).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── imports ─────────────────────────────

/// The filters of `GET /imports`; the query of `POST /imports` with a CSV body.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ImportQuery {
    status: Option<ImportStatus>,
    group_id: Option<Id<Group>>,
}

/// List the workspace's imports, newest first by default.
#[utoipa::path(
    get,
    path = "/imports",
    tag = "Audience",
    operation_id = "imports.list",
    params(
        ("limit" = Option<i64>, Query, description = "Imports per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<ImportStatus>, Query, description = "Only imports in this status."),
    ),
    responses(
        (status = 200, description = "A page of imports.", body = Page<ImportObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_imports(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<ImportQuery>,
) -> ApiResult<Json<Page<ImportObject>>> {
    principal.require(Scope::PeopleRead)?;
    let ws = principal.workspace;
    let status: Option<&'static str> = query.status.map(Into::into);
    let params = PageParams::from_query(&state.keys, ws, "imports", "id", &status, &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = imports::list(
        &mut tx,
        links(&state),
        ws,
        status,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(imports::count(&mut tx, ws, status, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |import| import.id.uuid(),
    )))
}

/// The JSON body of `POST /imports`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateImport {
    /// 1 to 1,000 people, each `{email, given_name?, family_name?, company?, fields?}`; each is
    /// checked by the import, which reports the invalid ones.
    #[garde(length(min = 1, max = imports::PEOPLE_MAX))]
    #[schema(value_type = Vec<Object>)]
    people: Vec<Value>,
    /// The group every imported person joins.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    group_id: Option<Id<Group>>,
}

/// Import people from a CSV file or from JSON.
///
/// A CSV file is the body (`Content-Type: text/csv`, at most 16 MiB, its first record the header;
/// `group_id` as a query parameter); JSON names up to 1,000 `people`. The import runs as a job:
/// poll the import, or listen for `import.completed`.
#[utoipa::path(
    post,
    path = "/imports",
    tag = "Audience",
    operation_id = "imports.create",
    params(("group_id" = Option<Id<Group>>, Query, description = "With a CSV body: the group every imported person joins.")),
    request_body(content(
        (CreateImport = "application/json", example = json!({"people": [{"email": "ada@example.com", "given_name": "Ada", "fields": {"tier": "gold"}}], "group_id": "grp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"})),
        (String = "text/csv", example = json!("email,first_name,company,tier\nada@example.com,Ada,Analytical,gold\n"))
    )),
    responses(
        (status = 202, description = "The import, queued; `Location` names it.", body = ImportObject,
         headers(("Location" = String, description = "The import's path."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write`."),
        (status = 404, description = "The group does not exist (`not_found`)."),
        (status = 413, description = "The file is larger than 16 MiB."),
        (status = 415, description = "The body is neither `text/csv` nor `application/json`."),
        (status = 422, description = "The body is invalid, or the file has no email column."),
        (status = 503, description = "Object storage is unavailable; retry with the same key."),
    ),
    security(("bearer" = []))
)]
async fn create_import(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<ImportQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, [(HeaderName, String); 1], Json<ImportObject>)> {
    principal.require(Scope::PeopleWrite)?;
    let essence = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|essence| essence.trim().to_ascii_lowercase());
    let (input, group) = match essence.as_deref() {
        Some("text/csv") => {
            if body.is_empty() {
                return Err(Problem::invalid_field(
                    "",
                    "required",
                    "the CSV file is empty",
                ));
            }
            (Input::Csv(body), query.group_id)
        }
        Some("application/json") => {
            let body: CreateImport = extract::parse(&body)?;
            (Input::Json(body.people), body.group_id)
        }
        _ => {
            return Err(Problem::new(
                Code::UnsupportedMediaType,
                "Send a CSV file as `text/csv`, or people as `application/json`.",
            ));
        }
    };
    let import =
        imports::create(&state.db, links(&state), principal.workspace, input, group).await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/v1/imports/{}", import.id))],
        Json(import),
    ))
}

/// Retrieve an import: its status, counts, first problems and the link to its error report.
#[utoipa::path(
    get,
    path = "/imports/{id}",
    tag = "Audience",
    operation_id = "imports.retrieve",
    params(("id" = Id<Import>, Path, description = "The import id (`imp_…`).")),
    responses(
        (status = 200, description = "The import.", body = ImportObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 404, description = "No such import in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_import(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Import>>,
) -> ApiResult<Json<ImportObject>> {
    principal.require(Scope::PeopleRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let import = imports::read(&mut tx, links(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("import"))?;
    tx.commit().await?;
    Ok(Json(import))
}

// ───────────────────────────── exports ─────────────────────────────

/// The status filter of `GET /exports`, on the stored status: an export whose file has expired
/// still matches its stored status (`ready`), and reads as `expired`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, strum::IntoStaticStr, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
enum ExportStatus {
    Queued,
    Running,
    Ready,
    Failed,
}

/// The filters of `GET /exports`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ExportQuery {
    status: Option<ExportStatus>,
}

/// List the workspace's exports, newest first by default.
#[utoipa::path(
    get,
    path = "/exports",
    tag = "Audience",
    operation_id = "exports.list",
    params(
        ("limit" = Option<i64>, Query, description = "Exports per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = inline(Option<ExportStatus>), Query, description = "Only exports in this stored status (an expired export matches `ready`)."),
    ),
    responses(
        (status = 200, description = "A page of exports.", body = Page<ExportObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_exports(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<ExportQuery>,
) -> ApiResult<Json<Page<ExportObject>>> {
    principal.require(
        exports::Resource::iter()
            .map(exports::Resource::read_scope)
            .find(|scope| principal.scopes.contains(*scope))
            .unwrap_or(Scope::PeopleRead),
    )?;
    // An export holds the rows of its resource's list: a credential sees the exports of the
    // resources it may read, and none at all without any of their scopes.
    let kinds: Vec<String> = exports::Resource::iter()
        .filter(|resource| principal.scopes.contains(resource.read_scope()))
        .map(|resource| <&'static str>::from(resource).to_owned())
        .collect();
    let ws = principal.workspace;
    let status: Option<&'static str> = query.status.map(Into::into);
    let params = PageParams::from_query(&state.keys, ws, "exports", "id", &status, &list)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = exports::list(&mut tx, links(&state), ws, &kinds, status, &params).await?;
    let total = match params.include_total {
        true => Some(exports::count(&mut tx, ws, &kinds, status, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |export| export.id.uuid(),
    )))
}

/// The body of `POST /exports`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateExport {
    /// What the file contains.
    #[garde(skip)]
    resource: exports::Resource,
    /// The filters of the resource's list. For `people`: `email`, `group_id`, `segment_id`, `q`,
    /// `created_at[gte]` and the other ranges. For history, `created_at[gte]` and
    /// `created_at[lt]` (when a message was created, an attempt claimed, an event recorded, an
    /// inbound message received) and: for `messages`, `campaign_id`, `connection_id`,
    /// `person_id`, `thread_id`, `state`; for `attempts`, `message_id`, `connection_id`; for
    /// `delivery_events`, `message_id`, `kind`; for `inbound_messages`, `message_id`,
    /// `connection_id`, `person_id`, `thread_id`, `classification`.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    filters: Option<Value>,
    /// `csv` (the default) or `jsonl`.
    #[garde(skip)]
    format: Option<exports::Format>,
}

/// Export a resource's list to a file.
///
/// The export runs as a job: poll the export for its link, or listen for `export.completed`.
/// History (`messages`, `attempts`, `delivery_events`, `inbound_messages`) includes the periods
/// already archived out of the database.
#[utoipa::path(
    post,
    path = "/exports",
    tag = "Audience",
    operation_id = "exports.create",
    request_body(content = CreateExport, example = json!({"resource": "people", "filters": {"group_id": "grp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"}, "format": "csv"})),
    responses(
        (status = 202, description = "The export, queued; `Location` names it.", body = ExportObject,
         headers(("Location" = String, description = "The export's path."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:write` (people), `messages:read` (messages, attempts, delivery events) or `inbox:read` (inbound messages)."),
        (status = 404, description = "The filters name a group or segment that does not exist."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_export(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateExport>,
) -> ApiResult<(StatusCode, [(HeaderName, String); 1], Json<ExportObject>)> {
    // The scope depends on what is exported: people take `people:write`, a history the scope
    // its list reads.
    principal.require(body.resource.create_scope())?;
    let ws = principal.workspace;
    let given = body.filters.unwrap_or_else(|| Value::Object(Map::new()));
    let unreadable =
        |error: serde_json::Error| Problem::invalid_field("/filters", "invalid", error.to_string());
    let filters = match body.resource {
        exports::Resource::People => {
            let filters: PeopleFilters = serde_json::from_value(given).map_err(unreadable)?;
            let mut tx = state.db.begin_in(ws).await?;
            selection(&mut tx, ws, &filters).await?;
            tx.commit().await?;
            serde_json::to_value(&filters).map_err(unreadable)?
        }
        history => {
            let filters: exports::history::HistoryFilters =
                serde_json::from_value(given).map_err(unreadable)?;
            exports::history::check(history, &filters).map_err(|filter| {
                Problem::invalid_field(
                    &format!("/filters/{filter}"),
                    "unknown",
                    "This resource's list has no such filter.",
                )
            })?;
            serde_json::to_value(&filters).map_err(unreadable)?
        }
    };
    let export = exports::create(
        &state.db,
        links(&state),
        ws,
        &principal.actor.id(),
        body.resource,
        filters,
        body.format.unwrap_or_default(),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/v1/exports/{}", export.id))],
        Json(export),
    ))
}

/// Retrieve an export, with a fresh download link when it is ready.
#[utoipa::path(
    get,
    path = "/exports/{id}",
    tag = "Audience",
    operation_id = "exports.retrieve",
    params(("id" = Id<Export>, Path, description = "The export id (`exp_…`).")),
    responses(
        (status = 200, description = "The export.", body = ExportObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks the scope that reads the export's resource: `people:read`, `messages:read` (messages, attempts, delivery events) or `inbox:read` (inbound messages)."),
        (status = 404, description = "No such export in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_export(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Export>>,
) -> ApiResult<Json<ExportObject>> {
    principal.require(
        exports::Resource::iter()
            .map(exports::Resource::read_scope)
            .find(|scope| principal.scopes.contains(*scope))
            .unwrap_or(Scope::PeopleRead),
    )?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let export = exports::read(&mut tx, links(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("export"))?;
    tx.commit().await?;
    // The export's file holds its resource's rows: reading it takes that resource's scope.
    let resource = export
        .resource
        .parse::<exports::Resource>()
        .map_or(Scope::PeopleRead, exports::Resource::read_scope);
    principal.require(resource)?;
    Ok(Json(export))
}

// ───────────────────────────── preflight ─────────────────────────────

/// The body of `POST /preflight`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreatePreflight {
    /// 1 to 100 addresses, each at most 320 characters.
    #[garde(length(min = 1, max = preflight::ADDRESSES_MAX), inner(length(max = 320)))]
    emails: Vec<String>,
}

/// What preflight found, in the order of the request's addresses.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct PreflightResult {
    /// One per address asked about, at most 100.
    #[schema(max_items = 100)]
    data: Vec<Finding>,
}

/// Check addresses before mailing them.
///
/// Syntax, DNS routing (MX, an implicit MX, or a null MX that refuses all mail), and the
/// workspace's suppressions and holds. When configured, the remote mail host also checks SMTP
/// recipient acceptance without sending a message. Nothing is stored. SMTP acceptance is not
/// proof of delivery; unavailable checks are skipped. A route the workspace's sending found in
/// DNS within the last day is answered
/// from that, as the sender reads it; any other is asked of DNS now, and a lookup DNS could not
/// answer is `unknown`. In a test-mode workspace only the syntax is checked, as its sender does:
/// its mail never leaves the fake transport. SMTP checks share a 15-second budget; use batches
/// of at most eight addresses to avoid skipping addresses when that budget expires.
#[utoipa::path(
    post,
    path = "/preflight",
    tag = "Audience",
    operation_id = "preflight.create",
    request_body(content = CreatePreflight, example = json!({"emails": ["ada@example.com", "not-an-address"]})),
    responses(
        (status = 200, description = "One finding per address.", body = PreflightResult),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `people:read`."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_preflight(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreatePreflight>,
) -> ApiResult<Json<PreflightResult>> {
    principal.require(Scope::PeopleRead)?;
    let mut data = preflight::check(
        &state.db,
        &state.resolver,
        principal.workspace,
        &body.emails,
        principal.test_mode,
    )
    .await?;
    // Capacity is reported on unchecked rows; campaign jobs retry it instead.
    let _ = state
        .settings
        .senders
        .recipient_validation
        .preflight(&mut data, principal.test_mode)
        .await;
    Ok(Json(PreflightResult { data }))
}
