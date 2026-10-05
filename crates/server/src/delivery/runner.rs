//! The delivery runner: claims due connections, takes each claimed message through preflight,
//! rendering, a session, its Start and its submission, and records the outcomes in micro-batches.
//!
//! # The loop
//!
//! One claim loop per process takes the turn of the workspace whose turn is oldest
//! ([`claim::turn`]), reads a page of its due connections ([`claim::page`]) and claims each one
//! ([`claim::claim`]) while a submission slot is free; a claimed wave runs in its own task. Each
//! claim charges its messages to this replica's provider limiters ([`Limits`]). The loop sleeps
//! when no workspace has due work, until a wake-up (`NOTIFY norbelys_work 'delivery'`, sent when
//! mail is created) or two seconds pass, which is short against the five-minute grid every cold
//! send is placed on.
//!
//! # One message
//!
//! Inside a wave, each message holds one submission slot from its preparation to its report:
//!
//! 1. **Render** ([`rendering::prepare`]): the envelope and the MIME bytes. Content that can never
//!    render fails the message; a database error returns it to the queue.
//! 2. **Preflight** ([`preflight::routes`] and `domain::policy::delivery::after_preflight`):
//!    every envelope address's syntax and mail route, from the workspace's preflight cache or
//!    DNS, never a probe of a mailbox. No address reachable fails the message with the findings
//!    as evidence; a DNS outage returns it to the queue for five minutes, and so does a database
//!    error on the cache. A workspace in test mode checks the syntax only: nothing is delivered,
//!    and test addresses often use domains that accept no mail (`example.com`).
//! 3. **A session** ([`Transports::acquire`]): an authenticated SMTP session or a fresh access
//!    token, still under the claim's lease. A workspace in test mode gets the fake transport and
//!    never reaches a provider.
//! 4. **The Start** ([`start::start`]), then the **submission** before the deadline it fixed.
//! 5. **The report**, flushed to [`finish::finish`] with the wave's others every two seconds or
//!    fifty reports, so an accepted message is never held back by the slowest of its wave.
//!
//! A message the Start returns or ends is not reported: the Start already wrote its state. A
//! report the database refuses is lost with its batch, and the leases expire into recovery,
//! which this process also runs every fifteen seconds ([`recover::sweep`]).
//!
//! # Shutdown
//!
//! On `SIGTERM` the claim loop stops claiming; a message not yet started goes back to the queue
//! unstarted; a submission under way finishes within the grace, and one that outlives it is
//! recovered from its expired lease (`uncertain` when its Start's marker was written, as a
//! submission whose answer is unknown must be).
//!
//! # Telemetry
//!
//! A wave runs in a `delivery.wave` span, each of its messages in a `delivery.message` child that
//! the configured trace sampler keeps with its wave, and a wave ends with one
//! `delivery.wave` event: claimed, submitted, accepted, transient, permanent and uncertain
//! counts, the provider's latency (median, 95th percentile, longest) and the most sessions held
//! at once. No message has an `info` line of its own (a `debug` one only). Every submission to a
//! provider counts in `norbelys_delivery_submissions_total{provider, outcome}` and
//! `norbelys_delivery_provider_latency_seconds{provider}`, every held session in
//! `norbelys_delivery_sessions_in_use{provider}`, and a cold Start's delay after its scheduled
//! instant in `norbelys_delivery_cold_start_lag_seconds{provider}`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};
use sqlx::postgres::PgListener;
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::crypto::Keys;
use crate::db::Database;
use crate::delivery::claim::{self, Claim, Claimed, Cursors, Scan, Wave};
use crate::delivery::evidence::Evidence;
use crate::delivery::finish::{self, Report, Reported};
use crate::delivery::limits::Limits;
use crate::delivery::submit::{self, Lane, Target, Transports};
use crate::delivery::{preflight, recover, start};
use crate::dns::Resolver;
use crate::domain::email::EmailAddress;
use crate::domain::policy::delivery::{
    self as policy, Category, Confidence, EventKind, Preflight, RecipientRef, Source,
};
use crate::domain::preflight::Reason;
use crate::domain::senders::Provider;
use crate::process::{self, Shutdown};
use crate::rendering;

