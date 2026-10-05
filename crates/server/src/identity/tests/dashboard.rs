//! The dashboard surface in process: its credential rules (`session_required`, the workspace in
//! the path), workspaces, members, invitations, API keys, the account and the audit log; and, at
//! the store level, the races its rules must survive.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::{id_at, mint, sign_in};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Membership, Workspace, WorkspaceId};
use crate::domain::scope::MembershipRole;
use crate::http::versioning::IfMatch;
use crate::identity::invitations::{self, InvitationError};
use crate::identity::memberships::{self, Change, MemberError};
use crate::testing::{TestApp, TestDb, TestSession, keys};

/// Adds the user holding `email` to `workspace` with `role` (as an accepted invitation would) and
/// signs them in: their browser and their membership.
async fn member(
    test: &TestDb,
    workspace: WorkspaceId,
    email: &str,
    role: &str,
) -> (TestSession, Id<Membership>) {
    let session = test.session(email).await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(workspace.uuid())
    .bind(session.user.uuid())
    .bind(role)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    (session, Id::from_uuid(id))
}

/// The membership of `user` in `workspace`.
async fn membership_of(test: &TestDb, workspace: WorkspaceId, user: Uuid) -> Id<Membership> {
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(workspace.uuid())
            .bind(user)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    Id::from_uuid(id)
}

/// The token of the latest mail to `email` (an invitation's link).
async fn latest_token(test: &TestDb, email: &str) -> String {
    let body: String = sqlx::query_scalar(
        "SELECT text_body FROM messages WHERE workspace_id = $1 AND to_addresses[1] = $2
          ORDER BY id DESC LIMIT 1",
    )
    .bind(crate::jobs::SYSTEM_WORKSPACE.uuid())
    .bind(email)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    body.split("token=")
        .nth(1)
        .and_then(|rest| rest.split(['&', '\n', ' ']).next())
        .unwrap()
        .to_owned()
}

/// The actions of `workspace`'s audit log, newest first.
async fn audited(app: &TestApp, token: &str, workspace: WorkspaceId) -> Vec<String> {
    let reply = app
        .get(&format!("/v1/workspaces/{workspace}/audit_log?limit=100"))
        .bearer(token)
        .send()
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    reply.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["action"].as_str().unwrap().to_owned())
        .collect()
}

/// No credential a program holds reaches the dashboard surface: an API key gets `403
/// session_required` on the workspace's members, keys and audit log and on the account, so a key
/// can never mint keys or add members.
#[tokio::test]
async fn programs_are_refused_the_dashboard_surface() {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let app = test.app();
    for path in [
        format!("/v1/workspaces/{}/members", workspace.id),
        format!("/v1/workspaces/{}/api_keys", workspace.id),
        format!("/v1/workspaces/{}/audit_log", workspace.id),
        "/v1/me".to_owned(),
        "/v1/workspaces".to_owned(),
    ] {
        let reply = app.get(&path).bearer(&workspace.key).send().await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(reply.json["code"], "session_required", "{path}");
    }
    let invite = app
        .post(&format!("/v1/workspaces/{}/invitations", workspace.id))
        .bearer(&workspace.key)
        .idempotency("invite")
        .json(json!({ "invitations": [{ "email": "x@example.com", "role": "admin" }] }))
        .send()
        .await;
    assert_eq!(invite.json["code"], "session_required");
}

