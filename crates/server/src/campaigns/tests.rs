//! The campaign tests: store behavior against real PostgreSQL (the creation pass under the
//! campaign's lock, settling, removals against Starts, the stop rules) and the API in process
//! (the contract, versions, tenancy with forged ids, the step-content form of messages).
//!
//! The pure decisions (allocation, winner selection, rotation, the removal rule, what follows a
//! message, the grid and windows) are tested beside their code in `domain/`; these tests prove
//! what only the database, the router and the jobs together can. The jobs run through the
//! runner's own steps ([`Harness`]) on the worker's login.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::advance::{self, EnrollmentAdvance};
use super::creator::{self, CHUNK};
use super::enrollments::{self, EnrollmentAdd};
use super::generate::MessageGenerate;
use super::materialise::CampaignMaterialise;
use super::removal::{self, Scope, SendersRemoved};
use super::steps::{self, BODY_MAX, CONTENT_MAX, STEPS_MAX, VARIANTS_MAX};
use crate::domain::ids::{Campaign, Id, Person, SenderIdentity, WorkspaceId};
use crate::jobs::runner::Harness;
use crate::jobs::{Queue, Registry};
use crate::mcp::MAX_RESPONSE;
use crate::testing::{Reply, SenderSpec, TestApp, TestDb, TestSender, TestWorkspace, keys};

/// The campaign kinds on the test's database, with the deployment's keys.
fn harness(test: &TestDb) -> Harness {
    let mut registry = Registry::default();
    registry
        .register::<CampaignMaterialise>()
        .unwrap()
        .register::<EnrollmentAdd>()
        .unwrap()
        .register::<EnrollmentAdvance>()
        .unwrap()
        .register::<SendersRemoved>()
        .unwrap()
        .register::<MessageGenerate>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(keys());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "campaigns-test",
    )
}

/// One workspace with two mailboxes, `a` and `b`.
struct Fixture {
    test: TestDb,
    ws: TestWorkspace,
    app: TestApp,
    a: TestSender,
    b: TestSender,
}

async fn fixture(slug: &str) -> Fixture {
    let test = TestDb::new().await;
    let ws = test.workspace(slug).await;
    let app = test.app();
    let a = test
        .sender(ws.id, &SenderSpec::mailbox(&format!("a@{slug}.example")))
        .await;
    let b = test
        .sender(ws.id, &SenderSpec::mailbox(&format!("b@{slug}.example")))
        .await;
    Fixture {
        test,
        ws,
        app,
        a,
        b,
    }
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

/// `PATCH path` with `body`, and `If-Match` when given.
async fn patch(app: &TestApp, key: &str, path: &str, body: Value, version: Option<i64>) -> Reply {
    let call = app.patch(path).bearer(key).json(body);
    match version {
        Some(version) => {
            call.header("if-match", &format!("\"{version}\""))
                .send()
                .await
        }
        None => call.send().await,
    }
}

/// The status and code of a reply, and its first pointer.
fn problem(reply: &Reply) -> (StatusCode, String, String) {
    (
        reply.status,
        reply.json["code"].as_str().unwrap_or_default().to_owned(),
        reply.json["errors"][0]["pointer"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// The uuid of a wire id (`cmp_…`, `per_…`).
fn uuid(id: &Value) -> Uuid {
    let text = id.as_str().unwrap();
    let (_, hex) = text.split_once('_').unwrap();
    Uuid::parse_str(hex).unwrap()
}

/// A campaign of two steps (the first with two variants) whose pool names `identities` and
/// selects `tags`.
fn two_steps(identities: &[Id<SenderIdentity>], tags: &[&str]) -> Value {
    json!({
        "name": "Launch",
        "steps": [
            {"name": "Intro", "variants": [
                {"subject": "Hi {{ person.given_name | default(\"there\") }}", "html": "<p>Hello {{ person.given_name | default(\"there\") }}</p>"},
                {"subject": "Hello", "html": "<p>Hey</p>"}
            ]},
            {"name": "Follow-up", "delay_seconds": 86400, "variants": [{"subject": "Re: hi", "html": "<p>Any news?</p>"}]}
        ],
        "senders": {"identity_ids": identities.iter().map(ToString::to_string).collect::<Vec<_>>(), "tags": tags},
    })
}

/// The body of a new variant whose template is [`SMALL_HTML`].
fn small_variant() -> Value {
    json!({"subject": "Hi", "html": SMALL_HTML})
}

/// The body of the variants [`wide`] makes.
const SMALL_HTML: &str = "<p>Hi</p>";

/// `count` new steps (`Step 1`, `Step 2`, …) of `variants` small variants each.
fn wide(count: usize, variants: usize) -> Vec<Value> {
    (1..=count)
        .map(|position| {
            json!({"name": format!("Step {position}"),
                   "variants": (0..variants).map(|_| small_variant()).collect::<Vec<_>>()})
        })
        .collect()
}

/// The content `campaign`'s steps hold, as the bound counts it.
async fn held(f: &Fixture, campaign: &Value) -> i64 {
    let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
    let bytes = steps::content(&mut tx, f.ws.id, Id::from_uuid(uuid(&campaign["id"])))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    bytes
}

/// Creates a campaign and returns its object.
async fn campaign(f: &Fixture, body: Value) -> Value {
    let reply = post(&f.app, &f.ws.key, "/v1/campaigns", body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json);
    reply.json
}

/// Creates people (`given_name@<domain>`, given name capitalised) and returns their ids.
async fn people(f: &Fixture, names: &[&str], domain: &str) -> Vec<Value> {
    let mut ids = Vec::new();
    for name in names {
        let reply = post(
            &f.app,
            &f.ws.key,
            "/v1/people",
            json!({"email": format!("{name}@{domain}"), "given_name": name}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json);
        ids.push(reply.json["id"].clone());
    }
    ids
}

/// Enrolls `people` into `campaign` inline.
async fn enroll(f: &Fixture, campaign: &Value, people: &[Value]) -> Reply {
    post(
        &f.app,
        &f.ws.key,
        "/v1/enrollments",
        json!({"campaign_id": campaign, "person_ids": people}),
    )
    .await
}

/// Starts `campaign` and runs its `campaign.materialise`: active, its due messages created.
async fn start(f: &Fixture, campaign: &Value) {
    let path = format!("/v1/campaigns/{}/start", campaign.as_str().unwrap());
    let reply = post(&f.app, &f.ws.key, &path, json!({})).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json);
    let outcomes = harness(&f.test).run_once(Queue::Enrollment, 4).await;
    assert!(
        outcomes.iter().all(|(_, outcome)| *outcome == "done"),
        "{outcomes:?}"
    );
}

/// Runs one creation pass over `campaign` in the worker's own transaction.
async fn pass(f: &Fixture, campaign: Option<Uuid>) -> creator::Pass {
    let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
    let pass = creator::pass(
        &mut tx,
        &keys(),
        f.ws.id,
        campaign.map(Id::from_uuid),
        jiff::Timestamp::now(),
        None,
        CHUNK,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    pass
}

/// A count read as the system role.
async fn count(f: &Fixture, sql: &'static str, id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(id)
        .fetch_one(f.test.system.pool())
        .await
        .unwrap()
}

/// A count read as the system role, with nothing bound.
async fn total(f: &Fixture, sql: &'static str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(f.test.system.pool())
        .await
        .unwrap()
}

/// Runs `sql` as the system role with one uuid bound.
async fn exec(f: &Fixture, sql: &'static str, id: Uuid) {
    sqlx::query(sql)
        .bind(id)
        .execute(f.test.system.pool())
        .await
        .unwrap();
}

/// The current message of `person`'s enrollment.
async fn message_of(f: &Fixture, person: &Value) -> Uuid {
    sqlx::query_scalar::<_, Uuid>("SELECT message_id FROM enrollments WHERE person_id = $1")
        .bind(uuid(person))
        .fetch_one(f.test.system.pool())
        .await
        .unwrap()
}

/// `person`'s enrollment: status, position, unstarted failures, whether its message pointer is
/// clear, whether its next step is due a day on, and its detail.
async fn state(f: &Fixture, person: &Value) -> (String, i32, i16, bool, bool, Option<String>) {
    sqlx::query_as::<_, (String, i32, i16, bool, bool, Option<String>)>(
        "SELECT status, current_position, attempts, message_id IS NULL,
                coalesce(next_run_at > now() + interval '22 hours', false), status_detail
           FROM enrollments WHERE person_id = $1",
    )
    .bind(uuid(person))
    .fetch_one(f.test.system.pool())
    .await
    .unwrap()
}

/// The live message of each enrollment of `campaign`: `(enrollment, message, identity, thread)`.
async fn live(f: &Fixture, campaign: Uuid) -> Vec<(Uuid, Uuid, Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid)>(
        "SELECT m.enrollment_id, m.id, m.sender_identity_id, m.thread_id FROM messages m
          WHERE m.campaign_id = $1 AND m.state <> 'cancelled' ORDER BY m.enrollment_id",
    )
    .bind(campaign)
    .fetch_all(f.test.system.pool())
    .await
    .unwrap()
}

// ───────────────────────────── the campaigns resource ─────────────────────────────

/// `steps` replaces the ordered list with the merge rules a client relies on: a step or variant
/// given by `id` alone stays exactly as it is (same revision), a changed step gets a new revision
/// (and a changed variant a new version) while a reorder or rename does not, a variant left out
/// is no longer offered, an unsent step left out is removed; every write moves the campaign's
/// version, and a stale `If-Match` changes nothing (`412`). Variants written together each keep
/// their own content and copies and list in creation order. A list leaves the bodies out.
#[tokio::test]
async fn steps_are_merged_by_id_and_revised_only_when_their_configuration_changes() {
    let f = fixture("cmp-steps").await;
    let created = campaign(&f, two_steps(&[f.a.identity], &[])).await;
    let id = created["id"].as_str().unwrap().to_owned();
    let path = format!("/v1/campaigns/{id}");
    let (s1, s2) = (
        created["steps"][0]["id"].clone(),
        created["steps"][1]["id"].clone(),
    );
    let v1 = created["steps"][0]["variants"][0]["id"].clone();
    assert_eq!(created["status"], "draft");
    assert_eq!(created["steps"][0]["revision"], 1);
    assert_eq!(created["steps"][0]["variants"].as_array().unwrap().len(), 2);
    assert!(created["steps"][0]["variants"][0]["html"].is_string());

    let listed = f.app.get("/v1/campaigns").bearer(&f.ws.key).send().await;
    assert_eq!(listed.status, StatusCode::OK);
    let variant = &listed.json["data"][0]["steps"][0]["variants"][0];
    assert!(variant["subject"].is_string() && variant.get("html").is_none());

    let version = created["version"].as_i64().unwrap();
    let kept = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"steps": [{"id": s1}, {"id": s2}]}),
        Some(version),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK, "{:?}", kept.json);
    assert_eq!(kept.json["steps"][0]["revision"], 1);
    assert_eq!(kept.json["steps"][1]["revision"], 1);
    assert_ne!(kept.json["version"].as_i64().unwrap(), version);
    assert_eq!(
        kept.header("etag"),
        Some(format!("\"{}\"", kept.json["version"]).as_str())
    );
    let stale = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"name": "Lost"}),
        Some(version),
    )
    .await;
    assert_eq!(problem(&stale).0, StatusCode::PRECONDITION_FAILED);

    let moved = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"steps": [{"id": s2, "delay_seconds": 3600, "name": "Nudge"}, {"id": s1}]}),
        None,
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.json);
    assert_eq!(moved.json["steps"][0]["id"], s2);
    assert_eq!(moved.json["steps"][0]["position"], 1);
    assert_eq!(moved.json["steps"][0]["name"], "Nudge");
    assert_eq!(moved.json["steps"][0]["revision"], 2);
    assert_eq!(moved.json["steps"][0]["delay_seconds"], 3600);
    assert_eq!(
        moved.json["steps"][1]["revision"], 1,
        "a reorder alone publishes nothing"
    );

    let edited = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"steps": [{"id": s1, "variants": [{"id": v1, "subject": "New subject"}]}, {"id": s2}]}),
        None,
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.json);
    let step = &edited.json["steps"][0];
    assert_eq!(step["revision"], 2);
    assert_eq!(step["variants"].as_array().unwrap().len(), 1);
    assert_eq!(step["variants"][0]["version"], 2);
    assert_eq!(step["variants"][0]["subject"], "New subject");
    assert_eq!(
        step["variants"][0]["html"],
        "<p>Hello {{ person.given_name | default(\"there\") }}</p>"
    );

    // Variants written together each keep their own content and copies, and list in the order
    // they were created: the kept one first, then the new ones as given.
    let mixed = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"steps": [{"id": s1, "variants": [
            {"subject": "One", "html": "<p>One</p>", "cc": ["a@mixed.example", "b@mixed.example"],
             "bcc": ["c@mixed.example"]},
            {"id": v1, "cc": ["d@mixed.example"]},
            {"subject": "Two", "html": "<p>Two</p>"}
        ]}, {"id": s2}]}),
        None,
    )
    .await;
    assert_eq!(mixed.status, StatusCode::OK, "{:?}", mixed.json);
    let offered: Vec<Value> = mixed.json["steps"][0]["variants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| {
            json!([
                variant["name"],
                variant["subject"],
                variant["version"],
                variant["cc"],
                variant["bcc"]
            ])
        })
        .collect();
    assert_eq!(
        offered,
        [
            json!(["A", "New subject", 3, ["d@mixed.example"], []]),
            json!([
                "A",
                "One",
                1,
                ["a@mixed.example", "b@mixed.example"],
                ["c@mixed.example"]
            ]),
            json!(["C", "Two", 1, [], []]),
        ]
    );

    let removed = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"steps": [{"id": s1}]}),
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK);
    assert_eq!(removed.json["steps"].as_array().unwrap().len(), 1);
}

