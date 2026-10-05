//! Store and protocol tests of AI calls, against real PostgreSQL and a fake provider.
//!
//! The ledger is tested through its own functions (reservation, settlement, the recovery hook,
//! notices, months) and through the job runner, with test kinds that call the use cases as the
//! `inbox.classify` and `message.generate` kinds will, or that abandon a reservation as a crash
//! would. Calls go to a fake OpenAI-compatible server on the loopback interface (`ai/fake.rs`),
//! which answers each test's script and records what it was sent.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Ai;
use super::classify::{Classified, Inbound, Skip, classify};
use super::fake::{Fake, Reply, json_answer, last_user_turn};
use super::snippets::{Snippets, SnippetsRequest, generate};
use super::store::settle_abandoned;
use super::store::{self, NewCall, Reserved};
use crate::domain::ai::{
    CallOutcome, CallState, ModelEntry, Settlement, Usage, UseCase, Verdicts, cost_micros,
};
use crate::domain::ids::{AiCall, Id, WorkspaceId};
use crate::domain::time::{Date, Timestamp};
use crate::jobs::runner::Harness;
use crate::jobs::{
    self, Effect, Job, JobContext, JobError, JobId, Outcome, Queue, RecoveryHook, Registry,
};
use crate::testing::TestDb;

/// The fake model's catalogue entry: $1 and $5 per million tokens.
fn entry() -> ModelEntry {
    format!("{}=1:5", super::fake::MODEL)
        .parse()
        .expect("an entry")
}

/// A verdict the fake answers with.
fn verdict(classification: &str, confidence: f64) -> Value {
    json!({
        "classification": classification,
        "sentiment": "neutral",
        "confidence": confidence,
        "reasons": ["Mentions a return date"],
    })
}

/// What a classification came to, as a test kind records it in its progress.
fn summary(classified: &Classified) -> Value {
    match classified {
        Classified::Apply(verdict) => json!({
            "result": "apply",
            "classification": verdict.classification.as_str(),
            "confidence": verdict.confidence,
        }),
        Classified::Review(verdict) => json!({
            "result": "review",
            "classification": verdict.classification.as_str(),
        }),
        Classified::Skipped(skip) => json!({
            "result": "skipped",
            "skip": match skip {
                Skip::Off => "off",
                Skip::Unavailable => "unavailable",
                Skip::OverBudget => "over_budget",
                Skip::Paused(_) => "paused",
                Skip::Refused => "refused",
                Skip::Truncated => "truncated",
                Skip::Invalid => "invalid",
                Skip::Provider { .. } => "provider",
            },
            "retry_after": skip.retry_after().map(|wait| wait.as_secs()),
        }),
    }
}

/// Classifies one message as the `inbox.classify` kind will, and records what came of it as
/// its progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClassifyMessage {
    body: String,
}

impl Job for ClassifyMessage {
    const KIND: &'static str = "test.ai_classify";
    const QUEUE: Queue = Queue::Ai;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const RECOVERY_HOOK: Option<RecoveryHook> = Some(settle_abandoned);

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let inbound = Inbound {
            from: Some("Ada Lovelace <ada@example.com>"),
            subject: Some("Re: Faster onboarding"),
            auto_submitted: None,
            in_reply_to: Some("<0190.thr.tag@mail.example>"),
            body: &self.body,
        };
        let classified = classify(cx, &inbound).await?;
        let chunk = cx.begin().await?;
        cx.checkpoint(chunk, summary(&classified)).await?;
        Ok(Outcome::Done)
    }
}

/// Writes snippets as the `message.generate` kind will, and records them as its progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WriteSnippets {
    names: Vec<String>,
    deadline_passed: bool,
}

impl Job for WriteSnippets {
    const KIND: &'static str = "test.ai_snippets";
    const QUEUE: Queue = Queue::Ai;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const RECOVERY_HOOK: Option<RecoveryHook> = Some(settle_abandoned);

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let fields =
            json!({"given_name": "Ada", "company": "Acme Robotics", "phone": "+1 555 010 9999"});
        let fields = fields.as_object().cloned().unwrap_or_default();
        let deadline = self
            .deadline_passed
            .then(|| crate::process::now().minus(Duration::from_secs(1)));
        let snippets = generate(
            cx,
            &SnippetsRequest {
                instructions: "Write a one-line opener about their company.",
                names: &self.names,
                fields: &fields,
                deadline,
            },
        )
        .await?;
        let progress = match snippets {
            Snippets::Generated(snippets) => json!({"generated": snippets}),
            Snippets::Defaults(reason) => json!({"defaults": reason.as_str()}),
        };
        let chunk = cx.begin().await?;
        cx.checkpoint(chunk, progress).await?;
        Ok(Outcome::Done)
    }
}

/// Reserves a call and ends before calling: by losing its lease, as a crashed worker would, or
/// with an error, as a failed settlement would.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Abandon {
    lose_lease: bool,
}

