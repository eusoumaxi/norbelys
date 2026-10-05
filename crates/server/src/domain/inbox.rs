//! What an inbound message is and what it may change: its classification by authority, the
//! review a notice proposes, what a delivery status notification reports about each recipient,
//! how far a report can be trusted, when a directory row may correlate a reply, and when a
//! receive binding is polled next.
//!
//! # Classification by authority
//!
//! An inbound message is classified by the strongest signal it carries, in this order
//! ([`Authority`]), so a weaker signal never overrides a stronger one:
//!
//! 1. **A delivery status notification** (RFC 3464, <https://www.rfc-editor.org/rfc/rfc3464>):
//!    a `bounce`, or an `address_change` when a recipient's status is `X.1.6` (moved).
//! 2. **An abuse report** (RFC 5965, <https://www.rfc-editor.org/rfc/rfc5965>): a `complaint`
//!    when its feedback type is about consent (`abuse`, `fraud`), else an `auto_reply`.
//! 3. **The automatic-mail markers**: `Auto-Submitted` other than `no` (RFC 3834,
//!    <https://www.rfc-editor.org/rfc/rfc3834>) or a `Precedence` of `auto_reply`, `bulk`,
//!    `junk` or `list`: an `out_of_office` when the text reads as an absence notice, else an
//!    `auto_reply`. Within this tier the notice patterns still say what the automatic reply is
//!    about when it announces a closed mailbox or a person who left (the classic "I no longer
//!    work here, write to …" auto-reply), because that is the case where the mail most needs a
//!    person's review; the markers keep the classification out of `human_reply` either way.
//! 4. **An unsubscribe request by mail**: the subject `unsubscribe` that our `List-Unsubscribe`
//!    `mailto:` link asks mail clients to send (the companion of the one-click POST of RFC 8058,
//!    <https://www.rfc-editor.org/rfc/rfc8058>).
//! 5. **Notice patterns**: words a person (or a mail system speaking for them) writes when a
//!    mailbox is closed, when they left, when they ask to be removed, or when they are away.
//! 6. **A human reply**: the message answers one of our messages and nothing above applied.
//!
//! Anything else is `unknown`. AI may refine only `human_reply` and `unknown`, and never a
//! classification a person set.
//!
//! # Reviews
//!
//! Nothing a person wrote is applied without a person's decision: a notice yields a
//! [`ReviewProposal`] (suppress the sender's address, or move the person to the new address the
//! notice gives) that the inbound message keeps until someone confirms or dismisses it. Reports
//! (DSN, ARF) and unsubscribe requests are evidence instead, recorded with their confidence,
//! and the evidence rules decide whether they act alone or ask for a review too.

use jiff::{SignedDuration, Timestamp};
use norbelys_mail::dsn::Action as DsnAction;
use serde::{Deserialize, Serialize};

use crate::domain::email::EmailAddress;
use crate::domain::policy::delivery::{
    Category, Confidence, EventKind, Proposal as EvidenceProposal,
};
use crate::domain::retry;
use crate::domain::suppressions::Reason as SuppressionReason;

/// The evidence of a classification a delivery status notification decided.
pub const DSN_EVIDENCE: &str = "delivery status notification";
/// The evidence of a classification an abuse report decided.
pub const ARF_EVIDENCE: &str = "abuse report";
/// The most characters of the evidence text kept (which header or field decided).
const EVIDENCE_CHARS: usize = 200;
/// The most characters of the text the notice patterns read.
const NOTICE_CHARS: usize = 2_000;

/// An inbound message's classification (`inbound_messages.classification`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = InboundClassification)]
pub enum Classification {
    /// A person answered one of our messages.
    HumanReply,
    /// An automatic reply other than an absence notice.
    AutoReply,
    /// An automatic absence notice.
    OutOfOffice,
    /// A delivery failure report, or a notice that the mailbox is closed.
    Bounce,
    /// An abuse report about consent.
    Complaint,
    /// A report or notice that the recipient moved to another address.
    AddressChange,
    /// A request to receive no more mail.
    Unsubscribe,
    /// Nothing decided it.
    Unknown,
}

impl Classification {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Whether AI may refine it: the rules found no automatic or reporting signal.
    #[must_use]
    pub fn open_to_ai(self) -> bool {
        matches!(self, Self::HumanReply | Self::Unknown)
    }
}

/// The writer's attitude towards the outreach (`inbound_messages.sentiment`), as AI judges it or
/// a person corrects it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(rename_all = "snake_case")]
pub enum Sentiment {
    /// Interested, asks for more, agrees to talk.
    Positive,
    /// Neither; always for automatic messages.
    Neutral,
    /// Declines, objects or asks to stop.
    Negative,
}