/// What a save can know is checked when it is saved, each refusal pointing at its field: a
/// template that does not parse, a new step without variants, a personalisation prompt that no
/// variant reads a snippet of; an identity of nobody in the workspace is `404`.
#[tokio::test]
async fn templates_steps_and_prompts_are_checked_when_saved() {
    let f = fixture("cmp-checks").await;
    let cases = [
        (
            json!({"name": "X", "steps": [{"name": "S", "variants": [{"subject": "Hi", "html": "{% if %}"}]}]}),
            "/steps/0/variants/0/html",
        ),
        (
            json!({"name": "X", "steps": [{"name": "S"}]}),
            "/steps/0/variants",
        ),
        (
            json!({"name": "X", "steps": [{"name": "S", "personalisation_prompt": "Write an opener.",
                                           "variants": [{"subject": "Hi", "html": "<p>Hi</p>"}]}]}),
            "/steps/0/personalisation_prompt",
        ),
        (
            json!({"name": "X", "schedule": {"send_window": {"days": [1], "start": "09:03", "end": "17:00"}}}),
            "/schedule/send_window",
        ),
    ];
    for (body, pointer) in cases {
        let reply = post(&f.app, &f.ws.key, "/v1/campaigns", body).await;
        let (status, code, at) = problem(&reply);
        assert_eq!(
            (status, code.as_str(), at.as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                pointer
            ),
            "{:?}",
            reply.json
        );
    }
    let prompted = post(
        &f.app,
        &f.ws.key,
        "/v1/campaigns",
        json!({"name": "X", "steps": [{"name": "S", "personalisation_prompt": "Write an opener.",
               "variants": [{"subject": "Hi", "html": "<p>{{ variables.opener | default(\"Hello\") }}</p>"}]}]}),
    )
    .await;
    assert_eq!(prompted.status, StatusCode::CREATED, "{:?}", prompted.json);
    let unknown = post(
        &f.app,
        &f.ws.key,
        "/v1/campaigns",
        json!({"name": "X", "senders": {"identity_ids": [Id::<SenderIdentity>::from_uuid(Uuid::now_v7()).to_string()]}}),
    )
    .await;
    assert_eq!(problem(&unknown).0, StatusCode::NOT_FOUND);
}

