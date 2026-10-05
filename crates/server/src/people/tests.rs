//! Store and API tests of the audience, against real PostgreSQL through the api's router in
//! process, with the import and export jobs run by the runner's own steps ([`Harness`]) on the
//! worker's login and the test's local object store.
//!
//! The pure decisions (field values, CSV columns and rows, segment filters, preflight's
//! verdicts, which suppressions lift) are tested beside their code in `domain/`; these tests
//! prove what only the database, the router and the jobs together can.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::exports::ExportRun;
use super::imports::PeopleImport;
use crate::domain::ids::{Group, Id, Import, Person, Segment, Suppression, WorkspaceId};
use crate::jobs::runner::Harness;
use crate::jobs::{Queue, Registry};
use crate::testing::{Reply, SenderSpec, TestApp, TestDb};

/// The import and export kinds on the test's database, with its object store.
fn harness(test: &TestDb) -> Harness {
    let mut registry = Registry::default();
    registry
        .register::<PeopleImport>()
        .unwrap()
        .register::<ExportRun>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(test.storage.clone());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "audience-test",
    )
}

/// Runs the due jobs of `queue` once; returns their outcomes.
async fn run(runner: &Harness, queue: Queue) -> Vec<&'static str> {
    runner
        .run_once(queue, 4)
        .await
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect()
}

/// `POST path` with `body` and a fresh idempotency key.
async fn post(app: &TestApp, key: &str, path: &str, body: Value) -> Reply {
    app.post(path)
        .bearer(key)
        .idempotency(&Uuid::now_v7().to_string())
        .json(body)
        .send()
        .await
}

/// Creates a resource and returns its id, asserting the creation succeeded.
async fn create(app: &TestApp, key: &str, path: &str, body: Value) -> String {
    let reply = post(app, key, path, body).await;
    assert!(reply.status.is_success(), "{path}: {:?}", reply.json);
    reply.json["id"].as_str().unwrap().to_owned()
}

/// The emails of a page of people, in the page's order.
fn emails(reply: &Reply) -> Vec<String> {
    reply.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|person| person["email"].as_str().unwrap().to_owned())
        .collect()
}

/// The status, code and first pointer of a problem.
fn problem(reply: &Reply) -> (StatusCode, &str, &str) {
    (
        reply.status,
        reply.json["code"].as_str().unwrap_or_default(),
        reply.json["errors"][0]["pointer"]
            .as_str()
            .unwrap_or_default(),
    )
}

/// The types and data of `workspace`'s outbox events, oldest first.
async fn events(test: &TestDb, workspace: WorkspaceId) -> Vec<(String, Value)> {
    sqlx::query!(
        "SELECT type AS kind, payload FROM outbox_events WHERE workspace_id = $1 ORDER BY id",
        workspace.uuid()
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.kind, row.payload["data"].clone()))
    .collect()
}

/// The path and query of an absolute link to this api.
fn local(url: &str) -> String {
    url.strip_prefix("http://127.0.0.1:3001")
        .unwrap()
        .to_owned()
}

/// Creates the typed fields used by both JSON writes and CSV imports.
async fn custom_fields(app: &TestApp, key: &str) {
    create(
        app,
        key,
        "/v1/fields",
        json!({ "key": "employees", "label": "Employees", "type": "number" }),
    )
    .await;
    create(
        app,
        key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "enum", "options": ["gold", "silver"] }),
    )
    .await;
}

/// A person lives its whole life on the API: created with typed values and groups (`201`),
/// read back with its `group_ids`, changed (values merged with `null` removing one, groups
/// replaced whole, a name cleared, blank text absent), and deleted (`204`, then `404`).
#[tokio::test]
async fn a_person_is_created_changed_and_deleted() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    custom_fields(&app, &acme.key).await;
    let a = create(&app, &acme.key, "/v1/groups", json!({ "name": "A" })).await;
    let b = create(&app, &acme.key, "/v1/groups", json!({ "name": "B" })).await;

    let created = post(
        &app,
        &acme.key,
        "/v1/people",
        json!({
            "email": " Ada@Example.com ", "given_name": "Ada", "company": "  ",
            "fields": { "employees": 12, "tier": "gold" }, "group_ids": [a]
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.json);
    let id = created.json["id"].as_str().unwrap();
    assert!(id.starts_with("per_"));
    assert_eq!(
        (
            &created.json["email"],
            &created.json["company"],
            &created.json["fields"],
            &created.json["group_ids"]
        ),
        (
            &json!("Ada@Example.com"),
            &Value::Null,
            &json!({ "employees": 12, "tier": "gold" }),
            &json!([a])
        )
    );

    let path = format!("/v1/people/{id}");
    let changed = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "given_name": null, "family_name": "Lovelace", "fields": { "employees": null, "tier": "silver" }, "group_ids": [b] }))
        .send()
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.json);
    let read = app.get(&path).bearer(&acme.key).send().await;
    assert_eq!(read.json, changed.json);
    assert_eq!(
        (
            &read.json["given_name"],
            &read.json["family_name"],
            &read.json["fields"],
            &read.json["group_ids"]
        ),
        (
            &Value::Null,
            &json!("Lovelace"),
            &json!({ "tier": "silver" }),
            &json!([b])
        )
    );

    assert_eq!(
        app.delete(&path).bearer(&acme.key).send().await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.get(&path).bearer(&acme.key).send().await.status,
        StatusCode::NOT_FOUND
    );
}

