//! API tests of the authorization server, in process on a real database: the metadata, the
//! command-line client's device flow end to end, the MCP clients' code flow with PKCE, the consent
//! contract, refresh rotation with reuse detection, revocation, and the surfaces each token
//! reaches.
//!
//! This module also holds what other modules' tests share: a registered MCP client, and a grant
//! with an access token minted directly.

use axum::http::StatusCode;
use serde_json::json;

use super::clients::CLI;
use super::grants::{self, NewGrant};
use crate::domain::identity::{AuthMethod, Proof};
use crate::domain::ids::{Grant, Id};
use crate::domain::oauth::{Audience, Resources};
use crate::domain::scope::ScopeSet;
use crate::identity::tokens::{AccessGrant, KeyRing};
use crate::testing::{DASHBOARD, Reply, TestApp, TestDb, TestSession, TestWorkspace, keys};

/// The tests' public API URL (the harness's `PUBLIC_API_URL`).
pub(crate) const API: &str = "http://127.0.0.1:3001";
/// The registered MCP client of the tests.
pub(crate) const MCP_CLIENT: &str = "mcp-test-client";
/// Its redirect URI.
const REDIRECT: &str = "https://client.test/callback";
/// A PKCE verifier and its S256 challenge (RFC 7636 appendix B).
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn resources() -> Resources {
    Resources::of(&url::Url::parse(API).unwrap())
}

/// Registers the tests' MCP client (public, one redirect URI), once per database.
pub(crate) async fn register_mcp_client(test: &TestDb) {
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, kind, name, redirect_uris)
         VALUES ($1, 'registered', 'Test MCP client', $2) ON CONFLICT DO NOTHING",
    )
    .bind(MCP_CLIENT)
    .bind(vec![REDIRECT.to_owned()])
    .execute(test.system.pool())
    .await
    .expect("register the client");
}

