//! Recipient checks run on the mail host, which has outbound TCP/25. The core calls this
//! signed control route over its existing private transport; no SMTP listener is exposed on
//! the application host. Eight sockets at most across all requests, with no waiting queue.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State as Extract;
use norbelys_mail::verify::{BATCH_MAX, BatchRequest, BatchResponse, RecipientCheck, Verifier};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::{ApiError, State, parse};

#[derive(Clone)]
pub struct Checks {
    verifier: Arc<Verifier>,
    from: String,
    slots: Arc<Semaphore>,
}

impl Checks {
    pub fn new(
        resolver: hickory_resolver::TokioResolver,
        host: &str,
    ) -> Result<Self, norbelys_mail::net::ConnectorError> {
        Ok(Self {
            verifier: Arc::new(Verifier::new(resolver, host.to_owned())?),
            from: format!("postmaster@{host}"),
            slots: Arc::new(Semaphore::new(BATCH_MAX)),
        })
    }
}

pub async fn check(
    Extract(state): Extract<State>,
    body: Bytes,
) -> Result<Json<BatchResponse>, ApiError> {
    let input: BatchRequest = parse(&body)?;
    if !(1..=BATCH_MAX).contains(&input.emails.len())
        || input.emails.iter().any(|email| email.len() > 254)
    {
        return Err(ApiError::Invalid(format!(
            "emails must contain 1 to {BATCH_MAX} addresses of at most 254 bytes"
        )));
    }
    let count = u32::try_from(input.emails.len()).unwrap_or_default();
    let _permits = state
        .recipient_checks
        .slots
        .clone()
        .try_acquire_many_owned(count)
        .map_err(|_| ApiError::Busy("recipient checks are busy; try again later".to_owned()))?;
    let mut tasks = JoinSet::new();
    for (index, email) in input.emails.into_iter().enumerate() {
        let verifier = Arc::clone(&state.recipient_checks.verifier);
        let from = state.recipient_checks.from.clone();
        tasks.spawn(async move {
            let found = verifier.check(&from, &email).await;
            (
                index,
                RecipientCheck {
                    email,
                    status: found.outcome,
                    detail: found.diagnostic,
                },
            )
        });
    }
    let mut found = Vec::new();
    while let Some(result) = tasks.join_next().await {
        found.push(
            result.map_err(|_| {
                ApiError::Unavailable("a recipient check could not finish".to_owned())
            })?,
        );
    }
    found.sort_by_key(|(index, _)| *index);
    Ok(Json(BatchResponse {
        data: found.into_iter().map(|(_, finding)| finding).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::json;

    use crate::testing::{TempDir, call, router, signed};

    const PATH: &str = "/v1/recipients/check";

    #[tokio::test]
    async fn recipient_checks_require_signed_control_requests_and_bound_the_batch() {
        let dir = TempDir::new();
        let (router, _) = router(&dir);
        let anonymous = Request::builder()
            .method("POST")
            .uri(PATH)
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(call(&router, anonymous).await.0, StatusCode::UNAUTHORIZED);
        for body in [
            json!({"emails": []}),
            json!({"emails": vec!["a@example.com"; 9]}),
            json!({"emails": ["a".repeat(255)]}),
        ] {
            assert_eq!(
                call(&router, signed("POST", PATH, &body.to_string()))
                    .await
                    .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }

    #[tokio::test]
    async fn unavailable_slots_do_not_queue_more_probes() {
        let dir = TempDir::new();
        let (router, state) = router(&dir);
        let _permits = state.recipient_checks.slots.acquire_many(8).await.unwrap();
        let body = json!({"emails": ["ada@example.com"]}).to_string();
        assert_eq!(
            call(&router, signed("POST", PATH, &body)).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn malformed_addresses_answer_in_order_without_network_access() {
        let dir = TempDir::new();
        let (router, _) = router(&dir);
        let body = json!({"emails": ["first", "second"]}).to_string();
        let (status, answer) = call(&router, signed("POST", PATH, &body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["data"][0]["email"], "first");
        assert_eq!(answer["data"][1]["email"], "second");
        assert_eq!(answer["data"][0]["status"], "unknown");
    }
}
