//! List hygiene hints: the risk that a cold email to one contact harms the sender, judged from
//! the contact's fields. No job kind calls it yet: its prompt, schema, checks and golden set
//! exist so that it can be added as one kind of the `ai` queue when a workspace asks for it,
//! with its evaluation already in place.

use norbelys_ai::{Message, Role};
use serde_json::{Map, Value, json};

use super::classify::{bounded, only, strings};

/// The risk of one contact.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumString, strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Risk {
    /// A named person at a domain matching their company.
    Low,
    /// A shared inbox, personal webmail for a business contact, contradictory fields.
    Medium,
    /// A disposable domain, an invented or trap-like address, a system address.
    High,
}

impl Risk {
    /// The risk as the schema writes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The model's hint, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// The risk.
    pub risk: Risk,
    /// One to three short reasons.
    pub reasons: Vec<String>,
}

/// The most characters of one field's value sent.
const FIELD_CHARS: usize = 300;
/// The most output tokens of an answer, thinking included.
pub const MAX_TOKENS: u32 = 1_024;

/// The input turn: the contact's fields, one per line, each value on one line and bounded.
#[must_use]
pub fn messages(fields: &Map<String, Value>) -> Vec<Message> {
    let lines: Vec<String> = fields
        .iter()
        .filter_map(|(key, value)| {
            let value = match value {
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
            Some(format!("- {key}: {}", value.trim()))
        })
        .collect();
    vec![Message {
        role: Role::User,
        content: format!("The contact's fields:\n{}", lines.join("\n")),
    }]
}

/// The JSON schema of a hint.
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "risk": {"type": "string", "enum": ["low", "medium", "high"]},
            "reasons": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["risk", "reasons"],
        "additionalProperties": false,
    })
}

/// Checks an answer: a known risk and reasons as strings (cut to three of 200 characters).
///
/// # Errors
///
/// What is wrong, to show the model.
pub fn check(answer: &Value) -> Result<Hint, String> {
    let fields = answer
        .as_object()
        .ok_or_else(|| "the answer is not a JSON object".to_owned())?;
    only(fields, &["risk", "reasons"])?;
    let risk = fields
        .get("risk")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Risk>().ok())
        .ok_or_else(|| "`risk` must be one of low, medium, high".to_owned())?;
    let reasons = strings(fields.get("reasons"))
        .ok_or_else(|| "`reasons` must be an array of strings".to_owned())?;
    Ok(Hint {
        risk,
        reasons: bounded(reasons),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::{Risk, check, messages};

    /// Every risk the schema lists is accepted and anything else is a violation naming the
    /// field; the fields reach the model one per line, structured values left out.
    #[test]
    fn hints_are_checked_and_fields_listed() {
        for risk in Risk::iter() {
            let hint = check(&json!({"risk": risk.as_str(), "reasons": ["Role inbox"]})).unwrap();
            assert_eq!(hint.risk, risk);
        }
        let violation = check(&json!({"risk": "severe", "reasons": []})).unwrap_err();
        assert!(violation.contains("`risk`") && !violation.contains("severe"));
        let fields =
            json!({"email": "info@acme.example", "given_name": "Ada\nLovelace", "tags": ["x"]});
        let content = messages(fields.as_object().unwrap())[0].content.clone();
        assert!(content.contains("- email: info@acme.example"), "{content}");
        assert!(content.contains("- given_name: Ada Lovelace"), "{content}");
        assert!(!content.contains("tags"), "{content}");
    }
}
