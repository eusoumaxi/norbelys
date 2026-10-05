//! Store and API tests of counters and the data lifecycle, against real PostgreSQL: the rollup's
//! drain and the nightly recount as the system login, the analytics and usage reads through the
//! router in process, and the archive, retention and erasure kinds through the runner's harness.

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use super::archive::ArchiveExport;
use super::deletion::WorkspaceDelete;
use super::retention::RetentionPrune;
use super::rollup::{self, AnalyticsRecount, AnalyticsRollup, Recounted};
use crate::domain::ids::WorkspaceId;
use crate::domain::time::{Date, Timestamp};
use crate::jobs::runner::Harness;
use crate::jobs::{self, Job, Queue, Registry, SYSTEM_WORKSPACE};
use crate::testing::{SenderSpec, TestDb};

/// A runner of this module's system kinds, as the worker registers them, with the test's object
/// store.
fn harness(test: &TestDb) -> Harness {
    let mut registry = Registry::default();
    registry
        .register::<AnalyticsRollup>()
        .unwrap()
        .register::<AnalyticsRecount>()
        .unwrap()
        .register::<ArchiveExport>()
        .unwrap()
        .register::<RetentionPrune>()
        .unwrap()
        .register::<WorkspaceDelete>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(test.storage.clone());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "analytics-test",
    )
}

/// Enqueues `job` in the `system` workspace, as its schedule does, and runs the maintenance
/// queue until nothing is due; returns every run's outcome.
async fn run_system<J: Job>(test: &TestDb, runner: &Harness, job: &J) -> Vec<&'static str> {
    let mut tx = test.system.begin().await.unwrap();
    jobs::enqueue(&mut tx, SYSTEM_WORKSPACE, job, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut outcomes = Vec::new();
    loop {
        let ran = runner.run_once(Queue::Maintenance, 1).await;
        if ran.is_empty() {
            return outcomes;
        }
        outcomes.extend(ran.into_iter().map(|(_, outcome)| outcome));
    }
}

/// The last error of the newest job of `kind`, for an assertion's message.
async fn last_error(test: &TestDb, kind: &str) -> Option<String> {
    sqlx::query_scalar("SELECT last_error FROM jobs WHERE kind = $1 ORDER BY id DESC LIMIT 1")
        .bind(kind)
        .fetch_one(test.system.pool())
        .await
        .unwrap()
}

/// Requests the deletion of `workspace` 31 days ago and runs `retention.prune`, which starts its
/// erasure, and the erasure itself.
async fn erase(test: &TestDb, runner: &Harness, workspace: WorkspaceId) {
    sqlx::query("UPDATE workspaces SET deleted_at = now() - interval '31 days' WHERE id = $1")
        .bind(workspace.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deletions (workspace_id, requested_by, requested_at) VALUES ($1, 'test', now() - interval '31 days')",
    )
    .bind(workspace.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let outcomes = run_system(test, runner, &RetentionPrune {}).await;
    assert_eq!(outcomes, ["done", "done"], "the prune, then the erasure");
}

/// Inserts `count` increments of `metric` for one campaign key of `workspace` on `day`, as a
/// producer does (the worker login, in the workspace), in one transaction.
async fn increments(
    test: &TestDb,
    workspace: WorkspaceId,
    key: Uuid,
    day: Date,
    metric: &str,
    count: i64,
) {
    let mut tx = test.worker.begin_in(workspace).await.unwrap();
    sqlx::query(
        "INSERT INTO stats_increments (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day, metric, delta)
         SELECT $1, $2, $2, 1, $2, 1, $3, $4, 1 FROM generate_series(1, $5)",
    )
    .bind(workspace.uuid())
    .bind(key)
    .bind(day)
    .bind(metric)
    .bind(count)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// The counter `metric` of one campaign key, summed over its days.
async fn counted(test: &TestDb, key: Uuid, metric: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT coalesce(sum({metric}), 0)::bigint FROM campaign_daily_stats WHERE campaign_id = $1"
    )))
    .bind(key)
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// Drains through the boundary of the database's clock now (no lag: the test's writers have
/// committed), as the system login, and commits.
async fn drain_now(test: &TestDb) -> rollup::Drained {
    let mut tx = test.system.begin().await.unwrap();
    let cutoff: Uuid = sqlx::query_scalar("SELECT uuidv7_boundary(clock_timestamp())")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let drained = rollup::drain(&mut tx, cutoff).await.unwrap();
    tx.commit().await.unwrap();
    drained
}

