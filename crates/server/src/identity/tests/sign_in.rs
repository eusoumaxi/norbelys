//! Sign-in end to end, in process: email codes and links, passkeys, OpenID Connect, the
//! login-CSRF guard, the rate limits and the captcha, and the workspace tokens minted from a
//! session.

use axum::http::StatusCode;
use secrecy::SecretString;
use serde_json::{Value, json};

use super::{
    Authenticator, FakeIdp, client_address, identity_with, mail_to, mint, parameter, set_cookie,
    sign_in, signed_in,
};
use crate::identity::Identity;
use crate::identity::captcha::{Captcha, Provider};
use crate::testing::{DASHBOARD, Reply, Sink, TestApp, TestDb};

/// Starts an email-code challenge for `email` from the client `address`.
async fn challenge(app: &TestApp, email: &str, address: &str) -> Reply {
    app.post("/v1/auth/challenges")
        .dashboard()
        .header("x-forwarded-for", address)
        .json(json!({ "method": "email_code", "email": email }))
        .send()
        .await
}

/// Finishes a sign-in with `body`, from the browser holding `cookie` (if any) at `address`.
async fn finish(app: &TestApp, body: Value, cookie: Option<&str>, address: &str) -> Reply {
    let mut call = app
        .post("/v1/auth/sessions")
        .dashboard()
        .header("x-forwarded-for", address);
    if let Some(cookie) = cookie {
        call = call.header("cookie", cookie);
    }
    call.json(body).send().await
}

/// An email code is the way in and the way to sign up: the challenge answers `201` and sends the
/// code and a link; the code, from the same browser, creates the user and issues a fresh
/// `__Host-` session cookie (`HttpOnly`, `Secure`, `SameSite=Lax`, no `Domain`) with its CSRF
/// token, which `GET /me` then accepts; and the same code never works twice.
#[tokio::test]
async fn an_email_code_signs_a_new_user_up_and_in() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    let address = client_address();
    let started = challenge(&app, "Ada@Example.com", &address).await;
    assert_eq!(started.status, StatusCode::CREATED, "{}", started.json);
    assert_eq!(started.json["method"], "email_code");
    assert!(started.header("ratelimit").is_some());
    let ceremony = set_cookie(&started, "__Host-nb_ceremony").unwrap();
    let (code, _) = mail_to(&test, "Ada@Example.com").await;
    let body = json!({ "challenge_id": started.json["id"], "code": code });

    let reply = finish(&app, body.clone(), Some(&ceremony), &address).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    assert_eq!(reply.json["user"]["email"], "Ada@Example.com");
    assert!(reply.json["user"]["email_verified_at"].is_string());
    assert_eq!(reply.json["session"]["auth_method"], "email_code");
    let cookie = reply
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__Host-nb_session="))
        .unwrap()
        .to_owned();
    for attribute in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax"] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    assert!(!cookie.contains("Domain"), "{cookie}");
    let browser = signed_in(&reply);
    let me = app.get("/v1/me").browser(&browser).send().await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.json);
    assert_eq!(me.json["csrf_token"], browser.csrf.as_str());
    assert_eq!(me.json["sessions"].as_array().unwrap().len(), 1);
    assert!(me.header("etag").is_some());

    let again = finish(&app, body, Some(&ceremony), &address).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
}

/// The 6-digit code is bound to the browser that asked for it (another browser's cookie proves
/// nothing and costs no attempt) and allows five attempts: after five wrong codes even the right
/// one is refused, so a code cannot be guessed.
#[tokio::test]
async fn a_code_works_only_in_its_browser_and_locks_after_five_wrong_tries() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    let address = client_address();
    let started = challenge(&app, "grace@example.com", &address).await;
    let ceremony = set_cookie(&started, "__Host-nb_ceremony").unwrap();
    let (code, _) = mail_to(&test, "grace@example.com").await;
    let with = |code: &str| json!({ "challenge_id": started.json["id"], "code": code });

    let elsewhere = finish(
        &app,
        with(&code),
        Some("__Host-nb_ceremony=another-browser"),
        &address,
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::UNAUTHORIZED);
    let wrong = if code == "000000" { "999999" } else { "000000" };
    for _ in 0..5 {
        let reply = finish(&app, with(wrong), Some(&ceremony), &address).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.json["code"], "unauthorized");
    }
    let locked = finish(&app, with(&code), Some(&ceremony), &address).await;
    assert_eq!(locked.status, StatusCode::UNAUTHORIZED);
    let attempts: i16 =
        sqlx::query_scalar("SELECT attempts FROM login_codes WHERE ceremony_id = $1")
            .bind(
                started.json["id"]
                    .as_str()
                    .unwrap()
                    .parse::<crate::domain::ids::Id<crate::domain::ids::Challenge>>()
                    .unwrap()
                    .uuid(),
            )
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(attempts, 5);
}

