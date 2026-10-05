//! `cargo xtask gates <gate>`: the crash gates. Each runs the roles as processes on a database of
//! its own, kills one with SIGKILL at a chosen moment, starts it again, reads from the database
//! what recovery left, and judges it with the decisions below.
//!
//! | Gate | What happens | Holds when |
//! |---|---|---|
//! | `sender-crash` | a mailbox submits to the gate's SMTP server, which never answers the end of a message ([`smtp`]); once the server has the message's `DATA`, the sender is killed, the message's lease runs out and a new sender starts | the message is `uncertain` and out of the queue, its one attempt closed `uncertain` with its reservation consumed, `message.uncertain` told once, never submitted again; messages and the connection's ledger conserved ([`sender_findings`], [`conservation_findings`]) |
//! | `worker-crash` | an import of 200,000 people (a hundred chunks) is posted; once its job has recorded chunks, the worker is killed in the middle of one, the job's lease runs out and a new worker starts | the job is recovered with one failed attempt ([`job_recovered`]), resumes from its checkpoint and completes, every row imported exactly once, its lane slot released and all lane reservations matching running jobs ([`worker_findings`]) |
//! | `stall-smtp` | the stalling SMTP server alone, to try a sender against it by hand | — |
//! | `all` | every gate, one after the other | each holds |
//!
//! A gate builds the server, creates `norbelys_gates_<gate>_<pid>` on the local server that
//! the explicit `TEST_DATABASE_URL` names, acknowledged with `NORBELYS_TEST_DATABASE_DISPOSABLE=1`.
//! It creates the schema with external SQLx tooling and runs each role with its own login
//! and the development password that `db migrate`
//! sets ([`environment`]). A lease is made to run out with one `UPDATE` rather than waited for, as
//! the server's own tests do, so a gate takes a minute or two. The sender's gate needs DNS: the
//! sender's preflight resolves the recipient's domain (`gmail.com`, which has MX records), and
//! nothing is ever delivered to it, the only server the mailbox reaches being the gate's own.
//! Ports 3961 to 3964 and 3966 on the loopback interface.

mod environment;
mod smtp;

use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use sqlx::types::Uuid;

use environment::{Body, Environment, note};

/// Where the sender's gate serves its stalling SMTP server.
pub const SMTP_PORT: u16 = 3966;
/// The recipient of the sender's gate: its domain has MX records, so preflight lets the message
/// through; nothing is ever delivered to it.
const RECIPIENT: &str = "crash-gate@gmail.com";
/// Rows of the worker's gate's import: a hundred chunks of 2,000, so the worker is killed in the
/// middle of the job rather than after it.
const ROWS: i64 = 200_000;

/// What the sender's gate reads about the message once it is recovered.
const RECOVERED_MESSAGE: &str = "
SELECT m.state,
       (SELECT count(*) FROM delivery_queue q WHERE q.workspace_id = m.workspace_id AND q.message_id = m.id),
       coalesce((SELECT array_agg(coalesce(a.outcome, 'open') || ' ' || a.quota_state ORDER BY a.attempt_number)
                   FROM attempts a WHERE a.workspace_id = m.workspace_id AND a.message_id = m.id), '{}'),
       (SELECT count(*) FROM outbox_events o WHERE o.type = 'message.uncertain' AND o.subject_id = m.id)
  FROM messages m
 WHERE m.id = $1";