/// The people list pages with signed cursors (the second page continues where the first
/// stopped) and filters by address ignoring case, search prefix and creation time; a cursor
/// reused with other filters is refused.
#[tokio::test]
async fn people_lists_page_and_filter() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    for email in ["ada@example.com", "adam@example.com", "bob@example.com"] {
        create(&app, &acme.key, "/v1/people", json!({ "email": email })).await;
    }
    let first = app.get("/v1/people?limit=2").bearer(&acme.key).send().await;
    assert_eq!(emails(&first), ["bob@example.com", "adam@example.com"]);
    assert_eq!(first.json["meta"]["has_more"], json!(true));
    let cursor = first.json["meta"]["next_cursor"].as_str().unwrap();
    let second = app
        .get(&format!("/v1/people?limit=2&cursor={cursor}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(emails(&second), ["ada@example.com"]);
    assert_eq!(second.json["meta"]["has_more"], json!(false));
    let ascending = app
        .get("/v1/people?order=asc&include=total_count")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        emails(&ascending),
        ["ada@example.com", "adam@example.com", "bob@example.com"]
    );
    assert_eq!(ascending.json["meta"]["total_count"], json!(3));

    let by_email = app
        .get("/v1/people?email=ADA@EXAMPLE.COM")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(emails(&by_email), ["ada@example.com"]);
    let by_prefix = app
        .get("/v1/people?q=Ad&order=asc")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(emails(&by_prefix), ["ada@example.com", "adam@example.com"]);
    let later = app
        .get("/v1/people?created_at%5Bgte%5D=2999-01-01T00:00:00Z")
        .bearer(&acme.key)
        .send()
        .await;
    assert!(emails(&later).is_empty());
    let foreign = app
        .get(&format!("/v1/people?q=b&cursor={cursor}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(problem(&foreign).0, StatusCode::BAD_REQUEST);
}

/// `GET /v1/people?sort=updated_at` lists people by when each last changed, each tie broken by
/// id, so a person changed later comes first, which the `id` order never does. One page at a
/// time, every person comes exactly once and in that order even where several changed in one
/// transaction (and share its instant), because each cursor carries the instant and the id of
/// the last person shown; `order=asc` reverses it.
#[tokio::test]
async fn people_sort_by_their_last_change() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mut ids = Vec::new();
    for email in ["ada@example.com", "bob@example.com", "eve@example.com"] {
        let id = create(&app, &acme.key, "/v1/people", json!({ "email": email })).await;
        ids.push(id.parse::<Id<Person>>().unwrap().uuid());
    }
    // Eve changes first; then Ada and Bob together, sharing their transaction's instant.
    sqlx::query("UPDATE people SET company = 'Eve & Co' WHERE id = $1")
        .bind(ids[2])
        .execute(test.system.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE people SET company = 'Same Co' WHERE id = ANY($1)")
        .bind(vec![ids[0], ids[1]])
        .execute(test.system.pool())
        .await
        .unwrap();

    let mut seen = Vec::new();
    let mut query = "sort=updated_at&limit=1".to_owned();
    loop {
        let page = app
            .get(&format!("/v1/people?{query}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(page.status, StatusCode::OK, "{:?}", page.json);
        seen.extend(emails(&page));
        let Some(cursor) = page.json["meta"]["next_cursor"].as_str() else {
            break;
        };
        query = format!("sort=updated_at&limit=1&cursor={cursor}");
    }
    assert_eq!(
        seen,
        ["bob@example.com", "ada@example.com", "eve@example.com"]
    );
    let ascending = app
        .get("/v1/people?sort=updated_at&order=asc")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        emails(&ascending),
        ["eve@example.com", "ada@example.com", "bob@example.com"]
    );
}

/// An address is one person in a workspace, whatever its ASCII case: creating it again, or
/// moving another person onto it, is `409 conflict`.
#[tokio::test]
async fn an_address_is_one_person() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ada@example.com" }),
    )
    .await;
    let twin = post(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ADA@example.com" }),
    )
    .await;
    assert_eq!(problem(&twin).0, StatusCode::CONFLICT);
    let bob = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "bob@example.com" }),
    )
    .await;
    let moved = app
        .patch(&format!("/v1/people/{bob}"))
        .bearer(&acme.key)
        .json(json!({ "email": "Ada@Example.com" }))
        .send()
        .await;
    assert_eq!(problem(&moved), (StatusCode::CONFLICT, "conflict", ""));
}

/// The API refuses custom values that do not fit the workspace's fields before any write,
/// each at its pointer: a value of the wrong type, a key no field defines.
#[tokio::test]
async fn the_api_refuses_values_that_do_not_fit_their_fields() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "employees", "label": "Employees", "type": "number" }),
    )
    .await;
    let reply = post(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ada@example.com", "fields": { "employees": "many", "phone": "1" } }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let pointers: Vec<&str> = reply.json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| error["pointer"].as_str().unwrap())
        .collect();
    assert_eq!(pointers, ["/fields/employees", "/fields/phone"]);
}

/// The database's trigger refuses a mistyped custom value whatever wrote it (the second net,
/// below the API's check): a direct insert of a string into a number field fails with a check
/// violation naming `people_field_definition`, and a fitting value passes.
#[tokio::test]
async fn the_database_refuses_a_mistyped_value_whatever_the_writer() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "employees", "label": "Employees", "type": "number" }),
    )
    .await;
    let insert = |email: &'static str, fields: Value| {
        let db = test.app.clone();
        let workspace = acme.id;
        async move {
            let mut tx = db.begin_in(workspace).await.unwrap();
            let result = sqlx::query!(
                "INSERT INTO people (workspace_id, email, custom_fields) VALUES ($1, $2, $3)",
                workspace.uuid(),
                email,
                fields
            )
            .execute(&mut *tx)
            .await;
            tx.commit().await.unwrap();
            result.map(|_| ()).map_err(|error| {
                let database = error.as_database_error().unwrap();
                (
                    database.code().unwrap().into_owned(),
                    database.constraint().unwrap_or_default().to_owned(),
                )
            })
        }
    };
    assert_eq!(
        insert("ada@example.com", json!({ "employees": "many" })).await,
        Err(("23514".to_owned(), "people_field_definition".to_owned()))
    );
    assert_eq!(
        insert("bob@example.com", json!({ "employees": 3 })).await,
        Ok(())
    );
}