impl Sentiment {
    /// The sentiment as `inbound_messages.sentiment` stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Who decided a classification (`inbound_messages.classification_source`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ClassificationSource {
    /// The rules of this module.
    Rules,
    /// A person.
    Manual,
    /// The AI classifier.
    Ai,
}

impl ClassificationSource {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The signals the rules weigh, strongest first: the order is the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::EnumIter)]
pub enum Authority {
    /// A delivery status notification.
    Dsn,
    /// An abuse report.
    Arf,
    /// `Auto-Submitted` or `Precedence`.
    Automatic,
    /// The subject our unsubscribe `mailto:` link asks for.
    UnsubscribeRequest,
    /// A notice pattern in the subject or the text.
    Notice,
    /// The message answers one of ours.
    Reply,
    /// Nothing.
    None,
}

/// The machine-readable report a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// A delivery status notification; `address_change` when a recipient's status is `X.1.6`.
    Dsn { address_change: bool },
    /// An abuse report; `complaint` when its feedback type is about consent.
    Arf { complaint: bool },
}

/// What the rules read from one inbound message.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts<'a> {
    /// The report it is, if any.
    pub report: Option<Report>,
    /// `Auto-Submitted`, lowercased.
    pub auto_submitted: Option<&'a str>,
    /// `Precedence`, lowercased.
    pub precedence: Option<&'a str>,
    /// The `From` address.
    pub from: Option<&'a str>,
    /// The subject.
    pub subject: Option<&'a str>,
    /// The start of the text body.
    pub text: Option<&'a str>,
    /// It answers one of our messages (its `In-Reply-To` or `References` correlated).
    pub answers_ours: bool,
}

/// What the rules decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The classification.
    pub classification: Classification,
    /// The signal that decided it.
    pub authority: Authority,
    /// Which header or field decided, in words (`inbound_messages.evidence`).
    pub evidence: String,
    /// What a person is asked to confirm, for a notice.
    pub proposal: Option<ReviewProposal>,
}

impl Verdict {
    /// Whether a person answered our mail, which ends the person's enrollment under the
    /// campaign's stop rules and counts as a reply: a human reply, or a notice a person wrote to
    /// end the conversation (remove me, I moved). A report or an automatic reply saying the same
    /// is no answer (a bounce is not a reply): what it proposes waits for its review.
    #[must_use]
    pub fn is_answer(&self) -> bool {
        matches!(self.authority, Authority::Notice | Authority::Reply)
            && matches!(
                self.classification,
                Classification::HumanReply
                    | Classification::Unsubscribe
                    | Classification::AddressChange
            )
    }
}

/// What confirming a review applies (`inbound_messages.review_proposal`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReviewProposal {
    /// Suppress `email` for `reason`.
    Suppress {
        email: String,
        reason: SuppressionReason,
    },
    /// Move the person at `email` to `new_email`; without a new address, the old one is
    /// suppressed as `address_changed`.
    ChangeAddress {
        email: String,
        new_email: Option<String>,
    },
    /// Apply an AI verdict that was not confident enough to apply by itself, or that was sampled
    /// for a person to check: `classification` and `sentiment` as the message stores them, and the
    /// model's confidence in percent. Confirming applies it; either decision is the label that
    /// measures the classifier's precision and recall on real mail.
    Classify {
        #[schema(value_type = Classification)]
        classification: String,
        #[schema(value_type = Sentiment)]
        sentiment: String,
        confidence_percent: u8,
    },
}

impl ReviewProposal {
    /// The proposal for the review that evidence about `email` asked for.
    #[must_use]
    pub fn from_evidence(proposal: EvidenceProposal, email: &str) -> Self {
        match proposal {
            EvidenceProposal::Suppress(reason) => Self::Suppress {
                email: email.to_owned(),
                reason,
            },
            EvidenceProposal::AddressChange => Self::ChangeAddress {
                email: email.to_owned(),
                new_email: None,
            },
        }
    }
}

/// What a notice says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
enum Notice {
    /// The mailbox is closed or no longer read.
    Closed,
    /// The person left; a new address may follow.
    Left,
    /// The person asks to be removed.
    RemoveMe,
    /// The person is away.
    Absent,
}

