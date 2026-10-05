//! What an operator's impersonation session may not make: anything that outlives it.

use axum::http::StatusCode;
use serde_json::json;

use super::mint;
use crate::domain::email::EmailAddress;
use crate::identity::oauth::clients::CLI;
use crate::identity::oauth::tests::API;
use crate::identity::recovery;
use crate::testing::{self, Reply, TestDb, TestSession};

/// Asserts that `reply` is the refusal of an impersonation, not some other failure.
fn refused(reply: &Reply, what: &str) {
    assert_eq!(
        reply.status,
        StatusCode::FORBIDDEN,
        "{what}: {}",
        reply.json
    );
    assert_eq!(reply.json["code"], "forbidden", "{what}: {}", reply.json);
    assert!(
        reply.json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("impersonation")),
        "{what}: {}",
        reply.json
    );
}

/// An operator's impersonation acts in the person's name for ten minutes, so it is refused every
/// credential that would outlive it: an application's grant (a device approval), an API key made
/// with a workspace token it minted, recovery codes, a passkey registration and an identity link.
/// It still mints the short workspace token that lets support see what the person sees. Without
/// these refusals an operator could leave with lasting access in the person's name, outside the
/// audit trail of the impersonation.
#[tokio::test]
async fn an_impersonation_makes_nothing_that_outlives_it() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let person = EmailAddress::parse("owner@acme.example").unwrap();
    let mut tx = test.system.begin().await.unwrap();
    let opened = recovery::impersonate(&mut tx, &testing::keys(), &person, "Ticket 4712: support")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let session = TestSession {
        user: opened.user,
        session: opened.session,
        cookie: opened.cookie,
        csrf: opened.csrf_token,
    };
    let app = test.app();

    let device = app
        .post("/oauth/device_authorization")
        .raw(
            "application/x-www-form-urlencoded",
            format!("client_id={CLI}&resource={API}"),
        )
        .send()
        .await;
    assert_eq!(device.status, StatusCode::OK, "{}", device.json);
    let approved = app
        .post("/oauth/consent")
        .browser(&session)
        .json(json!({
            "user_code": device.json["user_code"],
            "workspace_id": acme.id.to_string(),
            "approve": true,
        }))
        .send()
        .await;
    refused(&approved, "a device approval");

    let token = mint(&app, &session, acme.id).await;
    let key = app
        .post(&format!("/v1/workspaces/{}/api_keys", acme.id))
        .bearer(&token)
        .idempotency("key")
        .json(json!({ "name": "Support" }))
        .send()
        .await;
    refused(&key, "an API key");

    let codes = app
        .post("/v1/me/recovery_codes")
        .browser(&session)
        .idempotency("codes")
        .send()
        .await;
    refused(&codes, "recovery codes");

    let passkey = app
        .post("/v1/auth/challenges")
        .browser(&session)
        .json(json!({ "method": "passkey_registration" }))
        .send()
        .await;
    refused(&passkey, "a passkey registration");

    let link = app
        .post("/v1/auth/challenges")
        .browser(&session)
        .json(json!({ "method": "oidc", "provider": "google", "link": true }))
        .send()
        .await;
    refused(&link, "an identity link");

    let lasting: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM oauth_grants WHERE user_id = $1)
              + (SELECT count(*) FROM recovery_codes WHERE user_id = $1)
              + (SELECT count(*) FROM passkeys WHERE user_id = $1)
              + (SELECT count(*) FROM identity_links WHERE user_id = $1)
              + (SELECT count(*) FROM api_keys WHERE created_by = $1 AND name = 'Support')",
    )
    .bind(opened.user.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(lasting, 0, "nothing lasting was made");
}
