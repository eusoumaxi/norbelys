//! Protocol and parser tests for the openai wire adapter.

use serde_json::{Value, json};
use strum::IntoEnumIterator as _;

use super::{body, read};
use crate::client::Stop;
use crate::client::tests::{request, schema};
use crate::{Request, Usage};

/// The body follows chat completions: the system prompt as the first message with the
/// `system` role, then the turns; `max_completion_tokens`, since OpenAI's reasoning models
/// refuse `max_tokens`; `temperature`; and the schema under `response_format`, named and
/// with `strict: true`. Unset options and an empty system prompt are left out, so the
/// model's defaults apply.
#[test]
fn body_follows_chat_completions() {
    let full: Value = serde_json::from_slice(&body("model-x", &request()).unwrap()).unwrap();
    let turns = [
        json!({"role": "user", "content": "Out of office until Monday."}),
        json!({"role": "assistant", "content": "{}"}),
        json!({"role": "user", "content": "The answer lacks `classification`."}),
    ];
    let system = json!({"role": "system", "content": "Classify the reply."});
    assert_eq!(
        full,
        json!({
            "model": "model-x",
            "messages": [system, turns[0], turns[1], turns[2]],
            "max_completion_tokens": 256,
            "temperature": 0.5,
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "answer", "strict": true, "schema": schema()},
            },
        })
    );
    let bare = Request {
        system: String::new(),
        temperature: None,
        schema: None,
        ..request()
    };
    let bare: Value = serde_json::from_slice(&body("model-x", &bare).unwrap()).unwrap();
    assert_eq!(
        bare,
        json!({"model": "model-x", "messages": turns, "max_completion_tokens": 256})
    );
}

/// Every outcome has the finish reasons that lead to it: `stop` finishes the answer (an
/// empty `refusal` field does not change that), a `message.refusal` or a
/// `content_filter` finish is a decline, `length` cuts the answer, and what a request
/// without tools cannot lead to (`tool_calls`, no reason, no choice at all) is unexpected.
/// A new stop variant fails here until its reasons are listed.
#[test]
fn read_maps_every_finish_reason() {
    let choice = |finish_reason: Option<&str>, refusal: Option<&str>| {
        json!({"choices": [{
            "message": {"content": "text", "refusal": refusal},
            "finish_reason": finish_reason,
        }]})
    };
    for stop in Stop::iter() {
        let answers = match stop {
            Stop::Finished => vec![choice(Some("stop"), None), choice(Some("stop"), Some(""))],
            Stop::Refused => vec![
                choice(Some("stop"), Some("I can't help with that.")),
                choice(Some("content_filter"), None),
            ],
            Stop::Truncated => vec![choice(Some("length"), None)],
            Stop::Unexpected => vec![
                choice(Some("tool_calls"), None),
                choice(None, None),
                json!({"choices": []}),
            ],
        };
        for answer in answers {
            let parsed = read(answer.to_string().as_bytes()).unwrap();
            assert_eq!(parsed.stop, stop, "{answer}");
        }
    }
}

/// A refusal's explanation stands in for the absent content; the usage maps
/// `prompt_tokens` and `completion_tokens`; an answer without usage figures has none
/// rather than an estimate; and the model and the completion id are kept.
#[test]
fn read_keeps_the_refusal_text_and_the_usage() {
    let refused = json!({
        "id": "chatcmpl-1",
        "model": "model-x-snapshot",
        "choices": [{
            "message": {"content": null, "refusal": "I can't help with that."},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 30, "completion_tokens": 8, "total_tokens": 38},
    })
    .to_string();
    let refused = read(refused.as_bytes()).unwrap();
    assert_eq!(refused.text, "I can't help with that.");
    assert_eq!(
        refused.usage,
        Some(Usage {
            input_tokens: 30,
            output_tokens: 8
        })
    );
    assert_eq!(refused.model.as_deref(), Some("model-x-snapshot"));
    assert_eq!(refused.id.as_deref(), Some("chatcmpl-1"));
    let unmetered =
        json!({"choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]})
            .to_string();
    let unmetered = read(unmetered.as_bytes()).unwrap();
    assert_eq!(unmetered.text, "hello");
    assert_eq!(unmetered.usage, None);
}
