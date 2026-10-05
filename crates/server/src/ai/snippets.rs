//! Personalisation snippets: short texts a campaign step's template inserts, written for one
//! person by a model from the step's instructions and the person's usable fields.
//!
//! They are written before the message exists: the step's `message.generate` job asks for them,
//! then creates the message with the snippets in its render context, so a message's row never
//! changes after it is inserted and nothing on the delivery path waits for AI. Anything short of
//! valid snippets in time (the workspace turned them off, no provider, no budget, a refusal, an
//! invalid answer, the deadline) gives [`Snippets::Defaults`], and the template's defaults are
//! used: a message is never held for AI.
//!
//! What is sent: the step's instructions and the names of the snippets, and of the person only
//! the fields the workspace marked usable (`settings.ai.usable_fields`); never an address. A
//! snippet that invents a link, an address or a phone number is refused, since it would go out
//! in the customer's name; one longer than 500 characters too.
//!
//! # For the `message.generate` kind
//!
//! The kind (queue `ai`, effect `ExternalRetryable`, recovery hook
//! [`settle_abandoned`](super::store::settle_abandoned)) calls [`generate`] with the step's
//! instructions, the snippet names its template uses, the person's fields and the instant the
//! message must exist by, and renders [`Snippets::Generated`] or the template's defaults.

use std::collections::BTreeMap;

use norbelys_ai::{Message, Role};
use serde_json::{Map, Value, json};

use super::classify::only;
use super::prompts::current;
use super::{Ai, Answer, Call, CallError, store};
use crate::domain::ai::UseCase;
use crate::domain::ai::redact::holds_contact;
use crate::domain::time::Timestamp;
use crate::jobs::{JobContext, JobError};

/// The most snippets one call writes.
pub const NAMES_MAX: usize = 10;
/// The most characters of the step's instructions.
pub const INSTRUCTIONS_MAX: usize = 4_000;
/// The most characters of one snippet.
pub const SNIPPET_MAX: usize = 500;
/// The most characters of one field's value sent.
const FIELD_CHARS: usize = 300;
/// The most output tokens: room for the snippets and for the thinking current models do first.
pub const MAX_TOKENS: u32 = 2_048;
/// Less time than this before the deadline, and the call is not worth starting.
const DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_secs(5);

/// What the step asks for one person.
#[derive(Clone, Copy)]
pub struct SnippetsRequest<'a> {
    /// The step's instructions to the model, as its author wrote them.
    pub instructions: &'a str,
    /// The snippets the template uses: lowercase names such as `opener` or `ps`.
    pub names: &'a [String],
    /// The person's attributes and custom fields by key (`given_name`, `company`, a custom
    /// key); only those the workspace marked usable are sent, and only text, numbers and
    /// booleans.
    pub fields: &'a Map<String, Value>,
    /// When the message must exist; the call ends by then, and is not started too close to it.
    pub deadline: Option<Timestamp>,
}

/// The snippets of one person, or the reason to use the template's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snippets {
    /// Every requested snippet, by name.
    Generated(BTreeMap<String, String>),
    /// Use the template's defaults, for this reason.
    Defaults(Fallback),
}

/// Why the template's defaults are used (`messages.snippets_fallback`; a message shows it as
/// `snippets_fallback`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = SnippetsFallback, rename_all = "snake_case")]
pub enum Fallback {
    /// The workspace turned snippets off.
    Off,
    /// The deployment has no provider for snippets.
    Unavailable,
    /// The request cannot be served: no or too many names, a malformed name, no instructions or
    /// too long ones.
    Unusable,
    /// The month's AI budget does not admit the call.
    OverBudget,
    /// Snippets are paused after a provider's rate limit.
    Paused,
    /// The deadline passed, or was too close to start.
    Deadline,
    /// The model declined.
    Refused,
    /// The answer was cut short.
    Truncated,
    /// The answer failed its checks twice.
    Invalid,
    /// The provider gave no answer.
    Provider,
}