impl Job for Abandon {
    const KIND: &'static str = "test.ai_abandon";
    const QUEUE: Queue = Queue::Ai;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const RECOVERY_HOOK: Option<RecoveryHook> = Some(settle_abandoned);

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let entry = entry();
        let call = NewCall {
            id: Id::new(),
            job: cx.id(),
            use_case: UseCase::Classification,
            entry: &entry,
            prompt_id: "classification/reply-v1",
            canary: false,
            reserved: 5_000,
        };
        store::reserve(cx.db(), cx.workspace(), &call, crate::process::now()).await?;
        if self.lose_lease {
            Err(JobError::ClaimLost)
        } else {
            Err(JobError::Failed("the settlement failed".to_owned()))
        }
    }
}

/// A test's database, one workspace with classification on and a ten-dollar budget, and a
/// runner of the test kinds whose AI is the fake at `fake` with a deadline of
/// `timeout_seconds`.
struct World {
    test: TestDb,
    workspace: WorkspaceId,
    runner: Harness,
}

async fn world(fake: &Fake, timeout_seconds: u64) -> World {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await.id;
    set_ai(
        &test,
        workspace,
        json!({"classify_replies": true, "monthly_budget_usd": 10, "review_sample": 0}),
    )
    .await;
    let mut registry = Registry::default();
    registry
        .register::<ClassifyMessage>()
        .unwrap()
        .register::<WriteSnippets>()
        .unwrap()
        .register::<Abandon>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(Ai::from_args(&fake.args(timeout_seconds)).unwrap());
    let runner = Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "worker-ai-test",
    );
    World {
        test,
        workspace,
        runner,
    }
}

/// Sets `workspace`'s `settings.ai`.
async fn set_ai(test: &TestDb, workspace: WorkspaceId, ai: Value) {
    sqlx::query!(
        "UPDATE workspaces SET settings = jsonb_set(settings, '{ai}', $2) WHERE id = $1",
        workspace.uuid(),
        ai,
    )
    .execute(test.system.pool())
    .await
    .unwrap();
}