/// The counts conservation is judged on, over the gate's whole database.
const CONSERVATION: &str = "
SELECT (SELECT count(*) FROM messages),
       (SELECT count(*) FROM messages WHERE state IN ('sent', 'failed', 'cancelled', 'uncertain', 'suppressed')),
       (SELECT count(*) FROM delivery_queue),
       (SELECT count(*) FROM delivery_queue q JOIN messages m ON (m.workspace_id, m.id) = (q.workspace_id, q.message_id)
         WHERE m.state IN ('sent', 'failed', 'cancelled', 'uncertain', 'suppressed')),
       (SELECT count(*) FROM connection_usage u
         WHERE u.reserved <> (SELECT count(*) FROM attempts a
                               WHERE (a.workspace_id, a.connection_id, a.reserved_day) = (u.workspace_id, u.connection_id, u.day)
                                 AND a.quota_state = 'reserved')
            OR u.used <> (SELECT count(*) FROM attempts a
                           WHERE (a.workspace_id, a.connection_id, a.reserved_day) = (u.workspace_id, u.connection_id, u.day)
                             AND a.quota_state = 'consumed'))";

/// What the worker's gate reads once the import completed.
const COMPLETED_IMPORT: &str = "
SELECT i.imported, i.skipped, i.invalid,
       (SELECT count(*) FROM people p WHERE p.workspace_id = i.workspace_id),
       j.state, j.attempts, j.claims,
       (SELECT coalesce(sum(l.running), 0) FROM job_lanes l
         WHERE l.workspace_id = i.workspace_id AND l.queue = j.queue)::bigint,
       (SELECT count(*) FROM job_lanes l WHERE l.running <>
          (SELECT count(*) FROM jobs active WHERE active.workspace_id = l.workspace_id
             AND active.queue = l.queue AND active.state = 'running'))
  FROM imports i JOIN jobs j ON j.workspace_id = i.workspace_id AND j.id = i.job_id
 WHERE i.id = $1";

/// What the sender's gate reads once a new sender has recovered the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    /// The message's state.
    pub state: String,
    /// Its queue rows left.
    pub queued: i64,
    /// Its attempts in order, each as `<outcome> <quota state>`.
    pub attempts: Vec<String>,
    /// The `message.uncertain` events told about it.
    pub told: i64,
    /// The submissions the SMTP server ever received.
    pub submissions: usize,
}

/// What counts as recovered from a sender lost in the middle of a submission: the provider may
/// have the message, so it is `uncertain` and out of the queue; its one attempt is closed
/// `uncertain` with its reservation consumed, conservatively; `message.uncertain` is told once;
/// and it is never submitted again. Answers what does not hold.
#[must_use]
pub fn sender_findings(recovered: &Recovered) -> Vec<String> {
    let mut findings = Vec::new();
    if recovered.state != "uncertain" {
        findings.push(format!(
            "the message is `{}`, not `uncertain`",
            recovered.state
        ));
    }
    if recovered.queued != 0 {
        findings.push(format!(
            "the message still has {} queue row(s)",
            recovered.queued
        ));
    }
    if recovered.attempts != ["uncertain consumed"] {
        findings.push(format!(
            "its attempts are {:?}, not one closed `uncertain` with its reservation consumed",
            recovered.attempts
        ));
    }
    if recovered.told != 1 {
        findings.push(format!(
            "`message.uncertain` was told {} time(s), not once",
            recovered.told
        ));
    }
    if recovered.submissions != 1 {
        findings.push(format!(
            "the SMTP server received {} submission(s) of the message, not one",
            recovered.submissions
        ));
    }
    findings
}

/// The counts the conservation of messages and of the connections' ledger is judged on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Messages accepted.
    pub messages: i64,
    /// Of them, terminal: sent, failed, cancelled, uncertain or suppressed.
    pub terminal: i64,
    /// Queue rows: the messages still active.
    pub queued: i64,
    /// Queue rows of terminal messages.
    pub terminal_queued: i64,
    /// Connection ledger rows whose reserved or used units differ from their attempts.
    pub ledger_mismatches: i64,
}

