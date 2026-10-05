//! `sso_connections`: a workspace's identity providers. Every write runs the connection's checks
//! (discovery, and each domain's DNS proof) after its transaction commits, and `verify` runs them
//! on request; the daily check is `identity::sso`'s job. All need `workspace:manage`; turning
//! enforcement on or off, giving `admin` as the default role, and deleting an enforcing connection
//! are an owner's.

use axum::extract::State;
use axum::http::StatusCode;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::{dashboard, team};
use crate::domain::identity::AuthMethod;
use crate::domain::ids::{Id, SsoConnection, Workspace, WorkspaceId};
use crate::domain::scope::{MembershipRole, Scope};
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::audit::{self, Action};
use crate::identity::authority::Principal;
use crate::identity::sessions;
use crate::identity::sso::{self, SsoConnectionObject};
use crate::pagination::{self, Include, ListQuery, Order, Page, PageParams};
use crate::problem::{ApiResult, Problem};

/// The routes of `sso_connections`.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_connections, create_connection))
        .routes(routes!(update_connection, delete_connection))
        .routes(routes!(verify_connection))
}

/// The default role of members a connection provisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum DefaultRole {
    Admin,
    Member,
    Viewer,
}

impl From<DefaultRole> for MembershipRole {
    fn from(role: DefaultRole) -> Self {
        match role {
            DefaultRole::Admin => Self::Admin,
            DefaultRole::Member => Self::Member,
            DefaultRole::Viewer => Self::Viewer,
        }
    }
}

/// An issuer as a connection takes it: an absolute `https` (or, for a development deployment,
/// `http`) URL without a query or a fragment.
fn check_issuer(issuer: &str) -> Result<(), Problem> {
    let valid = url::Url::parse(issuer).is_ok_and(|url| {
        matches!(url.scheme(), "https" | "http")
            && url.host().is_some()
            && url.query().is_none()
            && url.fragment().is_none()
    });
    if valid {
        Ok(())
    } else {
        Err(Problem::invalid_field(
            "/issuer",
            "format",
            "The issuer is an `https` URL without a query or a fragment.",
        ))
    }
}

/// The normalised, de-duplicated domains of a request.
fn domains(input: &[String]) -> Result<Vec<String>, Problem> {
    let mut out: Vec<String> = Vec::with_capacity(input.len());
    for (index, domain) in input.iter().enumerate() {
        let domain = sso::normalise_domain(domain).ok_or_else(|| {
            Problem::invalid_field(
                &format!("/domains/{index}"),
                "format",
                "Not an email domain (`example.com`).",
            )
        })?;
        if !out.contains(&domain) {
            out.push(domain);
        }
    }
    Ok(out)
}

/// Runs a connection's checks after its transaction committed, and records them.
async fn check_and_record(
    app: &AppState,
    workspace: WorkspaceId,
    id: Id<SsoConnection>,
) -> ApiResult<()> {
    let mut tx = app.db.begin_in(workspace).await?;
    let targets = sso::check_targets(&mut tx, workspace, id.uuid()).await?;
    tx.commit().await?;
    let Some((issuer, domains)) = targets else {
        return Ok(());
    };
    let report = sso::check(&app.identity.fetcher, &app.resolver, &issuer, &domains).await;
    let mut tx = app.db.begin_in(workspace).await?;
    sso::record(&mut tx, workspace, id.uuid(), &report).await?;
    tx.commit().await?;
    Ok(())
}

/// The connection as it is now, with its version.
async fn tagged(
    app: &AppState,
    workspace: WorkspaceId,
    id: Id<SsoConnection>,
) -> ApiResult<Tagged<SsoConnectionObject>> {
    let mut tx = app.db.begin_in(workspace).await?;
    let connection = sso::read(&mut tx, workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("SSO connection"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: connection.version,
        body: connection,
    })
}