/// Enqueues `job` in the world's workspace, runs one claim of the `ai` queue, and returns the
/// job's id, its run's outcome and its progress.
async fn run<J: Job>(world: &World, job: &J) -> (JobId, &'static str, Value) {
    let mut tx = world.test.worker.begin_in(world.workspace).await.unwrap();
    let id = jobs::enqueue(&mut tx, world.workspace, job, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let ran = world.runner.run_once(Queue::Ai, 1).await;
    assert_eq!(ran.len(), 1, "one job ran");
    let progress = sqlx::query_scalar!("SELECT progress FROM jobs WHERE id = $1", id.uuid())
        .fetch_one(world.test.system.pool())
        .await
        .unwrap()
        .unwrap_or(Value::Null);
    (id, ran[0].1, progress)
}

/// One call row, as the tests read it.
#[derive(Debug, PartialEq)]
struct CallRow {
    state: String,
    outcome: Option<String>,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    settled_micros: Option<i64>,
    reserved_micros: i64,
    month: Date,
}

async fn calls(test: &TestDb, workspace: WorkspaceId) -> Vec<CallRow> {
    sqlx::query_as!(
        CallRow,
        r#"SELECT state, outcome, input_tokens, output_tokens, settled_micros, reserved_micros,
                  month AS "month: Date"
             FROM ai_calls WHERE workspace_id = $1 ORDER BY id"#,
        workspace.uuid(),
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// A month's usage row, as the tests read it.
#[derive(Debug, PartialEq, Eq)]
struct UsageRow {
    month: Date,
    calls: i32,
    cost_micros: i64,
    reserved_cost_micros: i64,
    budget_micros: i64,
}

async fn usage(test: &TestDb, workspace: WorkspaceId) -> Vec<UsageRow> {
    sqlx::query_as!(
        UsageRow,
        r#"SELECT month AS "month: Date", calls, cost_micros, reserved_cost_micros, budget_micros
             FROM ai_usage WHERE workspace_id = $1 ORDER BY month"#,
        workspace.uuid(),
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// The workspace's outbox events: type and data.
async fn events(test: &TestDb, workspace: WorkspaceId) -> Vec<(String, Value)> {
    sqlx::query!(
        r#"SELECT type AS kind, payload -> 'data' AS "data!" FROM outbox_events
            WHERE workspace_id = $1 ORDER BY id"#,
        workspace.uuid(),
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.kind, row.data))
    .collect()
}

/// A reservation of `reserved` micro-dollars for a new job, made directly in the store.
async fn reserve(
    test: &TestDb,
    workspace: WorkspaceId,
    job: JobId,
    reserved: u64,
    now: Timestamp,
) -> (Id<AiCall>, Reserved) {
    let entry = entry();
    let id = Id::new();
    let call = NewCall {
        id,
        job,
        use_case: UseCase::Classification,
        entry: &entry,
        prompt_id: "classification/reply-v1",
        canary: false,
        reserved,
    };
    let reserved = store::reserve(&test.worker, workspace, &call, now)
        .await
        .unwrap();
    (id, reserved)
}

/// A completed settlement charged `charged` micro-dollars.
fn completed(charged: u64) -> Settlement {
    Settlement {
        state: CallState::Settled,
        outcome: Some(CallOutcome::Completed),
        charged,
        usage: Some(Usage {
            input_tokens: 100,
            output_tokens: 20,
        }),
    }
}

/// A workspace whose budget is `budget_usd`, without a fake (the store tests).
async fn store_world(budget_usd: Value) -> (TestDb, WorkspaceId) {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await.id;
    set_ai(
        &test,
        workspace,
        json!({ "monthly_budget_usd": budget_usd }),
    )
    .await;
    (test, workspace)
}

/// A reservation holds its bound in the month's open reservations; its settlement moves the
/// month once (the charge joins the spend, the bound leaves the reservations, one call is
/// counted) and a second settlement of the same call changes nothing: settlement is once-only.
#[tokio::test]
async fn a_reservation_is_settled_once() {
    let (test, workspace) = store_world(json!(10)).await;
    let (call, reserved) =
        reserve(&test, workspace, JobId::new(), 5_000, crate::process::now()).await;
    assert_eq!(reserved, Reserved::Admitted);
    let month = usage(&test, workspace).await;
    assert_eq!(
        (
            month[0].calls,
            month[0].cost_micros,
            month[0].reserved_cost_micros
        ),
        (0, 0, 5_000)
    );
    assert_eq!(month[0].budget_micros, 10_000_000);
    let settled = store::settle(&test.worker, workspace, call, &completed(200))
        .await
        .unwrap();
    assert!(settled.is_some());
    let again = store::settle(&test.worker, workspace, call, &completed(999))
        .await
        .unwrap();
    assert!(again.is_none(), "a settled call settles no more");
    let month = usage(&test, workspace).await;
    assert_eq!(
        (
            month[0].calls,
            month[0].cost_micros,
            month[0].reserved_cost_micros
        ),
        (1, 200, 0)
    );
    let row = &calls(&test, workspace).await[0];
    assert_eq!(
        (
            row.state.as_str(),
            row.outcome.as_deref(),
            row.settled_micros
        ),
        ("settled", Some("completed"), Some(200))
    );
}

/// The call's own settlement and the recovery hook race for one reservation: exactly one of
/// them settles it, and the month is moved by that one alone, whichever wins.
#[tokio::test]
async fn two_settlers_race_and_one_wins() {
    let (test, workspace) = store_world(json!(10)).await;
    let job = JobId::new();
    let (call, _) = reserve(&test, workspace, job, 5_000, crate::process::now()).await;
    let settlement = completed(200);
    let own = store::settle(&test.worker, workspace, call, &settlement);
    let recovery = async {
        let mut tx = test.worker.begin_in(workspace).await.unwrap();
        settle_abandoned(&mut tx, workspace, job).await.unwrap();
        tx.commit().await.unwrap();
    };
    let (own, ()) = tokio::join!(own, recovery);
    let own_won = own.unwrap().is_some();
    let row = &calls(&test, workspace).await[0];
    let expected = if own_won {
        ("settled", 200)
    } else {
        ("interrupted", 5_000)
    };
    assert_eq!(
        (row.state.as_str(), row.settled_micros),
        (expected.0, Some(expected.1))
    );
    let month = usage(&test, workspace).await;
    assert_eq!(
        (
            month[0].calls,
            month[0].cost_micros,
            month[0].reserved_cost_micros
        ),
        (1, expected.1, 0)
    );
}

/// A call reserved in the last second of January is charged to January when it settles in
/// another month: the reservation's month is the settlement's month.
#[tokio::test]
async fn a_call_belongs_to_the_month_it_was_reserved_in() {
    let (test, workspace) = store_world(json!(10)).await;
    let late = Timestamp("2026-01-31T23:59:59Z".parse().unwrap());
    let (call, _) = reserve(&test, workspace, JobId::new(), 5_000, late).await;
    store::settle(&test.worker, workspace, call, &completed(300))
        .await
        .unwrap();
    let months = usage(&test, workspace).await;
    assert_eq!(months.len(), 1);
    assert_eq!(months[0].month.to_string(), "2026-01-01");
    assert_eq!(
        (months[0].cost_micros, months[0].reserved_cost_micros),
        (300, 0)
    );
    assert_eq!(
        calls(&test, workspace).await[0].month.to_string(),
        "2026-01-01"
    );
}

/// A month whose budget cannot admit a call refuses it without a row or a reservation, and
/// tells the workspace once that its budget is exhausted, however many calls are refused; a
/// budget change re-arms the notice and admits the call again.
#[tokio::test]
async fn a_refusal_reserves_nothing_and_notifies_once() {
    let (test, workspace) = store_world(json!(0.01)).await;
    for _ in 0..2 {
        let (_, reserved) = reserve(
            &test,
            workspace,
            JobId::new(),
            20_000,
            crate::process::now(),
        )
        .await;
        assert_eq!(reserved, Reserved::Refused);
    }
    assert!(calls(&test, workspace).await.is_empty());
    let month = &usage(&test, workspace).await[0];
    assert_eq!(
        (month.reserved_cost_micros, month.budget_micros),
        (0, 10_000)
    );
    let notices = events(&test, workspace).await;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, "ai.budget_exceeded");
    assert_eq!(
        notices[0].1,
        json!({"month": month.month, "spent_micros": 0, "budget_micros": 10_000})
    );
    set_ai(&test, workspace, json!({"monthly_budget_usd": 1})).await;
    let (_, reserved) = reserve(
        &test,
        workspace,
        JobId::new(),
        20_000,
        crate::process::now(),
    )
    .await;
    assert_eq!(reserved, Reserved::Admitted);
}

/// The warning is recorded once when settled spend reaches 80 % of the budget and the
/// exhaustion once when it reaches 100 %; a call refused afterwards adds no notice.
#[tokio::test]
async fn notices_fire_once_at_the_thresholds() {
    let (test, workspace) = store_world(json!(0.01)).await;
    let (first, _) = reserve(&test, workspace, JobId::new(), 9_000, crate::process::now()).await;
    store::settle(&test.worker, workspace, first, &completed(7_999))
        .await
        .unwrap();
    assert!(
        events(&test, workspace).await.is_empty(),
        "79.99 % is no warning"
    );
    let (second, _) = reserve(&test, workspace, JobId::new(), 1_000, crate::process::now()).await;
    store::settle(&test.worker, workspace, second, &completed(1))
        .await
        .unwrap();
    let (third, _) = reserve(&test, workspace, JobId::new(), 2_000, crate::process::now()).await;
    store::settle(&test.worker, workspace, third, &completed(2_000))
        .await
        .unwrap();
    let (_, refused) = reserve(&test, workspace, JobId::new(), 1, crate::process::now()).await;
    assert_eq!(refused, Reserved::Refused);
    let kinds: Vec<String> = events(&test, workspace)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["ai.budget_warning", "ai.budget_exceeded"]);
}

/// A run that reserved a call and lost its lease (a crashed worker) leaves the reservation
/// open; the recovery of its lease settles it as interrupted at its full bound in the same
/// transaction that makes the job available again, and a second recovery changes nothing.
#[tokio::test]
async fn recovery_settles_an_abandoned_call_once() {
    let fake = Fake::start(|_| Reply::Status {
        status: 500,
        retry_after: None,
    })
    .await;
    let world = world(&fake, 2).await;
    let (id, outcome, _) = run(&world, &Abandon { lose_lease: true }).await;
    assert_eq!(outcome, "lost");
    assert_eq!(
        calls(&world.test, world.workspace).await[0].state,
        "reserved"
    );
    sqlx::query!(
        "UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
        id.uuid()
    )
    .execute(world.test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        world.runner.recover_once().await,
        vec![(id, "available".to_owned())]
    );
    let row = &calls(&world.test, world.workspace).await[0];
    assert_eq!(
        (
            row.state.as_str(),
            row.outcome.as_deref(),
            row.settled_micros
        ),
        ("interrupted", Some("interrupted"), Some(5_000))
    );
    assert!(world.runner.recover_once().await.is_empty());
    let month = &usage(&world.test, world.workspace).await[0];
    assert_eq!(
        (month.calls, month.cost_micros, month.reserved_cost_micros),
        (1, 5_000, 0)
    );
}

/// A run that ends with an error after reserving (a failed settlement) leaves nothing open
/// either: its conclusion settles the reservation as interrupted before the job is retried.
#[tokio::test]
async fn a_concluded_run_leaves_no_reservation_open() {
    let fake = Fake::start(|_| Reply::Status {
        status: 500,
        retry_after: None,
    })
    .await;
    let world = world(&fake, 2).await;
    let (_, outcome, _) = run(&world, &Abandon { lose_lease: false }).await;
    assert_eq!(outcome, "retry");
    assert_eq!(
        calls(&world.test, world.workspace).await[0].state,
        "interrupted"
    );
    assert_eq!(
        usage(&world.test, world.workspace).await[0].reserved_cost_micros,
        0
    );
}

/// A completed call is applied and settled with the usage the provider reported, at the
/// model's prices; the request carries the schema in strict mode, the redacted excerpt, and no
/// temperature.
#[tokio::test]
async fn a_completed_call_is_applied_and_settled_with_its_usage() {
    let fake = Fake::start(|_| json_answer(&verdict("out_of_office", 0.92))).await;
    let world = world(&fake, 2).await;
    let body = "I am away until Monday. Call +1 555 010 9999.".to_owned();
    let (_, outcome, progress) = run(&world, &ClassifyMessage { body }).await;
    assert_eq!(outcome, "done");
    assert_eq!(
        progress,
        json!({"result": "apply", "classification": "out_of_office", "confidence": 0.92})
    );
    let row = &calls(&world.test, world.workspace).await[0];
    let charged = i64::try_from(cost_micros(100, 20, entry().price)).unwrap();
    assert_eq!(
        (
            row.state.as_str(),
            row.outcome.as_deref(),
            row.input_tokens,
            row.output_tokens,
            row.settled_micros
        ),
        (
            "settled",
            Some("completed"),
            Some(100),
            Some(20),
            Some(charged)
        )
    );
    assert!(row.reserved_micros >= charged);
    let month = &usage(&world.test, world.workspace).await[0];
    assert_eq!(
        (month.cost_micros, month.reserved_cost_micros),
        (charged, 0)
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request["model"], "gpt-6-luna");
    assert_eq!(request["response_format"]["json_schema"]["strict"], true);
    assert!(request.get("temperature").is_none());
    let input = last_user_turn(request);
    assert!(
        input.contains("I am away until Monday. Call [phone]."),
        "{input}"
    );
}

/// A refusal and a truncation are billed answers: each is settled with its usage and its
/// outcome, and the rules' verdict stands.
#[tokio::test]
async fn refusals_and_truncations_are_settled_and_skipped() {
    let fake = Fake::start(|request| {
        if last_user_turn(request).contains("refuse") {
            Reply::Refusal
        } else {
            Reply::Answer {
                content: "{\"classification\": \"human_".to_owned(),
                finish: "length",
                usage: Some((100, 1_024)),
            }
        }
    })
    .await;
    let world = world(&fake, 2).await;
    let (_, _, refused) = run(
        &world,
        &ClassifyMessage {
            body: "refuse".to_owned(),
        },
    )
    .await;
    assert_eq!(refused["skip"], "refused");
    let (_, _, truncated) = run(
        &world,
        &ClassifyMessage {
            body: "cut".to_owned(),
        },
    )
    .await;
    assert_eq!(truncated["skip"], "truncated");
    let rows = calls(&world.test, world.workspace).await;
    let price = entry().price;
    assert_eq!(
        rows.iter()
            .map(|row| (row.outcome.as_deref(), row.settled_micros))
            .collect::<Vec<_>>(),
        [
            (
                Some("refused"),
                i64::try_from(cost_micros(100, 5, price)).ok()
            ),
            (
                Some("truncated"),
                i64::try_from(cost_micros(100, 1_024, price)).ok()
            ),
        ]
    );
}

/// An answer that fails its checks is settled as invalid and shown to the model once with
/// what is wrong; a valid second answer is applied. Two invalid answers give up.
#[tokio::test]
async fn an_invalid_answer_is_corrected_once() {
    let fake = Fake::start(|request| {
        let turns = request["messages"].as_array().map_or(0, Vec::len);
        if turns <= 2 || request.to_string().contains("twice") {
            json_answer(&verdict("bounce", 0.9))
        } else {
            json_answer(&verdict("human_reply", 0.9))
        }
    })
    .await;
    let world = world(&fake, 2).await;
    let (_, _, corrected) = run(
        &world,
        &ClassifyMessage {
            body: "Sounds good, call me.".to_owned(),
        },
    )
    .await;
    assert_eq!(corrected["result"], "apply");
    assert_eq!(corrected["classification"], "human_reply");
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let correction = last_user_turn(&requests[1]);
    assert!(correction.contains("not valid") && correction.contains("`classification`"));
    assert_eq!(requests[1]["messages"][2]["role"], "assistant");
    let outcomes: Vec<Option<String>> = calls(&world.test, world.workspace)
        .await
        .into_iter()
        .map(|row| row.outcome)
        .collect();
    assert_eq!(
        outcomes,
        [
            Some("invalid_output".to_owned()),
            Some("completed".to_owned())
        ]
    );
    let (_, _, failed) = run(
        &world,
        &ClassifyMessage {
            body: "twice".to_owned(),
        },
    )
    .await;
    assert_eq!(failed["skip"], "invalid");
    assert_eq!(fake.requests().len(), 4);
}

/// A rate limit the provider asks to wait out beyond the call's deadline ends the call at
/// once, settled at nothing (the provider bills no `429`), and pauses classification in this
/// process for the provider's wait: the next message is skipped without any request.
#[tokio::test]
async fn a_rate_limit_pauses_the_use_case() {
    let fake = Fake::start(|_| Reply::Status {
        status: 429,
        retry_after: Some(120),
    })
    .await;
    let world = world(&fake, 2).await;
    let (_, _, limited) = run(
        &world,
        &ClassifyMessage {
            body: "Hi".to_owned(),
        },
    )
    .await;
    assert_eq!(limited["skip"], "provider");
    assert_eq!(limited["retry_after"], 120);
    let (_, _, paused) = run(
        &world,
        &ClassifyMessage {
            body: "Hi again".to_owned(),
        },
    )
    .await;
    assert_eq!(paused["skip"], "paused");
    assert_eq!(fake.requests().len(), 1);
    let rows = calls(&world.test, world.workspace).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].outcome.as_deref(), rows[0].settled_micros),
        (Some("provider_error"), Some(0))
    );
}

