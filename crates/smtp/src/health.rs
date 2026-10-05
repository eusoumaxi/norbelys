//! Health routes for the supervisor and the deploy, unsigned, on the control listener:
//! `GET /health/live` answers `204` while the process serves requests; `GET /health/ready`
//! answers `204` when the database answers too, else `503`.
//!
//! The background loops' state (tail errors, the evidence backlog, admission) is not part of
//! readiness, which is about serving the control API; it is in the metrics and the canonical
//! events, where alerts on the deferred queue, the disk runway and the event backlog read it.

use axum::extract::State as Extract;
use axum::http::StatusCode;

use crate::control::{ApiError, State};

/// `GET /health/live`: `204` while the process serves requests.
pub async fn live() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `GET /health/ready`: `204` when Turso answers, else `503`.
pub async fn ready(Extract(state): Extract<State>) -> StatusCode {
    let answered = state
        .db
        .call(|conn| {
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(ApiError::from)
        })
        .await;
    match answered {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(error) => {
            tracing::error!(error = ?error, "readiness check failed");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// `GET /metrics`: Prometheus exposition independent of OTLP export.
pub async fn metrics() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    match crate::telemetry::exposition() {
        Some(text) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