/// Conservation: every accepted message is terminal or active, never both and never neither
/// (`accepted − terminal − active = 0`), and each connection's ledger row holds exactly what its
/// attempts reserved and consumed. Answers what does not hold.
#[must_use]
pub fn conservation_findings(counts: &Counts) -> Vec<String> {
    let mut findings = Vec::new();
    let unbalanced = counts
        .messages
        .saturating_sub(counts.terminal)
        .saturating_sub(counts.queued);
    if unbalanced != 0 {
        findings.push(format!(
            "accepted − terminal − active = {unbalanced} ({} accepted, {} terminal, {} queued)",
            counts.messages, counts.terminal, counts.queued
        ));
    }
    if counts.terminal_queued != 0 {
        findings.push(format!(
            "{} terminal message(s) still queued",
            counts.terminal_queued
        ));
    }
    if counts.ledger_mismatches != 0 {
        findings.push(format!(
            "{} connection ledger row(s) differ from their attempts",
            counts.ledger_mismatches
        ));
    }
    findings
}

/// Whether the worker was killed in the middle of the import: some of its `rows` recorded, not
/// all. Killed before or after, the gate would prove nothing about a chunk cut short.
#[must_use]
pub fn killed_mid_import(imported: i64, rows: i64) -> bool {
    imported > 0 && imported < rows
}

/// Whether a new worker has recovered the job of the killed one: the lost run counted as one
/// failed attempt, and the job available again, or, its backoff short, already claimed again or
/// finished.
#[must_use]
pub fn job_recovered(state: &str, attempts: i16) -> bool {
    attempts == 1 && matches!(state, "available" | "running" | "completed")
}

/// What the worker's gate reads once the import completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// The import's rows imported, skipped and invalid.
    pub imported: i64,
    pub skipped: i64,
    pub invalid: i64,
    /// The people of the workspace.
    pub people: i64,
    /// The job's state, failed attempts and claims.
    pub job_state: String,
    pub attempts: i16,
    pub claims: i32,
    /// Slots held in the import's workspace and queue, where this gate enqueues only one job.
    pub lanes_running: i64,
    /// Lanes whose reservations disagree with their running jobs, across all workspaces.
    pub lane_mismatches: i64,
}

/// What counts as a worker crash survived by an import of `rows` rows: every row imported exactly
/// once (none skipped or invalid, and as many people as rows: the chunk cut short was rolled back
/// whole and done again, and none was counted twice); the job completed with the one failed
/// attempt of the lost run, claimed again after the crash; and its lane slot is freed. Other maintenance jobs may be active, but every lane
/// must hold exactly as many slots as it has running jobs.
/// Answers what does not hold.
#[must_use]
pub fn worker_findings(imported: &Imported, rows: i64) -> Vec<String> {
    let mut findings = Vec::new();
    if (imported.imported, imported.skipped, imported.invalid) != (rows, 0, 0) {
        findings.push(format!(
            "the import counted {} imported, {} skipped, {} invalid, not {rows} imported",
            imported.imported, imported.skipped, imported.invalid
        ));
    }
    if imported.people != rows {
        findings.push(format!(
            "the workspace has {} people, not {rows}",
            imported.people
        ));
    }
    if (imported.job_state.as_str(), imported.attempts) != ("completed", 1) {
        findings.push(format!(
            "the job is `{}` with {} failed attempt(s), not `completed` with the lost run's one",
            imported.job_state, imported.attempts
        ));
    }
    if imported.claims < 2 {
        findings.push(format!(
            "the job was claimed {} time(s): never again after the crash",
            imported.claims
        ));
    }
    if imported.lanes_running != 0 {
        findings.push(format!(
            "{} lane slot(s) still held",
            imported.lanes_running
        ));
    }
    if imported.lane_mismatches != 0 {
        findings.push(format!(
            "{} lane(s) disagree with their running jobs",
            imported.lane_mismatches
        ));
    }
    findings
}

/// The import file of the worker's gate: its header, then `person-<n>@gates.example` for each of
/// `rows` rows.
#[must_use]
pub fn people_csv(rows: i64) -> String {
    let mut csv = String::from("email,given_name\n");
    for n in 1..=rows {
        let _ = writeln!(csv, "person-{n}@gates.example,Person {n}");
    }
    csv
}

