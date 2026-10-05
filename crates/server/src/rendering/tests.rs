//! Tests of the rendering seam: the pure assembly of a message from what was read (policy), and
//! `prepare` itself under the sender's own login (store).

use std::time::Duration;

use axum::http::StatusCode;
use mail_parser::{Message as Parsed, MessageParser};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::testing::{TestDb, keys};

/// The settings of every test: the platform's tracking host is `https://t.norbelys.test`, and
/// the retry window the default 24 hours.
fn settings() -> Settings {
    window_of(Duration::from_secs(24 * 3_600))
}

/// The test settings with another retry window.
fn window_of(retry_window: Duration) -> Settings {
    Settings::new(
        keys(),
        &url::Url::parse("https://t.norbelys.test/ignored/path").unwrap(),
        retry_window,
    )
    .unwrap()
}

/// A direct message from `max@acme.example` through an SMTP login, as read from its row.
fn direct(provider: &str) -> Source {
    Source {
        workspace: WorkspaceId::trusted(Uuid::now_v7()),
        message: Id::new(),
        kind: Kind::Direct,
        campaign: None,
        person: true,
        provider: provider.to_owned(),
        transport: if matches!(provider, "google" | "microsoft") {
            "api"
        } else {
            "smtp"
        }
        .to_owned(),
        smtp: Some(json!({
            "host": "mail.norbelys.test", "port": 587, "security": "starttls", "username": "max",
            "configuration_set": "norbelys-events",
        })),
        from_email: "max@acme.example".to_owned(),
        from_name: Some("Max Mustermann".to_owned()),
        reply_to: Some("replies@acme.example".to_owned()),
        to: vec!["ada@example.com".to_owned()],
        cc: vec!["grace@example.com".to_owned()],
        bcc: vec!["audit@acme.example".to_owned()],
        subject: "Hello Ada".to_owned(),
        html: Some(
            "<p>Hi Ada, see <a href=\"https://example.com/pricing\">pricing</a>.</p>".to_owned(),
        ),
        text: Some("Hi Ada".to_owned()),
        variant: None,
        context: json!({ "sender": { "email": "max@acme.example" }, "variables": {} }),
        internet_message_id: "<0190aaaa.0190bbbb.0011223344556677@acme.example>".to_owned(),
        in_reply_to: None,
        thread_root: None,
        tracking: json!({}),
        signature_html: Some("<p>Max, Acme</p>".to_owned()),
        signature_text: Some("Max, Acme".to_owned()),
        deadline: None,
    }
}

/// A campaign message to Ada, opens and clicks tracked on the platform's host.
fn campaign() -> Source {
    Source {
        kind: Kind::Campaign,
        campaign: Some(Uuid::now_v7()),
        html: None,
        text: None,
        subject: "Quick question, Ada".to_owned(),
        variant: Some((
            Some("A note for {{ person.given_name }}".to_owned()),
            "<p>Hi {{ person.given_name }}, see <a href=\"https://example.com/pricing?a=1&amp;b=2\">pricing</a> or <a href=\"mailto:max@acme.example\">write</a>.</p>".to_owned(),
            Some("Hi {{ person.given_name }}".to_owned()),
        )),
        context: json!({
            "person": { "email": "ada@example.com", "given_name": "Ada", "fields": {} },
            "sender": { "email": "max@acme.example" },
            "campaign": { "name": "Launch" },
            "step": { "name": "First", "position": 1 },
            "variables": {},
        }),
        cc: Vec::new(),
        bcc: Vec::new(),
        tracking: json!({ "opens": true, "clicks": true, "hostname": null }),
        ..direct("smtp")
    }
}

fn parse(prepared: &Prepared) -> Parsed<'_> {
    MessageParser::default().parse(&prepared.raw).unwrap()
}