/// A live grant of `workspace`'s owner for `audience` with `scopes`, and an access token of it,
/// minted directly (the flows that create them are tested here).
pub(crate) async fn grant_token(
    test: &TestDb,
    workspace: &TestWorkspace,
    audience: Audience,
    scopes: ScopeSet,
) -> (Id<Grant>, String) {
    register_mcp_client(test).await;
    let resources = resources();
    let (client, resource) = match audience {
        Audience::Cli => (CLI, resources.api.clone()),
        Audience::Mcp => (MCP_CLIENT, resources.mcp.clone()),
    };
    let mut tx = test.app.begin().await.unwrap();
    let grant = grants::create(
        &mut tx,
        &NewGrant {
            client_id: client,
            user: workspace.owner,
            workspace: workspace.id,
            resource: &resource,
            scopes,
            proof: Proof {
                method: AuthMethod::EmailCode,
                connection: None,
                policy_version: None,
                authenticated_at: crate::process::now().0,
            },
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let minted = KeyRing::new()
        .mint_access(
            &test.app,
            &keys(),
            audience,
            &resources,
            &AccessGrant {
                grant,
                user: workspace.owner,
                workspace: workspace.id.id(),
                client_id: client.to_owned(),
                scopes,
            },
        )
        .await
        .unwrap();
    (grant, minted.token)
}

fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
        .into_bytes()
}

async fn post_form(app: &TestApp, path: &str, pairs: &[(&str, &str)]) -> Reply {
    app.post(path)
        .raw("application/x-www-form-urlencoded", form(pairs))
        .send()
        .await
}

/// The query parameters of a URL, by name.
fn query_of(location: &str) -> std::collections::HashMap<String, String> {
    url::Url::parse(location)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// A workspace whose owner is signed in, with a signing key.
async fn signed_in(test: &TestDb) -> (TestWorkspace, TestSession) {
    test.signing_key().await;
    let workspace = test.workspace("acme").await;
    let session = test.session("owner@acme.example").await;
    (workspace, session)
}

/// The metadata names every endpoint, S256 only, the `iss` parameter, and the MCP resource with
/// this server as its authorization server, which is all an MCP client needs to discover how to
/// authorize.
#[tokio::test]
async fn metadata_describes_the_server_and_the_resource() {
    let test = TestDb::new().await;
    let app = test.app();
    let server = app
        .get("/.well-known/oauth-authorization-server")
        .send()
        .await;
    assert_eq!(server.status, StatusCode::OK);
    assert_eq!(server.json["issuer"], API);
    assert_eq!(server.json["token_endpoint"], format!("{API}/oauth/token"));
    assert_eq!(
        server.json["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        server.json["authorization_response_iss_parameter_supported"],
        true
    );
    assert!(
        !server.json["scopes_supported"]
            .as_array()
            .unwrap()
            .contains(&json!("workspace:manage"))
    );
    let resource = app
        .get("/.well-known/oauth-protected-resource/mcp")
        .send()
        .await;
    assert_eq!(resource.json["resource"], format!("{API}/mcp"));
    assert_eq!(resource.json["authorization_servers"], json!([API]));
}

/// The command-line client's whole login, as the CLI performs it: a device code, polls answered
/// `authorization_pending` then `slow_down`, the person's approval from the dashboard (refused
/// without the CSRF token), the tokens once, an `nbc_` token that calls the API and nothing else,
/// a refresh that rotates, and a replayed refresh token that revokes the whole grant at once.
#[tokio::test]
async fn the_cli_logs_in_with_the_device_flow() {
    let test = TestDb::new().await;
    let (workspace, session) = signed_in(&test).await;
    let app = test.app();
    let start = post_form(
        &app,
        "/oauth/device_authorization",
        &[("client_id", CLI), ("resource", API)],
    )
    .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.json);
    assert_eq!(start.json["expires_in"], 600);
    assert_eq!(start.json["interval"], 5);
    assert_eq!(
        start.json["verification_uri"],
        format!("{DASHBOARD}/activate")
    );
    let user_code = start.json["user_code"].as_str().unwrap().to_owned();
    assert_eq!(user_code.len(), 9);
    let device_code = start.json["device_code"].as_str().unwrap().to_owned();
    let poll = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code.as_str()),
        ("client_id", CLI),
    ];
    let pending = post_form(&app, "/oauth/token", &poll).await;
    assert_eq!(
        (pending.status, pending.json["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("authorization_pending"))
    );
    let fast = post_form(&app, "/oauth/token", &poll).await;
    assert_eq!(fast.json["error"], "slow_down");

    let details = app
        .get(&format!(
            "/oauth/consent?user_code={}",
            user_code.to_lowercase()
        ))
        .browser(&session)
        .send()
        .await;
    assert_eq!(details.status, StatusCode::OK, "{}", details.json);
    assert_eq!(details.json["client_name"], "Norbelys CLI");
    assert_eq!(details.json["user_code"], user_code);
    let decision = json!({ "user_code": user_code, "workspace_id": workspace.id.to_string(), "approve": true });
    let forged = app
        .post("/oauth/consent")
        .header("cookie", &session.cookie)
        .header("origin", DASHBOARD)
        .json(decision.clone())
        .send()
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let approved = app
        .post("/oauth/consent")
        .browser(&session)
        .json(decision.clone())
        .send()
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.json);
    assert_eq!(approved.json["approved"], true);
    let again = app
        .post("/oauth/consent")
        .browser(&session)
        .json(decision)
        .send()
        .await;
    assert_eq!(
        again.status,
        StatusCode::NOT_FOUND,
        "a decided code is decided once"
    );

    let tokens = post_form(&app, "/oauth/token", &poll).await;
    assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.json);
    assert_eq!(tokens.header("cache-control"), Some("no-store"));
    assert_eq!(tokens.json["token_type"], "Bearer");
    assert_eq!(tokens.json["expires_in"], 600);
    let access = tokens.json["access_token"].as_str().unwrap().to_owned();
    let refresh = tokens.json["refresh_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("nbc_"));
    let redeemed = post_form(&app, "/oauth/token", &poll).await;
    assert_eq!(redeemed.json["error"], "invalid_grant");

    let people = app.get("/v1/people").bearer(&access).send().await;
    assert_eq!(people.status, StatusCode::OK, "{}", people.json);
    let dashboard = app
        .get(&format!("/v1/workspaces/{}/members", workspace.id))
        .bearer(&access)
        .send()
        .await;
    assert_eq!(dashboard.status, StatusCode::FORBIDDEN);
    assert_eq!(dashboard.json["code"], "session_required");

    let refresh_with = |token: String| {
        let body = form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &token),
            ("client_id", CLI),
        ]);
        app.post("/oauth/token")
            .raw("application/x-www-form-urlencoded", body)
            .send()
    };
    let rotated = refresh_with(refresh.clone()).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.json);
    let newer_access = rotated.json["access_token"].as_str().unwrap().to_owned();
    let newer_refresh = rotated.json["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(newer_refresh, refresh);

    let replayed = refresh_with(refresh).await;
    assert_eq!(replayed.json["error"], "invalid_grant");
    let after = refresh_with(newer_refresh).await;
    assert_eq!(
        after.json["error"], "invalid_grant",
        "reuse revoked the whole chain"
    );
    let refused = app.get("/v1/people").bearer(&newer_access).send().await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "and its access tokens at once"
    );
}