/// Phrases of each notice, lowercase, matched as substrings of the subject and the unquoted
/// text. English only for now; other languages are classified by AI or by a person.
const CLOSED: &[&str] = &[
    "mailbox is closed",
    "mailbox has been closed",
    "mailbox is no longer",
    "account has been closed",
    "account is closed",
    "address is no longer in use",
    "address is no longer active",
    "address is no longer valid",
    "address is not monitored",
    "is no longer monitored",
];
const LEFT: &[&str] = &[
    "no longer with",
    "no longer work",
    "no longer employed",
    "left the company",
    "left the organization",
    "left the organisation",
    "my new email",
    "my new address",
    "new email address is",
];
const REMOVE_ME: &[&str] = &[
    "remove me",
    "unsubscribe me",
    "please unsubscribe",
    "take me off",
    "stop emailing",
    "stop sending",
    "do not contact",
    "don't contact",
    "do not email",
    "don't email",
    "opt me out",
];
const ABSENT: &[&str] = &[
    "out of office",
    "out of the office",
    "automatic reply",
    "auto-reply",
    "autoreply",
    "on vacation",
    "on holiday",
    "on leave",
    "away from the office",
    "limited access to email",
    "abwesenheit",
];

impl Notice {
    fn phrases(self) -> &'static [&'static str] {
        match self {
            Self::Closed => CLOSED,
            Self::Left => LEFT,
            Self::RemoveMe => REMOVE_ME,
            Self::Absent => ABSENT,
        }
    }
}

/// Classifies one inbound message by authority (see the module).
#[must_use]
pub fn classify(facts: &Facts<'_>) -> Verdict {
    match facts.report {
        Some(Report::Dsn { address_change }) => {
            let classification = if address_change {
                Classification::AddressChange
            } else {
                Classification::Bounce
            };
            return verdict(classification, Authority::Dsn, DSN_EVIDENCE, None);
        }
        Some(Report::Arf { complaint }) => {
            let classification = if complaint {
                Classification::Complaint
            } else {
                Classification::AutoReply
            };
            return verdict(classification, Authority::Arf, ARF_EVIDENCE, None);
        }
        None => {}
    }
    let text = notice_text(facts.subject, facts.text);
    let notice = [
        Notice::Closed,
        Notice::Left,
        Notice::RemoveMe,
        Notice::Absent,
    ]
    .into_iter()
    .find_map(|notice| {
        notice
            .phrases()
            .iter()
            .find(|phrase| text.contains(*phrase))
            .map(|phrase| (notice, *phrase))
    });
    if let Some(marker) = automatic(facts) {
        return match notice {
            Some((found @ (Notice::Closed | Notice::Left), phrase)) => {
                let (classification, proposal) = noticed(found, facts, &text);
                verdict(
                    classification,
                    Authority::Automatic,
                    &format!("{marker}; notice: {phrase}"),
                    proposal,
                )
            }
            Some((Notice::Absent, phrase)) => verdict(
                Classification::OutOfOffice,
                Authority::Automatic,
                &format!("{marker}; notice: {phrase}"),
                None,
            ),
            Some((Notice::RemoveMe, _)) | None => verdict(
                Classification::AutoReply,
                Authority::Automatic,
                &marker,
                None,
            ),
        };
    }
    if is_unsubscribe_request(facts.subject) {
        return verdict(
            Classification::Unsubscribe,
            Authority::UnsubscribeRequest,
            "subject: unsubscribe",
            None,
        );
    }
    if let Some((found, phrase)) = notice {
        let (classification, proposal) = noticed(found, facts, &text);
        return verdict(
            classification,
            Authority::Notice,
            &format!("notice: {phrase}"),
            proposal,
        );
    }
    if facts.answers_ours {
        return verdict(
            Classification::HumanReply,
            Authority::Reply,
            "answers our message",
            None,
        );
    }
    verdict(Classification::Unknown, Authority::None, "no signal", None)
}

fn verdict(
    classification: Classification,
    authority: Authority,
    evidence: &str,
    proposal: Option<ReviewProposal>,
) -> Verdict {
    Verdict {
        classification,
        authority,
        evidence: evidence.chars().take(EVIDENCE_CHARS).collect(),
        proposal,
    }
}

/// The automatic-mail marker the message carries, in words.
fn automatic(facts: &Facts<'_>) -> Option<String> {
    if let Some(value) = facts.auto_submitted.filter(|value| *value != "no") {
        return Some(format!("auto-submitted: {value}"));
    }
    facts
        .precedence
        .filter(|value| matches!(*value, "auto_reply" | "bulk" | "junk" | "list"))
        .map(|value| format!("precedence: {value}"))
}