/// The link works in any browser, but only for the address the code was sent to: a link carried
/// to a page naming another account signs nobody in and stays usable; the right address signs in
/// once, and consumes the code with it.
#[tokio::test]
async fn a_link_signs_any_browser_in_only_to_the_account_it_names() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    let address = client_address();
    let started = challenge(&app, "linus@example.com", &address).await;
    let ceremony = set_cookie(&started, "__Host-nb_ceremony").unwrap();
    let (code, token) = mail_to(&test, "linus@example.com").await;

    let named_wrong = finish(
        &app,
        json!({ "token": token, "email": "eve@example.com" }),
        None,
        &address,
    )
    .await;
    assert_eq!(named_wrong.status, StatusCode::UNAUTHORIZED);
    let link = json!({ "token": token, "email": "linus@example.com" });
    let reply = finish(&app, link.clone(), None, &address).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    assert_eq!(reply.json["user"]["email"], "linus@example.com");
    assert_eq!(
        finish(&app, link, None, &address).await.status,
        StatusCode::UNAUTHORIZED
    );
    let code_after = finish(
        &app,
        json!({ "challenge_id": started.json["id"], "code": code }),
        Some(&ceremony),
        &address,
    )
    .await;
    assert_eq!(code_after.status, StatusCode::UNAUTHORIZED);
}

