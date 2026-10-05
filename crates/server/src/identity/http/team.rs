//! Workspaces and their team: listing and creating workspaces (the session cookie), and, with a
//! workspace token, updating and deleting the workspace, its members, invitations, API keys and
//! audit log.
//!
//! Scopes: reading the members needs `workspace:read`; everything else here needs
//! `workspace:manage` (owners and admins), and the owner-only actions (deleting the workspace,
//! making someone an owner) are checked by the operations themselves.

use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::dashboard;
use crate::crypto::Keys;
use crate::delivery::accept::{self, Transactional};
use crate::domain::email::EmailAddress;
use crate::domain::identity::{Lasting, MembershipStatus, may_make};
use crate::domain::ids::{ApiKey, Id, Invitation, Membership, Workspace, WorkspaceId};
use crate::domain::scope::{MembershipRole, Scope, ScopeSet};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::api_keys::{self, ApiKeyObject, KeyMode, KeyStatus};
use crate::identity::audit::{self, Action, AuditActor, AuditObject};
use crate::identity::authority::{Actor, Principal, read_standing};
use crate::identity::invitations::{self, InvitationObject, InvitationStatus};
use crate::identity::memberships::{self, Change, MemberObject};
use crate::identity::sessions::SignedIn;
use crate::identity::workspaces::{self, NewWorkspace, WorkspaceObject};
use crate::pagination::{self, Include, ListQuery, Order, Page, PageParams};
use crate::problem::{ApiResult, Problem};

/// The routes of workspaces and their team.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_workspaces, create_workspace))
        .routes(routes!(update_workspace, delete_workspace))
        .routes(routes!(list_members))
        .routes(routes!(update_member, delete_member))
        .routes(routes!(list_invitations, create_invitations))
        .routes(routes!(delete_invitation))
        .routes(routes!(list_api_keys, create_api_key))
        .routes(routes!(update_api_key, delete_api_key))
        .routes(routes!(list_audit_log))
}

/// The client address hash of the request, for the audit log.
fn ip_hash(keys: &Keys, client: crate::http::ratelimit::ClientAddress) -> Vec<u8> {
    keys.hash_address(&client.as_key())
}

/// `page` with its capped total, when the list was asked for one.
pub(super) fn with_total<T>(page: Page<T>, counted: Option<i64>) -> Page<T> {
    match counted {
        Some(counted) => page.with_total(counted),
        None => page,
    }
}

// ───────────────────────────── workspaces ─────────────────────────────

/// A workspace of the signed-in user, with their role in it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UserWorkspaceObject {
    /// The workspace.
    #[serde(flatten)]
    pub workspace: WorkspaceObject,
    /// The user's role there.
    pub role: MembershipRole,
}

/// List the signed-in user's workspaces.
///
/// Those they are an active member of, with their role in each; the dashboard's switcher.
#[utoipa::path(
    get,
    path = "/workspaces",
    tag = "Dashboard",
    operation_id = "workspaces.list",
    params(
        ("limit" = Option<i64>, Query, description = "Workspaces per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
    ),
    responses(
        (status = 200, description = "A page of workspaces.", body = Page<UserWorkspaceObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "A bearer credential (`session_required`)."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("session" = []))
)]
async fn list_workspaces(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Query(list): Query<ListQuery>,
) -> ApiResult<Json<Page<UserWorkspaceObject>>> {
    // A user's list belongs to no workspace: its cursor is bound to the user's id instead, which
    // no workspace id can equal (both are UUIDv7 of different rows).
    let binding = WorkspaceId::trusted(signed_in.user.uuid());
    let params = PageParams::from_query(&app.keys, binding, "workspaces", "id", &(), &list)?;
    let mut tx = app.db.begin_as_user(signed_in.user).await?;
    let rows = workspaces::of_user(
        &mut tx,
        signed_in.user,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let counted = match params.include_total {
        true => {
            Some(workspaces::count_of_user(&mut tx, signed_in.user, pagination::COUNT_CAP).await?)
        }
        false => None,
    };
    tx.commit().await?;
    let rows = rows
        .into_iter()
        .map(|(workspace, role)| UserWorkspaceObject { workspace, role })
        .collect();
    let page = Page::new(&app.keys, &params, rows, |row| {
        pagination::by_id(row.workspace.id.uuid())
    });
    Ok(Json(with_total(page, counted)))
}

/// The body of `POST /workspaces`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateWorkspace {
    /// The display name.
    #[garde(length(chars, min = 1, max = 100))]
    name: String,
    /// Lowercase letters, digits and hyphens, 2 to 63 characters; derived from the name when
    /// absent.
    #[garde(pattern(r"^[a-z0-9][a-z0-9-]{1,62}$"))]
    slug: Option<String>,
    /// `live` (default) or `test`: a test workspace never reaches a provider.
    #[garde(skip)]
    mode: Option<KeyMode>,
    /// The IANA time zone (`UTC` by default).
    #[garde(length(min = 1, max = 64))]
    timezone: Option<String>,
}