/// A header's value as a client reads it: unfolded, trimmed.
fn header(message: &Parsed<'_>, name: &str) -> Option<String> {
    message.header_raw(name).map(|value| {
        value
            .split(['\r', '\n'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_owned()
    })
}

/// The tokens of the links to `route` (`/t/c/`, `/t/o/`, `/u/`) in `text`.
fn tokens<'a>(text: &'a str, route: &str) -> Vec<&'a str> {
    text.split(&format!("https://t.norbelys.test{route}"))
        .skip(1)
        .filter_map(|rest| {
            rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .next()
        })
        .collect()
}

/// A direct message goes out as stored: its rendered subject and bodies with the identity's
/// signature, its own Message-ID, From with its name, Reply-To, To and Cc; no unsubscribe header
/// and no tracking, which only campaign mail carries. SMTP drops the Bcc header and keeps the
/// blind recipient in the envelope, after To and Cc.
#[test]
fn a_direct_message_goes_out_as_stored() {
    let source = direct("smtp");
    let prepared = assemble(&source, &settings(), crate::process::now()).unwrap();
    let message = parse(&prepared);
    assert_eq!(prepared.internet_message_id, source.internet_message_id);
    assert_eq!(
        message.message_id(),
        Some("0190aaaa.0190bbbb.0011223344556677@acme.example")
    );
    assert_eq!(message.subject(), Some("Hello Ada"));
    assert_eq!(
        header(&message, "From").as_deref(),
        Some("\"Max Mustermann\" <max@acme.example>")
    );
    assert_eq!(
        header(&message, "Reply-To").as_deref(),
        Some("replies@acme.example")
    );
    assert_eq!(header(&message, "Cc").as_deref(), Some("grace@example.com"));
    assert_eq!(header(&message, "Bcc"), None);
    assert_eq!(header(&message, "List-Unsubscribe"), None);
    assert_eq!(
        message.body_html(0).unwrap().replace("\r\n", "\n"),
        "<p>Hi Ada, see <a href=\"https://example.com/pricing\">pricing</a>.</p><div style=\"margin-top:16px\"><p>Max, Acme</p></div>"
    );
    assert_eq!(
        message.body_text(0).unwrap().replace("\r\n", "\n"),
        "Hi Ada\n\nMax, Acme"
    );
    assert_eq!(prepared.envelope.from().to_string(), "max@acme.example");
    let recipients: Vec<String> = prepared
        .envelope
        .recipients()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        recipients,
        ["ada@example.com", "grace@example.com", "audit@acme.example"]
    );
}

/// A campaign step's content sent to a person who is not enrolled is the same commercial mail to
/// a prospect as campaign mail, so it carries the one-click unsubscribe: the `List-Unsubscribe`
/// headers, and the stand-in link rendered at creation replaced by the person's real link. The same
/// content previewed at another address belongs to no person and carries neither, so an author
/// trying a step never unsubscribes anyone.
#[test]
fn step_content_to_a_person_carries_the_unsubscribe_and_a_preview_does_not() {
    let step_content = Source {
        context: json!({
            "person": { "email": "ada@example.com", "given_name": "Ada", "fields": {} },
            "sender": { "email": "max@acme.example" },
            "campaign": { "name": "Launch" },
            "step": { "name": "First", "position": 1 },
            "variables": {},
        }),
        html: Some(format!(
            "<p>Hi Ada. <a href=\"{UNSUBSCRIBE_STAND_IN}\">Unsubscribe</a></p>"
        )),
        text: Some(format!("Hi Ada. Unsubscribe: {UNSUBSCRIBE_STAND_IN}")),
        ..direct("smtp")
    };
    let prepared = assemble(&step_content, &settings(), crate::process::now()).unwrap();
    let message = parse(&prepared);
    assert_eq!(
        header(&message, "List-Unsubscribe-Post").as_deref(),
        Some("List-Unsubscribe=One-Click")
    );
    let html = message.body_html(0).unwrap();
    let text = message.body_text(0).unwrap();
    assert!(!html.contains(UNSUBSCRIBE_STAND_IN) && !text.contains(UNSUBSCRIBE_STAND_IN));
    assert_eq!(
        tokens(&html, "/u/").len(),
        1,
        "the author's link, and no second one"
    );

    let preview = Source {
        person: false,
        ..step_content
    };
    let message = parse(&assemble(&preview, &settings(), crate::process::now()).unwrap())
        .body_html(0)
        .map(|html| html.contains(UNSUBSCRIBE_STAND_IN));
    assert_eq!(message, Some(true));
    let prepared = assemble(&preview, &settings(), crate::process::now()).unwrap();
    assert_eq!(header(&parse(&prepared), "List-Unsubscribe"), None);
}

