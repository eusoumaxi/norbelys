use super::{FeedbackType, parse, returned};
use crate::testing::samples::{FBL_COMPLAINT, GMAIL_BOUNCE, RFC_5965};

/// The RFC's own example parses: an `abuse` report, its generator, and the reported message's
/// `Message-ID` even though the RFC writes it without angle brackets.
#[test]
fn the_rfc_5965_example_parses() {
    let arf = parse(RFC_5965.as_bytes()).expect("an ARF report");
    assert_eq!(arf.feedback_type, FeedbackType::Abuse);
    assert!(arf.feedback_type.is_complaint());
    assert_eq!(arf.user_agent.as_deref(), Some("SomeGenerator/1.0"));
    assert_eq!(
        arf.original_message_id.as_deref(),
        Some("8787KJKJ3K4J3K4J3K4J3.mail@example.net")
    );
    assert_eq!(arf.original_to.as_deref(), Some("<Undisclosed Recipients>"));
}

/// A feedback-loop complaint carries what correlates it to our message and recipient: the
/// original recipient, the envelope sender, the source IP, the reported domain, and from the
/// returned headers our `Message-ID` and `Feedback-ID`.
#[test]
fn a_feedback_loop_complaint_names_our_message_and_recipient() {
    let arf = parse(FBL_COMPLAINT.as_bytes()).expect("an ARF report");
    assert_eq!(arf.feedback_type, FeedbackType::Abuse);
    assert_eq!(arf.original_rcpt_to, ["grace@yahoo.example"]);
    assert_eq!(
        arf.original_mail_from.as_deref(),
        Some("bounces+m1@mail.example.com")
    );
    assert_eq!(arf.source_ip.as_deref(), Some("192.0.2.25"));
    assert_eq!(arf.reported_domains, ["example.com"]);
    assert_eq!(
        arf.original_message_id.as_deref(),
        Some("m1.t1.tag@mail.example.com")
    );
    assert_eq!(
        arf.feedback_id.as_deref(),
        Some("m1:campaign7:norbelys:esp")
    );
    assert_eq!(arf.original_to.as_deref(), Some("grace@yahoo.example"));
}

/// Only `abuse` and `fraud` are complaints about consent; `not-spam` withdraws one and
/// `auth-failure` or an unknown type is not about the recipient at all.
#[test]
fn only_abuse_and_fraud_are_complaints() {
    let with = |kind: &str| {
        FBL_COMPLAINT.replace("Feedback-Type: abuse", &format!("Feedback-Type: {kind}"))
    };
    let cases = [
        ("fraud", FeedbackType::Fraud, true),
        ("not-spam", FeedbackType::NotSpam, false),
        ("auth-failure", FeedbackType::AuthFailure, false),
        ("virus", FeedbackType::Virus, false),
        ("opt-out", FeedbackType::Other, false),
    ];
    for (kind, expected, complaint) in cases {
        let arf = parse(with(kind).as_bytes()).expect("an ARF report");
        assert_eq!(
            (arf.feedback_type, arf.feedback_type.is_complaint()),
            (expected, complaint),
            "{kind}"
        );
    }
}

/// A delivery status notification is not an abuse report.
#[test]
fn a_dsn_is_not_an_arf_report() {
    assert_eq!(parse(GMAIL_BOUNCE.as_bytes()), None);
}

/// What a report returns is handed over exactly as received, for the caller to verify the
/// signatures it carries: the whole returned message of a `message/rfc822` part, or the header
/// block of a `text/rfc822-headers` part. A message that is not a report returns nothing.
#[test]
fn hands_over_the_returned_message_as_received() {
    let headers =
        String::from_utf8(returned(FBL_COMPLAINT.as_bytes()).expect("the headers")).expect("ASCII");
    assert!(
        headers.starts_with("From: Ada <ada@example.com>\r\n"),
        "{headers}"
    );
    assert!(headers.contains("\r\nFeedback-ID: m1:campaign7:norbelys:esp\r\n"));
    let message =
        String::from_utf8(returned(RFC_5965.as_bytes()).expect("the message")).expect("ASCII");
    assert!(
        message.starts_with("Received: from mailserver.example.net\r\n"),
        "{message}"
    );
    assert!(message.contains("\r\n\r\nSpam Spam Spam"));
    assert_eq!(returned(GMAIL_BOUNCE.as_bytes()), None);
}
