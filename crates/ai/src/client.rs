//! The client: one [`Client::complete`] for both wire formats.
//!
//! The wire format is a deployment setting chosen from a closed set of two, so it is an enum
//! that each call dispatches on, not a trait with one implementation per format. The
//! `anthropic` and `openai` submodules hold what differs between the formats, the request
//! body and the reading of an answer; this module holds everything they share.
//!
//! # Deadline
//!
//! One deadline bounds a call, from the first byte sent to the last byte read, retries and
//! the waits between them included. When it passes, the call ends with
//! [`AiError::Timeout`], whatever it was doing. A caller that holds a lease while it waits
//! can therefore size the lease from [`Provider::timeout`] alone.
//!
//! # Retries
//!
//! A call is retried only when the provider cannot have billed it: on a `429`, on a `5xx`
//! (Anthropic's `529 Overloaded` included) and when the provider could not be reached. A
//! deadline or a connection lost after the request was sent may have been billed, so neither
//! is retried here: a retry could pay twice for one answer.
//!
//! How many retries follow the first attempt, and how long each waits, is the caller's
//! policy: the [`RetrySchedule`](crate::RetrySchedule) of the [`Provider`], whose
//! documentation states the contract. This module applies it: the provider's `Retry-After`
//! takes precedence over the schedule's wait, since the provider knows when its limit
//! resets, and a wait that would pass the deadline returns the error at once, with the
//! provider's wait, so the caller can hold back every call of the same kind instead of each
//! call waiting on its own.
//!
//! The HTTP client itself never retries: retries nested in two layers multiply the requests
//! a struggling provider receives. It follows no redirect, which could carry the key to
//! another host, and reads no proxy from the environment: the deployment's configuration is
//! read in one place, by the caller, and arrives here as a [`Provider`].

mod anthropic;
mod openai;

use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
use secrecy::ExposeSecret as _;
use serde::Serialize;
use tokio::time::Instant;
use tracing::Instrument as _;
use tracing::field::Empty;
use url::{Host, Url};

use crate::{
    AiError, Completion, Message, Outcome, Output, Provider, Request, RetrySchedule, Role, Usage,
    Wire,
};

/// The longest deadline a call may have: it bounds how long a caller waits on one call, and
/// providers answer the small structured requests this client makes well within it.
const MAX_TIMEOUT: Duration = Duration::from_secs(90);
/// How long establishing a connection may take before the provider counts as unreachable, so
/// that a dead host leaves time within the deadline to retry.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest answer read; a larger one is [`AiError::InvalidResponse`]. The answers this
/// client asks for are far smaller, and the bound keeps a misbehaving endpoint from filling
/// memory.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
/// The largest error body read, only to tell an account without credit from a rate limit.
const MAX_ERROR_BYTES: usize = 64 * 1024;
/// The longest call id OpenAI accepts in `X-Client-Request-Id`
/// (<https://developers.openai.com/api/reference/overview>).
const MAX_CALL_ID_BYTES: usize = 512;

/// A configured provider, ready to call. It holds one connection pool; cloning the client is
/// cheap and shares the pool, so one client per provider is built once and shared by every
/// caller.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: Url,
    wire: Wire,
    model: String,
    structured_output: bool,
    timeout: Duration,
    retries: RetrySchedule,
}

/// A success answer as it arrived: its body and the provider's request id header.
struct Reply {
    body: Vec<u8>,
    request_id: Option<String>,
}

/// What a wire format reads from a success answer, before the shared rules turn it into a
/// [`Completion`].
struct Answer {
    /// The answer's text: its text blocks or content, or the refusal's explanation.
    text: String,
    stop: Stop,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    model: Option<String>,
    id: Option<String>,
}

/// Why the model stopped, in this crate's terms; each wire format maps its own reasons here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(strum::EnumIter))]
enum Stop {
    Finished,
    Refused,
    Truncated,
    /// A reason no request of this crate leads to (a tool call, a pause), or none at all.
    Unexpected,
}

/// One message as both wire formats write it: `{ "role": …, "content": … }`.
#[derive(Serialize)]
struct Turn<'a> {
    role: &'static str,
    content: &'a str,
}

