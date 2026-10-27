//! Dashboard audience slices over recent sent messages. This never creates mail or enrollments.
//! A negative open signal excludes untracked messages and messages outside the online window.

use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::domain::ids::{Campaign, Id, Person, Workspace};
use crate::domain::scope::Scope;
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::identity::authority::Principal;
use crate::pagination::{ListQuery, Page, PageParams};
use crate::problem::{ApiResult, Problem};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum Engagement {
    #[default]
    All,
    Opened,
    NotOpened,
    Clicked,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Filters {
    #[serde(default)]
    engagement: Engagement,
}

/// A person with sent campaign messages in the last 30 days of online history.
#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
struct Recipient {
    person_id: Id<Person>,
    email: String,
    sent: i64,
    opened: i64,
    clicked: i64,
    opens_tracked: bool,
}

/// The dashboard's recent newsletter audience report.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list))
}

/// List recent sent recipients by recorded campaign engagement.
///
/// Only the last 30 days of online message history participate. Opens and clicks count
/// observations classified as human, not privacy proxies or scanners. Not-opened requires
/// open tracking on every included message; missing tracking is never proof of no open.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}/newsletters/{campaign_id}/recipients",
    tag = "Dashboard",
    operation_id = "newsletter_recipients.list",
    params(
        ("workspace_id" = Id<Workspace>, Path, description = "The current workspace."),
        ("campaign_id" = Id<Campaign>, Path, description = "The source campaign."),
        ("engagement" = Option<Engagement>, Query, description = "all, opened, not_opened or clicked."),
        ("limit" = Option<i64>, Query, description = "Recipients per page, 1 to 100."),
        ("cursor" = Option<String>, Query, description = "The previous page's next_cursor."),
    ),
    responses(
        (status = 200, description = "Recent recipients, with observed engagement.", body = Page<Recipient>),
        (status = 400, description = "Invalid cursor or unsupported list option."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "A workspace session and campaigns:read, people:read and messages:read are required."),
        (status = 404, description = "The workspace or campaign does not exist."),
        (status = 422, description = "A query parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list(
    principal: Principal,
    State(state): State<AppState>,
    Path((workspace_id, campaign_id)): Path<(Id<Workspace>, Id<Campaign>)>,
    Query(query): Query<ListQuery>,
    Query(filters): Query<Filters>,
) -> ApiResult<Json<Page<Recipient>>> {
    principal.require_session()?;
    principal.require(Scope::CampaignsRead)?;
    principal.require(Scope::PeopleRead)?;
    principal.require(Scope::MessagesRead)?;
    let workspace = principal.workspace;
    if workspace_id != workspace.id() {
        return Err(Problem::not_found("workspace"));
    }
    if query.include.is_some() {
        return Err(Problem::bad_request(
            "This report does not offer total_count.",
        ));
    }
    let params = PageParams::from_query(
        &state.keys,
        workspace,
        "newsletter_recipients",
        "id",
        &(campaign_id, &filters),
        &query,
    )?;
    let mut tx = state.db.begin_in(workspace).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM campaigns WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(workspace.uuid())
    .bind(campaign_id.uuid())
    .fetch_one(&mut *tx)
    .await?;
    if !exists {
        return Err(Problem::not_found("campaign"));
    }
    let kind = match filters.engagement {
        Engagement::All => "all",
        Engagement::Opened => "opened",
        Engagement::NotOpened => "not_opened",
        Engagement::Clicked => "clicked",
    };
    let rows = sqlx::query_as::<_, Recipient>(
        "SELECT p.id AS person_id, p.email, count(*) AS sent,
                count(*) FILTER (WHERE e.human_opens > 0) AS opened,
                count(*) FILTER (WHERE e.human_clicks > 0) AS clicked,
                bool_and(coalesce((m.tracking->>'opens')::boolean, false)) AS opens_tracked
           FROM people p
           JOIN messages m ON m.workspace_id = p.workspace_id AND m.person_id = p.id
           LEFT JOIN message_engagement e ON e.workspace_id = m.workspace_id AND e.message_id = m.id
          WHERE p.workspace_id = $1 AND m.campaign_id = $2 AND m.state = 'sent'
            AND m.deleted_at IS NULL
            AND m.id >= uuidv7_boundary(now() - interval '30 days')
            AND ($3::uuid IS NULL OR CASE WHEN $4 THEN p.id > $3 ELSE p.id < $3 END)
          GROUP BY p.id, p.email
         HAVING $5 = 'all'
             OR ($5 = 'opened' AND bool_or(coalesce(e.human_opens, 0) > 0))
             OR ($5 = 'clicked' AND bool_or(coalesce(e.human_clicks, 0) > 0))
             OR ($5 = 'not_opened' AND bool_and(coalesce((m.tracking->>'opens')::boolean, false))
                 AND bool_and(coalesce(e.opens, 0) = 0) AND bool_and(coalesce(e.clicks, 0) = 0))
          ORDER BY CASE WHEN $4 THEN p.id END, p.id DESC LIMIT $6",
    )
    .bind(workspace.uuid())
    .bind(campaign_id.uuid())
    .bind(params.after_id())
    .bind(params.ascending())
    .bind(kind)
    .bind(params.fetch())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        None,
        |row| row.person_id.uuid(),
    )))
}
