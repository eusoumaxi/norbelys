use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use secrecy::SecretString;
use tokio::time::Instant;

use super::{
    ImapAuth, ImapCursor, ImapError, ImapSecurity, ImapServer, ImapSession, connect, imap_date,
};
use crate::net::AddressPolicy;
use crate::receive::{ResetReason, TransportIdentity};
use crate::testing::{Imap, Transcript, connector, imap_ok, imap_server};

/// A folder as the fake server holds it: `UIDVALIDITY`, `UIDNEXT`, and messages by UID with
/// their `INTERNALDATE`.
struct Folder {
    uid_validity: u32,
    uid_next: u32,
    messages: BTreeMap<u32, &'static str>,
    since: Vec<u32>,
}

fn inbox() -> Folder {
    let messages = BTreeMap::from([
        (10, "29-Sep-2026 09:00:00 +0000"),
        (11, "30-Sep-2026 08:00:00 +0000"),
        (12, "30-Sep-2026 09:00:00 +0000"),
        (15, "30-Sep-2026 23:00:00 +0000"),
        (20, "01-Oct-2026 11:00:00 +0000"),
    ]);
    Folder {
        uid_validity: 7,
        uid_next: 21,
        messages,
        since: vec![15, 20],
    }
}

fn body(uid: u32) -> String {
    format!(
        "Subject: message {uid}\r\nMessage-ID: <{uid}@example.org>\r\n\r\nbody of message {uid}\r\n"
    )
}

fn uids(set: &str) -> Vec<u32> {
    set.split(',').filter_map(|uid| uid.parse().ok()).collect()
}

/// The fake server's answers for one folder (and a Sent folder holding one known message).
fn script(folder: Folder) -> impl Fn(&str) -> Imap + Send + Sync + 'static {
    let folder = Arc::new(folder);
    move |command: &str| {
        let words: Vec<&str> = command.split_whitespace().collect();
        match words.as_slice() {
            ["LOGIN", _, "\"wrong\""] => Imap {
                untagged: String::new(),
                status: "NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)".to_owned(),
            },
            ["LOGIN", _, "\"busy\""] => Imap {
                untagged: String::new(),
                status: "NO [UNAVAILABLE] Try again later".to_owned(),
            },
            ["AUTHENTICATE", "XOAUTH2", response] => {
                let decoded = STANDARD.decode(response).unwrap_or_default();
                if String::from_utf8_lossy(&decoded).contains("auth=Bearer good") {
                    imap_ok("")
                } else {
                    Imap {
                        untagged: String::new(),
                        status: "NO [AUTHENTICATIONFAILED] Invalid credentials".to_owned(),
                    }
                }
            }
            ["EXAMINE", ..] => {
                // A `uid_next` of 0 stands for a server that does not announce `UIDNEXT`.
                let next = if folder.uid_next == 0 {
                    String::new()
                } else {
                    format!("* OK [UIDNEXT {}] Predicted next UID\r\n", folder.uid_next)
                };
                imap_ok(&format!(
                    "* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {}] UIDs valid\r\n{next}",
                    folder.messages.len(),
                    folder.uid_validity,
                ))
            }
            ["UID", "FETCH", "*", "(UID)"] => {
                let last = folder.messages.keys().last().copied().unwrap_or(0);
                imap_ok(&format!(
                    "* {} FETCH (UID {last})\r\n",
                    folder.messages.len()
                ))
            }
            ["UID", "SEARCH", "UID", range] => {
                let (start, end) = range.split_once(':').unwrap_or((range, range));
                let (start, end): (u32, u32) =
                    (start.parse().unwrap_or(0), end.parse().unwrap_or(0));
                let found: Vec<String> = folder
                    .messages
                    .keys()
                    .filter(|uid| (start..=end).contains(*uid))
                    .map(ToString::to_string)
                    .collect();
                imap_ok(&format!("* SEARCH {}\r\n", found.join(" ")))
            }
            ["UID", "SEARCH", "SINCE", _] => {
                let found: Vec<String> = folder.since.iter().map(ToString::to_string).collect();
                imap_ok(&format!("* SEARCH {}\r\n", found.join(" ")))
            }
            [
                "UID",
                "SEARCH",
                "HEADER",
                "Message-ID",
                "\"<m1.t1.tag@mail.example.com>\"",
            ] => imap_ok("* SEARCH 4\r\n"),
            ["UID", "SEARCH", "HEADER", ..] => imap_ok("* SEARCH\r\n"),
            ["UID", "FETCH", set, "(UID", "INTERNALDATE)"] => {
                let mut out = String::new();
                for (sequence, uid) in uids(set).into_iter().enumerate() {
                    let date = folder
                        .messages
                        .get(&uid)
                        .copied()
                        .unwrap_or("01-Oct-2026 12:00:00 +0000");
                    out.push_str(&format!(
                        "* {} FETCH (UID {uid} INTERNALDATE \"{date}\")\r\n",
                        sequence + 1
                    ));
                }
                imap_ok(&out)
            }
            [
                "UID",
                "FETCH",
                set,
                "(UID",
                "INTERNALDATE",
                "RFC822.SIZE",
                section,
            ] => {
                let limit: usize = section
                    .trim_start_matches("BODY.PEEK[]<0.")
                    .trim_end_matches(">)")
                    .parse()
                    .unwrap_or(0);
                let mut out = String::new();
                for (sequence, uid) in uids(set).into_iter().chain([99]).enumerate() {
                    let full = body(uid);
                    let part = &full.as_bytes()[..full.len().min(limit)];
                    let date = folder
                        .messages
                        .get(&uid)
                        .copied()
                        .unwrap_or("01-Oct-2026 12:00:00 +0000");
                    out.push_str(&format!(
                        "* {} FETCH (UID {uid} INTERNALDATE \"{date}\" RFC822.SIZE {} BODY[]<0> {{{}}}\r\n{})\r\n",
                        sequence + 1,
                        full.len(),
                        part.len(),
                        String::from_utf8_lossy(part)
                    ));
                }
                imap_ok(&out)
            }
            ["LIST", ..] => imap_ok(
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Items\"\r\n* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"\r\n",
            ),
            ["LOGOUT"] => Imap {
                untagged: "* BYE logging out\r\n".to_owned(),
                status: "OK LOGOUT completed".to_owned(),
            },
            _ => imap_ok(""),
        }
    }
}

