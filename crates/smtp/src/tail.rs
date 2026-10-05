//! The collector: reads Postfix's `mail.log` from a checkpoint and turns the delivery lines of
//! authenticated submissions into delivery events in the `events` outbox ([`crate::events`]).
//! Evidence comes from Postfix's own log, so the reporter of every event is the next hop's
//! SMTP reply (or the local delivery agent), which the MTA itself observed: `provenance` is
//! `smtp_reply`. See <https://www.postfix.org/MAILLOG_README.html> for the log format.
//!
//! Correlation, with the expressions in [`Expressions`]: an `smtpd` line
//! with `sasl_username=` records the queue id's login (and forgets a message id inherited from a
//! reused queue id); the `cleanup` line within 300 seconds adds the `message-id`; each delivery
//! line of `smtp`, `lmtp`, `local`, `pipe` or `error` (the last carries cached destination
//! failures) with a `dsn=`, a `status=` and a recipient becomes one event; `qmgr … removed` ends
//! the queue id. Lines of unauthenticated mail, or of a login without a route, produce nothing:
//! a tenant is never guessed (the latter are counted).
//!
//! Return paths: when the `qmgr` line of an authenticated submission shows a VERP envelope
//! sender on the MTA's own host (`from=<bounce+<token>@<mail host>>`, [`crate::bounce`]), the
//! token is recorded in `returns` with the submission's login, Message-ID and queue id. That
//! record, made from the MTA's own log of an authenticated session, is what later proves a
//! returned notification concerns our message and selects its tenant. Rows of `submissions`
//! and `returns` live seven days at most (retention in [`prune`]): long enough for the slowest
//! remote retries to give up and report.
//!
//! The checkpoint is `(inode, byte offset)`: correlation changes first commit in Turso,
//! then the checkpoint and evidence commit together in the bounded local queue. Confirmed
//! archival advances the remote checkpoint before freeing local records. A crash replays
//! uncheckpointed lines and never skips them; event ids are SHA-256 of `node:inode:offset:sha256(line)`, so a replayed line
//! inserts nothing. Rotation must rename the file (logrotate without `copytruncate`, which would
//! lose unread bytes and invalidate the offset): the checkpointed inode is found beside
//! `mail.log`, drained, and left once it has been quiet for ten seconds. A missing or shortened
//! checkpointed file, or a line over 64 KiB, stops the tail with an error for an operator:
//! skipping would lose evidence silently.

use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{Connection, OptionalExtension as _, Transaction, params};
use jiff::Timestamp;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use regex::Regex;

use crate::events::{self, Evidence, Journaled};
use crate::serve::Shutdown;
use crate::telemetry;
use crate::{bounce, crypto, db};

/// The longest line accepted, newline excluded.
const MAX_LINE: u64 = 64 * 1024;
/// Lines read per transaction.
const LIMIT: usize = 100;
/// How long a rotated file must stay unchanged before the tail leaves it.
const QUIET: Duration = Duration::from_secs(10);
/// How long correlation rows are kept.
const SUBMISSION_TTL_SECONDS: f64 = 7.0 * 86_400.0;