/// Concurrent writers and two concurrent drains count every increment exactly once, and a drain
/// that dies before its commit loses nothing: the counters are summed from an append-only stream
/// by several workers, so a range counted twice or skipped would make every dashboard and winner
/// selection wrong. The concurrent drains use a lag longer than any writer's transaction, the
/// property the production lag guarantees.
#[tokio::test]
async fn the_rollup_counts_every_increment_once_across_writers_and_a_crash() {
    let test = TestDb::new().await;
    let workspace = test.workspace("rollup").await;
    let key = Uuid::now_v7();
    let today = Date::utc_day(crate::process::now());

    let mut writers = Vec::new();
    for _ in 0..4 {
        let test = &test;
        writers.push(async move {
            for _ in 0..10 {
                increments(test, workspace.id, key, today, "sent", 3).await;
            }
        });
    }
    let drains = async {
        for _ in 0..20 {
            let (a, b) = tokio::join!(
                async {
                    let mut tx = test.system.begin().await.unwrap();
                    let cutoff: Uuid = sqlx::query_scalar(
                        "SELECT uuidv7_boundary(clock_timestamp() - interval '2 seconds')",
                    )
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                    rollup::drain(&mut tx, cutoff).await.unwrap();
                    tx.commit().await.unwrap();
                },
                async {
                    let mut tx = test.system.begin().await.unwrap();
                    let cutoff: Uuid = sqlx::query_scalar(
                        "SELECT uuidv7_boundary(clock_timestamp() - interval '2 seconds')",
                    )
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                    rollup::drain(&mut tx, cutoff).await.unwrap();
                    tx.commit().await.unwrap();
                }
            );
            let ((), ()) = (a, b);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::join!(futures_util::future::join_all(writers), drains);

    // A drain that counts everything and then dies before its commit.
    {
        let mut tx = test.system.begin().await.unwrap();
        let cutoff: Uuid = sqlx::query_scalar("SELECT uuidv7_boundary(clock_timestamp())")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        rollup::drain(&mut tx, cutoff).await.unwrap();
        drop(tx);
    }
    let drained = drain_now(&test).await;
    assert_eq!(counted(&test, key, "sent").await, 120);
    // Draining again through the same instant adds nothing.
    let again = drain_now(&test).await;
    assert!(again.processed_to >= drained.processed_to);
    assert_eq!(counted(&test, key, "sent").await, 120);
    let (stats, computed_at) = super::campaign_stats(
        &mut test.app.begin_in(workspace.id).await.unwrap(),
        workspace.id,
        &[key],
    )
    .await
    .unwrap();
    assert_eq!(stats.get(&key).map(|counters| counters.sent), Some(120));
    assert!(computed_at.is_some());
}

/// Increments of an aborted transaction never count, and increments of a workspace that was
/// erased are skipped: the first would count facts that never happened, the second would bring
/// back the counters of a deleted workspace.
#[tokio::test]
async fn the_rollup_skips_aborted_facts_and_erased_workspaces() {
    let test = TestDb::new().await;
    let kept = test.workspace("kept").await;
    let gone = test.workspace("gone").await;
    let key = Uuid::now_v7();
    let other = Uuid::now_v7();
    let today = Date::utc_day(crate::process::now());
    {
        let mut tx = test.worker.begin_in(kept.id).await.unwrap();
        sqlx::query(
            "INSERT INTO stats_increments (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day, metric, delta)
             VALUES ($1, $2, $2, 1, $2, 1, $3, 'sent', 1)",
        )
        .bind(kept.id.uuid())
        .bind(key)
        .bind(today)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    }
    increments(&test, kept.id, key, today, "delivered", 2).await;
    increments(&test, gone.id, other, today, "sent", 5).await;
    erase(&test, &harness(&test), gone.id).await;
    drain_now(&test).await;
    assert_eq!(counted(&test, key, "sent").await, 0);
    assert_eq!(counted(&test, key, "delivered").await, 2);
    assert_eq!(counted(&test, other, "sent").await, 0);
}

/// The recount overwrites a day's counters with the sums of its increments, marks them verified,
/// records the day and reports the drift it corrected; a day whose increments leaf is gone is
/// marked unverified instead. The recount is what proves the rollup's bound each night, so a
/// counter that drifted must be corrected and the correction measured.
#[tokio::test]
async fn the_recount_overwrites_records_the_day_and_measures_drift() {
    let test = TestDb::new().await;
    let workspace = test.workspace("recount").await;
    let key = Uuid::now_v7();
    let now = crate::process::now();
    let yesterday = Date::utc_day(now.minus(Duration::from_secs(86_400)));
    increments(&test, workspace.id, key, yesterday, "sent", 4).await;
    increments(&test, workspace.id, key, yesterday, "opened", 2).await;
    drain_now(&test).await;
    sqlx::query(
        "UPDATE campaign_daily_stats SET sent = sent + 5, opened = 0 WHERE campaign_id = $1",
    )
    .bind(key)
    .execute(test.system.pool())
    .await
    .unwrap();

    let mut tx = test.system.begin().await.unwrap();
    // The database's clock generated the ids: the cutoff is read from it, not from this process's.
    let cutoff: Uuid = sqlx::query_scalar("SELECT uuidv7_boundary(clock_timestamp())")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    let outcome = rollup::recount(&mut tx, yesterday, cutoff).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        outcome,
        Recounted::Verified {
            drift: [5, 0, 0, 2, 0, 0, 0, 0]
        }
    );
    assert_eq!(counted(&test, key, "sent").await, 4);
    assert_eq!(counted(&test, key, "opened").await, 2);
    let verification: String =
        sqlx::query_scalar("SELECT verification FROM campaign_daily_stats WHERE campaign_id = $1")
            .bind(key)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(verification, "verified");
    let mut tx = test.system.begin().await.unwrap();
    assert_eq!(
        rollup::last_recounted(&mut tx).await.unwrap(),
        Some(yesterday)
    );
    drop(tx);

    // The leaf holding the day's first increments is gone: the day cannot be reconciled.
    let earlier = Date::utc_day(now.minus(Duration::from_secs(3 * 86_400)));
    let lost = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO campaign_daily_stats (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day, sent)
         VALUES ($1, $2, $2, 1, $2, 1, $3, 9)",
    )
    .bind(workspace.id.uuid())
    .bind(lost)
    .bind(earlier)
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO partition_leaves (parent, name, lower, upper, dropped_at)
         VALUES ('stats_increments', 'stats_increments_gone', $1::date::timestamp AT TIME ZONE 'UTC',
                 ($1::date + 1)::timestamp AT TIME ZONE 'UTC', now())",
    )
    .bind(earlier)
    .execute(test.system.pool())
    .await
    .unwrap();
    let mut tx = test.system.begin().await.unwrap();
    let outcome = rollup::recount(&mut tx, earlier, cutoff).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(outcome, Recounted::Unverified);
    let row: (i32, String) = sqlx::query_as(
        "SELECT sent, verification FROM campaign_daily_stats WHERE campaign_id = $1",
    )
    .bind(lost)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(row, (9, "unverified".to_owned()));
}