/// The uuid behind a wire id (`msg_…`, `imp_…`): the 32 hexadecimal digits after its prefix.
#[must_use]
pub fn uuid_of(id: &str) -> Option<Uuid> {
    let (_, hex) = id.split_once('_')?;
    Uuid::try_parse(hex).ok()
}

/// The `id` of an object the api answered.
fn id_of(object: &Value) -> anyhow::Result<String> {
    object
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("no id in {object}"))
}

/// The counts of conservation, read from the gate's database.
fn counts(gate: &Environment) -> anyhow::Result<Counts> {
    let (messages, terminal, queued, terminal_queued, ledger_mismatches): (
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = gate.block_on(sqlx::query_as(CONSERVATION).fetch_one(gate.pool()?))?;
    Ok(Counts {
        messages,
        terminal,
        queued,
        terminal_queued,
        ledger_mismatches,
    })
}

/// Serves the stalling SMTP server on `127.0.0.1:<port>` until interrupted, to try a sender
/// against it by hand.
///
/// # Errors
///
/// The port is taken.
pub fn stall_smtp(port: u16) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
    note(format!(
        "stalling SMTP server on 127.0.0.1:{port}; interrupt to stop"
    ));
    smtp::serve(&listener, &Arc::new(AtomicUsize::new(0)));
    Ok(())
}

/// Every gate, one after the other, each on its own database; their findings together.
///
/// # Errors
///
/// A gate could not run.
pub fn all(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut findings = sender_crash(root)?;
    findings.extend(worker_crash(root)?);
    Ok(findings)
}

