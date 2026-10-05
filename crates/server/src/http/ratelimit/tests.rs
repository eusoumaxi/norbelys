//! Tests of the rate limits. Policy: each budget and its share per replica, spends of several
//! units, the IETF fields of an admitted and a refused answer, who pays for a request, and the
//! client address. API: the middleware on real `/v1` answers, a replay, an exhausted budget and
//! the `send` budget of a created message.

use axum::http::{Request, StatusCode};
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::domain::identity::AuthMethod;
use crate::domain::scope::{MembershipRole, ScopeSet};
use crate::identity::authority::Actor;
use crate::identity::sessions::SessionRow;
use crate::testing::{Reply, TestApp, TestDb};

/// Every value of the field `name` in `reply`, in order.
fn values<'a>(reply: &'a Reply, name: &str) -> Vec<&'a str> {
    reply
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect()
}

/// `POST /v1/people` for Ada with `key`, under the idempotency key `ada`.
async fn create_ada(app: &TestApp, key: &str) -> Reply {
    app.post("/v1/people")
        .bearer(key)
        .idempotency("ada")
        .json(json!({ "email": "ada@example.com" }))
        .send()
        .await
}

/// Every policy's budget, as one replica of a deployment of 600 holds it (slow enough to refill
/// that nothing comes back during the loop), is admitted in full and the next request is refused
/// with a wait that never exceeds one refill period, while another key keeps its own whole
/// budget: the guarantee each row of the policy table promises.
#[test]
fn each_policy_admits_its_budget_per_key_and_no_more() {
    let limits = Limits::new(false, 600, vec![]);
    for policy in Policy::iter() {
        let (count, window) = policy.share(600);
        for spent in 1..=count {
            let allowance = limits.check(policy, b"a").unwrap();
            assert_eq!(allowance.remaining, count - spent, "{policy:?}");
        }
        let refused = limits.check(policy, b"a").unwrap_err();
        assert_eq!(refused.code, Code::RateLimited, "{policy:?}");
        let wait = refused.retry_after.unwrap();
        assert!(
            wait >= 1 && wait <= (window / count).as_secs().max(1),
            "{policy:?} waits {wait}s"
        );
        assert!(limits.check(policy, b"b").is_ok(), "{policy:?}");
    }
}

/// The request policies are budgets of the deployment, each api replica holding its share but
/// never less than the most one request spends, while every sign-in policy is each replica's
/// whole budget: what `API_REPLICAS` changes and what it leaves alone.
#[test]
fn shared_budgets_are_divided_among_replicas() {
    for policy in Policy::iter() {
        let whole = policy.budget();
        assert_eq!(policy.share(1), whole, "{policy:?}");
        assert_eq!(policy.share(0), whole, "{policy:?}");
        if policy.shared() {
            assert!(policy.share(5).0 < whole.0, "{policy:?}");
            assert_eq!(policy.share(5).1, whole.1, "{policy:?}");
        } else {
            assert_eq!(policy.share(5), whole, "{policy:?}");
        }
    }
    assert_eq!(Policy::Default.share(2).0, 3_000);
    assert_eq!(Policy::Access.share(3).0, 400);
    assert_eq!(Policy::Send.share(4).0, 750);
    assert_eq!(Policy::Default.share(1_000_000).0, 1);
    assert_eq!(Policy::Access.share(1_000_000).0, 1);
    assert_eq!(Policy::Send.share(1_000_000).0, MOST_MESSAGES_PER_REQUEST);
}

/// A spend of several units is admitted whole or refused whole, a refused spend takes nothing,
/// and a spend beyond the whole budget waits a window: how one request creating many messages
/// is counted.
#[test]
fn a_spend_of_several_units_is_admitted_whole_or_not_at_all() {
    let limits = Limits::new(false, 30, vec![]);
    let units = |count| NonZeroU32::new(count).unwrap();
    let first = limits.spend(Policy::Send, b"w", units(60)).unwrap();
    assert_eq!((first.quota, first.remaining), (100, 40));
    let refused = limits.spend(Policy::Send, b"w", units(50)).unwrap_err();
    assert_eq!((refused.policy, refused.quota), (Policy::Send, 100));
    assert!(refused.retry_after >= 1);
    let rest = limits.spend(Policy::Send, b"w", units(40)).unwrap();
    assert_eq!(rest.remaining, 0);
    let beyond = limits.spend(Policy::Send, b"v", units(101)).unwrap_err();
    assert_eq!(beyond.retry_after, 60);
    let whole = limits.spend(Policy::Send, b"v", units(100)).unwrap();
    assert_eq!(whole.remaining, 0);
    let workspace = WorkspaceId::trusted(Uuid::now_v7());
    let many = limits.spend_messages(workspace, 10_000).unwrap();
    assert_eq!(many.remaining, 0);
    assert!(limits.spend_messages(workspace, 0).is_err());
}

