//! What one provider event is as a delivery event: the pure rules the normaliser applies to every
//! event a provider's signed callback carried.
//!
//! # Kind and category
//!
//! A provider names what it observed in its own vocabulary, which the mail crate reduces to a
//! closed set ([`norbelys_mail::webhooks::EventKind`]). Most of it maps one to one: an acceptance,
//! a delivery to the next hop, a complaint, an unsubscribe. A failure says more through its
//! enhanced status (RFC 3463, <https://www.rfc-editor.org/rfc/rfc3463>), and a bounce or a
//! deferral reported by a provider means what the same status means in a delivery status
//! notification read from a mailbox, so both go through the one table of what a failure status
//! means (`domain::inbox::dsn_outcome`): a full mailbox (`X.2.2`) holds the recipient, a moved
//! address (`X.1.6`) asks a person, an address that does not exist (`X.1.x`) may suppress it, no
//! route (`X.4.x`), policy (`X.7.x`). A refusal the provider decided on its own (its suppression
//! list, a policy block, a retry window it gave up on) proves nothing about the address, so it is
//! never an invalid recipient whatever status it quotes: only a full mailbox and policy are read
//! from its status.
//!
//! # Confidence
//!
//! The webhook's signature authenticates the provider, so a relay's event (Amazon SES, SendGrid,
//! Mailgun) about a message we can name is `authenticated`: the provider is the party that
//! talked to the next hop. The managed MTA's signature authenticates the MTA, not the party that
//! reported to it, so its event carries how it learned what it says: the next hop's SMTP reply
//! and an abuse report from a feedback loop whose DKIM signature the MTA verified are
//! `authenticated`; a status notification returned to the message's VERP address and an abuse
//! report matched only by its signed `Feedback-ID` prove the message was ours but not who wrote
//! the report, so they are `corroborated`. An event that names none of our messages (no tag of
//! ours, no Message-ID we composed, or a message this connection did not send) matches our
//! records only partly: it is `inferred`, kept for review and counters, and never acts alone.

use norbelys_mail::dsn::Action as DsnAction;
use norbelys_mail::status::EnhancedStatus;
use norbelys_mail::webhooks::{EventKind as ProviderKind, Provenance};

use crate::domain::inbox::dsn_outcome;
use crate::domain::policy::delivery::{Category, Confidence, EventKind};

/// The delivery event's kind and category for a provider event of `kind` quoting `status`.
#[must_use]
pub fn outcome(kind: ProviderKind, status: Option<EnhancedStatus>) -> (EventKind, Category) {
    let status = status.map(|status| (status.class(), status.subject(), status.detail()));
    match kind {
        ProviderKind::Accepted => (EventKind::Accepted, Category::Accepted),
        ProviderKind::Delivered => (EventKind::Delivered, Category::Delivered),
        ProviderKind::Complaint => (EventKind::Complaint, Category::Complaint),
        ProviderKind::Unsubscribed => (EventKind::Unsubscribed, Category::Unsubscribed),
        ProviderKind::Bounced => dsn_outcome(Some(DsnAction::Failed), status),
        ProviderKind::Deferred => dsn_outcome(Some(DsnAction::Delayed), status),
        ProviderKind::Rejected => match status.map(|(_, subject, detail)| (subject, detail)) {
            Some((2, 2)) => (EventKind::Rejected, Category::MailboxFull),
            Some((7, _)) => (EventKind::Rejected, Category::Policy),
            _ => (EventKind::Rejected, Category::Rejected),
        },
    }
}

/// How far a provider event can be trusted: by how its reporter learned it (`provenance`, the
/// managed MTA's; `None` for a relay, whose events are its own signed observations) and whether
/// it names one of our messages sent through the webhook's connection (`matched`).
#[must_use]
pub fn confidence(provenance: Option<Provenance>, matched: bool) -> Confidence {
    if !matched {
        return Confidence::Inferred;
    }
    match provenance {
        None | Some(Provenance::SmtpReply | Provenance::FblArfDkim) => Confidence::Authenticated,
        Some(Provenance::VerpDsn | Provenance::FeedbackIdOnly) => Confidence::Corroborated,
    }
}