/// The rebuild recomputes a past day from the facts (an accepted message, its delivery, a human
/// open) and refuses yesterday and a day whose facts are partly archived: it is the repair for a
/// day whose increments are gone, and must neither touch a day the recount and the rollup still own
/// nor overwrite a day with the part of its facts still online.
#[tokio::test]
async fn the_rebuild_recomputes_a_past_day_from_the_facts() {
    let test = TestDb::new().await;
    let workspace = test.workspace("rebuild").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("rebuild@sender.test"))
        .await;
    let campaign = test.campaign(workspace.id, &sender, None).await;
    let message = test
        .campaign_message(workspace.id, campaign, &sender, "person@example.test", 0)
        .await;
    let now = crate::process::now();
    let day = Date::utc_day(now.minus(Duration::from_secs(2 * 86_400)));
    let noon = "12:00:00+00";
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH q AS (DELETE FROM delivery_queue WHERE message_id = $1)
         UPDATE messages SET state = 'sent', sent_at = ($2::date || ' {noon}')::timestamptz WHERE id = $1"
    )))
    .bind(message.uuid())
    .bind(day)
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO delivery_events (workspace_id, message_id, recipient_ref, source, source_event_id, kind, category,
                                      confidence, observed_at, created_at)
         VALUES ($1, $2, 'unknown', 'smtp', 'attempt:1', 'delivered', 'delivered', 'authenticated',
                 ($3::date || ' {noon}')::timestamptz, ($3::date || ' {noon}')::timestamptz)"
    )))
    .bind(workspace.id.uuid())
    .bind(message.uuid())
    .bind(day)
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT ensure_partition('tracking_events', ($1::date || ' {noon}')::timestamptz)"
    )))
    .bind(day)
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO tracking_events (workspace_id, id, message_id, kind, actor_class, occurred_at)
         VALUES ($1, gen_random_uuid(), $2, 'open', 'human', ($3::date || ' {noon}')::timestamptz),
                ($1, gen_random_uuid(), $2, 'open', 'human', ($3::date || ' {noon}')::timestamptz + interval '1 hour'),
                ($1, gen_random_uuid(), $2, 'click', 'scanner', ($3::date || ' {noon}')::timestamptz)"
    )))
    .bind(workspace.id.uuid())
    .bind(message.uuid())
    .bind(day)
    .execute(test.system.pool())
    .await
    .unwrap();

    let mut tx = test.system.begin().await.unwrap();
    assert_eq!(rollup::rebuild(&mut tx, day, now).await.unwrap(), 1);
    tx.commit().await.unwrap();
    let row: (i32, i32, i32, i32, String) = sqlx::query_as(
        "SELECT sent, delivered, opened, clicked, verification FROM campaign_daily_stats
          WHERE campaign_id = $1 AND day = $2",
    )
    .bind(campaign)
    .bind(day)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(row, (1, 1, 1, 0, "verified".to_owned()));

    let yesterday = Date::utc_day(now.minus(Duration::from_secs(86_400)));
    let mut tx = test.system.begin().await.unwrap();
    assert!(matches!(
        rollup::rebuild(&mut tx, yesterday, now).await,
        Err(rollup::RebuildError::TooRecent(_))
    ));
    drop(tx);

    // Once a period holding some of the day's events was archived, the database alone would
    // undercount the day: the rebuild refuses and leaves the counters as they are.
    sqlx::query(
        "INSERT INTO partition_leaves (parent, name, lower, upper, dropped_at)
         VALUES ('delivery_events', 'delivery_events_archived', $1::date::timestamp AT TIME ZONE 'UTC',
                 ($1::date + 1)::timestamp AT TIME ZONE 'UTC', now())",
    )
    .bind(day)
    .execute(test.system.pool())
    .await
    .unwrap();
    let mut tx = test.system.begin().await.unwrap();
    assert!(matches!(
        rollup::rebuild(&mut tx, day, now).await,
        Err(rollup::RebuildError::Archived(_))
    ));
    drop(tx);
    let sent: i32 = sqlx::query_scalar(
        "SELECT sent FROM campaign_daily_stats WHERE campaign_id = $1 AND day = $2",
    )
    .bind(campaign)
    .bind(day)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(sent, 1);
}

/// `GET /analytics` sums the workspace's counters over the range, filters by campaign, groups by
/// day or campaign, carries `computed_at`, refuses a reversed or too long range, and never shows
/// another workspace's counters: the contract of the reports surface.
#[tokio::test]
async fn analytics_reads_filter_group_and_stay_in_their_workspace() {
    let test = TestDb::new().await;
    let workspace = test.workspace("reports").await;
    let other = test.workspace("others").await;
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let now = crate::process::now();
    let today = Date::utc_day(now);
    let yesterday = Date::utc_day(now.minus(Duration::from_secs(86_400)));
    increments(&test, workspace.id, first, today, "sent", 3).await;
    increments(&test, workspace.id, first, yesterday, "sent", 2).await;
    increments(&test, workspace.id, second, today, "replied", 1).await;
    increments(&test, other.id, Uuid::now_v7(), today, "sent", 50).await;
    drain_now(&test).await;
    let app = test.app();

    let reply = app.get("/v1/analytics").bearer(&workspace.key).send().await;
    assert_eq!(reply.status, 200, "{}", reply.json);
    assert_eq!(reply.json["totals"]["sent"], 5);
    assert_eq!(reply.json["totals"]["replied"], 1);
    assert_eq!(reply.json["data"], json!([]));
    assert!(reply.json["computed_at"].is_string());

    let campaign = format!("cmp_{}", first.simple());
    let reply = app
        .get(&format!(
            "/v1/analytics?campaign_id={campaign}&group_by=day"
        ))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(reply.status, 200, "{}", reply.json);
    assert_eq!(reply.json["totals"]["sent"], 5);
    let days: Vec<(Value, Value)> = reply.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| (group["day"].clone(), group["counters"]["sent"].clone()))
        .collect();
    assert_eq!(
        days,
        vec![
            (json!(yesterday.to_string()), json!(2)),
            (json!(today.to_string()), json!(3))
        ]
    );

    let reply = app
        .get(&format!(
            "/v1/analytics?from={today}&to={today}&group_by=campaign"
        ))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(reply.status, 200, "{}", reply.json);
    assert_eq!(reply.json["totals"]["sent"], 3);
    assert_eq!(reply.json["data"].as_array().map(Vec::len), Some(2));

    let reply = app
        .get(&format!("/v1/analytics?from={today}&to={yesterday}"))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(reply.status, 422, "{}", reply.json);
    let reply = app
        .get("/v1/analytics?from=2020-01-01&to=2026-01-01")
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(reply.status, 422, "{}", reply.json);
    let reply = app
        .get("/v1/analytics?group_by=hour")
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(reply.status, 422, "{}", reply.json);
}