/// Only the command-line client may start a device grant, only for the API, and a denied code
/// answers `access_denied`, so neither another client nor the person's refusal yields a token.
#[tokio::test]
async fn device_grants_are_the_clis_and_can_be_denied() {
    let test = TestDb::new().await;
    let (_, session) = signed_in(&test).await;
    register_mcp_client(&test).await;
    let app = test.app();
    let other = post_form(
        &app,
        "/oauth/device_authorization",
        &[("client_id", MCP_CLIENT), ("resource", API)],
    )
    .await;
    assert_eq!(other.json["error"], "unauthorized_client");
    let unknown = post_form(
        &app,
        "/oauth/device_authorization",
        &[("client_id", "nobody"), ("resource", API)],
    )
    .await;
    assert_eq!(
        (unknown.status, unknown.json["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_client"))
    );
    let wrong = post_form(
        &app,
        "/oauth/device_authorization",
        &[("client_id", CLI), ("resource", &format!("{API}/mcp"))],
    )
    .await;
    assert_eq!(wrong.json["error"], "invalid_target");
    let start = post_form(
        &app,
        "/oauth/device_authorization",
        &[("client_id", CLI), ("resource", API)],
    )
    .await;
    let denied = app
        .post("/oauth/consent")
        .browser(&session)
        .json(json!({ "user_code": start.json["user_code"], "approve": false }))
        .send()
        .await;
    assert_eq!(denied.json["approved"], false, "{}", denied.json);
    let poll = post_form(
        &app,
        "/oauth/token",
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", start.json["device_code"].as_str().unwrap()),
            ("client_id", CLI),
        ],
    )
    .await;
    assert_eq!(poll.json["error"], "access_denied");
}