/// Fifty steps of fifty variants each, the most of both, are accepted and read back whole and
/// in order (steps by position, variants as given, every body, and the enrollment summary's
/// fifty steps); one step or one variant more is refused at its pointer (`422` at `/steps` or
/// `/steps/{i}/variants`), on a create and on an update, and the refused update changes nothing.
#[tokio::test]
async fn fifty_steps_of_fifty_variants_are_the_most_a_campaign_holds() {
    let f = fixture("cmp-limits").await;
    let created = campaign(
        &f,
        json!({"name": "Long", "steps": wide(STEPS_MAX, VARIANTS_MAX)}),
    )
    .await;
    let path = format!("/v1/campaigns/{}", created["id"].as_str().unwrap());
    let read = f.app.get(&path).bearer(&f.ws.key).send().await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.json);
    let steps = read.json["steps"].as_array().unwrap();
    assert_eq!(steps.len(), STEPS_MAX);
    let letters: Vec<Value> = ('A'..='Z')
        .map(|letter| json!(letter.to_string()))
        .chain((27..=VARIANTS_MAX).map(|number| json!(format!("Variant {number}"))))
        .collect();
    for (index, step) in steps.iter().enumerate() {
        assert_eq!(step["position"], index + 1);
        assert_eq!(step["name"], format!("Step {}", index + 1));
        let variants = step["variants"].as_array().unwrap();
        assert_eq!(variants.len(), VARIANTS_MAX);
        assert!(variants.iter().all(|variant| variant["html"] == SMALL_HTML));
        assert_eq!(
            variants
                .iter()
                .map(|variant| &variant["name"])
                .collect::<Vec<_>>(),
            letters.iter().collect::<Vec<_>>(),
            "variants list in the order given"
        );
    }
    assert_eq!(
        read.json["enrollments"]["steps"].as_array().unwrap().len(),
        STEPS_MAX
    );

    let mut one_wide = wide(2, 1);
    one_wide[1]["variants"] = json!(vec![small_variant(); VARIANTS_MAX + 1]);
    let refused = [
        (
            json!({"name": "X", "steps": wide(STEPS_MAX + 1, 1)}),
            "/steps",
        ),
        (json!({"name": "X", "steps": one_wide}), "/steps/1/variants"),
    ];
    for (body, pointer) in refused {
        let reply = post(&f.app, &f.ws.key, "/v1/campaigns", body).await;
        let (status, code, at) = problem(&reply);
        assert_eq!(
            (status, code.as_str(), at.as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                pointer
            ),
            "{:?}",
            reply.json
        );
    }
    let mut longer: Vec<Value> = steps.iter().map(|step| json!({"id": step["id"]})).collect();
    longer.push(json!({"name": "One more", "variants": [small_variant()]}));
    let update = patch(&f.app, &f.ws.key, &path, json!({"steps": longer}), None).await;
    assert_eq!(
        problem(&update),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed".to_owned(),
            "/steps".to_owned()
        )
    );
    let after = f.app.get(&path).bearer(&f.ws.key).send().await;
    assert_eq!(after.json["version"], read.json["version"]);
}

/// A campaign's steps hold at most 1.5 MiB of content, counted on what the campaign holds after
/// a change and not only on the request: a request over 1.5 MiB is `413`; updates that keep
/// steps and variants by `id` may fill a campaign of fifty steps of fifty variants up to exactly
/// the bound, but a small update that would pass it by one byte is `422` at `/steps` and changes
/// nothing. At the bound the campaign still answers within the MCP's bound on one answer, which
/// is what makes it one object a client and an AI agent can always read whole.
#[tokio::test]
async fn a_campaign_holds_at_most_its_content_bound_and_answers_within_it() {
    let f = fixture("cmp-content").await;
    let body = "x".repeat(BODY_MAX);
    let oversized = post(
        &f.app,
        &f.ws.key,
        "/v1/campaigns",
        json!({"name": "Big", "steps": [{"name": "S",
               "variants": vec![json!({"subject": "Hi", "html": body}); 7]}]}),
    )
    .await;
    assert_eq!(
        problem(&oversized).0,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{:?}",
        oversized.json
    );

    let created = campaign(
        &f,
        json!({"name": "Full", "steps": wide(STEPS_MAX, VARIANTS_MAX)}),
    )
    .await;
    let path = format!("/v1/campaigns/{}", created["id"].as_str().unwrap());
    let first = &created["steps"][0];
    let variant_ids: Vec<Value> = first["variants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| variant["id"].clone())
        .collect();
    // Replacing a variant's body adds its new length less the old one: the quotes around both
    // count once each.
    let mut room = usize::try_from(CONTENT_MAX - held(&f, &created).await).unwrap();
    let mut variants = Vec::new();
    for id in &variant_ids {
        let added = room.min(BODY_MAX - SMALL_HTML.len());
        room -= added;
        variants.push(if added == 0 {
            json!({"id": id})
        } else {
            json!({"id": id, "html": "x".repeat(SMALL_HTML.len() + added)})
        });
    }
    assert_eq!(room, 0, "fifty bodies hold the room");
    let mut kept: Vec<Value> = created["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| json!({"id": step["id"]}))
        .collect();
    kept[0] = json!({"id": first["id"], "variants": variants});
    let filled = patch(&f.app, &f.ws.key, &path, json!({"steps": kept}), None).await;
    assert_eq!(filled.status, StatusCode::OK, "{:?}", filled.json);
    assert_eq!(held(&f, &created).await, CONTENT_MAX);
    let full = f.app.get(&path).bearer(&f.ws.key).send().await;
    assert_eq!(full.status, StatusCode::OK);
    assert!(
        full.body.len() <= MAX_RESPONSE,
        "a full campaign answers {} bytes",
        full.body.len()
    );

    let last = variant_ids.last().unwrap();
    kept[0] = json!({"id": first["id"], "variants": variant_ids.iter().map(|id| {
        if id == last {
            json!({"id": id, "html": format!("{SMALL_HTML}x")})
        } else {
            json!({"id": id})
        }
    }).collect::<Vec<_>>()});
    let over = patch(&f.app, &f.ws.key, &path, json!({"steps": kept}), None).await;
    assert_eq!(
        problem(&over),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed".to_owned(),
            "/steps".to_owned()
        ),
        "{:?}",
        over.json
    );
    let after = f.app.get(&path).bearer(&f.ws.key).send().await;
    assert_eq!(after.json["version"], full.json["version"]);
    assert_eq!(held(&f, &created).await, CONTENT_MAX);
}

/// `start` and `pause` move a campaign only along its lifecycle (`409 invalid_state` otherwise),
/// a campaign without a sendable step does not start, and the materialise job makes a started
/// campaign `active`.
#[tokio::test]
async fn start_and_pause_follow_the_lifecycle() {
    let f = fixture("cmp-life").await;
    let empty = campaign(&f, json!({"name": "Empty"})).await;
    let action = |id: &Value, verb: &str| format!("/v1/campaigns/{}/{verb}", id.as_str().unwrap());
    let refused = post(&f.app, &f.ws.key, &action(&empty["id"], "start"), json!({})).await;
    assert_eq!(problem(&refused).1, "invalid_state");

    let id = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    let started = post(&f.app, &f.ws.key, &action(&id, "start"), json!({})).await;
    assert_eq!(started.json["status"], "materialising");
    let again = post(&f.app, &f.ws.key, &action(&id, "start"), json!({})).await;
    assert_eq!(
        problem(&again),
        (
            StatusCode::CONFLICT,
            "invalid_state".to_owned(),
            String::new()
        )
    );
    let paused = post(&f.app, &f.ws.key, &action(&id, "pause"), json!({})).await;
    assert_eq!(paused.json["status"], "paused");
    let twice = post(&f.app, &f.ws.key, &action(&id, "pause"), json!({})).await;
    assert_eq!(problem(&twice).1, "invalid_state");
    start(&f, &id).await;
    let read = f
        .app
        .get(&format!("/v1/campaigns/{}", id.as_str().unwrap()))
        .bearer(&f.ws.key)
        .send()
        .await;
    assert_eq!(read.json["status"], "active");
    assert_eq!(
        count(&f, "SELECT count(*) FROM outbox_events WHERE subject_id = $1 AND type = 'campaign.status_changed'", uuid(&id)).await,
        2,
        "paused, then active"
    );
}

/// Deleting a campaign that never sent removes it with its enrollments; deleting one that sent
/// archives it (`200` with the object), after which it takes no change and no people.
#[tokio::test]
async fn delete_removes_a_draft_and_archives_a_campaign_that_sent() {
    let f = fixture("cmp-delete").await;
    let ids = people(&f, &["ada", "bob"], "delete.example").await;
    let draft = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    assert_eq!(
        enroll(&f, &draft, &ids[..1]).await.status,
        StatusCode::CREATED
    );
    let path = format!("/v1/campaigns/{}", draft.as_str().unwrap());
    let deleted = f.app.delete(&path).bearer(&f.ws.key).send().await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        f.app.get(&path).bearer(&f.ws.key).send().await.status,
        StatusCode::NOT_FOUND
    );

    let sent = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    assert_eq!(
        enroll(&f, &sent, &ids[..1]).await.status,
        StatusCode::CREATED
    );
    start(&f, &sent).await;
    let path = format!("/v1/campaigns/{}", sent.as_str().unwrap());
    let archived = f.app.delete(&path).bearer(&f.ws.key).send().await;
    assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.json);
    assert_eq!(archived.json["status"], "archived");
    assert_eq!(
        problem(&patch(&f.app, &f.ws.key, &path, json!({"name": "Y"}), None).await).1,
        "invalid_state"
    );
    assert_eq!(
        problem(&enroll(&f, &sent, &ids[1..]).await).1,
        "invalid_state"
    );
}