/// The workspace object carries the month's usage: sends from the connection ledger, people and
/// connections as they stand, AI spend against its budget, with `computed_at`; the limits a
/// deployment without a billing plan does not set are `null`.
#[tokio::test]
async fn the_workspace_shows_its_usage_for_the_month() {
    let test = TestDb::new().await;
    let workspace = test.workspace("usage").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("usage@sender.test"))
        .await;
    let campaign = test.campaign(workspace.id, &sender, None).await;
    test.campaign_message(workspace.id, campaign, &sender, "a@example.test", 0)
        .await;
    sqlx::query(
        "INSERT INTO connection_usage (workspace_id, connection_id, day, used) VALUES ($1, $2, (now() AT TIME ZONE 'UTC')::date, 7)
         ON CONFLICT (workspace_id, connection_id, day) DO UPDATE SET used = 7",
    )
    .bind(workspace.id.uuid())
    .bind(sender.connection.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let reply = test
        .app()
        .get(&format!(
            "/v1/workspaces/ws_{}",
            workspace.id.uuid().simple()
        ))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(reply.status, 200, "{}", reply.json);
    let usage = &reply.json["usage"];
    assert_eq!(usage["sends"], json!({ "used": 7, "limit": null }));
    assert_eq!(usage["people"], json!({ "used": 1, "limit": null }));
    assert_eq!(usage["connections"]["used"], 1);
    assert_eq!(usage["ai"]["spent_micros"], 0);
    assert!(usage["ai"]["budget_micros"].is_i64());
    assert!(usage["computed_at"].is_string());
    assert!(
        usage["month"]
            .as_str()
            .is_some_and(|month| month.ends_with("-01"))
    );
}

/// `retention.prune` deletes exactly the rows that are due and keeps the others: an expired
/// idempotency key, a provider event key older than three days, a job finished more than a week
/// ago, a session revoked more than 30 days ago, a code a day past its expiry and an old ledger day
/// no attempt references go; the live ones, and an old ledger day an attempt still references,
/// stay. Pruning too much breaks replays, idempotency and settlement; too little fills the disk.
#[tokio::test]
async fn retention_prunes_only_what_is_due() {
    let test = TestDb::new().await;
    let workspace = test.workspace("prune").await;
    let ws = workspace.id.uuid();
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("prune@sender.test"))
        .await;
    let campaign = test.campaign(workspace.id, &sender, None).await;
    let message = test
        .campaign_message(workspace.id, campaign, &sender, "kept@example.test", 0)
        .await;
    let system = test.system.pool();
    let statements = [
        "INSERT INTO idempotency_keys (workspace_id, key, fingerprint, expires_at)
         VALUES ($1, 'due', '\\x00', now() - interval '1 hour'), ($1, 'kept', '\\x00', now() + interval '1 hour')",
        "INSERT INTO provider_event_keys (provider_webhook_id, event_id, body_hash, received_at)
         VALUES (gen_random_uuid(), 'due', '\\x00', now() - interval '4 days'), (gen_random_uuid(), 'kept', '\\x00', now() - interval '2 days')",
        "INSERT INTO jobs (workspace_id, queue, kind, state, finished_at)
         VALUES ($1, 'exports', 'test.due', 'completed', now() - interval '8 days'),
                ($1, 'exports', 'test.due', 'failed', now() - interval '8 days'),
                ($1, 'exports', 'test.kept', 'completed', now() - interval '6 days')",
        "INSERT INTO login_codes (email_key, purpose, code_hash, link_token_hash, expires_at)
         VALUES ('due@example.test', 'sign_in', '\\x00', '\\x01', now() - interval '2 days'),
                ('kept@example.test', 'sign_in', '\\x00', '\\x02', now() - interval '1 hour')",
        "INSERT INTO connection_usage (workspace_id, connection_id, day, used)
         SELECT $1, id, d, 1 FROM connections, unnest(ARRAY[(now() AT TIME ZONE 'UTC')::date - 40,
                                                            (now() AT TIME ZONE 'UTC')::date - 41,
                                                            (now() AT TIME ZONE 'UTC')::date - 10]) AS d
          WHERE workspace_id = $1",
    ];
    for statement in statements {
        sqlx::query(statement)
            .bind(ws)
            .execute(system)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO sessions (user_id, token_hash, auth_method, expires_at, idle_expires_at, revoked_at)
         VALUES ($1, '\\x01', 'email_code', now() + interval '60 days', now() + interval '20 days', now() - interval '31 days'),
                ($1, '\\x02', 'email_code', now() + interval '60 days', now() + interval '20 days', now() - interval '29 days'),
                ($1, '\\x03', 'email_code', now() - interval '31 days', now() - interval '40 days', NULL)",
    )
    .bind(workspace.owner.uuid())
    .execute(system)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO attempts (workspace_id, message_id, attempt_number, connection_id, reserved_day, recipient_count,
                               quota_state, lease_owner, finished_at, outcome)
         VALUES ($1, $2, 1, $3, (now() AT TIME ZONE 'UTC')::date - 41, 1, 'consumed', 'test', now(), 'accepted')",
    )
    .bind(ws)
    .bind(message.uuid())
    .bind(sender.connection.uuid())
    .execute(system)
    .await
    .unwrap();

    let outcomes = run_system(&test, &harness(&test), &RetentionPrune {}).await;
    assert_eq!(outcomes, ["done"]);
    let left: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT ARRAY[
             (SELECT string_agg(key, ',') FROM idempotency_keys WHERE workspace_id = $1),
             (SELECT string_agg(event_id, ',') FROM provider_event_keys),
             (SELECT string_agg(kind, ',') FROM jobs WHERE kind LIKE 'test.%'),
             (SELECT string_agg(email_key, ',') FROM login_codes),
             (SELECT count(*)::text FROM sessions WHERE user_id = $2),
             (SELECT string_agg(((now() AT TIME ZONE 'UTC')::date - day)::text, ',' ORDER BY day)
                FROM connection_usage WHERE workspace_id = $1)]",
    )
    .bind(ws)
    .bind(workspace.owner.uuid())
    .fetch_one(system)
    .await
    .unwrap();
    assert_eq!(
        left,
        [
            "kept",
            "kept",
            "test.kept",
            "kept@example.test",
            "1",
            "41,10"
        ]
        .map(|kept| Some(kept.to_owned()))
    );
}