/// How long acquiring a session may take, inside the claim's two-minute lease.
const ACQUIRE_BUDGET: Duration = Duration::from_secs(60);
/// How long a message waits after DNS could not answer for one of its addresses.
const DNS_RETRY: Duration = Duration::from_secs(300);
/// A wave's reports are flushed after this long, or at [`FLUSH_REPORTS`].
const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// A wave's reports are flushed once this many are waiting.
const FLUSH_REPORTS: usize = 50;
/// The idle wait between turns when no workspace has due work.
const IDLE: Duration = Duration::from_secs(2);
/// The pause after a page whose claims took nothing (limiters refused, rows held elsewhere).
const BACKOFF: Duration = Duration::from_millis(200);
/// How often expired leases are recovered.
const RECOVERY_EVERY: Duration = Duration::from_secs(15);
/// How long submissions under way may finish after a shutdown request: a submission waits up to
/// 120 seconds for the reply to its content, and one cut short becomes `uncertain`, so the stop
/// outlasts it (the container's stop grace is 150 seconds).
const GRACE: Duration = Duration::from_secs(140);

static SUBMISSIONS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_delivery_submissions_total")
        .with_description(
            "Submissions to providers, by provider and outcome (accepted, transient, permanent, \
             uncertain); a workspace in test mode reaches no provider and is not counted.",
        )
        .build()
});

static PROVIDER_LATENCY: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_delivery_provider_latency_seconds")
        .with_unit("s")
        .with_description("How long a provider took to answer a submission, by provider.")
        .with_boundaries(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 120.0])
        .build()
});

static SESSIONS: LazyLock<UpDownCounter<i64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .i64_up_down_counter("norbelys_delivery_sessions_in_use")
        .with_description(
            "Sessions and provider request slots this process holds for submissions now, by \
             provider.",
        )
        .build()
});

static COLD_START_LAG: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_delivery_cold_start_lag_seconds")
        .with_unit("s")
        .with_description(
            "How long after its scheduled instant a cold message's Start ran, by provider.",
        )
        .with_boundaries(vec![
            0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 3_600.0,
        ])
        .build()
});

/// The sessions this process holds for submissions now, read into each wave's event.
static HELD: AtomicI64 = AtomicI64::new(0);

/// A session (or a provider's request slot) held for one message's submission, counted in
/// `norbelys_delivery_sessions_in_use` while it lives.
struct InUse(Provider);

impl InUse {
    fn hold(provider: Provider) -> Self {
        HELD.fetch_add(1, Ordering::Relaxed);
        SESSIONS.add(1, &[KeyValue::new("provider", provider.as_str())]);
        Self(provider)
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::Relaxed);
        SESSIONS.add(-1, &[KeyValue::new("provider", self.0.as_str())]);
    }
}

/// What a wave's submissions came to, for its `delivery.wave` event: one line per wave, never
/// one per message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tally {
    accepted: u32,
    transient: u32,
    permanent: u32,
    uncertain: u32,
    /// Each submission's provider latency, in milliseconds.
    latencies_ms: Vec<u64>,
    /// Message tasks that panicked; their fenced leases are left for recovery.
    task_failures: u64,
    /// The most sessions this process held at once when one of the wave's submissions ended.
    sessions: i64,
}

impl Tally {
    /// Counts one submission that ended with `outcome` (`accepted` or a failure's spelling)
    /// after `latency`, while the process held `sessions`.
    fn record(&mut self, outcome: &str, latency: Duration, sessions: i64) {
        match outcome {
            "accepted" => self.accepted += 1,
            "transient" => self.transient += 1,
            "permanent" => self.permanent += 1,
            "uncertain" => self.uncertain += 1,
            _ => return,
        }
        self.latencies_ms
            .push(u64::try_from(latency.as_millis()).unwrap_or(u64::MAX));
        self.sessions = self.sessions.max(sessions);
    }

    /// The submissions counted.
    fn submitted(&self) -> u32 {
        self.accepted + self.transient + self.permanent + self.uncertain
    }

    /// The `percentile` (1–100) of the latencies by nearest rank; 0 without a submission.
    fn latency_ms(&self, percentile: usize) -> u64 {
        let mut sorted = self.latencies_ms.clone();
        sorted.sort_unstable();
        let rank = (percentile.min(100) * sorted.len()).div_ceil(100).max(1);
        sorted.get(rank - 1).copied().unwrap_or(0)
    }
}

/// What every task of the role shares.
struct Shared {
    db: Database,
    keys: Keys,
    resolver: Resolver,
    rendering: rendering::Settings,
    transports: Transports,
    limits: Limits,
    owner: String,
    retry_window: Duration,
    slots: Arc<Semaphore>,
    wake: Notify,
    shutdown: Shutdown,
}

/// Initialized dependencies and bounded capacity supplied by the sender process.
pub(crate) struct Settings {
    pub db: Database,
    pub keys: Keys,
    pub resolver: Resolver,
    pub rendering: rendering::Settings,
    pub transports: Transports,
    pub limits: Limits,
    pub retry_window: Duration,
    pub permits: u32,
}

