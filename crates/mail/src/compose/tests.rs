use std::time::Duration;

use mail_parser::{HeaderValue, MessageParser, MimeHeaders as _};
use uuid::Uuid;

use super::{
    ComposeError, Draft, FeedbackId, ListUnsubscribe, MESSAGE_TAG, Mailbox, Relay, compose,
};

const MESSAGE: Uuid = Uuid::from_u128(0x0192_8f5e_2b6c_7d3e_9a1b_4c5d_6e7f_8091);

fn draft<'a>() -> Draft<'a> {
    Draft {
        message_id: "<m1.t1.tag@mail.example.com>",
        from: Mailbox {
            name: Some("Ada Lovelace"),
            address: "ada@example.com",
        },
        reply_to: None,
        to: &[Mailbox {
            name: None,
            address: "grace@example.org",
        }],
        cc: &[],
        bcc: &[],
        keep_bcc: false,
        subject: "Quick question",
        text: Some("Hello Grace"),
        html: None,
        in_reply_to: None,
        references: &[],
        list_unsubscribe: None,
        feedback_id: None,
        relay: None,
    }
}

fn header(raw: &[u8], name: &str) -> Option<String> {
    MessageParser::default()
        .parse(raw)?
        .header_raw(name)
        .map(|value| value.trim().to_owned())
}

/// The headers Norbelys owns come out exactly as supplied: our `Message-ID` (never generated),
/// the From mailbox with its name, the threading headers of a follow-up (`References` in order),
/// a `Date`, and a `multipart/alternative` body when both a text and an HTML version exist.
#[test]
fn the_headers_norbelys_owns_are_written_as_supplied() {
    let references = ["<m0.t1.tag@mail.example.com>", "<reply@example.org>"];
    let raw = compose(&Draft {
        html: Some("<p>Hello Grace</p>"),
        in_reply_to: Some("<reply@example.org>"),
        references: &references,
        ..draft()
    })
    .expect("a message");
    let message = MessageParser::default()
        .parse(&raw)
        .expect("parseable MIME");
    assert_eq!(message.message_id(), Some("m1.t1.tag@mail.example.com"));
    assert_eq!(
        message.in_reply_to(),
        &HeaderValue::Text("reply@example.org".into())
    );
    assert_eq!(
        message.references(),
        &HeaderValue::TextList(vec![
            "m0.t1.tag@mail.example.com".into(),
            "reply@example.org".into()
        ])
    );
    let from = message
        .from()
        .and_then(|from| from.first())
        .expect("a From mailbox");
    assert_eq!(
        (from.name(), from.address()),
        (Some("Ada Lovelace"), Some("ada@example.com"))
    );
    assert_eq!(message.subject(), Some("Quick question"));
    assert!(message.date().is_some());
    assert!(message.is_content_type("multipart", "alternative"));
    assert_eq!(message.body_text(0).as_deref(), Some("Hello Grace"));
    assert_eq!(message.body_html(0).as_deref(), Some("<p>Hello Grace</p>"));
}

/// A campaign message carries RFC 8058 one-click unsubscribe: `List-Unsubscribe` with the
/// `https` URI first (and the optional `mailto:`), and `List-Unsubscribe-Post` asking for the
/// one-click `POST`; anything but `https` is refused, since mailbox providers ignore it.
#[test]
fn campaign_messages_carry_one_click_unsubscribe() {
    let unsubscribe = ListUnsubscribe {
        https: "https://t.example.com/u/abc",
        mailto: Some("mailto:u+abc@example.com"),
    };
    let raw = compose(&Draft {
        list_unsubscribe: Some(unsubscribe),
        ..draft()
    })
    .expect("a message");
    assert_eq!(
        header(&raw, "List-Unsubscribe").as_deref(),
        Some("<https://t.example.com/u/abc>, <mailto:u+abc@example.com>")
    );
    assert_eq!(
        header(&raw, "List-Unsubscribe-Post").as_deref(),
        Some("List-Unsubscribe=One-Click")
    );
    let insecure = ListUnsubscribe {
        https: "http://t.example.com/u/abc",
        mailto: None,
    };
    assert_eq!(
        compose(&Draft {
            list_unsubscribe: Some(insecure),
            ..draft()
        }),
        Err(ComposeError::Header("List-Unsubscribe"))
    );
}