/// A provider slower than the call's deadline ends the call as a timeout, charged its full
/// reservation: the provider may have billed an answer that never arrived.
#[tokio::test]
async fn a_deadline_is_charged_the_reservation() {
    let fake = Fake::start(|_| {
        Reply::Late(
            Duration::from_secs(3),
            Box::new(json_answer(&verdict("human_reply", 0.9))),
        )
    })
    .await;
    let world = world(&fake, 1).await;
    let (_, _, timed_out) = run(
        &world,
        &ClassifyMessage {
            body: "Hi".to_owned(),
        },
    )
    .await;
    assert_eq!(timed_out["skip"], "provider");
    let row = &calls(&world.test, world.workspace).await[0];
    assert_eq!(row.outcome.as_deref(), Some("timeout"));
    assert_eq!(row.settled_micros, Some(row.reserved_micros));
}

/// Nothing is sent for a workspace that has not turned classification on, nor beyond its
/// budget: both are skipped without a request, and the budget's exhaustion is notified.
#[tokio::test]
async fn nothing_is_sent_when_off_or_over_budget() {
    let fake = Fake::start(|_| json_answer(&verdict("human_reply", 0.9))).await;
    let world = world(&fake, 2).await;
    set_ai(
        &world.test,
        world.workspace,
        json!({"classify_replies": false}),
    )
    .await;
    let (_, _, off) = run(
        &world,
        &ClassifyMessage {
            body: "Hi".to_owned(),
        },
    )
    .await;
    assert_eq!(off["skip"], "off");
    set_ai(
        &world.test,
        world.workspace,
        json!({"classify_replies": true, "monthly_budget_usd": 0}),
    )
    .await;
    let (_, _, broke) = run(
        &world,
        &ClassifyMessage {
            body: "Hi".to_owned(),
        },
    )
    .await;
    assert_eq!(broke["skip"], "over_budget");
    assert!(fake.requests().is_empty());
    assert!(calls(&world.test, world.workspace).await.is_empty());
    let kinds: Vec<String> = events(&world.test, world.workspace)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["ai.budget_exceeded"]);
}