/// The admitted answer names its policy, quota, window, what is left and when the budget is
/// whole again; the refused one is the `429` problem with `Retry-After` and says that nothing is
/// left until then: the IETF field syntax a client parses.
#[test]
fn answers_carry_the_ietf_fields() {
    let limits = Limits::new(false, 1, vec![]);
    limits.check(Policy::SignIn, b"k").unwrap();
    let allowance = limits.check(Policy::SignIn, b"k").unwrap();
    let mut headers = HeaderMap::new();
    allowance.write(&mut headers);
    assert_eq!(
        headers.get("ratelimit-policy").unwrap(),
        "\"sign_in\";q=10;w=900"
    );
    assert_eq!(headers.get("ratelimit").unwrap(), "\"sign_in\";r=8;t=180");
    let refused = Refusal {
        policy: Policy::Default,
        quota: 10,
        window: Duration::from_secs(60),
        retry_after: 7,
    }
    .into_response();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.headers().get("retry-after").unwrap(), "7");
    assert_eq!(
        refused.headers().get("ratelimit-policy").unwrap(),
        "\"default\";q=10;w=60"
    );
    assert_eq!(
        refused.headers().get("ratelimit").unwrap(),
        "\"default\";r=0;t=7"
    );
}

/// A program's credential spends its workspace's `default`, the dashboard (a workspace token or
/// the session cookie alone) its person's `access`, and an anonymous request its client
/// address's `access`: who pays for a request follows from its credential alone.
#[test]
fn each_credential_spends_its_request_policy() {
    let workspace = WorkspaceId::trusted(Uuid::now_v7());
    let user = Id::<User>::new();
    let cases = [
        (
            Credential::ApiKey,
            Actor::ApiKey {
                key: Id::new(),
                created_by: user,
            },
            (Policy::Default, workspace_key(workspace)),
        ),
        (
            Credential::OAuth,
            Actor::OAuth {
                user,
                grant: Id::new(),
            },
            (Policy::Default, workspace_key(workspace)),
        ),
        (
            Credential::WorkspaceToken,
            Actor::User {
                user,
                session: Id::new(),
            },
            (Policy::Access, user_key(user)),
        ),
    ];
    for (credential, actor, expected) in cases {
        let mut extensions = Extensions::new();
        extensions.insert(Principal {
            workspace,
            actor,
            scopes: ScopeSet::all(),
            role: MembershipRole::Owner,
            test_mode: false,
            credential,
        });
        assert_eq!(
            subject(&extensions, &HeaderMap::new(), false, &[]),
            expected,
            "{credential:?}"
        );
    }
    let now = crate::process::now();
    let session = Id::new();
    let mut browser = Extensions::new();
    browser.insert(SignedIn {
        user,
        session,
        row: SessionRow {
            id: session,
            user,
            method: AuthMethod::EmailCode,
            sso_connection: None,
            sso_policy_version: None,
            authenticated_at: now,
            revoked_at: None,
            expires_at: now,
            idle_expires_at: now,
            last_seen_at: now,
            user_active: true,
        },
    });
    assert_eq!(
        subject(&browser, &HeaderMap::new(), false, &[]),
        (Policy::Access, user_key(user))
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
    assert_eq!(
        subject(&Extensions::new(), &headers, true, &[]),
        (Policy::Access, b"address:unknown".to_vec())
    );
}

/// The client address is the peer unless a trusted proxy's `X-Forwarded-For` is configured,
/// so a client cannot pick its own rate-limit key by sending the header itself.
#[test]
fn the_client_address_trusts_the_forwarded_header_only_when_told() {
    let mut request = Request::builder()
        .header("x-forwarded-for", "203.0.113.9, 10.0.0.1")
        .body(())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "198.51.100.4:4000".parse::<SocketAddr>().unwrap(),
    ));
    let (parts, ()) = request.into_parts();
    let of = |parts: &Parts, trust| {
        ClientAddress::of(
            &parts.headers,
            &parts.extensions,
            trust,
            &["198.51.100.4".parse().unwrap(), "10.0.0.1".parse().unwrap()],
        )
    };
    assert_eq!(of(&parts, false).as_key(), "198.51.100.4");
    assert_eq!(of(&parts, true).as_key(), "203.0.113.9");
    // Enabling forwarding alone does not authorize the TCP peer.
    assert_eq!(
        ClientAddress::of(&parts.headers, &parts.extensions, true, &[]).as_key(),
        "198.51.100.4"
    );
    // An attacker prepending a fake address cannot bypass the first untrusted hop.
    let mut forged = Request::builder()
        .header("x-forwarded-for", "192.0.2.66, 203.0.113.9")
        .body(())
        .unwrap();
    forged.extensions_mut().insert(ConnectInfo(
        "198.51.100.4:4000".parse::<SocketAddr>().unwrap(),
    ));
    let (forged, ()) = forged.into_parts();
    assert_eq!(of(&forged, true).as_key(), "203.0.113.9");
    let (bare, ()) = Request::builder().body(()).unwrap().into_parts();
    assert_eq!(of(&bare, true).as_key(), "unknown");
}

