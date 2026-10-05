//! Admission control: backpressure for authenticated submissions before the spool volume
//! fills. Postfix's own limits do not provide it: its active-queue limit only slows the queue
//! manager, and `queue_minfree` is the last floor, where Postfix itself refuses mail with
//! `452`. This module closes earlier, on three measures with hysteresis:
//!
//! - free bytes on the volume holding the queue (`statvfs`), closing below one threshold and
//!   reopening only above a higher one;
//! - free inodes on that volume, likewise;
//! - the backlog: authenticated submissions that entered the queue and have not left it, as the
//!   tail sees them (rows of `submissions` with a message id), closing above one threshold and
//!   reopening only below a lower one.
//!
//! Admission closes when any measure crosses its closing threshold and reopens only when all
//! are back past their reopening thresholds, so it does not flap at a boundary. The measures are
//! sampled every ten seconds; a failed sample closes admission.
//!
//! Postfix asks through its policy delegation protocol
//! (<https://www.postfix.org/SMTPD_POLICY_README.html>): `name=value` lines ending with an empty
//! line, answered with `action=…` and an empty line, on a connection Postfix keeps open. The
//! answer is `DUNNO` (no opinion) unless admission is closed and the request carries a
//! `sasl_username`; then it is `452 4.3.1` with a reason, which a submitting client treats as
//! temporary. Inbound mail is never refused here. The deployment wires the listener into the
//! submission service's `smtpd_recipient_restrictions` with `check_policy_service` and chooses
//! what Postfix does when the listener is down (its `default_action`).
//!
//! Metrics: `norbelys_mta_admission_open` (1 or 0), the three measures, every threshold as
//! `norbelys_mta_admission_threshold{measure, edge}`, and the answers given.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::AdmissionArgs;
use crate::db::Db;
use crate::serve::Shutdown;
use crate::telemetry;

/// How often the measures are sampled.
const PERIOD: Duration = Duration::from_secs(10);
/// The longest policy request line, newline included.
const MAX_LINE: u64 = 4096;
/// The most attributes in one policy request.
const MAX_ATTRIBUTES: usize = 200;
/// The answer while admission is closed.
const DEFER: &str =
    "action=452 4.3.1 Admission control: the mail queue is over its limit, try again later\n\n";
/// The answer otherwise.
const DUNNO: &str = "action=DUNNO\n\n";

/// One sample of the three measures.
#[derive(Debug, Clone, Copy)]
pub struct Measures {
    /// Bytes available to unprivileged writers on the spool volume.
    pub free_bytes: u64,
    /// Inodes available on the spool volume.
    pub free_inodes: u64,
    /// Authenticated submissions in the queue.
    pub backlog: u64,
}

/// The next state of admission, `true` when open, from the current state and a sample.
#[must_use]
pub fn decide(open: bool, m: Measures, t: &AdmissionArgs) -> bool {
    if open {
        m.free_bytes >= t.admission_close_free_bytes
            && m.free_inodes >= t.admission_close_free_inodes
            && m.backlog <= t.admission_close_backlog
    } else {
        m.free_bytes > t.admission_open_free_bytes
            && m.free_inodes > t.admission_open_free_inodes
            && m.backlog < t.admission_open_backlog
    }
}

/// Checks that every reopening threshold lies beyond its closing one.
///
/// # Errors
///
/// A pair is inverted or equal, which would make admission flap.
pub fn validate(t: &AdmissionArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        t.admission_open_free_bytes > t.admission_close_free_bytes,
        "the reopening free-bytes threshold must exceed the closing one"
    );
    anyhow::ensure!(
        t.admission_open_free_inodes > t.admission_close_free_inodes,
        "the reopening free-inodes threshold must exceed the closing one"
    );
    anyhow::ensure!(
        t.admission_open_backlog < t.admission_close_backlog,
        "the reopening backlog threshold must be below the closing one"
    );
    Ok(())
}

fn statvfs(path: &Path) -> std::io::Result<(u64, u64)> {
    let stat = rustix::fs::statvfs(path)?;
    Ok((stat.f_bavail.saturating_mul(stat.f_frsize), stat.f_favail))
}

