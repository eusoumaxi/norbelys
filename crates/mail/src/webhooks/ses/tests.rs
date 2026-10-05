use std::time::Duration;

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair as RsaKeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_SHA256};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde_json::{Value, json};
use tokio::time::Instant;
use url::Url;

use super::{
    Cached, SnsCertificates, SnsMessage, SnsTopic, certificate_url, confirm_subscription, events,
    spki_from_pem, verify,
};
use crate::compose::MESSAGE_TAG;
use crate::http::HttpClient;
use crate::testing::{Response, http_server};
use crate::webhooks::{EventKind, VerifyError};

const ARN: &str = "arn:aws:sns:us-east-1:123456789012:ses-events";
const CERT_URL: &str = "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-9c6465fa7f48f5cacd23014631ec1136.pem";
const SIGNED_AT: &str = "2026-10-01T12:00:00.000Z";

/// ISRG Root X1, a public RSA certificate, to prove the PEM and X.509 path.
const ISRG_ROOT_X1: &str = "-----BEGIN CERTIFICATE-----
MIIFazCCA1OgAwIBAgIRAIIQz7DSQONZRGPgu2OCiwAwDQYJKoZIhvcNAQELBQAw
TzELMAkGA1UEBhMCVVMxKTAnBgNVBAoTIEludGVybmV0IFNlY3VyaXR5IFJlc2Vh
cmNoIEdyb3VwMRUwEwYDVQQDEwxJU1JHIFJvb3QgWDEwHhcNMTUwNjA0MTEwNDM4
WhcNMzUwNjA0MTEwNDM4WjBPMQswCQYDVQQGEwJVUzEpMCcGA1UEChMgSW50ZXJu
ZXQgU2VjdXJpdHkgUmVzZWFyY2ggR3JvdXAxFTATBgNVBAMTDElTUkcgUm9vdCBY
MTCCAiIwDQYJKoZIhvcNAQEBBQADggIPADCCAgoCggIBAK3oJHP0FDfzm54rVygc
h77ct984kIxuPOZXoHj3dcKi/vVqbvYATyjb3miGbESTtrFj/RQSa78f0uoxmyF+
0TM8ukj13Xnfs7j/EvEhmkvBioZxaUpmZmyPfjxwv60pIgbz5MDmgK7iS4+3mX6U
A5/TR5d8mUgjU+g4rk8Kb4Mu0UlXjIB0ttov0DiNewNwIRt18jA8+o+u3dpjq+sW
T8KOEUt+zwvo/7V3LvSye0rgTBIlDHCNAymg4VMk7BPZ7hm/ELNKjD+Jo2FR3qyH
B5T0Y3HsLuJvW5iB4YlcNHlsdu87kGJ55tukmi8mxdAQ4Q7e2RCOFvu396j3x+UC
B5iPNgiV5+I3lg02dZ77DnKxHZu8A/lJBdiB3QW0KtZB6awBdpUKD9jf1b0SHzUv
KBds0pjBqAlkd25HN7rOrFleaJ1/ctaJxQZBKT5ZPt0m9STJEadao0xAH0ahmbWn
OlFuhjuefXKnEgV4We0+UXgVCwOPjdAvBbI+e0ocS3MFEvzG6uBQE3xDk3SzynTn
jh8BCNAw1FtxNrQHusEwMFxIt4I7mKZ9YIqioymCzLq9gwQbooMDQaHWBfEbwrbw
qHyGO0aoSCqI3Haadr8faqU9GY/rOPNk3sgrDQoo//fb4hVC1CLQJ13hef4Y53CI
rU7m2Ys6xt0nUW7/vGT1M0NPAgMBAAGjQjBAMA4GA1UdDwEB/wQEAwIBBjAPBgNV
HRMBAf8EBTADAQH/MB0GA1UdDgQWBBR5tFnme7bl5AFzgAiIyBpY9umbbjANBgkq
hkiG9w0BAQsFAAOCAgEAVR9YqbyyqFDQDLHYGmkgJykIrGF1XIpu+ILlaS/V9lZL
ubhzEFnTIZd+50xx+7LSYK05qAvqFyFWhfFQDlnrzuBZ6brJFe+GnY+EgPbk6ZGQ
3BebYhtF8GaV0nxvwuo77x/Py9auJ/GpsMiu/X1+mvoiBOv/2X/qkSsisRcOj/KK
NFtY2PwByVS5uCbMiogziUwthDyC3+6WVwW6LLv3xLfHTjuCvjHIInNzktHCgKQ5
ORAzI4JMPJ+GslWYHb4phowim57iaztXOoJwTdwJx4nLCgdNbOhdjsnvzqvHu7Ur
TkXWStAmzOVyyghqpZXjFaH3pO3JLF+l+/+sKAIuvtd7u+Nxe5AW0wdeRlN8NwdC
jNPElpzVmbUq4JUagEiuTDkHzsxHpFKVK7q4+63SM1N95R1NbdWhscdCb+ZAJzVc
oyi3B43njTOQ5yOf+1CceWxG1bQVs5ZufpsMljq4Ui0/1lvh+wjChP4kqKOJ2qxq
4RgqsahDYVvTH9w7jXbyLeiNdd8XM2w9U/t7y0Ff/9yi0GE44Za4rF2LN9d11TPA
mRGunUHBcnWEvgJBQl9nJEiU0Zsnvgc/ubhPgXRR4Xq37Z0j4r7g1SgEEzwxA57d
emyPxgcYxn/eR44/KJ4EBs+lVDR3veyJm+kXQ99b21/+jh5Xos1AnX5iItreGCc=
-----END CERTIFICATE-----
";

