//! The namespaces a template can read, frozen into the message when it is created.
//!
//! | Namespace | What it holds | When |
//! |---|---|---|
//! | `person` | `id`, `email`, `given_name`, `family_name`, `company`, and `fields`: the custom fields by key | the message has a person (campaign mail; a direct message to an address that is a person of the workspace) |
//! | `sender` | `email`, `name`: the From identity | always |
//! | `campaign` | `id`, `name` | campaign mail |
//! | `step` | `id`, `name`, `position` (1 for the first step) | campaign mail |
//! | `variables` | the values the creator passed: a direct message's `variables`, the snippets a generation wrote for a step | always (empty when none) |
//! | `unsubscribe_url` | the recipient's unsubscribe link | campaign mail, at sending; creation checks with a stand-in |
//!
//! **Frozen at creation.** The namespaces are read once, when the message is created, and
//! stored as `messages.render_context`. Every later render (each attempt, after any retry) reads
//! the same values, so a message renders the same however late it is sent, and an edit of the
//! person between attempts never produces two versions of one message. The person's row is read
//! in the creating transaction, under the same row security as the rest of it.
//!
//! **No nulls.** A value that is absent or `null` is left out, so a template meets it as
//! undefined (an error to print, false to test) rather than as the text `none`.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::db::Tx;
use crate::domain::ids::{Campaign, Id, Person, Step, WorkspaceId};

/// The key of the unsubscribe link in a render's context.
pub const UNSUBSCRIBE_URL: &str = "unsubscribe_url";

/// The frozen context of one message.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Namespaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person: Option<Value>,
    pub sender: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<Value>,
    pub variables: Value,
}

impl Namespaces {
    /// The context as `messages.render_context` stores it and the engine reads it.
    #[must_use]
    pub fn to_value(&self) -> Value {
        prune(serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new())))
    }
}

/// The `sender` namespace of an identity.
#[must_use]
pub fn sender(email: &str, name: Option<&str>) -> Value {
    prune(serde_json::json!({ "email": email, "name": name }))
}

/// The `variables` namespace from a creator's values (an object; anything else is empty).
#[must_use]
pub fn variables(values: Option<Map<String, Value>>) -> Value {
    prune(Value::Object(values.unwrap_or_default()))
}

/// The `person` namespace of `person`, read in the creating transaction; `None` when the person
/// does not exist in the workspace.
///
/// # Errors
///
/// The database is unavailable.
pub async fn person(
    tx: &mut Tx,
    workspace: WorkspaceId,
    person: Id<Person>,
) -> Result<Option<Value>, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT email, given_name, family_name, company, custom_fields
           FROM people WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        person.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| {
        prune(serde_json::json!({
            "id": person.to_string(),
            "email": row.email,
            "given_name": row.given_name,
            "family_name": row.family_name,
            "company": row.company,
            "fields": row.custom_fields,
        }))
    }))
}

/// The `campaign` and `step` namespaces of a step of a campaign; `None` when the step is not one
/// of the campaign's.
///
/// # Errors
///
/// The database is unavailable.
pub async fn campaign_and_step(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    step: Id<Step>,
) -> Result<Option<(Value, Value)>, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT c.name AS campaign_name, s.name AS step_name, s.position
           FROM campaigns c JOIN steps s ON s.workspace_id = c.workspace_id AND s.campaign_id = c.id
          WHERE c.workspace_id = $1 AND c.id = $2 AND s.id = $3",
        workspace.uuid(),
        campaign.uuid(),
        step.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| {
        (
            prune(serde_json::json!({ "id": campaign.to_string(), "name": row.campaign_name })),
            prune(serde_json::json!({
                "id": step.to_string(),
                "name": row.step_name,
                "position": row.position,
            })),
        )
    }))
}

/// `context` with the unsubscribe link added, for a render of campaign mail.
#[must_use]
pub fn with_unsubscribe(context: &Value, url: &str) -> Value {
    let mut context = context.clone();
    if let Value::Object(map) = &mut context {
        map.insert(UNSUBSCRIBE_URL.to_owned(), Value::String(url.to_owned()));
    }
    context
}

/// `value` without `null` members, at every depth of nested objects (array elements stay where
/// they are, so positions keep their meaning).
#[must_use]
pub fn prune(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, prune(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(prune).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Nulls disappear at every depth of objects, so a template meets an absent value as
    /// undefined; array positions are kept.
    #[test]
    fn nulls_are_left_out() {
        assert_eq!(
            prune(json!({ "a": null, "b": { "c": null, "d": 1 }, "e": [null, { "f": null }] })),
            json!({ "b": { "d": 1 }, "e": [null, {}] })
        );
        assert_eq!(
            sender("max@example.com", None),
            json!({ "email": "max@example.com" })
        );
        assert_eq!(variables(None), json!({}));
    }

    /// The stored context names exactly the namespaces the message has; `variables` is always
    /// there, so a missing variable is reported as the variable, not as its namespace.
    #[test]
    fn the_context_holds_only_what_the_message_has() {
        let direct = Namespaces {
            sender: sender("max@example.com", Some("Max")),
            variables: variables(None),
            ..Namespaces::default()
        };
        assert_eq!(
            direct.to_value(),
            json!({ "sender": { "email": "max@example.com", "name": "Max" }, "variables": {} })
        );
        let with_link = with_unsubscribe(&direct.to_value(), "https://t.example/u/x");
        assert_eq!(with_link[UNSUBSCRIBE_URL], "https://t.example/u/x");
    }
}