/// Each relay gets our message id where its events return it, so a webhook names the message
/// without parsing: an SES message tag beside its configuration set (without which SES publishes
/// no event), a SendGrid unique argument, a Mailgun variable with a delivery window clamped to
/// Mailgun's 5 minutes to 24 hours.
#[test]
fn relays_receive_our_message_id_in_their_own_headers() {
    let ses = compose(&Draft {
        relay: Some(Relay::Ses {
            configuration_set: "norbelys-events",
            message: MESSAGE,
        }),
        ..draft()
    })
    .expect("an SES message");
    assert_eq!(
        header(&ses, "X-SES-CONFIGURATION-SET").as_deref(),
        Some("norbelys-events")
    );
    assert_eq!(
        header(&ses, "X-SES-MESSAGE-TAGS"),
        Some(format!("{MESSAGE_TAG}={MESSAGE}"))
    );

    let sendgrid = compose(&Draft {
        relay: Some(Relay::Sendgrid { message: MESSAGE }),
        ..draft()
    })
    .expect("a SendGrid message");
    let smtpapi: serde_json::Value =
        serde_json::from_str(&header(&sendgrid, "X-SMTPAPI").expect("X-SMTPAPI")).expect("JSON");
    assert_eq!(
        smtpapi.pointer(&format!("/unique_args/{MESSAGE_TAG}")),
        Some(&serde_json::json!(MESSAGE.to_string()))
    );

    let mailgun = |window: u64| {
        compose(&Draft {
            relay: Some(Relay::Mailgun {
                message: MESSAGE,
                deliver_within: Duration::from_secs(window),
            }),
            ..draft()
        })
        .expect("a Mailgun message")
    };
    let raw = mailgun(3_600);
    let variables: serde_json::Value =
        serde_json::from_str(&header(&raw, "X-Mailgun-Variables").expect("variables"))
            .expect("JSON");
    assert_eq!(
        variables.get(MESSAGE_TAG),
        Some(&serde_json::json!(MESSAGE.to_string()))
    );
    assert_eq!(
        header(&raw, "X-Mailgun-Deliver-Within").as_deref(),
        Some("60m")
    );
    assert_eq!(
        header(&mailgun(10), "X-Mailgun-Deliver-Within").as_deref(),
        Some("5m")
    );
    assert_eq!(
        header(&mailgun(7 * 86_400), "X-Mailgun-Deliver-Within").as_deref(),
        Some("1440m")
    );

    assert_eq!(
        compose(&Draft {
            relay: Some(Relay::Ses {
                configuration_set: "bad set",
                message: MESSAGE
            }),
            ..draft()
        }),
        Err(ComposeError::Header("X-SES-CONFIGURATION-SET"))
    );
}

/// `Bcc` must not leak through SMTP, where the envelope carries the blind recipients, but must
/// stay for the Gmail API and Graph, which read recipients from the headers.
#[test]
fn bcc_is_kept_only_when_asked() {
    let bcc = [Mailbox {
        name: None,
        address: "hidden@example.org",
    }];
    let smtp = compose(&Draft {
        bcc: &bcc,
        ..draft()
    })
    .expect("a message");
    assert_eq!(header(&smtp, "Bcc"), None);
    let api = compose(&Draft {
        bcc: &bcc,
        keep_bcc: true,
        ..draft()
    })
    .expect("a message");
    assert_eq!(header(&api, "Bcc").as_deref(), Some("hidden@example.org"));
}

/// Composition refuses what would produce a broken or forged message: a `Message-ID` or
/// reference that is not `<left@right>`, an invalid address, a display name with a line break
/// (header injection), or no body at all.
#[test]
fn malformed_drafts_are_refused() {
    assert_eq!(
        compose(&Draft {
            message_id: "m1@example.com",
            ..draft()
        }),
        Err(ComposeError::Header("Message-ID"))
    );
    assert_eq!(
        compose(&Draft {
            message_id: "<m 1@example.com>",
            ..draft()
        }),
        Err(ComposeError::Header("Message-ID"))
    );
    assert_eq!(
        compose(&Draft {
            in_reply_to: Some("<nope>"),
            ..draft()
        }),
        Err(ComposeError::Header("In-Reply-To"))
    );
    assert_eq!(
        compose(&Draft {
            references: &["<ok@example.com>", "bad"],
            ..draft()
        }),
        Err(ComposeError::Header("References"))
    );
    assert_eq!(
        compose(&Draft {
            from: Mailbox {
                name: None,
                address: "not-an-address"
            },
            ..draft()
        }),
        Err(ComposeError::Address("not-an-address".to_owned()))
    );
    assert_eq!(
        compose(&Draft {
            from: Mailbox {
                name: Some("Ada\r\nBcc: x@evil.test"),
                address: "ada@example.com"
            },
            ..draft()
        }),
        Err(ComposeError::Header("display name"))
    );
    assert_eq!(
        compose(&Draft {
            text: None,
            html: None,
            ..draft()
        }),
        Err(ComposeError::NoBody)
    );
}