/// A workspace token reaches only the workspace it was minted for: another workspace's dashboard
/// paths are `404`, as for a workspace that does not exist, and its own lists show only its rows.
#[tokio::test]
async fn a_workspace_token_reaches_only_its_workspace() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let owner = test.session("owner@acme.example").await;
    let app = test.app();
    let token = mint(&app, &owner, acme.id).await;
    for path in [
        format!("/v1/workspaces/{}/members", globex.id),
        format!("/v1/workspaces/{}/audit_log", globex.id),
    ] {
        let reply = app.get(&path).bearer(&token).send().await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
    let patch = app
        .patch(&format!("/v1/workspaces/{}", globex.id))
        .bearer(&token)
        .json(json!({ "name": "Mine now" }))
        .send()
        .await;
    assert_eq!(patch.status, StatusCode::NOT_FOUND);
    let members = app
        .get(&format!("/v1/workspaces/{}/members", acme.id))
        .bearer(&token)
        .send()
        .await;
    let data = members.json["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["user"]["email"], "owner@acme.example");
}

/// The dashboard's lists count themselves on request like every list of the API:
/// `include=total_count` adds the exact number of rows the filters match (two members, shown one
/// per page) and says the count was not capped; without it a page carries no count. Every list of
/// this surface counts through the same helpers, so one list proves them.
#[tokio::test]
async fn dashboard_lists_count_on_request() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    member(&test, acme.id, "ada@acme.example", "member").await;
    let app = test.app();
    let token = mint(&app, &owner, acme.id).await;
    let path = format!("/v1/workspaces/{}/members?limit=1", acme.id);
    let counted = app
        .get(&format!("{path}&include=total_count"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(counted.status, StatusCode::OK, "{}", counted.json);
    assert_eq!(counted.json["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(counted.json["meta"]["total_count"], 2);
    assert_eq!(counted.json["meta"]["total_count_capped"], false);
    let plain = app.get(&path).bearer(&token).send().await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.json);
    assert!(plain.json["meta"].get("total_count").is_none());
}

/// A signed-in user creates a workspace and becomes its owner (the create is idempotent in the
/// user's own namespace), lists it, updates it under `If-Match`, and deletes it: deletion stops
/// every token of the workspace at once and takes it off the user's list.
#[tokio::test]
async fn workspaces_are_created_listed_updated_and_deleted() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let founder = test.session("founder@example.com").await;
    let app = test.app();
    let body = json!({ "name": "Acme Growth", "timezone": "Europe/Madrid" });
    let created = app
        .post("/v1/workspaces")
        .browser(&founder)
        .idempotency("create-acme")
        .json(body.clone())
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert!(
        created.json["slug"]
            .as_str()
            .unwrap()
            .starts_with("acme-growth-")
    );
    let etag = created.header("etag").unwrap().to_owned();
    assert_eq!(etag, format!("\"{}\"", created.json["version"]));
    let replay = app
        .post("/v1/workspaces")
        .browser(&founder)
        .idempotency("create-acme")
        .json(body.clone())
        .send()
        .await;
    assert_eq!(replay.json["id"], created.json["id"]);
    assert_eq!(replay.header("idempotent-replayed"), Some("true"));
    let unkeyed = app
        .post("/v1/workspaces")
        .browser(&founder)
        .json(body)
        .send()
        .await;
    assert_eq!(unkeyed.status, StatusCode::BAD_REQUEST);

    let list = app.get("/v1/workspaces").browser(&founder).send().await;
    assert_eq!(list.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(list.json["data"][0]["role"], "owner");

    let id: Id<Workspace> = id_at(&created, "/id");
    let workspace = WorkspaceId::trusted(id.uuid());
    let token = mint(&app, &founder, workspace).await;
    let path = format!("/v1/workspaces/{id}");
    let stale = app
        .patch(&path)
        .bearer(&token)
        .header("if-match", "\"1\"")
        .json(json!({ "name": "Acme" }))
        .send()
        .await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    let updated = app
        .patch(&path)
        .bearer(&token)
        .header("if-match", &etag)
        .json(json!({ "name": "Acme", "settings": { "ai": { "classify_replies": true } } }))
        .send()
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json);
    assert_eq!(updated.json["name"], "Acme");
    assert_eq!(updated.json["settings"]["ai"]["classify_replies"], true);
    assert_ne!(updated.header("etag"), Some(etag.as_str()));
    let bad_zone = app
        .patch(&path)
        .bearer(&token)
        .json(json!({ "timezone": "Mars/Olympus" }))
        .send()
        .await;
    assert_eq!(bad_zone.status, StatusCode::UNPROCESSABLE_ENTITY);
    let bad_ai = app
        .patch(&path)
        .bearer(&token)
        .json(json!({ "settings": { "ai": { "enabled": true } } }))
        .send()
        .await;
    assert_eq!(bad_ai.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad_ai.json["errors"][0]["pointer"], "/settings/ai/enabled");

    let deleted = app.delete(&path).bearer(&token).send().await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.json);
    assert!(deleted.json["deletion_requested_at"].is_string());
    assert_eq!(
        app.get(&path).bearer(&token).send().await.status,
        StatusCode::UNAUTHORIZED
    );
    let list = app.get("/v1/workspaces").browser(&founder).send().await;
    assert!(list.json["data"].as_array().unwrap().is_empty());
}

/// Ownership moves only between owners and never leaves the workspace without one: the last owner
/// cannot step down, an admin cannot make an owner, an owner can, and the previous owner then may
/// step down; every change is in the audit log.
#[tokio::test]
async fn ownership_moves_only_between_owners_and_never_leaves_none() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let (admin, admin_membership) = member(&test, acme.id, "admin@acme.example", "admin").await;
    let app = test.app();
    let owner_token = mint(&app, &owner, acme.id).await;
    let admin_token = mint(&app, &admin, acme.id).await;
    let owner_membership = membership_of(&test, acme.id, owner.user.uuid()).await;
    let member_path = |id: Id<Membership>| format!("/v1/workspaces/{}/members/{id}", acme.id);

    let last = app
        .patch(&member_path(owner_membership))
        .bearer(&owner_token)
        .json(json!({ "role": "admin" }))
        .send()
        .await;
    assert_eq!(last.status, StatusCode::CONFLICT);
    assert_eq!(last.json["code"], "invalid_state");
    let by_admin = app
        .patch(&member_path(admin_membership))
        .bearer(&admin_token)
        .json(json!({ "role": "owner" }))
        .send()
        .await;
    assert_eq!(by_admin.status, StatusCode::FORBIDDEN);
    let transferred = app
        .patch(&member_path(admin_membership))
        .bearer(&owner_token)
        .json(json!({ "role": "owner" }))
        .send()
        .await;
    assert_eq!(transferred.status, StatusCode::OK, "{}", transferred.json);
    assert_eq!(transferred.json["role"], "owner");
    let stepped_down = app
        .patch(&member_path(owner_membership))
        .bearer(&owner_token)
        .json(json!({ "role": "admin" }))
        .send()
        .await;
    assert_eq!(stepped_down.status, StatusCode::OK, "{}", stepped_down.json);
    let remove_last = app
        .delete(&member_path(admin_membership))
        .bearer(&admin_token)
        .send()
        .await;
    assert_eq!(remove_last.status, StatusCode::CONFLICT);
    let actions = audited(&app, &admin_token, acme.id).await;
    assert!(actions.contains(&"ownership.transferred".to_owned()));
    assert!(actions.contains(&"member.role_changed".to_owned()));
}

