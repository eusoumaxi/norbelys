//! The Anthropic Messages API, `POST /v1/messages` with `anthropic-version: 2023-06-01`: the
//! request body and the reading of an answer
//! (<https://platform.claude.com/docs/en/api/messages>, as documented on 2026-10-01).
//!
//! - Structured output is `output_config.format` with `type: "json_schema"`, generally
//!   available without a beta header
//!   (<https://platform.claude.com/docs/en/build-with-claude/structured-outputs>).
//! - Current models think by default. Thinking comes back in blocks of its own and counts in
//!   `output_tokens`; only the `text` blocks are the answer, joined in order.
//! - `stop_reason` decides the outcome: `end_turn` and `stop_sequence` mean the answer is
//!   finished; `refusal` is a decline by the model's safety classifiers, delivered with a
//!   success status and billed like any answer; `max_tokens` and
//!   `model_context_window_exceeded` mean it was cut; anything else (`tool_use`,
//!   `pause_turn`) cannot follow a request without tools and makes the answer invalid.
//! - Usage reports prompt-cache reads and writes apart from `input_tokens`; they are added to
//!   the input count.
//! - Errors and their statuses: <https://platform.claude.com/docs/en/api/errors>.

use serde::{Deserialize, Serialize};

use super::{Answer, Stop, Turn, turns};
use crate::{Request, Usage};

/// The path under the base URL. Anthropic's SDKs take the base URL without the version
/// (`https://api.anthropic.com`), so both segments are appended.
pub(super) const PATH: [&str; 2] = ["v1", "messages"];
/// The API version every request names in `anthropic-version`; a request without one is
/// refused.
pub(super) const VERSION: &str = "2023-06-01";
/// The response header with Anthropic's id for the request, which its support asks for.
pub(super) const REQUEST_ID: &str = "request-id";

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    messages: Vec<Turn<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig<'a>>,
}

#[derive(Serialize)]
struct OutputConfig<'a> {
    format: Format<'a>,
}

#[derive(Serialize)]
struct Format<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    schema: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct Reply {
    id: Option<String>,
    model: Option<String>,
    #[serde(default)]
    content: Vec<Block>,
    stop_reason: Option<String>,
    usage: Option<ReplyUsage>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ReplyUsage {
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
}

/// The JSON body of `request` for `model`. The system prompt is a top-level field, left out
/// when empty; `temperature` and `output_config` are left out when unset, so the model's
/// defaults apply.
pub(super) fn body(model: &str, request: &Request) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&Body {
        model,
        max_tokens: request.max_tokens,
        system: &request.system,
        messages: turns(&request.messages).collect(),
        temperature: request.temperature,
        output_config: request.schema.as_ref().map(|schema| OutputConfig {
            format: Format {
                kind: "json_schema",
                schema,
            },
        }),
    })
}

/// Reads a success body; `None` when it is not a Messages answer.
pub(super) fn read(body: &[u8]) -> Option<Answer> {
    let reply: Reply = serde_json::from_slice(body).ok()?;
    let stop = match reply.stop_reason.as_deref() {
        Some("end_turn" | "stop_sequence") => Stop::Finished,
        Some("refusal") => Stop::Refused,
        Some("max_tokens" | "model_context_window_exceeded") => Stop::Truncated,
        _ => Stop::Unexpected,
    };
    Some(Answer {
        text: reply
            .content
            .into_iter()
            .filter(|block| block.kind == "text")
            .filter_map(|block| block.text)
            .collect(),
        stop,
        finish_reason: reply.stop_reason,
        usage: reply.usage.map(|usage| Usage {
            input_tokens: usage
                .input_tokens
                .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(usage.cache_read_input_tokens.unwrap_or(0)),
            output_tokens: usage.output_tokens,
        }),
        model: reply.model,
        id: reply.id,
    })
}

#[cfg(test)]
mod tests;
