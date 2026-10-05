//! Classification of an inbound message the rules could not decide.
//!
//! The inbox's rules decide first, by authority: a delivery status notification, an abuse
//! report, an `Auto-Submitted` header, a one-click unsubscribe, the patterns of automatic
//! notices. Only a message they leave as a human reply or unknown reaches AI, and only in a
//! workspace that turned `settings.ai.classify_replies` on. AI may then say it is a human reply,
//! an out-of-office notice, another automatic reply or unknown, with a sentiment; it never
//! decides a bounce, a complaint or an unsubscribe, which carry effects.
//!
//! What is sent: the headers that decide (`From`, `Subject`, `Auto-Submitted`,
//! `In-Reply-To`, each bounded) and a redacted excerpt of the body: quoted earlier messages cut,
//! email addresses, URLs and phone numbers replaced (`domain::ai::redact`), at most 2,000
//! characters.
//!
//! The answer is checked against its schema; an answer that fails is shown to the model once
//! with what is wrong, and a second failure gives up. A verdict whose confidence is below the
//! workspace's threshold (0.7 by default) asks a person to review instead of being applied, and
//! so does a random sample of the confident ones (`settings.ai.review_sample`, 5 % by default):
//! self-reported confidence is not calibration, so the people's decisions on that uniform sample
//! measure each class's precision and recall in production, and the threshold is set from them.
//! Each review asked counts in `norbelys_ai_reviews_requested_total` by reason.
//!
//! The prompt is chosen once per message: the current one, or, while a newer version is tried as
//! a canary, that version on its share of the messages (`Ai::prompt`). The settled call's row
//! records whether its verdict fell below the threshold, which is the review rate the canary is
//! guarded by.
//!
//! # For the `inbox.classify` kind
//!
//! The kind (queue `ai`, effect `ExternalRetryable`, recovery hook
//! [`settle_abandoned`](super::store::settle_abandoned)) calls [`classify`] with the message it read,
//! then applies [`Classified::Apply`] with compare-and-set on the revision it read and
//! `classification_source <> 'manual'`, or sets `review_requested_at` for
//! [`Classified::Review`], and keeps the rules' verdict for [`Classified::Skipped`], trying again
//! after [`Skip::retry_after`] when it says so.

use std::fmt;
use std::time::Duration;

use norbelys_ai::{AiError, Message, Role};
use serde_json::{Map, Value, json};

use super::{Ai, Answer, Call, CallError, store, telemetry};
use crate::domain::ai::redact::excerpt;
use crate::domain::ai::{ReviewReason, UseCase, review};
use crate::jobs::{JobContext, JobError};

/// The most characters of the body's redacted excerpt sent.
pub const EXCERPT_CHARS: usize = 2_000;
/// The most characters of each header sent.
const HEADER_CHARS: usize = 300;
/// The most output tokens of an answer, thinking included: a verdict needs a few dozen.
pub const MAX_TOKENS: u32 = 1_024;
/// The most reasons kept from an answer, and the most characters of each.
const REASONS: usize = 3;
const REASON_CHARS: usize = 200;
/// How long a job waits before trying again after a provider failure that gave no wait.
const PROVIDER_RETRY: Duration = Duration::from_secs(60);

/// The inbound message to classify, as the inbox read it.
#[derive(Clone, Copy)]
pub struct Inbound<'a> {
    /// The `From` header.
    pub from: Option<&'a str>,
    /// The `Subject` header.
    pub subject: Option<&'a str>,
    /// The `Auto-Submitted` header.
    pub auto_submitted: Option<&'a str>,
    /// The `In-Reply-To` header.
    pub in_reply_to: Option<&'a str>,
    /// The body as text (the stored excerpt is enough); it is redacted before it is sent.
    pub body: &'a str,
}