/// Another workspace's audience does not exist for a credential: its people, fields, groups,
/// segments, suppressions, imports and exports are `404` to read, change or delete, exactly as
/// ids that exist nowhere, and its lists are empty.
#[tokio::test]
async fn another_workspaces_audience_is_not_found() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let app = test.app();
    let field = create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "text" }),
    )
    .await;
    let person = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ada@example.com" }),
    )
    .await;
    let group = create(&app, &acme.key, "/v1/groups", json!({ "name": "A" })).await;
    let segment = create(&app, &acme.key, "/v1/segments", json!({ "name": "S", "filter": { "conditions": [{ "field": "company", "operator": "exists" }] } })).await;
    let suppression = create(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "email": "no@example.com" }),
    )
    .await;
    let import = create(
        &app,
        &acme.key,
        "/v1/imports",
        json!({ "people": [{ "email": "x@example.com" }] }),
    )
    .await;
    let export = create(
        &app,
        &acme.key,
        "/v1/exports",
        json!({ "resource": "people" }),
    )
    .await;
    for path in [
        format!("/v1/people/{person}"),
        format!("/v1/groups/{group}"),
        format!("/v1/segments/{segment}"),
        format!("/v1/suppressions/{suppression}"),
        format!("/v1/imports/{import}"),
        format!("/v1/exports/{export}"),
    ] {
        assert_eq!(
            app.get(&path).bearer(&globex.key).send().await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in [
        format!("/v1/people/{person}"),
        format!("/v1/fields/{field}"),
        format!("/v1/groups/{group}"),
        format!("/v1/segments/{segment}"),
    ] {
        let changed = app
            .patch(&path)
            .bearer(&globex.key)
            .json(json!({}))
            .send()
            .await;
        assert_eq!(changed.status, StatusCode::NOT_FOUND, "{path}");
    }
    for path in [
        format!("/v1/people/{person}"),
        format!("/v1/fields/{field}"),
        format!("/v1/groups/{group}"),
        format!("/v1/segments/{segment}"),
        format!("/v1/suppressions/{suppression}"),
    ] {
        assert_eq!(
            app.delete(&path).bearer(&globex.key).send().await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    let listed = app
        .get(&format!("/v1/people?group_id={group}"))
        .bearer(&globex.key)
        .send()
        .await;
    assert_eq!(listed.status, StatusCode::NOT_FOUND);
    assert!(emails(&app.get("/v1/people").bearer(&globex.key).send().await).is_empty());
    assert_eq!(
        app.get(&format!("/v1/people/{person}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::OK
    );
}

/// A person with history cannot be deleted: an enrollment refers to it, so the delete is
/// `409 invalid_state` and the person stays.
#[tokio::test]
async fn a_person_with_history_cannot_be_deleted() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let person = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ada@example.com" }),
    )
    .await;
    let person_uuid = person.parse::<Id<Person>>().unwrap().uuid();
    let campaign = sqlx::query_scalar!(
        "INSERT INTO campaigns (workspace_id, name) VALUES ($1, 'C') RETURNING id",
        acme.id.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO enrollments (workspace_id, campaign_id, person_id) VALUES ($1, $2, $3)",
        acme.id.uuid(),
        campaign,
        person_uuid
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let path = format!("/v1/people/{person}");
    let deleted = app.delete(&path).bearer(&acme.key).send().await;
    assert_eq!(
        problem(&deleted),
        (StatusCode::CONFLICT, "invalid_state", "")
    );
    assert_eq!(
        app.get(&path).bearer(&acme.key).send().await.status,
        StatusCode::OK
    );
}

/// Membership is written on the person and read on the group: `people_count` counts it,
/// `?group_id=` lists it, replacing `group_ids` moves it, more than 100 ids or an absent group
/// are refused, and deleting a group removes its memberships but not its people.
#[tokio::test]
async fn group_membership_is_written_on_the_person() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let a = create(&app, &acme.key, "/v1/groups", json!({ "name": "Alpha" })).await;
    let b = create(
        &app,
        &acme.key,
        "/v1/groups",
        json!({ "name": "Beta", "description": "second" }),
    )
    .await;
    create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "one@example.com", "group_ids": [a] }),
    )
    .await;
    let two = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "two@example.com", "group_ids": [a, b] }),
    )
    .await;
    let count = |group: String| {
        let app = &app;
        let key = acme.key.clone();
        async move {
            app.get(&format!("/v1/groups/{group}"))
                .bearer(&key)
                .send()
                .await
                .json["people_count"]
                .clone()
        }
    };
    assert_eq!(count(a.clone()).await, json!(2));
    let in_b = app
        .get(&format!("/v1/people?group_id={b}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(emails(&in_b), ["two@example.com"]);
    let searched = app.get("/v1/groups?q=AL").bearer(&acme.key).send().await;
    assert_eq!(searched.json["data"][0]["name"], json!("Alpha"));

    let moved = app
        .patch(&format!("/v1/people/{two}"))
        .bearer(&acme.key)
        .json(json!({ "group_ids": [b] }))
        .send()
        .await;
    assert_eq!(moved.json["group_ids"], json!([b]));
    assert_eq!(count(a.clone()).await, json!(1));

    let many: Vec<String> = (0..101).map(|_| Id::<Group>::new().to_string()).collect();
    let too_many = app
        .patch(&format!("/v1/people/{two}"))
        .bearer(&acme.key)
        .json(json!({ "group_ids": many }))
        .send()
        .await;
    assert_eq!(
        problem(&too_many),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "/group_ids"
        )
    );
    let absent = Id::<Group>::new().to_string();
    let missing = app
        .patch(&format!("/v1/people/{two}"))
        .bearer(&acme.key)
        .json(json!({ "group_ids": [absent] }))
        .send()
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    assert_eq!(
        app.delete(&format!("/v1/groups/{b}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let after = app
        .get(&format!("/v1/people/{two}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (after.status, &after.json["group_ids"]),
        (StatusCode::OK, &json!([]))
    );
}

/// The `ETag` of a reply, which must be its body's `version` as a quoted entity tag.
fn etag(reply: &Reply) -> String {
    let tag = reply.header("etag").unwrap().to_owned();
    assert_eq!(tag, format!("\"{}\"", reply.json["version"]), "{reply:?}");
    tag
}

/// An update applies only to the version its `If-Match` names, so the dashboard's replacement of
/// a person's `group_ids` never erases a membership written since it read them: every answer
/// carries the version as `ETag`, a create's replay too; a stale version is
/// `412 precondition_failed` and changes nothing; the current one applies and answers a new
/// version; `*` and an absent header apply to whatever is current; a malformed header is the
/// client's mistake (`400`); and `*` on a person that does not exist is `404`, not a met
/// precondition.
#[tokio::test]
async fn an_update_applies_only_to_the_version_it_names() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let a = create(&app, &acme.key, "/v1/groups", json!({ "name": "A" })).await;
    let b = create(&app, &acme.key, "/v1/groups", json!({ "name": "B" })).await;
    let create_ada = || {
        app.post("/v1/people")
            .bearer(&acme.key)
            .idempotency("create-ada")
            .json(json!({ "email": "ada@example.com", "group_ids": [a] }))
            .send()
    };
    let created = create_ada().await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.json);
    let replayed = create_ada().await;
    assert_eq!(replayed.header("idempotent-replayed"), Some("true"));
    assert_eq!(etag(&replayed), etag(&created));
    let path = format!("/v1/people/{}", created.json["id"].as_str().unwrap());
    let patch = |if_match: Option<&str>, body: Value| {
        let call = app.patch(&path).bearer(&acme.key).json(body);
        match if_match {
            Some(tag) => call.header("if-match", tag),
            None => call,
        }
        .send()
    };
    let read = app.get(&path).bearer(&acme.key).send().await;
    let stale = etag(&read);
    assert_eq!(stale, etag(&created));

    let joined = patch(Some(&stale), json!({ "group_ids": [a, b] })).await;
    assert_eq!(joined.status, StatusCode::OK, "{:?}", joined.json);
    let current = etag(&joined);
    assert_ne!(current, stale);

    let refused = patch(Some(&stale), json!({ "group_ids": [a] })).await;
    assert_eq!(
        problem(&refused),
        (StatusCode::PRECONDITION_FAILED, "precondition_failed", "")
    );
    let unchanged = app.get(&path).bearer(&acme.key).send().await;
    assert_eq!(unchanged.json["group_ids"], json!([a, b]));
    assert_eq!(etag(&unchanged), current);

    let any = patch(Some("*"), json!({ "company": "Analytical" })).await;
    assert_eq!(any.status, StatusCode::OK, "{:?}", any.json);
    assert_ne!(etag(&any), current);
    let unconditional = patch(None, json!({ "company": "Engines" })).await;
    assert_eq!(unconditional.json["company"], "Engines");

    let malformed = patch(Some("17"), json!({ "company": "Other" })).await;
    assert_eq!(
        problem(&malformed),
        (StatusCode::BAD_REQUEST, "invalid_request", "")
    );
    let missing = app
        .patch(&format!("/v1/people/{}", Id::<Person>::new()))
        .bearer(&acme.key)
        .header("if-match", "*")
        .json(json!({ "company": "Other" }))
        .send()
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// An import that adds a membership to a person who exists already moves that person's version
/// in the transaction that adds it, so the dashboard, still holding the `ETag` it read before
/// the import, is refused with `412` when it replaces `group_ids`, instead of erasing the
/// imported membership.
#[tokio::test]
async fn an_import_moves_the_version_of_the_people_it_adds_to_a_group() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let a = create(&app, &acme.key, "/v1/groups", json!({ "name": "A" })).await;
    let imported = create(&app, &acme.key, "/v1/groups", json!({ "name": "Fair" })).await;
    let person = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ada@example.com", "group_ids": [a] }),
    )
    .await;
    let path = format!("/v1/people/{person}");
    let before = etag(&app.get(&path).bearer(&acme.key).send().await);

    let accepted = post(
        &app,
        &acme.key,
        "/v1/imports",
        json!({ "people": [{ "email": "Ada@example.com" }], "group_id": imported }),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{:?}", accepted.json);
    assert_eq!(run(&harness(&test), Queue::Imports).await, ["done"]);
    let after = app.get(&path).bearer(&acme.key).send().await;
    assert_eq!(after.json["group_ids"], json!([a, imported]));
    assert_ne!(etag(&after), before);

    let erasing = app
        .patch(&path)
        .bearer(&acme.key)
        .header("if-match", &before)
        .json(json!({ "group_ids": [a] }))
        .send()
        .await;
    assert_eq!(erasing.status, StatusCode::PRECONDITION_FAILED);
    let kept = app.get(&path).bearer(&acme.key).send().await;
    assert_eq!(kept.json["group_ids"], json!([a, imported]));
}

/// A field's rules protect stored values and segments: a key that names a person attribute
/// and an enum without options are refused; a taken key is a conflict; an option a person
/// holds cannot be removed; a field a segment uses cannot be deleted; deleting a field removes
/// its values from every person.
#[tokio::test]
async fn fields_keep_values_and_segments_valid() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let reserved = post(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "email", "label": "E", "type": "text" }),
    )
    .await;
    assert_eq!(
        problem(&reserved),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "/key"
        )
    );
    let bare = post(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "enum" }),
    )
    .await;
    assert_eq!(
        problem(&bare),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "/options"
        )
    );
    let tier = create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "enum", "options": ["gold", "silver"] }),
    )
    .await;
    let twin = post(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Again", "type": "text" }),
    )
    .await;
    assert_eq!(problem(&twin).0, StatusCode::CONFLICT);
    let person = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "ada@example.com", "fields": { "tier": "gold" } }),
    )
    .await;

    let path = format!("/v1/fields/{tier}");
    let removing = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "options": ["silver", "bronze"] }))
        .send()
        .await;
    assert_eq!(
        problem(&removing),
        (StatusCode::CONFLICT, "invalid_state", "")
    );
    let adding = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "label": "Level", "options": ["gold", "silver", "bronze"] }))
        .send()
        .await;
    assert_eq!(
        (&adding.json["label"], &adding.json["options"]),
        (&json!("Level"), &json!(["gold", "silver", "bronze"]))
    );

    let segment = create(&app, &acme.key, "/v1/segments", json!({ "name": "Gold", "filter": { "conditions": [{ "field": "fields.tier", "operator": "equals", "value": "gold" }] } })).await;
    let used = app.delete(&path).bearer(&acme.key).send().await;
    assert_eq!(problem(&used), (StatusCode::CONFLICT, "invalid_state", ""));
    assert_eq!(
        app.delete(&format!("/v1/segments/{segment}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.delete(&path).bearer(&acme.key).send().await.status,
        StatusCode::NO_CONTENT
    );
    let after = app
        .get(&format!("/v1/people/{person}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(after.json["fields"], json!({}));
    let listed = app.get("/v1/fields").bearer(&acme.key).send().await;
    assert_eq!(listed.json["data"], json!([]));
}

/// A segment means the people its filter matches at each read: its preview is the people list
/// with `segment_id`, its retrieve counts them now, a list leaves the count out, and a filter
/// that does not fit the workspace's fields is refused at its pointer.
#[tokio::test]
async fn segments_select_people_by_their_filter() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "enum", "options": ["gold", "silver"] }),
    )
    .await;
    create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "employees", "label": "Employees", "type": "number" }),
    )
    .await;
    for person in [
        json!({ "email": "ada@acme.com", "company": "Acme", "fields": { "tier": "gold", "employees": 50 } }),
        json!({ "email": "bob@acme.com", "company": "Acme", "fields": { "tier": "silver" } }),
        json!({ "email": "cy@other.org", "fields": { "tier": "gold", "employees": 5 } }),
    ] {
        create(&app, &acme.key, "/v1/people", person).await;
    }
    let gold_at_acme = create(
        &app,
        &acme.key,
        "/v1/segments",
        json!({ "name": "Gold at Acme", "filter": { "match": "all", "conditions": [
        { "field": "email_domain", "operator": "equals", "value": "ACME.com" },
        { "field": "fields.tier", "operator": "equals", "value": "gold" }
    ] } }),
    )
    .await;
    let either = create(
        &app,
        &acme.key,
        "/v1/segments",
        json!({ "name": "Either", "filter": { "match": "any", "conditions": [
        { "field": "company", "operator": "not_exists" },
        { "field": "fields.employees", "operator": "gt", "value": 10 }
    ] } }),
    )
    .await;
    let members = |segment: String| {
        let app = &app;
        let key = acme.key.clone();
        async move {
            emails(
                &app.get(&format!("/v1/people?segment_id={segment}&order=asc"))
                    .bearer(&key)
                    .send()
                    .await,
            )
        }
    };
    assert_eq!(members(gold_at_acme).await, ["ada@acme.com"]);
    assert_eq!(
        members(either.clone()).await,
        ["ada@acme.com", "cy@other.org"]
    );

    let counted = app
        .get(&format!("/v1/segments/{either}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (
            &counted.json["people_count"],
            &counted.json["people_count_capped"]
        ),
        (&json!(2), &json!(false))
    );
    assert!(counted.json["computed_at"].is_string());
    let listed = app.get("/v1/segments").bearer(&acme.key).send().await;
    assert_eq!(listed.json["data"][0]["people_count"], Value::Null);

    let invalid = post(&app, &acme.key, "/v1/segments", json!({ "name": "Bad", "filter": { "conditions": [{ "field": "fields.size", "operator": "equals", "value": 1 }] } })).await;
    assert_eq!(
        problem(&invalid),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "/filter/conditions/0/field"
        )
    );
    let absent = Id::<Segment>::new();
    assert_eq!(
        app.get(&format!("/v1/people?segment_id={absent}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// An address is suppressed once: a manual suppression is created (`suppression.created` is
/// recorded), a second one for the same address in another case is a conflict, and a reason
/// other than `manual` is refused on create. Only a manual suppression lifts, and the removal
/// is audited; one that evidence created is read-only.
#[tokio::test]
async fn suppressions_are_unique_and_only_manual_ones_lift() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let created = post(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "email": "Stop@Example.com" }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(
        (&created.json["reason"], &created.json["source"]),
        (&json!("manual"), &json!("manual"))
    );
    let id = created.json["id"].as_str().unwrap().to_owned();
    assert!(events(&test, acme.id).await.contains(&(
        "suppression.created".to_owned(),
        json!({ "suppression_id": id, "reason": "manual", "source": "manual" })
    )));
    let twin = post(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "email": "stop@example.com" }),
    )
    .await;
    assert_eq!(problem(&twin).0, StatusCode::CONFLICT);
    assert!(twin.json["detail"].as_str().unwrap().contains(&id));
    let bounce = post(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "email": "x@example.com", "reason": "bounce" }),
    )
    .await;
    assert_eq!(
        problem(&bounce),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "/reason"
        )
    );

    let evidence = sqlx::query_scalar!(
        "INSERT INTO suppressions (workspace_id, email, reason, source, evidence, created_by)
         VALUES ($1, 'gone@example.com', 'bounce', 'smtp', '{\"enhanced_status\": \"5.1.1\"}', 'system') RETURNING id",
        acme.id.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let evidence = Id::<Suppression>::from_uuid(evidence);
    let read_only = app
        .delete(&format!("/v1/suppressions/{evidence}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        problem(&read_only),
        (StatusCode::CONFLICT, "invalid_state", "")
    );
    let bounces = app
        .get("/v1/suppressions?reason=bounce&source=smtp")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        bounces.json["data"][0]["evidence"],
        json!({ "enhanced_status": "5.1.1" })
    );

    let path = format!("/v1/suppressions/{id}");
    assert_eq!(
        app.delete(&path).bearer(&acme.key).send().await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.get(&path).bearer(&acme.key).send().await.status,
        StatusCode::NOT_FOUND
    );
    let audited = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM audit_log WHERE workspace_id = $1 AND action = 'suppression.deleted' AND target = $2
             AND actor_kind = 'api_key' AND actor_id = (SELECT 'key_' || replace(id::text, '-', '') FROM api_keys WHERE workspace_id = $1 LIMIT 1)
             AND details @> '{"email":"Stop@Example.com","reason":"manual"}'::jsonb"#,
        acme.id.uuid(),
        id
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

/// The status and detail of the enrollment of the person at `email` in `workspace`.
async fn enrollment_of(
    test: &TestDb,
    workspace: WorkspaceId,
    email: &str,
) -> (String, Option<String>) {
    sqlx::query_as(
        "SELECT e.status, e.status_detail FROM enrollments e JOIN people p ON p.workspace_id = e.workspace_id AND p.id = e.person_id
          WHERE e.workspace_id = $1 AND p.email = $2",
    )
    .bind(workspace.uuid())
    .bind(email)
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// A list of addresses is suppressed in one call, as one decision with the effects of a single
/// suppression for each address it suppresses: the suppression (`manual`, as first spelled),
/// its `suppression.created` event, and the end of its person's live enrollments. An address
/// suppressed already is counted and left as it was; one given twice in another case counts
/// once; an entry that is not an address is reported with its position, the entry and why, and
/// stops nothing else. The answer is what a client shows a person, so its counts must be the
/// writes. Tenancy holds: another workspace's suppression of an address does not count here, and
/// its enrollment of the same address keeps running.
#[tokio::test]
async fn a_list_of_addresses_is_suppressed_in_one_call() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let app = test.app();
    for (workspace, mailbox) in [(acme.id, "a@acme.example"), (globex.id, "a@globex.example")] {
        let sender = test.sender(workspace, &SenderSpec::mailbox(mailbox)).await;
        let campaign = test.campaign(workspace, &sender, None).await;
        for person in ["ada@example.com", "cy@example.com"] {
            test.campaign_message(workspace, campaign, &sender, person, 3600)
                .await;
        }
    }
    create(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "email": "old@example.com" }),
    )
    .await;
    create(
        &app,
        &globex.key,
        "/v1/suppressions",
        json!({ "email": "bob@example.com" }),
    )
    .await;
    let before = events(&test, acme.id).await.len();

    let reply = post(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "emails": [
            "ada@example.com", "bob@example.com", "OLD@example.com", "not an address",
            "ADA@example.com", "@example.com"
        ] }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json);
    assert_eq!(
        reply.json,
        json!({
            "created": 2,
            "already": 1,
            "invalid": [
                { "index": 3, "value": "not an address", "detail": "an email address cannot contain spaces, control characters or angle brackets" },
                { "index": 5, "value": "@example.com", "detail": "an email address needs exactly one `@` with text on both sides" }
            ]
        })
    );

    let listed = app
        .get("/v1/suppressions?order=asc")
        .bearer(&acme.key)
        .send()
        .await;
    let rows: Vec<(&str, &str, &str)> = listed.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["email"].as_str().unwrap(),
                row["reason"].as_str().unwrap(),
                row["source"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("old@example.com", "manual", "manual"),
            ("ada@example.com", "manual", "manual"),
            ("bob@example.com", "manual", "manual"),
        ]
    );
    let created: Vec<Value> = listed.json["data"].as_array().unwrap()[1..]
        .iter()
        .map(|row| json!({ "suppression_id": row["id"], "reason": "manual", "source": "manual" }))
        .collect();
    let told: Vec<Value> = events(&test, acme.id).await[before..]
        .iter()
        .filter(|(kind, _)| kind == "suppression.created")
        .map(|(_, data)| data.clone())
        .collect();
    assert_eq!(told, created);

    assert_eq!(
        enrollment_of(&test, acme.id, "ada@example.com").await,
        (
            "stopped".to_owned(),
            Some("The address is suppressed.".to_owned())
        )
    );
    assert_eq!(
        enrollment_of(&test, acme.id, "cy@example.com").await,
        ("active".to_owned(), None)
    );
    assert_eq!(
        enrollment_of(&test, globex.id, "ada@example.com").await,
        ("active".to_owned(), None)
    );
}

