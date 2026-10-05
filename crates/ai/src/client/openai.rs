//! OpenAI-compatible chat completions, `POST /chat/completions`: the request body and the
//! reading of an answer
//! (<https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create>,
//! as documented on 2026-10-01).
//!
//! - The system prompt is the first message, with the `system` role, which OpenAI maps to
//!   the `developer` role of its reasoning models and which self-hosted servers understand.
//! - Structured output is `response_format` with a `json_schema` and `strict: true`. A model
//!   that declines answers with `message.refusal` instead of content; a `content_filter`
//!   finish (a provider's content filter stopped the answer) is a refusal too.
//! - The output bound is `max_completion_tokens`, because OpenAI's reasoning models refuse
//!   the older `max_tokens`. Some self-hosted servers read only `max_tokens` (Ollama, as of
//!   2026-10-01: <https://github.com/ollama/ollama/blob/main/docs/api/openai-compatibility.mdx>)
//!   and so do not apply the bound; the call's deadline bounds the answer instead. Whether
//!   such a server enforces the schema is what the capability matrix ([`crate::matrix`]) says,
//!   passed to the client as [`structured_output`](crate::Provider::structured_output).
//! - `finish_reason` decides the outcome: `stop` means the answer is finished, `length` that
//!   it was cut; anything else (`tool_calls`, or none at all) cannot follow a request without
//!   tools and makes the answer invalid. Only the first choice is read: the request never
//!   asks for more.
//! - `usage.prompt_tokens` already counts cached input, and `completion_tokens` counts
//!   reasoning tokens.

use serde::{Deserialize, Serialize};

use super::{Answer, Stop, Turn, turns};
use crate::{Request, Usage};

/// The path under the base URL. OpenAI's SDKs take the base URL with the version
/// (`https://api.openai.com/v1`), as self-hosted servers document theirs, so only these two
/// segments are appended.
pub(super) const PATH: [&str; 2] = ["chat", "completions"];
/// The response header with the provider's id for the request, which its support asks for.
pub(super) const REQUEST_ID: &str = "x-request-id";
/// The request header carrying the caller's call id. OpenAI keeps it so its support can find
/// a call whose answer never arrived
/// (<https://developers.openai.com/api/reference/overview>); other servers ignore it.
pub(super) const CLIENT_REQUEST_ID: &str = "x-client-request-id";
/// The name the schema is given, which `json_schema` requires; one name serves every request.
const SCHEMA_NAME: &str = "answer";

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    messages: Vec<Turn<'a>>,
    max_completion_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat<'a>>,
}

#[derive(Serialize)]
struct ResponseFormat<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchema<'a>,
}

#[derive(Serialize)]
struct JsonSchema<'a> {
    name: &'static str,
    strict: bool,
    schema: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct Reply {
    id: Option<String>,
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<ReplyUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<ReplyMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ReplyMessage {
    content: Option<String>,
    refusal: Option<String>,
}

#[derive(Deserialize)]
struct ReplyUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

/// The JSON body of `request` for `model`: the system prompt first, when not empty, then the
/// conversation; `temperature` and `response_format` are left out when unset, so the model's
/// defaults apply.
pub(super) fn body(model: &str, request: &Request) -> serde_json::Result<Vec<u8>> {
    let system = (!request.system.is_empty()).then_some(Turn {
        role: "system",
        content: &request.system,
    });
    serde_json::to_vec(&Body {
        model,
        messages: system.into_iter().chain(turns(&request.messages)).collect(),
        max_completion_tokens: request.max_tokens,
        temperature: request.temperature,
        response_format: request.schema.as_ref().map(|schema| ResponseFormat {
            kind: "json_schema",
            json_schema: JsonSchema {
                name: SCHEMA_NAME,
                strict: true,
                schema,
            },
        }),
    })
}

/// Reads a success body; `None` when it is not a chat completion. A refusal's explanation
/// stands in for the content.
pub(super) fn read(body: &[u8]) -> Option<Answer> {
    let reply: Reply = serde_json::from_slice(body).ok()?;
    let (message, finish_reason) = reply
        .choices
        .into_iter()
        .next()
        .map_or((None, None), |choice| {
            (choice.message, choice.finish_reason)
        });
    let (content, refusal) =
        message.map_or((None, None), |message| (message.content, message.refusal));
    let refusal = refusal.filter(|text| !text.is_empty());
    let stop = match (&refusal, finish_reason.as_deref()) {
        (Some(_), _) | (None, Some("content_filter")) => Stop::Refused,
        (None, Some("stop")) => Stop::Finished,
        (None, Some("length")) => Stop::Truncated,
        (None, _) => Stop::Unexpected,
    };
    Some(Answer {
        text: refusal.or(content).unwrap_or_default(),
        stop,
        finish_reason,
        usage: reply.usage.map(|usage| Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        }),
        model: reply.model,
        id: reply.id,
    })
}

#[cfg(test)]
mod tests;
