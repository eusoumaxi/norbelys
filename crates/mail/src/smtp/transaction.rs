//! One MAIL/RCPT/DATA transaction on an authenticated session, without retry or pool ownership.
//! A loss after DATA's 354 reply remains uncertain; partial refusals and throttles keep their scope.

use lettre::transport::smtp::client::AsyncSmtpConnection;
use lettre::transport::smtp::commands::{Data, Mail, Rcpt};
use lettre::transport::smtp::extension::{Extension, MailBodyParameter, MailParameter};
use lettre::transport::smtp::response::Response;
use tokio::time::{Instant, timeout_at};

use super::protocol::{earliest, failed, negative, reply_text, response_parts, within};
use super::reply::{self, ReplyId};
use super::{COMMAND_TIMEOUT, CONTENT_TIMEOUT, DATA_RESERVE};
use crate::status::EnhancedStatus;
use crate::submission::{
    Cause, Envelope, Failure, Phase, RecipientRefusal, Rejection, Scope, Submission,
};
use crate::text;

/// How a command ended when it did not succeed.
enum Step {
    /// A negative reply was read.
    Reply { code: u16, text: String },
    /// No reply: the rejection for the phase.
    Failed(Rejection),
}

pub(super) async fn run(
    connection: &mut AsyncSmtpConnection,
    envelope: &Envelope,
    mime: &[u8],
    deadline: Instant,
    reply_id: ReplyId,
) -> Result<Submission, Rejection> {
    if Instant::now() >= deadline {
        return Err(Rejection::local(
            Failure::Transient,
            Phase::MailFrom,
            Scope::Connection,
            Cause::Deadline,
            "the submission deadline passed before MAIL FROM",
        ));
    }
    let unsupported = |detail: &str| {
        Rejection::local(
            Failure::Permanent,
            Phase::MailFrom,
            Scope::Message,
            Cause::Unsupported,
            detail,
        )
    };
    let mut parameters = Vec::new();
    if envelope.has_non_ascii() {
        if !connection
            .server_info()
            .supports_feature(Extension::SmtpUtfEight)
        {
            return Err(unsupported(
                "an address needs SMTPUTF8, which the server does not offer",
            ));
        }
        parameters.push(MailParameter::SmtpUtfEight);
    }
    if !mime.is_ascii() {
        if !connection
            .server_info()
            .supports_feature(Extension::EightBitMime)
        {
            return Err(unsupported(
                "the content needs 8BITMIME, which the server does not offer",
            ));
        }
        parameters.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }

    let from = Mail::new(Some(envelope.from().clone()), parameters);
    command(connection, from, Phase::MailFrom, deadline)
        .await
        .map_err(|step| step.rejection(Phase::MailFrom))?;

    let mut refused = Vec::new();
    for recipient in envelope.recipients() {
        let rcpt = Rcpt::new(recipient.clone(), Vec::new());
        match command(connection, rcpt, Phase::RcptTo, deadline).await {
            Ok(_) => {}
            Err(Step::Reply { code, text }) => {
                let status = EnhancedStatus::find(&text);
                // A throttle or a closing channel speaks for the session, not the recipient: stop
                // before DATA, nothing sent, so the whole message is retried later.
                let stop = code == 421
                    || reply::classify(Phase::RcptTo, code, status, &text).cause
                        == Cause::Throttled;
                refused.push(RecipientRefusal {
                    recipient: recipient.to_string(),
                    code,
                    status,
                    diagnostic: text::bounded(&format!("{code} {text}"), text::DIAGNOSTIC_CHARS),
                });
                if stop {
                    return Err(all_refused(refused));
                }
            }
            Err(Step::Failed(rejection)) => {
                return Err(Rejection {
                    refused,
                    ..rejection
                });
            }
        }
    }
    if refused.len() == envelope.recipients().len() {
        return Err(all_refused(refused));
    }

    if deadline.saturating_duration_since(Instant::now()) < DATA_RESERVE {
        return Err(Rejection {
            refused,
            ..Rejection::local(
                Failure::Transient,
                Phase::Data,
                Scope::Connection,
                Cause::Deadline,
                "less than 180 s of the submission budget remained for DATA",
            )
        });
    }
    if let Err(step) = command(connection, Data, Phase::Data, deadline).await {
        return Err(Rejection {
            refused,
            ..step.rejection(Phase::Data)
        });
    }

    // The server answered 354: from here a failure without the final reply may hide an
    // acceptance, so it is uncertain.
    let uncertain = |detail: &str| {
        Rejection::local(
            Failure::Uncertain,
            Phase::Data,
            Scope::Connection,
            Cause::NoReply,
            detail,
        )
    };
    let sending = timeout_at(
        earliest(deadline, CONTENT_TIMEOUT),
        connection.message(mime),
    )
    .await;
    match sending {
        Ok(Ok(response)) => {
            let (code, text) = response_parts(&response);
            Ok(Submission {
                provider_message_id: reply_id.extract(&text),
                reply: Some(text::bounded(
                    &format!("{code} {text}"),
                    text::DIAGNOSTIC_CHARS,
                )),
                refused,
            })
        }
        Ok(Err(error)) => match error.status() {
            Some(code) => Err(Rejection {
                refused,
                ..negative(Phase::Data, u16::from(code), &reply_text(&error))
            }),
            None => Err(Rejection {
                refused,
                ..uncertain(&error.to_string())
            }),
        },
        Err(_) => Err(Rejection {
            refused,
            ..uncertain("no final reply after the content before the deadline")
        }),
    }
}

/// One command within the phase deadline and `deadline`.
async fn command<C: std::fmt::Display>(
    connection: &mut AsyncSmtpConnection,
    command: C,
    phase: Phase,
    deadline: Instant,
) -> Result<Response, Step> {
    match within(deadline, COMMAND_TIMEOUT, connection.command(command)).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => match error.status() {
            Some(code) => Err(Step::Reply {
                code: u16::from(code),
                text: reply_text(&error),
            }),
            None => Err(Step::Failed(failed(phase, &error))),
        },
        Err(_) => Err(Step::Failed(Rejection::local(
            Failure::Transient,
            phase,
            Scope::Connection,
            Cause::NoReply,
            "no reply before the deadline",
        ))),
    }
}

impl Step {
    fn rejection(self, phase: Phase) -> Rejection {
        match self {
            Self::Reply { code, text } => negative(phase, code, &text),
            Self::Failed(rejection) => rejection,
        }
    }
}

/// The message-level rejection when every recipient was refused: the dominant refusal's
/// meaning (a throttle first, then a `4xx`, then the first), listing them all.
fn all_refused(refused: Vec<RecipientRefusal>) -> Rejection {
    let meaning = |refusal: &RecipientRefusal| {
        reply::classify(
            Phase::RcptTo,
            refusal.code,
            refusal.status,
            &refusal.diagnostic,
        )
    };
    let dominant = refused
        .iter()
        .find(|refusal| meaning(refusal).cause == Cause::Throttled)
        .or_else(|| refused.iter().find(|refusal| refusal.code < 500))
        .or_else(|| refused.first());
    let Some(dominant) = dominant else {
        return Rejection::local(
            Failure::Permanent,
            Phase::RcptTo,
            Scope::Message,
            Cause::Refused,
            "no recipient",
        );
    };
    let decided = meaning(dominant);
    Rejection {
        failure: decided.failure,
        phase: Phase::RcptTo,
        scope: decided.scope,
        cause: decided.cause,
        code: Some(dominant.code),
        status: dominant.status,
        retry_after: None,
        diagnostic: dominant.diagnostic.clone(),
        refused,
    }
}