/// A verdict below the workspace's confidence threshold asks a person to review it, and its call
/// row records the low-confidence review a canary prompt is guarded by. A confident verdict drawn
/// into the review sample (here every one, a share of 1) asks for a review too, so production
/// precision can be measured, but its row records no low-confidence review: the sample is drawn
/// at random and says nothing about the prompt.
#[tokio::test]
async fn a_doubtful_or_sampled_verdict_asks_for_review() {
    let fake = Fake::start(|request| {
        let confidence = if last_user_turn(request).contains("Ticket") {
            0.5
        } else {
            0.95
        };
        json_answer(&verdict("auto_reply", confidence))
    })
    .await;
    let world = world(&fake, 2).await;
    let (_, _, doubtful) = run(
        &world,
        &ClassifyMessage {
            body: "Ticket #42".to_owned(),
        },
    )
    .await;
    assert_eq!(
        doubtful,
        json!({"result": "review", "classification": "auto_reply"})
    );
    set_ai(
        &world.test,
        world.workspace,
        json!({"classify_replies": true, "monthly_budget_usd": 10, "review_sample": 1}),
    )
    .await;
    let (_, _, sampled) = run(
        &world,
        &ClassifyMessage {
            body: "Back on Monday".to_owned(),
        },
    )
    .await;
    assert_eq!(
        sampled,
        json!({"result": "review", "classification": "auto_reply"})
    );
    let recorded = sqlx::query_scalar!(
        "SELECT review_requested FROM ai_calls WHERE workspace_id = $1 ORDER BY id",
        world.workspace.uuid(),
    )
    .fetch_all(world.test.system.pool())
    .await
    .unwrap();
    assert_eq!(recorded, [Some(true), Some(false)]);
}