fn check_timezone(timezone: &str) -> Result<(), Problem> {
    jiff::tz::TimeZone::get(timezone).map(|_| ()).map_err(|_| {
        Problem::invalid_field(
            "/timezone",
            "format",
            "Not an IANA time zone (`Europe/Madrid`).",
        )
    })
}

/// Create a workspace.
///
/// The signed-in user becomes its owner; a workspace token for it can be minted at once.
#[utoipa::path(
    post,
    path = "/workspaces",
    tag = "Dashboard",
    operation_id = "workspaces.create",
    request_body(content = CreateWorkspace, example = json!({"name": "Acme Growth", "timezone": "Europe/Madrid"})),
    responses(
        (status = 201, description = "The workspace.", body = WorkspaceObject,
         headers(("ETag" = String, description = "The workspace's version."))),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, or a bearer credential (`session_required`)."),
        (status = 409, description = "The slug is taken (`conflict`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("session" = []))
)]
async fn create_workspace(
    State(app): State<AppState>,
    signed_in: SignedIn,
    client: crate::http::ratelimit::ClientAddress,
    Json(body): Json<CreateWorkspace>,
) -> ApiResult<(StatusCode, Tagged<WorkspaceObject>)> {
    let timezone = body.timezone.as_deref().unwrap_or("UTC");
    check_timezone(timezone)?;
    let mut tx = app.db.begin_as_user(signed_in.user).await?;
    let workspace = workspaces::create_for(
        &mut tx,
        signed_in.user,
        &NewWorkspace {
            name: &body.name,
            slug: body.slug.as_deref(),
            mode: body.mode.unwrap_or(KeyMode::Live),
            timezone,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        WorkspaceId::trusted(workspace.id.uuid()),
        AuditActor::User(signed_in.user),
        Action::WorkspaceCreated,
        Some(workspace.id.to_string()),
        json!({ "slug": workspace.slug }),
        Some(&ip_hash(&app.keys, client)),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: workspace.version,
            body: workspace,
        },
    ))
}

/// The body of `PATCH /workspaces/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateWorkspace {
    /// A new display name.
    #[garde(length(chars, min = 1, max = 100))]
    name: Option<String>,
    /// A new IANA time zone.
    #[garde(length(min = 1, max = 64))]
    timezone: Option<String>,
    /// Settings to set: each top-level key given replaces the stored one. `ai` holds the AI
    /// switches and budget (`classify_replies`, `generate_snippets`, `monthly_budget_usd`,
    /// `confidence_threshold`, `review_sample`, `usable_fields`); an unknown or invalid field is
    /// refused.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    settings: Option<Map<String, Value>>,
    /// `live` or `test`. Switching retires every API key of the other mode at once.
    #[garde(skip)]
    mode: Option<KeyMode>,
}