/// A campaign answered alone counts its enrollments, so a client shows where everyone is and
/// when the next email goes without paging through enrollments: every status by name (zero when
/// none), the live enrollments at each step in order (zero included), and the next email, which
/// is now while a step's email is on its way and otherwise the earliest step an active
/// enrollment waits for (a paused enrollment's step and an ended one's message do not count). A
/// list leaves the summary out (`null`), because counting reads every enrollment of each
/// campaign.
#[tokio::test]
async fn a_campaign_answered_alone_counts_its_enrollments() {
    let f = fixture("cmp-summary").await;
    let empty = campaign(&f, two_steps(&[f.a.identity], &[])).await;
    assert_eq!(
        empty["enrollments"],
        json!({
            "active": 0, "paused": 0, "completed": 0, "replied": 0, "stopped": 0, "failed": 0,
            "steps": [
                {"step_id": empty["steps"][0]["id"], "live": 0},
                {"step_id": empty["steps"][1]["id"], "live": 0},
            ],
            "next_run_at": null,
        })
    );

    let names = ["ada", "bob", "cy", "dee", "eve", "fay", "gus"];
    let ids = people(&f, &names, "summary.example").await;
    let created = campaign(&f, two_steps(&[f.a.identity], &[])).await;
    let path = format!("/v1/campaigns/{}", created["id"].as_str().unwrap());
    assert_eq!(
        enroll(&f, &created["id"], &ids).await.status,
        StatusCode::CREATED
    );
    let before = jiff::Timestamp::now();
    start(&f, &created["id"]).await;
    let sending = f.app.get(&path).bearer(&f.ws.key).send().await;
    let after = jiff::Timestamp::now();
    let next: jiff::Timestamp = sending.json["enrollments"]["next_run_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        before <= next && next <= after,
        "every first email is on its way, so the next one is due now: {next}"
    );

    let moves: [&'static str; 7] = [
        "UPDATE enrollments SET message_id = NULL, next_run_at = '2031-01-06T09:00:00Z' WHERE person_id = $1",
        "UPDATE enrollments SET message_id = NULL, current_position = 2, next_run_at = '2030-01-07T09:00:00Z' WHERE person_id = $1",
        "UPDATE enrollments SET message_id = NULL, current_position = 2, status = 'paused', next_run_at = '2029-01-01T09:00:00Z' WHERE person_id = $1",
        "UPDATE enrollments SET message_id = NULL, current_position = 2, status = 'completed', next_run_at = NULL WHERE person_id = $1",
        "UPDATE enrollments SET message_id = NULL, status = 'replied', next_run_at = NULL WHERE person_id = $1",
        "UPDATE enrollments SET message_id = NULL, status = 'stopped', next_run_at = NULL WHERE person_id = $1",
        "UPDATE enrollments SET status = 'failed', next_run_at = NULL WHERE person_id = $1",
    ];
    for (person, sql) in ids.iter().zip(moves) {
        exec(&f, sql, uuid(person)).await;
    }
    let read = f.app.get(&path).bearer(&f.ws.key).send().await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(
        read.json["enrollments"],
        json!({
            "active": 2, "paused": 1, "completed": 1, "replied": 1, "stopped": 1, "failed": 1,
            "steps": [
                {"step_id": created["steps"][0]["id"], "live": 1},
                {"step_id": created["steps"][1]["id"], "live": 2},
            ],
            "next_run_at": "2030-01-07T09:00:00Z",
        })
    );

    let listed = f.app.get("/v1/campaigns").bearer(&f.ws.key).send().await;
    let data = listed.json["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert!(
        data.iter()
            .all(|campaign| campaign.get("enrollments") == Some(&Value::Null)),
        "{data:?}"
    );
}

// ───────────────────────────── enrollments ─────────────────────────────

/// Up to 100 people enroll inline (`201`), each person not enrolled reported with its reason
/// (unknown, suppressed, already enrolled); more go to `enrollment.add` (`202` naming the job),
/// which enrolls them all. The list filters, counts and pages; a request naming no source, or
/// two, is refused.
#[tokio::test]
async fn enrollments_are_made_inline_or_by_a_job_and_skip_who_cannot_be_enrolled() {
    let f = fixture("cmp-enroll").await;
    let ids = people(&f, &["ada", "bob", "cy", "dee"], "enroll.example").await;
    let first = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    let suppressed = post(
        &f.app,
        &f.ws.key,
        "/v1/suppressions",
        json!({"email": "dee@enroll.example"}),
    )
    .await;
    assert_eq!(
        suppressed.status,
        StatusCode::CREATED,
        "{:?}",
        suppressed.json
    );
    let ghost = Id::<Person>::from_uuid(Uuid::now_v7()).to_string();
    let mut asked: Vec<Value> = ids.clone();
    asked.push(json!(ghost));
    let made = enroll(&f, &first, &asked).await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.json);
    assert_eq!(made.json["data"].as_array().unwrap().len(), 3);
    assert_eq!(made.json["data"][0]["status"], "active");
    assert_eq!(made.json["data"][0]["position"], 1);
    let mut reasons: Vec<&str> = made.json["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    reasons.sort_unstable();
    assert_eq!(reasons, ["not_found", "suppressed"]);
    let again = enroll(&f, &first, &ids[..1]).await;
    assert_eq!(again.json["skipped"][0]["reason"], "already_enrolled");
    let by_email = post(
        &f.app,
        &f.ws.key,
        "/v1/enrollments",
        json!({"campaign_id": first, "emails": ["nobody@enroll.example"]}),
    )
    .await;
    assert_eq!(
        by_email.json["skipped"][0]["email"],
        "nobody@enroll.example"
    );
    for body in [
        json!({"campaign_id": first}),
        json!({"campaign_id": first, "person_ids": [ids[0]], "group_id": "grp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"}),
    ] {
        assert_eq!(
            problem(&post(&f.app, &f.ws.key, "/v1/enrollments", body).await).1,
            "validation_failed"
        );
    }

    let group = post(&f.app, &f.ws.key, "/v1/groups", json!({"name": "Big"}))
        .await
        .json["id"]
        .clone();
    let people: Vec<Value> = (0..101)
        .map(|n| json!({"email": format!("p{n}@big.example")}))
        .collect();
    let import = post(
        &f.app,
        &f.ws.key,
        "/v1/imports",
        json!({"people": people, "group_id": group}),
    )
    .await;
    assert_eq!(import.status, StatusCode::ACCEPTED, "{:?}", import.json);
    let mut registry = Registry::default();
    registry
        .register::<crate::people::imports::PeopleImport>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(f.test.storage.clone());
    Harness::new(
        f.test.worker.clone(),
        f.test.system.clone(),
        registry,
        env,
        "import",
    )
    .run_once(Queue::Imports, 1)
    .await;
    let second = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    let queued = post(
        &f.app,
        &f.ws.key,
        "/v1/enrollments",
        json!({"campaign_id": second, "group_id": group}),
    )
    .await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{:?}", queued.json);
    let location = queued.header("location").unwrap().to_owned();
    assert!(location.starts_with("/v1/jobs/job_"), "{location}");
    let outcomes = harness(&f.test).run_once(Queue::Enrollment, 4).await;
    assert_eq!(
        outcomes
            .iter()
            .map(|(_, outcome)| *outcome)
            .collect::<Vec<_>>(),
        ["done"]
    );
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM enrollments WHERE campaign_id = $1",
            uuid(&second)
        )
        .await,
        101
    );

    let list = |query: String| {
        f.app
            .get(&format!("/v1/enrollments?{query}"))
            .bearer(&f.ws.key)
            .send()
    };
    let page = list(format!(
        "campaign_id={}&limit=2&include=total_count",
        first.as_str().unwrap()
    ))
    .await;
    assert_eq!(page.json["meta"]["total_count"], 3, "{:?}", page.json);
    let cursor = page.json["meta"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let next = list(format!(
        "campaign_id={}&limit=2&cursor={cursor}",
        first.as_str().unwrap()
    ))
    .await;
    assert_eq!(next.json["data"].as_array().unwrap().len(), 1);
    assert_ne!(next.json["data"][0]["id"], page.json["data"][0]["id"]);
}

