//! What the inbox makes of a message it read: the facts the rules weigh, the evidence a report
//! is, and the `inbox.classify` kind, which asks AI about what the rules left open.
//!
//! # Evidence from the mailbox
//!
//! Three kinds of inbound mail are evidence, recorded through `delivery::evidence::record` in
//! the poll's transaction, once (the inbound message's transport key refuses a second read):
//!
//! - **A delivery status notification**: one event per recipient block, sharing the report's own
//!   `Message-ID` as the source event id; the block's action and status decide the kind and the
//!   category (`domain::inbox::dsn_outcome`).
//! - **An abuse report** about consent: one `complaint` event, naming the recipient only when the
//!   report gives exactly one address that is not redacted.
//! - **An unsubscribe request by mail** (the subject our `mailto:` link asks for): one
//!   `unsubscribed` event for the sender's address, which the evidence rules suppress.
//!
//! Each is `corroborated` only when it arrived through the mailbox of the connection that sent
//! the message, proves the message was ours (a verified tag, or a directory row accepted for
//! this mailbox) and names one of that message's envelope addresses; anything less is
//! `inferred` (`domain::inbox::report_confidence`). A report read from a mailbox is never
//! `authenticated`: correlation proves the message was ours, not who wrote the report. What the
//! evidence rules cannot apply alone comes back as a review, which the poll attaches to the
//! inbound message as its proposal.
//!
//! # The `inbox.classify` kind
//!
//! Enqueued by the poll, in its transaction, for a message the rules left `human_reply` or
//! `unknown` in a workspace that turned `settings.ai.classify_replies` on; one job per message
//! and revision. It reads the message, asks the classifier (`ai::classify`) and applies its
//! verdict with compare-and-set: only while the revision is still the one it was enqueued for
//! and nobody classified the message by hand, so a person's decision is never overwritten and a
//! stale verdict changes nothing. A verdict below the workspace's confidence threshold only asks
//! for a review. AI never creates evidence, suppressions or holds, and never resumes a stopped
//! enrollment.

use std::time::Duration;

use norbelys_mail::arf::Arf;
use norbelys_mail::dsn::{self, Dsn};
use norbelys_mail::inbound::{Inbound, Report as MailReport};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::correlate::Found;
use crate::ai::classify::{self as ai, Classified, Skip};
use crate::delivery::evidence::{Action, Evidence};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, InboundMessage, ReceiveBinding};
use crate::domain::inbox::{
    Authority, Classification, ClassificationSource, Facts, Report, ReportMatch, Verdict,
    dsn_outcome, report_confidence,
};
use crate::domain::policy::delivery::{Category, EventKind, RecipientRef, Source};
use crate::domain::time::Timestamp;
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue, RecoveryHook};

/// How long the kind waits before trying again after a provider failure that gave no wait.
const PROVIDER_RETRY: Duration = Duration::from_secs(60);

/// The facts the rules weigh for `inbound`; `answers_ours` when it correlated to our mail.
#[must_use]
pub fn facts(inbound: &Inbound, answers_ours: bool) -> Facts<'_> {
    let report = inbound.report.as_ref().map(|report| match report {
        MailReport::Dsn(dsn) => Report::Dsn {
            address_change: dsn.recipients.iter().any(|recipient| {
                recipient
                    .status
                    .is_some_and(|status| status.subject() == 1 && status.detail() == 6)
            }),
        },
        MailReport::Arf(arf) => Report::Arf {
            complaint: arf.feedback_type.is_complaint(),
        },
    });
    Facts {
        report,
        auto_submitted: inbound.auto_submitted.as_deref(),
        precedence: inbound.precedence.as_deref(),
        from: inbound.from.as_ref().map(|from| from.address.as_str()),
        subject: inbound.subject.as_deref(),
        text: inbound.excerpt.as_deref(),
        answers_ours,
    }
}

/// The ids that may name our mail, in the order correlation tries them, and the address a
/// directory match must find in the original envelope: for a report, its returned
/// `Message-ID` and the recipient it names first; for any other mail, `In-Reply-To`, then
/// `References` from the newest, and the sender.
#[must_use]
pub fn correlation_keys(inbound: &Inbound) -> (Vec<&str>, Option<&str>) {
    let mut ids: Vec<&str> = Vec::new();
    let mut address = inbound.from.as_ref().map(|from| from.address.as_str());
    match &inbound.report {
        Some(MailReport::Dsn(dsn)) => {
            ids.extend(dsn.original_message_id.as_deref());
            address = dsn
                .recipients
                .iter()
                .find_map(|recipient| recipient.final_recipient.as_deref());
        }
        Some(MailReport::Arf(arf)) => {
            ids.extend(arf.original_message_id.as_deref());
            address = arf_recipient(arf);
        }
        None => {}
    }
    ids.extend(inbound.in_reply_to.iter().map(String::as_str));
    ids.extend(inbound.references.iter().rev().map(String::as_str));
    (ids, address)
}

