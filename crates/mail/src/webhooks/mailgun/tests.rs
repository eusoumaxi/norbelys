use aws_lc_rs::hmac;
use jiff::Timestamp;
use secrecy::SecretString;
use serde_json::{Value, json};

use super::{MailgunKey, events, verify};
use crate::compose::MESSAGE_TAG;
use crate::webhooks::{EventKind, VerifyError};

const SIGNING_KEY: &str = "key-3ax6xnjp29jd6fds4gc373sgvjxteol0";
const NOW: i64 = 1_759_320_000;

fn now() -> Timestamp {
    Timestamp::from_second(NOW).expect("a timestamp")
}

fn key() -> MailgunKey {
    MailgunKey::new(&SecretString::from(SIGNING_KEY.to_owned())).expect("a key")
}

/// A webhook body signed the way Mailgun signs: the hex HMAC-SHA256 of timestamp and token.
fn signed_body(timestamp: i64, token: &str, data: Value) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, SIGNING_KEY.as_bytes());
    let tag = hmac::sign(&key, format!("{timestamp}{token}").as_bytes());
    let signature: String = tag
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({
        "signature": {"timestamp": timestamp.to_string(), "token": token, "signature": signature},
        "event-data": data,
    })
    .to_string()
    .into_bytes()
}

fn delivered() -> Value {
    json!({"id": "CPgfbmQMTCKtHW6uIWtuVe", "event": "delivered", "timestamp": 1_759_319_000.5, "recipient": "grace@example.org"})
}

/// A body signed with the account's key verifies and becomes one receipt keyed by the event's
/// id, holding the event without its signature: a retry Mailgun signed anew (another timestamp
/// and token) yields the very same receipt, so the caller recognises it as a replay.
#[test]
fn a_signed_body_becomes_one_receipt() {
    let body = signed_body(
        NOW - 60,
        "a8ce0edb2dd8301dee6c2405235584e45aa91d1e9f979f3de0",
        delivered(),
    );
    let receipt = verify(&key(), &body, now()).expect("verified");
    assert_eq!(receipt.event_id, "CPgfbmQMTCKtHW6uIWtuVe");
    let raw: Value = serde_json::from_slice(&receipt.raw).expect("JSON");
    assert_eq!(raw, json!({"event-data": delivered()}));
    let retry = signed_body(NOW - 30, "0f1e2d3c4b5a69788796a5b4c3d2e1f0", delivered());
    assert_eq!(verify(&key(), &retry, now()).expect("verified"), receipt);
    assert_eq!(events(&receipt.raw).expect("parsed").len(), 1);
}

/// Anything that is not Mailgun's own signature, fresh, over an identifiable event is refused:
/// another key, a changed token, a timestamp outside the window, a body without an event id, a
/// body above the size bound.
#[test]
fn unsigned_stale_or_unidentifiable_bodies_are_refused() {
    let token = "a8ce0edb2dd8301dee6c2405235584e45aa91d1e9f979f3de0";
    let other = MailgunKey::new(&SecretString::from("key-another".to_owned())).expect("a key");
    let body = signed_body(NOW - 60, token, delivered());
    assert!(matches!(
        verify(&other, &body, now()),
        Err(VerifyError::Unauthorized(_))
    ));
    let tampered = String::from_utf8(body.clone())
        .expect("UTF-8")
        .replacen(token, "b8ce0edb", 1);
    assert!(matches!(
        verify(&key(), tampered.as_bytes(), now()),
        Err(VerifyError::Unauthorized(_))
    ));
    let stale = signed_body(NOW - 13 * 3_600, token, delivered());
    assert_eq!(verify(&key(), &stale, now()), Err(VerifyError::Stale));
    let anonymous = signed_body(NOW, token, json!({"event": "delivered"}));
    assert!(matches!(
        verify(&key(), &anonymous, now()),
        Err(VerifyError::InvalidPayload(_))
    ));
    let huge = vec![b' '; super::super::MAX_BODY + 1];
    assert_eq!(verify(&key(), &huge, now()), Err(VerifyError::TooLarge));
    assert!(MailgunKey::new(&SecretString::from(" ".to_owned())).is_err());
}

/// Mailgun's events map onto Norbelys's kinds without overstating them: only a permanent failure
/// whose reason is a bounce is a bounce; its own suppression lists or an expired retry window are
/// rejections; temporary failures are deferrals; opens and clicks produce nothing.
#[test]
fn events_map_onto_kinds() {
    let cases = [
        (json!({"event": "accepted"}), Some(EventKind::Accepted)),
        (json!({"event": "delivered"}), Some(EventKind::Delivered)),
        (
            json!({"event": "failed", "severity": "temporary"}),
            Some(EventKind::Deferred),
        ),
        (
            json!({"event": "failed", "severity": "permanent", "reason": "bounce"}),
            Some(EventKind::Bounced),
        ),
        (
            json!({"event": "failed", "severity": "permanent", "reason": "suppress-bounce"}),
            Some(EventKind::Rejected),
        ),
        (
            json!({"event": "failed", "severity": "permanent", "reason": "old"}),
            Some(EventKind::Rejected),
        ),
        (json!({"event": "rejected"}), Some(EventKind::Rejected)),
        (json!({"event": "complained"}), Some(EventKind::Complaint)),
        (
            json!({"event": "unsubscribed"}),
            Some(EventKind::Unsubscribed),
        ),
        (json!({"event": "opened"}), None),
    ];
    for (mut data, expected) in cases {
        if let Some(object) = data.as_object_mut() {
            object.insert("id".to_owned(), json!("evt"));
            object.insert("timestamp".to_owned(), json!(1_759_319_000));
        }
        let raw = json!({"event-data": data}).to_string();
        let kinds: Vec<EventKind> = events(raw.as_bytes())
            .expect("parsed")
            .iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(kinds, expected.into_iter().collect::<Vec<_>>(), "{raw}");
    }
}

/// A bounce carries what the evidence needs: our message id from the user variable, the
/// recipient, the enhanced status and SMTP code from Mailgun's delivery status, the diagnostic,
/// the `Message-ID` and the fractional observation time.
#[test]
fn a_bounce_carries_its_evidence() {
    let message = "0192e3a4-0000-7000-8000-000000000001";
    let raw = json!({"event-data": {
        "id": "G9Bn5sl1TC6nu79C8C0bwg",
        "event": "failed",
        "severity": "permanent",
        "reason": "bounce",
        "timestamp": 1_759_319_000.25,
        "recipient": "ghost@example.org",
        "message": {"headers": {"message-id": "m1.t1.tag@mail.example.com"}},
        "delivery-status": {"code": 550, "message": "5.1.1 The email account that you tried to reach does not exist"},
        "user-variables": {MESSAGE_TAG: message},
    }})
    .to_string();
    let parsed = events(raw.as_bytes()).expect("parsed");
    let [event] = parsed.as_slice() else {
        panic!("one event: {parsed:?}")
    };
    assert_eq!(event.kind, EventKind::Bounced);
    assert_eq!(
        event.message_id.map(|id| id.to_string()).as_deref(),
        Some(message)
    );
    assert_eq!(event.recipient.as_deref(), Some("ghost@example.org"));
    assert_eq!(
        event.status.map(|status| status.to_string()).as_deref(),
        Some("5.1.1")
    );
    assert_eq!(event.smtp_code, Some(550));
    assert_eq!(
        event.internet_message_id.as_deref(),
        Some("m1.t1.tag@mail.example.com")
    );
    assert_eq!(event.observed_at.as_millisecond(), 1_759_319_000_250);
}
