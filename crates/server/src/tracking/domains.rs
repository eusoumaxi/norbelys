//! The ingress role's certificate permission and HTTPS route proof. Neither route
//! grants tenant access. Opens and clicks retain their database-free request path.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use serde::Deserialize;
use uuid::Uuid;

use crate::db::Database;
use crate::http::AppState;

#[derive(Deserialize)]
struct Permission {
    domain: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/internal/tracking-domains/allow", get(allow))
        .route("/.well-known/norbelys-tracking", get(proof))
}

/// Caddy requires 200 for an allowed certificate and denies every other status.
/// The reverse proxy keeps this permission route off the public HTTP surface.
async fn allow(State(db): State<Database>, Query(input): Query<Permission>) -> StatusCode {
    match route(&db, &input.domain).await {
        Ok(Some(_)) => StatusCode::OK,
        Ok(None) => StatusCode::FORBIDDEN,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// A valid certificate alone is insufficient: the hostname must reach this ingress
/// and resolve to the domain whose ownership the worker already checked.
async fn proof(State(db): State<Database>, headers: HeaderMap) -> Response {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match route(&db, host).await {
        Ok(Some(id)) => ([(header::CACHE_CONTROL, "no-store")], id.to_string()).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn route(db: &Database, host: &str) -> Result<Option<Uuid>, sqlx::Error> {
    let Some(host) = crate::senders::domains::hostname(host) else {
        return Ok(None);
    };
    sqlx::query_scalar("SELECT tracking_domain_route($1)")
        .bind(host)
        .fetch_one(db.pool())
        .await
}
