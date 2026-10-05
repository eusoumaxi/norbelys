//! The coverage tests of the request path: every operation of the router emits its request's
//! canonical event exactly once, and the two sides of the runtime reconciliation agree over
//! whatever the requests emitted, sampled successes included. (The job registry's walk is beside
//! the registry, in the worker role.)

use axum::http::StatusCode;

use super::capture::Capture;
use super::mirror;
use crate::domain::telemetry::{Event, HEAD_SHARE, bucket};
use crate::testing::TestDb;

/// `path` with every `{parameter}` replaced by `x`: a concrete path its route matches.
fn concrete(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut inside = false;
    for c in path.chars() {
        match c {
            '{' => {
                inside = true;
                out.push('x');
            }
            '}' => inside = false,
            _ if inside => {}
            _ => out.push(c),
        }
    }
    out
}

/// Walks every operation of the OpenAPI document (the router's own, dashboard operations
/// included) and sends one request to each through the in-process router: each emits exactly one
/// `http.request` event naming its route template and method, and every canonical event the
/// requests caused (their denials included) is counted on both sides. A route mounted outside
/// the request middleware, or an operation answering without passing through it, fails here
/// rather than going dark in production.
#[tokio::test]
async fn every_operation_emits_its_request_event_once() {
    let test = TestDb::new().await;
    let app = test.app();
    let capture = Capture::default();
    let _guard = capture.install();
    let document = serde_json::to_value(crate::http::router::openapi()).unwrap();
    let mut operations = 0_u64;
    for (path, item) in document["paths"].as_object().unwrap() {
        for method in ["get", "post", "put", "patch", "delete"] {
            if item.get(method).is_none() {
                continue;
            }
            let url = concrete(path);
            let call = match method {
                "get" => app.get(&url),
                "post" => app.post(&url),
                "patch" => app.patch(&url),
                "delete" => app.delete(&url),
                other => panic!("{other} {path}: the test client has no `{other}`; add it"),
            };
            let from = capture.events().len();
            let reply = call.send().await;
            let events = capture.named_since(from, Event::HttpRequest.as_str());
            assert_eq!(
                events.len(),
                1,
                "{method} {path} answered {} and emitted {} request events",
                reply.status,
                events.len()
            );
            assert_eq!(events[0].field("route"), path.as_str(), "{method} {path}");
            assert_eq!(
                events[0].field("method"),
                method.to_uppercase(),
                "{method} {path}"
            );
            operations += 1;
        }
    }
    assert!(
        operations > 50,
        "the document lists the operations ({operations})"
    );
    let (seen, units) = mirror::counts();
    assert_eq!(seen.get(&Event::HttpRequest).copied(), Some(operations));
    assert_eq!(seen, units, "every emitted event is counted on both sides");
}

/// A fast success on a hot route is recorded when its request id falls in the 5 % draw and not
/// otherwise, and the reconciliation counts only what was recorded, on both sides; a refusal of
/// the same route is recorded whatever its request id. Sampling thins the logs; it can neither
/// hide an error nor unbalance the coverage counters.
#[tokio::test]
async fn sampled_successes_stay_reconciled() {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let app = test.app();
    let kept = (0..)
        .map(|i| format!("kept-{i}"))
        .find(|id| bucket(id.as_bytes()) < HEAD_SHARE)
        .unwrap();
    let dropped = (0..)
        .map(|i| format!("dropped-{i}"))
        .find(|id| bucket(id.as_bytes()) >= HEAD_SHARE)
        .unwrap();
    // A first request opens the pool's connection: a cold start could pass the one-second rule,
    // which records every slow request.
    let warm = app.get("/v1/people").bearer(&workspace.key).send().await;
    assert_eq!(warm.status, StatusCode::OK);
    let capture = Capture::default();
    let _guard = capture.install();
    for id in [&kept, &dropped] {
        let reply = app
            .get("/v1/people")
            .bearer(&workspace.key)
            .header("x-request-id", id)
            .send()
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{id}");
    }
    let refused = app
        .get("/v1/people")
        .header("x-request-id", &dropped)
        .send()
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    let recorded: Vec<(String, String)> = capture
        .named_since(0, Event::HttpRequest.as_str())
        .iter()
        .map(|event| {
            (
                event.field("request_id").to_owned(),
                event.field("status").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            (kept.clone(), "200".to_owned()),
            (dropped.clone(), "401".to_owned())
        ]
    );
    let (seen, units) = mirror::counts();
    assert_eq!(seen.get(&Event::HttpRequest).copied(), Some(2));
    assert_eq!(seen, units);
}