impl Fallback {
    /// The reason as a short code, for the workspace and for telemetry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Writes the snippets `request` asks for, for the job `cx` runs, as the module describes.
///
/// # Errors
///
/// [`JobError`] when the job's lease was lost or the database failed; every other failure is
/// [`Snippets::Defaults`].
pub async fn generate(
    cx: &mut JobContext,
    request: &SnippetsRequest<'_>,
) -> Result<Snippets, JobError> {
    let settings = {
        let mut tx = cx.db().begin_in(cx.workspace()).await?;
        let settings = store::read_settings(&mut tx, cx.workspace()).await?;
        tx.commit().await?;
        settings
    };
    if !settings.generate_snippets {
        return Ok(Snippets::Defaults(Fallback::Off));
    }
    if !usable(request) {
        return Ok(Snippets::Defaults(Fallback::Unusable));
    }
    let deadline = match request.deadline {
        None => None,
        Some(at) => {
            let left = at.0.duration_since(crate::process::now().0);
            match std::time::Duration::try_from(left) {
                Ok(left) if left > DEADLINE_MARGIN => Some(tokio::time::Instant::now() + left),
                _ => return Ok(Snippets::Defaults(Fallback::Deadline)),
            }
        }
    };
    let Some(ai) = cx.env::<Ai>().ok().cloned() else {
        return Ok(Snippets::Defaults(Fallback::Unavailable));
    };
    let turns = messages(request, &settings.usable_fields);
    let names = request.names;
    let ask = |messages: Vec<Message>| Call {
        use_case: UseCase::Snippets,
        prompt: current(UseCase::Snippets),
        canary: false,
        messages,
        schema: schema(names),
        max_tokens: MAX_TOKENS,
        deadline,
    };
    let answer = match super::call(cx, &ai, ask(turns.clone()), |value| check(value, names)).await {
        Ok(Answer::Invalid { text, violation }) => {
            let mut corrected = turns;
            corrected.push(Message {
                role: Role::Assistant,
                content: text,
            });
            corrected.push(Message {
                role: Role::User,
                content: format!(
                    "Your answer was not valid: {violation}. Answer again with one JSON object that follows the schema exactly."
                ),
            });
            super::call(cx, &ai, ask(corrected), |value| check(value, names)).await
        }
        first => first,
    };
    Ok(match answer {
        Ok(Answer::Valid(snippets, _)) => Snippets::Generated(snippets),
        Ok(Answer::Refused) => Snippets::Defaults(Fallback::Refused),
        Ok(Answer::Truncated) => Snippets::Defaults(Fallback::Truncated),
        Ok(Answer::Invalid { .. }) => Snippets::Defaults(Fallback::Invalid),
        Err(CallError::Job(error)) => return Err(error),
        Err(CallError::Unavailable) => Snippets::Defaults(Fallback::Unavailable),
        Err(CallError::Paused(_)) => Snippets::Defaults(Fallback::Paused),
        Err(CallError::OverBudget) => Snippets::Defaults(Fallback::OverBudget),
        Err(CallError::Provider(norbelys_ai::AiError::Timeout)) => {
            Snippets::Defaults(Fallback::Deadline)
        }
        Err(CallError::Provider(_)) => Snippets::Defaults(Fallback::Provider),
    })
}

/// Whether a request can be served: one to [`NAMES_MAX`] distinct names, each a lowercase
/// letter followed by up to 63 lowercase letters, digits or underscores, and instructions that
/// are neither empty nor longer than [`INSTRUCTIONS_MAX`] characters.
#[must_use]
pub fn usable(request: &SnippetsRequest<'_>) -> bool {
    let named = |name: &String| {
        let mut bytes = name.bytes();
        bytes.next().is_some_and(|first| first.is_ascii_lowercase())
            && name.len() <= 64
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    let distinct = request
        .names
        .iter()
        .enumerate()
        .all(|(index, name)| !request.names.iter().take(index).any(|other| other == name));
    let instructions = request.instructions.trim();
    (1..=NAMES_MAX).contains(&request.names.len())
        && request.names.iter().all(named)
        && distinct
        && !instructions.is_empty()
        && instructions.chars().count() <= INSTRUCTIONS_MAX
}

/// The input turn: the instructions between markers, the names, and the usable facts, one per
/// line, each value on one line and bounded.
#[must_use]
pub fn messages(request: &SnippetsRequest<'_>, usable_fields: &[String]) -> Vec<Message> {
    let facts: Vec<String> = usable_fields
        .iter()
        .filter_map(|key| {
            let value = match request.fields.get(key)? {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                Value::Null | Value::Array(_) | Value::Object(_) => return None,
            };
            let value: String = value
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(FIELD_CHARS)
                .collect();
            let value = value.trim();
            (!value.is_empty()).then(|| format!("- {key}: {value}"))
        })
        .collect();
    let content = format!(
        "Instructions from the template's author:\n<<<\n{}\n>>>\n\nSnippets to write: {}\n\nFacts about the recipient:\n{}",
        request.instructions.trim(),
        request.names.join(", "),
        if facts.is_empty() {
            "(none)".to_owned()
        } else {
            facts.join("\n")
        },
    );
    vec![Message {
        role: Role::User,
        content,
    }]
}

/// The JSON schema of the answer: an object `snippets` with one required text per name, in the
/// strict form both wire formats enforce.
#[must_use]
pub fn schema(names: &[String]) -> Value {
    let properties: Map<String, Value> = names
        .iter()
        .map(|name| (name.clone(), json!({"type": "string"})))
        .collect();
    json!({
        "type": "object",
        "properties": {
            "snippets": {
                "type": "object",
                "properties": properties,
                "required": names,
                "additionalProperties": false,
            },
        },
        "required": ["snippets"],
        "additionalProperties": false,
    })
}

/// Checks an answer: exactly the requested names, each a non-empty text of at most
/// [`SNIPPET_MAX`] characters with no link, address or phone number. Violations name the
/// snippet, never its text.
///
/// # Errors
///
/// What is wrong, to show the model.
pub fn check(answer: &Value, names: &[String]) -> Result<BTreeMap<String, String>, String> {
    let fields = answer
        .as_object()
        .ok_or_else(|| "the answer is not a JSON object".to_owned())?;
    only(fields, &["snippets"])?;
    let snippets = fields
        .get("snippets")
        .and_then(Value::as_object)
        .ok_or_else(|| "`snippets` must be an object of texts".to_owned())?;
    if snippets.len() != names.len() {
        return Err(format!(
            "`snippets` must hold exactly these snippets: {}",
            names.join(", ")
        ));
    }
    let mut written = BTreeMap::new();
    for name in names {
        let text = snippets
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or_else(|| format!("`snippets.{name}` must be a text"))?;
        if text.is_empty() {
            return Err(format!("`snippets.{name}` is empty"));
        }
        if text.chars().count() > SNIPPET_MAX {
            return Err(format!(
                "`snippets.{name}` is longer than {SNIPPET_MAX} characters"
            ));
        }
        if holds_contact(text) {
            return Err(format!(
                "`snippets.{name}` contains a link, an email address or a phone number, which snippets never include"
            ));
        }
        written.insert(name.clone(), text.to_owned());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{SnippetsRequest, check, messages, schema, usable};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn fields() -> Map<String, Value> {
        json!({
            "given_name": "Ada",
            "company": "Acme Robotics",
            "phone_note": "call +1 555 010 9999",
            "industry": "Logistics\nand freight",
            "employees": 120,
            "tags": ["a", "b"],
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    /// Only the fields the workspace marked usable reach the model, as text on one line each;
    /// structured values are left out, and an empty set of facts says so.
    #[test]
    fn only_usable_fields_are_sent() {
        let fields = fields();
        let wanted = names(&["opener"]);
        let request = SnippetsRequest {
            instructions: "Open with a line about their industry.",
            names: &wanted,
            fields: &fields,
            deadline: None,
        };
        let usable_fields = names(&["given_name", "industry", "employees", "tags", "missing"]);
        let content = messages(&request, &usable_fields)[0].content.clone();
        assert!(content.contains("- given_name: Ada"), "{content}");
        assert!(
            content.contains("- industry: Logistics and freight"),
            "{content}"
        );
        assert!(content.contains("- employees: 120"), "{content}");
        assert!(!content.contains("Acme"), "{content}");
        assert!(!content.contains("555"), "{content}");
        assert!(!content.contains("tags"), "{content}");
        let none = messages(&request, &[])[0].content.clone();
        assert!(
            none.contains("Facts about the recipient:\n(none)"),
            "{none}"
        );
    }

    /// The schema requires exactly the requested snippets, as the strict mode of both wire
    /// formats needs.
    #[test]
    fn the_schema_lists_the_requested_snippets() {
        let schema = schema(&names(&["opener", "ps"]));
        assert_eq!(
            schema["properties"]["snippets"]["required"],
            json!(["opener", "ps"])
        );
        assert_eq!(
            schema["properties"]["snippets"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(schema["required"], json!(["snippets"]));
    }

    /// An answer is accepted with exactly the requested snippets; a missing, extra, empty,
    /// overlong snippet, or one inventing contact data, is a violation that names the snippet
    /// and never repeats its text.
    #[test]
    fn answers_are_checked() {
        let wanted = names(&["opener", "ps"]);
        let good = json!({"snippets": {"opener": " Congrats on the new warehouse. ", "ps": "Happy to share a case study."}});
        let checked = check(&good, &wanted).unwrap();
        assert_eq!(checked["opener"], "Congrats on the new warehouse.");
        for (wrong, mention) in [
            (json!({"snippets": {"opener": "Hi"}}), "exactly"),
            (
                json!({"snippets": {"opener": "Hi", "ps": "x", "extra": "y"}}),
                "exactly",
            ),
            (json!({"snippets": {"opener": "  ", "ps": "x"}}), "opener"),
            (
                json!({"snippets": {"opener": "x".repeat(501), "ps": "x"}}),
                "opener",
            ),
            (
                json!({"snippets": {"opener": "Book at cal.example.com/ada", "ps": "x"}}),
                "opener",
            ),
            (
                json!({"snippets": {"opener": "Hi", "ps": "Write to ada@example.com"}}),
                "ps",
            ),
            (json!({"snippets": {"opener": 3, "ps": "x"}}), "opener"),
            (json!({"snippets": "Hi"}), "snippets"),
            (
                json!({"snippets": {"opener": "Hi", "ps": "x"}, "note": "y"}),
                "property",
            ),
        ] {
            let violation = check(&wrong, &wanted).unwrap_err();
            assert!(violation.contains(mention), "{violation}");
            assert!(!violation.contains("ada@example.com"), "{violation}");
            assert!(!violation.contains("cal.example.com"), "{violation}");
        }
    }

    /// A request is served only with one to ten distinct, well-formed names and instructions of
    /// reasonable length; anything else falls back to the template's defaults without a call.
    #[test]
    fn unusable_requests_are_refused_before_any_call() {
        let fields = Map::new();
        let request = |names: &[String], instructions: &str| {
            usable(&SnippetsRequest {
                instructions,
                names,
                fields: &fields,
                deadline: None,
            })
        };
        assert!(request(&names(&["opener", "ps"]), "Write an opener."));
        assert!(!request(&names(&[]), "Write an opener."));
        assert!(!request(&names(&["opener", "opener"]), "Write an opener."));
        assert!(!request(&names(&["Opener"]), "Write an opener."));
        assert!(!request(&names(&["1st"]), "Write an opener."));
        let eleven: Vec<String> = (0..11).map(|index| format!("s{index}")).collect();
        assert!(!request(&eleven, "Write."));
        assert!(!request(&names(&["opener"]), "   "));
        assert!(!request(&names(&["opener"]), &"x".repeat(4_001)));
    }
}