/// Why the tail cannot advance.
#[derive(Debug, thiserror::Error)]
pub enum TailError {
    /// The checkpoint cannot be followed: its file is gone or shorter than the offset. An
    /// operator reconciles the rotated logs, then deletes the node's row from `cursors`; the
    /// tail restarts at the beginning of the current `mail.log`, and event ids make re-read
    /// lines insert nothing.
    #[error("log cursor gap: {0}; reconcile the rotated logs, then delete the node's cursor")]
    Gap(&'static str),
    /// A line longer than 64 KiB, far beyond any line Postfix logs: the file needs a look.
    #[error("a log line exceeds 64 KiB; operator review required")]
    Oversized,
    /// The log cannot be read.
    #[error("log: {0}")]
    Io(#[from] io::Error),
    /// The database failed; nothing of the step was committed.
    #[error("database: {0}")]
    Database(#[from] db::Error),
    /// An expression failed to compile.
    #[error("expression: {0}")]
    Regex(#[from] regex::Error),
}

/// The expressions over rsyslog's high-precision lines (`<RFC 3339 time> <host>
/// postfix/<service>[<pid>]: <queue id>: <detail>`).
struct Expressions {
    /// The whole line: time, service (`smtpd`, `submission/smtpd`, `cleanup`, `smtp`, …), queue
    /// id and detail.
    line: Regex,
    /// The authenticated login in an `smtpd` detail.
    auth: Regex,
    /// The `Message-ID` header `cleanup` logs, angle brackets included.
    message_id: Regex,
    /// A delivery agent's outcome: enhanced status (RFC 3463), `sent`, `deferred` or `bounced`,
    /// and the diagnostic in the final parentheses.
    delivery: Regex,
    /// `orig_to=<…>` (preferred: the address the submitter used) and `to=<…>`.
    recipient: Regex,
    /// A `qmgr` envelope sender that is a VERP return path on the MTA's own host; the token.
    return_path: Regex,
}

impl Expressions {
    /// The expressions, with `mail_host` the domain of the MTA's VERP return paths.
    fn new(mail_host: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            line: Regex::new(r"^(\S+) \S+ postfix/([a-z0-9_/-]+)\[\d+\]: ([A-Za-z0-9]+): (.*)$")?,
            auth: Regex::new(r"(?:^|, )sasl_username=([^,\s]+)")?,
            message_id: Regex::new(r"message-id=(<[^<>\s]{1,510}>)")?,
            delivery: Regex::new(
                r"(?:^|, )dsn=([245]\.\d{1,3}\.\d{1,3}), status=(sent|deferred|bounced) \((.*)\)$",
            )?,
            recipient: Regex::new(r"(?:^|, )(orig_to|to)=<([^<>\s]{1,254})>")?,
            return_path: Regex::new(&format!(
                r"^from=<{}([0-9a-z]{{{},{}}})@{}>, ",
                regex::escape(bounce::PREFIX),
                bounce::TOKEN_MIN,
                bounce::TOKEN_MAX,
                regex::escape(mail_host)
            ))?,
        })
    }
}

/// What one step read.
#[derive(Debug, Default)]
pub struct Progress {
    /// Complete lines consumed.
    pub lines: usize,
    /// Events inserted.
    pub events: usize,
    /// Delivery lines of logins without a route.
    pub unrouted: usize,
    /// VERP return paths recorded.
    pub returns: usize,
    /// Bytes of the log not yet read.
    pub lag_bytes: u64,
    /// `mail.log` does not exist (yet): nothing was read.
    pub missing: bool,
}

/// The reader of one node's `mail.log`.
pub struct Tail {
    node: String,
    path: PathBuf,
    conn: Connection,
    expressions: Expressions,
}

impl Tail {
    /// A tail of `path` checkpointing with its evidence through the supplied connection; `mail_host` is the domain
    /// of the MTA's VERP return paths.
    ///
    /// # Errors
    ///
    /// The database cannot be opened.
    pub fn new(
        node: String,
        path: PathBuf,
        conn: Connection,
        mail_host: &str,
    ) -> Result<Self, TailError> {
        Ok(Self {
            node,
            path,
            conn,
            expressions: Expressions::new(mail_host)?,
        })
    }

    /// Reads up to `limit` complete lines from the checkpoint and commits what they produced
    /// with the new checkpoint. A missing `mail.log` reads nothing.
    ///
    /// # Errors
    ///
    /// A cursor gap, an oversized line, or an I/O or database failure; nothing is committed.
    pub fn step(&mut self, limit: usize) -> Result<Progress, TailError> {
        let current = match fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Progress {
                    missing: true,
                    ..Progress::default()
                });
            }
            Err(error) => return Err(error.into()),
        };
        let current_inode = current.ino().to_string();
        let cursor = self.conn.cursor(&self.node)?;
        let tx = self.conn.transaction()?;
        let (inode, position) = match &cursor {
            Some((inode, position)) => (
                inode.clone(),
                u64::try_from(*position).map_err(|_| TailError::Gap("a negative checkpoint"))?,
            ),
            None => (current_inode.clone(), 0),
        };
        let source = if inode == current_inode {
            self.path.clone()
        } else {
            rotated(&self.path, &inode)?.ok_or(TailError::Gap(
                "the checkpointed file is no longer beside mail.log",
            ))?
        };
        let source_meta = fs::metadata(&source)?;
        if source_meta.len() < position {
            return Err(TailError::Gap("the log is shorter than its checkpoint"));
        }

