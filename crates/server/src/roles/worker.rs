//! The worker role: the job runner's lanes, the outbox relay and the maintenance lane.
//!
//! This module is the one place job kinds are registered: a kind enqueued by another role runs
//! only if a worker registers it here (a worker that meets an unregistered kind fails the job
//! with `unknown_kind` rather than guessing).
//!
//! The worker holds two pools. Its own login, `norbelys_worker`, runs under row security: it
//! claims and recovers jobs through the scheduler role's global view of lanes and leases, and
//! runs tenant and fan-out kinds inside one workspace at a time. The `norbelys_system` login
//! (`SYSTEM_DATABASE_URL`, two connections) bypasses row security: it serves the system kinds
//! of the maintenance lane, which need every workspace's rows by period (partitions today), and
//! seeds the periodic schedules at start. A third, dedicated connection of the worker's login
//! holds `LISTEN` for wake-ups.
//!
//! The runner and the health listener run until the process is asked to stop; the runner then
//! stops claiming and lets running jobs checkpoint and yield within its grace.

use std::time::Duration;

use anyhow::Context as _;

use crate::config::WorkerArgs;
use crate::db::Database;
use crate::delivery::limits::{Limits, Role, Shares, Split};
use crate::jobs::{self, Registry, Runner};
use crate::process::{self, Shutdown};
use crate::{senders, webhooks};

/// Every kind this worker runs: the one place a kind is registered.
///
/// # Errors
///
/// A kind is registered twice, or its declaration is inconsistent.
fn registry() -> Result<Registry, jobs::kinds::RegistryError> {
    let mut registry = Registry::default();
    registry
        .register::<jobs::kinds::PartitionsCreate>()?
        .register::<crate::analytics::rollup::AnalyticsRollup>()?
        .register::<crate::analytics::rollup::AnalyticsRecount>()?
        .register::<crate::analytics::archive::ArchiveExport>()?
        .register::<crate::analytics::retention::RetentionPrune>()?
        .register::<crate::analytics::deletion::WorkspaceDelete>()?
        .register::<webhooks::outbox::Relay>()?
        .register::<webhooks::deliver::Deliver>()?
        .register::<webhooks::deliver::FailureEmail>()?
        .register::<webhooks::normalize::Normalize>()?
        .register::<senders::check::ConnectionCheck>()?
        .register::<senders::check::ConnectionCheckDue>()?
        .register::<senders::domains::DomainPrepare>()?
        .register::<senders::domains::DomainVerify>()?
        .register::<senders::domains::DomainVerifyDue>()?
        .register::<senders::provision::NorbelysProvision>()?
        .register::<senders::health::HealthEmail>()?
        .register::<crate::people::imports::PeopleImport>()?
        .register::<crate::people::cleanup::PeopleCleanup>()?
        .register::<crate::people::exports::ExportRun>()?
        .register::<crate::identity::sso::SsoCheckDue>()?
        .register::<crate::campaigns::materialise::CampaignMaterialise>()?
        .register::<crate::campaigns::enrollments::EnrollmentAdd>()?
        .register::<crate::campaigns::advance::EnrollmentAdvance>()?
        .register::<crate::campaigns::removal::SendersRemoved>()?
        .register::<crate::campaigns::generate::MessageGenerate>()?
        .register::<crate::identity::sso::SsoRefreshMetadata>()?
        .register::<crate::identity::sessions::RevokeUser>()?
        .register::<crate::delivery::expire::DeliveryExpire>()?
        .register::<senders::warmup::ConnectionsWarmup>()?
        .register::<crate::inbox::classify::Classify>()?
        .register::<crate::delivery::evidence::ComplaintRate>()?
        .register::<webhooks::reconcile::ProviderReconcileDue>()?
        .register::<webhooks::reconcile::ProviderReconcile>()?;
    Ok(registry)
}