/// The canary's guard reads the verdicts of every workspace, as the scheduler: the canary's own
/// verdicts as a canary, the current prompt's only between the canary's first and last verdicts
/// (the same period, so the comparison is fair and stops changing once the canary serves no
/// more), and whether the canary has given a verdict as the use case's prompt, which marks its
/// promotion. Calls without a verdict and other prompts are not counted.
#[tokio::test]
async fn the_canary_guard_reads_verdicts_across_workspaces() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await.id;
    let globex = test.workspace("globex").await.id;
    let (canary, current) = ("classification/reply-v2", "classification/reply-v1");
    let call = |workspace: WorkspaceId,
                prompt: &'static str,
                as_canary: bool,
                review: Option<bool>,
                minutes: i32| {
        let test = &test;
        async move {
            sqlx::query(
                "INSERT INTO ai_calls (workspace_id, job_id, use_case, provider, model, prompt_id, canary, month,
                                       reserved_micros, price_input_micros_per_mtok, price_output_micros_per_mtok,
                                       state, outcome, review_requested, started_at, finished_at)
                 VALUES ($1, uuidv7(), 'classification', 'openai', 'gpt-6-luna', $2, $3, date_trunc('month', now())::date,
                         0, 1000000, 5000000, CASE WHEN $4::bool IS NULL THEN 'released' ELSE 'settled' END,
                         CASE WHEN $4::bool IS NULL THEN NULL ELSE 'completed' END, $4,
                         now() + make_interval(mins => $5), now() + make_interval(mins => $5))",
            )
            .bind(workspace.uuid())
            .bind(prompt)
            .bind(as_canary)
            .bind(review)
            .bind(minutes)
            .execute(test.system.pool())
            .await
            .unwrap();
        }
    };
    call(acme, canary, true, Some(true), 0).await;
    call(globex, canary, true, Some(false), 60).await;
    call(acme, canary, true, None, 30).await;
    call(acme, current, false, Some(true), 30).await;
    call(globex, current, false, Some(false), 45).await;
    call(acme, current, false, Some(true), -10).await;
    call(globex, current, false, Some(true), 120).await;
    call(globex, "classification/reply-v0", false, Some(true), 30).await;
    let read = || store::canary_stats(&test.worker, UseCase::Classification, canary, current);
    let stats = read().await.unwrap();
    assert!(stats.first.is_some());
    assert_eq!(
        (stats.canary, stats.current, stats.promoted),
        (
            Verdicts {
                verdicts: 2,
                reviews: 1
            },
            Verdicts {
                verdicts: 2,
                reviews: 1
            },
            false
        )
    );
    call(globex, canary, false, Some(false), 180).await;
    assert!(read().await.unwrap().promoted);
}