impl Client {
    /// Checks the configuration once and builds the connection pool.
    ///
    /// The checks: the base URL is HTTPS, or plain HTTP to `localhost` or to a loopback or
    /// private-network address (anywhere else the key would cross the internet in clear
    /// text); it carries no credentials, query or fragment; the timeout is above zero and at
    /// most 90 seconds; the key is a valid header value.
    ///
    /// # Errors
    ///
    /// [`AiError::Config`] when a check fails or the HTTP client cannot be built.
    pub fn new(provider: &Provider) -> Result<Self, AiError> {
        let base = &provider.base_url;
        let transport_ok =
            base.scheme() == "https" || (base.scheme() == "http" && local(base.host()));
        if !transport_ok
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(AiError::Config(
                "the base URL must be HTTPS (HTTP only to a loopback or private address), without credentials, query or fragment",
            ));
        }
        if provider.timeout.is_zero() || provider.timeout > MAX_TIMEOUT {
            return Err(AiError::Config(
                "the timeout must be above zero and at most 90 seconds",
            ));
        }
        let mut endpoint = base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| AiError::Config("the base URL cannot take a path"))?
            .pop_if_empty()
            .extend(match provider.wire {
                Wire::Anthropic => anthropic::PATH,
                Wire::OpenAiCompatible => openai::PATH,
            });
        let http = reqwest::Client::builder()
            .default_headers(headers(provider.wire, provider.api_key.expose_secret())?)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map_err(|_| AiError::Config("the HTTP client could not be built"))?;
        Ok(Self {
            http,
            endpoint,
            wire: provider.wire,
            model: provider.model.clone(),
            structured_output: provider.structured_output,
            timeout: provider.timeout,
            retries: provider.retries.clone(),
        })
    }

    /// Runs one call: the checks that need no network, then the request and the retries
    /// described in the module documentation, all within [`Provider::timeout`].
    ///
    /// Every answer the provider produced comes back as a [`Completion`], whatever its
    /// [`Outcome`], because each was billed.
    ///
    /// The call is one `tracing` span following the OpenTelemetry semantic conventions for
    /// generative AI client spans
    /// (<https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>): named
    /// `chat {model}`, with the provider, the requested and the answering model, the finish
    /// reason, the token usage and, on failure, `error.type`; plus the caller's
    /// [`Request::call_id`] and the provider's request id. Prompt and completion content are
    /// never recorded, since they may hold personal data.
    ///
    /// # Errors
    ///
    /// An [`AiError`] when no answer was read. Each variant states whether the call can have
    /// been billed, and [`AiError::is_retryable`] whether trying again later can help.
    pub async fn complete(&self, request: &Request) -> Result<Completion, AiError> {
        let span = tracing::info_span!(
            "gen_ai.chat",
            otel.name = %format_args!("chat {}", self.model),
            otel.kind = "client",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = match self.wire {
                Wire::Anthropic => "anthropic",
                Wire::OpenAiCompatible => "openai",
            },
            gen_ai.request.model = %self.model,
            gen_ai.request.max_tokens = request.max_tokens,
            server.address = self.endpoint.host_str(),
            norbelys.ai_call_id = %request.call_id,
            norbelys.ai.provider_request_id = Empty,
            gen_ai.response.model = Empty,
            gen_ai.response.finish_reasons = Empty,
            gen_ai.usage.input_tokens = Empty,
            gen_ai.usage.output_tokens = Empty,
            "error.type" = Empty,
        );
        let result = self.call(request).instrument(span.clone()).await;
        match &result {
            Ok(completion) => {
                span.record("gen_ai.response.model", completion.model.as_str());
                span.record(
                    "gen_ai.response.finish_reasons",
                    completion.finish_reason.as_deref(),
                );
                span.record(
                    "norbelys.ai.provider_request_id",
                    completion.provider_request_id.as_deref(),
                );
                if let Some(usage) = completion.usage {
                    span.record("gen_ai.usage.input_tokens", usage.input_tokens);
                    span.record("gen_ai.usage.output_tokens", usage.output_tokens);
                }
            }
            Err(error) => {
                span.record("error.type", tracing::field::display(error));
            }
        }
        result
    }

    /// [`Client::complete`] without its span: the checks, then the attempts within the
    /// deadline, then the reading of the answer.
    async fn call(&self, request: &Request) -> Result<Completion, AiError> {
        if request.schema.is_some() && !self.structured_output {
            return Err(AiError::Config(
                "a schema was asked of a provider and model that do not enforce one",
            ));
        }
        // A `HeaderValue` alone would let bytes above ASCII through (obsolete header text), so
        // the rule is checked on the bytes first.
        let call_id = Some(request.call_id.as_str())
            .filter(|id| {
                (1..=MAX_CALL_ID_BYTES).contains(&id.len())
                    && id.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .and_then(|id| HeaderValue::from_str(id).ok())
            .ok_or(AiError::Config(
                "the call id must be 1 to 512 visible ASCII characters, without spaces",
            ))?;
        let body = match self.wire {
            Wire::Anthropic => anthropic::body(&self.model, request),
            Wire::OpenAiCompatible => openai::body(&self.model, request),
        }
        .map_err(|_| AiError::Config("the request could not be encoded"))?;
        let body = Bytes::from(body);

        let deadline = Instant::now() + self.timeout;
        let mut retries = 0;
        let reply = loop {
            let error =
                match tokio::time::timeout_at(deadline, self.send(body.clone(), &call_id)).await {
                    Ok(Ok(reply)) => break reply,
                    Ok(Err(error)) => error,
                    Err(_elapsed) => return Err(AiError::Timeout),
                };
            let Some(wait) = retry_wait(&error, retries, &self.retries) else {
                return Err(error);
            };
            if wait >= deadline.saturating_duration_since(Instant::now()) {
                return Err(error);
            }
            tokio::time::sleep(wait).await;
            retries += 1;
        };

        let answer = match self.wire {
            Wire::Anthropic => anthropic::read(&reply.body),
            Wire::OpenAiCompatible => openai::read(&reply.body),
        }
        .ok_or(AiError::InvalidResponse)?;
        Ok(completion(
            answer,
            reply.request_id,
            request.schema.is_some(),
            &self.model,
        ))
    }

    /// One attempt: the answer's bytes, or the error its status means. `401` and `403` are
    /// [`AiError::Unauthorized`]; `429` is [`AiError::RateLimited`], or
    /// [`AiError::Rejected`] when its body says the account has no credit, which no wait
    /// cures; a `5xx` is [`AiError::Provider`]; any other status that is not a success is
    /// [`AiError::Rejected`]. A connection that could not be made is
    /// [`AiError::Unreachable`]; one that failed after the request was sent is
    /// [`AiError::Transport`].
    async fn send(&self, body: Bytes, call_id: &HeaderValue) -> Result<Reply, AiError> {
        let mut builder = self.http.post(self.endpoint.clone()).body(body);
        if self.wire == Wire::OpenAiCompatible {
            builder = builder.header(openai::CLIENT_REQUEST_ID, call_id.clone());
        }
        let response = builder.send().await.map_err(|error| {
            if error.is_connect() {
                AiError::Unreachable
            } else {
                AiError::Transport
            }
        })?;
        let status = response.status().as_u16();
        let retry_after = retry_after(response.headers(), jiff::Timestamp::now());
        let request_id = response
            .headers()
            .get(match self.wire {
                Wire::Anthropic => anthropic::REQUEST_ID,
                Wire::OpenAiCompatible => openai::REQUEST_ID,
            })
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match status {
            200..=299 => Ok(Reply {
                body: read(response, MAX_ANSWER_BYTES).await?,
                request_id,
            }),
            401 | 403 => Err(AiError::Unauthorized { status }),
            429 => Err(if out_of_credit(response).await {
                AiError::Rejected { status }
            } else {
                AiError::RateLimited { retry_after }
            }),
            500..=599 => Err(AiError::Provider {
                status,
                retry_after,
            }),
            _ => Err(AiError::Rejected { status }),
        }
    }
}

/// The [`Completion`] of `answer`. With a schema (`json`), a finished answer must parse as
/// JSON, else it is [`Outcome::InvalidOutput`] with the parser's message as the violation.
/// The provider's request id header is preferred to the answer's own id, and the configured
/// model stands in when the answer names none.
fn completion(
    answer: Answer,
    request_id: Option<String>,
    json: bool,
    configured: &str,
) -> Completion {
    let Answer {
        text,
        stop,
        finish_reason,
        usage,
        model,
        id,
    } = answer;
    let outcome = match stop {
        Stop::Finished if json => match serde_json::from_str(&text) {
            Ok(value) => Outcome::Completed(Output::Json(value)),
            Err(error) => Outcome::InvalidOutput {
                violation: format!("the answer is not valid JSON: {error}"),
                text,
            },
        },
        Stop::Finished => Outcome::Completed(Output::Text(text)),
        Stop::Refused => Outcome::Refused { text },
        Stop::Truncated => Outcome::Truncated { text },
        Stop::Unexpected => Outcome::InvalidOutput {
            violation: format!(
                "the answer stopped for an unexpected reason: {}",
                finish_reason.as_deref().unwrap_or("none")
            ),
            text,
        },
    };
    Completion {
        outcome,
        usage,
        model: model.unwrap_or_else(|| configured.to_owned()),
        finish_reason,
        provider_request_id: request_id.or(id),
    }
}

/// The default headers of `wire`: the content type, Anthropic's API version, and the key,
/// marked sensitive so that no `Debug` output of the HTTP client prints it.
fn headers(wire: Wire, key: &str) -> Result<HeaderMap, AiError> {
    let invalid = |_| AiError::Config("the API key is not a valid header value");
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let (name, mut value) = match wire {
        Wire::Anthropic => {
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(anthropic::VERSION),
            );
            (
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(key).map_err(invalid)?,
            )
        }
        Wire::OpenAiCompatible => (
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {key}")).map_err(invalid)?,
        ),
    };
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(headers)
}