/// Header values a person can type (the subject, a display name's quoting) cannot inject a header:
/// a line break in the subject is encoded into the subject itself.
#[test]
fn a_line_break_in_the_subject_cannot_inject_a_header() {
    let raw = compose(&Draft {
        subject: "Hello\r\nBcc: victim@example.net",
        ..draft()
    })
    .expect("a message");
    let message = MessageParser::default()
        .parse(&raw)
        .expect("parseable MIME");
    assert!(message.header("Bcc").is_none());
    assert!(
        message
            .subject()
            .is_some_and(|subject| subject.contains("victim@example.net"))
    );
}

/// Gmail's `Feedback-ID` comes out in its published form, `a:b:c:SenderId`: the identifiers in
/// order, then the sender's id, on one line. A field holding the separator, a space or nothing,
/// more than three identifiers, or a sender id outside 5 to 15 characters is refused, so a
/// malformed header never reaches the feedback loop to be counted under the wrong identifier; a
/// draft without one carries no such header.
#[test]
fn the_feedback_id_is_written_in_gmails_form() {
    let raw = compose(&Draft {
        feedback_id: Some(FeedbackId {
            identifiers: &["campaign7", "workspace3"],
            sender: "norbelys",
        }),
        ..draft()
    })
    .expect("a message");
    assert_eq!(
        header(&raw, "Feedback-ID").as_deref(),
        Some("campaign7:workspace3:norbelys")
    );
    assert_eq!(
        header(&compose(&draft()).expect("a message"), "Feedback-ID"),
        None
    );

    for feedback in [
        FeedbackId {
            identifiers: &["campaign:7"],
            sender: "norbelys",
        },
        FeedbackId {
            identifiers: &["campaign 7"],
            sender: "norbelys",
        },
        FeedbackId {
            identifiers: &["", "workspace3"],
            sender: "norbelys",
        },
        FeedbackId {
            identifiers: &["a", "b", "c", "d"],
            sender: "norbelys",
        },
        FeedbackId {
            identifiers: &[],
            sender: "nb",
        },
        FeedbackId {
            identifiers: &[],
            sender: "a-sender-id-too-long",
        },
    ] {
        assert_eq!(
            compose(&Draft {
                feedback_id: Some(feedback),
                ..draft()
            }),
            Err(ComposeError::Header("Feedback-ID")),
            "{feedback:?}"
        );
    }
}

/// Binary and Unicode-named attachments survive composition and MIME decoding unchanged,
/// while the HTML/plain-text alternatives and normal threading headers remain readable.
#[test]
fn attachments_round_trip_with_both_body_alternatives() {
    let file = super::Attachment {
        filename: "résumé.bin".to_owned(),
        content_type: "application/octet-stream".to_owned(),
        bytes: vec![0, 1, 127, 128, 255],
    };
    let raw = super::compose_with_attachments(
        &Draft {
            html: Some("<p>Hello Grace</p>"),
            ..draft()
        },
        std::slice::from_ref(&file),
    )
    .unwrap();
    let content = crate::inbound::content(&raw).unwrap();
    assert_eq!(content.text.as_deref(), Some("Hello Grace"));
    assert_eq!(content.html.as_deref(), Some("<p>Hello Grace</p>"));
    assert_eq!(content.attachments.len(), 1);
    let read = content.attachments.first().unwrap();
    assert_eq!(read.filename, file.filename);
    assert_eq!(read.bytes, file.bytes);
    assert_eq!(read.content_type, file.content_type);
}