/// Another workspace's ids are `404` everywhere, as absent ones are: campaigns read, changed,
/// deleted and started, enrollments read, stopped and created into, a step's content sent.
#[tokio::test]
async fn forged_ids_of_another_workspace_are_not_found() {
    let f = fixture("cmp-tenant").await;
    let other = f.test.workspace("cmp-tenant-other").await;
    let ids = people(&f, &["ada"], "tenant.example").await;
    let created = campaign(&f, two_steps(&[f.a.identity], &[])).await;
    let id = created["id"].as_str().unwrap();
    let step = created["steps"][0]["id"].clone();
    let enrollment = enroll(&f, &created["id"], &ids).await.json["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let replies = [
        f.app
            .get(&format!("/v1/campaigns/{id}"))
            .bearer(&other.key)
            .send()
            .await,
        patch(
            &f.app,
            &other.key,
            &format!("/v1/campaigns/{id}"),
            json!({"name": "Mine"}),
            None,
        )
        .await,
        f.app
            .delete(&format!("/v1/campaigns/{id}"))
            .bearer(&other.key)
            .send()
            .await,
        post(
            &f.app,
            &other.key,
            &format!("/v1/campaigns/{id}/start"),
            json!({}),
        )
        .await,
        f.app
            .get(&format!("/v1/enrollments/{enrollment}"))
            .bearer(&other.key)
            .send()
            .await,
        post(
            &f.app,
            &other.key,
            &format!("/v1/enrollments/{enrollment}/stop"),
            json!({}),
        )
        .await,
        post(
            &f.app,
            &other.key,
            "/v1/enrollments",
            json!({"campaign_id": id, "person_ids": ids}),
        )
        .await,
        post(
            &f.app,
            &other.key,
            "/v1/messages",
            json!({"step_id": step, "person_id": ids[0]}),
        )
        .await,
    ];
    for reply in replies {
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{:?}", reply.json);
    }
}

// ───────────────────────────── creating step messages ─────────────────────────────

/// Starting a campaign creates one message per due enrollment through the creation contract,
/// spreading new conversations over the pool (a named identity and one selected by tag) by
/// rotation, recording each conversation's sender, the person's variant and the rotation; the
/// messages are paced campaign mail. A second pass finds every step's message made and creates
/// nothing.
#[tokio::test]
async fn a_pass_creates_one_message_per_due_step_over_the_pool() {
    let f = fixture("cmp-pass").await;
    exec(
        &f,
        "UPDATE sender_identities SET tags = '{pool}' WHERE id = $1",
        f.b.identity.uuid(),
    )
    .await;
    let ids = people(&f, &["ada", "bob", "cy", "dee"], "pass.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity], &["pool"])).await["id"].clone();
    let campaign = uuid(&id);
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    start(&f, &id).await;
    let made = live(&f, campaign).await;
    assert_eq!(made.len(), 4);
    for identity in [f.a.identity.uuid(), f.b.identity.uuid()] {
        assert_eq!(
            made.iter()
                .filter(|(_, _, sender, _)| *sender == identity)
                .count(),
            2
        );
    }
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM campaign_sender_affinity WHERE campaign_id = $1",
            campaign
        )
        .await,
        4
    );
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM campaign_sender_rotation WHERE campaign_id = $1",
            campaign
        )
        .await,
        2
    );
    assert_eq!(
        count(&f, "SELECT count(*) FROM step_assignments a JOIN steps s ON s.id = a.step_id WHERE s.campaign_id = $1", campaign).await,
        4
    );
    assert_eq!(
        count(&f, "SELECT count(*) FROM delivery_queue q JOIN messages m ON m.id = q.message_id WHERE m.campaign_id = $1 AND q.paced", campaign).await,
        4
    );
    assert_eq!(
        count(&f, "SELECT count(*) FROM enrollments WHERE campaign_id = $1 AND message_id IS NOT NULL AND next_run_at IS NULL", campaign).await,
        4
    );
    assert_eq!(pass(&f, Some(campaign)).await.created, 0);
}

/// A chunk reads how many people each variant of a step already has from the stored
/// assignments, so a balanced split carries from one chunk to the next: ten people created one
/// chunk at a time over a step of five variants get two each. (Within a chunk the pass counts in
/// memory; the stored counts are what the next chunk starts from.)
#[tokio::test]
async fn the_split_carries_across_chunks_from_the_stored_assignments() {
    let f = fixture("cmp-split").await;
    let names = [
        "ada", "bob", "cy", "dee", "eve", "fay", "gus", "hal", "ivy", "jo",
    ];
    let ids = people(&f, &names, "split.example").await;
    let created = campaign(
        &f,
        json!({"name": "Split", "steps": [{"name": "Intro", "variants": vec![small_variant(); 5]}],
               "senders": {"identity_ids": [f.a.identity.to_string()]}}),
    )
    .await;
    let campaign = uuid(&created["id"]);
    assert_eq!(
        enroll(&f, &created["id"], &ids).await.status,
        StatusCode::CREATED
    );
    exec(
        &f,
        "UPDATE campaigns SET status = 'active' WHERE id = $1",
        campaign,
    )
    .await;
    for _ in names {
        let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
        let chunk = creator::pass(
            &mut tx,
            &keys(),
            f.ws.id,
            Some(Id::from_uuid(campaign)),
            jiff::Timestamp::now(),
            None,
            1,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(chunk.created, 1);
    }
    let split: Vec<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM step_assignments a JOIN steps s ON s.id = a.step_id
          WHERE s.campaign_id = $1 GROUP BY a.variant_id ORDER BY a.variant_id",
    )
    .bind(campaign)
    .fetch_all(f.test.system.pool())
    .await
    .unwrap();
    assert_eq!(split, [2; 5]);
}

/// Two passes over the same due enrollments at once (two workers) create exactly one message
/// per enrollment: the second waits for the campaign's lock and then finds every pointer set.
#[tokio::test]
async fn two_passes_at_once_create_one_message_per_step() {
    let f = fixture("cmp-race").await;
    let names: Vec<String> = (0..20).map(|n| format!("p{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let ids = people(&f, &names, "race.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity, f.b.identity], &[])).await["id"].clone();
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    exec(
        &f,
        "UPDATE campaigns SET status = 'active' WHERE id = $1",
        uuid(&id),
    )
    .await;
    let other = f.test.worker_pool(2).await;
    let run = |db: crate::db::Database, workspace: WorkspaceId| async move {
        let mut tx = db.begin_in(workspace).await.unwrap();
        let pass = creator::pass(
            &mut tx,
            &keys(),
            workspace,
            None,
            jiff::Timestamp::now(),
            None,
            CHUNK,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        pass.created
    };
    let (one, two) = tokio::join!(run(f.test.worker.clone(), f.ws.id), run(other, f.ws.id));
    assert_eq!(one + two, 20, "{one} + {two}");
    assert_eq!(live(&f, uuid(&id)).await.len(), 20);
}

/// When a step's message ends, the next pass moves its conversation on as the message's fate
/// says: sent → the next step, due its delay after the send; sent at the last step →
/// `completed`; failed before any submission → the step again, later; refused after a
/// submission → `failed`; suppressed or cancelled → `stopped`; uncertain → it waits. An
/// enrollment that ended while its message was queued gets that message cancelled.
#[tokio::test]
async fn settling_moves_each_conversation_by_how_its_message_ended() {
    let f = fixture("cmp-settle").await;
    let names = [
        "sent",
        "last",
        "unstarted",
        "refused",
        "suppressed",
        "cancelled",
        "uncertain",
        "ended",
    ];
    let ids = people(&f, &names, "settle.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity, f.b.identity], &[])).await["id"].clone();
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    start(&f, &id).await;
    let made = live(&f, uuid(&id)).await;
    assert_eq!(made.len(), 8);
    for (index, (state, attempts)) in [
        ("sent", 0),
        ("sent", 0),
        ("failed", 0),
        ("failed", 1),
        ("suppressed", 0),
        ("cancelled", 0),
        ("uncertain", 1),
    ]
    .into_iter()
    .enumerate()
    {
        let message = message_of(&f, &ids[index]).await;
        sqlx::query(
            "WITH gone AS (DELETE FROM delivery_queue WHERE message_id = $1 AND $2 <> 'uncertain')
             UPDATE messages SET state = $2, attempt_number = $3,
                    sent_at = CASE WHEN $2 = 'sent' THEN now() - interval '1 hour' END
              WHERE id = $1",
        )
        .bind(message)
        .bind(state)
        .bind(attempts)
        .execute(f.test.system.pool())
        .await
        .unwrap();
    }
    exec(
        &f,
        "UPDATE enrollments SET current_position = 2 WHERE person_id = $1",
        uuid(&ids[1]),
    )
    .await;
    exec(
        &f,
        "UPDATE enrollments SET status = 'stopped' WHERE person_id = $1",
        uuid(&ids[7]),
    )
    .await;
    let ended_message = message_of(&f, &ids[7]).await;

    let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
    advance::settle(&mut tx, f.ws.id, jiff::Timestamp::now(), 500)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (status, position, _, cleared, a_day_on, _) = state(&f, &ids[0]).await;
    assert_eq!(
        (status.as_str(), position, cleared, a_day_on),
        ("active", 2, true, true)
    );
    assert_eq!(state(&f, &ids[1]).await.0, "completed");
    let (status, position, attempts, cleared, ..) = state(&f, &ids[2]).await;
    assert_eq!(
        (status.as_str(), position, attempts, cleared),
        ("active", 1, 1, true)
    );
    assert_eq!(state(&f, &ids[3]).await.0, "failed");
    assert_eq!(state(&f, &ids[4]).await.0, "stopped");
    assert_eq!(state(&f, &ids[5]).await.0, "stopped");
    let (status, _, _, cleared, ..) = state(&f, &ids[6]).await;
    assert_eq!(
        (status.as_str(), cleared),
        ("active", false),
        "an uncertain message waits"
    );
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM messages WHERE id = $1 AND state = 'cancelled'",
            ended_message
        )
        .await,
        1
    );
    assert_eq!(
        count(&f, "SELECT count(*) FROM outbox_events WHERE type = 'enrollment.completed' AND subject_id IN (SELECT id FROM enrollments WHERE person_id = $1)", uuid(&ids[1])).await,
        1
    );
}