/// A stand-in for SNS: an RSA key whose public half is preloaded into the certificate cache
/// under the signing certificate's URL, so verification runs without the network.
struct Sns {
    pair: RsaKeyPair,
    certificates: SnsCertificates,
}

impl Sns {
    fn new() -> Self {
        let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("an RSA key");
        let spki = pair
            .public_key()
            .as_der()
            .expect("an SPKI")
            .as_ref()
            .to_vec();
        let certificates = SnsCertificates::new(1);
        certificates.cache.lock().expect("the cache").insert(
            CERT_URL.to_owned(),
            Cached {
                spki,
                until: Instant::now() + Duration::from_secs(600),
            },
        );
        Self { pair, certificates }
    }

    /// Signs `canonical` (the string SNS signs, written out by hand from AWS's documentation)
    /// with SHA-256 and returns the message envelope carrying it.
    fn envelope(&self, mut fields: Value, canonical: &str, version: &str) -> Vec<u8> {
        let mut signature = vec![0; self.pair.public_modulus_len()];
        self.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                canonical.as_bytes(),
                &mut signature,
            )
            .expect("a signature");
        if let Some(object) = fields.as_object_mut() {
            object.insert("SignatureVersion".to_owned(), json!(version));
            object.insert("Signature".to_owned(), json!(STANDARD.encode(&signature)));
            object.insert("SigningCertURL".to_owned(), json!(CERT_URL));
        }
        fields.to_string().into_bytes()
    }
}

fn notification(message: &str) -> (Value, String) {
    let fields = json!({
        "Type": "Notification",
        "MessageId": "22b80b92-fdea-4c2c-8f9d-bdfb0c7bf324",
        "TopicArn": ARN,
        "Subject": "Amazon SES Email Event Notification",
        "Message": message,
        "Timestamp": SIGNED_AT,
    });
    let canonical = format!(
        "Message\n{message}\nMessageId\n22b80b92-fdea-4c2c-8f9d-bdfb0c7bf324\nSubject\nAmazon SES Email Event Notification\nTimestamp\n{SIGNED_AT}\nTopicArn\n{ARN}\nType\nNotification\n"
    );
    (fields, canonical)
}

fn signed_now() -> Timestamp {
    SIGNED_AT.parse::<Timestamp>().expect("a timestamp")
}

async fn check(sns: &Sns, body: &[u8]) -> Result<SnsMessage, VerifyError> {
    let topic = SnsTopic::new(ARN).expect("a topic");
    let http = HttpClient::new().expect("a client");
    verify(&topic, &sns.certificates, &http, body, signed_now()).await
}