/// Update the workspace.
///
/// Its name, time zone, settings or mode.
#[utoipa::path(
    patch,
    path = "/workspaces/{id}",
    tag = "Dashboard",
    operation_id = "workspaces.update",
    params(("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."), IfMatch),
    request_body(content = UpdateWorkspace, example = json!({"name": "Acme", "settings": {"ai": {"classify_replies": true, "monthly_budget_usd": 25}}})),
    responses(
        (status = 200, description = "The workspace.", body = WorkspaceObject,
         headers(("ETag" = String, description = "The workspace's new version."))),
        (status = 400, description = "`If-Match` is malformed."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 412, description = "`If-Match` names a version that is no longer current; nothing changed."),
        (status = 422, description = "The body is invalid: an unknown time zone, or an invalid `settings.ai` or `settings.delivery` field (pointed at under `/settings/ai` or `/settings/delivery`)."),
    ),
    security(("bearer" = []))
)]
async fn update_workspace(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path(id): Path<Id<Workspace>>,
    if_match: IfMatch,
    Json(body): Json<UpdateWorkspace>,
) -> ApiResult<Tagged<WorkspaceObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    if let Some(timezone) = &body.timezone {
        check_timezone(timezone)?;
    }
    if let Some(settings) = &body.settings {
        crate::ai::check_settings(settings)?;
        crate::delivery::evidence::check_settings(settings)?;
    }
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let workspace = workspaces::update(
        &mut tx,
        principal.workspace,
        &workspaces::Changes {
            name: body.name.as_deref(),
            timezone: body.timezone.as_deref(),
            settings: body.settings.as_ref(),
            mode: body.mode,
        },
        &if_match,
    )
    .await?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::WorkspaceUpdated,
        Some(workspace.id.to_string()),
        json!({
            "name": body.name,
            "timezone": body.timezone,
            "settings": body.settings.as_ref().map(|settings| settings.keys().cloned().collect::<Vec<_>>()),
            "mode": body.mode,
        }),
        Some(&ip_hash(&app.keys, client)),
    )
    .await?;
    tx.commit().await?;
    if body.mode.is_some() {
        app.authority.forget_workspace(principal.workspace);
    }
    Ok(Tagged {
        version: workspace.version,
        body: workspace,
    })
}

/// Delete the workspace.
///
/// Owners only. Every credential of the workspace stops working within a minute and its data is
/// erased after a 30-day tombstone.
#[utoipa::path(
    delete,
    path = "/workspaces/{id}",
    tag = "Dashboard",
    operation_id = "workspaces.delete",
    params(("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`).")),
    responses(
        (status = 200, description = "The workspace, with `deletion_requested_at`.", body = WorkspaceObject,
         headers(("ETag" = String, description = "The workspace's version."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or not an owner."),
        (status = 404, description = "No such workspace for this credential."),
    ),
    security(("bearer" = []))
)]
async fn delete_workspace(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path(id): Path<Id<Workspace>>,
) -> ApiResult<Tagged<WorkspaceObject>> {
    dashboard(&principal, id)?;
    principal.require_owner()?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let workspace =
        workspaces::request_deletion(&mut tx, principal.workspace, &principal.actor.id())
            .await?
            .ok_or_else(|| Problem::not_found("workspace"))?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::WorkspaceDeletionRequested,
        Some(workspace.id.to_string()),
        json!({}),
        Some(&ip_hash(&app.keys, client)),
    )
    .await?;
    tx.commit().await?;
    app.authority.forget_workspace(principal.workspace);
    Ok(Tagged {
        version: workspace.version,
        body: workspace,
    })
}

// ───────────────────────────── members ─────────────────────────────

/// The filters of `GET /workspaces/{id}/members`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct MemberQuery {
    status: Option<MembershipStatus>,
}