/// The body takes exactly one form, within its bounds: `email` and `emails` together, or
/// neither, is refused as such; a list of more than 1,000 entries (the bound on one decision) or
/// of none is refused whole, as is a reason other than `manual`. A refused request suppresses
/// nothing, so a client can correct it and send it again.
#[tokio::test]
async fn a_suppression_body_takes_exactly_one_form_within_its_bounds() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let too_many: Vec<String> = (0..=1_000)
        .map(|n| format!("person{n}@example.com"))
        .collect();
    for (body, pointer) in [
        (
            json!({ "email": "a@example.com", "emails": ["b@example.com"] }),
            "",
        ),
        (json!({ "reason": "manual" }), ""),
        (json!({ "emails": too_many }), "/emails"),
        (json!({ "emails": [] }), "/emails"),
        (
            json!({ "emails": ["a@example.com"], "reason": "bounce" }),
            "/reason",
        ),
    ] {
        let reply = post(&app, &acme.key, "/v1/suppressions", body.clone()).await;
        assert_eq!(
            problem(&reply),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                pointer
            ),
            "{body}"
        );
    }
    let listed = app.get("/v1/suppressions").bearer(&acme.key).send().await;
    assert_eq!(listed.json["data"], json!([]));
}

/// A CSV import runs end to end through the runner: the upload answers `202` with the import
/// queued (a retry with the same key replays it), the job reads the header's common spellings,
/// creates people, merges an existing person without clearing what the file leaves empty,
/// skips a repeated address, rejects invalid rows with their row and column, joins the group,
/// completes with `import.completed`, and links the full error report.
#[tokio::test]
async fn a_csv_import_runs_end_to_end() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    custom_fields(&app, &acme.key).await;
    let group = create(&app, &acme.key, "/v1/groups", json!({ "name": "Imported" })).await;
    create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "existing@example.com", "family_name": "Person", "company": "Old" }),
    )
    .await;
    let csv = "\u{feff}Email,First Name,Company,Employees,Tier,Phone\n\
               ada@example.com,Ada,Analytical,12,gold,555\n\
               grace@example.com,Grace,,many,silver,\n\
               not-an-address,Bad,,,,\n\
               ADA@example.com,Ada again,,,,\n\
               linus@example.com,,Kernel,3,Platinum,\n\
               existing@example.com,Exi,,,,\n";
    let upload = |key: &'static str| {
        app.post(&format!("/v1/imports?group_id={group}"))
            .bearer(&acme.key)
            .idempotency(key)
            .raw("text/csv", csv.as_bytes().to_vec())
            .send()
    };
    let accepted = upload("import-1").await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{:?}", accepted.json);
    let id = accepted.json["id"].as_str().unwrap().to_owned();
    assert_eq!(
        accepted.header("location"),
        Some(format!("/v1/imports/{id}").as_str())
    );
    assert_eq!(
        (&accepted.json["status"], &accepted.json["format"]),
        (&json!("queued"), &json!("csv"))
    );
    let again = upload("import-1").await;
    assert_eq!(
        (again.header("idempotent-replayed"), &again.json["id"]),
        (Some("true"), &json!(id))
    );

    assert_eq!(run(&harness(&test), Queue::Imports).await, ["done"]);
    let import = app
        .get(&format!("/v1/imports/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        import.json["status"],
        json!("completed"),
        "{:?}",
        import.json
    );
    assert_eq!(
        import.json["counts"],
        json!({ "total": 6, "imported": 2, "skipped": 1, "invalid": 3 })
    );
    assert_eq!(
        import.json["errors"]["data"],
        json!([
            { "row": 3, "field": "fields.employees", "problem": "expected a number" },
            { "row": 4, "field": "email", "problem": "an email address needs exactly one `@` with text on both sides" },
            { "row": 6, "field": "fields.tier", "problem": "`Platinum` is not one of the field's options" }
        ])
    );
    assert_eq!(import.json["errors"]["has_more"], json!(false));

    let ada = app
        .get("/v1/people?email=ada@example.com")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (
            &ada.json["data"][0]["given_name"],
            &ada.json["data"][0]["company"],
            &ada.json["data"][0]["fields"],
            &ada.json["data"][0]["group_ids"]
        ),
        (
            &json!("Ada"),
            &json!("Analytical"),
            &json!({ "employees": 12, "tier": "gold" }),
            &json!([group])
        )
    );
    let existing = app
        .get("/v1/people?email=existing@example.com")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (
            &existing.json["data"][0]["given_name"],
            &existing.json["data"][0]["family_name"],
            &existing.json["data"][0]["company"]
        ),
        (&json!("Exi"), &json!("Person"), &json!("Old"))
    );
    let grouped = app
        .get(&format!("/v1/groups/{group}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(grouped.json["people_count"], json!(2));
    assert!(events(&test, acme.id).await.contains(&(
        "import.completed".to_owned(),
        json!({ "import_id": id, "total": 6, "imported": 2, "skipped": 1, "invalid": 3 })
    )));

    let report = app
        .get(&local(import.json["errors"]["url"].as_str().unwrap()))
        .send()
        .await;
    assert_eq!(report.status, StatusCode::OK);
    assert_eq!(
        report.json,
        json!(
            "row,field,problem\n3,fields.employees,expected a number\n4,email,an email address needs exactly one `@` with text on both sides\n6,fields.tier,`Platinum` is not one of the field's options\n"
        )
    );
}

/// A CSV file larger than the default 1 MiB body reaches the import (uploads are bounded at
/// 16 MiB, refused above with `413`), and the job works through it in many chunks of 2,000
/// rows in one run, skipping an address the file repeats in a later chunk.
#[tokio::test]
async fn a_large_csv_import_is_read_in_chunks() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mut csv = String::from("email,first_name,notes\n");
    for index in 0..25_000 {
        csv.push_str(&format!(
            "p{index}@example.com,Person {index},padding-padding-padding\n"
        ));
    }
    csv.push_str("P0@example.com,Repeated,\n");
    assert!(csv.len() > 1 << 20);
    let accepted = app
        .post("/v1/imports")
        .bearer(&acme.key)
        .idempotency("large")
        .raw("text/csv", csv.into_bytes())
        .send()
        .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{:?}", accepted.json);
    let id = accepted.json["id"].as_str().unwrap().to_owned();
    assert_eq!(run(&harness(&test), Queue::Imports).await, ["done"]);
    let import = app
        .get(&format!("/v1/imports/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (
            &import.json["status"],
            &import.json["counts"],
            &import.json["errors"]["url"]
        ),
        (
            &json!("completed"),
            &json!({ "total": 25_001, "imported": 25_000, "skipped": 1, "invalid": 0 }),
            &Value::Null
        )
    );
    let p0 = app
        .get("/v1/people?email=p0@example.com")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(p0.json["data"][0]["given_name"], json!("Person 0"));

    let oversized = app
        .post("/v1/imports")
        .bearer(&acme.key)
        .idempotency("oversized")
        .raw("text/csv", vec![b'a'; (16 << 20) + 1])
        .send()
        .await;
    assert_eq!(problem(&oversized).0, StatusCode::PAYLOAD_TOO_LARGE);
}

