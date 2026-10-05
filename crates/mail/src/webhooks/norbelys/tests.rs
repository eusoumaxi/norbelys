use aws_lc_rs::hmac;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderMap, HeaderValue};
use jiff::Timestamp;
use serde_json::json;

use super::{NorbelysKey, events, signature_matches, verify};
use crate::webhooks::{EventKind, Provenance, VerifyError};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const NOW: i64 = 1_759_320_000;

fn key() -> NorbelysKey {
    NorbelysKey::new(SECRET).expect("a key")
}

fn headers(id: &str, timestamp: i64, body: &[u8], secret: &[u8]) -> HeaderMap {
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, secret),
        format!("{id}.{timestamp}.")
            .as_bytes()
            .iter()
            .chain(body)
            .copied()
            .collect::<Vec<u8>>()
            .as_slice(),
    );
    let mut headers = HeaderMap::new();
    headers.insert("webhook-id", HeaderValue::from_str(id).expect("a header"));
    headers.insert(
        "webhook-timestamp",
        HeaderValue::from_str(&timestamp.to_string()).expect("a header"),
    );
    headers.insert(
        "webhook-signature",
        HeaderValue::from_str(&format!("v1,{}", STANDARD.encode(tag.as_ref()))).expect("a header"),
    );
    headers
}

/// The signing scheme matches the Standard Webhooks specification's published example (secret
/// from the public standard-webhooks JavaScript fixtures, message `msg_p5jXN8AQM9LWM0D4loKWxJek` at
/// 1614265330), so the managed MTA and any compliant library interoperate; a list of signatures
/// verifies when any `v1` entry does, which lets a secret rotate, and other versions are ignored.
#[test]
fn the_signature_scheme_matches_the_specification_vector() {
    let secret = STANDARD
        .decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")
        .expect("base64");
    let key = NorbelysKey::new(&secret).expect("a key");
    let body = br#"{"test": 2432232314}"#;
    let id = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    let valid = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";
    assert!(signature_matches(&key, id, "1614265330", body, valid));
    assert!(signature_matches(
        &key,
        id,
        "1614265330",
        body,
        &format!("v1,bm90IHRoaXMgb25l {valid}")
    ));
    assert!(!signature_matches(&key, id, "1614265331", body, valid));
    assert!(!signature_matches(
        &key,
        id,
        "1614265330",
        body,
        "v2,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
    ));
}