/// A sender's claim, notification and recovery loops. Process supervision observes `wait`;
/// `drain` runs afterwards so the 140-second submission grace is not shortened by the health
/// supervisor's cleanup deadline. Dropping the task set aborts its background loops.
pub(crate) struct Running {
    shared: Arc<Shared>,
    tasks: JoinSet<()>,
    permits: u32,
}

pub(crate) fn start(settings: Settings, listener: PgListener, shutdown: Shutdown) -> Running {
    let permits = settings.permits.max(1);
    let shared = Arc::new(Shared {
        db: settings.db,
        keys: settings.keys,
        resolver: settings.resolver,
        rendering: settings.rendering,
        transports: settings.transports,
        limits: settings.limits,
        owner: format!("sender:{}", Uuid::now_v7().simple()),
        retry_window: settings.retry_window,
        slots: Arc::new(Semaphore::new(usize::try_from(permits).unwrap_or(1))),
        wake: Notify::new(),
        shutdown,
    });
    tracing::info!(owner = %shared.owner, permits, "sender started");
    let mut tasks = JoinSet::new();
    tasks.spawn(listen_loop(Arc::clone(&shared), listener));
    tasks.spawn(recovery_loop(Arc::clone(&shared)));
    tasks.spawn(claim_loop(Arc::clone(&shared)));
    Running {
        shared,
        tasks,
        permits,
    }
}

impl Running {
    /// Any completed critical loop returns control to process supervision; a panic fails it.
    pub(crate) async fn wait(&mut self) -> anyhow::Result<()> {
        if let Some(joined) = self.tasks.join_next().await {
            joined.map_err(|_| anyhow::anyhow!("sender loop panicked or was cancelled"))?;
        }
        Ok(())
    }

    /// Stop taking work and allow submissions already holding permits to settle.
    pub(crate) async fn drain(&mut self) {
        self.shared.shutdown.request();
        self.shared.wake.notify_waiters();
        if tokio::time::timeout(GRACE, self.shared.slots.acquire_many(self.permits))
            .await
            .is_err()
        {
            tracing::warn!(
                "submissions still under way at the end of the grace; their leases will expire into recovery"
            );
        }
        self.tasks.abort_all();
        tracing::info!(owner = %self.shared.owner, "sender stopped");
    }
}

/// Wakes the claim loop on `NOTIFY norbelys_work 'delivery'`.
async fn listen_loop(shared: Arc<Shared>, mut listener: PgListener) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        let received = tokio::select! {
            received = listener.try_recv() => received,
            () = shutdown.wait() => return,
        };
        match received {
            Ok(Some(notification)) if notification.payload() == "delivery" => {
                shared.wake.notify_one();
            }
            Ok(Some(_)) => {}
            // The connection was lost and opened again: a wake-up may have been missed.
            Ok(None) => shared.wake.notify_one(),
            Err(error) => {
                tracing::warn!(error = %error, "the delivery wake-up listener failed; reconnecting");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    () = shutdown.wait() => return,
                }
            }
        }
    }
}

/// Recovers expired leases every [`RECOVERY_EVERY`].
async fn recovery_loop(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        tokio::select! {
            () = tokio::time::sleep(RECOVERY_EVERY) => {}
            () = shutdown.wait() => return,
        }
        // A sweep that recovered anything emits its own `delivery.recover` event.
        if let Err(error) = recover::sweep(&shared.db).await {
            tracing::warn!(error = %error, "the recovery sweep failed");
        }
    }
}