struct Instruments {
    open: Gauge<u64>,
    free_bytes: Gauge<u64>,
    free_inodes: Gauge<u64>,
    backlog: Gauge<u64>,
    thresholds: Gauge<u64>,
}

impl Instruments {
    fn new() -> Self {
        let meter = telemetry::meter();
        Self {
            open: meter
                .u64_gauge("norbelys_mta_admission_open")
                .with_description("1 while authenticated submissions are admitted")
                .build(),
            free_bytes: meter
                .u64_gauge("norbelys_mta_spool_free_bytes")
                .with_unit("By")
                .with_description("Bytes available on the spool volume")
                .build(),
            free_inodes: meter
                .u64_gauge("norbelys_mta_spool_free_inodes")
                .with_description("Inodes available on the spool volume")
                .build(),
            backlog: meter
                .u64_gauge("norbelys_mta_backlog_messages")
                .with_description("Authenticated submissions in the queue")
                .build(),
            thresholds: meter
                .u64_gauge("norbelys_mta_admission_threshold")
                .with_description("Admission thresholds by measure and edge (close, open)")
                .build(),
        }
    }

    fn thresholds(&self, t: &AdmissionArgs) {
        for (measure, edge, value) in [
            ("free_bytes", "close", t.admission_close_free_bytes),
            ("free_bytes", "open", t.admission_open_free_bytes),
            ("free_inodes", "close", t.admission_close_free_inodes),
            ("free_inodes", "open", t.admission_open_free_inodes),
            ("backlog", "close", t.admission_close_backlog),
            ("backlog", "open", t.admission_open_backlog),
        ] {
            self.thresholds.record(
                value,
                &[
                    KeyValue::new("measure", measure),
                    KeyValue::new("edge", edge),
                ],
            );
        }
    }
}

/// Samples the measures until shutdown and keeps `open` up to date.
///
/// # Errors
///
/// A task failure stops the service. A failed database or disk sample closes admission.
pub async fn sample(
    db: Db,
    queue: crate::queue::Queue,
    spool: PathBuf,
    thresholds: AdmissionArgs,
    open: Arc<AtomicBool>,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let instruments = Instruments::new();
    loop {
        let path = spool.clone();
        let capture = queue.clone();
        let disk = tokio::task::spawn_blocking(move || {
            let (bytes, inodes) = statvfs(&path)?;
            let full = capture.full().map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>((bytes, inodes, full))
        })
        .await?;
        let backlog = db
            .call(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM submissions WHERE internet_message_id IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(anyhow::Error::from)
            })
            .await;
        match (disk, backlog) {
            (Ok((free_bytes, free_inodes, full)), Ok(backlog)) => {
                let measures = Measures {
                    free_bytes,
                    free_inodes,
                    backlog: u64::try_from(backlog).unwrap_or(0),
                };
                let was = open.load(Ordering::Relaxed);
                let now = !full && decide(was, measures, &thresholds);
                if now != was {
                    open.store(now, Ordering::Relaxed);
                    let state = if now { "open" } else { "closed" };
                    telemetry::unit(telemetry::Event::Admission);
                    tracing::warn!(
                        event = "mta.admission",
                        state,
                        free_bytes = measures.free_bytes,
                        free_inodes = measures.free_inodes,
                        backlog = measures.backlog,
                        pending_full = full,
                        "mta.admission"
                    );
                }
                instruments.open.record(u64::from(now), &[]);
                instruments.free_bytes.record(measures.free_bytes, &[]);
                instruments.free_inodes.record(measures.free_inodes, &[]);
                instruments.backlog.record(measures.backlog, &[]);
                instruments.thresholds(&thresholds);
            }
            (Err(error), _) => {
                if open.swap(false, Ordering::Relaxed) {
                    telemetry::unit(telemetry::Event::Admission);
                    tracing::warn!(
                        event = "mta.admission",
                        state = "closed",
                        reason = "disk_unavailable",
                        "mta.admission"
                    );
                }
                instruments.open.record(0, &[]);
                tracing::error!(error = %error, spool = %spool.display(), "admission cannot measure the spool")
            }
            (_, Err(error)) => {
                if open.swap(false, Ordering::Relaxed) {
                    telemetry::unit(telemetry::Event::Admission);
                    tracing::warn!(
                        event = "mta.admission",
                        state = "closed",
                        reason = "database_unavailable",
                        "mta.admission"
                    );
                }
                instruments.open.record(0, &[]);
                tracing::error!(error = %error, "admission cannot count the backlog")
            }
        }
        if !shutdown.sleep(PERIOD).await {
            return Ok(());
        }
    }
}

