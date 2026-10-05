use std::time::Duration;

use jiff::Timestamp;
use secrecy::SecretString;
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use super::sigv4::AccessKey;
use super::{mailgun, sendgrid, ses};
use crate::http::{ApiError, HttpClient};
use crate::testing::{Response, http_server};
use crate::webhooks::EventKind;

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

fn aws_key() -> AccessKey {
    AccessKey {
        id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
        secret: SecretString::from("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
    }
}

/// EU SMTP credentials are checked and reconciled on the EU API, including case and DNS-root
/// spellings; a lookalike suffix cannot redirect the key to a caller-controlled host.
#[test]
fn sendgrid_api_region_follows_the_smtp_host() {
    for host in ["smtp.eu.sendgrid.net", " SMTP.EU.SENDGRID.NET. "] {
        assert_eq!(sendgrid::api_base(host), "https://api.eu.sendgrid.com");
    }
    for host in ["smtp.sendgrid.net", "smtp.eu.sendgrid.net.evil.test"] {
        assert_eq!(sendgrid::api_base(host), "https://api.sendgrid.com");
    }
}

/// The daily Mailgun API-key probe requests just one event from the SMTP domain, with HTTP
/// Basic authentication, and reports a revoked key separately from SMTP authentication.
#[tokio::test]
async fn mailgun_api_key_check_is_bounded_and_reports_revocation() {
    let (origin, requests) = http_server(|request, _| {
        if request.header("authorization") == Some("Basic YXBpOnRlc3Qta2V5") {
            Response::json(200, &json!({"items": [], "paging": {}}))
        } else {
            Response::json(401, &json!({"message": "Unauthorized"}))
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).unwrap();
    for (key, expected) in [
        ("test-key", Ok(())),
        ("revoked", Err(ApiError::Unauthorized)),
    ] {
        assert_eq!(
            mailgun::check_key(
                &http,
                &SecretString::from(key),
                "smtp.eu.mailgun.org",
                "postmaster@MG.Example.com",
                soon()
            )
            .await,
            expected,
        );
    }
    let seen = requests.all();
    assert_eq!(seen.len(), 2);
    for request in seen {
        assert_eq!(request.path(), "/v3/mg.example.com/events");
        assert!(request.target.contains("limit=1"));
        assert!(
            request
                .header("authorization")
                .is_some_and(|value| value.starts_with("Basic "))
        );
    }
}

/// A connection's daily SES check reads the account and an identity with requests signed for
/// `ses` in the connection's Region: the account's paused sending, its reputation status and its
/// sandbox come back typed; an identity the account does not hold is `None` (an address may
/// still send through its verified domain), a held one says whether it may send. The Region
/// comes from the SMTP host, so the API is asked about the Region the connection sends through.
#[tokio::test]
async fn ses_reads_the_account_and_identities_with_signed_requests() {
    let (origin, requests) = http_server(|request, _| match request.path() {
        "/v2/email/account" => Response::json(
            200,
            &json!({
                "DedicatedIpAutoWarmupEnabled": false,
                "EnforcementStatus": "SHUTDOWN",
                "ProductionAccessEnabled": false,
                "SendQuota": {"Max24HourSend": 200.0, "MaxSendRate": 1.0, "SentLast24Hours": 12.0},
                "SendingEnabled": false
            }),
        ),
        "/v2/email/identities/ada%40example.com" => Response::json(
            404,
            &json!({"message": "Email identity ada@example.com does not exist."}),
        ),
        "/v2/email/identities/example.com" => Response::json(
            200,
            &json!({
                "IdentityType": "DOMAIN",
                "VerificationStatus": "SUCCESS",
                "VerifiedForSendingStatus": true,
                "FeedbackForwardingStatus": true
            }),
        ),
        _ => Response::json(400, &json!({"message": "unexpected"})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    assert_eq!(
        ses::region_of("Email-SMTP.US-EAST-1.amazonaws.com").as_deref(),
        Some("us-east-1")
    );
    assert_eq!(ses::region_of("smtp.sendgrid.net"), None);

    let account = ses::account(&http, &aws_key(), "us-east-1", soon())
        .await
        .expect("the account");
    assert_eq!(
        account,
        ses::Account {
            sending_enabled: false,
            enforcement: ses::Enforcement::Shutdown,
            production_access: false,
        }
    );
    assert_eq!(
        ses::identity(&http, &aws_key(), "us-east-1", "ada@example.com", soon())
            .await
            .expect("an answer"),
        None
    );
    assert_eq!(
        ses::identity(&http, &aws_key(), "us-east-1", "example.com", soon())
            .await
            .expect("an answer"),
        Some(ses::Identity {
            verified_for_sending: true
        })
    );

    let seen = requests.all();
    assert_eq!(seen.len(), 3);
    for request in &seen {
        assert_eq!(request.method, "GET");
        let authorization = request.header("authorization").expect("signed");
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/")
                && authorization.contains(
                    "/us-east-1/ses/aws4_request, SignedHeaders=host;x-amz-date, Signature="
                ),
            "{authorization}"
        );
        let date = request.header("x-amz-date").expect("dated");
        assert!(date.len() == 16 && date.ends_with('Z'), "{date}");
    }
}

/// SES's refusals of a read come back as the client's errors, so the check can tell an access
/// key that is refused (`403`, which leaves sending alone) from SES asking to slow down (`429`).
#[tokio::test]
async fn ses_refusals_are_typed() {
    let (origin, _) = http_server(|request, _| match request.path() {
        "/v2/email/account" => Response::json(
            403,
            &json!({"message": "The security token included in the request is invalid."}),
        ),
        _ => Response::json(429, &json!({"message": "Too many requests"})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    assert!(matches!(
        ses::account(&http, &aws_key(), "eu-west-1", soon()).await,
        Err(ApiError::Forbidden { .. })
    ));
    assert!(matches!(
        ses::identity(&http, &aws_key(), "eu-west-1", "example.com", soon()).await,
        Err(ApiError::Throttled { .. })
    ));
    assert!(matches!(
        ses::account(&http, &aws_key(), "us-east-1.evil.test/", soon()).await,
        Err(ApiError::InvalidResponse(_))
    ));
}

/// The scopes of a SendGrid key are read with the key as the bearer token; a revoked key is
/// unauthorized, which the check reads as a lost credential.
#[tokio::test]
async fn sendgrid_scopes_are_read_and_a_revoked_key_is_unauthorized() {
    let (origin, requests) = http_server(|request, _| match request.header("authorization") {
        Some("Bearer SG.live") => Response::json(
            200,
            &json!({"scopes": ["mail.send", "alerts.read", "email_activity.read"]}),
        ),
        _ => Response::json(
            401,
            &json!({"errors": [{"field": null, "message": "authorization required"}]}),
        ),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let scopes = sendgrid::scopes(
        &http,
        &SecretString::from("SG.live"),
        "smtp.sendgrid.net",
        soon(),
    )
    .await
    .expect("the scopes");
    assert!(scopes.iter().any(|scope| scope == sendgrid::MAIL_SEND));
    assert_eq!(
        sendgrid::scopes(
            &http,
            &SecretString::from("SG.revoked"),
            "smtp.eu.sendgrid.net",
            soon()
        )
        .await,
        Err(ApiError::Unauthorized)
    );
    assert_eq!(requests.all()[0].path(), "/v3/scopes");
}

/// The Email Activity of one of our messages is found by its unique argument and read event by
/// event into receipts the Event Webhook parser reads like any callback: the processed and
/// delivered events and a hard bounce keep their meaning and name our message, an open is left
/// out, and each receipt's id is the same on every read, so a second reconciliation is a replay.
#[tokio::test]
async fn sendgrid_activity_becomes_webhook_receipts() {
    let message = Uuid::now_v7();
    let (origin, requests) = http_server(|request, _| match request.path() {
        "/v3/messages" => Response::json(
            200,
            &json!({"messages": [{
                "from_email": "ada@example.com",
                "msg_id": "fMnBvAx2Tw2ugM1xi9ESYw.filterdrecv-5bd4d79b85-qm9n6-1-6F1A2B3C-7.0",
                "subject": "Hello",
                "to_email": "grace@example.org",
                "status": "not_delivered",
                "opens_count": 1,
                "clicks_count": 0,
                "last_event_time": "2026-10-02T09:15:00Z"
            }]}),
        ),
        "/v3/messages/fMnBvAx2Tw2ugM1xi9ESYw.filterdrecv-5bd4d79b85-qm9n6-1-6F1A2B3C-7.0" => {
            Response::json(
                200,
                &json!({
                    "from_email": "ada@example.com",
                    "msg_id": "fMnBvAx2Tw2ugM1xi9ESYw.filterdrecv-5bd4d79b85-qm9n6-1-6F1A2B3C-7.0",
                    "to_email": "grace@example.org",
                    "status": "not_delivered",
                    "events": [
                        {"event_name": "processed", "processed": "2026-10-02T09:00:00Z"},
                        {"event_name": "deferred", "processed": "2026-10-02T09:01:00Z",
                         "reason": "421 4.7.0 Try again later", "attempt_num": 1},
                        {"event_name": "bounced", "processed": "2026-10-02T09:10:00Z",
                         "reason": "550 5.1.1 The email account that you tried to reach does not exist",
                         "bounce_type": "hard"},
                        {"event_name": "opened", "processed": "2026-10-02T09:15:00Z"}
                    ]
                }),
            )
        }
        _ => Response::json(404, &json!({"errors": [{"message": "not found"}]})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let key = SecretString::from("SG.activity");
    let receipts = sendgrid::activity(&http, &key, "smtp.eu.sendgrid.net", message, soon())
        .await
        .expect("the activity");
    let kinds: Vec<EventKind> = receipts
        .iter()
        .flat_map(|receipt| {
            crate::webhooks::sendgrid::events(&receipt.raw).expect("a SendGrid event")
        })
        .inspect(|event| assert_eq!(event.message_id, Some(message)))
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        kinds,
        [EventKind::Accepted, EventKind::Deferred, EventKind::Bounced]
    );
    let bounce = crate::webhooks::sendgrid::events(&receipts[2].raw).expect("an event");
    assert_eq!(bounce[0].recipient.as_deref(), Some("grace@example.org"));
    assert_eq!(
        bounce[0].status.map(|status| status.to_string()).as_deref(),
        Some("5.1.1")
    );

    let again = sendgrid::activity(&http, &key, "smtp.eu.sendgrid.net", message, soon())
        .await
        .expect("the activity");
    let ids = |receipts: &[crate::webhooks::Receipt]| -> Vec<String> {
        receipts
            .iter()
            .map(|receipt| receipt.event_id.clone())
            .collect()
    };
    assert_eq!(ids(&receipts), ids(&again));
    assert!(
        receipts[0]
            .event_id
            .starts_with("activity:fMnBvAx2Tw2ugM1xi9ESYw.")
    );

    let list = &requests.all()[0];
    assert_eq!(
        list.query("query").as_deref(),
        Some(format!("(unique_args['norbelys_message_id']=\"{message}\")").as_str())
    );
    assert_eq!(list.header("authorization"), Some("Bearer SG.activity"));
}

/// A Mailgun domain's events are read oldest first over the requested range with the private
/// key, page after page until an empty one; only the kinds the webhook parser records become
/// receipts, keyed and shaped as the webhook route stores them, so a reconciled event and its
/// webhook delivery are one replay key.
#[tokio::test]
async fn mailgun_events_follow_the_pages_of_the_range() {
    let message = Uuid::now_v7();
    let (origin, requests) = http_server(move |request, _| match request.path() {
        "/v3/mg.example.com/events" => Response::json(
            200,
            &json!({
                "items": [
                    {"id": "czsjqFATSlC3QtAK-C80nw", "event": "delivered", "timestamp": 1_790_000_000.5,
                     "recipient": "grace@example.org",
                     "user-variables": {"norbelys_message_id": message.to_string()},
                     "message": {"headers": {"message-id": "<a.b.c@example.com>"}},
                     "delivery-status": {"code": 250, "message": "OK"}},
                    {"id": "opened-1", "event": "opened", "timestamp": 1_790_000_001.0}
                ],
                "paging": {"next": "https://api.mailgun.net/v3/mg.example.com/events/W3siYSI6IGZhbHNlfV0="}
            }),
        ),
        "/v3/mg.example.com/events/W3siYSI6IGZhbHNlfV0=" => Response::json(
            200,
            &json!({
                "items": [
                    {"id": "Ase3o-PRRuaFm4DHZhbCvg", "event": "failed", "severity": "permanent",
                     "reason": "bounce", "timestamp": 1_790_000_100.0,
                     "recipient": "ghost@example.org",
                     "delivery-status": {"code": 550, "enhanced-code": "5.1.1",
                                         "message": "5.1.1 The email account does not exist"}}
                ],
                "paging": {"next": "https://api.mailgun.net/v3/mg.example.com/events/W3siYiI6IGZhbHNlfV0="}
            }),
        ),
        _ => Response::json(200, &json!({"items": [], "paging": {}})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let begin = Timestamp::from_second(1_789_996_400).unwrap();
    let end = Timestamp::from_second(1_790_003_600).unwrap();
    let read = mailgun::events(
        &http,
        &SecretString::from("key-private"),
        mailgun::api_base("smtp.mailgun.org"),
        "mg.example.com",
        (begin, end),
        10,
        soon(),
    )
    .await
    .expect("the events");
    assert!(read.complete);
    let ids: Vec<&str> = read
        .receipts
        .iter()
        .map(|receipt| receipt.event_id.as_str())
        .collect();
    assert_eq!(ids, ["czsjqFATSlC3QtAK-C80nw", "Ase3o-PRRuaFm4DHZhbCvg"]);
    let delivered = crate::webhooks::mailgun::events(&read.receipts[0].raw).expect("an event");
    assert_eq!(delivered[0].kind, EventKind::Delivered);
    assert_eq!(delivered[0].message_id, Some(message));
    let bounced = crate::webhooks::mailgun::events(&read.receipts[1].raw).expect("an event");
    assert_eq!(bounced[0].kind, EventKind::Bounced);

    let seen = requests.all();
    assert_eq!(seen.len(), 3, "two pages, then the empty one");
    let first = &seen[0];
    assert_eq!(first.query("begin").as_deref(), Some("1789996400"));
    assert_eq!(first.query("end").as_deref(), Some("1790003600"));
    assert_eq!(first.query("ascending").as_deref(), Some("yes"));
    assert_eq!(first.query("limit").as_deref(), Some("300"));
    assert!(
        first
            .header("authorization")
            .is_some_and(|value| value.starts_with("Basic ")),
        "the private key goes over HTTP Basic"
    );
    assert_eq!(
        mailgun::api_base("SMTP.EU.MAILGUN.ORG"),
        "https://api.eu.mailgun.net"
    );
    assert_eq!(
        mailgun::domain_of("postmaster@MG.Example.com").as_deref(),
        Some("mg.example.com")
    );
}

/// A next page that leaves the domain's events is refused rather than followed, so a forged or
/// mistaken link never receives the private key; a read stopped by its page bound says it is
/// incomplete, so the caller knows part of the range is left.
#[tokio::test]
async fn mailgun_paging_stays_on_the_domain_and_within_its_bound() {
    let (origin, _) = http_server(|request, _| {
        let next = if request.query("begin").is_some() {
            "https://api.mailgun.net/v3/mg.example.com/events/next"
        } else {
            "https://collector.example.net/v3/mg.example.com/events/stolen"
        };
        Response::json(
            200,
            &json!({
                "items": [{"id": "e1", "event": "accepted", "timestamp": 1_790_000_000.0}],
                "paging": {"next": next}
            }),
        )
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let key = SecretString::from("key-private");
    let range = (
        Timestamp::from_second(1_789_996_400).unwrap(),
        Timestamp::from_second(1_790_003_600).unwrap(),
    );
    let bounded = mailgun::events(
        &http,
        &key,
        "https://api.mailgun.net",
        "mg.example.com",
        range,
        1,
        soon(),
    )
    .await
    .expect("one page");
    assert!(!bounded.complete);
    assert_eq!(bounded.receipts.len(), 1);
    assert!(matches!(
        mailgun::events(
            &http,
            &key,
            "https://api.mailgun.net",
            "mg.example.com",
            range,
            5,
            soon()
        )
        .await,
        Err(ApiError::InvalidResponse(_))
    ));
}