/// Takes turns, pages and claims while slots are free (see the module).
async fn claim_loop(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.clone();
    let mut cursors = Cursors::default();
    while !shutdown.requested() {
        // A claim needs a free slot; wait for one.
        if shared.slots.available_permits() == 0 {
            tokio::select! {
                permit = shared.slots.acquire() => drop(permit),
                () = shutdown.wait() => break,
            }
            continue;
        }
        let workspace = match claim::turn(&shared.db).await {
            Ok(Some(workspace)) => workspace,
            Ok(None) => {
                idle(&shared, &mut shutdown, IDLE).await;
                continue;
            }
            Err(error) => {
                tracing::warn!(error = %error, "the turn could not be taken");
                idle(&shared, &mut shutdown, IDLE).await;
                continue;
            }
        };
        let page = match claim::page(&shared.db, workspace, &mut cursors).await {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(error = %error, "the page could not be read");
                idle(&shared, &mut shutdown, IDLE).await;
                continue;
            }
        };
        let mut took = false;
        for candidate in &page.candidates {
            if shutdown.requested() {
                break;
            }
            let free = u32::try_from(shared.slots.available_permits()).unwrap_or(u32::MAX);
            if free == 0 {
                break;
            }
            let limits = shared.limits.clone();
            let mut charge = move |charge: &claim::Charge<'_>| limits.admit(charge);
            match claim::claim(&shared.db, &shared.owner, candidate, free, &mut charge).await {
                Ok(Claim::Wave(wave)) => {
                    took = true;
                    let span = tracing::info_span!(
                        "delivery.wave",
                        connection_id = %wave.connection,
                        provider = wave.provider.as_str(),
                        otel.status_code = tracing::field::Empty
                    );
                    // Linked to, never continuing, the requests that created its messages.
                    for trace_parent in &wave.trace_parents {
                        crate::telemetry::link(&span, trace_parent);
                    }
                    let running = Arc::clone(&shared);
                    tokio::spawn(run_wave(running, wave).instrument(span));
                }
                Ok(Claim::Nothing | Claim::Spent { .. }) => {}
                Err(error) => {
                    tracing::warn!(error = %error, connection = %candidate.connection, "the claim failed");
                }
            }
            cursors.reached(candidate);
        }
        if page.clock_ended {
            cursors.end(workspace, Scan::Clock);
        }
        if page.api_ended {
            cursors.end(workspace, Scan::Api);
        }
        if !took {
            idle(&shared, &mut shutdown, BACKOFF).await;
        }
    }
}

/// Waits for a wake-up, `wait`, or the shutdown, whichever comes first.
async fn idle(shared: &Shared, shutdown: &mut Shutdown, wait: Duration) {
    tokio::select! {
        () = shared.wake.notified() => {}
        () = tokio::time::sleep(wait) => {}
        () = shutdown.wait() => {}
    }
}

/// Runs one claimed wave: each message in its own task holding a slot, and the reports flushed
/// to the Finish in micro-batches.
async fn run_wave(shared: Arc<Shared>, wave: Wave) {
    let connection = wave.connection;
    let started = Instant::now();
    let tally = Arc::new(Mutex::new(Tally::default()));
    let result = process::guarded("delivery.wave", run_wave_inner(&shared, &wave, &tally)).await;
    let test_mode = result.as_ref().ok().copied();
    let tally = tally.lock().map(|tally| tally.clone()).unwrap_or_default();
    let outcome = if result.is_err() || tally.task_failures > 0 {
        "failed"
    } else {
        "completed"
    };
    if outcome == "failed" {
        tracing::Span::current().record("otel.status_code", "ERROR");
    }
    crate::telemetry::unit(crate::telemetry::Event::DeliveryWave);
    tracing::info!(
        event = "delivery.wave",
        workspace_id = %wave.workspace,
        connection_id = %connection,
        provider = wave.provider.as_str(),
        outcome,
        task_failures = tally.task_failures,
        claimed = wave.messages.len(),
        submitted = tally.submitted(),
        accepted = tally.accepted,
        transient = tally.transient,
        permanent = tally.permanent,
        uncertain = tally.uncertain,
        provider_latency_ms.p50 = tally.latency_ms(50),
        provider_latency_ms.p95 = tally.latency_ms(95),
        provider_latency_ms.max = tally.latency_ms(100),
        sessions_in_use = tally.sessions,
        test_mode,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "delivery.wave"
    );
}