        let mut reader = BufReader::new(File::open(&source)?);
        reader.seek(SeekFrom::Start(position))?;
        let mut progress = Progress::default();
        let mut offset = position;
        let mut buffer = Vec::new();
        while progress.lines < limit {
            buffer.clear();
            let read = (&mut reader)
                .take(MAX_LINE + 1)
                .read_until(b'\n', &mut buffer)?;
            if read == 0 {
                break;
            }
            if buffer.last() != Some(&b'\n') {
                if u64::try_from(read).unwrap_or(u64::MAX) > MAX_LINE {
                    return Err(TailError::Oversized);
                }
                // A partial line: the writer has not finished it; it is read again next time.
                break;
            }
            let identity = format!(
                "{}:{inode}:{offset}:{}",
                self.node,
                crypto::sha256_hex(&buffer)
            );
            offset += u64::try_from(read).unwrap_or(0);
            let line = String::from_utf8_lossy(&buffer);
            parse(
                &self.expressions,
                &tx,
                line.trim_end_matches(['\n', '\r']),
                &identity,
                &mut progress,
            )?;
            progress.lines += 1;
        }

        let quiet = source_meta
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= QUIET);
        // A drained rotated file is left only once quiet: its writer may still be appending
        // until it reopens the new file.
        let leave = inode != current_inode && offset == source_meta.len() && quiet;
        let (next_inode, next_offset) = if leave {
            (current_inode.clone(), 0)
        } else {
            (inode.clone(), offset)
        };
        progress.lag_bytes = if next_inode == current_inode {
            current.len().saturating_sub(next_offset)
        } else {
            source_meta
                .len()
                .saturating_sub(offset)
                .saturating_add(current.len())
        };
        let checkpoint = (next_inode, i64::try_from(next_offset).unwrap_or(i64::MAX));
        if cursor.as_ref() != Some(&checkpoint) {
            tx.capture(crate::queue::Record::Cursor {
                node: self.node.clone(),
                inode: checkpoint.0,
                position: checkpoint.1,
            })?;
        }
        tx.commit()?;
        Ok(progress)
    }
}

