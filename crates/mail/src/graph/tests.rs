use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use secrecy::SecretString;
use serde_json::json;
use tokio::time::Instant;

use super::{GraphCursor, changes, checked_link, find_sent, me, message, send_mail};
use crate::http::HttpClient;
use crate::receive::ResetReason;
use crate::submission::{Cause, Failure, Scope};
use crate::testing::{Response, http_server, refused_origin};

const MIME: &[u8] =
    b"From: ada@contoso.com\r\nTo: grace@example.org\r\nSubject: hi\r\n\r\nhello\r\n";

fn token(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

fn since() -> Timestamp {
    "2026-10-01T00:00:00Z".parse().expect("a timestamp")
}

/// A submission is one `POST /me/sendMail` carrying the MIME base64-encoded as `text/plain`;
/// `202 Accepted` is the acceptance, without an id (Graph returns none).
#[tokio::test]
async fn send_mail_posts_base64_mime() {
    let (origin, requests) = http_server(|_, _| Response::raw(202, b"")).await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let submission = send_mail(&http, &token("eyJ0"), MIME, soon())
        .await
        .expect("accepted");
    assert_eq!(submission.provider_message_id, None);
    let sent = requests.all();
    let request = sent.first().expect("a request");
    assert_eq!(
        (request.method.as_str(), request.path()),
        ("POST", "/v1.0/me/sendMail")
    );
    assert_eq!(request.header("content-type"), Some("text/plain"));
    assert_eq!(request.header("authorization"), Some("Bearer eyJ0"));
    assert_eq!(STANDARD.decode(&request.body).expect("base64"), MIME);
}

/// Graph's answers map onto outcomes: `429` is a throttle of the mailbox with its `Retry-After`;
/// `ErrorSendAsDenied` refuses the message's From address; other `403`s refuse the account; a
/// `400` refuses the message; a `5xx` leaves the outcome unknown.
#[tokio::test]
async fn send_mail_errors_map_onto_outcomes() {
    let (origin, _requests) = http_server(|request, _| match request.header("authorization") {
        Some("Bearer 429") => Response::json(429, &json!({"error": {"code": "ApplicationThrottled", "message": "Application is over its MailboxConcurrency limit."}})).with("Retry-After", "7"),
        Some("Bearer sendas") => Response::json(403, &json!({"error": {"code": "ErrorSendAsDenied", "message": "The user account which was used to submit this request does not have the right to send mail on behalf of the specified sending account."}})),
        Some("Bearer denied") => Response::json(403, &json!({"error": {"code": "ErrorAccessDenied", "message": "Access is denied."}})),
        Some("Bearer bad") => Response::json(400, &json!({"error": {"code": "ErrorMimeContentInvalidBase64String", "message": "Invalid base64 string for MIME content."}})),
        _ => Response::json(503, &json!({"error": {"code": "ServiceUnavailable", "message": "Service Unavailable"}})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let cases = [
        (
            "429",
            (Failure::Transient, Scope::Connection, Cause::Throttled),
        ),
        (
            "sendas",
            (Failure::Permanent, Scope::Message, Cause::Refused),
        ),
        (
            "denied",
            (Failure::Transient, Scope::Connection, Cause::Forbidden),
        ),
        ("bad", (Failure::Permanent, Scope::Message, Cause::Refused)),
        (
            "503",
            (Failure::Uncertain, Scope::Connection, Cause::Refused),
        ),
    ];
    for (bearer, expected) in cases {
        let rejection = send_mail(&http, &token(bearer), MIME, soon())
            .await
            .expect_err("refused");
        assert_eq!(
            (rejection.failure, rejection.scope, rejection.cause),
            expected,
            "{bearer}"
        );
        assert_eq!(rejection.retry_after.is_some(), bearer == "429", "{bearer}");
    }
}

/// A delta round starts with the folder's messages received since `since`, asking for immutable
/// ids and a bounded page; removals are skipped; a next link continues the round at once and the
/// delta link ends it, both kept as the cursor.
#[tokio::test]
async fn delta_rounds_follow_next_and_delta_links() {
    let (origin, requests) = http_server(|request, origin| {
        if request.query("$deltatoken").is_some() {
            Response::json(200, &json!({"value": [], "@odata.deltaLink": format!("{origin}/v1.0/me/mailFolders('inbox')/messages/delta?$deltatoken=t3")}))
        } else if request.query("$skiptoken").is_some() {
            Response::json(200, &json!({"value": [{"id": "AAMk3"}], "@odata.deltaLink": format!("{origin}/v1.0/me/mailFolders('inbox')/messages/delta?$deltatoken=t2")}))
        } else {
            Response::json(200, &json!({
                "value": [{"id": "AAMk1", "receivedDateTime": "2026-10-01T10:00:00Z"}, {"id": "AAMk2", "@removed": {"reason": "deleted"}}],
                "@odata.nextLink": format!("{origin}/v1.0/me/mailFolders('inbox')/messages/delta?$skiptoken=s1"),
            }))
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let first = changes(&http, &token("t"), "INBOX", None, since(), 10, soon())
        .await
        .expect("a page");
    assert_eq!(first.ids, ["AAMk1"]);
    assert!(first.more && first.reset.is_none());
    let second = changes(
        &http,
        &token("t"),
        "INBOX",
        Some(&first.cursor),
        since(),
        10,
        soon(),
    )
    .await
    .expect("a page");
    assert_eq!(second.ids, ["AAMk3"]);
    assert!(!second.more);
    assert!(second.cursor.delta_link.ends_with("$deltatoken=t2"));
    let sent = requests.all();
    let start = sent.first().expect("a request");
    assert_eq!(start.path(), "/v1.0/me/mailFolders/inbox/messages/delta");
    assert_eq!(start.query("$filter"), None);
    assert_eq!(
        start.query("$select").as_deref(),
        Some("id,receivedDateTime")
    );
    let prefer = start.header("prefer").unwrap_or_default();
    assert!(
        prefer.contains("odata.maxpagesize=10") && prefer.contains("IdType=\"ImmutableId\""),
        "{prefer}"
    );
}

/// A delta token Graph no longer knows (`410 Gone`) starts a new round at `since` and reports the
/// reset; nothing jumps to the newest message.
#[tokio::test]
async fn an_expired_delta_token_restarts_at_since() {
    let (origin, _requests) = http_server(|request, origin| {
        if request.query("$deltatoken").as_deref() == Some("old") {
            Response::json(410, &json!({"error": {"code": "SyncStateNotFound", "message": "The sync state generation is not found."}}))
        } else {
            Response::json(200, &json!({"value": [{"id": "AAMk9"}], "@odata.deltaLink": format!("{origin}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=new")}))
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let cursor = GraphCursor {
        delta_link: format!("{origin}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=old"),
        ..Default::default()
    };
    let page = changes(
        &http,
        &token("t"),
        "INBOX",
        Some(&cursor),
        since(),
        10,
        soon(),
    )
    .await
    .expect("a page");
    assert_eq!(page.ids, ["AAMk9"]);
    let reset = page.reset.expect("a reset");
    assert_eq!((reset.reason, reset.since), (ResetReason::Delta, since()));
}

/// More than Graph's filtered-delta cap is recovered in bounded pages, across cursor reloads.
#[tokio::test]
async fn resync_recovers_over_five_thousand_messages_without_a_filtered_delta_cap() {
    let (origin, requests) = http_server(|request, origin| {
        assert!(request.query("$filter").is_none());
        let page: usize = request
            .query("$skiptoken")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let end = ((page + 1) * 1000).min(5001);
        let mut values: Vec<_> = (page * 1000..end)
            .map(
                |id| json!({"id": format!("AAMk{id}"), "receivedDateTime": "2026-10-02T00:00:00Z"}),
            )
            .collect();
        values.push(json!({"id": "Old", "receivedDateTime": "2026-09-01T00:00:00Z"}));
        let mut response = json!({"value": values});
        if end < 5001 {
            response["@odata.nextLink"] = json!(format!(
                "{origin}/v1.0/me/mailFolders/inbox/messages/delta?$skiptoken={}",
                page + 1
            ));
        } else {
            response["@odata.deltaLink"] = json!(format!(
                "{origin}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=done"
            ));
        }
        Response::json(200, &response)
    })
    .await;
    let http = HttpClient::rebased(&origin).unwrap();
    let mut cursor = None;
    let mut recovered = std::collections::BTreeSet::new();
    loop {
        let page = changes(
            &http,
            &token("t"),
            "INBOX",
            cursor.as_ref(),
            since(),
            50,
            soon(),
        )
        .await
        .unwrap();
        assert!(page.ids.len() <= 50);
        assert!(!page.ids.iter().any(|id| id == "Old"));
        for id in page.ids {
            assert!(recovered.insert(id));
        }
        // Persistence and a fresh process retain pending ids and the date filter.
        cursor = Some(serde_json::from_value(serde_json::to_value(page.cursor).unwrap()).unwrap());
        if !page.more {
            break;
        }
    }
    assert_eq!(recovered.len(), 5001);
    assert_eq!(requests.all().len(), 6);
    assert!(cursor.unwrap().resync_since.is_none());
}

/// The bearer token follows only Graph's own links under the user's mail folders: a link to
/// another host, another path or with a fragment is refused before any request.
#[tokio::test]
async fn links_outside_graphs_mail_folders_are_refused() {
    let http = HttpClient::new().expect("a client");
    assert!(
        checked_link(
            &http,
            "https://graph.microsoft.com/v1.0/me/mailFolders('inbox')/messages/delta?$deltatoken=x"
        )
        .is_ok()
    );
    for link in [
        "https://graph.microsoft.com.evil.test/v1.0/me/mailFolders/inbox/messages/delta",
        "http://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta",
        "https://graph.microsoft.com/v1.0/users/other/mailFolders/inbox/messages/delta",
        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta#x",
    ] {
        assert!(checked_link(&http, link).is_err(), "{link}");
    }
    let cursor = GraphCursor {
        delta_link: "https://attacker.test/v1.0/me/mailFolders/inbox/messages/delta".to_owned(),
        ..Default::default()
    };
    assert!(matches!(
        changes(
            &http,
            &token("t"),
            "INBOX",
            Some(&cursor),
            since(),
            10,
            soon()
        )
        .await,
        Err(crate::receive::Error {
            failure: crate::receive::Failure::InvalidResponse,
            ..
        })
    ));
}

/// A message is read as its MIME (`$value`) after its arrival time, both by immutable id; one
/// larger than the caller's bound is cut and marked truncated with the declared size.
#[tokio::test]
async fn messages_are_read_as_mime_and_bounded() {
    let (origin, requests) = http_server(|request, _| {
        if request.path().ends_with("/$value") {
            Response::raw(200, MIME)
        } else {
            Response::json(
                200,
                &json!({"id": "AAMk1", "receivedDateTime": "2026-10-01T10:00:00Z"}),
            )
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let whole = message(&http, &token("t"), "AAMk1", 1_000, soon())
        .await
        .expect("a message");
    assert_eq!((whole.raw.as_slice(), whole.truncated), (MIME, false));
    assert_eq!(
        whole.received_at.map(|at| at.to_string()).as_deref(),
        Some("2026-10-01T10:00:00Z")
    );
    let cut = message(&http, &token("t"), "AAMk1", 10, soon())
        .await
        .expect("a message");
    assert_eq!(
        (cut.raw.len(), cut.truncated, cut.size),
        (10, true, u64::try_from(MIME.len()).ok())
    );
    let sent = requests.all();
    let paths: Vec<&str> = sent.iter().take(2).map(|request| request.path()).collect();
    assert_eq!(
        paths,
        ["/v1.0/me/messages/AAMk1", "/v1.0/me/messages/AAMk1/$value"]
    );
    assert!(
        sent.iter()
            .all(|request| request.header("prefer") == Some("IdType=\"ImmutableId\""))
    );
}

/// An `uncertain` submission is settled read-only by filtering Sent Items on the
/// `internetMessageId`, written with angle brackets and with any quote doubled as OData requires.
#[tokio::test]
async fn the_sent_items_search_filters_on_the_message_id() {
    let (origin, requests) = http_server(|request, _| match request.query("$filter").as_deref() {
        Some("internetMessageId eq '<m1.t1.tag@mail.example.com>'") => {
            Response::json(200, &json!({"value": [{"id": "x"}]}))
        }
        _ => Response::json(200, &json!({"value": []})),
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    assert!(
        find_sent(&http, &token("t"), "m1.t1.tag@mail.example.com", soon())
            .await
            .expect("searched")
    );
    assert!(
        !find_sent(&http, &token("t"), "<o'brien@mail.example.com>", soon())
            .await
            .expect("searched")
    );
    let filters: Vec<Option<String>> = requests
        .all()
        .iter()
        .map(|request| request.query("$filter"))
        .collect();
    assert_eq!(
        filters.get(1).cloned().flatten().as_deref(),
        Some("internetMessageId eq '<o''brien@mail.example.com>'")
    );
    assert_eq!(
        requests
            .all()
            .first()
            .map(|request| request.path().to_owned())
            .as_deref(),
        Some("/v1.0/me/mailFolders/sentitems/messages")
    );
}

/// The identity read of a check returns the user's sign-in name, the address a Microsoft
/// connection proves.
#[tokio::test]
async fn me_returns_the_sign_in_name() {
    let (origin, requests) = http_server(|_, _| {
        Response::json(200, &json!({"id": "87d349ed-44d7-43e1-9a83-5f2406dee5bd", "userPrincipalName": "ada@contoso.com", "mail": "ada@contoso.com", "displayName": "Ada"}))
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let user = me(&http, &token("t"), soon()).await.expect("a user");
    assert_eq!(user.user_principal_name, "ada@contoso.com");
    assert_eq!(
        requests
            .all()
            .first()
            .map(|request| request.path().to_owned())
            .as_deref(),
        Some("/v1.0/me")
    );
}

/// Without an answer the outcome follows what may have reached Microsoft: a connection that hangs
/// up after the request was sent is uncertain; a refused connection and a deadline that passed
/// before the request are transient, and the latter sends nothing.
#[tokio::test]
async fn send_mail_without_an_answer() {
    let (origin, requests) = http_server(|_, _| Response::hang_up()).await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let hung = send_mail(&http, &token("t"), MIME, soon())
        .await
        .expect_err("no answer");
    assert_eq!(
        (hung.failure, hung.cause),
        (Failure::Uncertain, Cause::NoReply)
    );
    let late = send_mail(&http, &token("t"), MIME, Instant::now())
        .await
        .expect_err("too late");
    assert_eq!(
        (late.failure, late.cause),
        (Failure::Transient, Cause::Deadline)
    );
    assert_eq!(requests.all().len(), 1);
    let refused = HttpClient::rebased(&refused_origin().await).expect("a client");
    let unreachable = send_mail(&refused, &token("t"), MIME, soon())
        .await
        .expect_err("unreachable");
    assert_eq!(
        (unreachable.failure, unreachable.cause),
        (Failure::Transient, Cause::NoReply)
    );
}
