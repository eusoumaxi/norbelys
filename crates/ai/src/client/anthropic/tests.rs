//! Protocol and parser tests for the anthropic wire adapter.

use serde_json::{Value, json};
use strum::IntoEnumIterator as _;

use super::{body, read};
use crate::client::Stop;
use crate::client::tests::{request, schema};
use crate::{Request, Usage};

/// The body follows the Messages API: the system prompt as a top-level field, the turns
/// with their roles, `max_tokens`, `temperature`, and the schema under
/// `output_config.format` with `type: "json_schema"`. Unset options and an empty system
/// prompt are left out, so the model's defaults apply; current Claude models refuse an
/// explicit temperature.
#[test]
fn body_follows_the_messages_api() {
    let full: Value = serde_json::from_slice(&body("model-x", &request()).unwrap()).unwrap();
    let turns = json!([
        {"role": "user", "content": "Out of office until Monday."},
        {"role": "assistant", "content": "{}"},
        {"role": "user", "content": "The answer lacks `classification`."},
    ]);
    assert_eq!(
        full,
        json!({
            "model": "model-x",
            "max_tokens": 256,
            "system": "Classify the reply.",
            "messages": turns,
            "temperature": 0.5,
            "output_config": {"format": {"type": "json_schema", "schema": schema()}},
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
        json!({"model": "model-x", "max_tokens": 256, "messages": turns})
    );
}

/// Every outcome has the stop reasons that lead to it: `end_turn` and `stop_sequence`
/// finish the answer, `refusal` is a decline (sent with a success status), `max_tokens`
/// and `model_context_window_exceeded` cut it, and what a request without tools cannot
/// lead to (`tool_use`, `pause_turn`, no reason at all) is unexpected. The raw reason is
/// kept for logs. A new stop variant fails here until its reasons are listed.
#[test]
fn read_maps_every_stop_reason() {
    for stop in Stop::iter() {
        let reasons: &[Option<&str>] = match stop {
            Stop::Finished => &[Some("end_turn"), Some("stop_sequence")],
            Stop::Refused => &[Some("refusal")],
            Stop::Truncated => &[Some("max_tokens"), Some("model_context_window_exceeded")],
            Stop::Unexpected => &[Some("tool_use"), Some("pause_turn"), None],
        };
        for &reason in reasons {
            let body = json!({"content": [], "stop_reason": reason}).to_string();
            let answer = read(body.as_bytes()).unwrap();
            assert_eq!(answer.stop, stop, "{reason:?}");
            assert_eq!(answer.finish_reason.as_deref(), reason);
        }
    }
}

/// The answer is the `text` blocks joined in order, without the thinking blocks current
/// models add before them; the usage adds the prompt-cache reads and writes, which the API
/// reports apart, to the input tokens; and the model and the message id are kept.
#[test]
fn read_joins_text_blocks_and_counts_cache_input() {
    let body = json!({
        "id": "msg_1",
        "model": "model-x-2026",
        "content": [
            {"type": "thinking", "thinking": "", "signature": "c2ln"},
            {"type": "text", "text": "Hello, "},
            {"type": "text", "text": "world"},
        ],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 5,
            "cache_read_input_tokens": 7,
        },
    })
    .to_string();
    let answer = read(body.as_bytes()).unwrap();
    assert_eq!(answer.text, "Hello, world");
    assert_eq!(
        answer.usage,
        Some(Usage {
            input_tokens: 112,
            output_tokens: 20
        })
    );
    assert_eq!(answer.model.as_deref(), Some("model-x-2026"));
    assert_eq!(answer.id.as_deref(), Some("msg_1"));
}
