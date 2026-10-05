//! The `jobs` resource: retrieve and cancel.
//!
//! A job's id reaches a client through the `202 Accepted` responses of operations whose work
//! outlives a request (their `Location` points here), so the resource has no list and no
//! create. A job shows its kind, its state, its bounded progress and result, its failed
//! attempts and its last error; never its payload, which may hold internal inputs.
//!
//! Cancelling is a request: it sets `cancel_requested_at`. A job that is waiting (`available`)
//! is cancelled at once. A running job observes the request at its next chunk boundary,
//! records the outcome of an external effect already under way, and ends `cancelled`. A job
//! that already ended cannot be cancelled (`409 invalid_state`).

use axum::extract::State;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::JobId;
use crate::db::{Database, Tx};
use crate::domain::ids::{self, Id, WorkspaceId};
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Path};
use crate::identity::authority::Principal;
use crate::problem::{ApiResult, Problem};

/// Where a job is (`jobs.state`): `available` while it waits for its run, `running`, then
/// `completed`, `failed` or `cancelled`; `needs_review` when an operator decides whether an
/// ambiguous external effect happened.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum JobState {
    Available,
    Running,
    Completed,
    Failed,
    Cancelled,
    NeedsReview,
}

/// A job as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct JobObject {
    #[schema(value_type = Id<ids::Job>)]
    pub id: JobId,
    /// The kind of work, such as `enrollment.add`. New kinds may be added.
    pub kind: String,
    /// Where the job is. New values may be added.
    #[schema(value_type = JobState)]
    pub state: String,
    /// What the job has done so far, as its last checkpoint recorded it.
    pub progress: Option<serde_json::Value>,
    /// The job's bounded result, once it completed (at most 64 KiB).
    pub result: Option<serde_json::Value>,
    /// Why the last failed run failed, or why the job ended; null after a success.
    pub last_error: Option<LastError>,
    /// Failed runs so far; a yield is not a failure.
    pub attempts: i16,
    /// When cancellation was requested.
    pub cancel_requested_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// When the job reached `completed`, `failed` or `cancelled`.
    pub finished_at: Option<Timestamp>,
}

/// The last error of a deferred resource. Resources that keep it in a row of their own (imports,
/// exports) store this same shape as JSON.
#[derive(Debug, Clone, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct LastError {
    /// A short machine-readable code (`error`, `lease_expired`, `unknown_kind`, `cancelled`, …).
    pub code: String,
    /// A human-readable explanation.
    pub detail: String,
    /// When the job's row last changed: the moment of the error for a job that ended, and at
    /// or after it while the job is still waiting or running (the row keeps no separate time
    /// for its error).
    pub at: Timestamp,
}

/// The routes of this resource.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(retrieve))
        .routes(routes!(cancel))
}

