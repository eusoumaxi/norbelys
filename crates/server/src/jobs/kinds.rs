//! The registry of job kinds, and the runner's own maintenance kind.
//!
//! The worker role registers every kind it runs once, at start (`roles/worker.rs`). The
//! registry erases each kind's type behind a function that decodes the payload and runs the
//! job, and keeps what the runner needs to know without the type: the queue, the effect class,
//! the execution class, the payload version, the attempt limit and, for periodic kinds, the
//! schedule. Registration refuses what could never run correctly: a duplicate name, a system
//! kind off the maintenance queue, a periodic kind that is not a singleton of the `system`
//! workspace, or a cron expression that does not parse.
//!
//! A job whose kind is not registered in this worker (a newer release enqueued it, or a kind
//! was retired) is not guessed at: the runner ends it `failed` with `unknown_kind`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use sqlx::AssertSqlSafe;

use super::schedules::{Cron, CronError};
use super::{Class, Effect, Job, JobContext, JobError, Outcome, Queue, RecoveryHook};

/// The decoded-and-run future of one claimed job; it hands the context back for the conclusion.
pub(super) type Running =
    Pin<Box<dyn Future<Output = (Result<Outcome, JobError>, JobContext)> + Send>>;

/// What the runner knows about a registered kind.
pub(super) struct Kind {
    pub queue: Queue,
    pub effect: Effect,
    pub class: Class,
    pub version: u16,
    pub max_attempts: u16,
    pub run: fn(serde_json::Value, JobContext) -> Running,
    pub schedule: Option<Scheduled>,
    /// What settles a claim's open reservations when the claim ends (see
    /// [`Job::RECOVERY_HOOK`]).
    pub recovery_hook: Option<RecoveryHook>,
}

/// A periodic kind's schedule, checked at registration.
pub(super) struct Scheduled {
    pub expression: &'static str,
    pub cron: Cron,
    pub payload: serde_json::Value,
    pub unique_key: Option<String>,
}

