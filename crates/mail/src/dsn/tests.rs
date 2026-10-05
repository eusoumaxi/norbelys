use super::{Action, StreamReducer, parse};
use crate::status::EnhancedStatus;
use crate::testing::samples::{
    EXCHANGE_NDR, GMAIL_BOUNCE, HUMAN_REPLY, POSTFIX_DELAY, RFC_3464, RFC_5965,
};

fn status(text: &str) -> Option<EnhancedStatus> {
    text.parse().ok()
}

/// The RFC's own example parses into its fields: the reporting server without its `dns;` type,
/// the recipient without its `rfc822;` type, the action and status, the SMTP code inside an
/// `smtp;` diagnostic, and the returned message's `Message-ID` that correlates the bounce.
#[test]
fn the_rfc_3464_example_parses() {
    let dsn = parse(RFC_3464.as_bytes()).expect("a DSN");
    assert_eq!(dsn.reporting_mta.as_deref(), Some("cs.utk.edu"));
    assert_eq!(
        dsn.original_message_id.as_deref(),
        Some("199407021710.RAA0001@CS.UTK.EDU")
    );
    let [recipient] = dsn.recipients.as_slice() else {
        panic!("one recipient: {:?}", dsn.recipients)
    };
    assert_eq!(
        recipient.final_recipient.as_deref(),
        Some("louisl@larry.slip.umd.edu")
    );
    assert_eq!(
        recipient.original_recipient.as_deref(),
        Some("louisl@larry.slip.umd.edu")
    );
    assert_eq!(recipient.action, Some(Action::Failed));
    assert_eq!(recipient.status, status("4.0.0"));
    assert_eq!(
        recipient.diagnostic.as_deref(),
        Some("426 connection timed out")
    );
    assert_eq!(recipient.smtp_code, Some(426));
}

/// Gmail folds its diagnostic over several lines: the continuation lines are unfolded into one
/// value, so the whole explanation and its code survive.
#[test]
fn a_gmail_bounce_unfolds_its_diagnostic() {
    let dsn = parse(GMAIL_BOUNCE.as_bytes()).expect("a DSN");
    assert_eq!(
        dsn.original_message_id.as_deref(),
        Some("m1.t1.tag@mail.example.com")
    );
    let [recipient] = dsn.recipients.as_slice() else {
        panic!("one recipient: {:?}", dsn.recipients)
    };
    assert_eq!(
        recipient.final_recipient.as_deref(),
        Some("ghost@example.org")
    );
    assert_eq!(recipient.status, status("5.1.1"));
    assert_eq!(recipient.smtp_code, Some(550));
    let diagnostic = recipient.diagnostic.as_deref().unwrap_or_default();
    assert!(
        diagnostic.starts_with("550-5.1.1 The email account"),
        "{diagnostic}"
    );
    assert!(diagnostic.ends_with("?p=NoSuchUser"), "{diagnostic}");
}