// ───────────────────────────── losing a sender ─────────────────────────────

/// A sender taken out of the pool, under `reassign`: its queued follow-up is cancelled
/// ("sender removed") and the step re-armed (no message, no thread root, no affinity); one a
/// sender claimed is left to its Start and keeps the job unfinished; one in flight finishes.
/// Once the Start returns the claimed one, the next run cancels it too. The next pass then
/// makes exactly one new message per re-armed step, from the remaining sender, in a new thread.
#[tokio::test]
async fn a_removed_sender_is_replaced_once_in_a_new_thread() {
    let f = fixture("cmp-reassign").await;
    exec(
        &f,
        "UPDATE sender_identities SET tags = '{pool}' WHERE id = $1",
        f.a.identity.uuid(),
    )
    .await;
    let ids = people(&f, &["queued", "claimed", "flight"], "reassign.example").await;
    let id = campaign(&f, two_steps(&[], &["pool"])).await["id"].clone();
    let campaign = uuid(&id);
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    start(&f, &id).await;
    let before = live(&f, campaign).await;
    assert!(
        before
            .iter()
            .all(|(_, _, sender, _)| *sender == f.a.identity.uuid())
    );
    let path = format!("/v1/campaigns/{}", id.as_str().unwrap());
    let named = patch(
        &f.app,
        &f.ws.key,
        &path,
        json!({"senders": {"identity_ids": [f.b.identity.to_string()]}}),
        None,
    )
    .await;
    assert_eq!(named.status, StatusCode::OK, "{:?}", named.json);
    let messages: Vec<(Uuid, Uuid)> = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT e.person_id, e.message_id FROM enrollments e WHERE e.campaign_id = $1",
    )
    .bind(campaign)
    .fetch_all(f.test.system.pool())
    .await
    .unwrap();
    let of = |person: &Value| messages.iter().find(|(p, _)| *p == uuid(person)).unwrap().1;
    for (person, state) in [(&ids[1], "claimed"), (&ids[2], "in_flight")] {
        sqlx::query(
            "WITH q AS (UPDATE delivery_queue SET state = $2, lease_owner = 'replica-a', lease_generation = 1,
                                                  lease_expires_at = now() + interval '2 minutes'
                         WHERE message_id = $1)
             UPDATE messages SET state = $2 WHERE id = $1",
        )
        .bind(of(person))
        .bind(state)
        .execute(f.test.system.pool())
        .await
        .unwrap();
    }
    exec(
        &f,
        "UPDATE sender_identities SET tags = '{}' WHERE id = $1",
        f.a.identity.uuid(),
    )
    .await;
    let scope = Scope::Identities(vec![f.a.identity]);
    let apply = || async {
        let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
        let handled = removal::apply(&mut tx, f.ws.id, &scope, 500).await.unwrap();
        let left = removal::unstarted(&mut tx, f.ws.id, &scope).await.unwrap();
        tx.commit().await.unwrap();
        (handled, left)
    };
    assert_eq!(
        apply().await,
        (1, 1),
        "the queued one is handled, the claimed one keeps the job open"
    );
    assert_eq!(
        count(&f, "SELECT count(*) FROM messages WHERE id = $1 AND state = 'cancelled' AND status_detail = 'sender removed'", of(&ids[0])).await,
        1
    );
    assert_eq!(
        count(&f, "SELECT count(*) FROM enrollments WHERE person_id = $1 AND message_id IS NULL AND thread_root_message_id IS NULL AND next_run_at IS NOT NULL", uuid(&ids[0])).await,
        1
    );
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM campaign_sender_affinity WHERE person_id = $1",
            uuid(&ids[0])
        )
        .await,
        0
    );
    // The Start sees the removal and returns the claimed message to the queue.
    sqlx::query(
        "WITH q AS (UPDATE delivery_queue SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL WHERE message_id = $1)
         UPDATE messages SET state = 'queued' WHERE id = $1",
    )
    .bind(of(&ids[1]))
    .execute(f.test.system.pool())
    .await
    .unwrap();
    assert_eq!(apply().await, (1, 0));
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM messages WHERE id = $1 AND state = 'in_flight'",
            of(&ids[2])
        )
        .await,
        1,
        "a started submission finishes as it is"
    );

    assert_eq!(pass(&f, Some(campaign)).await.created, 2);
    let after = live(&f, campaign).await;
    assert_eq!(after.len(), 3, "one live message per conversation");
    for person in &ids[..2] {
        let enrollment: Uuid =
            sqlx::query_scalar("SELECT id FROM enrollments WHERE person_id = $1")
                .bind(uuid(person))
                .fetch_one(f.test.system.pool())
                .await
                .unwrap();
        let (_, _, sender, thread) = after.iter().find(|row| row.0 == enrollment).unwrap();
        let (_, _, _, old_thread) = before.iter().find(|row| row.0 == enrollment).unwrap();
        assert_eq!(*sender, f.b.identity.uuid());
        assert_ne!(thread, old_thread, "a new sender starts a new thread");
    }
}

/// Under `stop`, a conversation whose sender left the pool stops ("sender removed"), its queued
/// message cancelled; and archiving a connection enqueues `senders.removed` for its identities.
#[tokio::test]
async fn under_stop_a_removed_sender_stops_its_conversations() {
    let f = fixture("cmp-stop").await;
    let ids = people(&f, &["ada"], "stop.example").await;
    let mut body = two_steps(&[f.a.identity], &[]);
    body["senders"]["on_sender_removed"] = json!("stop");
    let id = campaign(&f, body).await["id"].clone();
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    start(&f, &id).await;
    let archived = f
        .app
        .delete(&format!("/v1/connections/{}", f.a.connection))
        .bearer(&f.ws.key)
        .send()
        .await;
    assert!(archived.status.is_success(), "{:?}", archived.json);
    let outcomes = harness(&f.test).run_once(Queue::Enrollment, 4).await;
    assert_eq!(
        outcomes
            .iter()
            .map(|(_, outcome)| *outcome)
            .collect::<Vec<_>>(),
        ["done"]
    );
    let (status, detail): (String, Option<String>) =
        sqlx::query_as("SELECT status, status_detail FROM enrollments WHERE campaign_id = $1")
            .bind(uuid(&id))
            .fetch_one(f.test.system.pool())
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), detail.as_deref()),
        ("stopped", Some("sender removed"))
    );
    assert_eq!(
        count(
            &f,
            "SELECT count(*) FROM messages WHERE campaign_id = $1 AND state = 'cancelled'",
            uuid(&id)
        )
        .await,
        1
    );
}