/// A batch signed with the webhook's secret verifies and becomes one receipt per event keyed by
/// its `event_id`, holding the event's exact bytes; a stale timestamp, a missing header, another
/// secret or a short secret is refused.
#[test]
fn a_signed_batch_becomes_receipts_and_everything_else_is_refused() {
    let now = Timestamp::from_second(NOW).expect("a timestamp");
    let body = br#"{"type":"mta.events","timestamp":"2025-10-01T12:00:00Z","data":{"events":[{"event_id":"ev1","kind":"delivered"},{"event_id":"ev2","kind":"bounced"}]}}"#;
    let receipts =
        verify(&key(), &headers("msg_1", NOW, body, SECRET), body, now).expect("verified");
    let ids: Vec<&str> = receipts
        .iter()
        .map(|receipt| receipt.event_id.as_str())
        .collect();
    assert_eq!(ids, ["ev1", "ev2"]);
    assert_eq!(
        receipts.first().map(|receipt| receipt.raw.as_slice()),
        Some(&br#"{"event_id":"ev1","kind":"delivered"}"#[..])
    );
    assert_eq!(
        verify(
            &key(),
            &headers("msg_1", NOW - 301, body, SECRET),
            body,
            now
        ),
        Err(VerifyError::Stale)
    );
    assert!(matches!(
        verify(
            &key(),
            &headers("msg_1", NOW, body, b"another secret of 32 bytes......"),
            body,
            now
        ),
        Err(VerifyError::Unauthorized(_))
    ));
    assert!(matches!(
        verify(&key(), &HeaderMap::new(), body, now),
        Err(VerifyError::Unauthorized(_))
    ));
    assert!(NorbelysKey::new(b"too short").is_err());
}

/// A verified body must be an `mta.events` batch of 1 to 100 events: another type, an empty
/// batch, a bare array or a batch over the bound is refused rather than half understood.
#[test]
fn the_batch_shape_is_enforced() {
    let now = Timestamp::from_second(NOW).expect("a timestamp");
    let batch = |kind: &str, count: usize| {
        let events: Vec<_> = (0..count)
            .map(|n| json!({"event_id": format!("ev{n}"), "kind": "delivered"}))
            .collect();
        json!({"type": kind, "timestamp": "2025-10-01T12:00:00Z", "data": {"events": events}})
            .to_string()
    };
    let check = |body: &str| {
        verify(
            &key(),
            &headers("msg_1", NOW, body.as_bytes(), SECRET),
            body.as_bytes(),
            now,
        )
    };
    assert_eq!(check(&batch("mta.events", 100)).map(|r| r.len()), Ok(100));
    assert!(matches!(
        check(&batch("mta.other", 1)),
        Err(VerifyError::InvalidPayload(_))
    ));
    assert!(matches!(
        check(&batch("mta.events", 0)),
        Err(VerifyError::InvalidPayload(_))
    ));
    assert!(matches!(
        check(r#"[{"event_id":"ev1","kind":"delivered"}]"#),
        Err(VerifyError::InvalidPayload(_))
    ));
    assert_eq!(check(&batch("mta.events", 101)), Err(VerifyError::TooLarge));
}

/// The MTA's event contract is enforced: every kind and provenance maps onto Norbelys's, the
/// queue id and our `Message-ID` are kept, and an unknown kind, a missing or unknown
/// provenance, or a malformed status is an error rather than a guess.
#[test]
fn events_follow_the_mta_contract() {
    let event = |kind: &str, provenance: &str, status: &str| {
        json!({
            "event_id": "ev1",
            "internet_message_id": "<m1.t1.tag@mail.example.com>",
            "username": "tenant-a",
            "recipient": "grace@example.org",
            "queue_id": "4ABC123",
            "kind": kind,
            "enhanced_status": status,
            "detail": "550 5.1.1 user unknown",
            "provenance": provenance,
            "observed_at": "2026-10-01T12:00:00Z",
        })
        .to_string()
    };
    let parsed = events(event("bounced", "verp_dsn", "5.1.1").as_bytes()).expect("parsed");
    let [bounce] = parsed.as_slice() else {
        panic!("one event: {parsed:?}")
    };
    assert_eq!(
        (bounce.kind, bounce.provenance),
        (EventKind::Bounced, Some(Provenance::VerpDsn))
    );
    assert_eq!(bounce.provider_message_id.as_deref(), Some("4ABC123"));
    assert_eq!(
        bounce.internet_message_id.as_deref(),
        Some("<m1.t1.tag@mail.example.com>")
    );
    assert_eq!(bounce.smtp_code, Some(550));
    for (kind, expected) in [
        ("accepted", EventKind::Accepted),
        ("deferred", EventKind::Deferred),
        ("delivered", EventKind::Delivered),
        ("complaint", EventKind::Complaint),
    ] {
        let parsed = events(event(kind, "smtp_reply", "").as_bytes()).expect("parsed");
        assert_eq!(
            parsed.first().map(|event| event.kind),
            Some(expected),
            "{kind}"
        );
    }
    for (provenance, expected) in [
        ("fbl_arf_dkim", Provenance::FblArfDkim),
        ("feedback_id_only", Provenance::FeedbackIdOnly),
    ] {
        let parsed = events(event("complaint", provenance, "").as_bytes()).expect("parsed");
        assert_eq!(
            parsed.first().and_then(|event| event.provenance),
            Some(expected),
            "{provenance}"
        );
    }
    assert!(events(event("opened", "smtp_reply", "").as_bytes()).is_err());
    assert!(events(event("bounced", "guess", "5.1.1").as_bytes()).is_err());
    assert!(events(event("bounced", "verp_dsn", "5.1").as_bytes()).is_err());
    let mut unattributed: serde_json::Value =
        serde_json::from_str(&event("bounced", "verp_dsn", "5.1.1")).expect("json");
    unattributed
        .as_object_mut()
        .expect("an object")
        .remove("provenance");
    assert!(events(unattributed.to_string().as_bytes()).is_err());
}