async fn run_wave_inner(shared: &Arc<Shared>, wave: &Wave, tally: &Arc<Mutex<Tally>>) -> bool {
    let connection = wave.connection;
    let target = match submit::target(&shared.db, &shared.keys, wave.workspace, connection).await {
        Ok(target) => target,
        Err(error) => {
            tracing::warn!(error = %error, connection = %connection, "the connection could not be read for sending");
            None
        }
    };
    let test_mode = test_mode(&shared.db, wave.workspace).await;
    let (sender, mut receiver) = mpsc::channel::<Report>(FLUSH_REPORTS);
    let mut messages = JoinSet::new();
    for claimed in wave.messages.iter().copied() {
        let shared = Arc::clone(shared);
        let sender = sender.clone();
        let target = target.clone();
        let tally = Arc::clone(tally);
        let provider = wave.provider;
        let workspace = wave.workspace;
        // A child of the wave's span, kept for a few of a kept wave's messages only.
        let span = tracing::info_span!("delivery.message", message_id = %claimed.message, otel.status_code = tracing::field::Empty);
        messages.spawn(
            async move {
                let Ok(_slot) = Arc::clone(&shared.slots).acquire_owned().await else {
                    return;
                };
                let report = message(
                    &shared,
                    workspace,
                    connection,
                    provider,
                    target.as_ref(),
                    test_mode,
                    claimed,
                    &tally,
                );
                let result = process::guarded("delivery.message", report).await;
                let outcome = match &result {
                    Ok(Some(_)) => "completed",
                    Ok(None) => "fenced",
                    Err(()) => "task_panic",
                };
                opentelemetry::global::meter("norbelys")
                    .u64_counter("norbelys_delivery_tasks_total")
                    .build().add(1, &[KeyValue::new("provider", provider.as_str()), KeyValue::new("outcome", outcome)]);
                if result.is_err() {
                    tracing::Span::current().record("otel.status_code", "ERROR");
                    if let Ok(mut tally) = tally.lock() { tally.task_failures += 1; }
                    tracing::error!(event = "delivery.message", message_id = %claimed.message, error_code = "task_panic", "message task failed; its lease remains fenced for recovery");
                }
                let report = result.ok().flatten();
                if let Some(report) = report {
                    let _ = sender.send(report).await;
                }
            }
            .instrument(span),
        );
    }
    drop(sender);

    let mut batch = Vec::with_capacity(FLUSH_REPORTS);
    let mut flush_at = Instant::now() + FLUSH_EVERY;
    loop {
        let received = tokio::select! {
            received = receiver.recv() => received,
            () = tokio::time::sleep_until(flush_at), if !batch.is_empty() => {
                flush(shared, wave, &mut batch).await;
                flush_at = Instant::now() + FLUSH_EVERY;
                continue;
            }
        };
        match received {
            Some(report) => {
                if batch.is_empty() {
                    flush_at = Instant::now() + FLUSH_EVERY;
                }
                batch.push(report);
                if batch.len() >= FLUSH_REPORTS {
                    flush(shared, wave, &mut batch).await;
                }
            }
            None => break,
        }
    }
    flush(shared, wave, &mut batch).await;
    while let Some(joined) = messages.join_next().await {
        if let Err(error) = joined {
            tracing::error!(connection_id = %connection, error_code = if error.is_panic() { "task_panic" } else { "task_cancelled" }, "delivery message task failed");
        }
    }
    test_mode
}

/// Records `batch` through the Finish and empties it.
async fn flush(shared: &Shared, wave: &Wave, batch: &mut Vec<Report>) {
    if batch.is_empty() {
        return;
    }
    match finish::finish(
        &shared.db,
        wave.workspace,
        wave.connection,
        &shared.owner,
        batch,
    )
    .await
    {
        Ok(finished) => {
            if let Some(until) = finished.platform_until {
                shared.limits.pause_platform(wave.provider, until);
            }
            if finished.lost > 0 {
                tracing::warn!(lost = finished.lost, connection = %wave.connection, "reports whose lease was lost were not recorded");
            }
        }
        Err(error) => {
            tracing::error!(error = %error, connection = %wave.connection, reports = batch.len(), "a finish failed; its leases expire into recovery");
        }
    }
    batch.clear();
}