/// A provider's bounded diagnostic may include a stable Jellyfish code. Return that exact code
/// for a message's history without interpreting an undocumented code as a recipient failure.
/// Text without a standalone `JFE` followed by six digits has no provider code.
#[must_use]
pub fn provider_code(diagnostic: &str) -> Option<String> {
    diagnostic.match_indices("JFE").find_map(|(start, _)| {
        let code = diagnostic.get(start..start + 9)?;
        let digits = code.as_bytes().get(3..)?;
        if digits.len() != 6 || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let before = diagnostic.get(..start)?.chars().next_back();
        let after = diagnostic.get(start + 9..)?.chars().next();
        if before.is_some_and(char::is_alphanumeric) || after.is_some_and(char::is_alphanumeric) {
            return None;
        }
        Some(code.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::delivery::{
        DeliverySettings, HoldReason, RecipientEffect, RecipientRef, recipient_effect,
    };
    use crate::domain::suppressions::Reason as SuppressionReason;

    /// The two codes seen in different failure classes remain distinct, while arbitrary digits
    /// and longer tokens never become a misleading provider reason in a message's history.
    #[test]
    fn jellyfish_codes_are_exact_and_optional() {
        for (diagnostic, expected) in [
            ("550 5.7.1 filtered (JFE040004)", Some("JFE040004")),
            (
                "550 5.7.1 invalid recipients (JFE050004)",
                Some("JFE050004"),
            ),
            ("550 5.7.1 policy", None),
            ("550 5.7.1 (JFE0500049)", None),
            ("550 5.7.1 xJFE040004", None),
            ("550 5.7.1 (JFE0400x4)", None),
        ] {
            assert_eq!(provider_code(diagnostic).as_deref(), expected);
        }
    }

    /// Every provider kind, kept complete by [`exhaustive`].
    const KINDS: [ProviderKind; 7] = [
        ProviderKind::Accepted,
        ProviderKind::Deferred,
        ProviderKind::Delivered,
        ProviderKind::Bounced,
        ProviderKind::Rejected,
        ProviderKind::Complaint,
        ProviderKind::Unsubscribed,
    ];

    /// Every provenance, and none (a relay), kept complete by [`exhaustive`].
    const PROVENANCES: [Option<Provenance>; 5] = [
        None,
        Some(Provenance::SmtpReply),
        Some(Provenance::VerpDsn),
        Some(Provenance::FblArfDkim),
        Some(Provenance::FeedbackIdOnly),
    ];

    /// Fails to compile when the mail crate adds a kind or a provenance, so the lists above (and
    /// the expected tables below) get its row.
    fn exhaustive(kind: ProviderKind, provenance: Provenance) {
        match kind {
            ProviderKind::Accepted
            | ProviderKind::Deferred
            | ProviderKind::Delivered
            | ProviderKind::Bounced
            | ProviderKind::Rejected
            | ProviderKind::Complaint
            | ProviderKind::Unsubscribed => {}
        }
        match provenance {
            Provenance::SmtpReply
            | Provenance::VerpDsn
            | Provenance::FblArfDkim
            | Provenance::FeedbackIdOnly => {}
        }
    }

    fn status(text: &str) -> Option<EnhancedStatus> {
        EnhancedStatus::find(text)
    }

    /// Each provider kind, with no status, becomes the delivery event its name says; a failure
    /// without a status is a plain rejection or transient wait, never a claim about the address.
    #[test]
    fn every_kind_has_its_event_without_a_status() {
        let _ = exhaustive;
        for kind in KINDS {
            let expected = match kind {
                ProviderKind::Accepted => (EventKind::Accepted, Category::Accepted),
                ProviderKind::Deferred => (EventKind::Deferred, Category::Transient),
                ProviderKind::Delivered => (EventKind::Delivered, Category::Delivered),
                ProviderKind::Bounced => (EventKind::Bounced, Category::Rejected),
                ProviderKind::Rejected => (EventKind::Rejected, Category::Rejected),
                ProviderKind::Complaint => (EventKind::Complaint, Category::Complaint),
                ProviderKind::Unsubscribed => (EventKind::Unsubscribed, Category::Unsubscribed),
            };
            assert_eq!(outcome(kind, None), expected, "{kind:?}");
        }
    }

    /// A bounce's status decides its category as a status notification's would (so a provider's
    /// bounce and a DSN for the same address agree), while a refusal the provider decided itself
    /// never becomes an invalid recipient or a missing route, whatever status it quotes: only the
    /// mailbox and policy are read from it.
    #[test]
    fn failures_read_their_status_by_who_decided() {
        let cases = [
            (
                "5.1.1",
                (EventKind::Bounced, Category::InvalidRecipient),
                (EventKind::Rejected, Category::Rejected),
            ),
            (
                "5.1.6",
                (EventKind::AddressChanged, Category::AddressChanged),
                (EventKind::Rejected, Category::Rejected),
            ),
            (
                "5.2.2",
                (EventKind::Bounced, Category::MailboxFull),
                (EventKind::Rejected, Category::MailboxFull),
            ),
            (
                "5.4.4",
                (EventKind::Bounced, Category::NoRoute),
                (EventKind::Rejected, Category::Rejected),
            ),
            (
                "5.7.1",
                (EventKind::Bounced, Category::Policy),
                (EventKind::Rejected, Category::Policy),
            ),
            (
                "5.3.0",
                (EventKind::Bounced, Category::Rejected),
                (EventKind::Rejected, Category::Rejected),
            ),
        ];
        for (text, bounced, rejected) in cases {
            assert_eq!(
                outcome(ProviderKind::Bounced, status(text)),
                bounced,
                "{text}"
            );
            assert_eq!(
                outcome(ProviderKind::Rejected, status(text)),
                rejected,
                "{text}"
            );
        }
        assert_eq!(
            outcome(ProviderKind::Deferred, status("4.2.2")),
            (EventKind::Deferred, Category::MailboxFull)
        );
        assert_eq!(
            outcome(ProviderKind::Deferred, status("4.7.1")),
            (EventKind::Deferred, Category::Policy)
        );
    }

    /// The confidence table: a relay's signed event about our message is authenticated; the
    /// managed MTA's is authenticated only when the MTA itself read the reply or verified the
    /// reporter, corroborated when the report merely matched our message; and any event that
    /// names none of our messages is inferred, whoever signed it.
    #[test]
    fn confidence_follows_the_reporter_and_the_match() {
        for provenance in PROVENANCES {
            let expected = match provenance {
                None | Some(Provenance::SmtpReply | Provenance::FblArfDkim) => {
                    Confidence::Authenticated
                }
                Some(Provenance::VerpDsn | Provenance::FeedbackIdOnly) => Confidence::Corroborated,
            };
            assert_eq!(confidence(provenance, true), expected, "{provenance:?}");
            assert_eq!(
                confidence(provenance, false),
                Confidence::Inferred,
                "{provenance:?}"
            );
        }
    }

    /// End to end through the evidence rules: a relay's signed hard bounce naming its recipient
    /// suppresses the address; the same bounce returned to the MTA's VERP address holds the
    /// address while a person reviews it; a relay's own refusal never suppresses; and an
    /// unmatched complaint never acts.
    #[test]
    fn provider_events_act_only_as_far_as_they_are_trusted() {
        let effect = |kind, text, provenance, matched| {
            let (kind, category) = outcome(kind, status(text));
            recipient_effect(
                kind,
                category,
                RecipientRef::Named,
                confidence(provenance, matched),
                DeliverySettings::default(),
            )
        };
        assert_eq!(
            effect(ProviderKind::Bounced, "5.1.1", None, true),
            RecipientEffect::Suppress(SuppressionReason::Bounce)
        );
        assert!(matches!(
            effect(
                ProviderKind::Bounced,
                "5.1.1",
                Some(Provenance::VerpDsn),
                true
            ),
            RecipientEffect::HoldAndReview(HoldReason::InvalidRecipient, _)
        ));
        assert_eq!(
            effect(ProviderKind::Rejected, "5.1.1", None, true),
            RecipientEffect::None
        );
        assert!(matches!(
            effect(ProviderKind::Complaint, "", None, false),
            RecipientEffect::Review(_)
        ));
    }
}