/// The classifications AI may give: the four of an inbound message's classifications that
/// carry no effect of their own.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumString, strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Classification {
    /// A person wrote it.
    HumanReply,
    /// An automatic reply other than an absence notice.
    AutoReply,
    /// An automatic absence notice.
    OutOfOffice,
    /// Cannot be told.
    Unknown,
}

impl Classification {
    /// The classification as `inbound_messages.classification` stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The writer's attitude towards the outreach; its vocabulary is the inbox's.
pub use crate::domain::inbox::Sentiment;

/// The model's verdict, checked.
#[derive(Clone, PartialEq)]
pub struct Verdict {
    /// The classification.
    pub classification: Classification,
    /// The sentiment.
    pub sentiment: Sentiment,
    /// The model's own confidence, from 0 to 1.
    pub confidence: f64,
    /// One to three short reasons, at most 200 characters each. They are the model's words
    /// about a customer's mail: shown to the workspace, never logged.
    pub reasons: Vec<String>,
}

// The reasons are the model's words about a customer's mail, so `Debug` shows their number.
impl fmt::Debug for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verdict")
            .field("classification", &self.classification)
            .field("sentiment", &self.sentiment)
            .field("confidence", &self.confidence)
            .field("reasons", &self.reasons.len())
            .finish()
    }
}

/// What the classification of one message came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Classified {
    /// Apply the verdict, with compare-and-set on the revision the message was read at and
    /// never over a manual classification.
    Apply(Verdict),
    /// The model was not confident enough: ask a person to review, proposing the verdict.
    Review(Verdict),
    /// No verdict: the rules' verdict stands.
    Skipped(Skip),
}

/// Why a message got no verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The workspace has not turned classification on.
    Off,
    /// The deployment has no provider for classification.
    Unavailable,
    /// The month's AI budget does not admit the call.
    OverBudget,
    /// Classification is paused after a provider's rate limit, for this long.
    Paused(Duration),
    /// The model declined.
    Refused,
    /// The answer was cut short.
    Truncated,
    /// The answer failed its checks twice.
    Invalid,
    /// The provider gave no answer; `retryable` when trying again later can help.
    Provider {
        /// Whether the same call can succeed later.
        retryable: bool,
        /// The provider's own wait, when it gave one.
        retry_after: Option<Duration>,
    },
}

impl Skip {
    /// How long to wait before classifying the message again, for the causes that pass (a
    /// pause, a provider failure); `None` when trying again would not help, and the rules'
    /// verdict is final.
    #[must_use]
    pub fn retry_after(self) -> Option<Duration> {
        match self {
            Self::Paused(wait) => Some(wait),
            Self::Provider {
                retryable: true,
                retry_after,
            } => Some(retry_after.unwrap_or(PROVIDER_RETRY)),
            Self::Off
            | Self::Unavailable
            | Self::OverBudget
            | Self::Refused
            | Self::Truncated
            | Self::Invalid
            | Self::Provider {
                retryable: false, ..
            } => None,
        }
    }
}