/// The workspace's mode, read once per wave: `true` for test mode. A read that fails counts as
/// test mode, so a database hiccup never sends a test workspace's mail to a provider (the Start
/// reads the mode again and decides).
async fn test_mode(db: &Database, workspace: crate::domain::ids::WorkspaceId) -> bool {
    let read: Result<Option<String>, sqlx::Error> = async {
        let mut tx = db.begin_in(workspace).await?;
        let mode = sqlx::query_scalar!(
            "SELECT mode FROM workspaces WHERE id = $1",
            workspace.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(mode)
    }
    .await;
    !matches!(read, Ok(Some(mode)) if mode == "live")
}

/// Takes one claimed message through rendering, preflight, a session, its Start and its
/// submission; the report to record, or `None` when the Start already wrote its fate. A
/// submission is counted in its wave's `tally` and in the delivery metrics.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site, the wave's, which hands each message what the wave read once"
)]
async fn message(
    shared: &Shared,
    workspace: crate::domain::ids::WorkspaceId,
    connection: crate::domain::ids::Id<crate::domain::ids::Connection>,
    provider: Provider,
    target: Option<&Target>,
    test_mode: bool,
    claimed: Claimed,
    tally: &Mutex<Tally>,
) -> Option<Report> {
    let report = |reported: Reported| Report {
        message: claimed.message,
        generation: claimed.generation,
        reported,
    };
    if shared.shutdown.requested() {
        return Some(report(Reported::Released { run_at: None }));
    }

    // 1. Render.
    let prepared = match rendering::prepare(
        &shared.db,
        &shared.rendering,
        workspace,
        claimed.message,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) if error.is_permanent() => {
            return Some(report(Reported::Skipped {
                category: Category::RenderFailed,
                detail: error.to_string(),
                evidence: Vec::new(),
            }));
        }
        Err(error) => {
            tracing::warn!(error = %error, message = %claimed.message, "the message could not be prepared now");
            return Some(report(Reported::Released { run_at: None }));
        }
    };
    let snapshot = async {
        let mut tx = shared.db.begin_in(workspace).await?;
        let stored = crate::delivery::content::prepared(
            &mut tx,
            workspace,
            claimed.message,
            &shared.owner,
            claimed.generation,
            &prepared.raw,
        )
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(stored)
    }
    .await;
    if !matches!(snapshot, Ok(true)) {
        return Some(report(Reported::Released { run_at: None }));
    }
    let recipients: Vec<String> = prepared
        .envelope
        .recipients()
        .iter()
        .map(ToString::to_string)
        .collect();

    // 2. Preflight. A workspace in test mode delivers nothing, so only the syntax is checked: its
    // developers write to addresses such as `@example.com`, a domain that accepts no mail.
    let reasons = if test_mode {
        recipients
            .iter()
            .map(|recipient| match EmailAddress::parse(recipient) {
                Ok(_) => Reason::Mx,
                Err(_) => Reason::Syntax,
            })
            .collect()
    } else {
        match routes(shared, workspace, &recipients).await {
            Ok(reasons) => reasons,
            Err(error) => {
                tracing::warn!(error = %error, message = %claimed.message, "the preflight cache could not be read or written now");
                return Some(report(Reported::Released { run_at: None }));
            }
        }
    };
    match policy::after_preflight(&reasons) {
        Preflight::Proceed { .. } => {}
        Preflight::Defer => {
            return Some(report(Reported::Released {
                run_at: Some(process::now().plus(DNS_RETRY)),
            }));
        }
        Preflight::Fail {
            category,
            unroutable,
        } => {
            let evidence = unroutable
                .iter()
                .filter_map(|index| {
                    let recipient = recipients.get(*index)?;
                    let reason = reasons.get(*index)?;
                    Some(preflight_evidence(&claimed, recipient, *reason, category))
                })
                .collect();
            return Some(report(Reported::Skipped {
                category,
                detail: "no envelope address can receive mail".to_owned(),
                evidence,
            }));
        }
    }

    // 3. A session, under the claim's lease.
    let lane = if test_mode {
        Lane::Fake
    } else {
        let Some(target) = target else {
            return Some(report(Reported::Released { run_at: None }));
        };
        let deadline = Instant::now() + ACQUIRE_BUDGET;
        match shared
            .transports
            .acquire(&shared.db, target, deadline)
            .await
        {
            Ok(lane) => lane,
            Err(rejection) => {
                let source = if matches!(provider, Provider::Google | Provider::Microsoft) {
                    Source::ProviderApi
                } else {
                    Source::Smtp
                };
                return Some(report(Reported::Answered(Box::new(submit::answered(
                    Err(rejection),
                    source,
                    None,
                    recipients,
                )))));
            }
        }
    };

    // The session (or request slot) is counted while it is held, until the message is done.
    let _in_use = (!matches!(lane, Lane::Fake)).then(|| InUse::hold(provider));

    // 4. The Start.
    let request = start::Start {
        workspace,
        connection,
        message: claimed.message,
        generation: claimed.generation,
        owner: &shared.owner,
        budget: lane.budget(),
        retry_window: shared.retry_window,
    };
    let begun = match start::start(&shared.db, &request).await {
        Ok(start::Started::Submit(begun)) => {
            if let Some(scheduled) = begun.scheduled {
                COLD_START_LAG.record(
                    crate::domain::schedule::cold_start_lag(scheduled.0, begun.started.0)
                        .as_secs_f64(),
                    &[KeyValue::new("provider", provider.as_str())],
                );
            }
            begun
        }
        Ok(start::Started::Returned(_) | start::Started::Ended(_) | start::Started::Lost) => {
            return None;
        }
        Err(error) => {
            tracing::warn!(error = %error, message = %claimed.message, "the Start failed; the lease expires into recovery");
            return None;
        }
    };
    let lane = match (begun.test_mode, lane) {
        (true, _) => Lane::Fake,
        (false, Lane::Fake) => {
            // The workspace left test mode after the wave read it: nothing was sent; the message
            // is tried again through its provider.
            let rejection = norbelys_mail::submission::Rejection {
                failure: norbelys_mail::submission::Failure::Transient,
                phase: norbelys_mail::submission::Phase::MailFrom,
                scope: norbelys_mail::submission::Scope::Message,
                cause: norbelys_mail::submission::Cause::Deadline,
                code: None,
                status: None,
                retry_after: None,
                diagnostic: "the workspace left test mode before the submission; tried again"
                    .to_owned(),
                refused: Vec::new(),
            };
            return Some(report(Reported::Answered(Box::new(submit::answered(
                Err(rejection),
                Source::Smtp,
                Some(begun.started),
                recipients,
            )))));
        }
        (false, lane) => lane,
    };

    // 5. The submission, before the Start's deadline.
    let source = lane.source();
    let submitted = Instant::now();
    let result = lane
        .submit(shared.transports.http(), &prepared, begun.deadline)
        .await;
    let latency = submitted.elapsed();
    let outcome = match &result {
        Ok(_) => "accepted",
        Err(rejection) => rejection.failure.as_str(),
    };
    if !begun.test_mode {
        let label = KeyValue::new("provider", provider.as_str());
        SUBMISSIONS.add(1, &[label.clone(), KeyValue::new("outcome", outcome)]);
        PROVIDER_LATENCY.record(latency.as_secs_f64(), &[label]);
    }
    if let Ok(mut tally) = tally.lock() {
        tally.record(outcome, latency, HELD.load(Ordering::Relaxed));
    }
    // One line per message, for debugging only: the wave's event is the record.
    tracing::debug!(
        workspace = %workspace,
        connection = %connection,
        message = %claimed.message,
        provider = provider.as_str(),
        test_mode = begun.test_mode,
        outcome,
        duration_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
        "submission"
    );
    Some(report(Reported::Answered(Box::new(submit::answered(
        result,
        source,
        Some(begun.started),
        recipients,
    )))))
}