/// The platform's own mail links its brand mark on the stand-in tracking origin from its creation
/// (the role creating it may know no tracking host), and the sender links it on the tracking host
/// as it prepares the message. Only transactional mail is touched: the same HTML in a direct
/// message keeps what its author wrote.
#[test]
fn transactional_mail_links_its_mark_on_the_tracking_host() {
    let path = crate::tracking::images::EMAIL_MARK_PATH;
    let html = format!("<img src=\"{TRACKING_ORIGIN_STAND_IN}{path}\" alt=\"Norbelys\"><p>Hi</p>");
    let sent = |kind: Kind| {
        let source = Source {
            kind,
            html: Some(html.clone()),
            ..direct("smtp")
        };
        parse(&assemble(&source, &settings(), crate::process::now()).unwrap())
            .body_html(0)
            .unwrap()
            .into_owned()
    };
    let transactional = sent(Kind::Transactional);
    assert!(
        transactional.contains(&format!("<img src=\"https://t.norbelys.test{path}\"")),
        "{transactional}"
    );
    assert!(!transactional.contains(TRACKING_ORIGIN_STAND_IN));
    assert!(sent(Kind::Direct).contains(&format!("{TRACKING_ORIGIN_STAND_IN}{path}")));
}

/// The Gmail API and Graph read recipients from the headers, so their messages keep `Bcc`.
#[test]
fn api_transports_keep_bcc() {
    for provider in ["google", "microsoft"] {
        let prepared = assemble(&direct(provider), &settings(), crate::process::now()).unwrap();
        assert_eq!(
            header(&parse(&prepared), "Bcc").as_deref(),
            Some("audit@acme.example"),
            "{provider}"
        );
    }
}

/// Every campaign message carries RFC 8058's one-click unsubscribe: `List-Unsubscribe` with the
/// https link of its first recipient (and a mailto to the sender) and `List-Unsubscribe-Post`.
/// The link's token names the workspace, the message and that address, and the same link ends
/// both bodies, since the author did not place it.
#[test]
fn campaign_mail_carries_one_click_unsubscribe() {
    let source = campaign();
    let prepared = assemble(&source, &settings(), crate::process::now()).unwrap();
    let message = parse(&prepared);
    assert_eq!(
        header(&message, "List-Unsubscribe-Post").as_deref(),
        Some("List-Unsubscribe=One-Click")
    );
    let list = header(&message, "List-Unsubscribe").unwrap();
    assert!(
        list.ends_with(", <mailto:max@acme.example?subject=unsubscribe>"),
        "{list}"
    );
    let [token] = tokens(&list, "/u/")[..] else {
        panic!("one unsubscribe link: {list}");
    };
    assert_eq!(
        Token::decode(&keys(), token),
        Ok(Token::Unsubscribe {
            workspace: source.workspace,
            message: source.message,
            email: "ada@example.com".to_owned(),
        })
    );
    let text = message.body_text(0).unwrap().replace("\r\n", "\n");
    assert!(text.starts_with("Hi Ada\n\nMax, Acme\n\nUnsubscribe: https://t.norbelys.test/u/"));
    assert_eq!(tokens(&text, "/u/"), [token]);
    assert_eq!(
        tokens(&message.body_html(0).unwrap().replace("\r\n", "\n"), "/u/"),
        [token]
    );
}