/// The anonymous sign-in calls are guarded against login CSRF: only the dashboard's `Origin` with
/// the custom header is accepted, and a program's bearer credential is refused with
/// `session_required`, so neither a third-party page nor an API key can drive a sign-in.
#[tokio::test]
async fn sign_in_calls_come_only_from_the_dashboard() {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let app = test.app();
    let body = json!({ "method": "passkey" });
    let bare = app
        .post("/v1/auth/challenges")
        .json(body.clone())
        .send()
        .await;
    assert_eq!(bare.status, StatusCode::FORBIDDEN);
    let foreign = app
        .post("/v1/auth/challenges")
        .header("origin", "https://evil.test")
        .header("x-csrf-token", "1")
        .json(body.clone())
        .send()
        .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    let no_header = app
        .post("/v1/auth/challenges")
        .header("origin", DASHBOARD)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(no_header.status, StatusCode::FORBIDDEN);
    let program = app
        .post("/v1/auth/challenges")
        .dashboard()
        .bearer(&workspace.key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(program.status, StatusCode::FORBIDDEN);
    assert_eq!(program.json["code"], "session_required");
    let dashboard = app
        .post("/v1/auth/challenges")
        .dashboard()
        .json(body)
        .send()
        .await;
    assert_eq!(dashboard.status, StatusCode::CREATED, "{}", dashboard.json);
}

/// Email-code challenges are limited to five per 15 minutes per address and per client, and
/// sign-in finishes to ten per 15 minutes per client; past a budget the answer is `429` with
/// `Retry-After`, and other addresses and clients keep their own budgets.
#[tokio::test]
async fn sign_in_is_rate_limited_per_address_and_per_client() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    for _ in 0..5 {
        let reply = challenge(&app, "zed@example.com", &client_address()).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    }
    let sixth = challenge(&app, "zed@example.com", &client_address()).await;
    assert_eq!(sixth.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(sixth.json["code"], "rate_limited");
    assert!(sixth.header("retry-after").is_some());

    let address = client_address();
    for n in 0..5 {
        let reply = challenge(&app, &format!("u{n}@example.com"), &address).await;
        assert_eq!(reply.status, StatusCode::CREATED);
    }
    assert_eq!(
        challenge(&app, "u5@example.com", &address).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    let address = client_address();
    let guess = json!({ "token": "guess", "email": "zed@example.com" });
    for _ in 0..10 {
        let reply = finish(&app, guess.clone(), None, &address).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let eleventh = finish(&app, guess, None, &address).await;
    assert_eq!(eleventh.status, StatusCode::TOO_MANY_REQUESTS);
}

/// Without a transactional sender the code cannot be delivered, so the challenge answers `503`
/// (the same for every address) instead of pretending it sent anything.
#[tokio::test]
async fn without_a_transactional_sender_codes_are_unavailable() {
    let test = TestDb::new().await;
    let app = test.app();
    let reply = challenge(&app, "ada@example.com", &client_address()).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
}

/// With a captcha configured, `GET /auth/config` names it for the dashboard's widget, a challenge
/// without a token is `422 captcha_failed`, and an unreachable provider lets the challenge through
/// under the tighter budget of one per address per 15 minutes.
#[tokio::test]
async fn the_captcha_gates_the_email_code_challenge() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let sink = Sink::start().await;
    let captcha = Captcha::new(
        Some(Provider::Turnstile),
        Some("site-key".to_owned()),
        Some(SecretString::from("secret")),
        None,
        vec!["app.norbelys.test".to_owned()],
    )
    .unwrap()
    .unwrap()
    .at(url::Url::parse(&sink.url("/503")).unwrap());
    let app = test.app_with_identity(Identity {
        captcha: Some(captcha),
        ..Identity::for_tests()
    });
    let config = app.get("/v1/auth/config").send().await;
    assert_eq!(config.json["captcha"]["provider"], "turnstile");
    assert_eq!(config.json["captcha"]["site_key"], "site-key");

    let missing = challenge(&app, "ada@example.com", &client_address()).await;
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(missing.json["code"], "captcha_failed");
    let with_token = |address: String| {
        let app = &app;
        async move {
            app.post("/v1/auth/challenges")
                .dashboard()
                .header("x-forwarded-for", &address)
                .json(json!({ "method": "email_code", "email": "ada@example.com", "captcha_token": "t" }))
                .send()
                .await
        }
    };
    let degraded = with_token(client_address()).await;
    assert_eq!(degraded.status, StatusCode::CREATED, "{}", degraded.json);
    let second = with_token(client_address()).await;
    assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
}

/// A passkey is registered from a signed-in session and then signs its user in with no email at
/// all: the authenticator names the user, the assertion is verified against the stored
/// credential, and the ceremony is spent by its finish.
#[tokio::test]
async fn a_passkey_registers_from_a_session_and_signs_in_without_an_email() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    let browser = sign_in(&test, &app, "hopper@example.com").await;
    let mut authenticator = Authenticator::new();

    let start = app
        .post("/v1/auth/challenges")
        .browser(&browser)
        .json(json!({ "method": "passkey_registration" }))
        .send()
        .await;
    assert_eq!(start.status, StatusCode::CREATED, "{}", start.json);
    let ceremony = set_cookie(&start, "__Host-nb_ceremony").unwrap();
    let created = app
        .post("/v1/me/passkeys")
        .browser(&browser)
        .header("cookie", &ceremony)
        .idempotency("register-laptop")
        .json(json!({
            "challenge_id": start.json["id"],
            "credential": authenticator.register(&start.json["options"]),
            "name": "Laptop",
        }))
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["name"], "Laptop");
    let me = app.get("/v1/me").browser(&browser).send().await;
    assert_eq!(me.json["passkeys"].as_array().unwrap().len(), 1);

    let begin = app
        .post("/v1/auth/challenges")
        .dashboard()
        .json(json!({ "method": "passkey" }))
        .send()
        .await;
    assert_eq!(begin.status, StatusCode::CREATED, "{}", begin.json);
    let ceremony = set_cookie(&begin, "__Host-nb_ceremony").unwrap();
    let assertion = authenticator.assert(&begin.json["options"], browser.user.uuid());
    let body = json!({ "challenge_id": begin.json["id"], "credential": assertion });
    let address = client_address();
    let reply = finish(&app, body.clone(), Some(&ceremony), &address).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    assert_eq!(reply.json["session"]["auth_method"], "passkey");
    assert_eq!(reply.json["user"]["id"], browser.user.to_string());
    let spent = finish(&app, body, Some(&ceremony), &address).await;
    assert_eq!(spent.status, StatusCode::UNAUTHORIZED);
}