/// The preflight reason of each address, in order: its syntax, then its mail route from the
/// workspace's preflight cache or DNS ([`preflight::routes`], which caches what DNS answered).
///
/// # Errors
///
/// The database is unavailable: the cache could not be read or written.
async fn routes(
    shared: &Shared,
    workspace: crate::domain::ids::WorkspaceId,
    recipients: &[String],
) -> Result<Vec<Reason>, sqlx::Error> {
    let parsed: Vec<Option<EmailAddress>> = recipients
        .iter()
        .map(|recipient| EmailAddress::parse(recipient).ok())
        .collect();
    let addresses: Vec<EmailAddress> = parsed.iter().flatten().cloned().collect();
    let routes = preflight::routes(&shared.db, &shared.resolver, workspace, &addresses).await?;
    Ok(parsed
        .iter()
        .map(|address| match address {
            None => Reason::Syntax,
            Some(address) => routes
                .get(&address.key())
                .copied()
                .unwrap_or(Reason::DnsUnavailable),
        })
        .collect())
}

/// The evidence a preflight finding about one address's domain records: the resolver's answer
/// about the domain itself, as trustworthy as DNS.
fn preflight_evidence(
    claimed: &Claimed,
    recipient: &str,
    reason: Reason,
    category: Category,
) -> Evidence {
    Evidence {
        message: Some(claimed.message),
        thread: None,
        attempt_number: Some(claimed.attempt_number),
        recipient: Some(recipient.to_owned()),
        recipient_ref: RecipientRef::Named,
        source: Source::Preflight,
        source_event_id: format!(
            "preflight:{}:{}:{recipient}",
            claimed.message, claimed.attempt_number
        ),
        received_via: None,
        kind: EventKind::Rejected,
        action: None,
        phase: None,
        enhanced_status: None,
        category,
        diagnostic: Some(format!("preflight: {}", reason_text(reason))),
        confidence: Confidence::Authenticated,
        receipt: None,
        observed_at: process::now(),
    }
}