/// A key dies with its creator's standing: suspending the member who created it revokes it in the
/// same transaction (refused at once on this process, and listed `revoked`), reactivating them
/// does not bring it back, and demoting a creator shrinks the scopes their keys act with.
#[tokio::test]
async fn keys_die_or_shrink_with_their_creators_membership() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let (admin, admin_membership) = member(&test, acme.id, "admin@acme.example", "admin").await;
    let app = test.app();
    let owner_token = mint(&app, &owner, acme.id).await;
    let admin_token = mint(&app, &admin, acme.id).await;
    let keys_path = format!("/v1/workspaces/{}/api_keys", acme.id);
    let create_key = |name: &'static str| {
        let (app, keys_path, admin_token) = (&app, &keys_path, &admin_token);
        async move {
            app.post(keys_path)
                .bearer(admin_token)
                .idempotency(name)
                .json(json!({ "name": name }))
                .send()
                .await
        }
    };
    let unknown_scope = app
        .post(&keys_path)
        .bearer(&admin_token)
        .idempotency("bad-scope")
        .json(json!({ "name": "x", "scopes": ["people:read", "root:everything"] }))
        .send()
        .await;
    assert_eq!(unknown_scope.status, StatusCode::UNPROCESSABLE_ENTITY);

    let shrinking = create_key("shrinking").await;
    assert_eq!(shrinking.status, StatusCode::CREATED, "{}", shrinking.json);
    let shrinking = shrinking.json["secret"].as_str().unwrap().to_owned();
    let person = |key: String, n: u32| {
        let app = &app;
        async move {
            app.post("/v1/people")
                .bearer(&key)
                .idempotency(&format!("person-{n}"))
                .json(json!({ "email": format!("p{n}@example.com") }))
                .send()
                .await
                .status
        }
    };
    assert_eq!(person(shrinking.clone(), 1).await, StatusCode::CREATED);
    let member_path = format!("/v1/workspaces/{}/members/{admin_membership}", acme.id);
    let demoted = app
        .patch(&member_path)
        .bearer(&owner_token)
        .json(json!({ "role": "viewer" }))
        .send()
        .await;
    assert_eq!(demoted.status, StatusCode::OK, "{}", demoted.json);
    assert_eq!(person(shrinking, 2).await, StatusCode::FORBIDDEN);

    let restored = app
        .patch(&member_path)
        .bearer(&owner_token)
        .json(json!({ "role": "admin" }))
        .send()
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    let admin_token = mint(&app, &admin, acme.id).await;
    let dying = app
        .post(&keys_path)
        .bearer(&admin_token)
        .idempotency("dying")
        .json(json!({ "name": "dying" }))
        .send()
        .await;
    assert_eq!(dying.status, StatusCode::CREATED, "{}", dying.json);
    let dying_key = dying.json["secret"].as_str().unwrap().to_owned();
    let workspace_path = format!("/v1/workspaces/{}", acme.id);
    assert_eq!(
        app.get(&workspace_path)
            .bearer(&dying_key)
            .send()
            .await
            .status,
        StatusCode::OK
    );
    let suspended = app
        .patch(&member_path)
        .bearer(&owner_token)
        .json(json!({ "status": "suspended" }))
        .send()
        .await;
    assert_eq!(suspended.status, StatusCode::OK, "{}", suspended.json);
    assert_eq!(
        app.get(&workspace_path)
            .bearer(&dying_key)
            .send()
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.get(&workspace_path)
            .bearer(&admin_token)
            .send()
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let reactivated = app
        .patch(&member_path)
        .bearer(&owner_token)
        .json(json!({ "status": "active" }))
        .send()
        .await;
    assert_eq!(reactivated.status, StatusCode::OK);
    assert_eq!(
        app.get(&workspace_path)
            .bearer(&dying_key)
            .send()
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let listed = app
        .get(&format!("{keys_path}?status=revoked"))
        .bearer(&owner_token)
        .send()
        .await;
    assert_eq!(listed.json["data"].as_array().unwrap().len(), 2);
    let actions = audited(&app, &owner_token, acme.id).await;
    assert!(actions.contains(&"member.suspended".to_owned()));
    assert!(actions.contains(&"api_key.created".to_owned()));
}

/// A key is created with its secret shown once, renamed under `If-Match`, and revoked: the revoked
/// key stops at once on this process and stays listed as `revoked`.
#[tokio::test]
async fn keys_are_created_renamed_and_revoked() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let app = test.app();
    let token = mint(&app, &owner, acme.id).await;
    let keys_path = format!("/v1/workspaces/{}/api_keys", acme.id);
    let created = app
        .post(&keys_path)
        .bearer(&token)
        .idempotency("sync")
        .json(json!({ "name": "CRM sync", "scopes": ["people:read"] }))
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let secret = created.json["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("nb_live_"));
    assert_eq!(created.json["scopes"], json!(["people:read"]));
    let path = format!("{keys_path}/{}", created.json["id"].as_str().unwrap());
    let etag = created.header("etag").unwrap().to_owned();
    let listed = app.get(&keys_path).bearer(&token).send().await;
    assert!(
        listed.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|key| key.get("secret").is_none())
    );

    let stale = app
        .patch(&path)
        .bearer(&token)
        .header("if-match", "\"7\"")
        .json(json!({ "name": "Other" }))
        .send()
        .await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    let renamed = app
        .patch(&path)
        .bearer(&token)
        .header("if-match", &etag)
        .json(json!({ "name": "CRM sync (prod)" }))
        .send()
        .await;
    assert_eq!(renamed.json["name"], "CRM sync (prod)");

    let workspace_path = format!("/v1/workspaces/{}", acme.id);
    assert_eq!(
        app.get(&workspace_path).bearer(&secret).send().await.status,
        StatusCode::FORBIDDEN,
        "the key lacks workspace:read"
    );
    let revoked = app.delete(&path).bearer(&token).send().await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert_eq!(revoked.json["status"], "revoked");
    assert_eq!(
        app.get(&workspace_path).bearer(&secret).send().await.status,
        StatusCode::UNAUTHORIZED
    );
}