/// Exchange Online reports several recipients in one notification and returns only the original
/// headers: each recipient keeps its own status (a full mailbox is not an unknown user), and the
/// `Message-ID` is read from the `text/rfc822-headers` part.
#[test]
fn an_exchange_report_keeps_each_recipient_and_the_returned_headers() {
    let dsn = parse(EXCHANGE_NDR.as_bytes()).expect("a DSN");
    assert_eq!(
        dsn.original_message_id.as_deref(),
        Some("m2.t1.tag@mail.example.com")
    );
    let found: Vec<(Option<&str>, Option<EnhancedStatus>, Option<u16>)> = dsn
        .recipients
        .iter()
        .map(|recipient| {
            (
                recipient.final_recipient.as_deref(),
                recipient.status,
                recipient.smtp_code,
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (Some("full@contoso.com"), status("5.2.2"), Some(554)),
            (Some("gone@contoso.com"), status("5.1.10"), Some(550))
        ]
    );
}

/// A delay warning is a DSN too, with `Action: delayed` and a transient status; a diagnostic of a
/// non-SMTP type carries no SMTP code.
#[test]
fn a_delay_warning_is_reported_as_delayed() {
    let dsn = parse(POSTFIX_DELAY.as_bytes()).expect("a DSN");
    let [recipient] = dsn.recipients.as_slice() else {
        panic!("one recipient: {:?}", dsn.recipients)
    };
    assert_eq!(recipient.action, Some(Action::Delayed));
    assert_eq!(recipient.status, status("4.4.7"));
    assert_eq!(recipient.smtp_code, None);
    assert_eq!(
        dsn.original_message_id.as_deref(),
        Some("m3.t2.tag@mail.example.com")
    );
}

/// Some servers omit the blank line between the message fields and the first recipient; that
/// recipient is still read rather than lost.
#[test]
fn a_report_without_the_separating_blank_line_keeps_its_recipient() {
    let raw = "Content-Type: multipart/report; report-type=delivery-status; boundary=\"b\"\r
\r
--b\r
Content-Type: message/delivery-status\r
\r
Reporting-MTA: dns; mx.example.net\r
Final-Recipient: rfc822; ghost@example.org\r
Action: failed\r
Status: 5.1.1\r
--b--\r
";
    let dsn = parse(raw.as_bytes()).expect("a DSN");
    assert_eq!(dsn.reporting_mta.as_deref(), Some("mx.example.net"));
    assert_eq!(dsn.recipients.len(), 1);
}

/// Only `multipart/report` messages of the delivery-status type are DSNs: an ordinary reply and
/// an abuse report are not, whatever their wording.
#[test]
fn other_messages_are_not_dsns() {
    assert_eq!(parse(HUMAN_REPLY.as_bytes()), None);
    assert_eq!(parse(RFC_5965.as_bytes()), None);
}

/// The stored spellings of the actions are a contract with the database.
#[test]
fn actions_keep_their_stored_spellings() {
    let all = [
        Action::Failed,
        Action::Delayed,
        Action::Delivered,
        Action::Relayed,
        Action::Expanded,
    ];
    let spelled: Vec<&str> = all.iter().map(|action| action.as_str()).collect();
    assert_eq!(
        spelled,
        ["failed", "delayed", "delivered", "relayed", "expanded"]
    );
    for action in all {
        assert_eq!(Action::parse(action.as_str()), Some(action));
    }
    assert_eq!(Action::parse("bounced"), None);
}

/// Internationalised reports (RFC 6533: `message/global-delivery-status` and returned
/// `message/global-headers`) are read like the ASCII ones, UTF-8 addresses included.
#[test]
fn internationalised_reports_are_read() {
    let raw = "Content-Type: multipart/report; report-type=global-delivery-status; boundary=\"g\"\r
\r
--g\r
Content-Type: message/global-delivery-status\r
\r
Reporting-MTA: dns; mx.example.net\r
\r
Final-Recipient: utf-8; josé@example.org\r
Action: failed\r
Status: 5.1.1\r
\r
--g\r
Content-Type: message/global-headers\r
\r
Message-ID: <m4.t1.tag@mail.example.com>\r
\r
--g--\r
";
    let dsn = parse(raw.as_bytes()).expect("a DSN");
    let recipients: Vec<Option<&str>> = dsn
        .recipients
        .iter()
        .map(|recipient| recipient.final_recipient.as_deref())
        .collect();
    assert_eq!(recipients, [Some("josé@example.org")]);
    assert_eq!(
        dsn.original_message_id.as_deref(),
        Some("m4.t1.tag@mail.example.com")
    );
}

#[test]
fn streaming_discards_large_returned_bodies_and_preserves_evidence() {
    let mut stream = StreamReducer::new(8192);
    for line in [
        b"Content-Type: multipart/report; boundary=report; report-type=delivery-status".as_slice(),
        b"",
        b"--report",
        b"Content-Type: text/plain",
        b"",
    ] {
        stream.line(line);
    }
    let large = vec![b'x'; 8192];
    for _ in 0..1024 {
        stream.line(&large);
    }
    for line in [
        b"--report".as_slice(),
        b"Content-Type: message/delivery-status",
        b"",
        b"Reporting-MTA: dns; example.test",
        b"",
        b"Final-Recipient: rfc822; test@example.test",
        b"Action: failed",
        b"Status: 5.1.1",
        b"",
        b"--report",
        b"Content-Type: message/rfc822",
        b"",
        b"Message-ID: <original@example.test>",
        b"",
    ] {
        stream.line(line);
    }
    for _ in 0..1024 {
        stream.line(&large);
    }
    stream.line(b"--report--");
    let retained = stream.finish().unwrap();
    assert!(retained.len() < 8192);
    let report = parse(&retained).unwrap();
    assert_eq!(
        report.original_message_id.as_deref(),
        Some("original@example.test")
    );
    assert_eq!(report.recipients.len(), 1);
    assert_eq!(report.recipients[0].action, Some(Action::Failed));
}

#[test]
fn streaming_refuses_oversized_evidence_and_preserves_non_reports() {
    let mut stream = StreamReducer::new(100);
    stream.line(b"Content-Type: text/plain");
    stream.line(b"");
    stream.line(&[b'x'; 100]);
    assert!(stream.finish().is_none());
    let mut stream = StreamReducer::new(1024);
    for line in [
        b"Content-Type: text/plain".as_slice(),
        b"",
        b"ordinary message",
    ] {
        stream.line(line);
    }
    assert_eq!(
        stream.finish().unwrap(),
        b"Content-Type: text/plain\r\n\r\nordinary message\r\n"
    );
}