/// Classifies `inbound` for the job `cx` runs, as the module describes: nothing is sent unless
/// the workspace turned classification on and the deployment serves it; the prompt is chosen once
/// (the current one, or a canary on its share of calls); an invalid answer is corrected once;
/// the workspace's threshold and review sample decide between applying and reviewing, and the
/// call's row records whether the verdict fell below the threshold.
///
/// # Errors
///
/// [`JobError`] when the job's lease was lost or the database failed; every other failure is a
/// [`Classified::Skipped`].
pub async fn classify(cx: &mut JobContext, inbound: &Inbound<'_>) -> Result<Classified, JobError> {
    let settings = {
        let mut tx = cx.db().begin_in(cx.workspace()).await?;
        let settings = store::read_settings(&mut tx, cx.workspace()).await?;
        tx.commit().await?;
        settings
    };
    if !settings.classify_replies {
        return Ok(Classified::Skipped(Skip::Off));
    }
    let Some(ai) = cx.env::<Ai>().ok().cloned() else {
        return Ok(Classified::Skipped(Skip::Unavailable));
    };
    let served = ai.prompt(cx.db(), UseCase::Classification).await;
    let turns = messages(inbound);
    let request = |messages: Vec<Message>| Call {
        use_case: UseCase::Classification,
        prompt: served.prompt,
        canary: served.canary,
        messages,
        schema: schema(),
        max_tokens: MAX_TOKENS,
        deadline: None,
    };
    let answer = match super::call(cx, &ai, request(turns.clone()), check).await {
        Ok(Answer::Invalid { text, violation }) => {
            let corrected = corrected(turns, text, &violation);
            super::call(cx, &ai, request(corrected), check).await
        }
        first => first,
    };
    Ok(match answer {
        Ok(Answer::Valid(verdict, call)) => {
            let reason = review(
                verdict.confidence,
                settings.confidence_threshold,
                settings.review_sample,
                crate::jobs::draw(),
            );
            let low = reason == Some(ReviewReason::LowConfidence);
            store::record_review(cx.db(), cx.workspace(), call, low).await?;
            match reason {
                None => Classified::Apply(verdict),
                Some(reason) => {
                    telemetry::review_requested(UseCase::Classification, reason);
                    Classified::Review(verdict)
                }
            }
        }
        Ok(Answer::Refused) => Classified::Skipped(Skip::Refused),
        Ok(Answer::Truncated) => Classified::Skipped(Skip::Truncated),
        Ok(Answer::Invalid { .. }) => Classified::Skipped(Skip::Invalid),
        Err(CallError::Job(error)) => return Err(error),
        Err(CallError::Unavailable) => Classified::Skipped(Skip::Unavailable),
        Err(CallError::Paused(wait)) => Classified::Skipped(Skip::Paused(wait)),
        Err(CallError::OverBudget) => Classified::Skipped(Skip::OverBudget),
        Err(CallError::Provider(error)) => Classified::Skipped(Skip::Provider {
            retryable: error.is_retryable(),
            retry_after: match error {
                AiError::RateLimited { retry_after } | AiError::Provider { retry_after, .. } => {
                    retry_after
                }
                _ => None,
            },
        }),
    })
}

/// The input turn: the deciding headers, each on one line and bounded, then the redacted
/// excerpt of the body between markers.
#[must_use]
pub fn messages(inbound: &Inbound<'_>) -> Vec<Message> {
    let header = |value: Option<&str>| {
        value.map_or_else(
            || "(absent)".to_owned(),
            |value| {
                value
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(HEADER_CHARS)
                    .collect::<String>()
                    .trim()
                    .to_owned()
            },
        )
    };
    let body = excerpt(inbound.body, EXCERPT_CHARS).text;
    let content = format!(
        "From: {}\nSubject: {}\nAuto-Submitted: {}\nIn-Reply-To: {}\n\nExcerpt of the body:\n<<<\n{}\n>>>",
        header(inbound.from),
        header(inbound.subject),
        header(inbound.auto_submitted),
        header(inbound.in_reply_to),
        if body.is_empty() {
            "(empty)"
        } else {
            body.as_str()
        },
    );
    vec![Message {
        role: Role::User,
        content,
    }]
}

/// The turns of the corrective call: the input, the invalid answer, and what is wrong with it.
fn corrected(mut turns: Vec<Message>, answer: String, violation: &str) -> Vec<Message> {
    turns.push(Message {
        role: Role::Assistant,
        content: answer,
    });
    turns.push(Message {
        role: Role::User,
        content: format!(
            "Your answer was not valid: {violation}. Answer again with one JSON object that follows the schema exactly."
        ),
    });
    turns
}

/// The JSON schema of a verdict, in the strict form both wire formats enforce (every property
/// required, no other property).
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "classification": {
                "type": "string",
                "enum": ["human_reply", "auto_reply", "out_of_office", "unknown"],
            },
            "sentiment": {"type": "string", "enum": ["positive", "neutral", "negative"]},
            "confidence": {"type": "number"},
            "reasons": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["classification", "sentiment", "confidence", "reasons"],
        "additionalProperties": false,
    })
}

