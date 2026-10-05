//! Complaints through feedback loops: the abuse reports (ARF, RFC 5965,
//! <https://www.rfc-editor.org/rfc/rfc5965>) mailbox providers send when a recipient marks a
//! message as spam become `complaint` events in the outbox ([`crate::events`]), through the same
//! contract as bounces.
//!
//! **The feedback address.** `fbl@<mail host>` is the address the sending domains' feedback
//! loops are enrolled with (Yahoo's Complaint Feedback Loop, Microsoft's JMRP, any other that
//! sends ARF). Postfix routes it to the LMTP listener of [`crate::bounce`], never to a mailbox;
//! a report is at most 4 MiB like any message that listener takes.
//!
//! **What proves what.** A report proves nothing by its content alone: anyone can send ARF to
//! the feedback address. Two proofs are accepted, and the one found sets the event's
//! `provenance`, which the core maps to its confidence:
//!
//! - `fbl_arf_dkim` (authenticated): the report itself carries a DKIM signature that verifies
//!   (`norbelys_mail::dkim`, the key read from DNS) by the domain of an enrolled feedback loop,
//!   or a subdomain of it (`NORBELYS_SMTP_FBL_REPORTERS`). The reporter is then the feedback
//!   loop itself, and its statement (this recipient complained about this message) stands.
//! - `feedback_id_only` (corroborated): the reporter is not verified, but the message the report
//!   returns carries our own DKIM signature over its `Feedback-ID` header: a signature of one of
//!   the domains registered here, verified with the key the MTA holds for it, on the header block
//!   alone (reports often return the headers only). That proves the message was ours, never who
//!   reported it. A feedback loop that redacts a header our signature covers (often `To`) breaks
//!   this proof, and such a report proves nothing.
//!
//! Either way the report must concern a message this MTA recorded from an authenticated
//! submission, which is what selects the login and its route: the VERP return path in
//! `Original-Mail-From` (the `returns` of [`crate::bounce`]), else the returned `Message-ID` among
//! the submissions. Both are kept seven days, so a complaint about an older message is not
//! attributed. A report whose returned `Message-ID` is another message's than the one its return
//! path recorded is ignored, as a bounce would be.
//!
//! **What is recorded.** One `complaint` event per report: the recipient from `Original-Rcpt-To`
//! when the feedback loop left an address there (they often redact it; the event then names no
//! recipient and the core reviews it), the Message-ID, queue id and login from the MTA's own
//! record, and the feedback type and reporter in `detail`. Only `abuse` and `fraud` reports are
//! complaints; any other type (`not-spam`, `auth-failure`, `virus`) is accepted and ignored. The
//! event id is SHA-256 of the node and the report's digest, so a report delivered twice records
//! nothing new.
//!
//! **Replies.** `250` once the event is recorded or the report ignored; `451`, which Postfix
//! retries, when the database fails, or when an enrolled feedback loop's key could not be read
//! from DNS and nothing proved the reporter otherwise: a DNS failure must never downgrade an
//! authenticated report to a corroborated one.

use std::collections::HashMap;
use std::sync::Arc;

use crate::db::{Connection, OptionalExtension as _, params};
use hickory_resolver::TokioResolver;
use jiff::Timestamp;
use norbelys_mail::arf::{self, Arf, FeedbackType};
use norbelys_mail::dkim::{self, Failure, KeySource};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

use crate::bounce;
use crate::crypto;
use crate::db::{self, Db};
use crate::events::{self, Evidence, Journaled};
use crate::telemetry;

/// The local part of the feedback address on the MTA's own host.
pub const LOCAL: &str = "fbl";

/// Whether `address` is the feedback address of `mail_host`.
#[must_use]
pub fn is_address(address: &str, mail_host: &str) -> bool {
    address.rsplit_once('@').is_some_and(|(local, domain)| {
        local.eq_ignore_ascii_case(LOCAL) && domain.eq_ignore_ascii_case(mail_host)
    })
}