/// The sender's gate: a sender killed in the middle of a submission (see the module).
///
/// # Errors
///
/// The gate could not run, or a step before the decisions did not happen in time.
pub fn sender_crash(root: &Path) -> anyhow::Result<Vec<String>> {
    note("gate sender-crash: a sender killed in the middle of a submission");
    let submissions = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind(("127.0.0.1", SMTP_PORT))
        .with_context(|| format!("cannot listen on 127.0.0.1:{SMTP_PORT}"))?;
    let served = Arc::clone(&submissions);
    std::thread::spawn(move || smtp::serve(&listener, &served));
    let mut gate = Environment::new(root, "sender")?;
    gate.start_api()?;
    gate.start_worker("worker")?;
    gate.start_sender("sender")?;
    gate.workspace("gates-sender")?;

    let mailbox = json!({
        "provider": "smtp",
        "account_email": "gates@mailbox.example",
        "smtp": {"host": "127.0.0.1", "port": SMTP_PORT, "security": "plain", "password": "gates"},
    });
    let connection = id_of(&gate.api("POST", "/v1/connections", Body::Json(&mailbox))?)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let read = gate.api("GET", &format!("/v1/connections/{connection}"), Body::Empty)?;
        if read.get("status").and_then(Value::as_str) == Some("active") {
            break;
        }
        if Instant::now() > deadline {
            bail!(
                "the mailbox was not verified within 60 s: see {}/worker.log",
                gate.work.display()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    note("the mailbox is verified and active");

    let mail = json!({
        "from": "gates@mailbox.example",
        "to": [RECIPIENT],
        "subject": "Crash gate",
        "html": "<p>This submission never finishes.</p>",
    });
    let message = id_of(&gate.api("POST", "/v1/messages", Body::Json(&mail))?)?;
    let id = uuid_of(&message).with_context(|| format!("`{message}` is not a message id"))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while submissions.load(Ordering::SeqCst) == 0 {
        if Instant::now() > deadline {
            bail!(
                "no submission reached the SMTP server within 120 s: see {}/sender.log",
                gate.work.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let under_way: Option<(String, bool)> = gate.block_on(
        sqlx::query_as(
            "SELECT state, submission_started_at IS NOT NULL FROM delivery_queue WHERE message_id = $1",
        )
        .bind(id)
        .fetch_optional(gate.pool()?),
    )?;
    if under_way != Some(("in_flight".to_owned(), true)) {
        bail!("the submission under way is not in flight with its Start's marker: {under_way:?}");
    }
    note("the submission is under way: the server has its DATA, the Start its marker");

    gate.kill("sender")?;
    let state: String = gate.block_on(
        sqlx::query_scalar("SELECT state FROM messages WHERE id = $1")
            .bind(id)
            .fetch_one(gate.pool()?),
    )?;
    if state != "in_flight" {
        bail!("the message is `{state}` after the kill, not `in_flight`");
    }
    // The lease the Start renewed outlives the submission's budget by minutes: let it run out now.
    gate.execute(
        "UPDATE delivery_queue SET lease_expires_at = now() - interval '1 second' WHERE message_id = $1",
        id,
    )?;
    gate.start_sender("sender-again")?;
    if !gate.until(
        Duration::from_secs(60),
        "SELECT state = 'uncertain' FROM messages WHERE id = $1",
        id,
    )? {
        bail!(
            "the new sender did not recover the message within 60 s: see {}/sender-again.log",
            gate.work.display()
        );
    }
    note("the new sender recovered the message");
    // A message whose fate is unknown is never resent automatically: let the new sender sweep and
    // claim a while before counting the submissions.
    std::thread::sleep(Duration::from_secs(20));

    let (state, queued, attempts, told): (String, i64, Vec<String>, i64) = gate.block_on(
        sqlx::query_as(RECOVERED_MESSAGE)
            .bind(id)
            .fetch_one(gate.pool()?),
    )?;
    let mut findings = sender_findings(&Recovered {
        state,
        queued,
        attempts,
        told,
        submissions: submissions.load(Ordering::SeqCst),
    });
    findings.extend(conservation_findings(&counts(&gate)?));
    if findings.is_empty() {
        gate.pass();
        note("sender-crash holds: the message is uncertain, settled once, never resent");
    }
    Ok(findings)
}

/// The worker's gate: a worker killed in the middle of a job's chunk (see the module).
///
/// # Errors
///
/// The gate could not run, or a step before the decisions did not happen in time.
pub fn worker_crash(root: &Path) -> anyhow::Result<Vec<String>> {
    note("gate worker-crash: a worker killed in the middle of a job's chunk");
    let mut gate = Environment::new(root, "worker")?;
    gate.start_api()?;
    gate.start_worker("worker")?;
    gate.workspace("gates-worker")?;

    let file = gate.work.join("people.csv");
    std::fs::write(&file, people_csv(ROWS))
        .with_context(|| format!("cannot write {}", file.display()))?;
    let import = id_of(&gate.api("POST", "/v1/imports", Body::File(&file, "text/csv"))?)?;
    let id = uuid_of(&import).with_context(|| format!("`{import}` is not an import id"))?;
    if !gate.until(
        Duration::from_secs(120),
        "SELECT status = 'processing' AND imported > 0 FROM imports WHERE id = $1",
        id,
    )? {
        bail!(
            "the import's job recorded no chunk within 120 s: see {}/worker.log",
            gate.work.display()
        );
    }

    gate.kill("worker")?;
    let (job, imported): (Option<Uuid>, i64) = gate.block_on(
        sqlx::query_as("SELECT job_id, imported FROM imports WHERE id = $1")
            .bind(id)
            .fetch_one(gate.pool()?),
    )?;
    let job = job.context("the import names no job")?;
    if !killed_mid_import(imported, ROWS) {
        bail!("the worker was not killed in the middle of the import: {imported} of {ROWS} rows");
    }
    note(format!("killed with {imported} of {ROWS} rows imported"));
    let read_job = |gate: &Environment| -> anyhow::Result<(String, i16)> {
        gate.block_on(
            sqlx::query_as("SELECT state, attempts FROM jobs WHERE id = $1")
                .bind(job)
                .fetch_one(gate.pool()?),
        )
        .context("cannot read the import's job")
    };
    let (state, attempts) = read_job(&gate)?;
    if (state.as_str(), attempts) != ("running", 0) {
        bail!("the job is `{state}` with {attempts} failed attempt(s) after the kill, not running");
    }
    // The lease outlives the dead worker by up to a minute: let it run out now.
    gate.execute(
        "UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
        job,
    )?;
    gate.start_worker("worker-again")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (state, attempts) = read_job(&gate)?;
        if job_recovered(&state, attempts) {
            break;
        }
        if Instant::now() > deadline {
            bail!(
                "the new worker did not recover the job within 60 s (`{state}`, {attempts} failed \
                 attempt(s)): see {}/worker-again.log",
                gate.work.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    note("the new worker recovered the job");
    // The recovery waits the job backoff (up to two minutes) before the job runs again: make it
    // due now, and wake its queue as an enqueue would.
    gate.execute(
        "UPDATE jobs SET run_at = now() WHERE id = $1 AND state = 'available'",
        job,
    )?;
    gate.wake("imports")?;
    if !gate.until(
        Duration::from_secs(300),
        "SELECT i.status = 'completed' AND j.state = 'completed' FROM imports i
            JOIN jobs j ON j.workspace_id = i.workspace_id AND j.id = i.job_id WHERE i.id = $1",
        id,
    )? {
        bail!(
            "the import did not complete within 300 s: see {}/worker-again.log",
            gate.work.display()
        );
    }

    let (
        imported,
        skipped,
        invalid,
        people,
        job_state,
        attempts,
        claims,
        lanes_running,
        lane_mismatches,
    ): (i64, i64, i64, i64, String, i16, i32, i64, i64) = gate.block_on(
        sqlx::query_as(COMPLETED_IMPORT)
            .bind(id)
            .fetch_one(gate.pool()?),
    )?;
    let findings = worker_findings(
        &Imported {
            imported,
            skipped,
            invalid,
            people,
            job_state,
            attempts,
            claims,
            lanes_running,
            lane_mismatches,
        },
        ROWS,
    );
    if findings.is_empty() {
        gate.pass();
        note("worker-crash holds: the job resumed and every row was imported exactly once");
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::{
        Counts, Imported, Recovered, conservation_findings, job_recovered, killed_mid_import,
        people_csv, sender_findings, uuid_of, worker_findings,
    };

    fn recovered() -> Recovered {
        Recovered {
            state: "uncertain".to_owned(),
            queued: 0,
            attempts: vec!["uncertain consumed".to_owned()],
            told: 1,
            submissions: 1,
        }
    }

    /// A message recovered as it must be holds; each way recovery can go wrong is found on its
    /// own: still in flight or requeued, still queued, its attempt left open or released, told
    /// twice, or submitted again.
    #[test]
    fn a_lost_submission_counts_as_recovered_only_when_uncertain_settled_and_never_resent() {
        assert!(sender_findings(&recovered()).is_empty());
        let wrong = [
            Recovered {
                state: "queued".to_owned(),
                ..recovered()
            },
            Recovered {
                queued: 1,
                ..recovered()
            },
            Recovered {
                attempts: vec!["open reserved".to_owned()],
                ..recovered()
            },
            Recovered {
                attempts: vec![
                    "uncertain consumed".to_owned(),
                    "accepted consumed".to_owned(),
                ],
                ..recovered()
            },
            Recovered {
                told: 2,
                ..recovered()
            },
            Recovered {
                submissions: 2,
                ..recovered()
            },
        ];
        for recovered in wrong {
            assert_eq!(
                sender_findings(&recovered).len(),
                1,
                "{recovered:?}: {:?}",
                sender_findings(&recovered)
            );
        }
    }

    /// Messages and ledgers are conserved when every message is terminal or queued, never both,
    /// and the ledger matches its attempts; a message lost, counted twice or left queued once
    /// terminal, or a ledger row off its attempts, is found.
    #[test]
    fn conservation_finds_lost_or_doubled_messages_and_ledgers_off_their_attempts() {
        let balanced = Counts {
            messages: 5,
            terminal: 3,
            queued: 2,
            terminal_queued: 0,
            ledger_mismatches: 0,
        };
        assert!(conservation_findings(&balanced).is_empty());
        for counts in [
            Counts {
                queued: 1,
                ..balanced
            },
            Counts {
                queued: 3,
                ..balanced
            },
            Counts {
                terminal_queued: 1,
                ..balanced
            },
            Counts {
                ledger_mismatches: 1,
                ..balanced
            },
        ] {
            assert_eq!(conservation_findings(&counts).len(), 1, "{counts:?}");
        }
    }

    /// The kill must land inside the import, and the job counts as recovered only once the lost
    /// run is one failed attempt, whether it then waits, runs again or has finished.
    #[test]
    fn the_kill_lands_mid_import_and_recovery_counts_one_failed_attempt() {
        assert!(killed_mid_import(2_000, 200_000));
        assert!(!killed_mid_import(0, 200_000));
        assert!(!killed_mid_import(200_000, 200_000));
        for state in ["available", "running", "completed"] {
            assert!(job_recovered(state, 1), "{state}");
            assert!(!job_recovered(state, 0), "{state} with no failed attempt");
            assert!(!job_recovered(state, 2), "{state} with two failed attempts");
        }
        for state in ["failed", "needs_review", "cancelled"] {
            assert!(!job_recovered(state, 1), "{state}");
        }
    }

    /// An import that survived its worker's crash imported every row once, its job completed with
    /// the lost run's failed attempt after a new claim, and no lane slot is held; a row imported
    /// twice, a person missing, a job not resumed or a slot leaked is found.
    #[test]
    fn a_survived_import_counts_every_row_once_and_holds_no_slot() {
        let survived = Imported {
            imported: 10,
            skipped: 0,
            invalid: 0,
            people: 10,
            job_state: "completed".to_owned(),
            attempts: 1,
            claims: 2,
            lanes_running: 0,
            lane_mismatches: 0,
        };
        assert!(worker_findings(&survived, 10).is_empty());
        for imported in [
            Imported {
                imported: 12,
                ..survived.clone()
            },
            Imported {
                people: 9,
                ..survived.clone()
            },
            Imported {
                job_state: "available".to_owned(),
                ..survived.clone()
            },
            Imported {
                claims: 1,
                ..survived.clone()
            },
            Imported {
                lane_mismatches: 1,
                ..survived.clone()
            },
            Imported {
                lanes_running: 1,
                ..survived.clone()
            },
        ] {
            assert_eq!(worker_findings(&imported, 10).len(), 1, "{imported:?}");
        }
    }

    /// The import file is its header and one person per row, each address distinct, so every row
    /// must become one person.
    #[test]
    fn the_import_file_has_one_distinct_person_per_row() {
        assert_eq!(people_csv(0), "email,given_name\n");
        assert_eq!(
            people_csv(2),
            "email,given_name\nperson-1@gates.example,Person 1\nperson-2@gates.example,Person 2\n"
        );
    }

    /// A wire id gives the uuid after its prefix; an id without a prefix or with something other
    /// than 32 hexadecimal digits gives none.
    #[test]
    fn a_wire_id_gives_its_uuid() {
        assert_eq!(
            uuid_of("msg_0190f8a2b4c87a10b6d2e4f6a8c0e2f4").map(|uuid| uuid.to_string()),
            Some("0190f8a2-b4c8-7a10-b6d2-e4f6a8c0e2f4".to_owned())
        );
        assert_eq!(uuid_of("0190f8a2b4c87a10b6d2e4f6a8c0e2f4"), None);
        assert_eq!(uuid_of("imp_not-hex"), None);
    }
}