fn reason_text(reason: Reason) -> &'static str {
    match reason {
        Reason::Mx => "the domain has MX hosts",
        Reason::ImplicitMx => "the domain is its own mail host",
        Reason::Syntax => "the address is not an address",
        Reason::NoDomain => "the domain does not exist",
        Reason::NullMx => "the domain accepts no mail (null MX)",
        Reason::NoRoute => "the domain has no mail route",
        Reason::DnsUnavailable => "DNS did not answer",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::delivery::limits::Shares;
    use crate::domain::ids::{Id, Message};
    use crate::senders;
    use crate::testing::{self, SenderSpec, TestDb};

    /// A wave's tally counts each outcome once and reads its latencies by nearest rank, so the
    /// wave's one event says how many were accepted and how slow the provider was, with no line
    /// per message; an outcome it does not know is not counted, and an empty wave reads 0.
    #[test]
    fn a_wave_tallies_its_submissions() {
        let mut tally = Tally::default();
        assert_eq!(tally.latency_ms(95), 0);
        for (outcome, ms, sessions) in [
            ("accepted", 40, 2),
            ("accepted", 10, 3),
            ("transient", 30, 1),
            ("uncertain", 120_000, 1),
            ("permanent", 20, 1),
            ("refused", 5, 9),
        ] {
            tally.record(outcome, Duration::from_millis(ms), sessions);
        }
        assert_eq!(
            (
                tally.accepted,
                tally.transient,
                tally.permanent,
                tally.uncertain
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(tally.submitted(), 5);
        assert_eq!(tally.latency_ms(50), 30);
        assert_eq!(tally.latency_ms(95), 120_000);
        assert_eq!(tally.latency_ms(100), 120_000);
        assert_eq!(tally.sessions, 3);
    }

    /// The role's shared state over a test database, with the fake-transport-only setup a test
    /// workspace needs: no OAuth app, an offline resolver, the worker's login.
    fn shared(test: &TestDb) -> Arc<Shared> {
        let keys = testing::keys();
        let resolver = Resolver::offline();
        let tracking = url::Url::parse("https://tracking.norbelys.test").unwrap();
        Arc::new(Shared {
            db: test.worker.clone(),
            keys: keys.clone(),
            resolver: resolver.clone(),
            rendering: rendering::Settings::new(
                keys.clone(),
                &tracking,
                Duration::from_secs(86_400),
            )
            .unwrap(),
            transports: Transports::new(submit::Config {
                keys,
                http: norbelys_mail::http::HttpClient::new().unwrap(),
                apps: senders::oauth::Apps::default(),
                resolver,
                allow_private_hosts: false,
            })
            .unwrap(),
            limits: Limits::new(Shares::default()),
            owner: "sender:test".to_owned(),
            retry_window: Duration::from_secs(86_400),
            slots: Arc::new(Semaphore::new(8)),
            wake: Notify::new(),
            shutdown: Shutdown::manual().0,
        })
    }

    /// A message of a workspace in test mode goes all the way: claimed with its wave, rendered,
    /// checked, started, answered by the fake transport by its recipient's local part, and
    /// recorded by the Finish. Accepted mail ends `sent`, a refusal for good `failed`, a refusal
    /// for now goes back to the queue for a later attempt, and a lost answer is `uncertain`,
    /// never retried automatically. This is the path every real submission takes too, with only
    /// the transport swapped.
    #[tokio::test]
    async fn a_test_workspace_message_goes_from_the_queue_to_its_outcome() {
        let test = TestDb::new().await;
        let ws = test.workspace("acme").await.id;
        sqlx::query("UPDATE workspaces SET mode = 'test' WHERE id = $1")
            .bind(ws.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
        let sender = test.sender(ws, &SenderSpec::relay("hello@acme.test")).await;
        let mut messages: HashMap<&str, Id<Message>> = HashMap::new();
        for to in [
            "ada@example.com",
            "bounce@example.com",
            "defer@example.com",
            "uncertain@example.com",
            "blocked@example.com",
        ] {
            messages.insert(to, test.direct_message(ws, &sender, &[to], -60).await);
        }

        let shared = shared(&test);
        let mut cursors = Cursors::default();
        let workspace = claim::turn(&shared.db).await.unwrap().unwrap();
        let page = claim::page(&shared.db, workspace, &mut cursors)
            .await
            .unwrap();
        let candidate = page
            .candidates
            .iter()
            .find(|candidate| candidate.connection == sender.connection)
            .unwrap();
        let limits = shared.limits.clone();
        let mut charge = move |charge: &claim::Charge<'_>| limits.admit(charge);
        let Claim::Wave(wave) = claim::claim(&shared.db, &shared.owner, candidate, 8, &mut charge)
            .await
            .unwrap()
        else {
            panic!("the relay's due messages are claimed");
        };
        assert_eq!(wave.messages.len(), 5);
        run_wave(Arc::clone(&shared), wave).await;

        let state = |message: Id<Message>| {
            let pool = test.system.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT state FROM messages WHERE id = $1")
                    .bind(message.uuid())
                    .fetch_one(pool.pool())
                    .await
                    .unwrap()
            }
        };
        assert_eq!(state(messages["ada@example.com"]).await, "sent");
        assert_eq!(state(messages["bounce@example.com"]).await, "failed");
        assert_eq!(state(messages["blocked@example.com"]).await, "failed");
        assert_eq!(state(messages["uncertain@example.com"]).await, "uncertain");
        assert_eq!(state(messages["defer@example.com"]).await, "queued");
        let attempts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM attempts WHERE workspace_id = $1 AND quota_state <> 'reserved'",
        )
        .bind(ws.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(attempts, 5, "every attempt is settled once");
    }
}