/// Invitations are sent by mail, sent again (extended, with the new role) for an address already
/// invited, accepted only by a signed-in user whose verified address is the invited one, once, and
/// a revoked invitation is accepted by nobody.
#[tokio::test]
async fn invitations_are_sent_resent_and_accepted_by_their_address_only() {
    let test = TestDb::new().await;
    test.signing_key().await;
    test.transactional_sender().await;
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let app = test.app();
    let token = mint(&app, &owner, acme.id).await;
    let path = format!("/v1/workspaces/{}/invitations", acme.id);
    let invite = |body: serde_json::Value, key: &'static str| {
        let (app, path, token) = (&app, &path, &token);
        async move {
            app.post(path)
                .bearer(token)
                .idempotency(key)
                .json(body)
                .send()
                .await
        }
    };
    let sent = invite(
        json!({ "invitations": [
            { "email": "grace@example.com", "role": "member" },
            { "email": "linus@example.com", "role": "viewer" },
        ]}),
        "first",
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.json);
    assert_eq!(sent.json["data"].as_array().unwrap().len(), 2);
    let resent = invite(
        json!({ "invitations": [{ "email": "Grace@Example.com", "role": "admin" }] }),
        "again",
    )
    .await;
    assert_eq!(resent.json["data"][0]["id"], sent.json["data"][0]["id"]);
    assert_eq!(resent.json["data"][0]["role"], "admin");
    assert!(
        resent.json["data"][0]["expires_at"].as_str().unwrap()
            >= sent.json["data"][0]["expires_at"].as_str().unwrap()
    );
    let duplicate = invite(
        json!({ "invitations": [
            { "email": "a@example.com", "role": "member" },
            { "email": "A@example.com", "role": "viewer" },
        ]}),
        "duplicate",
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::UNPROCESSABLE_ENTITY);
    let owner_invited = invite(
        json!({ "invitations": [{ "email": "x@example.com", "role": "owner" }] }),
        "owner",
    )
    .await;
    assert_eq!(owner_invited.status, StatusCode::UNPROCESSABLE_ENTITY);

    let grace_token = latest_token(&test, "Grace@Example.com").await;
    let linus_token = latest_token(&test, "linus@example.com").await;
    let grace = sign_in(&test, &app, "grace@example.com").await;
    let linus = sign_in(&test, &app, "linus@example.com").await;
    let accept = |browser: &TestSession, invitation: &str, key: &'static str| {
        let app = &app;
        let call = app
            .post("/v1/me/memberships")
            .browser(browser)
            .idempotency(key)
            .json(json!({ "invitation_token": invitation }));
        async move { call.send().await }
    };
    let wrong_person = accept(&linus, &grace_token, "linus-takes-grace").await;
    assert_eq!(wrong_person.status, StatusCode::FORBIDDEN);
    let accepted = accept(&grace, &grace_token, "grace-accepts").await;
    assert_eq!(accepted.status, StatusCode::CREATED, "{}", accepted.json);
    assert_eq!(accepted.json["role"], "admin");
    assert_eq!(accepted.json["workspace"]["id"], acme.id.to_string());
    let twice = accept(&grace, &grace_token, "grace-again").await;
    assert_eq!(twice.status, StatusCode::CONFLICT);
    let member_again = invite(
        json!({ "invitations": [{ "email": "grace@example.com", "role": "viewer" }] }),
        "member-again",
    )
    .await;
    assert_eq!(member_again.status, StatusCode::CONFLICT);

    let linus_invitation = sent.json["data"][1]["id"].as_str().unwrap();
    let revoked = app
        .delete(&format!("{path}/{linus_invitation}"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let too_late = accept(&linus, &linus_token, "linus-accepts").await;
    assert_eq!(too_late.status, StatusCode::CONFLICT);
    let listed = app
        .get(&format!("{path}?status=accepted"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(listed.json["data"].as_array().unwrap().len(), 1);
}

/// Two acceptances of one invitation racing (the invited person in two tabs) admit once: the
/// second waits for the workspace row, finds the invitation accepted, and changes nothing.
#[tokio::test]
async fn racing_acceptances_admit_once() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let email = EmailAddress::parse("grace@example.com").unwrap();
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    let sent = invitations::send(
        &mut tx,
        &keys(),
        acme.id,
        &email,
        MembershipRole::Member,
        acme.owner,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let user = test.session("grace@example.com").await.user;
    let accept = || {
        let (db, token) = (test.app.clone(), sent.token.clone());
        async move {
            let mut tx = db.begin_as_user(user).await.unwrap();
            let found = invitations::find(&mut tx, &keys(), &token)
                .await
                .unwrap()
                .unwrap();
            crate::db::set_workspace(&mut tx, found.workspace)
                .await
                .unwrap();
            let accepted =
                invitations::accept(&mut tx, found, user, Some("grace@example.com")).await;
            match &accepted {
                Ok(_) => tx.commit().await.unwrap(),
                Err(_) => tx.rollback().await.unwrap(),
            }
            accepted.map(|accepted| accepted.joined)
        }
    };
    let (one, two) = tokio::join!(accept(), accept());
    let outcomes = [one, two];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(true)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(InvitationError::NotPending)))
            .count(),
        1
    );
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1")
            .bind(user.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(memberships, 1);
}

/// Two owners demoting each other at the same instant leave exactly one owner: the workspace row
/// serialises the two changes, and the second sees the last owner and is refused.
#[tokio::test]
async fn owners_demoting_each_other_at_once_leave_one() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let first = membership_of(&test, acme.id, acme.owner.uuid()).await;
    let (_, second) = member(&test, acme.id, "second@acme.example", "owner").await;
    let demote = |target: Id<Membership>| {
        let db = test.app.clone();
        async move {
            let mut tx = db.begin_in(acme.id).await.unwrap();
            let changed = memberships::change(
                &mut tx,
                acme.id,
                target,
                MembershipRole::Owner,
                Change {
                    role: Some(MembershipRole::Admin),
                    status: None,
                },
                &IfMatch::Absent,
            )
            .await;
            match &changed {
                Ok(_) => tx.commit().await.unwrap(),
                Err(_) => tx.rollback().await.unwrap(),
            }
            changed.map(|_| ())
        }
    };
    let (one, two) = tokio::join!(demote(first), demote(second));
    let refused = [&one, &two]
        .iter()
        .filter(|outcome| {
            matches!(
                outcome,
                Err(MemberError::Refused(
                    crate::domain::identity::ChangeRefusal::LastOwner
                ))
            )
        })
        .count();
    assert_eq!((one.is_ok() || two.is_ok(), refused), (true, 1));
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE workspace_id = $1 AND role = 'owner' AND status = 'active'",
    )
    .bind(acme.id.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(owners, 1);
}