/// Retrieve a job of the credential's workspace.
#[utoipa::path(
    get,
    path = "/jobs/{id}",
    tag = "Automation",
    operation_id = "jobs.retrieve",
    params(("id" = Id<ids::Job>, Path, description = "The job id (`job_…`).")),
    responses(
        (status = 200, description = "The job.", body = JobObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:read`."),
        (status = 404, description = "No such job in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<JobId>,
) -> ApiResult<Json<JobObject>> {
    principal.require(Scope::AutomationRead)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    let job = read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("job"))?;
    tx.commit().await?;
    Ok(Json(job))
}

/// Request the cancellation of a job.
///
/// A waiting job is cancelled at once, a running one at its next chunk boundary.
#[utoipa::path(
    post,
    path = "/jobs/{id}/cancel",
    tag = "Automation",
    operation_id = "jobs.cancel",
    params(("id" = Id<ids::Job>, Path, description = "The job id (`job_…`).")),
    responses(
        (status = 200, description = "The job, cancelled or with its cancellation requested.", body = JobObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `automation:manage`."),
        (status = 404, description = "No such job in this workspace."),
        (status = 409, description = "The job already ended (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn cancel(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<JobId>,
) -> ApiResult<Json<JobObject>> {
    principal.require(Scope::AutomationManage)?;
    let mut tx = db.begin_in(principal.workspace).await?;
    // The row lock serialises with a claim: a claim that leased the job first makes this a
    // request the running job observes; otherwise the waiting job is cancelled here.
    let requested = sqlx::query_scalar!(
        "UPDATE jobs SET cancel_requested_at = coalesce(cancel_requested_at, now()),
                state = CASE WHEN state = 'available' THEN 'cancelled' ELSE state END,
                finished_at = CASE WHEN state = 'available' THEN now() ELSE finished_at END,
                last_error = CASE WHEN state = 'available' THEN 'cancelled: cancellation was requested' ELSE last_error END
          WHERE workspace_id = $1 AND id = $2 AND state IN ('available', 'running')
         RETURNING id",
        principal.workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let job = read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("job"))?;
    tx.commit().await?;
    if requested.is_none() {
        return Err(Problem::invalid_state(format!(
            "A job in state `{}` cannot be cancelled.",
            job.state
        )));
    }
    Ok(Json(job))
}

/// Reads one job of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: JobId,
) -> Result<Option<JobObject>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id AS "id: JobId", kind, state, progress, result, last_error, attempts,
                  cancel_requested_at AS "cancel_requested_at: Timestamp", created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp", finished_at AS "finished_at: Timestamp"
             FROM jobs WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| JobObject {
        id: row.id,
        kind: row.kind,
        state: row.state,
        progress: row.progress,
        result: row.result,
        last_error: row.last_error.map(|text| {
            // The runner writes `code: detail`.
            let (code, detail) = text.split_once(": ").unwrap_or(("error", text.as_str()));
            LastError {
                code: code.to_owned(),
                detail: detail.to_owned(),
                at: row.updated_at,
            }
        }),
        attempts: row.attempts,
        cancel_requested_at: row.cancel_requested_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
        finished_at: row.finished_at,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::domain::ids::Id;
    use crate::jobs::{self, JobId, Queue, lanes};
    use crate::testing::{TestDb, TestWorkspace};
    use crate::webhooks::deliver::Deliver;

    /// Enqueues a job of `workspace` as a business transaction would.
    async fn waiting_job(test: &TestDb, workspace: &TestWorkspace) -> JobId {
        let mut tx = test.app.begin_in(workspace.id).await.unwrap();
        let id = jobs::enqueue(
            &mut tx,
            workspace.id,
            &Deliver {
                delivery: Id::new(),
            },
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        id
    }

    /// A job is read by its workspace with its kind, state and attempts but never its payload,
    /// which may hold internal inputs; for another workspace it does not exist (`404`).
    #[tokio::test]
    async fn a_job_is_read_without_its_payload_and_only_by_its_workspace() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let globex = test.workspace("globex").await;
        let id = waiting_job(&test, &acme).await;
        let app = test.app();
        let path = format!("/v1/jobs/{id}");
        let read = app.get(&path).bearer(&acme.key).send().await;
        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(
            (
                read.json["kind"].clone(),
                read.json["state"].clone(),
                read.json["attempts"].clone(),
                read.json["last_error"].clone()
            ),
            (
                json!("webhook.deliver"),
                json!("available"),
                json!(0),
                json!(null)
            )
        );
        assert!(read.json.get("payload").is_none());
        assert_eq!(
            app.get(&path).bearer(&globex.key).send().await.status,
            StatusCode::NOT_FOUND
        );
    }

    /// Cancelling a waiting job ends it at once (`cancelled`, with the reason as its last
    /// error); cancelling it again is `409 invalid_state`, since an ended job cannot change.
    #[tokio::test]
    async fn cancelling_a_waiting_job_ends_it() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let id = waiting_job(&test, &acme).await;
        let app = test.app();
        let path = format!("/v1/jobs/{id}/cancel");
        let cancelled = app
            .post(&path)
            .bearer(&acme.key)
            .idempotency("cancel-1")
            .send()
            .await;
        assert_eq!(
            (cancelled.status, cancelled.json["state"].clone()),
            (StatusCode::OK, json!("cancelled"))
        );
        assert_eq!(cancelled.json["last_error"]["code"], json!("cancelled"));
        assert!(cancelled.json["finished_at"].is_string());
        let again = app
            .post(&path)
            .bearer(&acme.key)
            .idempotency("cancel-2")
            .send()
            .await;
        assert_eq!(
            (again.status, again.json["code"].as_str()),
            (StatusCode::CONFLICT, Some("invalid_state"))
        );
    }

    /// Cancelling a running job only asks it to stop (`cancel_requested_at`): the job keeps its
    /// lease and stops at its next chunk boundary, so an effect under way is recorded first.
    #[tokio::test]
    async fn cancelling_a_running_job_asks_it_to_stop() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let id = waiting_job(&test, &acme).await;
        assert_eq!(
            lanes::claim(&test.worker, Queue::Webhooks, "worker-a", 1)
                .await
                .unwrap()
                .len(),
            1
        );
        let reply = test
            .app()
            .post(&format!("/v1/jobs/{id}/cancel"))
            .bearer(&acme.key)
            .idempotency("cancel")
            .send()
            .await;
        assert_eq!(
            (reply.status, reply.json["state"].clone()),
            (StatusCode::OK, json!("running"))
        );
        assert!(reply.json["cancel_requested_at"].is_string());
    }
}