/// Answers Postfix's policy requests on `addr` until shutdown.
///
/// # Errors
///
/// The listener cannot bind.
pub async fn serve(
    addr: SocketAddr,
    open: Arc<AtomicBool>,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "admission policy listening");
    let answers = telemetry::meter()
        .u64_counter("norbelys_mta_policy_answers")
        .with_description("Policy answers by action: dunno, defer")
        .build();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let (open, answers) = (Arc::clone(&open), answers.clone());
                    tokio::spawn(async move {
                        if let Err(error) = answer(stream, &open, &answers).await {
                            tracing::debug!(error = %error, "policy connection closed");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(error = %error, "policy accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            () = shutdown.wait() => return Ok(()),
        }
    }
}

/// Serves one Postfix connection: one answer per request, until Postfix closes it. An
/// over-long line or request closes the connection.
async fn answer(
    stream: TcpStream,
    open: &AtomicBool,
    answers: &Counter<u64>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut authenticated = false;
    let mut attributes = 0_usize;
    let mut line = String::new();
    loop {
        line.clear();
        if (&mut reader).take(MAX_LINE).read_line(&mut line).await? == 0 || !line.ends_with('\n') {
            return Ok(());
        }
        let attribute = line.trim_end_matches(['\n', '\r']);
        if attribute.is_empty() {
            let defer = authenticated && !open.load(Ordering::Relaxed);
            write
                .write_all(if defer { DEFER } else { DUNNO }.as_bytes())
                .await?;
            answers.add(
                1,
                &[KeyValue::new(
                    "action",
                    if defer { "defer" } else { "dunno" },
                )],
            );
            authenticated = false;
            attributes = 0;
            continue;
        }
        attributes += 1;
        if attributes > MAX_ATTRIBUTES {
            return Ok(());
        }
        if let Some(login) = attribute.strip_prefix("sasl_username=") {
            authenticated = !login.is_empty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: AdmissionArgs = AdmissionArgs {
        admission_close_free_bytes: 100,
        admission_open_free_bytes: 200,
        admission_close_free_inodes: 10,
        admission_open_free_inodes: 20,
        admission_close_backlog: 1000,
        admission_open_backlog: 800,
    };

    const HEALTHY: Measures = Measures {
        free_bytes: 500,
        free_inodes: 50,
        backlog: 0,
    };

    /// Admission closes as soon as any measure crosses its closing threshold, and reopens only
    /// once every measure is past its reopening threshold: between the two it keeps its state,
    /// so it does not flap at a boundary.
    #[test]
    fn closes_on_any_measure_and_reopens_on_all() {
        for (open, measures, expected) in [
            (true, HEALTHY, true),
            (
                true,
                Measures {
                    free_bytes: 100,
                    ..HEALTHY
                },
                true,
            ),
            (
                true,
                Measures {
                    free_bytes: 99,
                    ..HEALTHY
                },
                false,
            ),
            (
                true,
                Measures {
                    free_inodes: 9,
                    ..HEALTHY
                },
                false,
            ),
            (
                true,
                Measures {
                    backlog: 1001,
                    ..HEALTHY
                },
                false,
            ),
            (
                false,
                Measures {
                    free_bytes: 150,
                    ..HEALTHY
                },
                false,
            ),
            (
                false,
                Measures {
                    free_bytes: 200,
                    ..HEALTHY
                },
                false,
            ),
            (
                false,
                Measures {
                    free_bytes: 201,
                    free_inodes: 20,
                    ..HEALTHY
                },
                false,
            ),
            (
                false,
                Measures {
                    backlog: 800,
                    ..HEALTHY
                },
                false,
            ),
            (
                false,
                Measures {
                    backlog: 799,
                    ..HEALTHY
                },
                true,
            ),
            (false, HEALTHY, true),
        ] {
            assert_eq!(decide(open, measures, &T), expected, "{open} {measures:?}");
        }
    }

    /// Thresholds whose reopening value is not beyond the closing one are refused at start.
    #[test]
    fn refuses_thresholds_without_hysteresis() {
        assert!(validate(&T).is_ok());
        for inverted in [
            AdmissionArgs {
                admission_open_free_bytes: 100,
                ..T
            },
            AdmissionArgs {
                admission_open_free_inodes: 5,
                ..T
            },
            AdmissionArgs {
                admission_open_backlog: 1000,
                ..T
            },
        ] {
            assert!(validate(&inverted).is_err());
        }
    }

    /// An unavailable canonical database closes admission even with healthy disk and an empty
    /// queue; a full pending queue also closes it while the remote database is healthy.
    #[test]
    fn storage_failures_and_pending_capacity_stop_new_submissions() {
        let server = crate::testing::SqlServer::new();
        let dir = crate::testing::TempDir::new();
        let queue =
            crate::queue::Queue::open(&dir.join("pending.sqlite"), "mail-a", 4 * 1024 * 1024)
                .unwrap();
        // This owner stays outside the async runtime so its blocking client shuts down there.
        let db = Db::remote(&server.args, "mail-a", None).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for unavailable in [true, false] {
            server.available.store(!unavailable, Ordering::SeqCst);
            if !unavailable {
                let mut next = 0;
                while !queue.full().unwrap() {
                    queue
                        .append(vec![crate::queue::Record::Event {
                            id: format!("capacity-{next}"),
                            route: "r1".to_owned(),
                            payload: "x".repeat(16 * 1024),
                            created: 1.0,
                        }])
                        .unwrap();
                    next += 1;
                }
            }
            let open = Arc::new(AtomicBool::new(true));
            let shared = Arc::clone(&open);
            let database = db.clone();
            let capture = queue.clone();
            let spool = dir.path().to_owned();
            runtime.block_on(async move {
                let (stop, receiver) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(sample(
                    database,
                    capture,
                    spool,
                    T,
                    shared,
                    Shutdown::from(receiver),
                ));
                tokio::time::timeout(Duration::from_secs(2), async {
                    while open.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                stop.send(true).unwrap();
                task.await.unwrap().unwrap();
            });
        }
    }

    /// Postfix's policy protocol over a kept-open connection: `DUNNO` while admission is open,
    /// `452 4.3.1` for an authenticated request while closed, `DUNNO` for inbound mail even then
    /// (attributes reset between requests), and an over-long line closes the connection.
    #[tokio::test]
    async fn answers_postfix_policy_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let open = Arc::new(AtomicBool::new(true));
        let shared = Arc::clone(&open);
        tokio::spawn(async move {
            let counter = telemetry::meter().u64_counter("test").build();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (open, counter) = (Arc::clone(&shared), counter.clone());
                tokio::spawn(async move { answer(stream, &open, &counter).await });
            }
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut ask = async |request: &str| {
            write.write_all(request.as_bytes()).await.unwrap();
            let mut answer = String::new();
            reader.read_line(&mut answer).await.unwrap();
            let mut blank = String::new();
            reader.read_line(&mut blank).await.unwrap();
            assert_eq!(blank, "\n");
            answer
        };
        let authenticated = "request=smtpd_access_policy\nsasl_username=relay@example.com\n\n";
        let inbound = "request=smtpd_access_policy\nsasl_username=\n\n";
        assert_eq!(ask(authenticated).await, "action=DUNNO\n");
        open.store(false, Ordering::Relaxed);
        assert!(ask(authenticated).await.starts_with("action=452 4.3.1 "));
        assert_eq!(ask(inbound).await, "action=DUNNO\n");

        let mut long = TcpStream::connect(addr).await.unwrap();
        long.write_all(&vec![b'x'; 5000]).await.unwrap();
        let mut rest = Vec::new();
        // The server closes with unread bytes pending, which may surface as a reset rather than
        // a clean end: either way the read returns, with no answer.
        let _ = long.read_to_end(&mut rest).await;
        assert!(rest.is_empty());
    }
}