/// A JSON import reads people with the API's types: the valid one is imported, and a person
/// with a mistyped value or an undefined field is reported at its position.
#[tokio::test]
async fn a_json_import_reads_typed_people() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "employees", "label": "Employees", "type": "number" }),
    )
    .await;
    let accepted = post(
        &app,
        &acme.key,
        "/v1/imports",
        json!({ "people": [
        { "email": "ada@example.com", "fields": { "employees": 12 } },
        { "email": "bob@example.com", "fields": { "employees": "12" } },
        { "email": "cy@example.com", "fields": { "phone": "1" } }
    ] }),
    )
    .await;
    assert_eq!(
        (accepted.status, &accepted.json["format"]),
        (StatusCode::ACCEPTED, &json!("json"))
    );
    assert_eq!(run(&harness(&test), Queue::Imports).await, ["done"]);
    let import = app
        .get(&format!(
            "/v1/imports/{}",
            accepted.json["id"].as_str().unwrap()
        ))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        import.json["counts"],
        json!({ "total": 3, "imported": 1, "skipped": 0, "invalid": 2 })
    );
    let rows: Vec<(i64, &str)> = import.json["errors"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| {
            (
                error["row"].as_i64().unwrap(),
                error["field"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(rows, [(2, "fields.employees"), (3, "fields.phone")]);
}

/// A file that cannot be imported as a whole is refused before anything is stored (no email
/// column: `422`), and an import whose file is gone from object storage ends `failed` with
/// `file_missing` rather than retrying forever.
#[tokio::test]
async fn an_unreadable_import_is_refused_or_fails() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let headless = app
        .post("/v1/imports")
        .bearer(&acme.key)
        .idempotency("no-email")
        .raw("text/csv", b"name,company\nAda,Acme\n".to_vec())
        .send()
        .await;
    assert_eq!(problem(&headless).0, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        headless.json["errors"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("email column")
    );

    let accepted = app
        .post("/v1/imports")
        .bearer(&acme.key)
        .idempotency("gone")
        .raw("text/csv", b"email\nada@example.com\n".to_vec())
        .send()
        .await;
    let id = accepted.json["id"].as_str().unwrap();
    let uuid = id.parse::<Id<Import>>().unwrap().uuid();
    test.storage
        .delete(&format!("imports/{}/{uuid}/source.csv", acme.id.uuid()))
        .await
        .unwrap();
    assert_eq!(run(&harness(&test), Queue::Imports).await, ["done"]);
    let import = app
        .get(&format!("/v1/imports/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (&import.json["status"], &import.json["last_error"]["code"]),
        (&json!("failed"), &json!("file_missing"))
    );
}

/// An export runs end to end through the runner: requested with the people list's filters
/// (`202`), written by the job, then `ready` with its row count, `export.completed` recorded,
/// and a link that downloads the CSV (on the local store, through the api's signed route,
/// which refuses an altered link).
#[tokio::test]
async fn an_export_is_written_and_downloaded_by_its_link() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    create(
        &app,
        &acme.key,
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "text" }),
    )
    .await;
    let group = create(&app, &acme.key, "/v1/groups", json!({ "name": "Exported" })).await;
    let ada = create(&app, &acme.key, "/v1/people", json!({ "email": "ada@example.com", "given_name": "Ada", "fields": { "tier": "gold, first" }, "group_ids": [group] })).await;
    let bob = create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "bob@example.com", "group_ids": [group] }),
    )
    .await;
    create(
        &app,
        &acme.key,
        "/v1/people",
        json!({ "email": "cy@example.com" }),
    )
    .await;

    let accepted = post(
        &app,
        &acme.key,
        "/v1/exports",
        json!({ "resource": "people", "filters": { "group_id": group } }),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{:?}", accepted.json);
    let id = accepted.json["id"].as_str().unwrap().to_owned();
    assert_eq!(
        accepted.header("location"),
        Some(format!("/v1/exports/{id}").as_str())
    );
    assert_eq!(
        (&accepted.json["status"], &accepted.json["url"]),
        (&json!("queued"), &Value::Null)
    );

    assert_eq!(run(&harness(&test), Queue::Exports).await, ["done"]);
    let export = app
        .get(&format!("/v1/exports/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        (&export.json["status"], &export.json["rows"]),
        (&json!("ready"), &json!(2)),
        "{:?}",
        export.json
    );
    assert!(events(&test, acme.id).await.contains(&(
        "export.completed".to_owned(),
        json!({ "export_id": id, "rows": 2 })
    )));

    let link = local(export.json["url"].as_str().unwrap());
    let file = app.get(&link).send().await;
    assert_eq!(
        (file.status, file.header("content-type")),
        (StatusCode::OK, Some("text/csv; charset=utf-8"))
    );
    let text = file.json.as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "id,email,given_name,family_name,company,group_ids,last_sent_at,replied_at,created_at,updated_at,tier"
    );
    assert_eq!(lines.len(), 3);
    // The export walks ids, which need not follow insertion order across database sessions.
    let (ada_row, bob_row) = if ada < bob {
        (lines[1], lines[2])
    } else {
        (lines[2], lines[1])
    };
    assert!(
        ada_row.starts_with(&format!("{ada},ada@example.com,Ada,,,{group},"))
            && ada_row.ends_with(",\"gold, first\"")
    );
    assert!(bob_row.starts_with(&format!("{bob},bob@example.com,")));
    let altered = link.replacen("signature=", "signature=A", 1);
    assert_eq!(app.get(&altered).send().await.status, StatusCode::NOT_FOUND);
}