/// Whether the subject is the one our unsubscribe `mailto:` link asks for.
fn is_unsubscribe_request(subject: Option<&str>) -> bool {
    subject.is_some_and(|subject| subject.trim().eq_ignore_ascii_case("unsubscribe"))
}

/// The classification and review proposal of a notice.
fn noticed(
    notice: Notice,
    facts: &Facts<'_>,
    text: &str,
) -> (Classification, Option<ReviewProposal>) {
    let from = facts.from.map(str::to_owned);
    match notice {
        Notice::Closed => (
            Classification::Bounce,
            from.map(|email| ReviewProposal::Suppress {
                email,
                reason: SuppressionReason::AccountClosed,
            }),
        ),
        Notice::Left => {
            let new_email = other_address(text, facts.from);
            (
                Classification::AddressChange,
                from.map(|email| match new_email {
                    Some(new_email) => ReviewProposal::ChangeAddress {
                        email,
                        new_email: Some(new_email),
                    },
                    None => ReviewProposal::Suppress {
                        email,
                        reason: SuppressionReason::AddressChanged,
                    },
                }),
            )
        }
        Notice::RemoveMe => (
            Classification::Unsubscribe,
            from.map(|email| ReviewProposal::Suppress {
                email,
                reason: SuppressionReason::Unsubscribe,
            }),
        ),
        Notice::Absent => (Classification::OutOfOffice, None),
    }
}

/// The subject and the text a person wrote, lowercase: quoted lines (`>`) dropped and the text
/// cut where a quoted earlier message starts, so our own words in the quote never match.
fn notice_text(subject: Option<&str>, text: Option<&str>) -> String {
    let mut out = subject.unwrap_or_default().to_lowercase();
    for line in text.unwrap_or_default().lines() {
        let line = line.trim();
        let lower = line.to_lowercase();
        if lower.starts_with("-----original message")
            || lower.starts_with("________________")
            || (lower.starts_with("on ") && lower.ends_with("wrote:"))
            || lower.starts_with("from:")
        {
            break;
        }
        if line.starts_with('>') {
            continue;
        }
        out.push(' ');
        out.push_str(&lower);
        if out.len() > NOTICE_CHARS {
            break;
        }
    }
    out
}

/// The first address in `text` other than `from`: the new address a "I left" notice gives.
fn other_address(text: &str, from: Option<&str>) -> Option<String> {
    let from = from.and_then(|from| EmailAddress::parse(from).ok().map(|from| from.key()));
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '<' | '>' | '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\''
            )
    })
    .map(|token| token.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?')))
    .filter(|token| token.contains('@'))
    .filter_map(|token| EmailAddress::parse(token.trim_start_matches("mailto:")).ok())
    .find(|address| Some(address.key()) != from)
    .map(|address| address.as_str().to_owned())
}

/// What one recipient block of a delivery status notification reports: the event's kind and its
/// category. The action decides the kind (a missing action is read from the status class); the
/// status's subject the category: a full mailbox (`X.2.2`), a moved address (`X.1.6`), an
/// address that does not exist (`X.1.x`), no route (`X.4.x`), policy (`X.7.x`).
#[must_use]
pub fn dsn_outcome(
    action: Option<DsnAction>,
    status: Option<(u8, u16, u16)>,
) -> (EventKind, Category) {
    let action = action.or(match status.map(|(class, _, _)| class) {
        Some(5) => Some(DsnAction::Failed),
        Some(4) => Some(DsnAction::Delayed),
        Some(2) => Some(DsnAction::Delivered),
        _ => None,
    });
    let detail = status.map(|(_, subject, detail)| (subject, detail));
    match action {
        Some(DsnAction::Failed) => match detail {
            Some((1, 6)) => (EventKind::AddressChanged, Category::AddressChanged),
            Some((2, 2)) => (EventKind::Bounced, Category::MailboxFull),
            Some((1, _)) => (EventKind::Bounced, Category::InvalidRecipient),
            Some((4, _)) => (EventKind::Bounced, Category::NoRoute),
            Some((7, _)) => (EventKind::Bounced, Category::Policy),
            _ => (EventKind::Bounced, Category::Rejected),
        },
        Some(DsnAction::Delayed) => match detail {
            Some((2, 2)) => (EventKind::Deferred, Category::MailboxFull),
            Some((7, _)) => (EventKind::Deferred, Category::Policy),
            _ => (EventKind::Deferred, Category::Transient),
        },
        Some(DsnAction::Delivered | DsnAction::Relayed | DsnAction::Expanded) => {
            (EventKind::Delivered, Category::Delivered)
        }
        None => (EventKind::Reported, Category::Rejected),
    }
}

