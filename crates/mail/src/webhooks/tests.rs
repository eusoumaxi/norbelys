use jiff::{SignedDuration, Timestamp};

use super::{
    EventKind, MAX_SKEW, Provenance, VerifyError, event_id, fresh, smtp_code, unix_seconds,
};

/// A signed timestamp is accepted from the provider's whole retry window in the past up to five
/// minutes of clock skew in the future, and refused outside it: a replay older than the window
/// fails the signature check, one inside it is caught by the stored event key.
#[test]
fn freshness_spans_the_retry_window_and_five_minutes_of_skew() {
    let now = Timestamp::from_second(1_759_320_000).expect("a timestamp");
    let window = SignedDuration::from_hours(1);
    let at = |offset: i64| now.as_second() + offset;
    assert_eq!(fresh(at(-3_600), now, window), Ok(()));
    assert_eq!(fresh(at(-3_601), now, window), Err(VerifyError::Stale));
    assert_eq!(fresh(at(MAX_SKEW.as_secs()), now, window), Ok(()));
    assert_eq!(
        fresh(at(MAX_SKEW.as_secs() + 1), now, window),
        Err(VerifyError::Stale)
    );
}

/// Providers write times as whole or fractional Unix seconds (Mailgun's are fractional), SMTP
/// codes inside typed diagnostics, and event ids that become replay keys: each is read
/// strictly, a missing or unusable value never invented.
#[test]
fn provider_values_are_read_strictly() {
    assert_eq!(
        unix_seconds(Some(&serde_json::json!(1_521_243_339))),
        Timestamp::from_second(1_521_243_339).ok()
    );
    let fractional = unix_seconds(Some(&serde_json::json!(1_521_243_339.5))).expect("a time");
    assert_eq!(fractional.as_millisecond(), 1_521_243_339_500);
    assert_eq!(unix_seconds(Some(&serde_json::json!("1521243339"))), None);
    assert_eq!(smtp_code("smtp; 550 5.1.1 user unknown"), Some(550));
    assert_eq!(smtp_code("421 4.7.0 try later"), Some(421));
    assert_eq!(smtp_code("X-Postfix; delivery temporarily suspended"), None);
    assert_eq!(smtp_code("Recipient address rejected; 550"), None);
    assert_eq!(event_id(Some(" evt-1 ")), Ok("evt-1".to_owned()));
    assert!(event_id(Some("")).is_err());
    assert!(event_id(Some("has space")).is_err());
    assert!(event_id(Some(&"x".repeat(257))).is_err());
}

/// Event kinds and provenances are stored as text under constraints: their spellings are a
/// contract.
#[test]
fn kinds_and_provenances_keep_their_spellings() {
    let kinds = [
        EventKind::Accepted,
        EventKind::Deferred,
        EventKind::Delivered,
        EventKind::Bounced,
        EventKind::Rejected,
        EventKind::Complaint,
        EventKind::Unsubscribed,
    ];
    let spelled: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
    assert_eq!(
        spelled,
        [
            "accepted",
            "deferred",
            "delivered",
            "bounced",
            "rejected",
            "complaint",
            "unsubscribed"
        ]
    );
    let provenances = [
        Provenance::SmtpReply,
        Provenance::VerpDsn,
        Provenance::FblArfDkim,
        Provenance::FeedbackIdOnly,
    ];
    let spelled: Vec<&str> = provenances
        .iter()
        .map(|provenance| provenance.as_str())
        .collect();
    assert_eq!(
        spelled,
        ["smtp_reply", "verp_dsn", "fbl_arf_dkim", "feedback_id_only"]
    );
}