/// The file beside `path` whose name starts with `path`'s and whose inode is `inode`;
/// compressed rotations are not read.
fn rotated(path: &Path, inode: &str) -> io::Result<Option<PathBuf>> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Ok(None);
    };
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(candidate) = file_name.to_str() else {
            continue;
        };
        if !candidate.starts_with(name) || candidate.ends_with(".gz") {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_file() && meta.ino().to_string() == inode {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Applies one line: correlation rows, or one event.
fn parse(
    expressions: &Expressions,
    tx: &Transaction<'_>,
    line: &str,
    identity: &str,
    progress: &mut Progress,
) -> Result<(), TailError> {
    let Some(captures) = expressions.line.captures(line) else {
        return Ok(());
    };
    let (Some(stamp), Some(service), Some(queue), Some(detail)) = (
        captures.get(1),
        captures.get(2),
        captures.get(3),
        captures.get(4),
    ) else {
        return Ok(());
    };
    // Only timestamps with an offset: a local time cannot be placed.
    let Ok(observed) = stamp.as_str().parse::<Timestamp>() else {
        return Ok(());
    };
    let at = db::seconds(observed);
    let (service, queue, detail) = (service.as_str(), queue.as_str(), detail.as_str());

    if service.ends_with("smtpd") {
        if let Some(user) = expressions.auth.captures(detail).and_then(|c| c.get(1)) {
            tx.prepare_cached(
                "INSERT INTO submissions (queue_id, username, authenticated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (queue_id) DO UPDATE SET username = excluded.username,
                        authenticated_at = excluded.authenticated_at, internet_message_id = NULL",
            )?
            .execute(params![queue, user.as_str(), at])?;
        }
    } else if service.rsplit('/').next() == Some("cleanup") {
        if let Some(id) = expressions
            .message_id
            .captures(detail)
            .and_then(|c| c.get(1))
        {
            // Never inherit a login from a reused queue id.
            tx.prepare_cached(
                "UPDATE submissions SET internet_message_id = ?1
                  WHERE queue_id = ?2 AND authenticated_at BETWEEN ?3 AND ?4",
            )?
            .execute(params![id.as_str(), queue, at - 300.0, at + 1.0])?;
        }
    } else if matches!(service, "smtp" | "lmtp" | "local" | "pipe" | "error") {
        deliver(expressions, tx, queue, detail, observed, identity, progress)?;
    } else if service == "qmgr" && detail == "removed" {
        tx.prepare_cached("DELETE FROM submissions WHERE queue_id = ?1")?
            .execute([queue])?;
    } else if service == "qmgr"
        && let Some(token) = expressions
            .return_path
            .captures(detail)
            .and_then(|c| c.get(1))
    {
        // Only an authenticated submission has a row: its login owns the return path, and the
        // first submission to use a token keeps it.
        progress.returns += tx
            .prepare_cached(
                "INSERT INTO returns (token, username, internet_message_id, queue_id, created)
                 SELECT ?1, username, internet_message_id, queue_id, ?2 FROM submissions WHERE queue_id = ?3
                 ON CONFLICT (token) DO NOTHING",
            )?
            .execute(params![token.as_str(), at, queue])?;
    }
    Ok(())
}

/// One delivery line of a correlated queue id becomes an event pinned to its login's route.
fn deliver(
    expressions: &Expressions,
    tx: &Transaction<'_>,
    queue: &str,
    detail: &str,
    observed: Timestamp,
    identity: &str,
    progress: &mut Progress,
) -> Result<(), TailError> {
    let Some(delivery) = expressions.delivery.captures(detail) else {
        return Ok(());
    };
    let (Some(status), Some(outcome), Some(diagnostic)) =
        (delivery.get(1), delivery.get(2), delivery.get(3))
    else {
        return Ok(());
    };
    let (mut original, mut final_recipient) = (None, None);
    for found in expressions.recipient.captures_iter(detail) {
        match (found.get(1).map(|m| m.as_str()), found.get(2)) {
            (Some("orig_to"), Some(address)) => original = Some(address.as_str()),
            (Some(_), Some(address)) => final_recipient = Some(address.as_str()),
            _ => {}
        }
    }
    let Some(recipient) = original.or(final_recipient) else {
        return Ok(());
    };
    let submission: Option<(Option<String>, String)> = tx
        .prepare_cached(
            "SELECT internet_message_id, username FROM submissions WHERE queue_id = ?1",
        )?
        .query_row([queue], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((Some(message_id), username)) = submission else {
        return Ok(());
    };
    let event_id = crypto::sha256_hex(identity.as_bytes());
    let journaled = events::journal(
        tx,
        &Evidence {
            event_id: &event_id,
            internet_message_id: Some(&message_id),
            username: &username,
            recipient: Some(recipient),
            queue_id: Some(queue),
            kind: match outcome.as_str() {
                "sent" => "delivered",
                "deferred" => "deferred",
                _ => "bounced",
            },
            enhanced_status: Some(status.as_str()),
            detail: Some(diagnostic.as_str().chars().take(2000).collect()),
            provenance: "smtp_reply",
            observed_at: observed.to_string(),
        },
    )?;
    match journaled {
        Journaled::Inserted => progress.events += 1,
        Journaled::Unrouted => progress.unrouted += 1,
        Journaled::Duplicate => {}
    }
    Ok(())
}

/// Removes correlation rows and return paths older than seven days; returns how many.
///
/// # Errors
///
/// A delete fails.
pub fn prune(conn: &Connection) -> db::Result<usize> {
    let cutoff = db::now() - SUBMISSION_TTL_SECONDS;
    let submissions = conn.execute(
        "DELETE FROM submissions WHERE authenticated_at < ?1",
        [cutoff],
    )?;
    let returns = conn.execute("DELETE FROM returns WHERE created < ?1", [cutoff])?;
    Ok(submissions + returns)
}

struct Instruments {
    lines: Counter<u64>,
    events: Counter<u64>,
    unrouted: Counter<u64>,
    errors: Counter<u64>,
    lag: Gauge<u64>,
}

impl Instruments {
    fn new() -> Self {
        let meter = telemetry::meter();
        Self {
            lines: meter
                .u64_counter("norbelys_mta_tail_lines")
                .with_description("mail.log lines read")
                .build(),
            events: meter
                .u64_counter("norbelys_mta_events_captured")
                .with_description("Delivery events journaled")
                .build(),
            unrouted: meter
                .u64_counter("norbelys_mta_events_unrouted")
                .with_description("Delivery lines of logins without a route, not journaled")
                .build(),
            errors: meter
                .u64_counter("norbelys_mta_tail_errors")
                .with_description("Tail steps that failed")
                .build(),
            lag: meter
                .u64_gauge("norbelys_mta_tail_lag_bytes")
                .with_unit("By")
                .with_description("Bytes of mail.log not yet read")
                .build(),
        }
    }
}

/// Runs the tail until shutdown: at once again after a full step, every second when caught up,
/// every ten seconds after an error (logged when it changes).
///
/// # Errors
///
/// The blocking task running a step panicked.
pub async fn run(mut tail: Tail, mut shutdown: Shutdown) -> anyhow::Result<()> {
    let instruments = Instruments::new();
    let mut last_error: Option<String> = None;
    let mut warned_missing = false;
    loop {
        let started = Instant::now();
        let (back, result) = tokio::task::spawn_blocking(move || {
            let result = tail.step(LIMIT);
            (tail, result)
        })
        .await?;
        tail = back;
        let wait = match result {
            Ok(progress) => {
                // A wrong path would otherwise read nothing, silently, forever.
                if progress.missing && !warned_missing {
                    tracing::warn!(path = %tail.path.display(), "mail.log does not exist; nothing is read until it does");
                }
                warned_missing = progress.missing;
                if last_error.take().is_some() {
                    tracing::info!("mail.log is read again");
                }
                instruments
                    .lines
                    .add(u64::try_from(progress.lines).unwrap_or(0), &[]);
                instruments
                    .events
                    .add(u64::try_from(progress.events).unwrap_or(0), &[]);
                instruments
                    .unrouted
                    .add(u64::try_from(progress.unrouted).unwrap_or(0), &[]);
                instruments.lag.record(progress.lag_bytes, &[]);
                if progress.lines > 0 {
                    let duration_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    telemetry::unit(telemetry::Event::Tail);
                    tracing::info!(
                        event = "mta.tail",
                        outcome = "read",
                        lines = progress.lines,
                        events = progress.events,
                        unrouted = progress.unrouted,
                        returns = progress.returns,
                        lag_bytes = progress.lag_bytes,
                        duration_ms,
                        "mta.tail"
                    );
                }
                if progress.lines == LIMIT {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                }
            }
            Err(error) => {
                instruments
                    .errors
                    .add(1, &[KeyValue::new("kind", error_kind(&error))]);
                let message = error.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    telemetry::unit(telemetry::Event::Tail);
                    tracing::error!(event = "mta.tail", outcome = "failed", error = %message, "mta.tail");
                    last_error = Some(message);
                }
                Duration::from_secs(10)
            }
        };
        if !shutdown.sleep(wait).await {
            return Ok(());
        }
    }
}

fn error_kind(error: &TailError) -> &'static str {
    match error {
        TailError::Gap(_) => "gap",
        TailError::Oversized => "oversized",
        TailError::Io(_) => "io",
        TailError::Database(_) => "database",
        TailError::Regex(_) => "internal",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::time::SystemTime;

    use super::*;
    use crate::testing::{TempDir, memory};

    const T: &str = "2026-10-02T04:06:00.000000+00:00 mail postfix";

    /// rsyslog lines for one queue id: the authenticated submission, the cleanup, then the
    /// given delivery lines.
    fn submission(queue: &str, login: &str, deliveries: &[&str]) -> String {
        let mut text = format!(
            "{T}/submission/smtpd[1]: {queue}: client=core[10.0.0.2], sasl_method=PLAIN, sasl_username={login}\n\
             {T}/cleanup[2]: {queue}: message-id=<{queue}@example.com>\n"
        );
        for delivery in deliveries {
            text.push_str(&format!("{T}/smtp[3]: {queue}: {delivery}\n"));
        }
        text
    }

    const SENT: &str = "to=<alice@example.net>, relay=mx[192.0.2.20]:25, delay=1, dsn=2.0.0, status=sent (250 2.0.0 OK)";

    /// A database where `relay@example.com` reports to route `pwh_1`.
    fn routed() -> Connection {
        let conn = memory();
        conn.execute_batch(
            "INSERT INTO domains (name, ownership_token, dkim_selector, created_at) VALUES ('example.com', 't', 's', 'now');
             INSERT INTO accounts (username, domain, kind, rate_class, created_at)
               VALUES ('relay@example.com', 'example.com', 'relay', 'relay', 'now'),
                      ('owner@example.com', 'example.com', 'mailbox', 'customer', 'now');
             INSERT INTO routes VALUES ('pwh_1', 'https://localhost/webhooks/pwh_1', x'00', 'now');
             INSERT INTO account_routes VALUES ('relay@example.com', 'pwh_1');",
        )
        .unwrap();
        conn
    }

    /// Feeds `text` through the line parser as the tail would, one transaction.
    fn feed(conn: &mut Connection, text: &str) -> Progress {
        let expressions = Expressions::new("mail.example.com").unwrap();
        let tx = conn.transaction().unwrap();
        let mut progress = Progress::default();
        for (offset, line) in text.lines().enumerate() {
            parse(
                &expressions,
                &tx,
                line,
                &format!("test:{offset}:{line}"),
                &mut progress,
            )
            .unwrap();
        }
        tx.commit().unwrap();
        progress
    }

    fn events(conn: &Connection) -> Vec<serde_json::Value> {
        conn.prepare("SELECT payload FROM events ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
            .collect()
    }

    /// Each expression matches the Postfix lines it is for and nothing else: the line shape,
    /// the authenticated login, the Message-ID, a delivery outcome with its enhanced status and
    /// diagnostic, and both recipient forms.
    #[test]
    fn expressions_read_postfix_lines() {
        let e = Expressions::new("mail.example.com").unwrap();
        let line = e
            .line
            .captures("2026-10-02T04:06:00+00:00 mail postfix/submission/smtpd[12]: 4ZQ1: client=x")
            .unwrap();
        assert_eq!(
            (&line[2], &line[3], &line[4]),
            ("submission/smtpd", "4ZQ1", "client=x")
        );
        assert!(
            e.line
                .captures("Oct  2 04:06:00 mail postfix/smtpd[12]: 4ZQ1: client=x")
                .is_none()
        );
        assert!(
            e.line
                .captures("2026-10-02T04:06:00+00:00 mail dovecot[12]: 4ZQ1: x")
                .is_none()
        );

        assert_eq!(
            &e.auth
                .captures("client=a[1.2.3.4], sasl_method=PLAIN, sasl_username=a@b.c")
                .unwrap()[1],
            "a@b.c"
        );
        assert!(e.auth.captures("client=a[1.2.3.4]").is_none());

        assert_eq!(
            &e.message_id.captures("message-id=<x.y@b.c>").unwrap()[1],
            "<x.y@b.c>"
        );
        assert!(e.message_id.captures("message-id=x.y@b.c").is_none());

        let bounced = e
            .delivery
            .captures("to=<b@x.y>, relay=mx, delay=1, dsn=5.1.1, status=bounced (550 5.1.1 (user) unknown)")
            .unwrap();
        assert_eq!(
            (&bounced[1], &bounced[2], &bounced[3]),
            ("5.1.1", "bounced", "550 5.1.1 (user) unknown")
        );
        assert!(
            e.delivery
                .captures("to=<b@x.y>, dsn=2.0.0, status=expired (x)")
                .is_none()
        );

        let recipients: Vec<_> = e
            .recipient
            .captures_iter("to=<final@x.y>, orig_to=<given@x.y>, relay=mx")
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect();
        assert_eq!(
            recipients,
            [
                ("to".to_owned(), "final@x.y".to_owned()),
                ("orig_to".to_owned(), "given@x.y".to_owned())
            ]
        );
    }

    /// An authenticated submission becomes one event per delivery line, with the contract's
    /// fields, pinned to its login's route; cached failures from `error` count too; `qmgr …
    /// removed` ends the queue id; the original recipient is preferred. A provider's policy code
    /// in a deferred diagnostic is retained for the message history, not made into a bounce.
    #[test]
    fn turns_authenticated_deliveries_into_events() {
        let mut conn = routed();
        let mut text = submission(
            "4ZQ1",
            "relay@example.com",
            &[
                SENT,
                "to=<b@example.net>, orig_to=<bob@example.net>, relay=mx, dsn=5.1.1, status=bounced (550 5.1.1 unknown)",
            ],
        );
        text.push_str(&format!("{T}/error[4]: 4ZQ1: to=<c@example.net>, relay=none, dsn=4.7.1, status=deferred (550 5.7.1 unusual invalid recipients (JFE050004))\n"));
        text.push_str(&format!("{T}/qmgr[5]: 4ZQ1: removed\n"));
        let progress = feed(&mut conn, &text);

        assert_eq!((progress.events, progress.unrouted), (3, 0));
        let events = events(&conn);
        let first = events[0].as_object().unwrap();
        assert_eq!(first["internet_message_id"], "<4ZQ1@example.com>");
        assert_eq!(first["username"], "relay@example.com");
        assert_eq!(first["recipient"], "alice@example.net");
        assert_eq!(first["queue_id"], "4ZQ1");
        assert_eq!(first["kind"], "delivered");
        assert_eq!(first["enhanced_status"], "2.0.0");
        assert_eq!(first["detail"], "250 2.0.0 OK");
        assert_eq!(first["provenance"], "smtp_reply");
        assert_eq!(first["observed_at"], "2026-10-02T04:06:00Z");
        assert_eq!(first["event_id"].as_str().unwrap().len(), 64);
        assert_eq!(
            (events[1]["kind"].as_str(), events[1]["recipient"].as_str()),
            (Some("bounced"), Some("bob@example.net"))
        );
        assert_eq!(events[2]["kind"], "deferred");
        assert_eq!(
            events[2]["detail"],
            "550 5.7.1 unusual invalid recipients (JFE050004)"
        );
        let routes: i64 = conn
            .query_row(
                "SELECT count(*) FROM events WHERE provider_webhook_id = 'pwh_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let submissions: i64 = conn
            .query_row("SELECT count(*) FROM submissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!((routes, submissions), (3, 0));
    }

    /// Mail that was not submitted with a login produces nothing (a tenant is never guessed),
    /// and a login without a route is counted but not journaled.
    #[test]
    fn never_guesses_a_tenant() {
        let mut conn = routed();
        let inbound = format!(
            "{T}/smtpd[1]: 4ZQ2: client=mx.example.org[192.0.2.30]\n{T}/cleanup[2]: 4ZQ2: message-id=<in@x.y>\n{T}/smtp[3]: 4ZQ2: {SENT}\n"
        );
        let unrouted = submission("4ZQ3", "owner@example.com", &[SENT]);
        let progress = feed(&mut conn, &format!("{inbound}{unrouted}"));
        assert_eq!((progress.events, progress.unrouted), (0, 1));
    }

    /// A reused queue id never inherits an earlier login's message: the Message-ID binds only
    /// within 300 seconds of the authentication, so a late line produces no event.
    #[test]
    fn binds_a_message_id_only_near_its_authentication() {
        let mut conn = routed();
        let late = "2026-10-02T04:11:01.000000+00:00 mail postfix";
        let text = format!(
            "{T}/submission/smtpd[1]: 4ZQ4: client=core, sasl_username=relay@example.com\n\
             {late}/cleanup[2]: 4ZQ4: message-id=<late@x.y>\n{late}/smtp[3]: 4ZQ4: {SENT}\n"
        );
        assert_eq!(feed(&mut conn, &text).events, 0);
    }

    /// The `qmgr` line of an authenticated submission records its VERP return path with the
    /// submission's login, Message-ID and queue id; the same return path in unauthenticated
    /// mail, or one on another host, records nothing, so no stranger can claim a token.
    #[test]
    fn records_the_return_paths_of_authenticated_submissions() {
        let mut conn = routed();
        let from = |queue: &str, token: &str, host: &str| {
            format!(
                "{T}/qmgr[4]: {queue}: from=<bounce+{token}@{host}>, size=1234, nrcpt=1 (queue active)\n"
            )
        };
        let ours = "0123456789abcdef0123456789abcdef";
        let mut text = submission("4ZQ8", "relay@example.com", &[]);
        text.push_str(&from("4ZQ8", ours, "mail.example.com"));
        text.push_str(&format!(
            "{T}/smtpd[1]: 4ZQ9: client=mx.example.org[192.0.2.30]\n"
        ));
        text.push_str(&from("4ZQ9", "1111111111111111", "mail.example.com"));
        text.push_str(&submission("4ZQA", "relay@example.com", &[]));
        text.push_str(&from("4ZQA", "2222222222222222", "other.example.com"));
        assert_eq!(feed(&mut conn, &text).returns, 1);
        let recorded: (String, String, String, String) = conn
            .query_row(
                "SELECT token, username, internet_message_id, queue_id FROM returns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            recorded,
            (
                ours.to_owned(),
                "relay@example.com".to_owned(),
                "<4ZQ8@example.com>".to_owned(),
                "4ZQ8".to_owned()
            )
        );
    }

    /// Correlation rows and return paths older than seven days are removed; younger ones stay.
    #[test]
    fn prunes_week_old_correlations() {
        let conn = memory();
        let (old, new) = (db::now() - SUBMISSION_TTL_SECONDS - 1.0, db::now() - 60.0);
        conn.execute(
            "INSERT INTO submissions VALUES ('old', 'a@b.c', NULL, ?1), ('new', 'a@b.c', NULL, ?2)",
            params![old, new],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO returns VALUES ('old', 'a@b.c', NULL, 'q1', ?1), ('new', 'a@b.c', NULL, 'q2', ?2)",
            params![old, new],
        )
        .unwrap();
        assert_eq!(prune(&conn).unwrap(), 2);
    }

    fn tail(dir: &TempDir) -> Tail {
        Tail::new(
            "test".to_owned(),
            dir.join("mail.log"),
            db::open(&dir.join("smtp.sqlite")).unwrap(),
            "mail.example.com",
        )
        .unwrap()
    }

    fn append(path: &Path, text: &str) {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn routed_file(dir: &TempDir) -> Connection {
        let conn = db::open(&dir.join("smtp.sqlite")).unwrap();
        conn.execute_batch(
            "INSERT INTO domains (name, ownership_token, dkim_selector, created_at) VALUES ('example.com', 't', 's', 'now');
             INSERT INTO accounts (username, domain, kind, rate_class, created_at) VALUES ('relay@example.com', 'example.com', 'relay', 'relay', 'now');
             INSERT INTO routes VALUES ('pwh_1', 'https://localhost/webhooks/pwh_1', x'00', 'now');
             INSERT INTO account_routes VALUES ('relay@example.com', 'pwh_1');",
        )
        .unwrap();
        conn
    }

    /// A missing log reads nothing and says so; a partial last line waits until its writer
    /// finishes it; the checkpoint survives a restart, and lines read again (a crash before
    /// the commit) insert nothing twice.
    #[test]
    fn checkpoints_complete_lines_only_and_replays_nothing() {
        let dir = TempDir::new();
        let conn = routed_file(&dir);
        let log = dir.join("mail.log");
        assert!(tail(&dir).step(LIMIT).unwrap().missing);

        let text = submission("4ZQ5", "relay@example.com", &[SENT]);
        let (complete, partial) = text.split_at(text.len() - 10);
        append(&log, complete);
        assert_eq!(tail(&dir).step(LIMIT).unwrap().events, 0);
        append(&log, partial);
        assert_eq!(tail(&dir).step(LIMIT).unwrap().events, 1);

        conn.execute("DELETE FROM cursors", []).unwrap();
        let replay = tail(&dir).step(LIMIT).unwrap();
        assert_eq!((replay.lines, replay.events), (3, 0));
    }

    /// After a rename rotation the old file is drained first, then left once quiet, and the
    /// new file is read from its start: no line is lost or read twice.
    #[test]
    fn drains_a_rotated_log_before_the_new_one() {
        let dir = TempDir::new();
        let conn = routed_file(&dir);
        let log = dir.join("mail.log");
        let first = submission("4ZQ6", "relay@example.com", &[SENT]);
        append(&log, &first[..first.find("/smtp[").unwrap()]);
        let mut reader = tail(&dir);
        reader.step(LIMIT).unwrap();
        append(&log, &first[first.find("/smtp[").unwrap()..]);
        fs::rename(&log, dir.join("mail.log.1")).unwrap();
        append(&log, &submission("4ZQ7", "relay@example.com", &[SENT]));

        // Not quiet yet: the rotated file is kept even though it is drained.
        assert_eq!(reader.step(LIMIT).unwrap().events, 1);
        let rotated = File::options()
            .write(true)
            .open(dir.join("mail.log.1"))
            .unwrap();
        rotated.set_modified(SystemTime::now() - QUIET * 2).unwrap();
        assert_eq!(reader.step(LIMIT).unwrap().events, 0);
        assert_eq!(reader.step(LIMIT).unwrap().events, 1);
        let total: i64 = conn
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    /// The tail stops rather than skip evidence: when the checkpointed file is gone, when the
    /// log is shorter than the checkpoint, and on a line far longer than Postfix writes.
    #[test]
    fn stops_on_gaps_and_oversized_lines() {
        let dir = TempDir::new();
        let _conn = routed_file(&dir);
        let log = dir.join("mail.log");
        append(&log, "one line\n");
        tail(&dir).step(LIMIT).unwrap();

        fs::write(&log, "").unwrap();
        assert!(matches!(tail(&dir).step(LIMIT), Err(TailError::Gap(_))));

        // Moved out of the directory, not deleted: its inode stays taken, so the new file
        // cannot reuse it.
        let elsewhere = TempDir::new();
        fs::rename(&log, elsewhere.join("mail.log")).unwrap();
        append(&log, "a new file\n");
        assert!(matches!(tail(&dir).step(LIMIT), Err(TailError::Gap(_))));

        let other = TempDir::new();
        let _other_conn = routed_file(&other);
        append(&other.join("mail.log"), &"x".repeat(70_000));
        assert!(matches!(
            tail(&other).step(LIMIT),
            Err(TailError::Oversized)
        ));
    }
}