/// Checks an answer against the schema and the bounds the schema cannot express: a known
/// classification and sentiment, a confidence from 0 to 1, reasons as strings. Extra reasons
/// and long ones are cut rather than refused. Violations name the field, never its value.
///
/// # Errors
///
/// What is wrong, to show the model.
pub fn check(answer: &Value) -> Result<Verdict, String> {
    let fields = answer
        .as_object()
        .ok_or_else(|| "the answer is not a JSON object".to_owned())?;
    only(
        fields,
        &["classification", "sentiment", "confidence", "reasons"],
    )?;
    let classification = fields
        .get("classification")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Classification>().ok())
        .ok_or_else(|| {
            "`classification` must be one of human_reply, auto_reply, out_of_office, unknown"
                .to_owned()
        })?;
    let sentiment = fields
        .get("sentiment")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Sentiment>().ok())
        .ok_or_else(|| "`sentiment` must be one of positive, neutral, negative".to_owned())?;
    let confidence = fields
        .get("confidence")
        .and_then(Value::as_f64)
        .filter(|confidence| (0.0..=1.0).contains(confidence))
        .ok_or_else(|| "`confidence` must be a number from 0 to 1".to_owned())?;
    let reasons = strings(fields.get("reasons"))
        .ok_or_else(|| "`reasons` must be an array of strings".to_owned())?;
    Ok(Verdict {
        classification,
        sentiment,
        confidence,
        reasons: bounded(reasons),
    })
}

/// Refuses an object with a property outside `allowed`, without naming it.
pub(super) fn only(fields: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if fields.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(format!(
            "the answer has a property the schema does not allow; its properties are {}",
            allowed.join(", ")
        ))
    }
}

/// The strings of a JSON array, or `None` when it is not an array of strings.
pub(super) fn strings(value: Option<&Value>) -> Option<Vec<&str>> {
    value?.as_array()?.iter().map(Value::as_str).collect()
}