/// What the intake reaches beyond the database: the resolver the reporters' keys are read
/// through, the enrolled feedback loops, and its outcome counter.
pub struct Intake {
    /// The DNS resolver of the reporters' keys.
    resolver: TokioResolver,
    /// The enrolled feedback loops' domains, lowercase.
    reporters: Vec<String>,
    /// Reports by outcome.
    outcomes: Counter<u64>,
}

impl Intake {
    /// The intake of reports signed by the domains `reporters` (or their subdomains), their keys
    /// read through `resolver`.
    #[must_use]
    pub fn new(resolver: TokioResolver, reporters: Vec<String>) -> Self {
        Self {
            resolver,
            reporters,
            outcomes: telemetry::meter()
                .u64_counter("norbelys_mta_feedback_reports")
                .with_description("Feedback reports by outcome: recorded, ignored, retried, failed")
                .build(),
        }
    }
}

/// What a report's signatures prove, found before the database is written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proof {
    /// The domain of the enrolled feedback loop whose signature on the report verified.
    pub reporter: Option<String>,
    /// Whether the returned message carries our own verified signature over its `Feedback-ID`.
    pub signed_feedback_id: bool,
    /// Whether an enrolled feedback loop's key could not be read now.
    pub reporter_unavailable: bool,
}

/// The provenance a proof earns: a verified feedback loop first, then our own signature over
/// the `Feedback-ID`; `None` when it proves neither.
#[must_use]
pub fn provenance(proof: &Proof) -> Option<&'static str> {
    if proof.reporter.is_some() {
        Some("fbl_arf_dkim")
    } else if proof.signed_feedback_id {
        Some("feedback_id_only")
    } else {
        None
    }
}