/// A workspace token proves nothing once its session ends: signing out (`DELETE
/// /me/sessions/current`) clears the cookie and the very next request with the token is `401`
/// on this process, without waiting for the cache.
#[tokio::test]
async fn a_workspace_token_dies_with_its_session() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let workspace = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let app = test.app();
    let token = mint(&app, &owner, workspace.id).await;
    let path = format!("/v1/workspaces/{}", workspace.id);
    assert_eq!(
        app.get(&path).bearer(&token).send().await.status,
        StatusCode::OK
    );

    let signed_out = app
        .delete("/v1/me/sessions/current")
        .browser(&owner)
        .send()
        .await;
    assert_eq!(signed_out.status, StatusCode::NO_CONTENT);
    assert!(
        signed_out
            .headers
            .get_all("set-cookie")
            .iter()
            .any(|value| value.to_str().unwrap().starts_with("__Host-nb_session=;"))
    );
    assert_eq!(
        app.get(&path).bearer(&token).send().await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.get("/v1/me").browser(&owner).send().await.status,
        StatusCode::UNAUTHORIZED
    );
}

/// Minting a workspace token is a cookie-authorised change: it needs the session's CSRF token,
/// refuses a bearer credential with `session_required`, and mints only for a workspace the user is
/// an active member of; the token carries the role's scopes and the `nbs_` prefix.
#[tokio::test]
async fn minting_needs_the_csrf_token_and_a_membership() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let owner = test.session("owner@acme.example").await;
    let app = test.app();
    let body = json!({ "workspace_id": acme.id.to_string() });
    let no_csrf = app
        .post("/v1/auth/tokens")
        .header("cookie", &owner.cookie)
        .header("origin", DASHBOARD)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    let program = app
        .post("/v1/auth/tokens")
        .browser(&owner)
        .bearer(&acme.key)
        .json(body.clone())
        .send()
        .await;
    assert_eq!(program.json["code"], "session_required");
    let foreign = app
        .post("/v1/auth/tokens")
        .browser(&owner)
        .json(json!({ "workspace_id": globex.id.to_string() }))
        .send()
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    let minted = app
        .post("/v1/auth/tokens")
        .browser(&owner)
        .json(body)
        .send()
        .await;
    assert_eq!(minted.status, StatusCode::CREATED, "{}", minted.json);
    assert!(minted.json["token"].as_str().unwrap().starts_with("nbs_"));
    assert_eq!(minted.json["role"], "owner");
    assert!(
        minted.json["scopes"]
            .as_array()
            .unwrap()
            .contains(&json!("workspace:manage"))
    );
}

/// Starts an `oidc` challenge with `body`: its id, the ceremony cookie and the provider's
/// authorization URL.
async fn oidc(
    app: &TestApp,
    body: Value,
    browser: Option<&crate::testing::TestSession>,
) -> (String, String, String) {
    let call = match browser {
        Some(browser) => app.post("/v1/auth/challenges").browser(browser),
        None => app.post("/v1/auth/challenges").dashboard(),
    };
    let reply = call.json(body).send().await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    (
        reply.json["id"].as_str().unwrap().to_owned(),
        set_cookie(&reply, "__Host-nb_ceremony").unwrap(),
        reply.json["authorization_url"].as_str().unwrap().to_owned(),
    )
}

/// Comes back from the provider to the callback with `cookie`.
async fn callback(app: &TestApp, state: &str, cookie: &str) -> Reply {
    app.get(&format!("/v1/auth/callback?state={state}&code=the-code"))
        .header("cookie", cookie)
        .send()
        .await
}

