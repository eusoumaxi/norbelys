//! API contracts for retained content, search and files, using the router and real role pools.

use crate::domain::ids::{Id, Message};
use crate::domain::scope::{Scope, ScopeSet};
use crate::testing::{self, SenderSpec, TestDb};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

/// An uploaded file is attached atomically to a message, reaches the shared MIME composer,
/// survives a refused delete, and can be read only inside its workspace. Prepared content is
/// fenced by the sending lease, so an older owner cannot overwrite it. Literal search finds
/// content beyond the old classification excerpt and binds its cursor to the same filters.
#[tokio::test]
async fn files_content_search_and_tenancy() {
    let test = TestDb::new().await;
    let workspace = test.workspace("content").await;
    let stranger = test.workspace("stranger").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("sender@example.com"))
        .await;
    let app = test.app();
    let uploaded = app
        .post("/v1/attachments")
        .bearer(&workspace.key)
        .idempotency(&Uuid::now_v7().to_string())
        .json(
            json!({"filename":"notes.txt","content_type":"text/plain","content_base64":"SGVsbG8="}),
        )
        .send()
        .await;
    assert_eq!(uploaded.status, StatusCode::CREATED, "{}", uploaded.json);
    let file = uploaded.json["id"].as_str().unwrap();
    let mut ids = Vec::new();
    for n in 0..2 {
        let created=app.post("/v1/messages").bearer(&workspace.key).idempotency(&Uuid::now_v7().to_string()).json(json!({"from":sender.identity,"to":["reader@example.com"],"subject":format!("Report {n}"),"html":format!("<p>{} needle_100%</p>","content ".repeat(700)),"attachments":[file]})).send().await;
        assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.json);
        ids.push(created.json["id"].as_str().unwrap().to_owned());
    }
    let id: Id<Message> = ids.first().unwrap().parse().unwrap();
    let settings = crate::rendering::Settings::new(
        testing::keys(),
        &url::Url::parse("https://tracking.example.com").unwrap(),
        std::time::Duration::from_secs(86400),
    )
    .unwrap()
    .with_storage(test.storage.clone());
    let prepared = crate::rendering::prepare(&test.worker, &settings, workspace.id, id)
        .await
        .unwrap();
    let mime = norbelys_mail::inbound::content(&prepared.raw).unwrap();
    assert_eq!(mime.attachments.len(), 1);
    assert_eq!(mime.attachments.first().unwrap().bytes, b"Hello");
    let mut tx = test.worker.begin_in(workspace.id).await.unwrap();
    sqlx::query("UPDATE delivery_queue SET state='claimed', lease_owner='content-test', lease_generation=7, lease_expires_at=clock_timestamp()+interval '1 hour' WHERE workspace_id=$1 AND message_id=$2")
        .bind(workspace.id.uuid()).bind(id.uuid()).execute(&mut *tx).await.unwrap();
    assert!(
        super::content::prepared(&mut tx, workspace.id, id, "content-test", 7, &prepared.raw)
            .await
            .unwrap()
    );
    for (owner, generation) in [("old-owner", 7), ("content-test", 6)] {
        assert!(
            !super::content::prepared(
                &mut tx,
                workspace.id,
                id,
                owner,
                generation,
                b"Subject: stale\r\n\r\nwrong body"
            )
            .await
            .unwrap()
        );
    }
    sqlx::query("UPDATE delivery_queue SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE workspace_id=$1 AND message_id=$2")
        .bind(workspace.id.uuid()).bind(id.uuid()).execute(&mut *tx).await.unwrap();
    assert!(
        !super::content::prepared(
            &mut tx,
            workspace.id,
            id,
            "content-test",
            7,
            b"Subject: stale\r\n\r\nexpired"
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();
    let content = app
        .get(&format!("/v1/messages/{id}/content"))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(content.status, StatusCode::OK, "{}", content.json);
    assert!(
        content.json["html"]
            .as_str()
            .unwrap()
            .contains("needle_100%")
    );
    assert!(content.json["prepared_at"].is_string());
    assert_eq!(content.json["attachments"][0]["id"], file);
    assert_eq!(
        app.delete(&format!("/v1/attachments/{file}"))
            .bearer(&workspace.key)
            .send()
            .await
            .status,
        StatusCode::CONFLICT
    );
    for path in [
        format!("/v1/attachments/{file}"),
        format!("/v1/messages/{id}/content"),
    ] {
        assert_eq!(
            app.get(&path).bearer(&stranger.key).send().await.status,
            StatusCode::NOT_FOUND
        );
    }
    let first = app
        .get("/v1/messages/search?q=needle_100%25&direction=outbound&limit=1")
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.json);
    assert_eq!(first.json["data"].as_array().unwrap().len(), 1);
    assert!(first.json["data"][0].get("html").is_none());
    let cursor = first.json["meta"]["next_cursor"].as_str().unwrap();
    let next = app
        .get(&format!(
            "/v1/messages/search?q=needle_100%25&direction=outbound&limit=1&cursor={cursor}"
        ))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.json);
    assert_ne!(first.json["data"][0]["id"], next.json["data"][0]["id"]);
    let changed = app
        .get(&format!(
            "/v1/messages/search?q=different&direction=outbound&limit=1&cursor={cursor}"
        ))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(changed.status, StatusCode::BAD_REQUEST);
    let read_only = test
        .api_key(&workspace, ScopeSet::from_iter([Scope::MessagesRead]))
        .await;
    assert_eq!(
        app.get("/v1/messages/search?direction=outbound")
            .bearer(&read_only)
            .send()
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.get("/v1/messages/search")
            .bearer(&read_only)
            .send()
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.get("/v1/messages/search")
            .bearer(&stranger.key)
            .send()
            .await
            .json["data"],
        json!([])
    );
}

/// Upload validation rejects injection and invalid base64 before writing metadata; a failed
/// acceptance with a forged attachment id leaves no message or queue row behind.
#[tokio::test]
async fn invalid_files_leave_no_message() {
    let test = TestDb::new().await;
    let workspace = test.workspace("invalid-files").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("sender@example.com"))
        .await;
    let app = test.app();
    for body in [
        json!({"filename":"../secret","content_type":"text/plain","content_base64":"AA=="}),
        json!({"filename":"ok","content_type":"text/plain\r\nInjected: yes","content_base64":"AA=="}),
        json!({"filename":"ok","content_type":"text/plain","content_base64":"!!!"}),
    ] {
        let refused = app
            .post("/v1/attachments")
            .bearer(&workspace.key)
            .idempotency(&Uuid::now_v7().to_string())
            .json(body)
            .send()
            .await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            refused.json
        );
    }
    let result=app.post("/v1/messages").bearer(&workspace.key).idempotency(&Uuid::now_v7().to_string()).json(json!({"from":sender.identity,"to":["reader@example.com"],"subject":"Hello","html":"<p>Hello</p>","attachments":[Id::<crate::domain::ids::Attachment>::new()]})).send().await;
    assert_eq!(result.status, StatusCode::NOT_FOUND, "{}", result.json);
    let listed = app
        .get("/v1/messages/search?direction=outbound")
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(listed.json["data"], Value::Array(Vec::new()));
}
