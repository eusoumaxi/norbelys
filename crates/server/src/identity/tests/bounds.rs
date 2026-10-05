//! The bounds of what a person's account holds, enforced on write: OAuth grants (at most 50, the
//! oldest ending) and linked identities (at most 10, refused beyond).

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::domain::identity::{AuthMethod, Proof};
use crate::domain::oauth::Resources;
use crate::domain::scope::ScopeSet;
use crate::identity::oauth::grants::{self, MAX_GRANTS, NewGrant};
use crate::identity::oauth::tests::{API, MCP_CLIENT, register_mcp_client};
use crate::testing::{TestDb, keys};

/// The live grants of `user`.
async fn live(test: &TestDb, user: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM oauth_grants WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user)
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// A consent beyond a person's 50 live grants ends the oldest, with its refresh chain, and records
/// the end in that grant's workspace's audit log, so `GET /v1/me` always lists every grant and a
/// forgotten application's access does not pile up for ever.
#[tokio::test]
async fn a_grant_beyond_fifty_ends_the_oldest() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    register_mcp_client(&test).await;
    let resource = Resources::of(&url::Url::parse(API).unwrap()).mcp;
    let grant = || NewGrant {
        client_id: MCP_CLIENT,
        user: acme.owner,
        workspace: acme.id,
        resource: &resource,
        scopes: ScopeSet::all(),
        proof: Proof {
            method: AuthMethod::EmailCode,
            connection: None,
            policy_version: None,
            authenticated_at: crate::process::now().0,
        },
    };
    let mut tx = test.app.begin().await.unwrap();
    let oldest = grants::create(&mut tx, &grant()).await.unwrap();
    let _refresh = grants::issue_refresh(
        &mut tx,
        &keys(),
        oldest,
        None,
        crate::process::now().plus(Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    for _ in 1..MAX_GRANTS {
        grants::create(&mut tx, &grant()).await.unwrap();
    }
    tx.commit().await.unwrap();
    assert_eq!(
        live(&test, acme.owner.uuid()).await,
        MAX_GRANTS,
        "fifty live grants end nothing"
    );

    let mut tx = test.app.begin().await.unwrap();
    let newest = grants::create(&mut tx, &grant()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live(&test, acme.owner.uuid()).await, MAX_GRANTS);
    let (oldest_revoked, newest_live, refresh_revoked, audited): (bool, bool, bool, i64) =
        sqlx::query_as(
            "SELECT (SELECT revoked_at IS NOT NULL FROM oauth_grants WHERE id = $1),
                    (SELECT revoked_at IS NULL FROM oauth_grants WHERE id = $2),
                    (SELECT bool_and(revoked_at IS NOT NULL) FROM oauth_refresh_tokens WHERE grant_id = $1),
                    (SELECT count(*) FROM audit_log WHERE workspace_id = $3 AND action = 'grant.revoked'
                        AND target = $4 AND details ->> 'reason' = 'replaced')",
        )
        .bind(oldest.uuid())
        .bind(newest.uuid())
        .bind(acme.id.uuid())
        .bind(oldest.to_string())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert!(oldest_revoked && newest_live && refresh_revoked);
    assert_eq!(audited, 1);
}

/// A person with 10 linked identities is refused an eleventh link at its start with
/// `409 invalid_state`, before any provider is asked, so the account's list stays within the
/// bound `GET /v1/me` declares.
#[tokio::test]
async fn an_eleventh_identity_is_refused() {
    let test = TestDb::new().await;
    let app = test.app();
    let session = test.session("ada@example.com").await;
    for n in 0..10 {
        sqlx::query(
            "INSERT INTO identity_links (issuer, subject, user_id) VALUES ('https://idp.example', $1, $2)",
        )
        .bind(format!("subject-{n}"))
        .bind(session.user.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    }
    let link = app
        .post("/v1/auth/challenges")
        .browser(&session)
        .json(json!({ "method": "oidc", "provider": "google", "link": true }))
        .send()
        .await;
    assert_eq!(link.status, StatusCode::CONFLICT, "{}", link.json);
    assert_eq!(link.json["code"], "invalid_state");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM identity_links WHERE user_id = $1")
        .bind(session.user.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(count, 10);
}