/// The conversation as both wire formats write it, oldest first.
fn turns(messages: &[Message]) -> impl Iterator<Item = Turn<'_>> {
    messages.iter().map(|message| Turn {
        role: match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        content: &message.content,
    })
}

/// Whether `host` is this machine or a private network, where plain HTTP may carry the key:
/// `localhost`, a loopback address, an IPv4 private (RFC 1918) or link-local address, or an
/// IPv6 unique-local or link-local address. Other names do not count, even if they resolve to
/// such an address, because a name can resolve anywhere.
fn local(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Some(Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
        None => false,
    }
}

/// The body of `response`, at most `limit` bytes: a longer one is
/// [`AiError::InvalidResponse`], and a connection that fails while reading is
/// [`AiError::Transport`].
async fn read(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, AiError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| AiError::Transport)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(AiError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Whether a `429` means an account without credit rather than a rate limit: OpenAI answers
/// `insufficient_quota` in `error.type` or `error.code`, and no wait cures it. A body that
/// cannot be read or parsed counts as an ordinary rate limit.
async fn out_of_credit(response: reqwest::Response) -> bool {
    let Ok(body) = read(response, MAX_ERROR_BYTES).await else {
        return false;
    };
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    ["/error/type", "/error/code"].iter().any(|pointer| {
        body.pointer(pointer).and_then(serde_json::Value::as_str) == Some("insufficient_quota")
    })
}

/// The provider's `Retry-After` at `now`, as delay-seconds or as an HTTP date (RFC 9110, section
/// 10.2.3: <https://www.rfc-editor.org/rfc/rfc9110#section-10.2.3>). A zero, past or
/// unreadable value counts as absent, so the caller's schedule decides the wait instead and
/// a meaningless value never causes an immediate retry.
fn retry_after(headers: &HeaderMap, now: jiff::Timestamp) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        Err(_) => {
            let at = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
            Duration::try_from(at.duration_since(now)).ok()?
        }
    };
    (!wait.is_zero()).then_some(wait)
}

/// The wait before the next attempt after `error`, or `None` when `error` is not retried
/// (it may have been billed, or the request must change first) or `schedule` allows no more
/// retries. `retries` counts the retries already made; the provider's own wait takes
/// precedence over the schedule's, and the schedule is consulted only when there is none.
fn retry_wait(error: &AiError, retries: u32, schedule: &RetrySchedule) -> Option<Duration> {
    if retries >= schedule.max_retries {
        return None;
    }
    match error {
        AiError::RateLimited { retry_after } | AiError::Provider { retry_after, .. } => {
            Some(retry_after.unwrap_or_else(|| (schedule.wait)(retries)))
        }
        AiError::Unreachable => Some((schedule.wait)(retries)),
        AiError::Config(_)
        | AiError::Transport
        | AiError::Timeout
        | AiError::Unauthorized { .. }
        | AiError::Rejected { .. }
        | AiError::InvalidResponse => None,
    }
}

#[cfg(test)]
mod tests;