/// List the workspace's members.
///
/// With their role and status; `status` filters.
#[utoipa::path(
    get,
    path = "/workspaces/{id}/members",
    tag = "Dashboard",
    operation_id = "members.list",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("limit" = Option<i64>, Query, description = "Members per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<MembershipStatus>, Query, description = "Only members of this status."),
    ),
    responses(
        (status = 200, description = "A page of members.", body = Page<MemberObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:read`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_members(
    State(app): State<AppState>,
    principal: Principal,
    Path(id): Path<Id<Workspace>>,
    Query(list): Query<ListQuery>,
    Query(query): Query<MemberQuery>,
) -> ApiResult<Json<Page<MemberObject>>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceRead)?;
    let params = PageParams::from_query(
        &app.keys,
        principal.workspace,
        "members",
        "id",
        &query,
        &list,
    )?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let rows = memberships::list(
        &mut tx,
        principal.workspace,
        query.status,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let counted = match params.include_total {
        true => Some(
            memberships::count(
                &mut tx,
                principal.workspace,
                query.status,
                pagination::COUNT_CAP,
            )
            .await?,
        ),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&app.keys, &params, rows, |member| {
        pagination::by_id(member.id.uuid())
    });
    Ok(Json(with_total(page, counted)))
}

/// The body of `PATCH /workspaces/{id}/members/{member_id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateMember {
    /// A new role; `owner` makes the member an owner (owners only).
    #[garde(skip)]
    role: Option<MembershipRole>,
    /// `suspended` pauses the member and revokes their API keys; `active` lets them act again.
    #[garde(skip)]
    status: Option<MemberStatusChange>,
}

/// A status a member can be given by an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum MemberStatusChange {
    Active,
    Suspended,
}

/// Update a member.
///
/// Their role (making a member `owner` transfers ownership, owners only; the last owner cannot be
/// demoted) or their status (suspending revokes their API keys).
#[utoipa::path(
    patch,
    path = "/workspaces/{id}/members/{member_id}",
    tag = "Dashboard",
    operation_id = "members.update",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("member_id" = Id<Membership>, Path, description = "The member id (`mem_…`)."),
        IfMatch,
    ),
    request_body(content = UpdateMember, example = json!({"role": "admin"})),
    responses(
        (status = 200, description = "The member.", body = MemberObject,
         headers(("ETag" = String, description = "The member's new version."))),
        (status = 400, description = "`If-Match` is malformed."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), it lacks `workspace:manage`, or only an owner may do this."),
        (status = 404, description = "No such member in this workspace."),
        (status = 409, description = "The last owner cannot lose ownership, or the member was removed (`invalid_state`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current; nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_member(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, member)): Path<(Id<Workspace>, Id<Membership>)>,
    if_match: IfMatch,
    Json(body): Json<UpdateMember>,
) -> ApiResult<Tagged<MemberObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let status = body.status.map(|status| match status {
        MemberStatusChange::Active => MembershipStatus::Active,
        MemberStatusChange::Suspended => MembershipStatus::Suspended,
    });
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let before = memberships::read(&mut tx, principal.workspace, member).await?;
    let changed = memberships::change(
        &mut tx,
        principal.workspace,
        member,
        principal.role,
        Change {
            role: body.role,
            status,
        },
        &if_match,
    )
    .await?;
    let ip_hash = ip_hash(&app.keys, client);
    if let Some(before) = &before {
        let after = &changed.member;
        let mut actions = Vec::new();
        if before.role != after.role {
            actions.push(if after.role == MembershipRole::Owner {
                Action::OwnershipTransferred
            } else {
                Action::MemberRoleChanged
            });
        }
        if before.status != after.status {
            actions.push(match after.status {
                MembershipStatus::Suspended => Action::MemberSuspended,
                MembershipStatus::Active => Action::MemberReactivated,
                MembershipStatus::Removed => Action::MemberRemoved,
            });
        }
        for action in actions {
            audit::record(
                &mut tx,
                principal.workspace,
                principal.actor.into(),
                action,
                Some(member.to_string()),
                json!({
                    "role": { "from": before.role, "to": after.role },
                    "status": { "from": before.status, "to": after.status },
                    "revoked_keys": changed.revoked_keys.len(),
                }),
                Some(&ip_hash),
            )
            .await?;
        }
    }
    tx.commit().await?;
    forget(&app, principal.workspace, &changed).await;
    Ok(Tagged {
        version: changed.member.version,
        body: changed.member,
    })
}

/// Drops what a member change invalidated from this process's authority.
async fn forget(app: &AppState, workspace: WorkspaceId, changed: &memberships::Changed) {
    for hash in &changed.revoked_keys {
        app.authority.forget(hash).await;
    }
    app.authority
        .forget_member(workspace, changed.member.user.id);
}

/// Remove a member.
///
/// A tombstone: the person loses access at once (their tokens within a minute), their API keys
/// are revoked, and only a new invitation brings them back. The last owner cannot be removed.
#[utoipa::path(
    delete,
    path = "/workspaces/{id}/members/{member_id}",
    tag = "Dashboard",
    operation_id = "members.delete",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("member_id" = Id<Membership>, Path, description = "The member id (`mem_…`)."),
    ),
    responses(
        (status = 200, description = "The removed member.", body = MemberObject,
         headers(("ETag" = String, description = "The member's version."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), it lacks `workspace:manage`, or only an owner may remove an owner."),
        (status = 404, description = "No such member in this workspace."),
        (status = 409, description = "The last owner cannot be removed (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn delete_member(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, member)): Path<(Id<Workspace>, Id<Membership>)>,
) -> ApiResult<Tagged<MemberObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let changed = memberships::remove(&mut tx, principal.workspace, member, principal.role).await?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::MemberRemoved,
        Some(member.to_string()),
        json!({ "revoked_keys": changed.revoked_keys.len() }),
        Some(&ip_hash(&app.keys, client)),
    )
    .await?;
    tx.commit().await?;
    forget(&app, principal.workspace, &changed).await;
    Ok(Tagged {
        version: changed.member.version,
        body: changed.member,
    })
}

// ───────────────────────────── invitations ─────────────────────────────

/// The filters of `GET /workspaces/{id}/invitations`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct InvitationQuery {
    status: Option<InvitationStatus>,
}