/// A variant without a text body gets one derived from its rendered HTML alone, then the text
/// part's own additions: the identity's text signature as written (not the HTML one's text, since
/// the identity wrote both) and the unsubscribe line. The HTML part's hidden preheader and its
/// signature stay in the HTML part, so a reader of the text part reads each thing once.
#[test]
fn a_derived_text_part_ends_with_the_text_signature() {
    let source = Source {
        variant: Some((
            Some("A note for {{ person.given_name }}".to_owned()),
            "<p>Hi {{ person.given_name }}</p>".to_owned(),
            None,
        )),
        signature_html: Some("<p><b>Max</b> at Acme</p>".to_owned()),
        ..campaign()
    };
    let prepared = assemble(&source, &settings(), crate::process::now()).unwrap();
    let message = parse(&prepared);
    let text = message.body_text(0).unwrap().replace("\r\n", "\n");
    assert!(
        text.starts_with("Hi Ada\n\nMax, Acme\n\nUnsubscribe: https://t.norbelys.test/u/"),
        "{text}"
    );
    let html = message.body_html(0).unwrap();
    assert!(html.contains("<p><b>Max</b> at Acme</p>"), "{html}");
}

/// A campaign message's HTML is rendered now from its variant with the frozen context: the
/// preheader leads it, its web link leads through a click link carrying the exact destination,
/// the mailto link and the unsubscribe link stay direct, and the open pixel ends it.
#[test]
fn campaign_html_is_rendered_and_tracked() {
    let source = campaign();
    let prepared = assemble(&source, &settings(), crate::process::now()).unwrap();
    let html = parse(&prepared).body_html(0).unwrap().replace("\r\n", "\n");
    assert!(
        html.starts_with("<div style=\"display:none;max-height:0;overflow:hidden;mso-hide:all\">A note for Ada</div><p>Hi Ada, see <a href=\"https://t.norbelys.test/t/c/"),
        "{html}"
    );
    assert!(html.contains("<a href=\"mailto:max@acme.example\">write</a>"));
    let clicks = tokens(&html, "/t/c/");
    assert_eq!(clicks.len(), 1, "{html}");
    let Ok(Token::Click { link, url, .. }) = Token::decode(&keys(), clicks[0]) else {
        panic!("not a click token");
    };
    assert_eq!(
        (link, url.as_str()),
        (0, "https://example.com/pricing?a=1&b=2")
    );
    let opens = tokens(&html, "/t/o/");
    assert_eq!(opens.len(), 1);
    assert!(html.ends_with("height:1px\">"), "{html}");
}

/// A campaign created with its own tracking domain sends its links there, and a message that
/// tracks nothing is left untouched.
#[test]
fn tracking_follows_what_the_message_froze() {
    let mut own = campaign();
    own.tracking = json!({ "opens": true, "clicks": false, "hostname": "go.acme.example" });
    let html = parse(&assemble(&own, &settings(), crate::process::now()).unwrap())
        .body_html(0)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        html.contains("<img src=\"https://go.acme.example/t/o/"),
        "{html}"
    );
    assert!(html.contains("<a href=\"https://example.com/pricing?a=1&amp;b=2\">"));
    assert!(html.contains("https://go.acme.example/u/"));
    let mut none = campaign();
    none.tracking = json!({ "opens": false, "clicks": false });
    let html = parse(&assemble(&none, &settings(), crate::process::now()).unwrap())
        .body_html(0)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(!html.contains("/t/"), "{html}");
}

/// A reply answers its parent and keeps the thread's root before it in `References`, so clients
/// thread it under the conversation; the root alone is not repeated.
#[test]
fn replies_name_their_parent_and_root() {
    let mut reply = direct("smtp");
    reply.in_reply_to = Some("<parent@example.com>".to_owned());
    reply.thread_root = Some("<root@acme.example>".to_owned());
    let message_bytes = assemble(&reply, &settings(), crate::process::now()).unwrap();
    let message = parse(&message_bytes);
    assert_eq!(message.in_reply_to().as_text(), Some("parent@example.com"));
    assert_eq!(
        message.references().as_text_list().map(<[_]>::to_vec),
        Some(vec![
            "root@acme.example".into(),
            "parent@example.com".into()
        ])
    );
    reply.thread_root = Some("<parent@example.com>".to_owned());
    let message_bytes = assemble(&reply, &settings(), crate::process::now()).unwrap();
    assert_eq!(
        parse(&message_bytes).references().as_text(),
        Some("parent@example.com")
    );
}