/// Whether `domain` is the enrolled `reporter` or one of its subdomains.
fn enrolled(domain: &str, reporter: &str) -> bool {
    domain == reporter
        || domain
            .strip_suffix(reporter)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Why a report could not be taken now; Postfix delivers it again.
#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    /// The database failed; nothing was recorded.
    #[error("database: {0}")]
    Database(#[from] db::Error),
    /// The blocking task failed.
    #[error("blocking task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Verifies what the report `raw` (parsed as `report`) proves: its own signatures, with keys from
/// `reporter_keys`, against the enrolled `reporters`; and, when it returns a `Feedback-ID`, the
/// returned message's signatures against our own domains' keys, read from `db`.
///
/// # Errors
///
/// The database fails.
pub async fn prove<K: KeySource + Sync>(
    raw: &[u8],
    report: &Arf,
    reporter_keys: &K,
    reporters: &[String],
    db: &Db,
    now: Timestamp,
) -> Result<Proof, FeedbackError> {
    let mut proof = Proof::default();
    for verdict in dkim::verify(raw, reporter_keys, now).await {
        if !reporters
            .iter()
            .any(|reporter| enrolled(&verdict.domain, reporter))
        {
            continue;
        }
        match verdict.result {
            Ok(()) => {
                proof.reporter = Some(verdict.domain);
                proof.reporter_unavailable = false;
                break;
            }
            Err(Failure::KeyUnavailable(_)) => proof.reporter_unavailable = true,
            Err(_) => {}
        }
    }
    if report.feedback_id.is_some()
        && let Some(returned) = arf::returned(raw)
    {
        let signers = dkim::signers(&returned);
        let own = db
            .call(move |conn| own_keys(conn, &signers).map_err(FeedbackError::from))
            .await?;
        proof.signed_feedback_id = dkim::verify_headers(&returned, &own, now)
            .await
            .iter()
            .any(|verdict| verdict.covers("feedback-id"));
    }
    Ok(proof)
}

/// The key records of the signers `signers` names that are domains registered here with their
/// key: `<selector>._domainkey.<domain>` → `v=DKIM1; k=rsa; p=<key>`, the record the domain
/// publishes. A signer that is not one of ours gets nothing, so its signature cannot verify.
fn own_keys(
    conn: &Connection,
    signers: &[(String, String)],
) -> db::Result<HashMap<String, String>> {
    let statement = conn.prepare_cached(
        "SELECT dkim_public FROM domains
          WHERE name = ?1 AND dkim_selector = ?2 AND verified_at IS NOT NULL AND dkim_public IS NOT NULL",
    )?;
    let mut keys = HashMap::new();
    for (domain, selector) in signers {
        let public: Option<String> = statement
            .query_row(params![domain, selector], |row| row.get(0))
            .optional()?;
        if let Some(public) = public {
            keys.insert(
                format!("{selector}._domainkey.{domain}"),
                format!("v=DKIM1; k=rsa; p={public}"),
            );
        }
    }
    Ok(keys)
}

/// What one report produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// A complaint of this provenance was offered to the journal, with what became of it.
    Complaint {
        /// `fbl_arf_dkim` or `feedback_id_only`.
        provenance: &'static str,
        /// Recorded, already recorded, or not recorded for want of a route.
        journaled: Journaled,
    },
    /// Accepted, but not evidence: why.
    Ignored(&'static str),
}

/// The MTA's record of the message a report concerns.
struct Origin {
    username: String,
    internet_message_id: Option<String>,
    queue_id: String,
}

/// The message `report` concerns, from the MTA's records: the VERP return path its
/// `Original-Mail-From` names on `mail_host`, else the latest submission of its returned
/// `Message-ID`.
fn origin(conn: &Connection, mail_host: &str, report: &Arf) -> db::Result<Option<Origin>> {
    if let Some(token) = report
        .original_mail_from
        .as_deref()
        .and_then(|address| bounce::token(address, mail_host))
        && let Some(found) = bounce::lookup(conn, token)?
    {
        return Ok(Some(Origin {
            username: found.username,
            internet_message_id: found.internet_message_id,
            queue_id: found.queue_id,
        }));
    }
    let Some(message_id) = report.original_message_id.as_deref() else {
        return Ok(None);
    };
    let message_id = format!("<{message_id}>");
    conn.prepare_cached(
        "SELECT username, queue_id FROM submissions WHERE internet_message_id = ?1
          ORDER BY authenticated_at DESC LIMIT 1",
    )?
    .query_row([&message_id], |row| {
        Ok(Origin {
            username: row.get(0)?,
            internet_message_id: Some(message_id.clone()),
            queue_id: row.get(1)?,
        })
    })
    .optional()
}

/// Records the complaint of `report` (whose raw bytes hash to `digest`), proven by `proof`, as
/// observed at `now`, in one transaction (see the module).
///
/// # Errors
///
/// The database fails.
pub fn record(
    conn: &mut Connection,
    node: &str,
    mail_host: &str,
    digest: &str,
    report: &Arf,
    proof: &Proof,
    now: Timestamp,
) -> Result<Taken, FeedbackError> {
    if !report.feedback_type.is_complaint() {
        return Ok(Taken::Ignored("not a complaint"));
    }
    let Some(provenance) = provenance(proof) else {
        return Ok(Taken::Ignored(
            "neither the reporter nor a Feedback-ID of ours is verified",
        ));
    };
    let tx = conn.transaction()?;
    let Some(origin) = origin(&tx, mail_host, report)? else {
        return Ok(Taken::Ignored(
            "the report concerns no message this MTA recorded",
        ));
    };
    if let (Some(recorded), Some(returned)) = (
        origin.internet_message_id.as_deref(),
        report.original_message_id.as_deref(),
    ) && recorded.trim_start_matches('<').trim_end_matches('>') != returned
    {
        return Ok(Taken::Ignored(
            "the returned Message-ID is another message's",
        ));
    }
    let event_id = crypto::sha256_hex(format!("{node}:arf:{digest}").as_bytes());
    let kind = if report.feedback_type == FeedbackType::Fraud {
        "fraud"
    } else {
        "abuse"
    };
    let detail = match (&proof.reporter, &report.user_agent) {
        (Some(reporter), _) => format!("{kind} report by {reporter}"),
        (None, Some(agent)) => format!("{kind} report by {agent} (reporter not verified)"),
        (None, None) => format!("{kind} report (reporter not verified)"),
    };
    let journaled = events::journal(
        &tx,
        &Evidence {
            event_id: &event_id,
            internet_message_id: origin.internet_message_id.as_deref(),
            username: &origin.username,
            recipient: report
                .original_rcpt_to
                .iter()
                .find(|address| address.contains('@'))
                .map(String::as_str),
            queue_id: Some(&origin.queue_id),
            kind: "complaint",
            enhanced_status: None,
            detail: Some(detail.chars().take(2000).collect()),
            provenance,
            observed_at: now.to_string(),
        },
    )?;
    tx.commit()?;
    Ok(Taken::Complaint {
        provenance,
        journaled,
    })
}

/// What the listener answers for one report, and the outcome it counts.
enum Answer {
    Recorded,
    Ignored(&'static str),
    Retry(&'static str),
    Failed(FeedbackError),
}

/// Takes one report delivered to the feedback address `fbl@<mail_host>`; the LMTP reply that
/// says how it went (see the module).
pub async fn receive(
    db: &Db,
    node: &str,
    mail_host: &str,
    intake: &Intake,
    raw: &Arc<Vec<u8>>,
) -> &'static str {
    let answer = take(db, node, mail_host, intake, raw, Timestamp::now()).await;
    let (outcome, reply) = match &answer {
        Answer::Recorded => ("recorded", "250 2.0.0 Recorded"),
        Answer::Ignored(reason) => {
            telemetry::unit(telemetry::Event::Feedback);
            tracing::info!(
                event = "mta.feedback",
                reason,
                outcome = "ignored",
                "mta.feedback"
            );
            ("ignored", "250 2.0.0 Accepted, not evidence")
        }
        Answer::Retry(reason) => {
            telemetry::unit(telemetry::Event::Feedback);
            tracing::warn!(
                event = "mta.feedback",
                reason,
                outcome = "retried",
                "mta.feedback"
            );
            (
                "retried",
                "451 4.4.3 The reporter's key could not be read; try again later",
            )
        }
        Answer::Failed(error) => {
            telemetry::unit(telemetry::Event::Feedback);
            tracing::error!(event = "mta.feedback", error = %error, outcome = "failed", "mta.feedback");
            ("failed", "451 4.3.0 Temporary failure, try again later")
        }
    };
    intake.outcomes.add(1, &[KeyValue::new("outcome", outcome)]);
    reply
}

async fn take(
    db: &Db,
    node: &str,
    mail_host: &str,
    intake: &Intake,
    raw: &Arc<Vec<u8>>,
    now: Timestamp,
) -> Answer {
    let Some(report) = arf::parse(raw) else {
        return Answer::Ignored("not a feedback report");
    };
    if !report.feedback_type.is_complaint() {
        return Answer::Ignored("not a complaint");
    }
    let proof = match prove(raw, &report, &intake.resolver, &intake.reporters, db, now).await {
        Ok(proof) => proof,
        Err(error) => return Answer::Failed(error),
    };
    if proof.reporter.is_none() && proof.reporter_unavailable {
        return Answer::Retry("an enrolled feedback loop's key could not be read");
    }
    let (node, mail_host, digest) = (
        node.to_owned(),
        mail_host.to_owned(),
        crypto::sha256_hex(raw),
    );
    let taken = db
        .call(move |conn| record(conn, &node, &mail_host, &digest, &report, &proof, now))
        .await;
    match taken {
        Ok(Taken::Complaint {
            provenance,
            journaled,
        }) => {
            telemetry::unit(telemetry::Event::Feedback);
            tracing::info!(event = "mta.feedback", provenance, journaled = ?journaled, outcome = "recorded", "mta.feedback");
            Answer::Recorded
        }
        Ok(Taken::Ignored(reason)) => Answer::Ignored(reason),
        Err(error) => Answer::Failed(error),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use aws_lc_rs::encoding::AsDer as _;
    use aws_lc_rs::rsa::{KeyPair as RsaKeyPair, KeySize};
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use norbelys_mail::dkim::{SigningKey, sign};

    use super::*;
    use crate::testing::{TempDir, memory};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// The header block of the reported message, as the MTA signed and sent it.
    pub(crate) const HEADERS: &str = concat!(
        "From: Ada <ada@example.com>\r\n",
        "To: grace@yahoo.example\r\n",
        "Subject: Quick question\r\n",
        "Message-ID: <m1.t1.tag@mail.example.com>\r\n",
        "Feedback-ID: m1:campaign7:norbelys:esp\r\n",
    );

    /// A feedback report of `feedback_type` returning the header block `returned`, naming the
    /// envelope sender `mail_from` and the recipient `rcpt` when given.
    pub(crate) fn report(
        feedback_type: &str,
        mail_from: Option<&str>,
        rcpt: Option<&str>,
        returned: &[u8],
    ) -> Vec<u8> {
        let mut fields = format!(
            "Feedback-Type: {feedback_type}\r\nUser-Agent: Yahoo!-Mail-Feedback/2.0\r\nVersion: 0.1\r\n"
        );
        if let Some(from) = mail_from {
            fields.push_str(&format!("Original-Mail-From: <{from}>\r\n"));
        }
        if let Some(rcpt) = rcpt {
            fields.push_str(&format!("Original-Rcpt-To: <{rcpt}>\r\n"));
        }
        let mut out = format!(
            concat!(
                "From: feedback@arf.yahoo.example\r\n",
                "To: fbl@mail.example.com\r\n",
                "Subject: complaint about a message\r\n",
                "MIME-Version: 1.0\r\n",
                "Content-Type: multipart/report; report-type=feedback-report; boundary=\"arf\"\r\n",
                "\r\n",
                "--arf\r\n",
                "Content-Type: text/plain\r\n",
                "\r\n",
                "This is a spam complaint.\r\n",
                "\r\n",
                "--arf\r\n",
                "Content-Type: message/feedback-report\r\n",
                "\r\n",
                "{}",
                "\r\n",
                "--arf\r\n",
                "Content-Type: text/rfc822-headers\r\n",
                "\r\n",
            ),
            fields
        )
        .into_bytes();
        out.extend_from_slice(returned);
        out.extend_from_slice(b"\r\n--arf--\r\n");
        out
    }

    /// A database where `relay@example.com` (on `example.com`, signed with `public` as the key
    /// of selector `norbelys` when given) reports to route `pwh_1`, and the tail recorded the
    /// submission of `<m1.t1.tag@mail.example.com>` (queue id `4ZQB1xYz`) through the return
    /// path [`TOKEN`].
    pub(crate) fn recorded(conn: &Connection, public: Option<&str>) {
        conn.execute(
            "INSERT INTO domains (name, ownership_token, verified_at, dkim_selector, dkim_public, created_at)
             VALUES ('example.com', 't', '2026-10-01T00:00:00Z', 'norbelys', ?1, 'now')",
            [public],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (username, domain, kind, rate_class, created_at)
               VALUES ('relay@example.com', 'example.com', 'relay', 'relay', 'now');
             INSERT INTO routes VALUES ('pwh_1', 'https://localhost/webhooks/pwh_1', x'00', 'now');
             INSERT INTO account_routes VALUES ('relay@example.com', 'pwh_1');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO returns VALUES (?1, 'relay@example.com', '<m1.t1.tag@mail.example.com>', '4ZQB1xYz', 0)",
            [TOKEN],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO submissions VALUES ('4ZQB1xYz', 'relay@example.com', '<m1.t1.tag@mail.example.com>', 0)",
            [],
        )
        .unwrap();
    }

    fn payloads(conn: &Connection) -> Vec<serde_json::Value> {
        conn.prepare("SELECT payload FROM events ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
            .collect()
    }

    /// An RSA key of ours: the signing half, and the public half as `domains.dkim_public` holds
    /// it (SubjectPublicKeyInfo in base64).
    pub(crate) fn our_key() -> (SigningKey, String) {
        let pair = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let public = STANDARD.encode(pair.public_key().as_der().unwrap().as_ref());
        (SigningKey::Rsa(pair), public)
    }

    /// [`HEADERS`] signed by `example.com` with `key`, over every field, `Feedback-ID` included,
    /// as Rspamd signs the MTA's mail.
    pub(crate) fn signed_headers(key: &SigningKey) -> Vec<u8> {
        sign(
            HEADERS.as_bytes(),
            key,
            "example.com",
            "norbelys",
            &["from", "to", "subject", "message-id", "feedback-id"],
        )
        .unwrap()
    }

    fn now() -> Timestamp {
        Timestamp::from_second(1_790_910_000).unwrap()
    }

    /// The feedback address is `fbl` on the MTA's own host, in any case; any other address is
    /// not.
    #[test]
    fn recognises_the_feedback_address() {
        assert!(is_address("fbl@mail.example.com", "mail.example.com"));
        assert!(is_address("FBL@Mail.Example.com", "mail.example.com"));
        for other in [
            "fbl@example.com",
            "abuse@mail.example.com",
            "fbl",
            "xfbl@mail.example.com",
        ] {
            assert!(!is_address(other, "mail.example.com"), "{other}");
        }
    }

    /// The provenance follows the strongest proof: a verified feedback loop is authenticated
    /// whatever else holds, our own signed `Feedback-ID` alone is corroborated, and nothing
    /// proven is no evidence at all.
    #[test]
    fn provenance_follows_the_strongest_proof() {
        for (reporter, signed, expected) in [
            (true, true, Some("fbl_arf_dkim")),
            (true, false, Some("fbl_arf_dkim")),
            (false, true, Some("feedback_id_only")),
            (false, false, None),
        ] {
            let proof = Proof {
                reporter: reporter.then(|| "yahoo.example".to_owned()),
                signed_feedback_id: signed,
                reporter_unavailable: false,
            };
            assert_eq!(provenance(&proof), expected, "{proof:?}");
        }
        assert!(enrolled("arf.yahoo.example", "yahoo.example"));
        assert!(!enrolled("evilyahoo.example", "yahoo.example"));
    }

    /// A proven complaint about a recorded message is one `complaint` event for the login's
    /// route: the recipient from the report, the Message-ID, queue id and login from the MTA's
    /// record, the reporter in `detail`; delivered again, it records nothing; a report whose
    /// recipient is redacted still records the event, naming no recipient.
    #[test]
    fn records_a_proven_complaint_once() {
        let mut conn = memory();
        recorded(&conn, None);
        let authenticated = Proof {
            reporter: Some("arf.yahoo.example".to_owned()),
            ..Proof::default()
        };
        let from = format!("bounce+{TOKEN}@mail.example.com");
        let raw = report(
            "abuse",
            Some(&from),
            Some("grace@yahoo.example"),
            HEADERS.as_bytes(),
        );
        let parsed = arf::parse(&raw).unwrap();
        let first = record(
            &mut conn,
            "node",
            "mail.example.com",
            "d1",
            &parsed,
            &authenticated,
            now(),
        )
        .unwrap();
        assert_eq!(
            first,
            Taken::Complaint {
                provenance: "fbl_arf_dkim",
                journaled: Journaled::Inserted
            }
        );
        let again = record(
            &mut conn,
            "node",
            "mail.example.com",
            "d1",
            &parsed,
            &authenticated,
            now(),
        )
        .unwrap();
        assert_eq!(
            again,
            Taken::Complaint {
                provenance: "fbl_arf_dkim",
                journaled: Journaled::Duplicate
            }
        );
        let redacted = arf::parse(&report(
            "abuse",
            Some(&from),
            Some("redacted"),
            HEADERS.as_bytes(),
        ))
        .unwrap();
        let corroborated = Proof {
            signed_feedback_id: true,
            ..Proof::default()
        };
        record(
            &mut conn,
            "node",
            "mail.example.com",
            "d2",
            &redacted,
            &corroborated,
            now(),
        )
        .unwrap();

        let events = payloads(&conn);
        assert_eq!(events.len(), 2);
        let complaint = events[0].as_object().unwrap();
        assert_eq!(complaint["kind"], "complaint");
        assert_eq!(complaint["provenance"], "fbl_arf_dkim");
        assert_eq!(complaint["recipient"], "grace@yahoo.example");
        assert_eq!(
            complaint["internet_message_id"],
            "<m1.t1.tag@mail.example.com>"
        );
        assert_eq!(complaint["queue_id"], "4ZQB1xYz");
        assert_eq!(complaint["username"], "relay@example.com");
        assert_eq!(complaint["detail"], "abuse report by arf.yahoo.example");
        assert_eq!(complaint["observed_at"], "2026-10-02T03:00:00Z");
        assert_eq!(events[1]["provenance"], "feedback_id_only");
        assert!(events[1]["recipient"].is_null());
    }

    /// Nothing is recorded without proof, a complaint and a recorded message: a report that
    /// proves nothing, a `not-spam` report, a report about a message the MTA never recorded, and
    /// one whose returned Message-ID is another message's than its return path recorded are all
    /// accepted and ignored. Without a return path, the returned Message-ID finds the submission.
    #[test]
    fn ignores_what_proves_nothing() {
        let mut conn = memory();
        recorded(&conn, None);
        let proven = Proof {
            reporter: Some("arf.yahoo.example".to_owned()),
            ..Proof::default()
        };
        let from = format!("bounce+{TOKEN}@mail.example.com");
        let mut take = |raw: &[u8], proof: &Proof| {
            record(
                &mut conn,
                "node",
                "mail.example.com",
                &crypto::sha256_hex(raw),
                &arf::parse(raw).unwrap(),
                proof,
                now(),
            )
            .unwrap()
        };
        let abuse = report("abuse", Some(&from), None, HEADERS.as_bytes());
        assert_eq!(
            take(&abuse, &Proof::default()),
            Taken::Ignored("neither the reporter nor a Feedback-ID of ours is verified")
        );
        assert_eq!(
            take(
                &report("not-spam", Some(&from), None, HEADERS.as_bytes()),
                &proven
            ),
            Taken::Ignored("not a complaint")
        );
        let stranger = HEADERS.replace("<m1.t1.tag@", "<m9.t9.tag@");
        assert_eq!(
            take(&report("abuse", None, None, stranger.as_bytes()), &proven),
            Taken::Ignored("the report concerns no message this MTA recorded")
        );
        assert_eq!(
            take(
                &report("abuse", Some(&from), None, stranger.as_bytes()),
                &proven
            ),
            Taken::Ignored("the returned Message-ID is another message's")
        );
        assert_eq!(
            take(&report("abuse", None, None, HEADERS.as_bytes()), &proven),
            Taken::Complaint {
                provenance: "fbl_arf_dkim",
                journaled: Journaled::Inserted
            }
        );
    }

    /// The proofs are read from the signatures: a report signed by an enrolled feedback loop's
    /// subdomain proves its reporter, one signed by any other domain or changed after signing
    /// does not, and an enrolled loop's key that cannot be read now is reported as such; the
    /// returned headers prove our `Feedback-ID` with the key the MTA holds for the domain, and
    /// stop proving it once a signed field was changed (a redacted `To`) or another key signed.
    #[tokio::test]
    async fn proofs_come_from_the_signatures() {
        let dir = TempDir::new();
        let database = dir.join("smtp.sqlite");
        let (ours, public) = our_key();
        recorded(&db::open(&database).unwrap(), Some(public.as_str()));
        let db = Db::open(&database).unwrap();
        let reporters = vec!["yahoo.example".to_owned()];
        let loop_key = Ed25519KeyPair::generate().unwrap();
        let keys = HashMap::from([(
            "fbl._domainkey.arf.yahoo.example".to_owned(),
            format!(
                "v=DKIM1; k=ed25519; p={}",
                STANDARD.encode(loop_key.public_key().as_ref())
            ),
        )]);
        let loop_key = SigningKey::Ed25519(loop_key);
        let unsigned = report(
            "abuse",
            None,
            Some("grace@yahoo.example"),
            HEADERS.as_bytes(),
        );
        let by = |domain: &str, raw: &[u8]| {
            sign(raw, &loop_key, domain, "fbl", &["from", "to", "subject"]).unwrap()
        };
        let proof = async |raw: &[u8], keys: &HashMap<String, String>| {
            let parsed = arf::parse(raw).unwrap();
            prove(raw, &parsed, keys, &reporters, &db, now())
                .await
                .unwrap()
        };

        let signed = by("arf.yahoo.example", unsigned.as_slice());
        assert_eq!(
            proof(signed.as_slice(), &keys).await.reporter.as_deref(),
            Some("arf.yahoo.example")
        );
        let other = by("spam.example", unsigned.as_slice());
        assert_eq!(proof(other.as_slice(), &keys).await.reporter, None);
        let changed = String::from_utf8(signed.clone())
            .unwrap()
            .replace("grace@yahoo.example>", "ada@yahoo.example>");
        assert_eq!(proof(changed.as_bytes(), &keys).await.reporter, None);
        let no_key = proof(signed.as_slice(), &HashMap::new()).await;
        assert_eq!(
            (no_key.reporter, no_key.reporter_unavailable),
            (None, false)
        );

        let returned = signed_headers(&ours);
        let corroborated = proof(report("abuse", None, None, &returned).as_slice(), &keys).await;
        assert_eq!(
            corroborated,
            Proof {
                signed_feedback_id: true,
                ..Proof::default()
            }
        );
        let redacted = String::from_utf8(returned.clone())
            .unwrap()
            .replace("To: grace@yahoo.example", "To: redacted@yahoo.example");
        assert!(
            !proof(
                report("abuse", None, None, redacted.as_bytes()).as_slice(),
                &keys
            )
            .await
            .signed_feedback_id
        );
        let (stranger, _) = our_key();
        let foreign = signed_headers(&stranger);
        assert!(
            !proof(report("abuse", None, None, &foreign).as_slice(), &keys)
                .await
                .signed_feedback_id
        );
    }

    /// A key source whose every lookup fails, as DNS does when it times out.
    struct Down;

    impl KeySource for Down {
        fn txt(
            &self,
            _: &str,
        ) -> impl Future<Output = Result<Vec<String>, dkim::KeyUnavailable>> + Send {
            std::future::ready(Err::<Vec<String>, _>(dkim::KeyUnavailable(
                "timed out".to_owned(),
            )))
        }
    }

    /// An enrolled feedback loop's key that DNS cannot give now marks the proof as unavailable,
    /// which makes the listener answer `451` rather than record a weaker provenance.
    #[tokio::test]
    async fn an_unreadable_reporter_key_is_temporary() {
        let dir = TempDir::new();
        let db = Db::open(&dir.join("smtp.sqlite")).unwrap();
        let key = SigningKey::Ed25519(Ed25519KeyPair::generate().unwrap());
        let raw = sign(
            &report("abuse", None, None, HEADERS.as_bytes()),
            &key,
            "yahoo.example",
            "fbl",
            &["from"],
        )
        .unwrap();
        let parsed = arf::parse(&raw).unwrap();
        let proof = prove(
            &raw,
            &parsed,
            &Down,
            &["yahoo.example".to_owned()],
            &db,
            now(),
        )
        .await
        .unwrap();
        assert_eq!((proof.reporter, proof.reporter_unavailable), (None, true));
    }
}
