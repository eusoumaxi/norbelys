//! The authorization server's gate, in process: the scenarios it must pass before a deployment
//! turns it on, as API tests on a real database through the product router.
//!
//! The flows themselves are proven in `identity::oauth::tests`: the authorization code with PKCE
//! S256 (a wrong verifier refused, a replayed code revoking its grant, a `plain` challenge and a
//! foreign resource or redirect refused), refresh rotation with reuse detection, the device flow's
//! states (pending, slow down, approved, denied, consumed once) and revocation by the client that
//! holds a token. This module proves the rest of the gate: the switch that keeps the whole server
//! off, a client trying to revoke another client's token, the token endpoint's budget, a client
//! metadata document that is stale or too large, and a consent without the workspace's single
//! sign-on proof. Running the MCP conformance suite and real clients (the MCP Inspector, Claude,
//! Cursor, the command-line client) against a deployment remains the gate's manual step.

use axum::http::StatusCode;
use serde_json::json;

use super::clients::CLI;
use super::tests::{API, MCP_CLIENT, grant_token, register_mcp_client};
use crate::domain::oauth::Audience;
use crate::domain::scope::ScopeSet;
use crate::identity::Identity;
use crate::identity::fetch::{FetchError, Fetcher};
use crate::testing::{DASHBOARD, Reply, Sink, TestApp, TestDb};

/// A metadata-document client's redirect URI in these tests.
const REDIRECT: &str = "https://client.test/callback";
/// A PKCE S256 challenge (RFC 7636 appendix B).
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// `pairs` posted as a form to `path`, as an OAuth client sends it.
async fn post_form(app: &TestApp, path: &str, pairs: &[(&str, &str)]) -> Reply {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    app.post(path)
        .raw("application/x-www-form-urlencoded", body)
        .send()
        .await
}

/// A device authorization of the command-line client; answers its user code.
async fn device_code(app: &TestApp) -> String {
    let started = post_form(
        app,
        "/oauth/device_authorization",
        &[("client_id", CLI), ("resource", API)],
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.json);
    started.json["user_code"]
        .as_str()
        .expect("a user code")
        .to_owned()
}

/// With the authorization server turned off (`OAUTH_SERVER_DISABLED`), each of its routes answers
/// `404` as if it did not exist, the metadata that would send an MCP client to it included, while
/// the key set that verifies workspace tokens stays. This is the deferred state a deployment runs
/// in until the gate passes: no client can start a grant, and none is told there is a server.
#[tokio::test]
async fn the_authorization_server_can_be_turned_off() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let app = test.app_with_identity(Identity {
        oauth_server: false,
        ..Identity::for_tests()
    });
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource/mcp",
        "/oauth/authorize?response_type=code&client_id=norbelys-cli",
        "/oauth/consent?user_code=ABCDEFGH",
    ] {
        let reply = app.get(path).send().await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
    for path in [
        "/oauth/token",
        "/oauth/revoke",
        "/oauth/device_authorization",
    ] {
        let reply = post_form(&app, path, &[("client_id", CLI)]).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
    let keys = app.get("/.well-known/jwks.json").send().await;
    assert_eq!(keys.status, StatusCode::OK);
}

/// A client may revoke only the tokens issued to it (RFC 7009 §2.1): another client presenting a
/// token is refused with `invalid_grant` and the grant keeps working, so one application cannot
/// end another's access.
#[tokio::test]
async fn a_client_cannot_revoke_another_clients_token() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let workspace = test.workspace("acme").await;
    let (_, access) = grant_token(&test, &workspace, Audience::Cli, ScopeSet::all()).await;
    let app = test.app();
    let refused = post_form(
        &app,
        "/oauth/revoke",
        &[("token", &access), ("client_id", MCP_CLIENT)],
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.json);
    assert_eq!(refused.json["error"], "invalid_grant");
    let still = app.get("/v1/people").bearer(&access).send().await;
    assert_eq!(still.status, StatusCode::OK, "the grant was left as it is");
}

