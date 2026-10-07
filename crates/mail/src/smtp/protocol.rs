//! Shared command deadlines and mapping of lettre replies to the submission contract.

use std::future::Future;
use std::time::Duration;

use lettre::transport::smtp::Error as SmtpError;
use lettre::transport::smtp::response::Response;
use tokio::time::{Instant, timeout_at};

use super::reply;
use crate::status::EnhancedStatus;
use crate::submission::{Cause, Failure, Phase, Rejection, Scope};
use crate::text;

/// `future` bounded by `limit` from now and by `deadline`.
pub(super) async fn within<F: Future>(
    deadline: Instant,
    limit: Duration,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    timeout_at(earliest(deadline, limit), future).await
}

pub(super) fn earliest(deadline: Instant, limit: Duration) -> Instant {
    Instant::now()
        .checked_add(limit)
        .map_or(deadline, |at| deadline.min(at))
}

/// The rejection for a `lettre` error: the reply's meaning when one was read, else `transient`
/// without a reply.
pub(super) fn failed(phase: Phase, error: &SmtpError) -> Rejection {
    match error.status() {
        Some(code) => negative(phase, u16::from(code), &reply_text(error)),
        None => Rejection::local(
            Failure::Transient,
            phase,
            Scope::Connection,
            Cause::NoReply,
            &error.to_string(),
        ),
    }
}

/// The rejection for a negative reply read in `phase`.
pub(super) fn negative(phase: Phase, code: u16, text: &str) -> Rejection {
    let status = EnhancedStatus::find(text);
    let meaning = reply::classify(phase, code, status, text);
    Rejection {
        failure: meaning.failure,
        phase,
        scope: meaning.scope,
        cause: meaning.cause,
        code: Some(code),
        status,
        retry_after: None,
        diagnostic: text::bounded(&format!("{code} {text}"), text::DIAGNOSTIC_CHARS),
        refused: Vec::new(),
    }
}

/// The text of a negative reply `lettre` turned into an error (its lines, joined).
pub(crate) fn reply_text(error: &SmtpError) -> String {
    std::error::Error::source(error)
        .map(ToString::to_string)
        .unwrap_or_default()
}

pub(crate) fn response_parts(response: &Response) -> (u16, String) {
    (
        u16::from(response.code()),
        response.message().collect::<Vec<_>>().join(" "),
    )
}