/// Preflight reports each address, in order and without an idempotency key (it stores
/// nothing): a malformed address as `syntax`, a suppressed one with its suppression, and a
/// domain DNS cannot answer for as `unknown`, never `invalid`; more than 100 addresses are
/// refused.
#[tokio::test]
async fn preflight_reports_each_address() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let suppression = create(
        &app,
        &acme.key,
        "/v1/suppressions",
        json!({ "email": "blocked@example.com" }),
    )
    .await;
    let checked = app
        .post("/v1/preflight")
        .bearer(&acme.key)
        .json(json!({ "emails": ["not-an-address", "Blocked@Example.com", "ok@example.com"] }))
        .send()
        .await;
    assert_eq!(checked.status, StatusCode::OK, "{:?}", checked.json);
    let data = &checked.json["data"];
    assert_eq!(
        (&data[0]["status"], &data[0]["reason"]),
        (&json!("invalid"), &json!("syntax"))
    );
    assert!(data[0]["detail"].is_string());
    assert_eq!(
        data[1]["suppression"],
        json!({ "id": suppression, "reason": "manual" })
    );
    assert_eq!(
        (
            &data[2]["status"],
            &data[2]["reason"],
            &data[2]["suppression"]
        ),
        (&json!("unknown"), &json!("dns_unavailable"), &Value::Null)
    );
    let many: Vec<String> = (0..101)
        .map(|index| format!("p{index}@example.com"))
        .collect();
    let refused = app
        .post("/v1/preflight")
        .bearer(&acme.key)
        .json(json!({ "emails": many }))
        .send()
        .await;
    assert_eq!(
        problem(&refused),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "/emails"
        )
    );
}