/// How a mail-borne report (a DSN or an ARF report read from a mailbox) matches our records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReportMatch {
    /// It arrived through a binding of the connection that sent the message.
    pub own_binding: bool,
    /// The returned headers carry our Message-ID with a valid tag, or a provider's id our
    /// directory holds for the message.
    pub ours: bool,
    /// The recipient it names is one of the message's envelope addresses.
    pub recipient_in_envelope: bool,
}

/// How far a mail-borne report can be trusted: `corroborated` when it arrived through the
/// sending identity's own mailbox, proves the message was ours and names one of its recipients;
/// `inferred` for any partial match. Correlation never authenticates the reporter, so a report
/// read from a mailbox is never `authenticated`.
#[must_use]
pub fn report_confidence(found: ReportMatch) -> Confidence {
    if found.own_binding && found.ours && found.recipient_in_envelope {
        Confidence::Corroborated
    } else {
        Confidence::Inferred
    }
}

/// Whether a directory row (an outbound id a provider put in place of ours) may correlate mail
/// read through `binding_connection`: only through the connection of the thread's identity, and
/// only when the mail's sender, or the recipient a report names, is one of the original
/// envelope's addresses. An unsigned id proves nothing alone, so both must hold.
#[must_use]
pub fn directory_accepts(
    binding_connection: uuid::Uuid,
    identity_connection: uuid::Uuid,
    address: Option<&str>,
    recipients: &[String],
) -> bool {
    let Some(address) = address.and_then(|address| EmailAddress::parse(address).ok()) else {
        return false;
    };
    let key = address.key();
    binding_connection == identity_connection
        && recipients.iter().any(|recipient| {
            EmailAddress::parse(recipient).is_ok_and(|recipient| recipient.key() == key)
        })
}

/// How a poll ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polled {
    /// A page was stored; `full` when the provider has more waiting.
    Page { full: bool },
    /// The poll failed; `failures` counts the consecutive failures including this one.
    Failed { failures: u32 },
}

