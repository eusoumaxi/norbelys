use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use jiff::Timestamp;
use reqwest::StatusCode;
use secrecy::SecretString;
use serde_json::json;
use tokio::time::Instant;

use super::{
    GmailCursor, changes, find_sent, message, profile, rejection, retry_time, send, send_as,
};
use crate::http::HttpClient;
use crate::receive::{ResetReason, TransportIdentity};
use crate::submission::{Cause, Failure, Phase, Scope};
use crate::testing::{Response, http_server, refused_origin};

const MIME: &[u8] =
    b"From: ada@example.com\r\nTo: grace@example.org\r\nSubject: hi\r\n\r\nhello\r\n";

fn token() -> SecretString {
    SecretString::from("ya29.access".to_owned())
}

#[tokio::test]
async fn history_records_with_many_messages_keep_pending_ids_across_cursor_reload() {
    let (origin, requests) = http_server(|_, _| Response::json(200, &json!({
        "history": [{"id": "2000", "messagesAdded": (1..=5).map(|id| json!({"message": {"id": format!("m{id}")}})).collect::<Vec<_>>() }],
        "historyId": "2100"
    }))).await;
    let http = HttpClient::rebased(&origin).unwrap();
    let mut cursor: GmailCursor = serde_json::from_value(json!({"history_id": "1000"})).unwrap();
    let mut ids = Vec::new();
    loop {
        let page = changes(&http, &token(), "INBOX", Some(&cursor), since(), 2, soon())
            .await
            .unwrap();
        assert!(page.ids.len() <= 2);
        ids.extend(page.ids);
        cursor = serde_json::from_value(serde_json::to_value(page.cursor).unwrap()).unwrap();
        if !page.more {
            break;
        }
    }
    assert_eq!(ids, ["m1", "m2", "m3", "m4", "m5"]);
    assert_eq!(cursor.history_id, "2100");
    assert!(cursor.pending.is_none());
    assert_eq!(requests.all().len(), 1);
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

fn since() -> Timestamp {
    "2026-10-01T00:00:00Z".parse().expect("a timestamp")
}

/// A submission is one `messages.send` with the bearer token and the MIME as base64url in `raw`;
/// `200` is the acceptance and its `id` the provider's message id.
#[tokio::test]
async fn a_send_posts_raw_mime_and_returns_the_message_id() {
    let (origin, requests) =
        http_server(|_, _| Response::json(200, &json!({"id": "18c2f1a2b3c4d5e6", "threadId": "18c2f1a2b3c4d5e6", "labelIds": ["SENT"]})))
            .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let submission = send(&http, &token(), MIME, soon()).await.expect("accepted");
    assert_eq!(
        submission.provider_message_id.as_deref(),
        Some("18c2f1a2b3c4d5e6")
    );
    let sent = requests.all();
    let request = sent.first().expect("a request");
    assert_eq!(
        (request.method.as_str(), request.path()),
        ("POST", "/gmail/v1/users/me/messages/send")
    );
    assert_eq!(request.header("authorization"), Some("Bearer ya29.access"));
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("JSON");
    let raw = body
        .get("raw")
        .and_then(serde_json::Value::as_str)
        .expect("raw");
    assert_eq!(URL_SAFE.decode(raw).expect("base64url"), MIME);
}

/// Gmail's errors map onto outcomes: `401` is a lost token; `429` and the rate-limit `403`s are
/// throttles of the mailbox, unless Google's wording names the project's quota (or the reason is
/// `dailyLimitExceeded`), which belongs to the platform; `domainPolicy` and other `403`s refuse
/// the account; other `4xx` refuse the message; `5xx` leaves the outcome unknown.
#[test]
fn send_errors_map_onto_outcomes() {
    let project = "Quota exceeded for quota metric 'Queries' and limit 'Queries per minute' of service 'gmail.googleapis.com' for consumer 'project_number:123'.";
    let cases = [
        (
            401,
            "authError",
            "",
            (Failure::Transient, Scope::Connection, Cause::Unauthorized),
        ),
        (
            429,
            "rateLimitExceeded",
            "User-rate limit exceeded.",
            (Failure::Transient, Scope::Connection, Cause::Throttled),
        ),
        (
            429,
            "rateLimitExceeded",
            project,
            (Failure::Transient, Scope::Platform, Cause::Throttled),
        ),
        (
            403,
            "dailyLimitExceeded",
            "Daily Limit Exceeded",
            (Failure::Transient, Scope::Platform, Cause::Throttled),
        ),
        (
            403,
            "userRateLimitExceeded",
            "User Rate Limit Exceeded",
            (Failure::Transient, Scope::Connection, Cause::Throttled),
        ),
        (
            403,
            "domainPolicy",
            "The domain administrators have disabled Gmail apps.",
            (Failure::Transient, Scope::Connection, Cause::Forbidden),
        ),
        (
            400,
            "failedPrecondition",
            "Mail service not enabled",
            (Failure::Transient, Scope::Connection, Cause::Forbidden),
        ),
        (
            400,
            "invalidArgument",
            "Invalid To header",
            (Failure::Permanent, Scope::Message, Cause::Refused),
        ),
        (
            413,
            "",
            "Request Entity Too Large",
            (Failure::Permanent, Scope::Message, Cause::Refused),
        ),
        (
            500,
            "backendError",
            "Backend Error",
            (Failure::Uncertain, Scope::Connection, Cause::Refused),
        ),
    ];
    for (status, reason, message, expected) in cases {
        let found = rejection(
            StatusCode::from_u16(status).expect("a status"),
            reason,
            message,
            None,
        );
        assert_eq!(
            (found.failure, found.scope, found.cause),
            expected,
            "{status} {reason}"
        );
        assert_eq!((found.phase, found.code), (Phase::Api, Some(status)));
    }
}

/// Gmail's own wait in a throttle message is used when no `Retry-After` header came; a past time
/// is no wait.
#[test]
fn gmail_retry_times_are_read_from_the_message() {
    let future = Timestamp::now()
        .checked_add(jiff::SignedDuration::from_secs(600))
        .expect("a time");
    let message = format!("User-rate limit exceeded.  Retry after {future}.");
    assert_eq!(retry_time(&message), Some(future));
    let throttled = rejection(
        StatusCode::TOO_MANY_REQUESTS,
        "rateLimitExceeded",
        &message,
        None,
    );
    assert_eq!(throttled.retry_after, Some(future));
    assert_eq!(
        retry_time("User-rate limit exceeded.  Retry after 2020-01-01T00:00:00.000Z"),
        None
    );
}

/// Over the wire: a `429` keeps its `Retry-After` as an absolute instant; a `503`, or a connection
/// that hangs up after the request was sent, is uncertain (it may have been processed); a refused
/// connection and a deadline that passed before the request are transient, since nothing was
/// sent.
#[tokio::test]
async fn send_outcomes_over_the_wire() {
    let (origin, requests) = http_server(|request, _| match request.header("authorization") {
        Some("Bearer hang-up") => Response::hang_up(),
        Some("Bearer throttled") => Response::json(429, &json!({"error": {"code": 429, "message": "User-rate limit exceeded.", "errors": [{"reason": "rateLimitExceeded"}]}})).with("Retry-After", "30"),
        _ => Response::json(503, &json!({"error": {"code": 503, "message": "Backend Error"}})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let before = Timestamp::now();
    let throttled = send(
        &http,
        &SecretString::from("throttled".to_owned()),
        MIME,
        soon(),
    )
    .await
    .expect_err("throttled");
    assert_eq!(
        (throttled.failure, throttled.cause),
        (Failure::Transient, Cause::Throttled)
    );
    let wait = throttled
        .retry_after
        .expect("a wait")
        .duration_since(before)
        .as_secs();
    assert!((29..=31).contains(&wait), "{wait}");
    let unknown = send(&http, &token(), MIME, soon())
        .await
        .expect_err("unknown");
    assert_eq!(
        (unknown.failure, unknown.code),
        (Failure::Uncertain, Some(503))
    );

    let hung = send(
        &http,
        &SecretString::from("hang-up".to_owned()),
        MIME,
        soon(),
    )
    .await
    .expect_err("no answer");
    assert_eq!(
        (hung.failure, hung.cause),
        (Failure::Uncertain, Cause::NoReply)
    );

    let refused = HttpClient::rebased(&refused_origin().await).expect("a client");
    let unreachable = send(&refused, &token(), MIME, soon())
        .await
        .expect_err("unreachable");
    assert_eq!(
        (unreachable.failure, unreachable.cause),
        (Failure::Transient, Cause::NoReply)
    );
    let sent = requests.all().len();
    let late = send(&http, &token(), MIME, Instant::now())
        .await
        .expect_err("too late");
    assert_eq!(
        (late.failure, late.cause),
        (Failure::Transient, Cause::Deadline)
    );
    assert_eq!(requests.all().len(), sent);
}

/// The history page lists the messages added to the label after the cursor (each once); a full
/// page follows the provider's token with a fixed anchor, and the last page moves to the mailbox's
/// history id.
#[tokio::test]
async fn history_pages_advance_the_cursor() {
    let (origin, requests) = http_server(|request, _| {
        assert_eq!(request.query("startHistoryId").as_deref(), Some("1000"));
        if request.query("pageToken").is_none() {
            Response::json(200, &json!({
                "history": [
                    {"id": "1001", "messagesAdded": [{"message": {"id": "m1", "threadId": "t1"}}]},
                    {"id": "1002", "messagesAdded": [{"message": {"id": "m2", "threadId": "t2"}}, {"message": {"id": "m1", "threadId": "t1"}}]},
                ],
                "nextPageToken": "next",
                "historyId": "1010",
            }))
        } else {
            Response::json(200, &json!({"history": [{"id": "1003", "messagesAdded": [{"message": {"id": "m3"}}]}], "historyId": "1010"}))
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let first = changes(
        &http,
        &token(),
        "INBOX",
        Some(&GmailCursor {
            history_id: "1000".to_owned(),
            ..Default::default()
        }),
        since(),
        2,
        soon(),
    )
    .await
    .expect("a page");
    assert_eq!(first.ids, ["m1", "m2"]);
    assert_eq!(first.cursor.history_id, "1000");
    assert_eq!(first.cursor.history_page_token.as_deref(), Some("next"));
    assert!(first.more);
    let second = changes(
        &http,
        &token(),
        "INBOX",
        Some(&first.cursor),
        since(),
        2,
        soon(),
    )
    .await
    .expect("a page");
    assert_eq!(second.ids, ["m3"]);
    assert_eq!(second.cursor.history_id, "1010");
    assert!(!second.more && second.reset.is_none());
    let sent = requests.all();
    let request = sent.first().expect("a request");
    assert_eq!(request.path(), "/gmail/v1/users/me/history");
    assert_eq!(
        request.query("historyTypes").as_deref(),
        Some("messageAdded")
    );
    assert_eq!(request.query("labelId").as_deref(), Some("INBOX"));
    assert_eq!(request.query("maxResults").as_deref(), Some("2"));
}

/// A history id Gmail no longer keeps (`404`) restarts at `since`: the mailbox's current history
/// id is read first, then the label's messages received after `since` are listed oldest first,
/// and the page reports the reset while retaining its continuation instead of cutting the listing.
#[tokio::test]
async fn an_expired_history_restarts_at_since() {
    let (origin, requests) = http_server(|request, _| match request.path() {
        "/gmail/v1/users/me/history" => Response::json(404, &json!({"error": {"code": 404, "message": "Requested entity was not found.", "errors": [{"reason": "notFound"}]}})),
        "/gmail/v1/users/me/profile" => Response::json(200, &json!({"emailAddress": "ada@example.com", "messagesTotal": 10, "threadsTotal": 8, "historyId": "5000"})),
        _ => Response::json(200, &json!({"messages": [{"id": "new", "threadId": "a"}, {"id": "older", "threadId": "b"}], "nextPageToken": "more"})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let page = changes(
        &http,
        &token(),
        "INBOX",
        Some(&GmailCursor {
            history_id: "1".to_owned(),
            ..Default::default()
        }),
        since(),
        2,
        soon(),
    )
    .await
    .expect("a page");
    assert_eq!(page.ids, ["older", "new"]);
    assert_eq!(page.cursor.history_id, "5000");
    let reset = page.reset.expect("a reset");
    assert_eq!(
        (reset.reason, reset.since, reset.truncated),
        (ResetReason::History, since(), false)
    );
    assert!(page.more);
    assert_eq!(page.cursor.resync.as_ref().unwrap().page_token, "more");
    let paths: Vec<String> = requests
        .all()
        .iter()
        .map(|request| request.path().to_owned())
        .collect();
    assert_eq!(
        paths,
        [
            "/gmail/v1/users/me/history",
            "/gmail/v1/users/me/profile",
            "/gmail/v1/users/me/messages"
        ]
    );
    let listing = requests.all().into_iter().last().expect("a listing");
    assert_eq!(
        listing.query("q"),
        Some(format!("after:{}", since().as_second()))
    );
}

/// A message is read as raw MIME (base64url, padded or not) with Gmail's size and arrival time;
/// one larger than the caller's bound is cut and marked truncated; one too large to read whole
/// comes back as its headers alone.
#[tokio::test]
async fn messages_are_read_raw_and_bounded() {
    let raw = URL_SAFE.encode(MIME);
    let big = URL_SAFE.encode(vec![b'x'; 200_000]);
    let (origin, _requests) = http_server(move |request, _| match (request.path(), request.query("format").as_deref()) {
        ("/gmail/v1/users/me/messages/small", Some("raw")) => {
            Response::json(200, &json!({"id": "small", "raw": raw, "sizeEstimate": 70, "internalDate": "1759320000000"}))
        }
        ("/gmail/v1/users/me/messages/huge", Some("raw")) => Response::json(200, &json!({"id": "huge", "raw": big})),
        (_, _) => Response::json(200, &json!({
            "id": "huge",
            "sizeEstimate": 200_000,
            "internalDate": "1759320000000",
            "payload": {"headers": [{"name": "Message-ID", "value": "<big@example.org>"}, {"name": "Subject", "value": "Large"}]},
        })),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let whole = message(&http, &token(), "small", 1_000, soon())
        .await
        .expect("a message");
    assert_eq!(
        whole.identity,
        TransportIdentity::Provider {
            provider_message_id: "small".to_owned()
        }
    );
    assert_eq!(
        (whole.raw.as_slice(), whole.size, whole.truncated),
        (MIME, Some(70), false)
    );
    assert_eq!(
        whole.received_at,
        Timestamp::from_second(1_759_320_000).ok()
    );
    let cut = message(&http, &token(), "small", 10, soon())
        .await
        .expect("a message");
    assert_eq!((cut.raw.len(), cut.truncated), (10, true));
    let headers = message(&http, &token(), "huge", 1_000, soon())
        .await
        .expect("headers only");
    assert!(headers.truncated);
    assert_eq!(
        headers.raw,
        b"Message-ID: <big@example.org>\r\nSubject: Large\r\n\r\n"
    );
    assert!(
        message(&http, &token(), "../profile", 1_000, soon())
            .await
            .is_err()
    );
}

/// An `uncertain` submission is settled read-only by searching the Sent label for its
/// `Message-ID` (`rfc822msgid:`, without angle brackets); finding nothing settles nothing.
#[tokio::test]
async fn the_sent_search_looks_for_the_message_id() {
    let (origin, requests) = http_server(|request, _| match request.query("q").as_deref() {
        Some("rfc822msgid:m1.t1.tag@mail.example.com") => Response::json(
            200,
            &json!({"messages": [{"id": "x"}], "resultSizeEstimate": 1}),
        ),
        _ => Response::json(200, &json!({"resultSizeEstimate": 0})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    assert!(
        find_sent(&http, &token(), "<m1.t1.tag@mail.example.com>", soon())
            .await
            .expect("searched")
    );
    assert!(
        !find_sent(&http, &token(), "<m2.t1.tag@mail.example.com>", soon())
            .await
            .expect("searched")
    );
    let sent = requests.all();
    let request = sent.first().expect("a request");
    assert_eq!(request.query("labelIds").as_deref(), Some("SENT"));
    assert_eq!(request.query("maxResults").as_deref(), Some("1"));
}

/// The identity reads of a check: the profile's address and history id, and the send-as list with
/// its verification (absent for the primary address).
#[tokio::test]
async fn identity_reads_parse_the_profile_and_send_as() {
    let (origin, _requests) = http_server(|request, _| match request.path() {
        "/gmail/v1/users/me/profile" => Response::json(
            200,
            &json!({"emailAddress": "ada@example.com", "historyId": "77"}),
        ),
        _ => Response::json(
            200,
            &json!({"sendAs": [
                {"sendAsEmail": "ada@example.com", "isPrimary": true, "isDefault": true},
                {"sendAsEmail": "sales@example.com", "verificationStatus": "accepted"},
            ]}),
        ),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let mailbox = profile(&http, &token(), soon()).await.expect("a profile");
    assert_eq!(
        (mailbox.email_address.as_str(), mailbox.history_id.as_str()),
        ("ada@example.com", "77")
    );
    let addresses = send_as(&http, &token(), soon()).await.expect("a list");
    let found: Vec<(&str, bool, Option<&str>)> = addresses
        .iter()
        .map(|address| {
            (
                address.send_as_email.as_str(),
                address.is_primary,
                address.verification_status.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("ada@example.com", true, None),
            ("sales@example.com", false, Some("accepted"))
        ]
    );
}

/// A first read drains every list page, keeps its original date and history anchor across reloads,
/// and reads history only afterwards. Messages arriving during listing cannot fall into a gap.
#[tokio::test]
async fn resync_drains_all_pages_before_reading_history() {
    let (origin, requests) = http_server(|request, _| match request.path() {
        "/gmail/v1/users/me/profile" => Response::json(200, &json!({"emailAddress": "ada@example.com", "historyId": "1000"})),
        "/gmail/v1/users/me/messages" => {
            let page = request.query("pageToken").and_then(|token| token.parse::<usize>().ok()).unwrap_or(0);
            let first = page * 50;
            let ids: Vec<_> = (first..first + 50).map(|id| json!({"id": format!("m{id}")})).collect();
            let mut response = json!({"messages": ids});
            if page < 2 { response["nextPageToken"] = json!((page + 1).to_string()); }
            Response::json(200, &response)
        }
        "/gmail/v1/users/me/history" => {
            assert_eq!(request.query("startHistoryId").as_deref(), Some("1000"));
            Response::json(200, &json!({"history": [{"id": "1001", "messagesAdded": [{"message": {"id": "concurrent"}}]}], "historyId": "1100"}))
        }
        _ => panic!("unexpected Gmail request"),
    }).await;
    let http = HttpClient::rebased(&origin).unwrap();
    let mut cursor = None;
    let mut ids = Vec::new();
    for page in 0..3 {
        let read = changes(
            &http,
            &token(),
            "INBOX",
            cursor.as_ref(),
            since() + jiff::SignedDuration::from_hours(page),
            50,
            soon(),
        )
        .await
        .unwrap();
        assert_eq!(read.more, page < 2);
        assert_eq!(read.cursor.history_id, "1000");
        ids.extend(read.ids);
        cursor = Some(serde_json::from_value(serde_json::to_value(read.cursor).unwrap()).unwrap());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 150);
    let read = changes(
        &http,
        &token(),
        "INBOX",
        cursor.as_ref(),
        since(),
        50,
        soon(),
    )
    .await
    .unwrap();
    assert_eq!(read.ids, ["concurrent"]);
    assert_eq!(read.cursor.history_id, "1100");
    let sent = requests.all();
    assert_eq!(sent.len(), 5);
    for request in sent
        .iter()
        .filter(|request| request.path().ends_with("messages"))
    {
        assert_eq!(
            request.query("q"),
            Some(format!("after:{}", since().as_second()))
        );
    }
}

/// An empty filtered history page can still have a continuation; skipping it would lose the
/// next page's messages. Invalid opaque tokens are refused before a request or cursor is stored.
#[tokio::test]
async fn empty_history_pages_keep_their_continuation() {
    let (origin, requests) = http_server(|request, _| {
        if request.query("pageToken").is_none() {
            Response::json(200, &json!({"historyId": "2000", "nextPageToken": "next"}))
        } else {
            assert_eq!(request.query("startHistoryId").as_deref(), Some("1000"));
            Response::json(200, &json!({"history": [{"id": "1001", "messagesAdded": [{"message": {"id": "message"}}]}], "historyId": "2000"}))
        }
    }).await;
    let http = HttpClient::rebased(&origin).unwrap();
    let cursor = GmailCursor {
        history_id: "1000".into(),
        ..Default::default()
    };
    let first = changes(&http, &token(), "INBOX", Some(&cursor), since(), 50, soon())
        .await
        .unwrap();
    assert!(first.ids.is_empty() && first.more);
    let second = changes(
        &http,
        &token(),
        "INBOX",
        Some(&first.cursor),
        since(),
        50,
        soon(),
    )
    .await
    .unwrap();
    assert_eq!(second.ids, ["message"]);
    for page_token in [
        "".to_owned(),
        "x".repeat(16 * 1024 + 1),
        "bad\nheader".to_owned(),
    ] {
        let cursor = GmailCursor {
            history_page_token: Some(page_token),
            ..cursor.clone()
        };
        assert_eq!(
            changes(&http, &token(), "INBOX", Some(&cursor), since(), 50, soon())
                .await
                .unwrap_err()
                .failure,
            crate::receive::Failure::InvalidResponse
        );
    }
    assert_eq!(requests.all().len(), 2);
}