/// A conversation keeps its sender while the sender is only unavailable: its next step waits and
/// the enrollment shows `waiting_for`. A pool with no usable sender holds new conversations
/// without failing the campaign (`last_error` `no_sender`, still `active`) until a sender is
/// back, when the waiting conversations continue and the error clears.
#[tokio::test]
async fn conversations_wait_for_an_unavailable_sender_and_an_empty_pool() {
    let f = fixture("cmp-wait").await;
    let ids = people(&f, &["ada"], "wait.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    let campaign = uuid(&id);
    let enrolled = enroll(&f, &id, &ids).await.json["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    exec(
        &f,
        "UPDATE connections SET paused = true WHERE id = $1",
        f.a.connection.uuid(),
    )
    .await;
    start(&f, &id).await;
    assert!(live(&f, campaign).await.is_empty());
    let read = f
        .app
        .get(&format!("/v1/campaigns/{}", id.as_str().unwrap()))
        .bearer(&f.ws.key)
        .send()
        .await;
    assert_eq!(read.json["status"], "active");
    assert_eq!(read.json["last_error"]["code"], "no_sender");

    exec(
        &f,
        "UPDATE connections SET paused = false WHERE id = $1",
        f.a.connection.uuid(),
    )
    .await;
    assert_eq!(pass(&f, Some(campaign)).await.created, 1);
    let read = f
        .app
        .get(&format!("/v1/campaigns/{}", id.as_str().unwrap()))
        .bearer(&f.ws.key)
        .send()
        .await;
    assert!(
        read.json["last_error"].is_null(),
        "{:?}",
        read.json["last_error"]
    );

    // The first message was sent; the follow-up is due while its sender is paused.
    exec(
        &f,
        "WITH m AS (UPDATE messages SET state = 'sent', sent_at = now() - interval '2 days' WHERE campaign_id = $1 RETURNING id)
         DELETE FROM delivery_queue q USING m WHERE q.message_id = m.id",
        campaign,
    )
    .await;
    let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
    advance::settle(&mut tx, f.ws.id, jiff::Timestamp::now(), 500)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    exec(
        &f,
        "UPDATE connections SET paused = true WHERE id = $1",
        f.a.connection.uuid(),
    )
    .await;
    assert_eq!(pass(&f, Some(campaign)).await.created, 0);
    let waiting = f
        .app
        .get(&format!("/v1/enrollments/{enrolled}"))
        .bearer(&f.ws.key)
        .send()
        .await;
    assert_eq!(waiting.json["position"], 2);
    assert_eq!(waiting.json["waiting_for"], json!(f.a.identity.to_string()));
    assert_eq!(
        waiting.json["sender_identity_id"],
        json!(f.a.identity.to_string())
    );
    exec(
        &f,
        "UPDATE connections SET paused = false WHERE id = $1",
        f.a.connection.uuid(),
    )
    .await;
    assert_eq!(pass(&f, Some(campaign)).await.created, 1);
    let threads = sqlx::query_scalar::<_, i64>(
        "SELECT count(DISTINCT thread_id) FROM messages WHERE campaign_id = $1",
    )
    .bind(campaign)
    .fetch_one(f.test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        threads, 1,
        "the follow-up continues the conversation's thread"
    );
}

// ───────────────────────────── stop rules ─────────────────────────────

/// A reply stops enrollments as the replier's campaign says: `all` ends the person's
/// enrollments in every campaign `replied`, `campaign` only that campaign's, `none` none; with
/// the company rule, that campaign's enrollments of people at the same domain end `stopped`.
/// The same reply applied twice ends nothing more.
#[tokio::test]
async fn a_reply_stops_enrollments_as_the_campaign_says() {
    let f = fixture("cmp-reply").await;
    let ids = people(&f, &["ada", "bob", "cy"], "reply.example").await;
    let outsider = people(&f, &["dee"], "elsewhere.example").await;
    let mut body = two_steps(&[f.a.identity], &[]);
    body["stop_rules"] = json!({"on_reply": "all", "company_on_reply": true});
    let first = campaign(&f, body).await["id"].clone();
    let second = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    let mut everyone = ids.clone();
    everyone.extend(outsider);
    assert_eq!(
        enroll(&f, &first, &everyone).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        enroll(&f, &second, &ids[..1]).await.status,
        StatusCode::CREATED
    );

    let reply = || async {
        let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
        let stopped = enrollments::stop_for_reply(
            &mut tx,
            f.ws.id,
            Id::<Campaign>::from_uuid(uuid(&first)),
            Id::<Person>::from_uuid(uuid(&ids[0])),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        stopped
    };
    let stopped = reply().await;
    assert_eq!((stopped.replied.len(), stopped.company.len()), (2, 2));
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT person_id, status FROM enrollments ORDER BY person_id")
            .fetch_all(f.test.system.pool())
            .await
            .unwrap();
    let of = |person: &Value| {
        statuses
            .iter()
            .filter(|(p, _)| *p == uuid(person))
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(of(&ids[0]), ["replied", "replied"]);
    assert_eq!(of(&ids[1]), ["stopped"]);
    assert_eq!(
        of(&everyone[3]),
        ["active"],
        "another domain is another company"
    );
    assert_eq!(reply().await, enrollments::Stopped::default());

    exec(
        &f,
        "UPDATE campaigns SET stop_on_reply = 'none', stop_company_on_reply = false WHERE id = $1",
        uuid(&second),
    )
    .await;
    assert_eq!(
        enroll(&f, &second, &ids[1..2]).await.status,
        StatusCode::CREATED
    );
    let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
    let none = enrollments::stop_for_reply(
        &mut tx,
        f.ws.id,
        Id::<Campaign>::from_uuid(uuid(&second)),
        Id::<Person>::from_uuid(uuid(&ids[1])),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(none, enrollments::Stopped::default());
}

// ───────────────────────────── jobs ─────────────────────────────

/// The 5-minute pass works in every workspace with enrollments: it creates the due messages of
/// two workspaces' campaigns in one run of the system workspace's singleton.
#[tokio::test]
async fn the_five_minute_pass_creates_in_every_workspace() {
    let f = fixture("cmp-tick").await;
    let ids = people(&f, &["ada"], "tick.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    exec(
        &f,
        "UPDATE campaigns SET status = 'active' WHERE id = $1",
        uuid(&id),
    )
    .await;
    let other = f.test.workspace("cmp-tick-other").await;
    let sender = f
        .test
        .sender(other.id, &SenderSpec::mailbox("c@tick-other.example"))
        .await;
    let person = post(
        &f.app,
        &other.key,
        "/v1/people",
        json!({"email": "bob@tick.example"}),
    )
    .await
    .json["id"]
        .clone();
    let created = post(
        &f.app,
        &other.key,
        "/v1/campaigns",
        two_steps(&[sender.identity], &[]),
    )
    .await
    .json["id"]
        .clone();
    let made = post(
        &f.app,
        &other.key,
        "/v1/enrollments",
        json!({"campaign_id": created, "person_ids": [person]}),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    exec(
        &f,
        "UPDATE campaigns SET status = 'active' WHERE id = $1",
        uuid(&created),
    )
    .await;
    let mut tx = f
        .test
        .system
        .begin_in(crate::jobs::SYSTEM_WORKSPACE)
        .await
        .unwrap();
    crate::jobs::enqueue(
        &mut tx,
        crate::jobs::SYSTEM_WORKSPACE,
        &EnrollmentAdvance {},
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let outcomes = harness(&f.test).run_once(Queue::Enrollment, 4).await;
    assert_eq!(
        outcomes
            .iter()
            .map(|(_, outcome)| *outcome)
            .collect::<Vec<_>>(),
        ["done"]
    );
    assert_eq!(live(&f, uuid(&id)).await.len(), 1);
    assert_eq!(live(&f, uuid(&created)).await.len(), 1);
}

/// A run of the 5-minute pass that loses its claim in the middle of a workspace (its worker
/// stalled in the creation step past its lease, and the lease was recovered) is continued by the
/// next claim from the start of that workspace, not after it: the lost run's unfinished chunk is
/// rolled back, the steps it had finished are redone harmlessly, and every enrollment gets
/// exactly one message. Continuing after the workspace would leave its conversations without
/// their step until a later run; redoing a step twice would send twice.
#[tokio::test]
async fn a_run_recovered_mid_workspace_finishes_that_workspace() {
    let f = fixture("cmp-recovered").await;
    let ids = people(&f, &["ada", "bob"], "recovered.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity], &[])).await["id"].clone();
    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    exec(
        &f,
        "UPDATE campaigns SET status = 'active' WHERE id = $1",
        uuid(&id),
    )
    .await;
    let mut tx = f
        .test
        .system
        .begin_in(crate::jobs::SYSTEM_WORKSPACE)
        .await
        .unwrap();
    let job = crate::jobs::enqueue(
        &mut tx,
        crate::jobs::SYSTEM_WORKSPACE,
        &EnrollmentAdvance {},
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // The campaign's row is held here: the run settles, then waits for it in its creation chunk.
    let mut holder = f.test.system.pool().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM campaigns WHERE id = $1 FOR UPDATE")
        .bind(uuid(&id))
        .execute(&mut *holder)
        .await
        .unwrap();
    let first = harness(&f.test);
    let second = harness(&f.test);
    let (lost, ()) = tokio::join!(first.run_once(Queue::Enrollment, 1), async {
        let mut step = None;
        for _ in 0..500 {
            step = sqlx::query_scalar::<_, Option<String>>(
                "SELECT progress ->> 'step' FROM jobs WHERE id = $1",
            )
            .bind(job.uuid())
            .fetch_one(f.test.system.pool())
            .await
            .unwrap();
            if step.as_deref() == Some("archived") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            step.as_deref(),
            Some("archived"),
            "the run never reached its creation step"
        );
        exec(
            &f,
            "UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
            job.uuid(),
        )
        .await;
        assert_eq!(
            second.recover_once().await,
            vec![(job, "available".to_owned())]
        );
        holder.rollback().await.unwrap();
    });
    assert_eq!(
        lost.iter().map(|(_, outcome)| *outcome).collect::<Vec<_>>(),
        ["lost"]
    );
    assert!(
        live(&f, uuid(&id)).await.is_empty(),
        "the lost run's creation chunk was rolled back"
    );
    exec(
        &f,
        "UPDATE jobs SET run_at = now() WHERE id = $1",
        job.uuid(),
    )
    .await;
    let outcomes = second.run_once(Queue::Enrollment, 1).await;
    assert_eq!(
        outcomes
            .iter()
            .map(|(_, outcome)| *outcome)
            .collect::<Vec<_>>(),
        ["done"]
    );
    let made = live(&f, uuid(&id)).await;
    assert_eq!(made.len(), 2, "{made:?}");
    assert_ne!(made[0].0, made[1].0, "one message per enrollment");
}

/// A step with a personalisation prompt gets its message from `message.generate`: the pass
/// assigns the sender and the variant and leaves the enrollment without a message; the job,
/// with no AI provider configured, falls back to the template's defaults and creates the
/// message with no snippets. A job whose sender left the pool meanwhile creates nothing.
#[tokio::test]
async fn generated_steps_are_created_by_their_job_with_the_templates_defaults() {
    let f = fixture("cmp-generate").await;
    let ids = people(&f, &["ada", "bob"], "generate.example").await;
    let body = json!({"name": "Personal", "steps": [{"name": "Intro", "personalisation_prompt": "Open with their company.",
        "variants": [{"subject": "Hi", "html": "<p>{{ variables.opener | default(\"Hello\") }}</p>"}]}],
        "senders": {"identity_ids": [f.a.identity.to_string()]}});
    let id = campaign(&f, body).await["id"].clone();
    assert_eq!(enroll(&f, &id, &ids[..1]).await.status, StatusCode::CREATED);
    start(&f, &id).await;
    assert!(live(&f, uuid(&id)).await.is_empty());
    assert_eq!(
        total(
            &f,
            "SELECT count(*) FROM jobs WHERE kind = 'message.generate' AND state = 'available'"
        )
        .await,
        1
    );
    let outcomes = harness(&f.test).run_once(Queue::Ai, 2).await;
    assert_eq!(
        outcomes
            .iter()
            .map(|(_, outcome)| *outcome)
            .collect::<Vec<_>>(),
        ["done"]
    );
    let context: Value =
        sqlx::query_scalar("SELECT render_context FROM messages WHERE campaign_id = $1")
            .bind(uuid(&id))
            .fetch_one(f.test.system.pool())
            .await
            .unwrap();
    assert!(
        context
            .get("variables")
            .is_none_or(|variables| variables == &json!({})),
        "{context}"
    );

    assert_eq!(enroll(&f, &id, &ids[1..]).await.status, StatusCode::CREATED);
    assert_eq!(pass(&f, Some(uuid(&id))).await.generating, 1);
    exec(
        &f,
        "UPDATE sender_identities SET enabled = false WHERE id = $1",
        f.a.identity.uuid(),
    )
    .await;
    harness(&f.test).run_once(Queue::Ai, 2).await;
    assert_eq!(
        live(&f, uuid(&id)).await.len(),
        1,
        "a job whose sender left creates nothing"
    );
}

/// Every removal enqueues `senders.removed` in its own transaction: taking a tag out of a
/// campaign's pool, untagging or disabling an identity; and a manual suppression stops the
/// person's live enrollments at once.
#[tokio::test]
async fn removals_enqueue_their_job_and_a_suppression_stops_enrollments() {
    let f = fixture("cmp-hooks").await;
    let ids = people(&f, &["ada"], "hooks.example").await;
    let id = campaign(&f, two_steps(&[f.a.identity], &["pool"])).await["id"].clone();
    let path = format!("/v1/campaigns/{}", id.as_str().unwrap());
    let jobs = || {
        total(
            &f,
            "SELECT count(*) FROM jobs WHERE kind = 'senders.removed'",
        )
    };
    assert_eq!(
        patch(
            &f.app,
            &f.ws.key,
            &path,
            json!({"senders": {"tags": ["pool", "more"]}}),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(jobs().await, 0, "a pool that grows removes nobody");
    assert_eq!(
        patch(
            &f.app,
            &f.ws.key,
            &path,
            json!({"senders": {"tags": ["more"]}}),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(jobs().await, 1);

    let mut tx = f.test.worker.begin_in(f.ws.id).await.unwrap();
    let email = crate::domain::email::EmailAddress::parse("b@cmp-hooks.example").unwrap();
    let mut input = crate::senders::identities::IdentityInput::address(email, true);
    input.id = Some(f.b.identity);
    input.enabled = Some(false);
    crate::senders::identities::replace(&mut tx, f.ws.id, f.b.connection, &[input])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(jobs().await, 2);

    assert_eq!(enroll(&f, &id, &ids).await.status, StatusCode::CREATED);
    let suppressed = post(
        &f.app,
        &f.ws.key,
        "/v1/suppressions",
        json!({"email": "ada@hooks.example"}),
    )
    .await;
    assert_eq!(suppressed.status, StatusCode::CREATED);
    let (status, detail): (String, Option<String>) =
        sqlx::query_as("SELECT status, status_detail FROM enrollments WHERE person_id = $1")
            .bind(uuid(&ids[0]))
            .fetch_one(f.test.system.pool())
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), detail.as_deref()),
        ("stopped", Some("The address is suppressed."))
    );
}

// ───────────────────────────── the step-content form of messages ─────────────────────────────

/// A step's content goes to people who are not enrolled as direct messages rendered from the
/// variant with each person's fields: with `person_id` and `to`, that person's version goes to
/// another address (a preview, without the variant's copies); with `person_ids`, each person
/// gets a result, the message or the problem a single request would answer.
#[tokio::test]
async fn a_step_is_sent_to_people_not_enrolled_and_previewed_elsewhere() {
    let f = fixture("cmp-content").await;
    let ids = people(&f, &["ada", "zed"], "content.example").await;
    let mut body = two_steps(&[f.a.identity], &[]);
    body["steps"][0]["variants"] = json!([{"subject": "Hi {{ person.given_name }}", "html": "<p>Hello</p>", "cc": ["copy@content.example"]}]);
    let created = campaign(&f, body).await;
    let step = created["steps"][0]["id"].clone();
    let preview = post(
        &f.app,
        &f.ws.key,
        "/v1/messages",
        json!({"step_id": step, "person_id": ids[0], "to": "max@preview.example"}),
    )
    .await;
    assert_eq!(preview.status, StatusCode::ACCEPTED, "{:?}", preview.json);
    assert!(
        preview
            .header("location")
            .unwrap()
            .starts_with("/v1/messages/msg_")
    );
    assert_eq!(preview.json["to"], json!(["max@preview.example"]));
    assert_eq!(preview.json["subject"], "Hi ada");
    assert_eq!(preview.json["kind"], "direct");
    assert_eq!(preview.json["cc"], json!([]));

    let suppressed = post(
        &f.app,
        &f.ws.key,
        "/v1/suppressions",
        json!({"email": "zed@content.example"}),
    )
    .await;
    assert_eq!(suppressed.status, StatusCode::CREATED);
    let ghost = Id::<Person>::from_uuid(Uuid::now_v7()).to_string();
    let many = post(
        &f.app,
        &f.ws.key,
        "/v1/messages",
        json!({"step_id": step, "person_ids": [ids[0], ids[1], ghost]}),
    )
    .await;
    assert_eq!(many.status, StatusCode::ACCEPTED, "{:?}", many.json);
    let results = many.json["data"].as_array().unwrap();
    assert_eq!(results[0]["message"]["to"], json!(["ada@content.example"]));
    assert_eq!(results[0]["message"]["cc"], json!(["copy@content.example"]));
    assert_eq!(results[1]["error"]["code"], "suppressed");
    assert_eq!(results[2]["error"]["code"], "not_found");
    let refused = post(
        &f.app,
        &f.ws.key,
        "/v1/messages",
        json!({"step_id": step, "person_ids": [ids[0]], "to": "max@preview.example"}),
    )
    .await;
    assert_eq!(problem(&refused).2, "/to");
}
