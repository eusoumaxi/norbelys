use super::{Report, read};
use crate::testing::samples::{FBL_COMPLAINT, GMAIL_BOUNCE, HUMAN_REPLY};

/// A reply yields what correlation needs, without angle brackets: its own id, the id it answers
/// and the folded `References` in order; the sender with its decoded name; the decoded subject;
/// the date; and an excerpt of the text version with its line breaks.
#[test]
fn a_reply_yields_its_ids_sender_and_excerpt() {
    let inbound = read(HUMAN_REPLY.as_bytes(), 1_000).expect("a message");
    assert_eq!(
        inbound.message_id.as_deref(),
        Some("CAF=reply-1@mail.example.org")
    );
    assert_eq!(inbound.in_reply_to, ["m2.t1.tag@mail.example.com"]);
    assert_eq!(
        inbound.references,
        ["m1.t1.tag@mail.example.com", "m2.t1.tag@mail.example.com"]
    );
    let sender = inbound.from.expect("a sender");
    assert_eq!(
        (sender.name.as_deref(), sender.address.as_str()),
        (Some("Grace Hopper"), "grace@example.org")
    );
    assert_eq!(
        inbound.subject.as_deref(),
        Some("Re: Quick question — thanks")
    );
    assert_eq!(
        inbound.date.map(|date| date.to_string()).as_deref(),
        Some("2026-10-01T12:30:00Z")
    );
    assert_eq!(
        inbound.excerpt.as_deref(),
        Some("Sounds good, let's talk Friday.\n\n> earlier text")
    );
    assert_eq!(inbound.auto_submitted, None);
    assert_eq!(inbound.report, None);
}

/// Automatic replies are recognised by their markers: `Auto-Submitted` (RFC 3834) and
/// `Precedence`, read as lowercased tokens; an HTML-only body still yields a text excerpt, cut at
/// the caller's bound.
#[test]
fn automatic_mail_markers_and_html_bodies_are_read() {
    let raw = "From: Grace <grace@example.org>\r
Subject: Out of office\r
Auto-Submitted: Auto-Replied; owner-email=grace@example.org\r
Precedence: Bulk\r
Content-Type: text/html; charset=UTF-8\r
\r
<html><body><p>I am away until Monday.</p></body></html>\r
";
    let inbound = read(raw.as_bytes(), 9).expect("a message");
    assert_eq!(inbound.auto_submitted.as_deref(), Some("auto-replied"));
    assert_eq!(inbound.precedence.as_deref(), Some("bulk"));
    assert_eq!(inbound.excerpt.as_deref(), Some("I am away"));
}

/// Bounces and complaints are recognised as such, so classification can rank them above any
/// wording in their bodies.
#[test]
fn reports_are_recognised() {
    let bounce = read(GMAIL_BOUNCE.as_bytes(), 100).expect("a message");
    assert!(matches!(bounce.report, Some(Report::Dsn(_))));
    let complaint = read(FBL_COMPLAINT.as_bytes(), 100).expect("a message");
    assert!(matches!(complaint.report, Some(Report::Arf(_))));
}