/// `retention.prune` deletes what object storage keeps past its time, never leaving it to a
/// bucket's rule: an expired export loses its file (the key it records, or where its job writes
/// it when it never became ready) and then its row, while a live export keeps both; an uploaded
/// import file whose import never committed is deleted once a day old, while the file of a
/// committed import and a fresh upload whose request may still be committing stay. A file kept
/// forever costs storage and keeps customer data past its promise; one deleted too early breaks
/// a download or an import.
#[tokio::test]
async fn retention_deletes_expired_exports_and_abandoned_uploads() {
    let test = TestDb::new().await;
    let workspace = test.workspace("files").await;
    let ws = workspace.id.uuid();
    let system = test.system.pool();
    let (ready, unready, live) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (id, status, hours) in [
        (ready, "ready", -1),
        (unready, "failed", -1),
        (live, "ready", 1),
    ] {
        let key = format!("exports/{ws}/{id}.csv");
        sqlx::query(
            "INSERT INTO exports (workspace_id, id, kind, status, object_key, requested_by, expires_at)
             VALUES ($1, $2, 'people', $3, $4, 'test', now() + make_interval(hours => $5))",
        )
        .bind(ws)
        .bind(id)
        .bind(status)
        .bind((status == "ready").then(|| key.clone()))
        .bind(hours)
        .execute(system)
        .await
        .unwrap();
        test.storage
            .put(&key, bytes::Bytes::from_static(b"email\n"))
            .await
            .unwrap();
    }
    let (abandoned, committed): (Uuid, Uuid) = sqlx::query_as(
        "SELECT uuidv7_boundary(now() - interval '2 days'), uuidv7_boundary(now() - interval '3 days')",
    )
    .fetch_one(system)
    .await
    .unwrap();
    let fresh = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO imports (workspace_id, id, source, status) VALUES ($1, $2, '{}', 'completed')",
    )
    .bind(ws)
    .bind(committed)
    .execute(system)
    .await
    .unwrap();
    for import in [abandoned, committed, fresh] {
        test.storage
            .put(
                &format!("imports/{ws}/{import}/source.csv"),
                bytes::Bytes::from_static(b"email\n"),
            )
            .await
            .unwrap();
    }

    let outcomes = run_system(&test, &harness(&test), &RetentionPrune {}).await;
    assert_eq!(
        outcomes,
        ["done"],
        "{:?}",
        last_error(&test, "retention.prune").await
    );
    let mut kept = test.storage.list(&format!("exports/{ws}")).await.unwrap();
    kept.extend(test.storage.list(&format!("imports/{ws}")).await.unwrap());
    assert_eq!(
        kept,
        [
            format!("exports/{ws}/{live}.csv"),
            format!("imports/{ws}/{committed}/source.csv"),
            format!("imports/{ws}/{fresh}/source.csv"),
        ]
    );
    let rows: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM exports WHERE workspace_id = $1")
        .bind(ws)
        .fetch_all(system)
        .await
        .unwrap();
    assert_eq!(rows, [live]);
}

/// `workspace.delete` leaves nothing of the workspace: no row in any table that has a
/// `workspace_id` (increments aside, which expire with their partitions), no stored file under its
/// prefixes, no workspace row; the request is completed with its tombstone, and another
/// workspace's rows are untouched. A withdrawn request erases nothing. Erasure is a promise to the
/// customer; a row left behind breaks it, and a row of another workspace erased is data loss.
#[tokio::test]
async fn workspace_deletion_leaves_nothing() {
    let test = TestDb::new().await;
    let runner = harness(&test);
    let gone = test.workspace("erased").await;
    let kept = test.workspace("kept").await;
    for workspace in [&gone, &kept] {
        let sender = test
            .sender(workspace.id, &SenderSpec::mailbox("erase@sender.test"))
            .await;
        let campaign = test.campaign(workspace.id, &sender, None).await;
        let message = test
            .campaign_message(workspace.id, campaign, &sender, "person@example.test", 0)
            .await;
        // The cycles: an enrollment's current message, a revision's winner.
        sqlx::query("UPDATE enrollments SET message_id = $2 WHERE workspace_id = $1")
            .bind(workspace.id.uuid())
            .bind(message.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
        sqlx::query(
            "UPDATE step_revisions r SET winner_variant_id = o.variant_id, winner_variant_version = o.variant_version, winner_selected_at = now()
               FROM step_revision_variants o WHERE o.workspace_id = r.workspace_id AND o.step_id = r.step_id AND r.workspace_id = $1",
        )
        .bind(workspace.id.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
        increments(
            &test,
            workspace.id,
            campaign,
            Date::utc_day(crate::process::now()),
            "sent",
            1,
        )
        .await;
        test.storage
            .put(
                &format!("exports/{}/file.csv", workspace.id.uuid()),
                bytes::Bytes::from_static(b"a,b\n"),
            )
            .await
            .unwrap();
    }
    drain_now(&test).await;

    // A withdrawn request erases nothing.
    sqlx::query(
        "INSERT INTO workspace_deletions (workspace_id, requested_by, requested_at) VALUES ($1, 'test', now() - interval '31 days')",
    )
    .bind(kept.id.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    erase(&test, &runner, gone.id).await;

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
          WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition
            AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'workspace_id' AND NOT a.attisdropped)
            AND c.relname NOT IN ('workspace_deletions', 'stats_increments')",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert!(tables.len() > 40, "{tables:?}");
    let mut kept_rows = 0_i64;
    let system = test.system.pool();
    for table in &tables {
        let count = |workspace: WorkspaceId| {
            let table = table.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM \"{table}\" WHERE workspace_id = $1"
                )))
                .bind(workspace.uuid())
                .fetch_one(system)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            count(gone.id).await,
            0,
            "{table} still holds rows of the erased workspace"
        );
        kept_rows += count(kept.id).await;
    }
    assert!(kept_rows > 10, "the other workspace kept its rows");
    let workspaces: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM workspaces WHERE id = ANY($1)")
        .bind(vec![gone.id.uuid(), kept.id.uuid()])
        .fetch_all(test.system.pool())
        .await
        .unwrap();
    assert_eq!(workspaces, vec![kept.id.uuid()]);
    let request: (bool, Option<String>) = sqlx::query_as(
        "SELECT completed_at IS NOT NULL, tombstone_object_key FROM workspace_deletions WHERE workspace_id = $1",
    )
    .bind(gone.id.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        request,
        (true, Some(format!("tombstones/{}.json", gone.id.uuid())))
    );
    assert!(
        test.storage
            .list(&format!("exports/{}", gone.id.uuid()))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        test.storage
            .list(&format!("exports/{}", kept.id.uuid()))
            .await
            .unwrap()
            .len(),
        1
    );
    // Increments of the erased workspace never come back as counters.
    increments(
        &test,
        kept.id,
        Uuid::now_v7(),
        Date::utc_day(crate::process::now()),
        "sent",
        1,
    )
    .await;
    drain_now(&test).await;
    let counters: i64 =
        sqlx::query_scalar("SELECT count(*) FROM campaign_daily_stats WHERE workspace_id = $1")
            .bind(gone.id.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(counters, 0);
}