async fn session(folder: Folder, password: &str) -> (Result<ImapSession, ImapError>, Transcript) {
    let (port, transcript) = imap_server(script(folder)).await;
    let secret = SecretString::from(password.to_owned());
    let auth = ImapAuth::Password {
        username: "ada@example.com",
        password: &secret,
    };
    let server = ImapServer {
        host: "127.0.0.1",
        port,
        security: ImapSecurity::Plain,
    };
    let opened = connect(
        &connector(AddressPolicy::Any),
        &server,
        &auth,
        64 * 1024,
        Instant::now() + Duration::from_secs(30),
    )
    .await;
    (opened, transcript)
}

fn at(text: &str) -> Timestamp {
    text.parse().expect("a timestamp")
}

/// A refused login is a lost credential (`[AUTHENTICATIONFAILED]` or any other `NO`), except a
/// `NO [UNAVAILABLE]`, which only asks to try later.
#[tokio::test]
async fn refused_logins_are_told_apart() {
    let (lost, _) = session(inbox(), "wrong").await;
    assert!(matches!(lost, Err(ImapError::Unauthorized(_))), "{lost:?}");
    let (busy, _) = session(inbox(), "busy").await;
    assert!(matches!(busy, Err(ImapError::Refused(_))), "{busy:?}");
}