/// The token endpoint admits 30 requests a minute per client, counted before the client
/// authenticates, then answers `429` with `Retry-After`; another client keeps its own budget. A
/// client stuck in a loop, or guessing a secret, is slowed without touching the others.
#[tokio::test]
async fn the_token_endpoint_admits_thirty_requests_a_minute_per_client() {
    let test = TestDb::new().await;
    register_mcp_client(&test).await;
    let app = test.app();
    let refresh = |client: &'static str| {
        let app = &app;
        async move {
            post_form(
                app,
                "/oauth/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", "nonsense"),
                    ("client_id", client),
                ],
            )
            .await
        }
    };
    let mut admitted = 0;
    let limited = loop {
        let reply = refresh(CLI).await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            break reply;
        }
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.json);
        admitted += 1;
        // The budget refills one request every two seconds: a slow run admits a few more, never
        // dozens.
        assert!(admitted < 40, "the token endpoint never refused");
    };
    assert!(admitted >= 30, "only {admitted} requests were admitted");
    assert!(limited.header("retry-after").is_some());
    let other = refresh(MCP_CLIENT).await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST, "{}", other.json);
}

/// The revoke endpoint admits 60 requests a minute per client address, counted before any
/// metadata fetch like its siblings, then answers `429` with `Retry-After`; another address keeps
/// its own budget. Before the `OauthAddress` guard, a single anonymous address could spray
/// `/oauth/revoke` with fresh `https://…` client ids and spend the process-global fetch slots a
/// `clients::fetch` needs, degrading `authorize` and `token` onboarding of any new MCP server; the
/// guard bounds it like the rest, and a second address is unaffected.
#[tokio::test]
async fn the_revoke_endpoint_admits_sixty_requests_a_minute_per_client_address() {
    let test = TestDb::new().await;
    let app = test.app();
    let revoke = |addr: &'static str| {
        let app = &app;
        async move {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([("token", "nonsense"), ("client_id", CLI)])
                .finish();
            app.post("/oauth/revoke")
                .header("x-forwarded-for", addr)
                .raw("application/x-www-form-urlencoded", body)
                .send()
                .await
        }
    };
    let mut admitted = 0;
    let limited = loop {
        let reply = revoke("203.0.113.9").await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            break reply;
        }
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
        admitted += 1;
        // The budget refills one request per second: a slow run admits a few more, never dozens.
        assert!(admitted < 70, "the revoke endpoint never refused");
    };
    assert!(admitted >= 60, "only {admitted} requests were admitted");
    assert!(limited.header("retry-after").is_some());
    let other = revoke("203.0.113.10").await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.json);
}

/// The `OauthAddress` guard on `/oauth/revoke` fires before any metadata fetch, the ordering this
/// change protects: a `https://…` client id that is not kept yet makes `clients::find` attempt a
/// fetch, which fails at once in the test harness. Within the address budget the request reaches
/// the handler and is answered `401 invalid_client` (the fetch failed); once the budget is spent
/// it is answered `429` before the fetch is ever attempted. If the guard moved after
/// `authenticated_client`, the fetch would fail first on every request and the endpoint would
/// answer `401` forever, never `429` — so this test pins the ordering directly.
#[tokio::test]
async fn the_revoke_endpoint_spends_oauth_address_before_a_metadata_fetch() {
    let test = TestDb::new().await;
    let app = test.app();
    // A document client id nothing answers on: any fetch fails at once (connection refused).
    let client = "https://127.0.0.1:1/revoke-order.json";
    let revoke = |addr: &'static str| {
        let app = &app;
        let client = client;
        async move {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([("token", "nonsense"), ("client_id", client)])
                .finish();
            app.post("/oauth/revoke")
                .header("x-forwarded-for", addr)
                .raw("application/x-www-form-urlencoded", body)
                .send()
                .await
        }
    };
    let mut admitted = 0;
    let limited = loop {
        let reply = revoke("203.0.113.9").await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            break reply;
        }
        // Within budget the fetch is attempted and fails (invalid_client), not 200: this is what
        // proves the request passed the guard and reached `authenticated_client`.
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.json);
        admitted += 1;
        // The budget refills one request per second: a slow run admits a few more, never dozens.
        assert!(admitted < 70, "the revoke endpoint never refused");
    };
    assert!(admitted >= 60, "only {admitted} requests were admitted");
    assert!(limited.header("retry-after").is_some());
    // A second address keeps its own budget, and the fetch is still attempted there.
    let other = revoke("203.0.113.10").await;
    assert_eq!(other.status, StatusCode::UNAUTHORIZED, "{}", other.json);
}

