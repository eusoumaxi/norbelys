use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderMap, HeaderValue};
use jiff::Timestamp;
use serde_json::json;

use super::{SendgridKey, events, verify};
use crate::compose::MESSAGE_TAG;
use crate::webhooks::{EventKind, VerifyError};

const NOW: i64 = 1_759_320_000;

/// A SendGrid-style key pair: the private half signs like SendGrid does, the public half is what
/// its settings page shows (base64 of a DER `SubjectPublicKeyInfo`).
fn key_pair() -> (EcdsaKeyPair, SendgridKey) {
    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).expect("a P-256 key");
    let public = pair.public_key().as_der().expect("an SPKI");
    let key = SendgridKey::new(&STANDARD.encode(public.as_ref())).expect("a verification key");
    (pair, key)
}

fn signed_headers(pair: &EcdsaKeyPair, timestamp: i64, body: &[u8]) -> HeaderMap {
    let mut message = timestamp.to_string().into_bytes();
    message.extend_from_slice(body);
    let signature = pair
        .sign(&SystemRandom::new(), &message)
        .expect("a signature");
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-twilio-email-event-webhook-signature",
        HeaderValue::from_str(&STANDARD.encode(signature.as_ref())).expect("a header"),
    );
    headers.insert(
        "x-twilio-email-event-webhook-timestamp",
        HeaderValue::from_str(&timestamp.to_string()).expect("a header"),
    );
    headers
}

fn now() -> Timestamp {
    Timestamp::from_second(NOW).expect("a timestamp")
}

/// A batch signed with SendGrid's key over its timestamp and exact body verifies, and each event
/// becomes a receipt keyed by `sg_event_id` holding that element's exact bytes, so a later replay
/// of one event is recognised on its own.
#[test]
fn a_signed_batch_becomes_one_receipt_per_event() {
    let (pair, key) = key_pair();
    let body = br#"[{"email":"a@example.org","event":"processed","sg_event_id":"e1","timestamp":1759319000},{"email":"b@example.org","event":"delivered","sg_event_id":"e2","timestamp":1759319001}]"#;
    let receipts =
        verify(&key, &signed_headers(&pair, NOW - 30, body), body, now()).expect("verified");
    let keys: Vec<&str> = receipts
        .iter()
        .map(|receipt| receipt.event_id.as_str())
        .collect();
    assert_eq!(keys, ["e1", "e2"]);
    assert_eq!(
        receipts.get(1).map(|receipt| receipt.raw.as_slice()),
        Some(&br#"{"email":"b@example.org","event":"delivered","sg_event_id":"e2","timestamp":1759319001}"#[..])
    );
}

/// The signature covers the body byte for byte and the timestamp: a changed body, a stale or
/// missing signature, another key, or an event without an id is refused.
#[test]
fn tampered_stale_or_unidentifiable_batches_are_refused() {
    let (pair, key) = key_pair();
    let body = br#"[{"event":"delivered","sg_event_id":"e1","timestamp":1759319000}]"#;
    let headers = signed_headers(&pair, NOW, body);
    let tampered = br#"[{"event":"bounce","sg_event_id":"e1","timestamp":1759319000}]"#;
    assert!(matches!(
        verify(&key, &headers, tampered, now()),
        Err(VerifyError::Unauthorized(_))
    ));
    let (_, other) = key_pair();
    assert!(matches!(
        verify(&other, &headers, body, now()),
        Err(VerifyError::Unauthorized(_))
    ));
    let stale = signed_headers(&pair, NOW - 26 * 3_600, body);
    assert_eq!(verify(&key, &stale, body, now()), Err(VerifyError::Stale));
    assert!(matches!(
        verify(&key, &HeaderMap::new(), body, now()),
        Err(VerifyError::Unauthorized(_))
    ));
    let anonymous = br#"[{"event":"delivered","timestamp":1759319000}]"#;
    let headers = signed_headers(&pair, NOW, anonymous);
    assert!(matches!(
        verify(&key, &headers, anonymous, now()),
        Err(VerifyError::InvalidPayload(_))
    ));
    assert!(SendgridKey::new("not base64!").is_err());
}

/// SendGrid's events map onto Norbelys's kinds: a `blocked` bounce and a `dropped` message are
/// rejections (no proof the address is bad), only a real bounce is a bounce, and group
/// unsubscribes, opens and clicks produce nothing.
#[test]
fn events_map_onto_kinds() {
    let cases = [
        (json!({"event": "processed"}), Some(EventKind::Accepted)),
        (json!({"event": "deferred"}), Some(EventKind::Deferred)),
        (json!({"event": "delivered"}), Some(EventKind::Delivered)),
        (
            json!({"event": "bounce", "type": "bounce"}),
            Some(EventKind::Bounced),
        ),
        (
            json!({"event": "bounce", "type": "blocked"}),
            Some(EventKind::Rejected),
        ),
        (json!({"event": "dropped"}), Some(EventKind::Rejected)),
        (json!({"event": "spamreport"}), Some(EventKind::Complaint)),
        (
            json!({"event": "unsubscribe"}),
            Some(EventKind::Unsubscribed),
        ),
        (json!({"event": "group_unsubscribe"}), None),
        (json!({"event": "open"}), None),
    ];
    for (mut event, expected) in cases {
        if let Some(object) = event.as_object_mut() {
            object.insert("sg_event_id".to_owned(), json!("e1"));
            object.insert("timestamp".to_owned(), json!(1_759_319_000));
        }
        let raw = event.to_string();
        let kinds: Vec<EventKind> = events(raw.as_bytes())
            .expect("parsed")
            .iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(kinds, expected.into_iter().collect::<Vec<_>>(), "{raw}");
    }
}

/// A bounce carries our message id from the unique argument, the recipient, SendGrid's status,
/// the SMTP code at the start of its reason, and SendGrid's own message id.
#[test]
fn a_bounce_carries_its_evidence() {
    let message = "0192e3a4-0000-7000-8000-000000000002";
    let raw = json!({
        "email": "ghost@example.org",
        "timestamp": 1_759_319_000,
        "event": "bounce",
        "type": "bounce",
        "sg_event_id": "ZGVsaXZlcmVk",
        "sg_message_id": "14c5d75ce93.dfd.64b469.filter0001.16648.5515E0B88.0",
        "smtp-id": "<m1.t1.tag@mail.example.com>",
        "reason": "550 5.1.1 The email account that you tried to reach does not exist.",
        "status": "5.1.1",
        MESSAGE_TAG: message,
    })
    .to_string();
    let parsed = events(raw.as_bytes()).expect("parsed");
    let [event] = parsed.as_slice() else {
        panic!("one event: {parsed:?}")
    };
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
        event.provider_message_id.as_deref(),
        Some("14c5d75ce93.dfd.64b469.filter0001.16648.5515E0B88.0")
    );
    assert_eq!(
        event.internet_message_id.as_deref(),
        Some("<m1.t1.tag@mail.example.com>")
    );
}