/// The account: `GET /me` carries the version as `ETag`; `PATCH /me` applies under `If-Match`;
/// another session of the user is revoked by its id; sub-resources that are not the user's are
/// `404`.
#[tokio::test]
async fn the_account_shows_and_changes_under_its_own_rules() {
    let test = TestDb::new().await;
    let browser = test.session("ada@example.com").await;
    let other = test.session("ada@example.com").await;
    let app = test.app();
    let me = app.get("/v1/me").browser(&browser).send().await;
    assert_eq!(me.json["sessions"].as_array().unwrap().len(), 2);
    let etag = me.header("etag").unwrap().to_owned();
    let stale = app
        .patch("/v1/me")
        .browser(&browser)
        .header("if-match", "\"3\"")
        .json(json!({ "name": "Ada" }))
        .send()
        .await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    let changed = app
        .patch("/v1/me")
        .browser(&browser)
        .header("if-match", &etag)
        .json(json!({ "name": "Ada Lovelace", "locale": "es-ES" }))
        .send()
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json);
    assert_eq!(changed.json["name"], "Ada Lovelace");
    assert_eq!(changed.json["locale"], "es-ES");

    let revoked = app
        .delete(&format!("/v1/me/sessions/{}", other.session))
        .browser(&browser)
        .send()
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get("/v1/me").browser(&other).send().await.status,
        StatusCode::UNAUTHORIZED
    );
    for path in [
        format!("/v1/me/sessions/{}", other.session),
        format!("/v1/me/passkeys/pky_{}", Uuid::now_v7().simple()),
        format!("/v1/me/grants/grt_{}", Uuid::now_v7().simple()),
        format!("/v1/me/identities/idn_{}", Uuid::now_v7().simple()),
    ] {
        let reply = app.delete(&path).browser(&browser).send().await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
}