/// List the workspace's invitations.
///
/// Pending, accepted, revoked and expired; `status` filters.
#[utoipa::path(
    get,
    path = "/workspaces/{id}/invitations",
    tag = "Dashboard",
    operation_id = "invitations.list",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("limit" = Option<i64>, Query, description = "Invitations per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<InvitationStatus>, Query, description = "Only invitations of this status."),
    ),
    responses(
        (status = 200, description = "A page of invitations.", body = Page<InvitationObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_invitations(
    State(app): State<AppState>,
    principal: Principal,
    Path(id): Path<Id<Workspace>>,
    Query(list): Query<ListQuery>,
    Query(query): Query<InvitationQuery>,
) -> ApiResult<Json<Page<InvitationObject>>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let params = PageParams::from_query(
        &app.keys,
        principal.workspace,
        "invitations",
        "id",
        &query,
        &list,
    )?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let rows = invitations::list(
        &mut tx,
        principal.workspace,
        query.status,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let counted = match params.include_total {
        true => Some(
            invitations::count(
                &mut tx,
                principal.workspace,
                query.status,
                pagination::COUNT_CAP,
            )
            .await?,
        ),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&app.keys, &params, rows, |invitation| {
        pagination::by_id(invitation.id.uuid())
    });
    Ok(Json(with_total(page, counted)))
}

/// The role an invitation offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum InvitedRole {
    Admin,
    Member,
    Viewer,
}

impl From<InvitedRole> for MembershipRole {
    fn from(role: InvitedRole) -> Self {
        match role {
            InvitedRole::Admin => Self::Admin,
            InvitedRole::Member => Self::Member,
            InvitedRole::Viewer => Self::Viewer,
        }
    }
}

/// One address to invite.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct InviteInput {
    /// The address.
    #[garde(length(chars, min = 3, max = 254))]
    email: String,
    /// `admin`, `member` or `viewer` (ownership is given to a member afterwards).
    #[garde(skip)]
    role: InvitedRole,
}

/// The body of `POST /workspaces/{id}/invitations`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateInvitations {
    /// The addresses to invite, 1 to 50, each once.
    #[garde(length(min = 1, max = 50), dive)]
    invitations: Vec<InviteInput>,
}

/// The invitations a request sent.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct InvitationList {
    /// One per address, in the request's order.
    #[schema(max_items = 50)]
    pub data: Vec<InvitationObject>,
}

/// Invite people to the workspace.
///
/// One or more addresses, each with a role; each gets a mail with a link valid for 10 days. An
/// address with a pending invitation gets it again, extended.
#[utoipa::path(
    post,
    path = "/workspaces/{id}/invitations",
    tag = "Dashboard",
    operation_id = "invitations.create",
    params(("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`).")),
    request_body(content = CreateInvitations, example = json!({"invitations": [{"email": "grace@example.com", "role": "member"}, {"email": "linus@example.com", "role": "viewer"}]})),
    responses(
        (status = 201, description = "The invitations, sent.", body = InvitationList),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 409, description = "An address is already a member (`conflict`)."),
        (status = 422, description = "The body is invalid."),
        (status = 503, description = "Mail cannot be sent."),
    ),
    security(("bearer" = []))
)]
async fn create_invitations(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path(id): Path<Id<Workspace>>,
    Json(body): Json<CreateInvitations>,
) -> ApiResult<(StatusCode, Json<InvitationList>)> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let mut emails = Vec::with_capacity(body.invitations.len());
    for (index, input) in body.invitations.iter().enumerate() {
        let email = EmailAddress::parse(&input.email).map_err(|error| {
            Problem::invalid_field(
                &format!("/invitations/{index}/email"),
                "format",
                error.to_string(),
            )
        })?;
        if emails
            .iter()
            .any(|(other, _): &(EmailAddress, MembershipRole)| other.key() == email.key())
        {
            return Err(Problem::invalid_field(
                &format!("/invitations/{index}/email"),
                "duplicate",
                "Each address once.",
            ));
        }
        emails.push((email, MembershipRole::from(input.role)));
    }
    let dashboard_url = app
        .identity
        .settings
        .dashboard()
        .ok_or_else(|| Problem::internal(&"no dashboard origin is configured"))?
        .clone();
    let inviter = principal.actor.user();
    let ip_hash = ip_hash(&app.keys, client);
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let workspace = workspaces::read(&mut tx, principal.workspace)
        .await?
        .ok_or_else(|| Problem::not_found("workspace"))?;
    let inviter_name = sqlx::query!(
        "SELECT name, email FROM users WHERE id = $1",
        inviter.uuid()
    )
    .fetch_optional(&mut *tx)
    .await?
    .map(|row| row.name.unwrap_or(row.email));
    let mut sent = Vec::with_capacity(emails.len());
    for (email, role) in &emails {
        let invitation = invitations::send(
            &mut tx,
            &app.keys,
            principal.workspace,
            email,
            *role,
            inviter,
        )
        .await?;
        accept::transactional(
            &mut tx,
            &app.keys,
            &Transactional::Invitation {
                to: email,
                workspace_name: &workspace.name,
                inviter: inviter_name.as_deref(),
                role: role.as_str(),
                link: &invitations::link(&dashboard_url, &invitation.token),
                expires_at: invitation.invitation.expires_at,
            },
        )
        .await
        .map_err(|error| Problem::from(crate::identity::codes::CodeError::Mail(error)))?;
        audit::record(
            &mut tx,
            principal.workspace,
            principal.actor.into(),
            Action::InvitationSent,
            Some(invitation.invitation.id.to_string()),
            json!({ "email": email.as_str(), "role": role.as_str() }),
            Some(&ip_hash),
        )
        .await?;
        sent.push(invitation.invitation);
    }
    tx.commit().await?;
    accept::wake(&app.db).await;
    Ok((StatusCode::CREATED, Json(InvitationList { data: sent })))
}