/// At most [`REASONS`] non-empty reasons of at most [`REASON_CHARS`] characters each.
pub(super) fn bounded(reasons: Vec<&str>) -> Vec<String> {
    reasons
        .into_iter()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .take(REASONS)
        .map(|reason| reason.chars().take(REASON_CHARS).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::{Classification, Inbound, Sentiment, Skip, check, messages};

    /// Every classification and sentiment the schema lists is accepted, and a value outside them
    /// (an invented class, a class AI may never give such as `bounce`), a confidence outside 0
    /// to 1, a missing or extra property and a non-object are violations that name the field but
    /// never repeat the value, so they can be shown to the model and never leak.
    #[test]
    fn answers_are_checked_against_the_schema() {
        for classification in Classification::iter() {
            for sentiment in Sentiment::iter() {
                let answer = json!({
                    "classification": classification.as_str(),
                    "sentiment": sentiment.as_str(),
                    "confidence": 0.9,
                    "reasons": ["Mentions a return date"],
                });
                let checked = check(&answer).unwrap();
                assert_eq!(
                    (checked.classification, checked.sentiment),
                    (classification, sentiment)
                );
            }
        }
        let good = json!({"classification": "human_reply", "sentiment": "positive", "confidence": 1, "reasons": []});
        assert!(check(&good).is_ok());
        for (wrong, field, value) in [
            (
                json!({"classification": "bounce", "sentiment": "neutral", "confidence": 0.5, "reasons": []}),
                "classification",
                "bounce",
            ),
            (
                json!({"classification": "human_reply", "sentiment": "angry", "confidence": 0.5, "reasons": []}),
                "sentiment",
                "angry",
            ),
            (
                json!({"classification": "human_reply", "sentiment": "neutral", "confidence": 1.5, "reasons": []}),
                "confidence",
                "1.5",
            ),
            (
                json!({"classification": "human_reply", "sentiment": "neutral", "confidence": "high", "reasons": []}),
                "confidence",
                "high",
            ),
            (
                json!({"classification": "human_reply", "sentiment": "neutral", "confidence": 0.5, "reasons": "secret-reason"}),
                "reasons",
                "secret-reason",
            ),
            (
                json!({"classification": "human_reply", "sentiment": "neutral", "confidence": 0.5}),
                "reasons",
                "secret-reason",
            ),
            (
                json!({"classification": "human_reply", "sentiment": "neutral", "confidence": 0.5, "reasons": [], "leak": "ada@example.com"}),
                "property",
                "ada@example.com",
            ),
            (json!(["human_reply"]), "object", "human_reply"),
        ] {
            let violation = check(&wrong).unwrap_err();
            assert!(violation.contains(field), "{violation}");
            assert!(!violation.contains(value), "{violation} repeats {value}");
        }
    }

    /// Reasons are kept to three, each trimmed and cut at 200 characters, empty ones dropped:
    /// a verbose answer is shortened, not refused and paid for again.
    #[test]
    fn reasons_are_bounded_not_refused() {
        let long = "x".repeat(500);
        let answer = json!({
            "classification": "auto_reply",
            "sentiment": "neutral",
            "confidence": 0.8,
            "reasons": ["  first ", "", long, "third", "fourth"],
        });
        let reasons = check(&answer).unwrap().reasons;
        assert_eq!(reasons.len(), 3);
        assert_eq!(reasons[0], "first");
        assert_eq!(reasons[1].chars().count(), 200);
        assert_eq!(reasons[2], "third");
    }

    /// Only the causes that pass are tried again: a pause after its wait, a retryable provider
    /// failure after the provider's wait or a minute; everything else keeps the rules' verdict.
    #[test]
    fn only_passing_causes_are_retried() {
        let wait = Duration::from_secs(7);
        assert_eq!(Skip::Paused(wait).retry_after(), Some(wait));
        assert_eq!(
            Skip::Provider {
                retryable: true,
                retry_after: Some(wait)
            }
            .retry_after(),
            Some(wait)
        );
        assert_eq!(
            Skip::Provider {
                retryable: true,
                retry_after: None
            }
            .retry_after(),
            Some(Duration::from_secs(60))
        );
        for final_skip in [
            Skip::Off,
            Skip::Unavailable,
            Skip::OverBudget,
            Skip::Refused,
            Skip::Truncated,
            Skip::Invalid,
            Skip::Provider {
                retryable: false,
                retry_after: Some(wait),
            },
        ] {
            assert_eq!(final_skip.retry_after(), None, "{final_skip:?}");
        }
    }

    /// The input carries the four deciding headers on one line each (a header cannot inject a
    /// line), absent ones said so, and the body's redacted excerpt only: the quoted thread, the
    /// address and the phone number never reach the provider.
    #[test]
    fn the_input_is_bounded_and_redacted() {
        let inbound = Inbound {
            from: Some("Ada <ada@example.com>"),
            subject: Some("Re: Demo\r\nBcc: someone"),
            auto_submitted: None,
            in_reply_to: Some("<msg.thr.tag@mail.example>"),
            body: "Call me on +1 555 010 9999.\n\nOn Mon, Oct 5, 2026 at 10:02 AM Max <max@example.org> wrote:\n> Original pitch",
        };
        let turns = messages(&inbound);
        assert_eq!(turns.len(), 1);
        let content = &turns[0].content;
        assert!(content.contains("From: Ada <ada@example.com>"), "{content}");
        assert!(
            content.contains("Subject: Re: Demo  Bcc: someone"),
            "{content}"
        );
        assert!(content.contains("Auto-Submitted: (absent)"), "{content}");
        assert!(content.contains("Call me on [phone]."), "{content}");
        assert!(!content.contains("Original pitch"), "{content}");
        assert!(!content.contains("max@example.org"), "{content}");
    }
}
