//! The Norbelys AI client: one [`Client::complete`] call that speaks two wire formats, the
//! Anthropic Messages API and OpenAI-compatible chat completions, over `reqwest`.
//!
//! # What this crate is for
//!
//! Norbelys uses language models for small, bounded jobs, such as classifying an inbound
//! reply or writing a personalised snippet, always with structured output under a JSON
//! schema. This crate is only the wire: it knows providers and their HTTP formats, never
//! workspaces, budgets, prompts or the database. The caller writes the prompt, reserves
//! budget before the call, records the call and decides what to do with the answer.
//!
//! The crate's folder also holds the platform's prompts (`prompts/<use case>/<name>-v<N>.txt`) and
//! their golden sets (`evals/`) as data files: the server embeds the prompts and evaluates them
//! against the golden sets; no code of this crate reads them.
//!
//! # Why two native wire formats
//!
//! Anthropic offers an OpenAI-compatible endpoint, but that endpoint ignores
//! `response_format` and `strict`, so it cannot enforce a schema. Anthropic's native Messages
//! API enforces one through `output_config.format`, and OpenAI's chat completions through
//! `response_format` with `strict: true`, so each provider is spoken to in its own format.
//! Self-hosted models (Ollama, vLLM and others) are reached through the OpenAI-compatible
//! format; whether a given wire and model really enforce a schema is stated by the tested
//! capability matrix ([`matrix`]), and a caller passes its answer to the client as
//! [`Provider::structured_output`].
//!
//! # Billing and the shape of results
//!
//! Providers bill every answer they produce: a refusal, an answer cut at the token limit and
//! an answer that does not match the schema cost the same as a good one. All of these are a
//! [`Completion`] carrying the provider's [`Usage`], told apart by its [`Outcome`], and never
//! an error, so a caller that tracks spend records them with their real usage.
//!
//! An [`AiError`] means no answer was read. Each variant states whether the provider can
//! have billed the call anyway: nothing is billed for a call that was never sent or that the
//! provider refused with an error status, but a call whose answer was lost after sending (a
//! deadline, a broken connection) may have been billed at an unknown amount, which a careful
//! caller charges at the most the call could have cost.
//!
//! # Invariants
//!
//! - One deadline of at most 90 seconds bounds a call, retries and the waits between them
//!   included.
//! - A call is retried only when the provider cannot have billed it, so a retry never pays
//!   twice for one answer.
//! - Prompt and completion content never appear in `Debug` output, in errors or in
//!   telemetry: it may hold personal data from customers' mailboxes.
//! - Model ids, keys and endpoints are configuration ([`Provider`]): model ids and prices
//!   change often, so nothing but the wire formats themselves and the capability matrix is
//!   written in code. The matrix is code because each of its rows is a claim about a provider
//!   that must be tested, never a setting an operator asserts.

mod client;
pub mod matrix;

pub use client::Client;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use url::Url;

/// One provider endpoint as the deployment configures it: the wire format it speaks, where it
/// answers, which model to call and with which key, and how the caller wants failed calls
/// retried. A [`Client`] is built from one `Provider` and calls one model.
#[derive(Debug, Clone)]
pub struct Provider {
    /// The wire format the endpoint speaks.
    pub wire: Wire,
    /// The base URL as the provider's own SDK takes it: `https://api.anthropic.com` for
    /// Anthropic, to which the client appends `/v1/messages`; `https://api.openai.com/v1`, or
    /// a self-hosted `http://127.0.0.1:11434/v1`, for chat completions, to which it appends
    /// `/chat/completions`. A trailing slash makes no difference.
    ///
    /// HTTPS is required, because the key travels in a header. Plain HTTP is accepted only to
    /// `localhost` or to a loopback or private-network address, for a model the operator runs
    /// on their own host or network. The URL may not carry credentials, a query or a fragment.
    pub base_url: Url,
    /// The model every call names, such as `claude-haiku-4-5`.
    pub model: String,
    /// The provider's key: sent as `x-api-key` to Anthropic and as a bearer token to chat
    /// completions, and marked sensitive, so it never appears in `Debug` output.
    pub api_key: SecretString,
    /// Whether this provider and model enforce a JSON schema strictly. Support varies by
    /// provider and by model (a self-hosted server may accept `response_format` and ignore
    /// it), so the caller sets it from the capability matrix ([`matrix::strict`]). When it is
    /// `false`, a request with a schema is refused before anything is sent, instead of paying
    /// for an answer that was never constrained.
    pub structured_output: bool,
    /// The deadline of one call, retries and the waits between them included: above zero and
    /// at most 90 seconds. A caller that holds a lease while a call runs can size the lease
    /// from this value alone.
    pub timeout: Duration,
    /// How many times, and after which waits, a failed call is retried: the caller's policy.
    pub retries: RetrySchedule,
}