/// Revoke a pending invitation.
///
/// Its link stops working.
#[utoipa::path(
    delete,
    path = "/workspaces/{id}/invitations/{invitation_id}",
    tag = "Dashboard",
    operation_id = "invitations.delete",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("invitation_id" = Id<Invitation>, Path, description = "The invitation id (`inv_…`)."),
    ),
    responses(
        (status = 204, description = "The invitation is revoked."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such invitation in this workspace."),
        (status = 409, description = "The invitation is no longer pending (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn delete_invitation(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, invitation)): Path<(Id<Workspace>, Id<Invitation>)>,
) -> ApiResult<StatusCode> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    if !invitations::revoke(&mut tx, principal.workspace, invitation).await? {
        return Err(
            if invitations::exists(&mut tx, principal.workspace, invitation).await? {
                Problem::invalid_state("The invitation is no longer pending.")
            } else {
                Problem::not_found("invitation")
            },
        );
    }
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::InvitationRevoked,
        Some(invitation.to_string()),
        json!({}),
        Some(&ip_hash(&app.keys, client)),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── API keys ─────────────────────────────

/// The filters of `GET /workspaces/{id}/api_keys`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct KeyQuery {
    status: Option<KeyStatus>,
}

/// List the workspace's API keys.
///
/// Without their secrets; `status` filters.
#[utoipa::path(
    get,
    path = "/workspaces/{id}/api_keys",
    tag = "Dashboard",
    operation_id = "api_keys.list",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("limit" = Option<i64>, Query, description = "Keys per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<KeyStatus>, Query, description = "Only keys of this status."),
    ),
    responses(
        (status = 200, description = "A page of keys.", body = Page<ApiKeyObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_api_keys(
    State(app): State<AppState>,
    principal: Principal,
    Path(id): Path<Id<Workspace>>,
    Query(list): Query<ListQuery>,
    Query(query): Query<KeyQuery>,
) -> ApiResult<Json<Page<ApiKeyObject>>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let params = PageParams::from_query(
        &app.keys,
        principal.workspace,
        "api_keys",
        "id",
        &query,
        &list,
    )?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let rows = api_keys::list(
        &mut tx,
        principal.workspace,
        query.status,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let counted = match params.include_total {
        true => Some(
            api_keys::count(
                &mut tx,
                principal.workspace,
                query.status,
                pagination::COUNT_CAP,
            )
            .await?,
        ),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&app.keys, &params, rows, |key| {
        pagination::by_id(key.id.uuid())
    });
    Ok(Json(with_total(page, counted)))
}

/// The body of `POST /workspaces/{id}/api_keys`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateApiKey {
    /// A name for people.
    #[garde(length(chars, min = 1, max = 100))]
    name: String,
    /// The scopes, within the creator's role; the role's scopes by default.
    #[garde(length(min = 1, max = 32))]
    scopes: Option<Vec<String>>,
    /// When it stops working; never by default.
    #[garde(skip)]
    expires_at: Option<Timestamp>,
}

/// Create an API key.
///
/// Delegated by the member who creates it: its scopes are within theirs, it acts with their
/// current role, and it dies with their membership. The secret is in this answer only.
#[utoipa::path(
    post,
    path = "/workspaces/{id}/api_keys",
    tag = "Dashboard",
    operation_id = "api_keys.create",
    params(("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`).")),
    request_body(content = CreateApiKey, example = json!({"name": "CRM sync", "scopes": ["people:read", "people:write"]})),
    responses(
        (status = 201, description = "The key, with its secret shown once.", body = ApiKeyObject,
         headers(("ETag" = String, description = "The key's version."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), it lacks `workspace:manage`, or it was minted from an operator's impersonation session."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 422, description = "The body is invalid, or a scope is beyond the creator's role."),
    ),
    security(("bearer" = []))
)]
async fn create_api_key(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path(id): Path<Id<Workspace>>,
    Json(body): Json<CreateApiKey>,
) -> ApiResult<(StatusCode, Tagged<ApiKeyObject>)> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    if body
        .expires_at
        .is_some_and(|expires_at| expires_at <= crate::process::now())
    {
        return Err(Problem::invalid_field(
            "/expires_at",
            "range",
            "A key expires in the future.",
        ));
    }
    // A key outlives the session whose token creates it: an operator's impersonation makes none
    // (`domain::identity::may_make`), read from the session row rather than trusted from a cache.
    if let Actor::User { session, .. } = principal.actor {
        let method = crate::identity::sessions::by_id(&app.db, session)
            .await?
            .map(|row| row.method);
        if method.is_some_and(|method| !may_make(method, Lasting::ApiKey)) {
            return Err(Problem::forbidden(
                "An impersonation session cannot create API keys in the person's name.",
            ));
        }
    }
    let creator = principal.actor.user();
    // A sensitive write reads the creator's role again rather than trusting a cache.
    let standing = read_standing(&app.db, principal.workspace, creator)
        .await?
        .filter(|standing| standing.status == MembershipStatus::Active)
        .ok_or_else(Problem::unauthorized)?;
    // A key is within its creator's role and within the creating credential's own scopes, so a
    // token that may only repair the workspace (break-glass) cannot mint a key that reaches more.
    let allowed = standing.role.scopes().intersect(principal.scopes);
    let scopes = match &body.scopes {
        None => allowed,
        Some(names) => {
            let scopes = ScopeSet::parse(names.iter().map(String::as_str)).map_err(|name| {
                Problem::invalid_field("/scopes", "invalid", format!("`{name}` is not a scope."))
            })?;
            if scopes.intersect(allowed) != scopes {
                return Err(Problem::invalid_field(
                    "/scopes",
                    "range",
                    "A key's scopes are within its creator's role and the credential creating it.",
                ));
            }
            scopes
        }
    };
    let mode = if principal.test_mode {
        KeyMode::Test
    } else {
        KeyMode::Live
    };
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let (key, secret) = api_keys::create(
        &mut tx,
        principal.workspace,
        creator,
        &body.name,
        scopes,
        mode,
        body.expires_at,
    )
    .await
    .map_err(|error| match error {
        api_keys::CreateError::Db(error) => Problem::from(error),
        api_keys::CreateError::Crypto(error) => Problem::from(error),
    })?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::ApiKeyCreated,
        Some(key.to_string()),
        json!({ "name": body.name, "scopes": scopes.to_strings() }),
        Some(&ip_hash(&app.keys, client)),
    )
    .await?;
    let mut object = api_keys::read(&mut tx, principal.workspace, key)
        .await?
        .ok_or_else(|| Problem::not_found("API key"))?;
    tx.commit().await?;
    object.secret = Some(secret.secret);
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: object.version,
            body: object,
        },
    ))
}