/// A metadata-document client is trusted only while its kept document is fresh: within its cache
/// lifetime an authorization request goes on to consent; once stale, the document is fetched
/// again first, and when that fails the request is refused without a redirect, as for an unknown
/// client. A client that changed its document (or lost its domain) is never served from an old
/// copy.
#[tokio::test]
async fn a_stale_metadata_document_is_fetched_again_before_it_is_used() {
    let test = TestDb::new().await;
    // A document URL nothing answers on: any fetch fails at once.
    let client = "https://127.0.0.1:1/client.json";
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, kind, name, redirect_uris, metadata, fetched_at)
         VALUES ($1, 'cimd', 'Remote client', $2, '{\"cache_seconds\": 60}', now())",
    )
    .bind(client)
    .bind(vec![REDIRECT.to_owned()])
    .execute(test.system.pool())
    .await
    .unwrap();
    let app = test.app();
    let mcp = format!("{API}/mcp");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("response_type", "code"),
            ("client_id", client),
            ("redirect_uri", REDIRECT),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("resource", mcp.as_str()),
            ("state", "s1"),
        ])
        .finish();
    let authorize = format!("/oauth/authorize?{query}");

    let fresh = app.get(&authorize).send().await;
    assert_eq!(fresh.status, StatusCode::SEE_OTHER, "{}", fresh.json);
    assert!(
        fresh
            .header("location")
            .is_some_and(|location| location.starts_with(&format!("{DASHBOARD}/oauth/consent")))
    );

    sqlx::query(
        "UPDATE oauth_clients SET fetched_at = now() - interval '2 minutes' WHERE client_id = $1",
    )
    .bind(client)
    .execute(test.system.pool())
    .await
    .unwrap();
    let stale = app.get(&authorize).send().await;
    assert_eq!(stale.status, StatusCode::BAD_REQUEST, "{}", stale.json);
    assert!(stale.header("location").is_none());
}

/// A metadata document larger than 64 KiB is refused by the bounded fetcher every document goes
/// through, before it is parsed, while a small answer from the same place is read: a client cannot
/// make the server hold an arbitrary document.
#[tokio::test]
async fn an_oversized_metadata_document_is_refused() {
    let sink = Sink::start().await;
    let fetcher = Fetcher::new(true).unwrap();
    let oversized = url::Url::parse(&sink.url("/200?bytes=70000")).unwrap();
    assert!(matches!(
        fetcher.json(&oversized).await,
        Err(FetchError::TooLarge)
    ));
    let small = url::Url::parse(&sink.url("/200?bytes=10")).unwrap();
    assert!(matches!(
        fetcher.json(&small).await,
        Err(FetchError::NotJson)
    ));
}

/// In a workspace that enforces single sign-on, a person signed in another way (an email code)
/// cannot approve an application there: the consent is refused with `403` and no grant is made,
/// so an application never gets access the workspace's own sign-in would not give.
#[tokio::test]
async fn consent_without_the_sso_proof_is_refused_under_enforcement() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let workspace = test.workspace("acme").await;
    let session = test.session("owner@acme.example").await;
    sqlx::query(
        "INSERT INTO sso_connections (workspace_id, kind, name, issuer, client_id, enforced, enforced_at, status)
         VALUES ($1, 'oidc', 'Okta', 'https://idp.example', 'norbelys', true, now(), 'active')",
    )
    .bind(workspace.id.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let app = test.app();
    let user_code = device_code(&app).await;
    let approved = app
        .post("/oauth/consent")
        .browser(&session)
        .json(json!({
            "user_code": user_code,
            "workspace_id": workspace.id.to_string(),
            "approve": true,
        }))
        .send()
        .await;
    assert_eq!(approved.status, StatusCode::FORBIDDEN, "{}", approved.json);
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_grants WHERE user_id = $1")
        .bind(workspace.owner.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(grants, 0);
}