/// How a call is retried after an error the provider cannot have billed: a `429`, a `5xx`, or
/// a provider that could not be reached. The schedule is the caller's policy, typically an
/// exponential backoff with jitter; the client applies it and decides only which errors are
/// retried.
///
/// The contract:
///
/// - After such an error, the client retries while it has made fewer than
///   [`max_retries`](RetrySchedule::max_retries) retries; `0` turns retries off.
/// - Before each retry it waits. The provider's `Retry-After` takes precedence when the
///   provider gave one, since the provider knows when its limit resets; otherwise the wait is
///   [`wait(n)`](RetrySchedule::wait), where `n` counts the retries already made, so
///   `wait(0)` comes before the first retry and `wait(1)` before the second.
/// - A wait that would pass the call's deadline ([`Provider::timeout`]) is never slept: the
///   error is returned at once instead, so the caller can hold back every call of the same
///   kind rather than each call blocking until its deadline.
/// - Errors the provider may have billed (a deadline, a connection lost after the request
///   was sent) and errors no wait cures (a refused key, an invalid request) are never
///   retried, whatever the schedule says: a retry could pay twice, or fail the same way.
///
/// `wait` runs on the calling task, so it must be quick and must not block; it is called at
/// most `max_retries` times per call, and only when the provider gave no `Retry-After`.
#[derive(Clone)]
pub struct RetrySchedule {
    /// The most retries after the first attempt.
    pub max_retries: u32,
    /// The wait before a retry when the provider gave no `Retry-After`, given the number of
    /// retries already made (`0` before the first retry).
    pub wait: Arc<dyn Fn(u32) -> Duration + Send + Sync>,
}

// A function cannot be printed, so `Debug` shows the number of retries alone.
impl fmt::Debug for RetrySchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetrySchedule")
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

/// The wire format of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub enum Wire {
    /// The Anthropic Messages API, `POST /v1/messages`
    /// (<https://platform.claude.com/docs/en/api/messages>), with structured output through
    /// `output_config.format`.
    Anthropic,
    /// OpenAI-compatible chat completions, `POST /chat/completions`
    /// (<https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create>),
    /// with structured output through `response_format` and `strict: true`. Most self-hosted
    /// servers speak this format too.
    OpenAiCompatible,
}

/// One call: a system prompt and a conversation, with the bounds of the answer.
#[derive(Clone)]
pub struct Request {
    /// The caller's id for this call, to correlate logs and traces: for example the id of
    /// the record that holds the call's reservation. It is recorded on the call's span, and
    /// chat-completions providers receive it as `X-Client-Request-Id`, which OpenAI keeps so
    /// its support can find a call whose answer never arrived. It must be 1 to 512 visible
    /// ASCII characters (letters, digits and punctuation: no spaces, control characters or
    /// non-ASCII text), as OpenAI requires; the check runs on both wire formats, so a request
    /// is valid whichever provider is configured.
    pub call_id: String,
    /// The system prompt: the instructions that frame the whole conversation. An empty one is
    /// not sent.
    pub system: String,
    /// The conversation, oldest first. It starts with a user message, as both providers
    /// require.
    pub messages: Vec<Message>,
    /// The most output tokens the provider may produce, thinking included on models that
    /// think. It bounds the cost of the answer, so a caller can price a reservation on it
    /// before the call. An answer that reaches it is [`Outcome::Truncated`].
    pub max_tokens: u32,
    /// The sampling temperature, or `None` to keep the model's default and not send the
    /// parameter at all. Current Claude models and OpenAI's reasoning models refuse an
    /// explicit value with `400 Bad Request` (as documented on 2026-10-01), so it is set only
    /// for a model known to take it.
    pub temperature: Option<f32>,
    /// A JSON schema the answer must follow, enforced by the provider. Both wire formats
    /// require every object to set `"additionalProperties": false` and to list all of its
    /// properties as required, and may not support other constraints (numeric ranges, string
    /// lengths). With a schema, a finished answer is parsed as JSON and returned as
    /// [`Output::Json`]. Checking the value against the schema (an enum value the model
    /// invented, for example) is left to the caller's typed deserialisation: the provider's
    /// enforcement is the first check and the caller's types are the second.
    pub schema: Option<serde_json::Value>,
}