/// Why a kind could not be registered.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("the kind `{0}` is registered twice")]
    Duplicate(&'static str),
    #[error(
        "the system kind `{0}` must run on the maintenance queue, where the system pool serves it"
    )]
    SystemOffMaintenance(&'static str),
    #[error("the periodic kind `{0}` must be a fan-out or system kind with a unique key")]
    PeriodicNotSingleton(&'static str),
    #[error("the periodic kind `{kind}` has an invalid schedule: {error}")]
    Schedule {
        kind: &'static str,
        error: CronError,
    },
    #[error("the periodic kind `{kind}` has a payload that does not serialize: {error}")]
    Payload {
        kind: &'static str,
        error: sqlx::Error,
    },
}

/// Every kind this worker runs.
#[derive(Default)]
pub struct Registry {
    kinds: HashMap<&'static str, Kind>,
}

impl Registry {
    /// Registers `J`.
    ///
    /// # Errors
    ///
    /// The kind is registered already, or its declaration is inconsistent (see the module).
    pub fn register<J: Job>(&mut self) -> Result<&mut Self, RegistryError> {
        if self.kinds.contains_key(J::KIND) {
            return Err(RegistryError::Duplicate(J::KIND));
        }
        if J::CLASS == Class::System && J::QUEUE != Queue::Maintenance {
            return Err(RegistryError::SystemOffMaintenance(J::KIND));
        }
        let schedule = match J::schedule() {
            None => None,
            Some((expression, job)) => {
                let unique_key = job.unique_key();
                if J::CLASS == Class::Tenant || unique_key.is_none() {
                    return Err(RegistryError::PeriodicNotSingleton(J::KIND));
                }
                Some(Scheduled {
                    expression,
                    cron: Cron::parse(expression).map_err(|error| RegistryError::Schedule {
                        kind: J::KIND,
                        error,
                    })?,
                    payload: super::payload_of(&job, J::VERSION).map_err(|error| {
                        RegistryError::Payload {
                            kind: J::KIND,
                            error,
                        }
                    })?,
                    unique_key,
                })
            }
        };
        self.kinds.insert(
            J::KIND,
            Kind {
                queue: J::QUEUE,
                effect: J::EFFECT,
                class: J::CLASS,
                version: J::VERSION,
                max_attempts: J::MAX_ATTEMPTS,
                run: run::<J>,
                schedule,
                recovery_hook: J::RECOVERY_HOOK,
            },
        );
        Ok(self)
    }

    /// The kind registered under `name`.
    pub(super) fn get(&self, name: &str) -> Option<&Kind> {
        self.kinds.get(name)
    }

    /// Every registered kind's name and queue, by name: what the coverage test walks.
    #[cfg(test)]
    pub(crate) fn kinds(&self) -> Vec<(&'static str, Queue)> {
        let mut kinds: Vec<(&'static str, Queue)> = self
            .kinds
            .iter()
            .map(|(name, kind)| (*name, kind.queue))
            .collect();
        kinds.sort_unstable();
        kinds
    }

    /// The queues of the registered kinds, each once, in a stable order.
    pub(super) fn queues(&self) -> Vec<Queue> {
        let mut queues: Vec<Queue> = self.kinds.values().map(|kind| kind.queue).collect();
        queues.sort_unstable();
        queues.dedup();
        queues
    }

    /// The periodic kinds and their schedules, sorted by name: every worker writes their
    /// `job_schedules` rows in the same order, so two workers seeding at once never lock the
    /// rows in opposite orders (a deadlock).
    pub(super) fn scheduled(&self) -> impl Iterator<Item = (&'static str, &Scheduled)> {
        let mut scheduled: Vec<(&'static str, &Scheduled)> = self
            .kinds
            .iter()
            .filter_map(|(name, kind)| kind.schedule.as_ref().map(|scheduled| (*name, scheduled)))
            .collect();
        scheduled.sort_unstable_by_key(|(name, _)| *name);
        scheduled.into_iter()
    }
}

/// Decodes the payload as `J` and runs it.
fn run<J: Job>(payload: serde_json::Value, mut cx: JobContext) -> Running {
    Box::pin(async move {
        let result = match serde_json::from_value::<J>(payload) {
            Ok(job) => job.run(&mut cx).await,
            Err(error) => Err(JobError::InvalidPayload(error.to_string())),
        };
        (result, cx)
    })
}

/// `partitions.create`: keeps the time-partitioned tables two days ahead, and their parents'
/// statistics fresh.
///
/// Several fact tables (messages, attempts, events, the outbox and webhook deliveries, receipts,
/// increments, tracking) are range-partitioned by period, and none has a default partition: a
/// row for a period without its partition is refused rather than piling up in a catch-all. So
/// every day, shortly after midnight UTC, this job runs `ensure_partitions_ahead('2 days')` at
/// the current instant, which creates each missing leaf up to two days ahead, and always the
/// next period's (next month's for a monthly table), with its grants and row security, and
/// returns the leaves it ensured. The function runs with its owner's rights,
/// granted to `norbelys_system` only, so this is a system kind on the maintenance lane.
/// Ensuring a leaf that exists is a no-op, so a replayed run is harmless.
///
/// Then, in a chunk of its own (the new leaves are committed first), it analyzes every
/// partitioned parent the policies name, the parent only (`ANALYZE ONLY`). Autovacuum analyzes
/// the leaves as they change but never a partitioned parent, so without this the statistics the
/// planner uses for a query through a parent (a join, a range over several periods) would never
/// be refreshed. Sampling the parent reads a bounded sample across its leaves, whatever their
/// size. `ANALYZE` needs the table's owner (or its maintain privilege), so the chunk switches to
/// the owner for those statements only, with `SET LOCAL ROLE`, and back before its checkpoint,
/// whose write the owner's view of the jobs table would not see. Analyzing again is harmless.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartitionsCreate {}

impl Job for PartitionsCreate {
    const KIND: &'static str = "partitions.create";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::System;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("5 0 * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut chunk = cx.begin().await?;
        // Two days ahead: the expected count of leaves (`domain::telemetry::expected_leaves`,
        // the `archive-debt` alert) assumes exactly this.
        let leaves =
            sqlx::query_scalar!(r#"SELECT ensure_partitions_ahead(interval '2 days') AS "leaf!""#)
                .fetch_all(&mut **chunk.tx())
                .await?;
        cx.checkpoint(chunk, serde_json::json!({ "leaves": leaves.len() }))
            .await?;

        let mut chunk = cx.begin().await?;
        let statements = sqlx::query_scalar!(
            r#"SELECT format('ANALYZE ONLY %I', table_name) AS "statement!"
                 FROM partition_policies ORDER BY table_name"#
        )
        .fetch_all(&mut **chunk.tx())
        .await?;
        crate::db::as_owner(chunk.tx()).await?;
        for statement in &statements {
            // Built by `format('%I')` from the policies' table names, so each is quoted.
            sqlx::raw_sql(AssertSqlSafe(statement.as_str()))
                .execute(&mut **chunk.tx())
                .await?;
        }
        crate::db::reset_role(chunk.tx()).await?;
        cx.checkpoint(
            chunk,
            serde_json::json!({ "leaves": leaves.len(), "analyzed": statements.len() }),
        )
        .await?;
        Ok(Outcome::Done)
    }
}
