//! The capability matrix: which combinations of a wire format and a model enforce a JSON schema
//! strictly.
//!
//! Every use of a model in Norbelys answers under a JSON schema. A provider that accepts a schema
//! without enforcing it bills an unconstrained answer that then fails to parse, and support
//! varies by wire format, by provider and by model: Anthropic's OpenAI-compatible endpoint ignores
//! `response_format` and `strict`
//! (<https://platform.claude.com/docs/en/api/openai-sdk>), while its Messages API enforces a
//! schema through `output_config.format`; a self-hosted server may accept `response_format` and
//! ignore it, or enforce it for some models only. So support is not something a deployment
//! declares: this table states each combination that was tested, and a combination it does not
//! list is untested, which a caller treats as unsupported and refuses before any call is made.
//!
//! # What a row means
//!
//! - **Strict**: the provider documents schema enforcement for this model through this wire
//!   (read on 2026-10-01), and the evaluation of the platform's prompts confirms it whenever it
//!   runs against the model: it sends every golden case under its use case's schema and fails on
//!   any answer that violates it.
//! - **Not strict**: a documented absence of enforcement, kept so a refusal can say the
//!   combination is known not to work rather than merely untested.
//!
//! The rows are checked by the tests below: one claim per combination, model ids written as
//! providers write them.
//!
//! A model id is matched exactly. A dated snapshot (`claude-haiku-4-5-20251001`) is another row,
//! since a snapshot is another model. Self-hosted models (Ollama, vLLM and others, reached through
//! the OpenAI-compatible wire) are refused until a row says what they support: to add one, run
//! the evaluation against it and record the result here with the date it was read.

use crate::Wire;

/// One combination of a wire format and a model, and whether it enforces a JSON schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// The wire format the model is reached through.
    pub wire: Wire,
    /// The model id, exactly as the provider names it in requests.
    pub model: &'static str,
    /// Whether the provider enforces a JSON schema strictly for this model through this wire.
    pub strict: bool,
}

const fn row(wire: Wire, model: &'static str, strict: bool) -> Capability {
    Capability {
        wire,
        model,
        strict,
    }
}

/// The tested combinations, read on 2026-10-01 from the providers' structured output pages.
pub const MATRIX: &[Capability] = &[
    // Anthropic's Messages API: structured outputs through `output_config.format`.
    row(Wire::Anthropic, "claude-haiku-4-5", true),
    row(Wire::Anthropic, "claude-sonnet-5-5", true),
    row(Wire::Anthropic, "claude-opus-5-5", true),
    // OpenAI's chat completions: `response_format` with `strict: true`.
    row(Wire::OpenAiCompatible, "gpt-6-luna", true),
    row(Wire::OpenAiCompatible, "gpt-6.1-sol", true),
    row(Wire::OpenAiCompatible, "gpt-6-astra", true),
    // Anthropic's OpenAI-compatible endpoint ignores `response_format` and `strict`: a Claude
    // model reached through chat completions answers unconstrained.
    row(Wire::OpenAiCompatible, "claude-haiku-4-5", false),
    row(Wire::OpenAiCompatible, "claude-sonnet-5-5", false),
    row(Wire::OpenAiCompatible, "claude-opus-5-5", false),
];

/// What the matrix says of `model` reached through `wire`: `Some(true)` when the provider
/// enforces a JSON schema for it, `Some(false)` when it is known not to, and `None` when the
/// combination was never tested, which a caller treats as unsupported.
#[must_use]
pub fn strict(wire: Wire, model: &str) -> Option<bool> {
    MATRIX
        .iter()
        .find(|capability| capability.wire == wire && capability.model == model)
        .map(|capability| capability.strict)
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{MATRIX, strict};
    use crate::Wire;

    /// Each combination is listed once, with one claim: two rows for the same wire and model
    /// could say both things, and the first would silently win.
    #[test]
    fn every_combination_is_listed_once() {
        for (index, capability) in MATRIX.iter().enumerate() {
            assert!(
                !MATRIX.iter().skip(index + 1).any(|other| {
                    other.wire == capability.wire && other.model == capability.model
                }),
                "{capability:?} is listed twice"
            );
        }
    }

    /// Model ids are written as providers write them in requests: 1 to 128 visible ASCII
    /// characters, no space and no case folding, so a lookup by the configured id is exact.
    #[test]
    fn model_ids_are_written_as_providers_write_them() {
        for capability in MATRIX {
            let id = capability.model;
            assert!(
                (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_graphic()),
                "{id:?}"
            );
            assert_eq!(id, id.to_ascii_lowercase(), "{id:?}");
        }
    }

    /// The answer for every wire: each provider's own models are strict on their own wire; a
    /// Claude model through the OpenAI-compatible wire is known not to be (Anthropic's
    /// compatibility layer ignores the schema); an OpenAI model through the Anthropic wire, a
    /// self-hosted model, an unknown or differently written id are untested, hence unsupported.
    #[test]
    fn the_matrix_answers_for_every_wire() {
        for wire in Wire::iter() {
            for claude in ["claude-haiku-4-5", "claude-sonnet-5-5", "claude-opus-5-5"] {
                let expected = match wire {
                    Wire::Anthropic => Some(true),
                    Wire::OpenAiCompatible => Some(false),
                };
                assert_eq!(strict(wire, claude), expected, "{wire:?} {claude}");
            }
            for openai in ["gpt-6-luna", "gpt-6.1-sol", "gpt-6-astra"] {
                let expected = match wire {
                    Wire::Anthropic => None,
                    Wire::OpenAiCompatible => Some(true),
                };
                assert_eq!(strict(wire, openai), expected, "{wire:?} {openai}");
            }
            for untested in [
                "llama3.1:8b",
                "qwen3",
                "Claude-Haiku-4-5",
                "claude-haiku-4-5-20251001",
                "",
            ] {
                assert_eq!(strict(wire, untested), None, "{wire:?} {untested}");
            }
        }
    }
}