/// List the workspace's SSO connections.
///
/// With their domains and the records that prove them.
#[utoipa::path(
    get,
    path = "/workspaces/{id}/sso_connections",
    tag = "Dashboard",
    operation_id = "sso_connections.list",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("limit" = Option<i64>, Query, description = "Connections per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
    ),
    responses(
        (status = 200, description = "A page of connections.", body = Page<SsoConnectionObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_connections(
    State(app): State<AppState>,
    principal: Principal,
    Path(id): Path<Id<Workspace>>,
    Query(list): Query<ListQuery>,
) -> ApiResult<Json<Page<SsoConnectionObject>>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let params = PageParams::from_query(
        &app.keys,
        principal.workspace,
        "sso_connections",
        "id",
        &(),
        &list,
    )?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let rows = sso::list(
        &mut tx,
        principal.workspace,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let counted = match params.include_total {
        true => Some(sso::count(&mut tx, principal.workspace, pagination::COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&app.keys, &params, rows, |connection| {
        pagination::by_id(connection.id.uuid())
    });
    Ok(Json(team::with_total(page, counted)))
}

/// The body of `POST /workspaces/{id}/sso_connections`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateSsoConnection {
    /// A name for people.
    #[garde(length(chars, min = 1, max = 100))]
    name: String,
    /// The OpenID Connect issuer (its discovery document is at
    /// `{issuer}/.well-known/openid-configuration`); for Microsoft, the tenant's issuer.
    #[garde(length(min = 8, max = 2048))]
    issuer: String,
    /// The client id registered at the provider.
    #[garde(length(min = 1, max = 512))]
    client_id: String,
    /// The client secret, for a confidential client; stored sealed, never shown again.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    client_secret: Option<SecretString>,
    /// `member` (default) or `viewer`; `admin` only when an owner sets it.
    #[garde(skip)]
    default_role: Option<DefaultRole>,
    /// Create memberships for people of the proved domains at their first sign-in (default true).
    #[garde(skip)]
    jit_provisioning: Option<bool>,
    /// The email domains the provider is authoritative for, at most 20.
    #[garde(length(max = 20))]
    domains: Vec<String>,
}

/// Create an SSO connection.
///
/// Its discovery document and its domains' DNS records are checked at once; each domain routes
/// sign-ins once its TXT record is found. Enforcement is turned on afterwards, by an owner who has
/// signed in through the connection.
#[utoipa::path(
    post,
    path = "/workspaces/{id}/sso_connections",
    tag = "Dashboard",
    operation_id = "sso_connections.create",
    params(("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`).")),
    request_body(content = CreateSsoConnection, example = json!({"name": "Okta", "issuer": "https://acme.okta.com", "client_id": "0oa1b2c3", "client_secret": "s3cr3t", "domains": ["acme.com"]})),
    responses(
        (status = 201, description = "The connection, with what its checks found.", body = SsoConnectionObject,
         headers(("ETag" = String, description = "The connection's version."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), it lacks `workspace:manage`, or `admin` as default role is an owner's."),
        (status = 404, description = "No such workspace for this credential."),
        (status = 409, description = "A domain belongs to another connection of the workspace (`conflict`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_connection(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path(id): Path<Id<Workspace>>,
    Json(body): Json<CreateSsoConnection>,
) -> ApiResult<(StatusCode, Tagged<SsoConnectionObject>)> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    check_issuer(&body.issuer)?;
    let domains = domains(&body.domains)?;
    let default_role = body.default_role.unwrap_or(DefaultRole::Member);
    if default_role == DefaultRole::Admin {
        principal.require_owner()?;
    }
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let connection = sso::create(
        &mut tx,
        &app.keys,
        principal.workspace,
        &sso::NewConnection {
            name: &body.name,
            issuer: &body.issuer,
            client_id: &body.client_id,
            client_secret: body.client_secret.as_ref(),
            default_role: default_role.into(),
            jit_provisioning: body.jit_provisioning.unwrap_or(true),
            enforced: false,
            domains: &domains,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::SsoConnectionCreated,
        Some(connection.to_string()),
        json!({ "issuer": body.issuer, "domains": domains }),
        Some(&app.keys.hash_address(&client.as_key())),
    )
    .await?;
    tx.commit().await?;
    check_and_record(&app, principal.workspace, connection).await?;
    Ok((
        StatusCode::CREATED,
        tagged(&app, principal.workspace, connection).await?,
    ))
}

/// The body of `PATCH /workspaces/{id}/sso_connections/{sso_id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateSsoConnection {
    /// A new name.
    #[garde(length(chars, min = 1, max = 100))]
    name: Option<String>,
    /// A new issuer.
    #[garde(length(min = 8, max = 2048))]
    issuer: Option<String>,
    /// A new client id.
    #[garde(length(min = 1, max = 512))]
    client_id: Option<String>,
    /// A new client secret.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    client_secret: Option<SecretString>,
    /// A new default role (`admin` only by an owner).
    #[garde(skip)]
    default_role: Option<DefaultRole>,
    /// Provisioning on or off.
    #[garde(skip)]
    jit_provisioning: Option<bool>,
    /// Enforcement on or off (owners only; turning it on needs the owner's own session to be proven
    /// through this connection within the last 24 hours).
    #[garde(skip)]
    enforced: Option<bool>,
    /// The new list of domains, replacing the old one (at most 20).
    #[garde(length(max = 20))]
    domains: Option<Vec<String>>,
}

/// Update an SSO connection.
///
/// Any policy change (issuer, client, domains, provisioning, enforcement, default role) moves its
/// `policy_version`: sessions proven under an older version stop counting for its workspace, so
/// affected people sign in through the provider again.
#[utoipa::path(
    patch,
    path = "/workspaces/{id}/sso_connections/{sso_id}",
    tag = "Dashboard",
    operation_id = "sso_connections.update",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("sso_id" = Id<SsoConnection>, Path, description = "The connection id (`sso_…`)."),
        IfMatch,
    ),
    request_body(content = UpdateSsoConnection, example = json!({"enforced": true})),
    responses(
        (status = 200, description = "The connection.", body = SsoConnectionObject,
         headers(("ETag" = String, description = "The connection's new version."))),
        (status = 400, description = "`If-Match` is malformed."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), it lacks `workspace:manage`, or only an owner may do this."),
        (status = 404, description = "No such connection in this workspace."),
        (status = 409, description = "Enforcement needs the owner's session proven through the connection first (`invalid_state`), or a domain belongs to another connection (`conflict`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current; nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_connection(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, connection)): Path<(Id<Workspace>, Id<SsoConnection>)>,
    if_match: IfMatch,
    Json(body): Json<UpdateSsoConnection>,
) -> ApiResult<Tagged<SsoConnectionObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    if let Some(issuer) = &body.issuer {
        check_issuer(issuer)?;
    }
    let domains = body.domains.as_deref().map(domains).transpose()?;
    if body.enforced.is_some() || body.default_role == Some(DefaultRole::Admin) {
        principal.require_owner()?;
    }
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let locked = sso::lock(&mut tx, principal.workspace, connection)
        .await?
        .ok_or_else(|| Problem::not_found("SSO connection"))?;
    if_match.check(locked.version)?;
    if body.enforced == Some(true) && !locked.enforced {
        // An owner cannot lock themself out: their own session must already come through it.
        let proven = match principal.session() {
            Some(session) => sessions::by_id(&app.db, session).await?.is_some_and(|row| {
                let age = crate::process::now()
                    .0
                    .duration_since(row.authenticated_at.0);
                row.method == AuthMethod::Sso
                    && row.sso_connection == Some(connection.uuid())
                    && !age.is_negative()
                    && age.unsigned_abs() < crate::domain::identity::PROOF_MAX_AGE
            }),
            None => false,
        };
        if !proven {
            return Err(Problem::invalid_state(
                "Sign in through this connection (within the last 24 hours) before enforcing it.",
            ));
        }
    }
    let changes = sso::Changes {
        name: body.name.as_deref(),
        issuer: body.issuer.as_deref(),
        client_id: body.client_id.as_deref(),
        client_secret: body.client_secret.as_ref(),
        default_role: body.default_role.map(Into::into),
        jit_provisioning: body.jit_provisioning,
        enforced: body.enforced,
        domains: domains.as_deref(),
    };
    sso::update(
        &mut tx,
        &app.keys,
        principal.workspace,
        connection,
        &changes,
    )
    .await?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::SsoConnectionUpdated,
        Some(connection.to_string()),
        json!({
            "policy_changed": changes.changes_policy(),
            "issuer": body.issuer,
            "domains": domains,
            "jit_provisioning": body.jit_provisioning,
            "enforced": body.enforced,
            "default_role": body.default_role,
            "client_changed": body.client_id.is_some() || body.client_secret.is_some(),
        }),
        Some(&app.keys.hash_address(&client.as_key())),
    )
    .await?;
    tx.commit().await?;
    if body.issuer.is_some() || domains.is_some() {
        check_and_record(&app, principal.workspace, connection).await?;
    }
    if changes.changes_policy() {
        app.authority.forget_workspace(principal.workspace);
    }
    tagged(&app, principal.workspace, connection).await
}

/// Delete an SSO connection.
///
/// Its domains stop routing; an enforcing connection is an owner's to delete, and deleting it
/// ends the enforcement.
#[utoipa::path(
    delete,
    path = "/workspaces/{id}/sso_connections/{sso_id}",
    tag = "Dashboard",
    operation_id = "sso_connections.delete",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("sso_id" = Id<SsoConnection>, Path, description = "The connection id (`sso_…`)."),
    ),
    responses(
        (status = 204, description = "The connection is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), it lacks `workspace:manage`, or an enforcing connection is an owner's to delete."),
        (status = 404, description = "No such connection in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_connection(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, connection)): Path<(Id<Workspace>, Id<SsoConnection>)>,
) -> ApiResult<StatusCode> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    let locked = sso::lock(&mut tx, principal.workspace, connection)
        .await?
        .ok_or_else(|| Problem::not_found("SSO connection"))?;
    if locked.enforced {
        principal.require_owner()?;
    }
    sso::delete(&mut tx, principal.workspace, connection).await?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::SsoConnectionDeleted,
        Some(connection.to_string()),
        json!({}),
        Some(&app.keys.hash_address(&client.as_key())),
    )
    .await?;
    tx.commit().await?;
    app.authority.forget_workspace(principal.workspace);
    Ok(StatusCode::NO_CONTENT)
}

/// Check an SSO connection now.
///
/// Reads its discovery document and its domains' DNS records, and records what they say: the
/// connection's status and detail, and which domains route.
#[utoipa::path(
    post,
    path = "/workspaces/{id}/sso_connections/{sso_id}/verify",
    tag = "Dashboard",
    operation_id = "sso_connections.verify",
    params(
        ("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`)."),
        ("sso_id" = Id<SsoConnection>, Path, description = "The connection id (`sso_…`)."),
    ),
    responses(
        (status = 200, description = "The connection, with what its checks found.", body = SsoConnectionObject,
         headers(("ETag" = String, description = "The connection's version."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "Not a workspace token (`session_required`), or it lacks `workspace:manage`."),
        (status = 404, description = "No such connection in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn verify_connection(
    State(app): State<AppState>,
    principal: Principal,
    client: crate::http::ratelimit::ClientAddress,
    Path((id, connection)): Path<(Id<Workspace>, Id<SsoConnection>)>,
) -> ApiResult<Tagged<SsoConnectionObject>> {
    dashboard(&principal, id)?;
    principal.require(Scope::WorkspaceManage)?;
    tagged(&app, principal.workspace, connection).await?;
    check_and_record(&app, principal.workspace, connection).await?;
    let mut tx = app.db.begin_in(principal.workspace).await?;
    audit::record(
        &mut tx,
        principal.workspace,
        principal.actor.into(),
        Action::SsoConnectionVerified,
        Some(connection.to_string()),
        json!({}),
        Some(&app.keys.hash_address(&client.as_key())),
    )
    .await?;
    tx.commit().await?;
    tagged(&app, principal.workspace, connection).await
}