/// A notification signed over AWS's canonical string (version 2, SHA-256) by the certificate at
/// an SNS URL of the topic's Region verifies and becomes a receipt keyed by SNS's message id,
/// holding the SES event SNS carried.
#[tokio::test]
async fn a_signed_notification_becomes_a_receipt() {
    let sns = Sns::new();
    let (fields, canonical) = notification(r#"{"eventType":"Delivery"}"#);
    let verified = check(&sns, &sns.envelope(fields, &canonical, "2"))
        .await
        .expect("verified");
    let SnsMessage::Notification(receipt) = verified else {
        panic!("a notification: {verified:?}")
    };
    assert_eq!(receipt.event_id, "22b80b92-fdea-4c2c-8f9d-bdfb0c7bf324");
    assert_eq!(receipt.raw, br#"{"eventType":"Delivery"}"#);
}

/// Anyone can create a topic and subscribe our URL, so a valid signature alone proves nothing:
/// another topic is refused, as are a changed message, a SHA-256 signature labelled as version 1
/// (SHA-1), and a timestamp outside the window.
#[tokio::test]
async fn foreign_tampered_mislabelled_or_stale_messages_are_refused() {
    let sns = Sns::new();
    let (fields, canonical) = notification("{}");
    let mut foreign = fields.clone();
    if let Some(object) = foreign.as_object_mut() {
        object.insert(
            "TopicArn".to_owned(),
            json!("arn:aws:sns:us-east-1:999999999999:attacker"),
        );
    }
    let foreign_canonical = canonical.replace(ARN, "arn:aws:sns:us-east-1:999999999999:attacker");
    assert!(matches!(
        check(&sns, &sns.envelope(foreign, &foreign_canonical, "2")).await,
        Err(VerifyError::Unauthorized(_))
    ));

    let tampered = String::from_utf8(sns.envelope(fields.clone(), &canonical, "2"))
        .expect("UTF-8")
        .replace("\"{}\"", "\"{\\\"x\\\":1}\"");
    assert!(matches!(
        check(&sns, tampered.as_bytes()).await,
        Err(VerifyError::Unauthorized(_))
    ));

    assert!(matches!(
        check(&sns, &sns.envelope(fields.clone(), &canonical, "1")).await,
        Err(VerifyError::Unauthorized(_))
    ));

    let mut stale = fields;
    if let Some(object) = stale.as_object_mut() {
        object.insert("Timestamp".to_owned(), json!("2026-10-01T09:00:00.000Z"));
    }
    let stale_canonical = canonical.replace(SIGNED_AT, "2026-10-01T09:00:00.000Z");
    assert_eq!(
        check(&sns, &sns.envelope(stale, &stale_canonical, "2")).await,
        Err(VerifyError::Stale)
    );
}

/// A subscription confirmation is signed over its own field list (with `SubscribeURL` and
/// `Token`) and is handed back for confirmation rather than stored.
#[tokio::test]
async fn a_subscription_confirmation_is_returned_for_confirming() {
    let sns = Sns::new();
    let subscribe =
        "https://sns.us-east-1.amazonaws.com/?Action=ConfirmSubscription&TopicArn=x&Token=t0k3n";
    let fields = json!({
        "Type": "SubscriptionConfirmation",
        "MessageId": "165545c9-2a5c-472c-8df2-7ff2be2b3b1b",
        "Token": "t0k3n",
        "TopicArn": ARN,
        "Message": "You have chosen to subscribe to the topic.",
        "SubscribeURL": subscribe,
        "Timestamp": SIGNED_AT,
    });
    let canonical = format!(
        "Message\nYou have chosen to subscribe to the topic.\nMessageId\n165545c9-2a5c-472c-8df2-7ff2be2b3b1b\nSubscribeURL\n{subscribe}\nTimestamp\n{SIGNED_AT}\nToken\nt0k3n\nTopicArn\n{ARN}\nType\nSubscriptionConfirmation\n"
    );
    let verified = check(&sns, &sns.envelope(fields, &canonical, "2"))
        .await
        .expect("verified");
    let SnsMessage::SubscriptionConfirmation(confirmation) = verified else {
        panic!("a confirmation: {verified:?}")
    };
    assert_eq!(
        confirmation.message_id,
        "165545c9-2a5c-472c-8df2-7ff2be2b3b1b"
    );
    assert_eq!(confirmation.token, "t0k3n");
}

/// The signing certificate is fetched only from the topic's own SNS host over `https`, at an SNS
/// certificate path: any other URL is refused before a request is made.
#[test]
fn certificate_urls_must_be_the_topics_sns_host() {
    let topic = SnsTopic::new(ARN).expect("a topic");
    assert!(certificate_url(&topic, CERT_URL).is_ok());
    for refused in [
        "http://sns.us-east-1.amazonaws.com/SimpleNotificationService-abc.pem",
        "https://sns.eu-west-1.amazonaws.com/SimpleNotificationService-abc.pem",
        "https://sns.us-east-1.amazonaws.com.evil.test/SimpleNotificationService-abc.pem",
        "https://sns.us-east-1.amazonaws.com/other.pem",
        "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-abc.pem?x=1",
        "https://user@sns.us-east-1.amazonaws.com/SimpleNotificationService-abc.pem",
        "https://sns.us-east-1.amazonaws.com:8443/SimpleNotificationService-abc.pem",
    ] {
        assert!(certificate_url(&topic, refused).is_err(), "{refused}");
    }
}

/// Topic ARNs are accepted only for SNS in AWS's partitions, which also fixes the SNS host the
/// certificate must come from (China's partition has its own domain).
#[test]
fn topics_come_from_sns_arns() {
    assert_eq!(
        SnsTopic::new(ARN).map(|topic| topic.host),
        Ok("sns.us-east-1.amazonaws.com".to_owned())
    );
    assert_eq!(
        SnsTopic::new("arn:aws-cn:sns:cn-north-1:123456789012:events").map(|topic| topic.host),
        Ok("sns.cn-north-1.amazonaws.com.cn".to_owned())
    );
    for invalid in [
        "arn:aws:sqs:us-east-1:123456789012:events",
        "arn:other:sns:us-east-1:123456789012:events",
        "arn:aws:sns:us-east-1:1234:events",
        "arn:aws:sns:US-EAST-1:123456789012:events",
        "arn:aws:sns:us-east-1:123456789012:",
    ] {
        assert!(SnsTopic::new(invalid).is_err(), "{invalid}");
    }
}

/// The public key is read from a real X.509 certificate in PEM form (the parser rustls uses) as
/// a key RSA verification accepts; anything else is refused.
#[test]
fn the_public_key_is_read_from_a_pem_certificate() {
    let spki = spki_from_pem(ISRG_ROOT_X1.as_bytes()).expect("a public key");
    assert!(aws_lc_rs::signature::ParsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &spki).is_ok());
    assert_eq!(
        spki_from_pem(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"),
        None
    );
}

/// A certificate is fetched once and then served from the cache; a failed fetch is reported as
/// unavailable, so SNS retries instead of the event being refused for good.
#[tokio::test]
async fn certificates_are_fetched_once_and_cached() {
    let (origin, requests) = http_server(|request, _| {
        if request
            .path()
            .ends_with("-9c6465fa7f48f5cacd23014631ec1136.pem")
        {
            Response::raw(200, ISRG_ROOT_X1.as_bytes())
        } else {
            Response::raw(404, b"")
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let certificates = SnsCertificates::new(1);
    let url = Url::parse(CERT_URL).expect("a URL");
    let first = certificates.get(&http, &url).await.expect("fetched");
    let second = certificates.get(&http, &url).await.expect("cached");
    assert_eq!(first, second);
    assert_eq!(requests.all().len(), 1);
    let missing =
        Url::parse("https://sns.us-east-1.amazonaws.com/SimpleNotificationService-0000.pem")
            .expect("a URL");
    assert!(matches!(
        certificates.get(&http, &missing).await,
        Err(VerifyError::Unavailable(_))
    ));
}

/// A subscription is confirmed on the topic's SNS host with `ConfirmSubscription`, the topic and
/// the token, never by following the URL inside the message.
#[tokio::test]
async fn subscriptions_are_confirmed_on_the_topics_host() {
    let (origin, requests) =
        http_server(|_, _| Response::raw(200, b"<ConfirmSubscriptionResponse/>")).await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let topic = SnsTopic::new(ARN).expect("a topic");
    let confirmation = super::SubscriptionConfirmation {
        message_id: "m".to_owned(),
        token: "t0k3n".to_owned(),
    };
    confirm_subscription(
        &http,
        &topic,
        &confirmation,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("confirmed");
    let [request] = requests
        .all()
        .try_into()
        .unwrap_or_else(|all: Vec<_>| panic!("one request: {all:?}"));
    assert_eq!(
        request.query("Action").as_deref(),
        Some("ConfirmSubscription")
    );
    assert_eq!(request.query("TopicArn").as_deref(), Some(ARN));
    assert_eq!(request.query("Token").as_deref(), Some("t0k3n"));
}

fn ses_event(event_type: &str, section: &str, detail: Value) -> Vec<u8> {
    json!({
        "eventType": event_type,
        section: detail,
        "mail": {
            "timestamp": "2026-10-01T11:59:00.000Z",
            "messageId": "0100018a1b2c3d4e-12345678-1234-1234-1234-123456789012-000000",
            "destination": ["ghost@example.org"],
            "headersTruncated": false,
            "headers": [{"name": "From", "value": "ada@example.com"}, {"name": "Message-ID", "value": "<m1.t1.tag@mail.example.com>"}],
            "tags": {"ses:configuration-set": ["norbelys-events"], MESSAGE_TAG: ["0192e3a4-0000-7000-8000-000000000003"]},
        },
    })
    .to_string()
    .into_bytes()
}

fn kinds(raw: &[u8]) -> Vec<(EventKind, Option<String>)> {
    events(raw, "sns-1")
        .expect("parsed")
        .into_iter()
        .map(|event| (event.kind, event.recipient))
        .collect()
}

/// A permanent bounce names each bounced recipient with SES's status and the remote server's
/// diagnostic, and the message three ways: our tag, SES's token and our original `Message-ID`.
#[test]
fn a_permanent_bounce_names_recipients_and_the_message() {
    let raw = ses_event(
        "Bounce",
        "bounce",
        json!({
            "bounceType": "Permanent",
            "bounceSubType": "General",
            "timestamp": "2026-10-01T12:00:01.000Z",
            "bouncedRecipients": [
                {"emailAddress": "ghost@example.org", "action": "failed", "status": "5.1.1", "diagnosticCode": "smtp; 550 5.1.1 user unknown"},
                {"emailAddress": "gone@example.org", "action": "failed", "status": "5.1.10", "diagnosticCode": "smtp; 550 5.1.10 RecipientNotFound"},
            ],
        }),
    );
    let parsed = events(&raw, "sns-1").expect("parsed");
    let [first, second] = parsed.as_slice() else {
        panic!("two events: {parsed:?}")
    };
    assert_eq!(
        (first.kind, first.recipient.as_deref(), first.smtp_code),
        (EventKind::Bounced, Some("ghost@example.org"), Some(550))
    );
    assert_eq!(
        second.status.map(|status| status.to_string()).as_deref(),
        Some("5.1.10")
    );
    assert_eq!(first.event_id, "sns-1");
    assert_eq!(
        first.message_id.map(|id| id.to_string()).as_deref(),
        Some("0192e3a4-0000-7000-8000-000000000003")
    );
    assert_eq!(
        first.provider_message_id.as_deref(),
        Some("0100018a1b2c3d4e-12345678-1234-1234-1234-123456789012-000000")
    );
    assert_eq!(
        first.internet_message_id.as_deref(),
        Some("<m1.t1.tag@mail.example.com>")
    );
    assert_eq!(first.observed_at.to_string(), "2026-10-01T12:00:01Z");
}

/// Only a permanent bounce of subtype `General` or `NoEmail` proves the address bad: SES's own
/// suppression lists, transient bounces it stopped retrying and undetermined ones are
/// rejections.
#[test]
fn only_hard_bounces_are_bounces() {
    let bounce = |kind: &str, subtype: &str| {
        ses_event(
            "Bounce",
            "bounce",
            json!({
                "bounceType": kind,
                "bounceSubType": subtype,
                "timestamp": "2026-10-01T12:00:01.000Z",
                "bouncedRecipients": [{"emailAddress": "ghost@example.org"}],
            }),
        )
    };
    let recipient = Some("ghost@example.org".to_owned());
    assert_eq!(
        kinds(&bounce("Permanent", "NoEmail")),
        [(EventKind::Bounced, recipient.clone())]
    );
    assert_eq!(
        kinds(&bounce("Permanent", "OnAccountSuppressionList")),
        [(EventKind::Rejected, recipient.clone())]
    );
    assert_eq!(
        kinds(&bounce("Transient", "MailboxFull")),
        [(EventKind::Rejected, recipient.clone())]
    );
    assert_eq!(
        kinds(&bounce("Undetermined", "Undetermined")),
        [(EventKind::Rejected, recipient)]
    );
}

/// A complaint names its recipient only when SES lists exactly one; with several possible
/// complainants it names nobody, so no address is suppressed on a guess. A complaint SES stopped
/// because of its suppression list is a rejection, and `not-spam` feedback is not a complaint.
#[test]
fn complaints_name_a_recipient_only_when_it_is_certain() {
    let complaint = |recipients: Value, extra: Value| {
        let mut detail =
            json!({"timestamp": "2026-10-01T12:00:01.000Z", "complainedRecipients": recipients});
        if let (Some(detail), Some(extra)) = (detail.as_object_mut(), extra.as_object()) {
            detail.extend(extra.clone());
        }
        ses_event("Complaint", "complaint", detail)
    };
    let one = json!([{"emailAddress": "grace@example.org"}]);
    let two = json!([{"emailAddress": "grace@example.org"}, {"emailAddress": "linus@example.org"}]);
    assert_eq!(
        kinds(&complaint(
            one.clone(),
            json!({"complaintFeedbackType": "abuse"})
        )),
        [(EventKind::Complaint, Some("grace@example.org".to_owned()))]
    );
    assert_eq!(
        kinds(&complaint(two, json!({"complaintFeedbackType": "abuse"}))),
        [(EventKind::Complaint, None)]
    );
    assert_eq!(
        kinds(&complaint(
            one.clone(),
            json!({"complaintSubType": "OnAccountSuppressionList"})
        )),
        [(EventKind::Rejected, Some("grace@example.org".to_owned()))]
    );
    assert_eq!(
        kinds(&complaint(
            one,
            json!({"complaintFeedbackType": "not-spam"})
        )),
        []
    );
}

/// The other event types map onto kinds per recipient or for the whole message: sending
/// accepted, delivery and delay per recipient, rejection and rendering failure for the message;
/// opens produce nothing (Norbelys tracks engagement itself); the older notification format is
/// read the same way.
#[test]
fn other_events_map_onto_kinds() {
    let to = |address: &str| Some(address.to_owned());
    assert_eq!(
        kinds(&ses_event("Send", "send", json!({}))),
        [(EventKind::Accepted, None)]
    );
    let delivery = ses_event(
        "Delivery",
        "delivery",
        json!({"timestamp": "2026-10-01T12:00:02.000Z", "recipients": ["grace@example.org", "linus@example.org"], "smtpResponse": "250 2.6.0 Message received"}),
    );
    assert_eq!(
        kinds(&delivery),
        [
            (EventKind::Delivered, to("grace@example.org")),
            (EventKind::Delivered, to("linus@example.org"))
        ]
    );
    let delay = ses_event(
        "DeliveryDelay",
        "deliveryDelay",
        json!({"timestamp": "2026-10-01T12:30:00.000Z", "delayType": "MailboxFull", "delayedRecipients": [{"emailAddress": "grace@example.org", "status": "4.2.2", "diagnosticCode": "smtp; 452 4.2.2 mailbox full"}]}),
    );
    assert_eq!(
        kinds(&delay),
        [(EventKind::Deferred, to("grace@example.org"))]
    );
    assert_eq!(
        kinds(&ses_event(
            "Reject",
            "reject",
            json!({"reason": "Bad content"})
        )),
        [(EventKind::Rejected, None)]
    );
    assert_eq!(
        kinds(&ses_event(
            "Rendering Failure",
            "failure",
            json!({"errorMessage": "Attribute 'name' is not present"})
        )),
        [(EventKind::Rejected, None)]
    );
    assert_eq!(kinds(&ses_event("Open", "open", json!({}))), []);
    let older = String::from_utf8(ses_event(
        "Delivery",
        "delivery",
        json!({"recipients": ["grace@example.org"]}),
    ))
    .expect("UTF-8")
    .replace("eventType", "notificationType");
    assert_eq!(
        kinds(older.as_bytes()),
        [(EventKind::Delivered, to("grace@example.org"))]
    );
}

/// Signature version 1 is RSA with SHA-1, which SNS still uses for topics left at their default;
/// a message SNS-style signed with SHA-1 over a canonical string without `Subject` (produced with
/// OpenSSL, since the crate's cryptography cannot sign SHA-1) verifies.
#[tokio::test]
async fn a_version_1_signature_verifies_with_sha1() {
    let spki = STANDARD
        .decode(concat!(
            "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAx/JZJ54oqfcv/PYVC2N/WjCGLTtaxneqaNjWyHkBf4rPNfVNFRGw/98v",
            "TcRD7OKogdS2iNaiJzYrCaNUkEgWd2Ks5aeH8KpoUWGtshP3Ce8Ka7+gp3OSBESqdYzOY2C+KM+88+iqioyK71LYSzeRjuq/EOmF",
            "kwYLg7rhNceJzJyIQvRDksYLkvnwgY1k32Izolajb3vpyArHowPoxM3kdeCC4ua9YxxQs1u6zUk8U/auvEt8bIKjJp9Tz7ZlsO1a",
            "0rnHP8cO+hpPUUE/s5PcyH1or6ozRs94sdrZR3CYdm4bXR6e0DROAMnXy828RXQEEHgehqnCkHPa0IW0OI7akQIDAQAB",
        ))
        .expect("base64");
    let signature = concat!(
        "sbr5xBqO6Qn5ClzmSb4NX1DcDYQiDDxfFKb1bcmZTJCNxqc/Ha0GTI/F/kMsy/pNXSyn7FVQJtk/XtC8hbHAHXQ7milEnE0eY2cJ",
        "h6MKBWbLKFg3V4Ye7udNBWrcK6MfXanbkEya1RJiIFDpH9Sfi1PczaLio48Z1MyHiWTPDD23Mteuueyj3Q3FsgLDNSb9ZkosLfAz",
        "KnpWi1nUjYYkeJdV7Okh4rvTioTAYW8keArgkUWRx3oe+CeXSnieKYKFSFy27cOS7xzNJucrW61KNZnh+gzyrT36ee86Tem/vGya",
        "CC2cfbqYuHCNGVe3S+N+T7TvoieYN18ashClLKr/Ug==",
    );
    let certificates = SnsCertificates::new(1);
    certificates.cache.lock().expect("the cache").insert(
        CERT_URL.to_owned(),
        Cached {
            spki,
            until: Instant::now() + Duration::from_secs(600),
        },
    );
    let body = json!({
        "Type": "Notification",
        "MessageId": "v1-message",
        "TopicArn": ARN,
        "Message": "{\"eventType\":\"Send\"}",
        "Timestamp": SIGNED_AT,
        "SignatureVersion": "1",
        "Signature": signature,
        "SigningCertURL": CERT_URL,
    })
    .to_string();
    let topic = SnsTopic::new(ARN).expect("a topic");
    let http = HttpClient::new().expect("a client");
    let verified = verify(&topic, &certificates, &http, body.as_bytes(), signed_now())
        .await
        .expect("verified");
    assert!(
        matches!(verified, SnsMessage::Notification(ref receipt) if receipt.event_id == "v1-message"),
        "{verified:?}"
    );
}