/// OpenID Connect signs people in and never merges accounts by itself: a verified address nobody
/// holds creates the user, the same `(issuer, subject)` finds them again, an unverified address
/// creates nobody, and a verified address someone holds is refused until that person links the
/// identity from their own session, after which it signs them in.
#[tokio::test]
async fn openid_connect_signs_people_in_without_silent_merges() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    let config = app.get("/v1/auth/config").send().await;
    assert_eq!(config.json["oidc_providers"], json!(["test"]));

    let sign_in_with = |subject: &'static str, email: &'static str, verified: bool| {
        let (app, idp) = (&app, &idp);
        async move {
            let (id, ceremony, url) = oidc(
                app,
                json!({ "method": "oidc", "provider": "test", "return_to": "/home" }),
                None,
            )
            .await;
            assert_eq!(parameter(&url, "state"), id);
            assert_eq!(parameter(&url, "code_challenge_method"), "S256");
            idp.issue(
                subject,
                Some(email),
                verified,
                &parameter(&url, "nonce"),
                None,
            );
            callback(app, &id, &ceremony).await
        }
    };
    let first = sign_in_with("subject-1", "turing@example.com", true).await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert_eq!(first.header("location"), Some("/home?signed_in=true"));
    let cookie = set_cookie(&first, "__Host-nb_session").unwrap();
    let again = sign_in_with("subject-1", "turing@example.com", true).await;
    assert!(set_cookie(&again, "__Host-nb_session").is_some());
    let users: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email_key = 'turing@example.com'")
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(users, 1);
    assert!(!cookie.is_empty());

    let unverified = sign_in_with("subject-2", "anon@example.com", false).await;
    assert!(
        unverified
            .header("location")
            .unwrap()
            .contains("error=unverified_email")
    );

    let holder = sign_in(&test, &app, "lamport@example.com").await;
    let held = sign_in_with("subject-3", "lamport@example.com", true).await;
    assert!(
        held.header("location")
            .unwrap()
            .contains("error=needs_link")
    );

    let (id, ceremony, url) = oidc(
        &app,
        json!({ "method": "oidc", "provider": "test", "link": true, "return_to": "/settings" }),
        Some(&holder),
    )
    .await;
    idp.issue(
        "subject-3",
        Some("lamport@example.com"),
        true,
        &parameter(&url, "nonce"),
        None,
    );
    let linked = callback(&app, &id, &format!("{}; {ceremony}", holder.cookie)).await;
    assert_eq!(linked.header("location"), Some("/settings?linked=true"));
    let after = sign_in_with("subject-3", "lamport@example.com", true).await;
    assert_eq!(after.header("location"), Some("/home?signed_in=true"));
    let session = crate::testing::TestSession {
        cookie: set_cookie(&after, "__Host-nb_session").unwrap(),
        ..holder.clone()
    };
    let me = app
        .get("/v1/me")
        .header("cookie", &session.cookie)
        .send()
        .await;
    assert_eq!(me.json["id"], holder.user.to_string());
    assert_eq!(me.json["identities"].as_array().unwrap().len(), 1);
}

/// The callback finishes a ceremony only in the browser that started it: without the ceremony
/// cookie, or with another browser's, it is refused and the ceremony stays usable for its own
/// browser.
#[tokio::test]
async fn the_callback_finishes_only_in_the_browser_that_started() {
    let test = TestDb::new().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    let (id, ceremony, url) =
        oidc(&app, json!({ "method": "oidc", "provider": "test" }), None).await;
    let without = app
        .get(&format!("/v1/auth/callback?state={id}&code=the-code"))
        .send()
        .await;
    assert_eq!(without.status, StatusCode::BAD_REQUEST);
    let elsewhere = callback(&app, &id, "__Host-nb_ceremony=another-browser").await;
    assert_eq!(elsewhere.status, StatusCode::BAD_REQUEST);
    idp.issue(
        "subject-9",
        Some("knuth@example.com"),
        true,
        &parameter(&url, "nonce"),
        None,
    );
    let own = callback(&app, &id, &ceremony).await;
    assert_eq!(own.status, StatusCode::SEE_OTHER);
    assert_eq!(own.header("location"), Some("/?signed_in=true"));
}