/// Sets the online window of messages and attempts (an archive pair, changed together) so that
/// today's leaf is past it.
async fn messages_due_today(test: &TestDb) {
    sqlx::query(
        "UPDATE partition_policies SET retention = interval '-1 day' WHERE table_name IN ('messages', 'attempts')",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
}

/// The archive's gate keeps a messages period online while one of its messages is still queued
/// (retention debt, never a forced drop), and PostgreSQL itself refuses, as the owner, to detach a
/// leaf the queue still references: a period archived with live mail would lose it.
#[tokio::test]
async fn the_archive_gate_refuses_while_referenced() {
    let test = TestDb::new().await;
    let workspace = test.workspace("gate").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("gate@sender.test"))
        .await;
    let campaign = test.campaign(workspace.id, &sender, None).await;
    let message = test
        .campaign_message(workspace.id, campaign, &sender, "queued@example.test", 0)
        .await;
    messages_due_today(&test).await;
    let outcomes = run_system(&test, &harness(&test), &ArchiveExport {}).await;
    assert_eq!(outcomes, ["done"]);
    let leaf: (String, Option<Timestamp>) = sqlx::query_as(
        "SELECT name, dropped_at FROM partition_leaves
          WHERE parent = 'messages' AND lower <= now() AND now() < upper",
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(leaf.1, None, "the leaf with a queued message stays");
    let present: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE id = $1)")
        .bind(message.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert!(present);

    // The backstop: the detach itself is refused as the owner while the queue references it.
    let mut connection = test.system.pool().acquire().await.unwrap();
    connection.close_on_drop();
    sqlx::query("SET ROLE norbelys_owner")
        .execute(&mut *connection)
        .await
        .unwrap();
    let refused = sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE messages DETACH PARTITION \"{}\"",
        leaf.0
    )))
    .execute(&mut *connection)
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains("foreign key"),
        "the detach failed for another reason: {refused}"
    );
}

/// Once its period is settled, the archive takes a messages period out in the sealed order: the
/// attempts leaf first, the references released, each leaf exported to Parquet whose rows read
/// back, verified and recorded with its checksum and manifest, then dropped; a second run finds
/// nothing to do. This is the only copy once the leaf is gone, so it must be complete and
/// readable.
#[tokio::test]
async fn the_archive_exports_verifies_and_drops_a_settled_period() {
    let test = TestDb::new().await;
    let workspace = test.workspace("archive").await;
    let ws = workspace.id.uuid();
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("archive@sender.test"))
        .await;
    let campaign = test.campaign(workspace.id, &sender, None).await;
    let mut messages = Vec::new();
    for index in 0..3 {
        messages.push(
            test.campaign_message(
                workspace.id,
                campaign,
                &sender,
                &format!("person{index}@example.test"),
                0,
            )
            .await,
        );
    }
    let ids: Vec<Uuid> = messages.iter().map(|message| message.uuid()).collect();
    for statement in [
        "DELETE FROM delivery_queue WHERE workspace_id = $1",
        "UPDATE messages SET state = 'sent', sent_at = now() WHERE workspace_id = $1",
        "INSERT INTO connection_usage (workspace_id, connection_id, day, used)
         SELECT $1, id, (now() AT TIME ZONE 'UTC')::date, 3 FROM connections WHERE workspace_id = $1
         ON CONFLICT DO NOTHING",
        "INSERT INTO attempts (workspace_id, message_id, attempt_number, connection_id, reserved_day, recipient_count,
                               quota_state, lease_owner, finished_at, outcome)
         SELECT workspace_id, id, 1, connection_id, (now() AT TIME ZONE 'UTC')::date, 1, 'consumed', 'test', now(), 'accepted'
           FROM messages WHERE workspace_id = $1",
        "UPDATE enrollments e SET message_id = m.id FROM messages m
          WHERE m.workspace_id = e.workspace_id AND m.enrollment_id = e.id AND e.workspace_id = $1",
    ] {
        sqlx::query(statement)
            .bind(ws)
            .execute(test.system.pool())
            .await
            .unwrap();
    }
    messages_due_today(&test).await;
    let runner = harness(&test);
    assert_eq!(
        run_system(&test, &runner, &ArchiveExport {}).await,
        ["done"],
        "{:?}",
        last_error(&test, "archive.export").await
    );

    for table in ["messages", "attempts"] {
        let leaf: (String, Option<String>, Option<i64>, Option<String>, bool, bool) = sqlx::query_as(
            "SELECT name, archive_key, archived_rows, archive_sha256, dropped_at IS NOT NULL, to_regclass(name) IS NULL
               FROM partition_leaves WHERE parent = $1 AND lower <= now() AND now() < upper",
        )
        .bind(table)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        let (name, key, rows, sha256, dropped, gone) = leaf;
        assert!(dropped && gone, "{table}: the leaf was dropped");
        assert_eq!(key, Some(format!("archive/{table}/{name}.parquet")));
        assert_eq!(rows, Some(3), "{table}");
        let key = key.unwrap();
        let bytes = test.storage.get(&key).await.unwrap();
        assert_eq!(
            sha256,
            Some(crate::crypto::hex(&crate::crypto::sha256(&bytes)))
        );
        let path = std::env::temp_dir().join(format!("{name}-{}.parquet", Uuid::now_v7()));
        std::fs::write(&path, &bytes).unwrap();
        let mut seen = Vec::new();
        let file = std::fs::File::open(&path).unwrap();
        let rows = super::parquet::Rows::open(file, Some(&ws.to_string())).unwrap();
        let column = if table == "messages" {
            "id"
        } else {
            "message_id"
        };
        let id = rows.names().iter().position(|name| name == column).unwrap();
        for cells in rows {
            if let Some(super::parquet::Cell::Text(id)) = cells.unwrap().get(id) {
                seen.push(id.parse::<Uuid>().unwrap());
            }
        }
        std::fs::remove_file(&path).unwrap();
        seen.sort_unstable();
        let mut expected = ids.clone();
        expected.sort_unstable();
        assert_eq!(seen, expected, "{table}: every row read back");
        let manifest: Value = serde_json::from_slice(
            &test
                .storage
                .get(&format!("archive/{table}/{name}.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["rows"], 3);
    }
    let references: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM enrollments WHERE workspace_id = $1 AND message_id IS NOT NULL",
    )
    .bind(ws)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        references, 0,
        "the enrollments forgot their archived messages"
    );
    let online: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE workspace_id = $1")
        .bind(ws)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(online, 0);
    assert_eq!(
        run_system(&test, &runner, &ArchiveExport {}).await,
        ["done"]
    );
}