/// Snippets are written from the usable fields only, refused when they invent contact data
/// (twice, then the template's defaults), and never asked for once the deadline has passed.
#[tokio::test]
async fn snippets_are_written_or_fall_back_to_the_defaults() {
    let fake = Fake::start(|request| {
        let names: Vec<String> = request["response_format"]["json_schema"]["schema"]["properties"]
            ["snippets"]["required"]
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let text = if names.iter().any(|name| name == "contact") {
            "Call me at +1 555 010 9999."
        } else {
            "Congrats on the growth at Acme Robotics."
        };
        let snippets: serde_json::Map<String, Value> =
            names.into_iter().map(|name| (name, json!(text))).collect();
        json_answer(&json!({ "snippets": snippets }))
    })
    .await;
    let world = world(&fake, 2).await;
    set_ai(
        &world.test,
        world.workspace,
        json!({"usable_fields": ["given_name", "company"]}),
    )
    .await;
    let opener = vec!["opener".to_owned()];
    let (_, _, written) = run(
        &world,
        &WriteSnippets {
            names: opener.clone(),
            deadline_passed: false,
        },
    )
    .await;
    assert_eq!(
        written,
        json!({"generated": {"opener": "Congrats on the growth at Acme Robotics."}})
    );
    let input = last_user_turn(&fake.requests()[0]);
    assert!(input.contains("- company: Acme Robotics"), "{input}");
    assert!(!input.contains("555"), "{input}");
    let (_, _, late) = run(
        &world,
        &WriteSnippets {
            names: opener,
            deadline_passed: true,
        },
    )
    .await;
    assert_eq!(late, json!({"defaults": "deadline"}));
    assert_eq!(fake.requests().len(), 1);
    let (_, _, invented) = run(
        &world,
        &WriteSnippets {
            names: vec!["contact".to_owned()],
            deadline_passed: false,
        },
    )
    .await;
    assert_eq!(invented, json!({"defaults": "invalid"}));
}

/// The `ai` member of a workspace settings update is checked whole, every invalid field
/// answered at once with its pointer under `/settings/ai`; other members and an absent or
/// `null` `ai` pass untouched.
#[test]
fn settings_updates_are_checked_with_pointers() {
    let settings = |value: Value| value.as_object().cloned().unwrap();
    assert!(super::check_settings(&settings(json!({"theme": "dark"}))).is_ok());
    assert!(super::check_settings(&settings(json!({"ai": null}))).is_ok());
    assert!(
        super::check_settings(&settings(json!({
            "ai": {"classify_replies": true, "monthly_budget_usd": 25, "usable_fields": ["company"]}
        })))
        .is_ok()
    );
    let problem = super::check_settings(&settings(json!({
        "ai": {"classify_replies": "yes", "monthly_budget_usd": -1}
    })))
    .unwrap_err();
    let mut pointers: Vec<&str> = problem
        .errors
        .iter()
        .map(|error| error.pointer.as_str())
        .collect();
    pointers.sort_unstable();
    assert_eq!(
        pointers,
        [
            "/settings/ai/classify_replies",
            "/settings/ai/monthly_budget_usd"
        ]
    );
}