/// `XOAUTH2` sends `user=<login>^Aauth=Bearer <token>^A^A` in `AUTHENTICATE`; a refused token is
/// an unauthorized login.
#[tokio::test]
async fn xoauth2_logs_in_with_the_bearer_token() {
    let (port, transcript) = imap_server(script(inbox())).await;
    let server = ImapServer {
        host: "127.0.0.1",
        port,
        security: ImapSecurity::Plain,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let good = SecretString::from("good".to_owned());
    let auth = ImapAuth::Xoauth2 {
        username: "ada@example.com",
        token: &good,
    };
    connect(
        &connector(AddressPolicy::Any),
        &server,
        &auth,
        1_024,
        deadline,
    )
    .await
    .expect("a session");
    let response = transcript
        .lines()
        .into_iter()
        .find_map(|line| {
            line.strip_prefix("AUTHENTICATE XOAUTH2 ")
                .map(str::to_owned)
        })
        .expect("a response");
    assert_eq!(
        STANDARD.decode(response).expect("base64"),
        b"user=ada@example.com\x01auth=Bearer good\x01\x01"
    );
    let bad = SecretString::from("expired".to_owned());
    let refused = connect(
        &connector(AddressPolicy::Any),
        &server,
        &ImapAuth::Xoauth2 {
            username: "ada@example.com",
            token: &bad,
        },
        1_024,
        deadline,
    )
    .await;
    assert!(
        matches!(refused, Err(ImapError::Unauthorized(_))),
        "{refused:?}"
    );
}

/// Pages follow the last UID read within the same `UIDVALIDITY`: a page holds at most the limit,
/// oldest first, says when more wait, and an up-to-date cursor reads nothing without searching.
#[tokio::test]
async fn pages_follow_the_last_uid_and_stay_bounded() {
    let (opened, transcript) = session(inbox(), "app-password").await;
    let mut mailbox = opened.expect("a session");
    let since = at("2026-10-01T00:00:00Z");
    let first = mailbox
        .changes(
            "INBOX",
            Some(&ImapCursor {
                uid_validity: 7,
                last_uid: 10,
            }),
            since,
            2,
        )
        .await
        .expect("a page");
    assert_eq!(
        (first.ids.as_slice(), first.cursor, first.more),
        (
            &[11, 12][..],
            ImapCursor {
                uid_validity: 7,
                last_uid: 12
            },
            true
        )
    );
    let second = mailbox
        .changes("INBOX", Some(&first.cursor), since, 10)
        .await
        .expect("a page");
    assert_eq!(
        (second.ids.as_slice(), second.cursor.last_uid, second.more),
        (&[15, 20][..], 20, false)
    );
    let searches = transcript
        .lines()
        .iter()
        .filter(|line| line.starts_with("UID SEARCH"))
        .count();
    let third = mailbox
        .changes("INBOX", Some(&second.cursor), since, 10)
        .await
        .expect("a page");
    assert!(third.ids.is_empty() && !third.more && third.reset.is_none());
    assert_eq!(
        transcript
            .lines()
            .iter()
            .filter(|line| line.starts_with("UID SEARCH"))
            .count(),
        searches
    );
    assert!(transcript.has("EXAMINE \"INBOX\""));
}

/// When the folder's `UIDVALIDITY` changed every UID was reassigned, so the page restarts at
/// `since`: `SEARCH SINCE` a day earlier (the server's time zone) narrows the candidates, their
/// exact arrival times pick the first message at or after `since`, and the page reports the
/// reset instead of jumping to the newest message.
#[tokio::test]
async fn a_changed_uidvalidity_restarts_at_since() {
    let (opened, transcript) = session(inbox(), "app-password").await;
    let mut mailbox = opened.expect("a session");
    let since = at("2026-10-01T00:00:00Z");
    let page = mailbox
        .changes(
            "INBOX",
            Some(&ImapCursor {
                uid_validity: 5,
                last_uid: 3,
            }),
            since,
            10,
        )
        .await
        .expect("a page");
    assert_eq!(page.ids, [20]);
    assert_eq!(
        page.cursor,
        ImapCursor {
            uid_validity: 7,
            last_uid: 20
        }
    );
    let reset = page.reset.expect("a reset");
    assert_eq!(
        (reset.reason, reset.since, reset.truncated),
        (ResetReason::UidValidity, since, false)
    );
    assert!(transcript.has("UID SEARCH SINCE 30-Sep-2026"));

    let fresh = mailbox
        .changes("INBOX", None, at("2026-10-01T12:00:00Z"), 10)
        .await
        .expect("a page");
    assert!(fresh.ids.is_empty() && fresh.reset.is_none());
    assert_eq!(fresh.cursor.last_uid, 20);
}

/// A restart examines at most the newest 500 candidates; when older ones were left out while the
/// oldest examined was still recent, the reset says so, making the gap visible.
#[tokio::test]
async fn a_restart_beyond_its_bound_reports_a_gap() {
    let messages: BTreeMap<u32, &'static str> = (1..=600)
        .map(|uid| (uid, "01-Oct-2026 12:00:00 +0000"))
        .collect();
    let folder = Folder {
        uid_validity: 9,
        uid_next: 601,
        messages,
        since: (1..=600).collect(),
    };
    let (opened, _) = session(folder, "app-password").await;
    let mut mailbox = opened.expect("a session");
    let page = mailbox
        .changes(
            "INBOX",
            Some(&ImapCursor {
                uid_validity: 1,
                last_uid: 0,
            }),
            at("2026-10-01T00:00:00Z"),
            5,
        )
        .await
        .expect("a page");
    assert_eq!(page.ids, [101, 102, 103, 104, 105]);
    assert!(page.reset.is_some_and(|reset| reset.truncated));
}

/// Messages are fetched in one command, each read up to the bound with `BODY.PEEK` (never marking
/// it read): the identity carries the `UIDVALIDITY`, a cut message is marked truncated with its
/// full size, and responses for UIDs not asked for are ignored.
#[tokio::test]
async fn fetched_messages_carry_identity_size_and_arrival() {
    let (port, transcript) = imap_server(script(inbox())).await;
    let secret = SecretString::from("app-password".to_owned());
    let auth = ImapAuth::Password {
        username: "ada@example.com",
        password: &secret,
    };
    let server = ImapServer {
        host: "127.0.0.1",
        port,
        security: ImapSecurity::Plain,
    };
    let mut mailbox = connect(
        &connector(AddressPolicy::Any),
        &server,
        &auth,
        40,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .expect("a session");
    let fetched = mailbox
        .fetch("INBOX", 7, &[11, 20])
        .await
        .expect("messages");
    let found: Vec<(TransportIdentity, usize, bool)> = fetched
        .iter()
        .map(|message| {
            (
                message.identity.clone(),
                message.raw.len(),
                message.truncated,
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (
                TransportIdentity::Imap {
                    uid_validity: 7,
                    uid: 11
                },
                40,
                true
            ),
            (
                TransportIdentity::Imap {
                    uid_validity: 7,
                    uid: 20
                },
                40,
                true
            )
        ]
    );
    let first = fetched.first().expect("a message");
    assert_eq!(first.size, u64::try_from(body(11).len()).ok());
    assert_eq!(first.received_at, Some(at("2026-09-30T08:00:00Z")));
    assert!(transcript.has("UID FETCH 11,20 (UID INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.40>)"));
    assert!(matches!(
        mailbox.fetch("INBOX", 6, &[11]).await,
        Err(crate::receive::Error {
            failure: crate::receive::Failure::Unavailable,
            ..
        })
    ));
}

/// The Sent folder is found by its RFC 6154 `\Sent` attribute, whatever its name, and searched
/// by `Message-ID` (quoted) to settle an `uncertain` submission read-only.
#[tokio::test]
async fn the_sent_folder_is_found_and_searched_by_message_id() {
    let (opened, transcript) = session(inbox(), "app-password").await;
    let mut mailbox = opened.expect("a session");
    assert_eq!(
        mailbox.sent_folder().await.expect("a listing").as_deref(),
        Some("Sent Items")
    );
    assert!(
        mailbox
            .find_message_id("Sent Items", "<m1.t1.tag@mail.example.com>")
            .await
            .expect("searched")
    );
    assert!(
        !mailbox
            .find_message_id("Sent Items", "<m2.t1.tag@mail.example.com>")
            .await
            .expect("searched")
    );
    assert!(transcript.has("UID SEARCH HEADER Message-ID \"<m1.t1.tag@mail.example.com>\""));
    assert!(
        mailbox
            .find_message_id("Sent Items", "<id with spaces>")
            .await
            .is_err()
    );
    mailbox.logout().await;
    assert!(transcript.has("LOGOUT"));
}

/// Plaintext IMAP is refused for tenant-typed hosts before any connection.
#[tokio::test]
async fn plaintext_is_refused_for_tenant_hosts() {
    let secret = SecretString::from("app-password".to_owned());
    let auth = ImapAuth::Password {
        username: "ada@example.com",
        password: &secret,
    };
    let server = ImapServer {
        host: "127.0.0.1",
        port: 143,
        security: ImapSecurity::Plain,
    };
    let refused = connect(
        &connector(AddressPolicy::PublicOnly),
        &server,
        &auth,
        1_024,
        Instant::now() + Duration::from_secs(5),
    )
    .await;
    assert!(matches!(refused, Err(ImapError::Plaintext)), "{refused:?}");
}

/// IMAP dates are `day-Mon-year` in English, the day not padded, taken in UTC.
#[test]
fn imap_dates_are_written_in_the_protocols_form() {
    assert_eq!(imap_date(at("2026-10-01T00:30:00Z")), "1-Oct-2026");
    assert_eq!(imap_date(at("2026-12-31T23:59:59Z")), "31-Dec-2026");
}

/// A server that does not announce `UIDNEXT` is asked for its last message's UID instead, so the
/// window search still ends at the newest message.
#[tokio::test]
async fn without_uidnext_the_newest_uid_is_fetched() {
    let folder = Folder {
        uid_next: 0,
        ..inbox()
    };
    let (opened, transcript) = session(folder, "app-password").await;
    let mut mailbox = opened.expect("a session");
    let page = mailbox
        .changes(
            "INBOX",
            Some(&ImapCursor {
                uid_validity: 7,
                last_uid: 12,
            }),
            at("2026-10-01T00:00:00Z"),
            10,
        )
        .await
        .expect("a page");
    assert_eq!(
        (page.ids.as_slice(), page.cursor.last_uid, page.more),
        (&[15, 20][..], 20, false)
    );
    assert!(transcript.has("UID FETCH * (UID)"));
}