/// In a test-mode workspace preflight checks the syntax only, as that workspace's sender does: an
/// address on a domain DNS cannot answer for is routable there, since its mail never leaves the
/// fake transport and developers write to domains that accept no mail; a malformed address is
/// still refused as `syntax`.
#[tokio::test]
async fn preflight_in_test_mode_checks_only_the_syntax() {
    let test = TestDb::new().await;
    let sandbox = test.test_workspace("sandbox").await;
    let app = test.app();
    let checked = app
        .post("/v1/preflight")
        .bearer(&sandbox.key)
        .json(json!({ "emails": ["dev@example.com", "not-an-address"] }))
        .send()
        .await;
    assert_eq!(checked.status, StatusCode::OK, "{:?}", checked.json);
    let data = &checked.json["data"];
    assert_eq!(
        (&data[0]["status"], &data[0]["reason"]),
        (&json!("routable"), &json!("mx"))
    );
    assert_eq!(
        (&data[1]["status"], &data[1]["reason"]),
        (&json!("invalid"), &json!("syntax"))
    );
}

/// An export holds the rows of its resource's list, so reading it takes that resource's scope:
/// a key that may only read people neither lists nor retrieves a messages export (whose file
/// would hand it every message), and a key that may only read messages sees its messages export
/// and not the people export beside it.
#[tokio::test]
async fn an_export_is_read_with_its_resources_scope() {
    use crate::domain::scope::{Scope, ScopeSet};

    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let people_export = create(
        &app,
        &acme.key,
        "/v1/exports",
        json!({ "resource": "people" }),
    )
    .await;
    let messages_export = create(
        &app,
        &acme.key,
        "/v1/exports",
        json!({ "resource": "messages" }),
    )
    .await;
    let people_reader = test
        .api_key(&acme, [Scope::PeopleRead].into_iter().collect::<ScopeSet>())
        .await;
    let messages_reader = test
        .api_key(
            &acme,
            [Scope::MessagesRead].into_iter().collect::<ScopeSet>(),
        )
        .await;

    let ids = |reply: &Reply| -> Vec<String> {
        reply.json["data"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let listed = app.get("/v1/exports").bearer(&people_reader).send().await;
    assert_eq!(ids(&listed), std::slice::from_ref(&people_export));
    let listed = app.get("/v1/exports").bearer(&messages_reader).send().await;
    assert_eq!(ids(&listed), std::slice::from_ref(&messages_export));

    let refused = app
        .get(&format!("/v1/exports/{messages_export}"))
        .bearer(&people_reader)
        .send()
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let allowed = app
        .get(&format!("/v1/exports/{messages_export}"))
        .bearer(&messages_reader)
        .send()
        .await;
    assert_eq!(allowed.status, StatusCode::OK);
}