/// The body of `PATCH /workspaces/{id}/api_keys/{key_id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateApiKey {
    /// A new name.
    #[garde(length(chars, min = 1, max = 100))]
    name: Option<String>,
}

/// Rename an API key.
///
/// Its name only; its scopes and secret never change.
#[utoipa::path(
    patch,
    path = "/workspaces/{id}/api_keys/{key_id}",
    tag = "Dashboard",
    operation_id = "api_keys.update",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("key_id" = Id<ApiKey>, Path, description = "The key id (`key_…`)."),
        IfMatch,
    ),
    request_body(content = UpdateApiKey, example = json!({"name": "CRM sync (prod)"})),
    responses(
        (status = 200, description = "The key.", body = ApiKeyObject,
         headers(("ETag" = String, description = "The key's new version."))),
        (status = 400, description = "`If-Match` is malformed."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such key in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current; nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_api_key(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, key)): Path<(Id<Workspace>, Id<ApiKey>)>,
    if_match: IfMatch,
    Json(body): Json<UpdateApiKey>,
) -> ApiResult<Tagged<ApiKeyObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let current = api_keys::lock_version(&mut tx, principal.workspace, key)
        .await?
        .ok_or_else(|| Problem::not_found("API key"))?;
    if_match.check(current)?;
    if let Some(name) = &body.name {
        api_keys::rename(&mut tx, principal.workspace, key, name).await?;
        audit::record(
            &mut tx,
            principal.workspace,
            principal.actor.into(),
            Action::ApiKeyUpdated,
            Some(key.to_string()),
            json!({ "name": name }),
            Some(&ip_hash(&app.keys, client)),
        )
        .await?;
    }
    let object = api_keys::read(&mut tx, principal.workspace, key)
        .await?
        .ok_or_else(|| Problem::not_found("API key"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: object.version,
        body: object,
    })
}