/// A session stops proving its user at its idle expiry and at its absolute expiry, whatever the
/// cookie says; and a user keeps at most 50 live sessions: the 51st sign-in ends the oldest, which
/// then proves nothing.
#[tokio::test]
async fn sessions_end_at_their_expiries_and_beyond_fifty() {
    let test = TestDb::new().await;
    let app = test.app();
    let idle = test.session("ada@example.com").await;
    let absolute = test.session("ada@example.com").await;
    let ended = [
        (
            "UPDATE sessions SET idle_expires_at = now() - interval '1 second' WHERE id = $1",
            &idle,
        ),
        (
            "UPDATE sessions SET expires_at = now() - interval '1 second' WHERE id = $1",
            &absolute,
        ),
    ];
    for (column, browser) in ended {
        sqlx::query(column)
            .bind(browser.session.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
        let reply = app.get("/v1/me").browser(browser).send().await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{column}");
    }

    let oldest = test.session("grace@example.com").await;
    for _ in 0..50 {
        test.session("grace@example.com").await;
    }
    let (reason, live): (Option<String>, i64) = sqlx::query_as(
        "SELECT (SELECT revoked_reason FROM sessions WHERE id = $1),
                (SELECT count(*) FROM sessions WHERE user_id = $2 AND revoked_at IS NULL)",
    )
    .bind(oldest.session.uuid())
    .bind(oldest.user.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!((reason.as_deref(), live), (Some("replaced"), 50));
    assert_eq!(
        app.get("/v1/me").browser(&oldest).send().await.status,
        StatusCode::UNAUTHORIZED
    );
}

/// A code and its link expire with their ceremony: past the ten minutes neither signs anyone in.
#[tokio::test]
async fn codes_and_links_expire() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    let address = client_address();
    let started = challenge(&app, "late@example.com", &address).await;
    let ceremony = set_cookie(&started, "__Host-nb_ceremony").unwrap();
    let (code, token) = mail_to(&test, "late@example.com").await;
    sqlx::query(
        "UPDATE login_codes SET expires_at = now() - interval '1 second' WHERE email_key = 'late@example.com'",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE auth_ceremonies SET expires_at = now() - interval '1 second'")
        .execute(test.system.pool())
        .await
        .unwrap();
    let by_code = finish(
        &app,
        json!({ "challenge_id": started.json["id"], "code": code }),
        Some(&ceremony),
        &address,
    )
    .await;
    assert_eq!(by_code.status, StatusCode::UNAUTHORIZED);
    let by_link = finish(
        &app,
        json!({ "token": token, "email": "late@example.com" }),
        None,
        &address,
    )
    .await;
    assert_eq!(by_link.status, StatusCode::UNAUTHORIZED);
}

/// Signs in through the fake provider as `subject`, whose verified address is `email`: the
/// callback's answer.
async fn oidc_sign_in(app: &TestApp, idp: &FakeIdp, subject: &str, email: &str) -> Reply {
    let (id, ceremony, url) =
        oidc(app, json!({ "method": "oidc", "provider": "test" }), None).await;
    idp.issue(subject, Some(email), true, &parameter(&url, "nonce"), None);
    callback(app, &id, &ceremony).await
}

/// The welcomes queued for `email`: each one's text and how long it stays useful, in hours.
async fn welcomes(test: &TestDb, email: &str) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT m.text_body, round(extract(epoch FROM q.expires_at - now()) / 3600)::bigint
           FROM messages m JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
          WHERE m.workspace_id = $1 AND m.kind = 'transactional' AND m.to_addresses[1] = $2
            AND m.subject = 'Your Norbelys account is ready'",
    )
    .bind(crate::jobs::SYSTEM_WORKSPACE.uuid())
    .bind(email)
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// The sign-in that creates an account welcomes its person once: the first email-code finish
/// queues the welcome, with the dashboard's link, useful for three days, and signing in again
/// queues no second one.
#[tokio::test]
async fn the_first_email_code_sign_in_welcomes_the_new_user_once() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let app = test.app();
    sign_in(&test, &app, "ada@example.com").await;
    sign_in(&test, &app, "ada@example.com").await;
    let welcomes = welcomes(&test, "ada@example.com").await;
    let [(text, hours)] = &welcomes[..] else {
        panic!("one welcome: {welcomes:?}");
    };
    assert!(
        text.contains(&format!("Open your dashboard:\n{DASHBOARD}/\n")),
        "{text}"
    );
    assert_eq!(*hours, 72);
}

/// An OpenID Connect sign-in that creates the account welcomes its person too, once: the same
/// identity signing in again queues no second welcome.
#[tokio::test]
async fn the_first_openid_connect_sign_in_welcomes_the_new_user_once() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    for _ in 0..2 {
        let reply = oidc_sign_in(&app, &idp, "subject-1", "turing@example.com").await;
        assert_eq!(reply.header("location"), Some("/?signed_in=true"));
    }
    assert_eq!(welcomes(&test, "turing@example.com").await.len(), 1);
}

/// A missing transactional sender never stops a sign-up: the person whose first sign-in creates
/// their account (here through OpenID Connect, which needs no mail) is signed in, and only the
/// welcome is skipped.
#[tokio::test]
async fn a_sign_up_without_a_transactional_sender_skips_only_the_welcome() {
    let test = TestDb::new().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    let reply = oidc_sign_in(&app, &idp, "subject-1", "turing@example.com").await;
    assert_eq!(reply.header("location"), Some("/?signed_in=true"));
    assert!(set_cookie(&reply, "__Host-nb_session").is_some());
    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(messages, 0);
}