/// Runs the exports queue of `workspace` until nothing is due; returns every run's outcome.
async fn run_exports(runner: &Harness) -> Vec<&'static str> {
    let mut outcomes = Vec::new();
    loop {
        let ran = runner.run_once(Queue::Exports, 1).await;
        if ran.is_empty() {
            return outcomes;
        }
        outcomes.extend(ran.into_iter().map(|(_, outcome)| outcome));
    }
}

/// Requests a JSON-lines export of `resource` with `filters` and runs it; returns its rows' `id`
/// (or `message_id`) column, read back from the stored file.
async fn exported(
    test: &TestDb,
    runner: &Harness,
    key: &str,
    resource: &str,
    filters: Value,
) -> Vec<String> {
    let app = test.app();
    let accepted = app
        .post("/v1/exports")
        .bearer(key)
        .idempotency(&Uuid::now_v7().to_string())
        .json(json!({ "resource": resource, "format": "jsonl", "filters": filters }))
        .send()
        .await;
    assert_eq!(accepted.status, 202, "{}", accepted.json);
    assert_eq!(run_exports(runner).await, ["done"]);
    let id = accepted.json["id"].as_str().unwrap();
    let export = app
        .get(&format!("/v1/exports/{id}"))
        .bearer(key)
        .send()
        .await;
    assert_eq!(export.json["status"], "ready", "{}", export.json);
    let uuid = id.trim_start_matches("exp_").parse::<Uuid>().unwrap();
    let workspace: Uuid = sqlx::query_scalar("SELECT workspace_id FROM exports WHERE id = $1")
        .bind(uuid)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    let file = test
        .storage
        .get(&format!("exports/{workspace}/{uuid}.jsonl"))
        .await
        .unwrap();
    let mut ids: Vec<String> = std::str::from_utf8(&file)
        .unwrap()
        .lines()
        .map(|line| {
            let row: Value = serde_json::from_str(line).unwrap();
            row[if resource == "attempts" {
                "message_id"
            } else {
                "id"
            }]
            .as_str()
            .unwrap()
            .to_owned()
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(export.json["rows"], json!(ids.len()));
    ids
}

/// A history export reads the rows still in the database and, once their period is archived,
/// the same rows from the archive's Parquet objects, with the list's filters applied to both;
/// a filter the resource's list does not have is refused. Exports are the only way to read
/// archived ranges, so an archived period must export exactly as it did online.
#[tokio::test]
async fn a_history_export_reads_archived_periods() {
    let test = TestDb::new().await;
    let workspace = test.workspace("history").await;
    let other = test.workspace("elsewhere").await;
    let mut runner_registry = Registry::default();
    runner_registry
        .register::<ArchiveExport>()
        .unwrap()
        .register::<crate::people::exports::ExportRun>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(test.storage.clone());
    let runner = Harness::new(
        test.worker.clone(),
        test.system.clone(),
        runner_registry,
        env,
        "history-test",
    );
    let mut campaigns = Vec::new();
    for workspace in [&workspace, &other] {
        let sender = test
            .sender(workspace.id, &SenderSpec::mailbox("history@sender.test"))
            .await;
        let campaign = test.campaign(workspace.id, &sender, None).await;
        for index in 0..2 {
            test.campaign_message(
                workspace.id,
                campaign,
                &sender,
                &format!("person{index}@example.test"),
                0,
            )
            .await;
        }
        campaigns.push(campaign);
        sqlx::query(
            "WITH q AS (DELETE FROM delivery_queue WHERE workspace_id = $1)
             UPDATE messages SET state = 'sent', sent_at = now() WHERE workspace_id = $1",
        )
        .bind(workspace.id.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    }
    let campaign = format!("cmp_{}", campaigns.first().unwrap().simple());
    let mut expected: Vec<String> =
        sqlx::query_scalar("SELECT id::text FROM messages WHERE workspace_id = $1")
            .bind(workspace.id.uuid())
            .fetch_all(test.system.pool())
            .await
            .unwrap();
    expected.sort_unstable();

    let online = exported(
        &test,
        &runner,
        &workspace.key,
        "messages",
        json!({ "campaign_id": campaign }),
    )
    .await;
    assert_eq!(online, expected);

    messages_due_today(&test).await;
    assert_eq!(
        run_system(&test, &runner, &ArchiveExport {}).await,
        ["done"]
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(left, 0, "the period left the database");

    let archived = exported(
        &test,
        &runner,
        &workspace.key,
        "messages",
        json!({ "campaign_id": campaign }),
    )
    .await;
    assert_eq!(
        archived, expected,
        "the archived rows of this workspace only"
    );
    let none = exported(
        &test,
        &runner,
        &workspace.key,
        "messages",
        json!({ "campaign_id": campaign, "state": "failed" }),
    )
    .await;
    assert!(none.is_empty());

    let refused = test
        .app()
        .post("/v1/exports")
        .bearer(&workspace.key)
        .idempotency(&Uuid::now_v7().to_string())
        .json(json!({ "resource": "attempts", "filters": { "state": "sent" } }))
        .send()
        .await;
    assert_eq!(refused.status, 422, "{}", refused.json);
}

/// `retention.prune` deletes a device code a day past its expiry and the entries of both audit
/// logs older than the 180 days they are shown, and keeps the rest: nothing else ever removes a
/// device code nobody approved, nor an entry no reader is shown any longer.
#[tokio::test]
async fn retention_prunes_old_device_codes_and_audit_entries() {
    let test = TestDb::new().await;
    let workspace = test.workspace("audit").await;
    let system = test.system.pool();
    sqlx::query(
        "INSERT INTO oauth_device_codes (device_code_hash, user_code, client_id, resource, scopes, expires_at)
         VALUES ('\\x01', 'DUEDUEDU', 'norbelys-cli', 'http://api.test', '{}', now() - interval '2 days'),
                ('\\x02', 'KEPTKEPT', 'norbelys-cli', 'http://api.test', '{}', now() - interval '1 hour')",
    )
    .execute(system)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_log (workspace_id, actor_kind, actor_id, action, created_at)
         VALUES ($1, 'system', 'test', 'test.due', now() - interval '181 days'),
                ($1, 'system', 'test', 'test.kept', now() - interval '179 days')",
    )
    .bind(workspace.id.uuid())
    .execute(system)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_audit_log (user_id, actor_kind, actor_id, action, created_at)
         VALUES ($1, 'system', 'test', 'test.due', now() - interval '181 days'),
                ($1, 'system', 'test', 'test.kept', now() - interval '179 days')",
    )
    .bind(workspace.owner.uuid())
    .execute(system)
    .await
    .unwrap();

    let outcomes = run_system(&test, &harness(&test), &RetentionPrune {}).await;
    assert_eq!(outcomes, ["done"]);
    let left: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT ARRAY[
             (SELECT string_agg(user_code, ',') FROM oauth_device_codes),
             (SELECT string_agg(action, ',') FROM audit_log WHERE action LIKE 'test.%'),
             (SELECT string_agg(action, ',') FROM user_audit_log WHERE action LIKE 'test.%')]",
    )
    .fetch_one(system)
    .await
    .unwrap();
    assert_eq!(
        left,
        ["KEPTKEPT", "test.kept", "test.kept"].map(|kept| Some(kept.to_owned()))
    );
}

/// `retention.prune` deletes the preflight cache's expired verdicts and keeps the live ones: an
/// expired verdict is never read again, and without the prune the cache would keep every address
/// ever checked.
#[tokio::test]
async fn retention_prunes_expired_preflight_verdicts() {
    let test = TestDb::new().await;
    let workspace = test.workspace("prune").await;
    sqlx::query(
        "INSERT INTO recipient_validations (workspace_id, email_key, status, reason, checked_at, expires_at)
         VALUES ($1, 'kept@example.test', 'routable', 'mx', now(), now() + interval '1 hour'),
                ($1, 'gone@example.test', 'invalid', 'null_mx', now() - interval '25 hours',
                 now() - interval '1 hour')",
    )
    .bind(workspace.id.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();

    let outcomes = run_system(&test, &harness(&test), &RetentionPrune {}).await;
    assert_eq!(outcomes, ["done"]);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT email_key FROM recipient_validations WHERE workspace_id = $1")
            .bind(workspace.id.uuid())
            .fetch_all(test.system.pool())
            .await
            .unwrap();
    assert_eq!(left, ["kept@example.test"]);
}

/// General metrics count direct messages through the same watermark as campaigns, exactly
/// once across repeated drains; connection and kind filters remain tenant-bound and rates
/// have an explicit denominator rather than returning NaN for an empty selection.
#[tokio::test]
async fn general_metrics_count_all_kinds_and_preserve_the_watermark() {
    let test = TestDb::new().await;
    let workspace = test.workspace("metrics").await;
    let other = test.workspace("metrics-other").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("sender@example.com"))
        .await;
    let message = test
        .direct_message(workspace.id, &sender, &["reader@example.com"], 0)
        .await;
    let mut tx = test.worker.begin_in(workspace.id).await.unwrap();
    crate::delivery::evidence::increment(
        &mut tx,
        workspace.id,
        &[message],
        crate::domain::policy::delivery::Metric::Sent,
    )
    .await
    .unwrap();
    crate::delivery::evidence::increment(
        &mut tx,
        workspace.id,
        &[message],
        crate::domain::policy::delivery::Metric::Delivered,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = test.system.begin().await.unwrap();
    let cutoff = crate::domain::analytics::uuidv7_boundary(
        crate::process::now().plus(Duration::from_secs(1)),
    );
    rollup::drain(&mut tx, cutoff).await.unwrap();
    rollup::drain(&mut tx, cutoff).await.unwrap();
    tx.commit().await.unwrap();
    let app = test.app();
    let read = app
        .get(&format!(
            "/v1/metrics?family=rates&kind=direct&connection_id={}",
            sender.connection
        ))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(read.status, http::StatusCode::OK, "{}", read.json);
    assert_eq!(read.json["totals"]["sent"], 1);
    assert_eq!(read.json["totals"]["delivered"], 1);
    assert_eq!(read.json["rates"]["delivered"], 10000);
    assert!(read.json["computed_at"].is_string());
    let empty = app
        .get("/v1/metrics?family=rates")
        .bearer(&other.key)
        .send()
        .await;
    assert_eq!(empty.json["totals"]["sent"], 0);
    assert!(empty.json["rates"]["delivered"].is_null());
    let campaign = app
        .get("/v1/metrics?kind=campaign")
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(campaign.json["totals"]["sent"], 0);
    let usage = app
        .get("/v1/metrics?family=usage&kind=direct")
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(usage.status, http::StatusCode::UNPROCESSABLE_ENTITY);
}