/// Revoke an API key.
///
/// It stops working within a minute on every replica (at once on this one); the row stays, as
/// `revoked`, for the history it signed.
#[utoipa::path(
    delete,
    path = "/workspaces/{id}/api_keys/{key_id}",
    tag = "Dashboard",
    operation_id = "api_keys.delete",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("key_id" = Id<ApiKey>, Path, description = "The key id (`key_…`)."),
    ),
    responses(
        (status = 200, description = "The revoked key.", body = ApiKeyObject,
         headers(("ETag" = String, description = "The key's version."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such key in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_api_key(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, key)): Path<(Id<Workspace>, Id<ApiKey>)>,
) -> ApiResult<Tagged<ApiKeyObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let revoked = api_keys::revoke(&mut tx, principal.workspace, key).await?;
    if revoked.is_some() {
        audit::record(
            &mut tx,
            principal.workspace,
            principal.actor.into(),
            Action::ApiKeyRevoked,
            Some(key.to_string()),
            json!({}),
            Some(&ip_hash(&app.keys, client)),
        )
        .await?;
    }
    let object = api_keys::read(&mut tx, principal.workspace, key)
        .await?
        .ok_or_else(|| Problem::not_found("API key"))?;
    tx.commit().await?;
    if let Some(hash) = revoked {
        app.authority.forget(&hash).await;
    }
    Ok(Tagged {
        version: object.version,
        body: object,
    })
}

// ───────────────────────────── audit log ─────────────────────────────

/// The filters of `GET /workspaces/{id}/audit_log`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct AuditQuery {
    action: Option<String>,
}

/// List the workspace's audit log.
///
/// Every change to its access (members, invitations, keys, SSO, grants, the workspace) of the
/// last 180 days, newest first; `action` filters.
#[utoipa::path(
    get,
    path = "/workspaces/{id}/audit_log",
    tag = "Dashboard",
    operation_id = "audit_log.list",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("limit" = Option<i64>, Query, description = "Entries per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("action" = Option<String>, Query, description = "Only entries of this action (`member.role_changed`)."),
    ),
    responses(
        (status = 200, description = "A page of entries.", body = Page<AuditObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_audit_log(
    State(app): State<AppState>,
    principal: Principal,
    Path(id): Path<Id<Workspace>>,
    Query(list): Query<ListQuery>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Page<AuditObject>>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let params = PageParams::from_query(
        &app.keys,
        principal.workspace,
        "audit_log",
        "id",
        &query,
        &list,
    )?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let rows = audit::list(
        &mut tx,
        principal.workspace,
        query.action.as_deref(),
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let counted = match params.include_total {
        true => Some(
            audit::count(
                &mut tx,
                principal.workspace,
                query.action.as_deref(),
                pagination::COUNT_CAP,
            )
            .await?,
        ),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&app.keys, &params, rows, |entry| {
        pagination::by_id(entry.id.uuid())
    });
    Ok(Json(with_total(page, counted)))
}
