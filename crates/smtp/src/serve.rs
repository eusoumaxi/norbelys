//! `norbelys-smtp serve`: the unprivileged, long-running process on the mail host. It runs, as
//! tasks of one runtime:
//!
//! - the control API and the health routes ([`crate::control`], [`crate::health`]);
//! - the admission sampler and the Postfix policy listener ([`crate::admission`]);
//! - the tail of `mail.log` ([`crate::tail`]), when a log is configured;
//! - the LMTP listener for notifications returned to VERP paths ([`crate::bounce`]) and for
//!   feedback-loop reports to the feedback address ([`crate::feedback`]);
//! - the evidence outbox ([`crate::events`]);
//! - confirmed transfer from the bounded local queue to Turso ([`crate::archive`]);
//! - maintenance, every minute: correlation rows older than seven days are removed from
//!   Turso and the remote outbox, provisioning backlog and local queue size are measured.
//!
//! One `serve` per state directory: an exclusive lock on `serve.lock` refuses a second one,
//! since the tail and the outbox assume a single reader and sender. `SIGTERM` or Ctrl-C stops
//! every task: the control API finishes its requests, the outbox its requests in flight. Any
//! task that ends on its own, or fails, stops the others and the process exits with an error,
//! so the supervisor restarts it whole.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use hickory_resolver::TokioResolver;
use opentelemetry::metrics::Gauge;
use secrecy::ExposeSecret as _;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::{self, ServeArgs};
use crate::control::{self, Settings, is_domain};
use crate::crypto::Keys;
use crate::db::{self, Db};
use crate::telemetry;
use crate::{admission, archive, bounce, events, feedback, queue::Queue, tail};

/// How often maintenance runs.
const MAINTENANCE: Duration = Duration::from_secs(60);

/// A stop flag the tasks watch: `true` once the process must stop.
#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl From<watch::Receiver<bool>> for Shutdown {
    /// The flag a `watch` channel sets: `true` once the process must stop.
    fn from(receiver: watch::Receiver<bool>) -> Self {
        Self(receiver)
    }
}

impl Shutdown {
    /// True once the process must stop.
    #[must_use]
    pub fn requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves when the process must stop.
    pub async fn wait(&mut self) {
        let _ = self.0.wait_for(|stopping| *stopping).await;
    }

    /// Sleeps for `duration`; `false` when the process must stop first.
    pub async fn sleep(&mut self, duration: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(duration) => !self.requested(),
            () = self.wait() => false,
        }
    }
}

/// Runs `task` and returns its result under `name`, so the first task to end can be named.
async fn named(
    name: &'static str,
    task: impl Future<Output = anyhow::Result<()>>,
) -> (&'static str, anyhow::Result<()>) {
    (name, task.await)
}

/// Resolves on Ctrl-C or `SIGTERM`.
async fn signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(error = %error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutdown requested");
}

/// Checks the settings that clap cannot.
pub fn validate(args: &ServeArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        config::is_label(&args.common.node),
        "NORBELYS_SMTP_NODE must be a lowercase DNS label"
    );
    anyhow::ensure!(
        config::is_label(&args.dkim_selector),
        "NORBELYS_SMTP_DKIM_SELECTOR must be a lowercase DNS label"
    );
    anyhow::ensure!(
        args.spf_include
            .as_ref()
            .is_none_or(|name| is_domain(&name.replace('_', "a"))),
        "NORBELYS_SMTP_SPF_INCLUDE must be a lowercase fully qualified hostname"
    );
    anyhow::ensure!(
        is_domain(&args.mail_host),
        "NORBELYS_SMTP_MAIL_HOST must be a lowercase fully qualified host name"
    );
    anyhow::ensure!(
        args.fbl_reporters
            .iter()
            .filter(|reporter| !reporter.is_empty())
            .all(|reporter| is_domain(&reporter.to_ascii_lowercase())),
        "NORBELYS_SMTP_FBL_REPORTERS must be fully qualified domain names"
    );
    admission::validate(&args.admission)
}