/// The live run of classification, for a person to inspect (ignored, see the reason): six
/// messages are classified through the job runner against a fake OpenAI-compatible server on
/// 127.0.0.1:3981 that reports a large usage per call, under a five-cent budget, on a database
/// kept for `psql`. The month passes 80 % and then 100 % of its budget, an invalid answer is
/// settled and its correction refused for lack of budget, and the last message is refused
/// before any call.
#[tokio::test]
#[ignore = "a live run for a person: binds 127.0.0.1:3981 and keeps its database; run `cargo test -p norbelys-server ai::tests::live_classification -- --ignored --nocapture`, inspect the printed database with psql, then drop it"]
#[expect(
    clippy::print_stderr,
    reason = "the live run tells the person which database to inspect"
)]
async fn live_classification() {
    let fake = Fake::start_on("127.0.0.1:3981", |request| {
        let input = last_user_turn(request);
        let first = request["messages"].as_array().map_or(0, Vec::len) <= 2;
        let (classification, sentiment, confidence) = if input.contains("weekend") && first {
            ("bounce", "neutral", 0.9)
        } else if input.contains("out of the office") {
            ("out_of_office", "neutral", 0.97)
        } else if input.contains("received") {
            ("auto_reply", "neutral", 0.91)
        } else if input.contains("Not interested") {
            ("human_reply", "negative", 0.93)
        } else {
            ("human_reply", "positive", 0.88)
        };
        Reply::Answer {
            content: json!({
                "classification": classification,
                "sentiment": sentiment,
                "confidence": confidence,
                "reasons": ["The wording of the reply"],
            })
            .to_string(),
            finish: "stop",
            usage: Some((5_000, 1_000)),
        }
    })
    .await;
    let world = world(&fake, 10).await;
    set_ai(
        &world.test,
        world.workspace,
        json!({"classify_replies": true, "monthly_budget_usd": 0.05}),
    )
    .await;
    let mut outcomes = Vec::new();
    for body in [
        "Thanks, let's talk on Tuesday at 10.",
        "I am out of the office until Monday 12 October. Call +1 555 010 9999 for urgent matters.",
        "Your request #48213 has been received; a member of our team will reply within two days.",
        "Not interested, please remove me.",
        "Maybe, can you send pricing? I'm away this weekend.",
        "Sounds good.",
    ] {
        let (_, _, progress) = run(
            &world,
            &ClassifyMessage {
                body: body.to_owned(),
            },
        )
        .await;
        outcomes.push(progress);
    }
    let kinds: Vec<String> = events(&world.test, world.workspace)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["ai.budget_warning", "ai.budget_exceeded"]);
    assert_eq!(outcomes[4]["skip"], "over_budget");
    assert_eq!(outcomes[5]["skip"], "over_budget");
    let workspace = world.workspace;
    let requests = fake.requests().len();
    let database = world.test.keep();
    eprintln!(
        "live run: database {database}, workspace {workspace} ({}), {requests} requests to the fake provider; outcomes {outcomes:#?}",
        workspace.uuid()
    );
}

/// A workspace's AI usage for the month (what its `usage` object shows) is zero spend under the
/// budget in force before any call, and then the month's calls, spend and open reservations.
#[tokio::test]
async fn month_usage_shows_the_spend_under_the_budget_in_force() {
    let (test, workspace) = store_world(json!(12.5)).await;
    let now = crate::process::now();
    let read = || async {
        let mut tx = test.worker.begin_in(workspace).await.unwrap();
        let usage = store::month_usage(&mut tx, workspace, now).await.unwrap();
        tx.commit().await.unwrap();
        usage
    };
    let empty = read().await;
    assert_eq!(
        (
            empty.calls,
            empty.spent_micros,
            empty.reserved_micros,
            empty.budget_micros
        ),
        (0, 0, 0, 12_500_000)
    );
    let (call, _) = reserve(&test, workspace, JobId::new(), 5_000, now).await;
    reserve(&test, workspace, JobId::new(), 3_000, now).await;
    store::settle(&test.worker, workspace, call, &completed(200))
        .await
        .unwrap();
    let used = read().await;
    assert_eq!(
        (
            used.calls,
            used.spent_micros,
            used.reserved_micros,
            used.budget_micros
        ),
        (1, 200, 3_000, 12_500_000)
    );
    assert_eq!(used.month, empty.month);
}