/// When a binding is polled next: its start plus `interval` after a page, at once while pages
/// come back full (a backlog drains without waiting), and after a failure the wait of the inbox's
/// retry policy ([`retry::inbox`]) for its consecutive failures, chosen by the random `draw`:
/// between one interval and two after the first failure, the step doubling with each further
/// one, an hour at most. A page ignores `draw`.
#[must_use]
pub fn next_poll(
    start: Timestamp,
    now: Timestamp,
    interval: SignedDuration,
    polled: Polled,
    draw: u64,
) -> Timestamp {
    match polled {
        Polled::Page { full: true } => now,
        Polled::Page { full: false } => start.saturating_add(interval).unwrap_or(start),
        Polled::Failed { failures } => {
            let wait = retry::backoff(
                failures.saturating_sub(1),
                &retry::inbox(interval.unsigned_abs()),
                draw,
            );
            now.saturating_add(wait).unwrap_or(now)
        }
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;

    fn facts_with(authority: Authority) -> Facts<'static> {
        match authority {
            Authority::Dsn => Facts {
                report: Some(Report::Dsn {
                    address_change: false,
                }),
                ..Facts::default()
            },
            Authority::Arf => Facts {
                report: Some(Report::Arf { complaint: true }),
                ..Facts::default()
            },
            Authority::Automatic => Facts {
                auto_submitted: Some("auto-replied"),
                ..Facts::default()
            },
            Authority::UnsubscribeRequest => Facts {
                subject: Some("Unsubscribe"),
                ..Facts::default()
            },
            Authority::Notice => Facts {
                text: Some("Please remove me from your list."),
                ..Facts::default()
            },
            Authority::Reply => Facts {
                answers_ours: true,
                ..Facts::default()
            },
            Authority::None => Facts::default(),
        }
    }

    /// Merges the signals of two facts: what a message carrying both would show.
    fn both(a: Facts<'static>, b: Facts<'static>) -> Facts<'static> {
        Facts {
            report: a.report.or(b.report),
            auto_submitted: a.auto_submitted.or(b.auto_submitted),
            precedence: a.precedence.or(b.precedence),
            from: Some("ada@example.com"),
            subject: a.subject.or(b.subject),
            text: a.text.or(b.text),
            answers_ours: a.answers_ours || b.answers_ours,
        }
    }

    /// The authority order is the contract of classification: for every pair of signals a
    /// message can carry, the stronger one decides, whatever the weaker one says. A new signal
    /// fails here until it has its place in the order.
    #[test]
    fn the_stronger_signal_always_decides() {
        for strong in Authority::iter() {
            for weak in Authority::iter().filter(|weak| *weak > strong) {
                let verdict = classify(&both(facts_with(strong), facts_with(weak)));
                assert_eq!(verdict.authority, strong, "{strong:?} over {weak:?}");
            }
            let alone = classify(&both(facts_with(strong), Facts::default()));
            let expected = match strong {
                Authority::Dsn => Classification::Bounce,
                Authority::Arf => Classification::Complaint,
                Authority::Automatic => Classification::AutoReply,
                Authority::UnsubscribeRequest | Authority::Notice => Classification::Unsubscribe,
                Authority::Reply => Classification::HumanReply,
                Authority::None => Classification::Unknown,
            };
            assert_eq!(alone.classification, expected, "{strong:?}");
        }
    }

    /// A report's own content decides between its two classifications: a moved address is an
    /// address change, not a bounce, and an abuse report that is not about consent (an
    /// authentication failure, "not spam") is no complaint, so it carries no complaint's effects.
    #[test]
    fn reports_classify_by_their_content() {
        let cases = [
            (
                Report::Dsn {
                    address_change: true,
                },
                Classification::AddressChange,
            ),
            (
                Report::Dsn {
                    address_change: false,
                },
                Classification::Bounce,
            ),
            (Report::Arf { complaint: true }, Classification::Complaint),
            (Report::Arf { complaint: false }, Classification::AutoReply),
        ];
        for (report, expected) in cases {
            let facts = Facts {
                report: Some(report),
                ..Facts::default()
            };
            assert_eq!(classify(&facts).classification, expected, "{report:?}");
        }
    }

    /// Every notice classifies as its kind and proposes what a person must confirm, addressed to
    /// the sender: a closed mailbox and a removal request propose a suppression for their
    /// reason, a person who left proposes the new address the notice gives (or the old one's
    /// suppression when it gives none), and an absence proposes nothing. A notice never carries
    /// an effect of its own.
    #[test]
    fn notices_propose_and_never_apply() {
        for notice in Notice::iter() {
            let phrase = notice.phrases().first().copied().unwrap_or_default();
            let text = format!("Hello, {phrase}. Regards");
            let facts = Facts {
                from: Some("ada@example.com"),
                text: Some(&text),
                answers_ours: true,
                ..Facts::default()
            };
            let verdict = classify(&facts);
            assert_eq!(verdict.authority, Authority::Notice, "{notice:?}");
            let (classification, proposal) = match notice {
                Notice::Closed => (
                    Classification::Bounce,
                    Some(ReviewProposal::Suppress {
                        email: "ada@example.com".to_owned(),
                        reason: SuppressionReason::AccountClosed,
                    }),
                ),
                Notice::Left => (
                    Classification::AddressChange,
                    Some(ReviewProposal::Suppress {
                        email: "ada@example.com".to_owned(),
                        reason: SuppressionReason::AddressChanged,
                    }),
                ),
                Notice::RemoveMe => (
                    Classification::Unsubscribe,
                    Some(ReviewProposal::Suppress {
                        email: "ada@example.com".to_owned(),
                        reason: SuppressionReason::Unsubscribe,
                    }),
                ),
                Notice::Absent => (Classification::OutOfOffice, None),
            };
            assert_eq!(verdict.classification, classification, "{notice:?}");
            assert_eq!(verdict.proposal, proposal, "{notice:?}");
        }
    }

    /// The new address of a "left" notice is the first address in the text other than the
    /// sender's, in any of the ways people write it; an automatic reply saying so still proposes
    /// it, because the markers decide only that a person did not write it.
    #[test]
    fn a_left_notice_proposes_the_new_address() {
        for text in [
            "I no longer work here. Please write to <Grace@Example.com>.",
            "I have left the company; contact grace@example.com instead.",
            "My new email address is mailto:grace@example.com, ada@example.com is closed soon.",
        ] {
            let facts = Facts {
                from: Some("ADA@example.com"),
                auto_submitted: Some("auto-replied"),
                text: Some(text),
                ..Facts::default()
            };
            let verdict = classify(&facts);
            assert_eq!(
                verdict.classification,
                Classification::AddressChange,
                "{text}"
            );
            let Some(ReviewProposal::ChangeAddress { email, new_email }) = verdict.proposal else {
                panic!("a change of address is proposed for {text}");
            };
            assert_eq!(email, "ADA@example.com");
            assert_eq!(
                new_email.map(|address| address.to_ascii_lowercase()),
                Some("grace@example.com".to_owned()),
                "{text}"
            );
        }
    }

    /// Our own words quoted in a reply never make it a notice: a reply that quotes our footer
    /// ("unsubscribe", "remove me") with `>` or below an "On … wrote:" line, or an Outlook header,
    /// stays a human reply.
    #[test]
    fn quoted_text_is_not_a_notice() {
        for text in [
            "Sounds good, let's talk Tuesday.\n> Not interested? Reply \"remove me\".",
            "Sounds good.\nOn Tue, 1 Oct 2026, Max <max@acme.example> wrote:\nReply remove me to stop.",
            "Sounds good.\nFrom: Max\nSent: Tuesday\nReply remove me to stop.",
        ] {
            let facts = Facts {
                text: Some(text),
                answers_ours: true,
                ..Facts::default()
            };
            assert_eq!(
                classify(&facts).classification,
                Classification::HumanReply,
                "{text}"
            );
        }
    }

    /// The automatic markers: any `Auto-Submitted` but `no`, and the four `Precedence` values of
    /// automatic mail; an absence notice among them is `out_of_office`.
    #[test]
    fn automatic_markers_and_what_they_carry() {
        let cases = [
            (Some("auto-replied"), None, None, Classification::AutoReply),
            (Some("no"), None, None, Classification::HumanReply),
            (None, Some("bulk"), None, Classification::AutoReply),
            (
                None,
                Some("auto_reply"),
                Some("Out of Office: Ada"),
                Classification::OutOfOffice,
            ),
            (None, Some("first-class"), None, Classification::HumanReply),
        ];
        for (auto_submitted, precedence, subject, expected) in cases {
            let facts = Facts {
                auto_submitted,
                precedence,
                subject,
                answers_ours: true,
                ..Facts::default()
            };
            assert_eq!(
                classify(&facts).classification,
                expected,
                "{auto_submitted:?} {precedence:?} {subject:?}"
            );
        }
    }

    /// Who answered, for every pair of signal and classification: a human reply, or a notice a
    /// person wrote to end the conversation, ends the person's enrollment and counts as a reply;
    /// the same classification decided by a report or an automatic reply does not (a bounce
    /// saying the address moved is no reply). Only what the rules left open goes to AI. A new
    /// signal or classification fails to compile here until it has its outcome.
    #[test]
    fn answers_and_what_ai_may_refine() {
        for authority in Authority::iter() {
            let person = match authority {
                Authority::Notice | Authority::Reply => true,
                Authority::Dsn
                | Authority::Arf
                | Authority::Automatic
                | Authority::UnsubscribeRequest
                | Authority::None => false,
            };
            for classification in Classification::iter() {
                let (ends, ai) = match classification {
                    Classification::HumanReply => (true, true),
                    Classification::Unsubscribe | Classification::AddressChange => (true, false),
                    Classification::Unknown => (false, true),
                    Classification::AutoReply
                    | Classification::OutOfOffice
                    | Classification::Bounce
                    | Classification::Complaint => (false, false),
                };
                let verdict = Verdict {
                    classification,
                    authority,
                    evidence: String::new(),
                    proposal: None,
                };
                assert_eq!(
                    verdict.is_answer(),
                    person && ends,
                    "{authority:?} {classification:?}"
                );
                assert_eq!(classification.open_to_ai(), ai, "{classification:?}");
            }
        }
    }

    /// Every action and status subject of a DSN maps to the event the evidence rules act on: a
    /// failed `5.1.1` is an invalid recipient (suppression or review), a full mailbox a hold in
    /// either action, a moved address its own kind, a delay transient, a positive report
    /// delivered; a report without an action is read from its status class.
    #[test]
    fn dsn_blocks_map_to_events() {
        let failed = Some(DsnAction::Failed);
        let delayed = Some(DsnAction::Delayed);
        let cases = [
            (
                failed,
                Some((5, 1, 1)),
                (EventKind::Bounced, Category::InvalidRecipient),
            ),
            (
                failed,
                Some((5, 1, 6)),
                (EventKind::AddressChanged, Category::AddressChanged),
            ),
            (
                failed,
                Some((5, 2, 2)),
                (EventKind::Bounced, Category::MailboxFull),
            ),
            (
                failed,
                Some((5, 4, 4)),
                (EventKind::Bounced, Category::NoRoute),
            ),
            (
                failed,
                Some((5, 7, 1)),
                (EventKind::Bounced, Category::Policy),
            ),
            (
                failed,
                Some((5, 3, 0)),
                (EventKind::Bounced, Category::Rejected),
            ),
            (failed, None, (EventKind::Bounced, Category::Rejected)),
            (
                delayed,
                Some((4, 2, 2)),
                (EventKind::Deferred, Category::MailboxFull),
            ),
            (
                delayed,
                Some((4, 7, 0)),
                (EventKind::Deferred, Category::Policy),
            ),
            (
                delayed,
                Some((4, 4, 1)),
                (EventKind::Deferred, Category::Transient),
            ),
            (
                Some(DsnAction::Delivered),
                None,
                (EventKind::Delivered, Category::Delivered),
            ),
            (
                Some(DsnAction::Relayed),
                None,
                (EventKind::Delivered, Category::Delivered),
            ),
            (
                Some(DsnAction::Expanded),
                None,
                (EventKind::Delivered, Category::Delivered),
            ),
            (
                None,
                Some((5, 1, 1)),
                (EventKind::Bounced, Category::InvalidRecipient),
            ),
            (
                None,
                Some((4, 4, 1)),
                (EventKind::Deferred, Category::Transient),
            ),
            (None, None, (EventKind::Reported, Category::Rejected)),
        ];
        for (action, status, expected) in cases {
            assert_eq!(
                dsn_outcome(action, status),
                expected,
                "{action:?} {status:?}"
            );
        }
    }

    /// A mail-borne report is `corroborated` only when all three proofs hold (our own mailbox,
    /// our message, its recipient); any partial match is `inferred`, and none is ever
    /// `authenticated`.
    #[test]
    fn a_report_is_corroborated_only_by_every_proof() {
        for bits in 0_u8..8 {
            let found = ReportMatch {
                own_binding: bits & 1 != 0,
                ours: bits & 2 != 0,
                recipient_in_envelope: bits & 4 != 0,
            };
            let expected = if bits == 7 {
                Confidence::Corroborated
            } else {
                Confidence::Inferred
            };
            assert_eq!(report_confidence(found), expected, "{found:?}");
        }
    }

    /// A directory row correlates only through the identity's own connection and only for an
    /// address of the original envelope (ignoring ASCII case); a stranger's mail naming a
    /// guessed provider id, or the right id read through another mailbox, is not correlated.
    #[test]
    fn the_directory_needs_the_connection_and_the_envelope() {
        let own = uuid::Uuid::from_u128(1);
        let other = uuid::Uuid::from_u128(2);
        let recipients = vec!["ada@example.com".to_owned(), "grace@example.com".to_owned()];
        assert!(directory_accepts(
            own,
            own,
            Some("Ada@Example.com"),
            &recipients
        ));
        assert!(!directory_accepts(
            other,
            own,
            Some("ada@example.com"),
            &recipients
        ));
        assert!(!directory_accepts(
            own,
            own,
            Some("mallory@example.com"),
            &recipients
        ));
        assert!(!directory_accepts(own, own, None, &recipients));
    }

    /// The poll schedule: the interval after a page, at once after a full page (whatever the
    /// draw), and after failures a wait of the inbox's retry policy: never sooner than the
    /// interval, within a step that doubles from twice the interval, never beyond an hour.
    #[test]
    fn polls_are_scheduled_by_their_outcome() {
        let start: Timestamp = "2026-10-02T10:00:00Z".parse().unwrap();
        let now: Timestamp = "2026-10-02T10:00:20Z".parse().unwrap();
        let interval = SignedDuration::from_mins(5);
        let at = |minutes: i64| {
            now.saturating_add(SignedDuration::from_mins(minutes))
                .unwrap()
        };
        assert_eq!(
            next_poll(start, now, interval, Polled::Page { full: false }, u64::MAX),
            start.saturating_add(interval).unwrap()
        );
        assert_eq!(
            next_poll(start, now, interval, Polled::Page { full: true }, u64::MAX),
            now
        );
        let steps = [(1, 10), (2, 20), (3, 40), (4, 60), (40, 60)];
        for (failures, minutes) in steps {
            let failed = Polled::Failed { failures };
            assert_eq!(
                next_poll(start, now, interval, failed, 0),
                at(5),
                "{failures}"
            );
            for draw in [1, 299_999, u64::MAX / 3, u64::MAX] {
                let next = next_poll(start, now, interval, failed, draw);
                assert!(
                    next >= at(5) && next <= at(minutes),
                    "{failures} failures, draw {draw}: {next}"
                );
            }
        }
    }
}