/// The authorization endpoint redirects nothing until the client and its redirect URI are known,
/// then refuses a missing or `plain` PKCE challenge and a foreign resource back at the redirect
/// URI with `state` and `iss`, and hands an acceptable request to the dashboard's consent page.
#[tokio::test]
async fn authorization_requests_are_checked_before_consent() {
    let test = TestDb::new().await;
    register_mcp_client(&test).await;
    let app = test.app();
    let mcp = format!("{API}/mcp");
    let query = |pairs: &[(&str, &str)]| {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    };
    let base = [
        ("response_type", "code"),
        ("client_id", MCP_CLIENT),
        ("redirect_uri", REDIRECT),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("resource", mcp.as_str()),
        ("state", "xyz"),
    ];
    let with = |name: &str, value: &str| -> String {
        let pairs: Vec<(&str, &str)> = base
            .iter()
            .map(|(key, current)| (*key, if *key == name { value } else { *current }))
            .collect();
        format!("/oauth/authorize?{}", query(&pairs))
    };
    for (name, value) in [
        ("client_id", "nobody"),
        ("redirect_uri", "https://evil.test/cb"),
    ] {
        let reply = app.get(&with(name, value)).send().await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name}");
        assert!(reply.header("location").is_none(), "{name}");
    }
    for (name, value, error) in [
        ("code_challenge_method", "plain", "invalid_request"),
        ("code_challenge", "", "invalid_request"),
        ("resource", API, "invalid_target"),
        ("response_type", "token", "unsupported_response_type"),
    ] {
        let reply = app.get(&with(name, value)).send().await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{name}");
        let back = query_of(reply.header("location").unwrap());
        assert_eq!(back["error"], error, "{name}");
        assert_eq!((back["state"].as_str(), back["iss"].as_str()), ("xyz", API));
    }
    let accepted = app.get(&with("state", "xyz")).send().await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER);
    let location = accepted.header("location").unwrap();
    assert!(location.starts_with(&format!("{DASHBOARD}/oauth/consent?request=")));
}

/// Starts a code grant and returns the sealed consent request the dashboard receives.
async fn consent_request(app: &TestApp) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("response_type", "code"),
            ("client_id", MCP_CLIENT),
            ("redirect_uri", REDIRECT),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("resource", &format!("{API}/mcp")),
            ("scope", "people:read"),
            ("state", "s1"),
        ])
        .finish();
    let reply = app.get(&format!("/oauth/authorize?{query}")).send().await;
    query_of(reply.header("location").unwrap())["request"].clone()
}

/// The MCP client's code flow: the dashboard shows the client and its redirect host, approval
/// sends the browser back with a code, `state` and `iss`; the code is exchanged once, with the
/// right verifier only, for an `nbo_` token the API refuses; a replayed code revokes its grant;
/// a denial returns `access_denied`.
#[tokio::test]
async fn mcp_clients_use_the_code_flow_with_pkce() {
    let test = TestDb::new().await;
    let (workspace, session) = signed_in(&test).await;
    register_mcp_client(&test).await;
    let app = test.app();

    let request = consent_request(&app).await;
    let details = app
        .get(&format!("/oauth/consent?request={request}"))
        .browser(&session)
        .send()
        .await;
    assert_eq!(details.status, StatusCode::OK, "{}", details.json);
    assert_eq!(details.json["client_name"], "Test MCP client");
    assert_eq!(details.json["redirect_host"], "client.test");
    assert_eq!(details.json["scopes"], json!(["people:read"]));
    let approve = |request: &str| json!({ "request": request, "workspace_id": workspace.id.to_string(), "approve": true });
    let exchange = |code: &str, verifier: &str| {
        let body = form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", verifier),
            ("client_id", MCP_CLIENT),
        ]);
        app.post("/oauth/token")
            .raw("application/x-www-form-urlencoded", body)
            .send()
    };
    let code_of = |reply: &Reply| {
        let back = query_of(reply.json["redirect_to"].as_str().unwrap());
        assert_eq!((back["state"].as_str(), back["iss"].as_str()), ("s1", API));
        back["code"].clone()
    };

    let approved = app
        .post("/oauth/consent")
        .browser(&session)
        .json(approve(&request))
        .send()
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.json);
    let wrong = exchange(&code_of(&approved), &VERIFIER.replace('d', "e")).await;
    assert_eq!(wrong.json["error"], "invalid_grant");

    let approved = app
        .post("/oauth/consent")
        .browser(&session)
        .json(approve(&request))
        .send()
        .await;
    let code = code_of(&approved);
    let tokens = exchange(&code, VERIFIER).await;
    assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.json);
    assert_eq!(tokens.json["scope"], "people:read");
    let access = tokens.json["access_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("nbo_"));
    let api = app.get("/v1/people").bearer(&access).send().await;
    assert_eq!(
        api.status,
        StatusCode::UNAUTHORIZED,
        "the API refuses MCP tokens"
    );

    let replayed = exchange(&code, VERIFIER).await;
    assert_eq!(replayed.json["error"], "invalid_grant");
    let refresh = post_form(
        &app,
        "/oauth/token",
        &[
            ("grant_type", "refresh_token"),
            (
                "refresh_token",
                tokens.json["refresh_token"].as_str().unwrap(),
            ),
            ("client_id", MCP_CLIENT),
        ],
    )
    .await;
    assert_eq!(
        refresh.json["error"], "invalid_grant",
        "the replay revoked the grant"
    );

    let denied = app
        .post("/oauth/consent")
        .browser(&session)
        .json(json!({ "request": request, "approve": false }))
        .send()
        .await;
    let back = query_of(denied.json["redirect_to"].as_str().unwrap());
    assert_eq!(back["error"], "access_denied");
}