/// Each relay gets its own headers with our message id, so its webhooks name the message: SES its
/// configuration set and tag, SendGrid a unique argument, Mailgun a variable and the remaining
/// usefulness (the delivery deadline, else the deployment's retry window, within Mailgun's 5
/// minutes to 24 hours).
#[test]
fn relays_get_their_headers() {
    let now = crate::process::now();
    let id = |source: &Source| source.message.uuid().to_string();
    let ses = direct("ses");
    let message_bytes = assemble(&ses, &settings(), now).unwrap();
    let message = parse(&message_bytes);
    assert_eq!(
        header(&message, "X-SES-CONFIGURATION-SET").as_deref(),
        Some("norbelys-events")
    );
    assert_eq!(
        header(&message, "X-SES-MESSAGE-TAGS"),
        Some(format!("norbelys_message_id={}", id(&ses)))
    );
    let sendgrid = direct("sendgrid");
    let message_bytes = assemble(&sendgrid, &settings(), now).unwrap();
    assert!(
        header(&parse(&message_bytes), "X-SMTPAPI")
            .unwrap()
            .contains(&id(&sendgrid))
    );
    let mut mailgun = direct("mailgun");
    let message_bytes = assemble(&mailgun, &settings(), now).unwrap();
    assert_eq!(
        header(&parse(&message_bytes), "X-Mailgun-Deliver-Within").as_deref(),
        Some("1440m")
    );
    let six_hours = window_of(Duration::from_secs(6 * 3_600));
    let message_bytes = assemble(&mailgun, &six_hours, now).unwrap();
    assert_eq!(
        header(&parse(&message_bytes), "X-Mailgun-Deliver-Within").as_deref(),
        Some("360m"),
        "the deployment's retry window, not a fixed day"
    );
    mailgun.deadline = Some(now.plus(Duration::from_secs(90 * 60)));
    let message_bytes = assemble(&mailgun, &settings(), now).unwrap();
    let message = parse(&message_bytes);
    assert_eq!(
        header(&message, "X-Mailgun-Deliver-Within").as_deref(),
        Some("90m")
    );
    assert!(
        header(&message, "X-Mailgun-Variables")
            .unwrap()
            .contains(&id(&mailgun))
    );
    mailgun.deadline = Some(now.minus(Duration::from_secs(60)));
    let message_bytes = assemble(&mailgun, &settings(), now).unwrap();
    assert_eq!(
        header(&parse(&message_bytes), "X-Mailgun-Deliver-Within").as_deref(),
        Some("5m")
    );
}

/// On the managed MTA the reverse path is the message's VERP address on the MTA's host, so a
/// bounce reaching the MTA is matched to the message by the MTA's own record.
#[test]
fn the_managed_mta_gets_a_verp_return_path() {
    let source = direct("norbelys");
    let prepared = assemble(&source, &settings(), crate::process::now()).unwrap();
    assert_eq!(
        prepared.envelope.from().to_string(),
        format!(
            "bounce+{}@mail.norbelys.test",
            source.message.uuid().simple()
        )
    );
}