/// One turn of the conversation.
#[derive(Clone)]
pub struct Message {
    /// Who wrote it.
    pub role: Role,
    /// Its text.
    pub content: String,
}

/// The author of a turn. The system prompt is not a turn; it is [`Request::system`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The caller: instructions and the input to work on.
    User,
    /// The model, in an earlier answer: for example an invalid answer, followed by a user
    /// turn that names what is wrong with it, so the model can correct it.
    Assistant,
}

/// The provider's answer to one call.
///
/// The provider billed every outcome, refusals and invalid answers included, so a caller that
/// tracks spend records [`Completion::usage`] whatever the [`Outcome`].
pub struct Completion {
    /// How the answer ended, with what the model wrote.
    pub outcome: Outcome,
    /// The tokens the provider reported for this call. `None` when the answer carried no
    /// figures, as some self-hosted servers do: the real usage is then unknown, and a caller
    /// that tracks spend charges the most the call could have cost. Usage is never estimated
    /// here.
    pub usage: Option<Usage>,
    /// The model that answered, as the provider names it (often a dated snapshot of the
    /// configured id); the configured model when the answer does not say.
    pub model: String,
    /// The provider's own stop or finish reason (`end_turn`, `stop`, `refusal`, `length`, …),
    /// for logs; [`Completion::outcome`] is its interpretation.
    pub finish_reason: Option<String>,
    /// The provider's id for this call, to quote to its support: the request id header
    /// (`request-id` from Anthropic, `x-request-id` from chat completions), else the answer's
    /// own id (`msg_…`, `chatcmpl-…`).
    pub provider_request_id: Option<String>,
}

/// How an answer ended. The provider billed every variant.
#[derive(PartialEq)]
pub enum Outcome {
    /// The model finished normally: its text, or with a schema the parsed JSON.
    Completed(Output),
    /// The model declined to answer, for safety or policy reasons: Anthropic's
    /// `stop_reason: "refusal"`, a `message.refusal` from chat completions, or a
    /// `content_filter` finish. The answer is not usable; the caller goes on as it would
    /// without AI.
    Refused {
        /// What the model wrote, such as its explanation; possibly empty.
        text: String,
    },
    /// The answer was cut short: it reached [`Request::max_tokens`] or the model's context
    /// window. With a schema, the partial text is not valid JSON.
    Truncated {
        /// The partial answer.
        text: String,
    },
    /// The answer cannot be used as asked: a schema was asked for and the text is not JSON,
    /// or the answer stopped for a reason no request of this crate leads to, such as a tool
    /// call when no tools were offered. A caller can recover by calling once more with the
    /// invalid answer as an assistant turn and the violation in a user turn, and give up if
    /// that answer fails too.
    InvalidOutput {
        /// What the model wrote.
        text: String,
        /// What is wrong with it, written so it can be shown to the model in such a retry.
        violation: String,
    },
}

/// A finished answer.
#[derive(PartialEq)]
pub enum Output {
    /// The text, when the request had no schema.
    Text(String),
    /// The parsed JSON value, when it had one.
    Json(serde_json::Value),
}

/// The tokens one call was billed for, as the provider reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens. From Anthropic, this adds the prompt-cache reads and writes, which the
    /// API reports apart from `input_tokens`; this crate sets no cache breakpoints, so they
    /// are normally zero. From chat completions, `prompt_tokens`, which already counts cached
    /// input.
    pub input_tokens: u32,
    /// Output tokens, including any thinking or reasoning tokens the model spent.
    pub output_tokens: u32,
}