/// Consent is a dashboard mutation: a bearer credential is refused (`session_required`), and the
/// person must be an active member of the workspace they choose, so a program cannot consent and
/// nobody grants access to a workspace that is not theirs.
#[tokio::test]
async fn consent_needs_the_persons_own_workspace() {
    let test = TestDb::new().await;
    let (workspace, session) = signed_in(&test).await;
    let other = test.workspace("globex").await;
    register_mcp_client(&test).await;
    let app = test.app();
    let request = consent_request(&app).await;
    let bearer = app
        .post("/oauth/consent")
        .bearer(&workspace.key)
        .json(json!({ "request": request, "workspace_id": workspace.id.to_string(), "approve": true }))
        .send()
        .await;
    assert_eq!(bearer.json["code"], "session_required");
    let foreign = app
        .post("/oauth/consent")
        .browser(&session)
        .json(json!({ "request": request, "workspace_id": other.id.to_string(), "approve": true }))
        .send()
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    let tampered = app
        .post("/oauth/consent")
        .browser(&session)
        .json(json!({ "request": format!("{request}x"), "workspace_id": workspace.id.to_string(), "approve": true }))
        .send()
        .await;
    assert_eq!(tampered.status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Revocation (RFC 7009) by the client that holds the token ends the grant: its refresh token
/// stops refreshing and its access token stops authorizing at once; an unknown token is
/// answered `200` all the same, and the token endpoint refuses what is not a form.
#[tokio::test]
async fn revocation_ends_the_grant() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let workspace = test.workspace("acme").await;
    let (grant, access) = grant_token(&test, &workspace, Audience::Cli, ScopeSet::all()).await;
    let mut tx = test.app.begin().await.unwrap();
    let refresh = grants::issue_refresh(
        &mut tx,
        &keys(),
        grant,
        None,
        crate::process::now().plus(std::time::Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let app = test.app();
    assert_eq!(
        app.get("/v1/people").bearer(&access).send().await.status,
        StatusCode::OK
    );
    let unknown = post_form(
        &app,
        "/oauth/revoke",
        &[("token", "nonsense"), ("client_id", CLI)],
    )
    .await;
    assert_eq!(unknown.status, StatusCode::OK);
    let revoked = post_form(
        &app,
        "/oauth/revoke",
        &[("token", &refresh), ("client_id", CLI)],
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK);
    let refused = post_form(
        &app,
        "/oauth/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", CLI),
        ],
    )
    .await;
    assert_eq!(refused.json["error"], "invalid_grant");
    assert_eq!(
        app.get("/v1/people").bearer(&access).send().await.status,
        StatusCode::UNAUTHORIZED
    );
    let not_form = app
        .post("/oauth/token")
        .json(json!({ "grant_type": "refresh_token" }))
        .send()
        .await;
    assert_eq!(not_form.json["error"], "invalid_request");
    let unsupported = post_form(
        &app,
        "/oauth/token",
        &[("grant_type", "password"), ("client_id", CLI)],
    )
    .await;
    assert_eq!(unsupported.json["error"], "unsupported_grant_type");
}