/// What can never be fixed by trying again is permanent: an SES connection without its
/// configuration set, a template that no longer renders. A database failure is transient.
#[test]
fn permanent_and_transient_failures_are_told_apart() {
    let mut ses = direct("ses");
    ses.smtp = Some(json!({ "host": "email-smtp.eu-west-1.amazonaws.com" }));
    let missing = assemble(&ses, &settings(), crate::process::now()).unwrap_err();
    assert!(missing.is_permanent(), "{missing}");
    let mut broken = campaign();
    broken.context = json!({ "sender": {}, "variables": {} });
    let unrenderable = assemble(&broken, &settings(), crate::process::now()).unwrap_err();
    assert!(matches!(unrenderable, Error::Template(_)), "{unrenderable}");
    assert!(unrenderable.is_permanent());
    assert!(!Error::Db(sqlx::Error::PoolTimedOut).is_permanent());
}

/// The tracking origin must be https, because `List-Unsubscribe` accepts nothing else; only the
/// origin of the URL is kept.
#[test]
fn the_tracking_origin_is_https() {
    let http = url::Url::parse("http://t.norbelys.test").unwrap();
    assert!(Settings::new(keys(), &http, Duration::from_secs(86_400)).is_err());
    assert_eq!(settings().tracking_origin, "https://t.norbelys.test");
}

/// The sender's own login can read everything `prepare` reads, inside the message's workspace:
/// a direct message created through the API is prepared by the worker pool exactly as stored.
#[tokio::test]
async fn the_sender_login_prepares_a_message() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let connection = app
        .post("/v1/connections")
        .bearer(&acme.key)
        .idempotency("connection")
        .json(json!({
            "provider": "smtp",
            "account_email": "max@acme.example",
            "smtp": { "host": "smtp.acme.example", "port": 587, "security": "starttls", "password": "secret" },
        }))
        .send()
        .await;
    assert_eq!(
        connection.status,
        StatusCode::CREATED,
        "{}",
        connection.json
    );
    let created = app
        .post("/v1/messages")
        .bearer(&acme.key)
        .idempotency("message")
        .json(json!({
            "from": "max@acme.example",
            "to": ["ada@example.com"],
            "subject": "Hello {{ variables.name }}",
            "html": "<p>Hi {{ variables.name }}</p>",
            "variables": { "name": "Ada" },
        }))
        .send()
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.json);
    let message: Id<Message> = created.json["id"].as_str().unwrap().parse().unwrap();
    let prepared = prepare(&test.worker, &settings(), acme.id, message)
        .await
        .unwrap();
    let parsed = parse(&prepared);
    assert_eq!(parsed.subject(), Some("Hello Ada"));
    assert_eq!(parsed.body_text(0).unwrap().trim_end(), "Hi Ada");
    assert_eq!(
        Value::from(prepared.internet_message_id.clone()),
        created.json["internet_message_id"]
    );
    let foreign = test.workspace("other").await;
    assert!(matches!(
        prepare(&test.worker, &settings(), foreign.id, message).await,
        Err(Error::NotFound)
    ));
}

/// Mail through the managed MTA carries Gmail's `Feedback-ID`: its campaign's id (or `direct`
/// for the workspace's other mail), its workspace's id and the platform's one sender id, so
/// complaints from Gmail's feedback loop are counted per campaign and per workspace, never per
/// message. Mail through a customer's own mailbox carries none, so it is never marked as the
/// platform's.
#[test]
fn managed_mta_mail_carries_its_feedback_id_and_a_mailbox_none() {
    let one = direct("norbelys");
    let prepared = assemble(&one, &settings(), crate::process::now()).unwrap();
    assert_eq!(
        header(&parse(&prepared), "Feedback-ID"),
        Some(format!("direct:{}:norbelys", one.workspace.uuid().simple()))
    );

    let step = Source {
        provider: "norbelys".to_owned(),
        ..campaign()
    };
    let prepared = assemble(&step, &settings(), crate::process::now()).unwrap();
    assert_eq!(
        header(&parse(&prepared), "Feedback-ID"),
        Some(format!(
            "{}:{}:norbelys",
            step.campaign.unwrap().simple(),
            step.workspace.uuid().simple()
        ))
    );

    let mailbox = assemble(&campaign(), &settings(), crate::process::now()).unwrap();
    assert_eq!(header(&parse(&mailbox), "Feedback-ID"), None);
}