/// Where a report was read, and its identity as a source.
#[derive(Debug, Clone)]
pub struct Origin {
    /// The binding it arrived through.
    pub binding: Id<ReceiveBinding>,
    /// That binding's connection.
    pub connection: Uuid,
    /// The report's own identity: its `Message-ID`, else its transport key.
    pub source_event_id: String,
    /// When the mailbox received it.
    pub observed_at: Timestamp,
}

/// The evidence `inbound` is, given what the rules decided and what it correlated to (see the
/// module); empty for mail that is not evidence.
#[must_use]
pub fn evidence(
    inbound: &Inbound,
    verdict: &Verdict,
    found: Option<&Found>,
    origin: &Origin,
) -> Vec<Evidence> {
    match (&inbound.report, verdict.authority) {
        (Some(MailReport::Dsn(dsn)), Authority::Dsn) => dsn_evidence(dsn, found, origin),
        (Some(MailReport::Arf(arf)), Authority::Arf) if arf.feedback_type.is_complaint() => {
            let recipient = arf_recipient(arf).map(str::to_owned);
            vec![one(
                found,
                origin,
                recipient,
                Source::Arf,
                (EventKind::Complaint, Category::Complaint),
                None,
                None,
                arf.feedback_id.clone(),
            )]
        }
        (_, Authority::UnsubscribeRequest) => inbound
            .from
            .as_ref()
            .map(|from| {
                one(
                    found,
                    origin,
                    Some(from.address.clone()),
                    Source::Unsubscribe,
                    (EventKind::Unsubscribed, Category::Unsubscribed),
                    None,
                    None,
                    None,
                )
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// One event per recipient block of a DSN.
fn dsn_evidence(dsn: &Dsn, found: Option<&Found>, origin: &Origin) -> Vec<Evidence> {
    dsn.recipients
        .iter()
        .map(|block| {
            let status = block
                .status
                .map(|status| (status.class(), status.subject(), status.detail()));
            let action = block.action.map(|action| match action {
                dsn::Action::Failed => Action::Failed,
                dsn::Action::Delayed => Action::Delayed,
                dsn::Action::Delivered => Action::Delivered,
                dsn::Action::Relayed => Action::Relayed,
                dsn::Action::Expanded => Action::Expanded,
            });
            one(
                found,
                origin,
                block.final_recipient.clone(),
                Source::Dsn,
                dsn_outcome(block.action, status),
                action,
                block.status.map(|status| status.to_string()),
                block.diagnostic.clone(),
            )
        })
        .collect()
}

/// One observation from the mailbox about `recipient` (or, without one, about the single
/// address of the message's envelope when it had one).
#[allow(
    clippy::too_many_arguments,
    reason = "one call site per evidence kind, each naming every fact"
)]
fn one(
    found: Option<&Found>,
    origin: &Origin,
    recipient: Option<String>,
    source: Source,
    (kind, category): (EventKind, Category),
    action: Option<Action>,
    enhanced_status: Option<String>,
    diagnostic: Option<String>,
) -> Evidence {
    let envelope = found
        .map(|found| found.envelope.as_slice())
        .unwrap_or_default();
    let (recipient, recipient_ref) = match recipient {
        Some(recipient) => (Some(recipient), RecipientRef::Named),
        None => match envelope {
            [only] => (Some(only.clone()), RecipientRef::SingleEnvelope),
            _ => (None, RecipientRef::Unknown),
        },
    };
    let key = |address: &str| {
        EmailAddress::parse(address)
            .ok()
            .map(|address| address.key())
    };
    let in_envelope = recipient.as_deref().and_then(key).is_some_and(|wanted| {
        envelope
            .iter()
            .any(|address| key(address).as_ref() == Some(&wanted))
    });
    let confidence = report_confidence(ReportMatch {
        own_binding: found.and_then(|found| found.sent_through) == Some(origin.connection),
        ours: found.is_some(),
        recipient_in_envelope: in_envelope,
    });
    Evidence {
        message: found.and_then(|found| found.message),
        thread: found.map(|found| found.thread.id.uuid()),
        attempt_number: None,
        recipient,
        recipient_ref,
        source,
        source_event_id: origin.source_event_id.clone(),
        received_via: Some(origin.binding),
        kind,
        action,
        phase: None,
        enhanced_status,
        category,
        diagnostic,
        confidence,
        receipt: None,
        observed_at: origin.observed_at,
    }
}

/// The recipient an abuse report names: its one `Original-Rcpt-To` when that is an address
/// and not redacted (feedback loops commonly blank the local part).
fn arf_recipient(arf: &Arf) -> Option<&str> {
    let [only] = arf.original_rcpt_to.as_slice() else {
        return None;
    };
    let address = EmailAddress::parse(only).ok()?;
    let local = address.local();
    let redacted = only.to_ascii_lowercase().contains("redacted")
        || local.chars().all(|c| matches!(c, 'x' | 'X' | '*'));
    (!redacted).then_some(only.as_str())
}

/// A confidence from 0 to 1 as a whole percentage, rounded half up and kept within 0..=100 (a
/// value outside, or not a number, is bounded or read as 0).
fn percent(confidence: f64) -> u8 {
    let scaled = confidence.clamp(0.0, 1.0) * 100.0;
    (0..=100u8)
        .rev()
        .find(|step| f64::from(*step) <= scaled + 0.5)
        .unwrap_or(0)
}

/// `inbox.classify`: asks AI about one inbound message the rules left open (see the module).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classify {
    /// The message.
    pub inbound: Id<InboundMessage>,
    /// The revision it was enqueued for; a verdict applies only while it is current.
    pub revision: i32,
}

impl Job for Classify {
    const KIND: &'static str = "inbox.classify";
    const QUEUE: Queue = Queue::Ai;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const RECOVERY_HOOK: Option<RecoveryHook> = Some(crate::ai::store::settle_abandoned);

    fn unique_key(&self) -> Option<String> {
        Some(format!("{}:{}", self.inbound, self.revision))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let mut tx = cx.db().begin_in(workspace).await?;
        let row = sqlx::query!(
            "SELECT from_email, from_name, subject, in_reply_to, body_text, classification, classification_source, revision
               FROM inbound_messages WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NULL",
            workspace.uuid(),
            self.inbound.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let open = row.as_ref().is_some_and(|row| {
            row.revision == self.revision
                && row.classification_source != ClassificationSource::Manual.as_str()
                && row
                    .classification
                    .parse::<Classification>()
                    .is_ok_and(Classification::open_to_ai)
        });
        let Some(row) = row.filter(|_| open) else {
            return Ok(Outcome::Done);
        };
        let from = match (&row.from_name, &row.from_email) {
            (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
            (None, Some(email)) => Some(email.clone()),
            _ => None,
        };
        let classified = ai::classify(
            cx,
            &ai::Inbound {
                from: from.as_deref(),
                subject: row.subject.as_deref(),
                auto_submitted: None,
                in_reply_to: row.in_reply_to.as_deref(),
                body: row.body_text.as_deref().unwrap_or_default(),
            },
        )
        .await?;
        // A verdict a person must check is kept as the review's proposal: confirming applies it, and
        // either decision is the label that measures the classifier on real mail.
        let (classification, sentiment, review, proposal) = match &classified {
            Classified::Apply(verdict) => (
                Some(verdict.classification.as_str()),
                Some(verdict.sentiment.as_str()),
                false,
                None,
            ),
            Classified::Review(verdict) => (
                None,
                None,
                true,
                serde_json::to_value(crate::domain::inbox::ReviewProposal::Classify {
                    classification: verdict.classification.as_str().to_owned(),
                    sentiment: verdict.sentiment.as_str().to_owned(),
                    confidence_percent: percent(verdict.confidence),
                })
                .ok(),
            ),
            Classified::Skipped(Skip::Paused(after)) => {
                return Ok(Outcome::Yield { after: *after });
            }
            Classified::Skipped(skip @ Skip::Provider { .. }) => {
                return Ok(Outcome::Retry {
                    after: skip.retry_after().unwrap_or(PROVIDER_RETRY),
                });
            }
            Classified::Skipped(_) => return Ok(Outcome::Done),
        };
        let mut chunk = cx.begin().await?;
        let applied = sqlx::query!(
            "UPDATE inbound_messages
                SET classification = coalesce($4, classification),
                    sentiment = coalesce($5, sentiment),
                    classification_source = CASE WHEN $4::text IS NULL THEN classification_source ELSE 'ai' END,
                    revision = revision + CASE WHEN $4::text IS NULL THEN 0 ELSE 1 END,
                    review_requested_at = CASE WHEN $6 THEN coalesce(review_requested_at, now()) ELSE review_requested_at END,
                    review_proposal = CASE WHEN $6 THEN coalesce(review_proposal, $7) ELSE review_proposal END
              WHERE workspace_id = $1 AND id = $2 AND revision = $3 AND classification_source <> 'manual'",
            workspace.uuid(),
            self.inbound.uuid(),
            self.revision,
            classification,
            sentiment,
            review,
            proposal,
        )
        .execute(&mut **chunk.tx())
        .await?
        .rows_affected();
        cx.checkpoint(
            chunk,
            serde_json::json!({ "applied": applied == 1, "review": review }),
        )
        .await?;
        if applied == 1
            && let Some(classification) = classification
        {
            super::poll::count_classified(classification, ClassificationSource::Ai.as_str());
        }
        Ok(Outcome::Done)
    }
}

#[cfg(test)]
mod percent_tests {
    use super::percent;

    /// A verdict's confidence is kept on its review proposal as a whole percentage: rounded half
    /// up, bounded to 0..=100, and 0 for a value that is not a number, so a malformed model
    /// answer can never store an out-of-range confidence.
    #[test]
    fn a_confidence_becomes_a_bounded_percentage() {
        for (confidence, expected) in [
            (0.0, 0),
            (0.724, 72),
            (0.725, 73),
            (1.0, 100),
            (1.7, 100),
            (-0.2, 0),
            (f64::NAN, 0),
        ] {
            assert_eq!(percent(confidence), expected, "{confidence}");
        }
    }
}