/// Why a call produced no [`Completion`].
///
/// Each variant states whether the provider can have billed the call, which matters to a
/// caller that reserved budget: a call that was never sent, or that the provider refused with
/// an error status, cost nothing; a call whose answer was lost after sending may have cost up
/// to its bound. Messages carry no URL, key or body, so they are safe to log.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub enum AiError {
    /// The call was refused before anything was sent: an invalid configuration
    /// ([`Client::new`]), a schema asked of a provider whose
    /// [`Provider::structured_output`] is `false`, an invalid [`Request::call_id`], or a
    /// request that could not be encoded. Not billed. Not retryable: the configuration or the
    /// request must change first.
    #[error("AI call refused before sending: {0}")]
    Config(&'static str),
    /// The provider could not be reached: DNS, the connection or TLS failed, or connecting
    /// took longer than 10 seconds. Nothing was sent, so nothing was billed. Retried as the
    /// [`RetrySchedule`] allows, within the deadline, before it is returned.
    #[error("the AI provider could not be reached")]
    Unreachable,
    /// The connection failed after the request was sent, so the answer was lost. The
    /// provider may have run the call and billed it, at an unknown amount. Never retried
    /// here, since a retry could pay twice for one answer.
    #[error("the connection to the AI provider failed after the request was sent")]
    Transport,
    /// The deadline, [`Provider::timeout`], passed before a complete answer arrived. The
    /// provider may have run the call and billed it, at an unknown amount. Never retried
    /// here, since a retry could pay twice for one answer.
    #[error("the AI call passed its deadline")]
    Timeout,
    /// `429 Too Many Requests`: the provider's rate limit. Not billed. Retried as the
    /// [`RetrySchedule`] allows, after the provider's `Retry-After` when it gave one; returned
    /// when the retries are spent or the wait would pass the deadline, so the caller can hold
    /// back every call of the same kind for `retry_after` instead of each call waiting on its
    /// own.
    #[error("the AI provider is rate limiting")]
    RateLimited {
        /// The provider's wait, when it gave one.
        retry_after: Option<Duration>,
    },
    /// `401 Unauthorized` or `403 Forbidden`: the key is invalid or revoked, or it may not
    /// use this model or region. Not billed. Not retryable: an operator must fix the key.
    #[error("the AI provider refused the credentials (HTTP {status})")]
    Unauthorized {
        /// The status code.
        status: u16,
    },
    /// A `5xx` status, Anthropic's `529 Overloaded` among them: the provider failed. Not
    /// billed. Retried as the [`RetrySchedule`] allows, within the deadline, before it is
    /// returned.
    #[error("the AI provider failed (HTTP {status})")]
    Provider {
        /// The status code.
        status: u16,
        /// The provider's wait, when it gave one.
        retry_after: Option<Duration>,
    },
    /// Any other status: the provider refused this request and would refuse it again. For
    /// example an invalid parameter (`400`), an unknown model (`404`), a request too large
    /// (`413`), a schema the provider cannot compile, or an account without credit
    /// (Anthropic's `402`, or OpenAI's `429` with `insufficient_quota`, which no wait cures).
    /// Not billed. Not retryable without a change.
    #[error("the AI provider rejected the request (HTTP {status})")]
    Rejected {
        /// The status code.
        status: u16,
    },
    /// A success status whose body is not an answer in the configured wire format, or is
    /// larger than 4 MiB. The provider may have billed the call, at an unknown amount. Not
    /// retryable: it usually means the endpoint does not speak the configured format.
    #[error("the AI provider's answer could not be read")]
    InvalidResponse,
}

impl AiError {
    /// Whether the same request can succeed if the caller tries it again later, as a new
    /// call: `true` when the provider could not be reached or failed, the answer was lost,
    /// the deadline passed or the provider was rate limiting; `false` when the
    /// configuration, the key or the request must change first. The client's own retries
    /// have already run by the time an error is returned; this is about trying again later.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Unreachable
            | Self::Transport
            | Self::Timeout
            | Self::RateLimited { .. }
            | Self::Provider { .. } => true,
            Self::Config(_)
            | Self::Unauthorized { .. }
            | Self::Rejected { .. }
            | Self::InvalidResponse => false,
        }
    }
}

// Prompt and completion content may hold personal data from customers' mailboxes, so these
// `Debug` implementations print the shape of a value (its role, counts, the outcome's kind)
// and never its text.

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("call_id", &self.call_id)
            .field("messages", &self.messages.len())
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("schema", &self.schema.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Completion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Completion")
            .field("outcome", &self.outcome)
            .field("usage", &self.usage)
            .field("model", &self.model)
            .field("finish_reason", &self.finish_reason)
            .field("provider_request_id", &self.provider_request_id)
            .finish()
    }
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed(output) => f.debug_tuple("Completed").field(output).finish(),
            Self::Refused { .. } => f.write_str("Refused"),
            Self::Truncated { .. } => f.write_str("Truncated"),
            Self::InvalidOutput { violation, .. } => f
                .debug_struct("InvalidOutput")
                .field("violation", violation)
                .finish_non_exhaustive(),
        }
    }
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text(_) => "Text(..)",
            Self::Json(_) => "Json(..)",
        })
    }
}