/// Every `/v1` answer carries its request policy's fields, whatever its status and whoever
/// asks: a program's key spends its workspace's `default` (a refused body as well), a
/// signed-in browser and an anonymous request each spend their own `access`.
#[tokio::test]
async fn every_v1_answer_carries_its_request_policy() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    // Shares of a deployment of 600 replicas: `default` 10 and `access` 2 per minute, slow
    // enough to refill that the counts below are exact.
    let app = test.app_with_replicas(600);
    let listed = app.get("/v1/people").bearer(&acme.key).send().await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(
        values(&listed, "ratelimit-policy"),
        ["\"default\";q=10;w=60"]
    );
    assert_eq!(values(&listed, "ratelimit"), ["\"default\";r=9;t=6"]);
    let invalid = app
        .post("/v1/people")
        .bearer(&acme.key)
        .idempotency("invalid")
        .json(json!({ "email": "not an address" }))
        .send()
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        invalid.json
    );
    assert_eq!(values(&invalid, "ratelimit"), ["\"default\";r=8;t=12"]);
    let anonymous = app.get("/v1/auth/config").send().await;
    assert_eq!(anonymous.status, StatusCode::OK, "{}", anonymous.json);
    assert_eq!(
        values(&anonymous, "ratelimit-policy"),
        ["\"access\";q=2;w=60"]
    );
    assert_eq!(values(&anonymous, "ratelimit"), ["\"access\";r=1;t=30"]);
    let session = test.session("ada@example.com").await;
    let me = app.get("/v1/me").browser(&session).send().await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.json);
    assert_eq!(values(&me, "ratelimit"), ["\"access\";r=1;t=30"]);
}

/// A retry answered from its stored response still spends a unit of the request policy and
/// shows the current fields: a retry storm with one key is limited like any other.
#[tokio::test]
async fn a_replayed_answer_spends_and_shows_the_current_budget() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app_with_replicas(600);
    let first = create_ada(&app, &acme.key).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json);
    assert_eq!(values(&first, "ratelimit"), ["\"default\";r=9;t=6"]);
    let replayed = create_ada(&app, &acme.key).await;
    assert_eq!(replayed.status, StatusCode::CREATED, "{}", replayed.json);
    assert_eq!(replayed.header("idempotent-replayed"), Some("true"));
    assert_eq!(values(&replayed, "ratelimit"), ["\"default\";r=8;t=12"]);
}

/// An exhausted budget answers `429 rate_limited` with `Retry-After` and fields saying that
/// nothing is left until then, while another workspace's budget is untouched: each workspace is
/// limited alone.
#[tokio::test]
async fn an_exhausted_budget_answers_429_until_it_refills() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    // A deployment of 6,000 replicas: each holds one request per minute of `default`.
    let app = test.app_with_replicas(6_000);
    let admitted = app.get("/v1/people").bearer(&acme.key).send().await;
    assert_eq!(admitted.status, StatusCode::OK, "{}", admitted.json);
    assert_eq!(
        values(&admitted, "ratelimit-policy"),
        ["\"default\";q=1;w=60"]
    );
    assert_eq!(values(&admitted, "ratelimit"), ["\"default\";r=0;t=60"]);
    let refused = app.get("/v1/people").bearer(&acme.key).send().await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.json["code"], "rate_limited");
    let wait: u64 = refused.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait), "waits {wait}s");
    assert_eq!(
        values(&refused, "ratelimit-policy"),
        ["\"default\";q=1;w=60"]
    );
    assert_eq!(
        values(&refused, "ratelimit"),
        [format!("\"default\";r=0;t={wait}").as_str()]
    );
    let other = app.get("/v1/people").bearer(&globex.key).send().await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.json);
}

/// `POST /v1/messages` spends the workspace's `send` budget, a unit per message, and its answer
/// shows that policy before the request's own: the two budgets a sending client paces itself
/// by.
#[tokio::test]
async fn a_created_message_shows_the_send_budget() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app_with_replicas(600);
    let connected = app
        .post("/v1/connections")
        .bearer(&acme.key)
        .idempotency("connection")
        .json(json!({
            "provider": "smtp",
            "account_email": "max@acme.example",
            "smtp": { "host": "smtp.acme.example", "port": 587, "security": "starttls", "password": "secret" },
        }))
        .send()
        .await;
    assert_eq!(connected.status, StatusCode::CREATED, "{}", connected.json);
    let sent = app
        .post("/v1/messages")
        .bearer(&acme.key)
        .idempotency("message")
        .json(json!({
            "from": "max@acme.example",
            "to": ["ada@example.com"],
            "subject": "Hello",
            "html": "<p>Hi Ada</p>",
        }))
        .send()
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.json);
    assert_eq!(
        values(&sent, "ratelimit-policy"),
        ["\"send\";q=100;w=60", "\"default\";q=10;w=60"]
    );
    assert_eq!(
        values(&sent, "ratelimit"),
        ["\"send\";r=99;t=1", "\"default\";r=8;t=12"]
    );
}