/// Acquires the directory's exclusive service lock before any queue handle is opened.
/// Refuses a second owner; the caller holds the returned file until every task has stopped.
pub fn lock(state_dir: &Path) -> anyhow::Result<File> {
    db::create_private_dir(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    let lock = File::create(state_dir.join("serve.lock"))?;
    lock.try_lock()
        .map_err(|_| anyhow::anyhow!("another serve process holds {}", state_dir.display()))?;
    Ok(lock)
}

/// Runs `serve` until shutdown or the first task failure.
///
/// # Errors
///
/// Invalid settings, a second `serve` on the state directory, an unusable database, a listener
/// that cannot bind, or a task that failed or stopped on its own.
pub async fn run(
    args: Box<ServeArgs>,
    database: Db,
    queue: Queue,
    lock: File,
) -> anyhow::Result<()> {
    let state_dir = &args.common.state_dir;
    // From here the service runs: its heartbeat tells the backend so, every collection.
    telemetry::heartbeat();
    let keys =
        Arc::new(Keys::from_secret(args.secret.expose_secret()).context("NORBELYS_SMTP_SECRET")?);
    let open_db = || {
        database
            .fork(None)
            .context("cannot create a remote database stream")
    };
    let control_db = open_db()?;

    let (stop, receiver) = watch::channel(false);
    let shutdown = Shutdown::from(receiver);
    let signal_stop = stop.clone();
    tokio::spawn(async move {
        signal().await;
        let _ = signal_stop.send(true);
    });

    let state = control::State {
        db: control_db,
        keys: Arc::clone(&keys),
        settings: Arc::new(Settings {
            mail_host: args.mail_host.clone(),
            public_ipv4: args.public_ipv4,
            spf_include: args.spf_include.clone(),
            dkim_selector: args.dkim_selector.clone(),
            evidence_hosts: args
                .evidence_hosts
                .iter()
                .filter(|h| !h.is_empty())
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            trigger: args.common.trigger(),
        }),
        seen: Arc::new(Mutex::new(HashMap::new())),
        resolver: resolver()?,
    };
    let intake = feedback::Intake::new(
        resolver()?,
        args.fbl_reporters
            .iter()
            .filter(|reporter| !reporter.is_empty())
            .map(|reporter| reporter.to_ascii_lowercase())
            .collect(),
    );
    let open = Arc::new(AtomicBool::new(false));
    let spool = args.spool_dir.clone().unwrap_or_else(|| state_dir.clone());

    let mut tasks: JoinSet<(&'static str, anyhow::Result<()>)> = JoinSet::new();
    tasks.spawn(named(
        "control",
        control::serve(
            args.listen,
            args.control_private_transport,
            state,
            shutdown.clone(),
        ),
    ));
    tasks.spawn(named(
        "policy",
        admission::serve(args.policy_listen, Arc::clone(&open), shutdown.clone()),
    ));
    tasks.spawn(named(
        "admission",
        admission::sample(
            open_db()?,
            queue.clone(),
            spool,
            args.admission,
            open,
            shutdown.clone(),
        ),
    ));
    tasks.spawn(named(
        "bounce",
        bounce::serve(
            args.lmtp_listen,
            database.fork(Some(queue.clone()))?,
            args.common.node.clone(),
            args.mail_host.clone(),
            intake,
            shutdown.clone(),
        ),
    ));
    match &args.mail_log {
        Some(log) => {
            let reader = tail::Tail::new(
                args.common.node.clone(),
                log.clone(),
                database.connection(Some(queue.clone()))?,
                &args.mail_host,
            )?;
            tasks.spawn(named("tail", tail::run(reader, shutdown.clone())));
        }
        None => tracing::warn!("NORBELYS_SMTP_MAIL_LOG is unset: no evidence is collected"),
    }
    tasks.spawn(named(
        "events",
        events::run(open_db()?, keys, shutdown.clone()),
    ));
    tasks.spawn(named(
        "archive",
        archive::run(open_db()?, queue.clone(), shutdown.clone()),
    ));
    tasks.spawn(named(
        "maintenance",
        maintenance(open_db()?, queue, shutdown.clone()),
    ));

    let mut failure = None;
    while let Some(joined) = tasks.join_next().await {
        let (name, outcome) = joined.unwrap_or_else(|error| ("task", Err(error.into())));
        match outcome {
            Err(error) => {
                tracing::error!(task = name, error = %error, "task failed");
                failure.get_or_insert(error);
            }
            Ok(()) if !shutdown.requested() => {
                tracing::error!(task = name, "task stopped on its own");
                failure
                    .get_or_insert_with(|| anyhow::anyhow!("the {name} task stopped on its own"));
            }
            Ok(()) => {}
        }
        let _ = stop.send(true);
    }
    drop(lock);
    failure.map_or(Ok(()), Err)
}

/// The resolver of ownership checks: the host's configuration, three seconds per query, two
/// attempts.
fn resolver() -> anyhow::Result<TokioResolver> {
    let mut builder =
        TokioResolver::builder_tokio().context("cannot read the host's DNS configuration")?;
    builder.options_mut().timeout = Duration::from_secs(3);
    builder.options_mut().attempts = 2;
    Ok(builder.build()?)
}

struct Gauges {
    pending: Gauge<u64>,
    dead: Gauge<u64>,
    oldest: Gauge<f64>,
    changes_pending: Gauge<u64>,
    changes_refused: Gauge<u64>,
    queue_bytes: Gauge<u64>,
}

/// Remote correlation retention and backlog gauges; canonical history stays in Turso.
async fn maintenance(db: Db, queue: Queue, mut shutdown: Shutdown) -> anyhow::Result<()> {
    let meter = telemetry::meter();
    let gauges = Gauges {
        queue_bytes: meter
            .u64_gauge("norbelys_mta_queue_bytes")
            .with_unit("By")
            .with_description("Local pending queue and WAL bytes")
            .build(),
        pending: meter
            .u64_gauge("norbelys_mta_events_pending")
            .with_description("Events not yet delivered")
            .build(),
        dead: meter
            .u64_gauge("norbelys_mta_events_dead")
            .with_description("Dead letters awaiting review")
            .build(),
        oldest: meter
            .f64_gauge("norbelys_mta_events_oldest_pending_seconds")
            .with_unit("s")
            .with_description("Age of the oldest event not yet delivered")
            .build(),
        changes_pending: meter
            .u64_gauge("norbelys_mta_changes_pending")
            .with_description("Provisioning changes waiting for provision-apply")
            .build(),
        changes_refused: meter
            .u64_gauge("norbelys_mta_changes_refused")
            .with_description("Provisioning changes refused in the last seven days")
            .build(),
    };
    loop {
        let capture = queue.clone();
        let bytes = tokio::task::spawn_blocking(move || capture.bytes()).await??;
        gauges.queue_bytes.record(bytes, &[]);
        let result = db
            .call(move |conn| {
                let submissions = tail::prune(conn)?;
                let backlog = events::backlog(conn)?;
                let (pending, refused): (i64, i64) = conn.query_row(
                    "SELECT count(*) FILTER (WHERE applied IS NULL),
                            count(*) FILTER (WHERE applied > ?1 AND json_extract(payload, '$.error') IS NOT NULL)
                       FROM pending_changes",
                    [db::now() - 7.0 * 86_400.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok::<_, anyhow::Error>((submissions, backlog, pending, refused))
            })
            .await;
        match result {
            Ok((submissions, backlog, pending, refused)) => {
                gauges.pending.record(backlog.pending, &[]);
                gauges.dead.record(backlog.dead, &[]);
                gauges.oldest.record(backlog.oldest_seconds, &[]);
                gauges
                    .changes_pending
                    .record(u64::try_from(pending).unwrap_or(0), &[]);
                gauges
                    .changes_refused
                    .record(u64::try_from(refused).unwrap_or(0), &[]);
                if submissions > 0 {
                    telemetry::unit(telemetry::Event::Retention);
                    tracing::info!(event = "mta.retention", submissions, "mta.retention");
                }
            }
            Err(error) => tracing::error!(error = %error, "maintenance failed"),
        }
        if !shutdown.sleep(MAINTENANCE).await {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second service is refused before opening a second handle on pending evidence;
    /// releasing the owner permits a clean restart on the same directory.
    #[test]
    fn locks_the_pending_directory_before_starting_another_service() {
        let dir = crate::testing::TempDir::new();
        let owner = lock(dir.path()).unwrap();
        assert!(lock(dir.path()).is_err());
        drop(owner);
        assert!(lock(dir.path()).is_ok());
    }

    /// A loop's sleep ends early, reporting `false`, when the process is asked to stop, so
    /// every task exits within moments of `SIGTERM`; otherwise it sleeps its full time.
    #[tokio::test]
    async fn sleeps_until_shutdown() {
        let (stop, receiver) = watch::channel(false);
        let mut shutdown = Shutdown(receiver);
        assert!(shutdown.sleep(Duration::from_millis(5)).await);
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop.send(true).unwrap();
        });
        assert!(!shutdown.sleep(Duration::from_secs(60)).await);
        assert!(shutdown.requested());
        stopper.await.unwrap();
    }
}