pub async fn run(args: WorkerArgs) -> anyhow::Result<()> {
    let db = super::connect(
        &args.common,
        super::background_pool("norbelys-worker", args.pool_size, Duration::from_secs(60)),
    )
    .await?;
    let system_url = args
        .system_database_url
        .as_ref()
        .context("SYSTEM_DATABASE_URL is required by the worker")?;
    let system = Database::connect(
        system_url,
        super::background_pool("norbelys-worker-system", 2, Duration::from_secs(60)),
    )
    .await
    .context("cannot connect the worker's system pool to the database")?;
    let listen_url = args
        .common
        .database_url
        .clone()
        .context("DATABASE_URL is required by this role")?;

    let registry = registry()?;
    let schedules = jobs::schedules::seed(&system, &registry).await?;

    let mut env = http::Extensions::new();
    env.insert(super::role_keys(
        &args.common,
        crate::config::KeyRole::Worker,
    )?);
    env.insert(crate::storage::Storage::from_args(
        &args.storage,
        &args.common.environment,
    )?);
    // The process's one DNS resolver, shared by the kinds that resolve through the environment.
    let resolver = crate::dns::Resolver::system()?;
    // The daily check of SSO connections reads DNS through the same resolver and identity
    // providers' documents through the bounded fetcher.
    env.insert(resolver.clone());
    env.insert(crate::identity::fetch::Fetcher::new(
        args.identity_allow_private_issuers,
    )?);
    // Connection checks charge their Gmail reads to maintenance's part of the provider limits,
    // divided by the worker replicas; a split or a replica count that cannot hold refuses to start.
    let limits = Limits::new(
        Shares::new(Role::Maintenance, Split::from(args.shares), args.replicas)
            .context("the worker's share of the provider rate limits is refused")?,
    );
    env.insert(senders::Env::from_args(&args.mail, resolver, limits)?);
    if args.mail.mail_allow_private_hosts {
        tracing::warn!(
            "mailbox and relay hosts may be private addresses and plaintext: a development setting"
        );
    }
    // The deployment's AI providers, for the kinds of the `ai` queue; a use case configured with a
    // model that is not priced or does not enforce a schema stops the worker here.
    env.insert(crate::ai::Ai::from_args(&args.ai)?);
    env.insert(webhooks::deliver::Sender::new(
        args.webhook_allow_private_targets,
    )?);
    if args.webhook_allow_private_targets {
        tracing::warn!(
            "webhooks may target private addresses and plain http: a development setting"
        );
    }

    let shutdown = Shutdown::watch_signal();
    // The grid's planned sends for the next hour, as a gauge every minute.
    process::spawn_exporter(
        "delivery.projection",
        crate::delivery::projection::export(db.clone(), shutdown.clone()),
        shutdown.clone(),
    );
    // The deployment's backlog, uncertain, budget and inbox gauges, read through the system
    // login every minute.
    process::spawn_exporter(
        "telemetry.fleet",
        crate::telemetry::fleet::export(system.clone(), shutdown.clone()),
        shutdown.clone(),
    );
    // PostgreSQL's own statistics, every 15 seconds on one connection of the metrics login.
    match &args.metrics_database_url {
        Some(url) => {
            let metrics = Database::connect_lazy(
                url,
                crate::db::PoolSettings {
                    min_connections: 0,
                    ..super::background_pool("norbelys-metrics", 1, Duration::from_secs(5))
                },
            )
            .context("METRICS_DATABASE_URL is not a database URL")?;
            process::spawn_exporter(
                "telemetry.postgres",
                crate::telemetry::postgres::scrape(metrics, shutdown.clone()),
                shutdown.clone(),
            );
        }
        None => {
            tracing::warn!("METRICS_DATABASE_URL is not set: PostgreSQL's statistics are not read")
        }
    }
    // The review queue's arrivals and time to review, as gauges every minute.
    process::spawn_exporter(
        "ai.review",
        crate::ai::review::export(db.clone(), shutdown.clone()),
        shutdown.clone(),
    );
    let runner = Runner {
        db: db.clone(),
        system,
        listen_url,
        registry,
        env,
        permits: args.job_permits,
        shutdown: shutdown.clone(),
    };
    tracing::info!(schedules, "worker starting");
    process::supervise(
        "job.runner",
        async { runner.run().await.map_err(Into::into) },
        "health",
        process::serve_health(args.common.health_addr, db, shutdown.clone()),
        shutdown,
    )
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::registry;
    use crate::jobs::runner::Harness;
    use crate::jobs::{NewJob, Queue, SYSTEM_WORKSPACE, enqueue_value};
    use crate::telemetry::Event;
    use crate::telemetry::capture::Capture;
    use crate::telemetry::mirror;
    use crate::testing::TestDb;

    /// Walks the worker's registry: one job of every registered kind, each with a payload no
    /// release reads (version 0), is claimed and run through the runner, and each emits exactly
    /// one `job.run` event naming its kind, counted on both sides of the reconciliation. A kind
    /// run anywhere but through the runner, or registered where the runner does not look, would
    /// be a kind whose runs nobody sees.
    #[tokio::test]
    async fn every_registered_kind_emits_its_run_event_once() {
        let test = TestDb::new().await;
        let registry = registry().unwrap();
        let kinds = registry.kinds();
        let mut tx = test.system.begin().await.unwrap();
        for &(kind, queue) in &kinds {
            let job = NewJob {
                kind,
                queue,
                max_attempts: 1,
                payload: json!({ "version": 0 }),
                unique_key: None,
            };
            enqueue_value(&mut tx, SYSTEM_WORKSPACE, &job, None)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let harness = Harness::new(
            test.worker.clone(),
            test.system.clone(),
            registry,
            http::Extensions::new(),
            "coverage",
        );

        let capture = Capture::default();
        let _guard = capture.install();
        for queue in Queue::iter() {
            while !harness.run_once(queue, 8).await.is_empty() {}
        }
        let runs = capture.named_since(0, Event::JobRun.as_str());
        for &(kind, _) in &kinds {
            let emitted = runs.iter().filter(|run| run.field("kind") == kind).count();
            assert_eq!(emitted, 1, "{kind} emitted {emitted} run events");
        }
        assert_eq!(runs.len(), kinds.len());
        let (seen, units) = mirror::counts();
        assert_eq!(seen, units, "every run event is counted on both sides");
    }
}
