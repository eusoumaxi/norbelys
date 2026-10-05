//! Segment filters: which people a segment means, as conditions on their attributes and custom
//! fields, and the one form the database evaluates them in.
//!
//! # The filter
//!
//! `{"match": "all" | "any", "conditions": [{"field", "operator", "value"}, …]}`: a person is in
//! the segment when every condition holds (`all`, the default) or at least one does (`any`).
//! A condition names a person's `email`, `email_domain`, `given_name`, `family_name`,
//! `company` or `created_at`, or a custom field as `fields.<key>`, and compares it with
//! `value` by an operator that fits the field's type (see [`allowed`]). Text compares exactly
//! as stored; addresses and domains compare in lowercase, as the address key does.
//!
//! # Evaluation
//!
//! A filter is compiled into a SQL/JSON path predicate
//! (<https://www.postgresql.org/docs/current/functions-json.html#FUNCTIONS-SQLJSON-PATH>) over
//! a small JSON document of each person, with every value passed as a variable (`$v0`, `$v1`,
//! …): the path's text holds only fixed names and field keys checked when they were defined,
//! never a value, so a filter cannot inject anything, and the queries that apply it stay static
//! SQL that is checked at compile time. The document the queries build is, exactly:
//!
//! `{"email": email_key, "email_domain": the part after "@", "given_name", "family_name",
//! "company", "created_at": seconds since 1970 with their fraction, "fields": custom_fields}`.
//!
//! Building that document is most of a filter's cost (about 3 µs a person): a count or a
//! sparse segment's first page reads every person of the workspace once.
//!
//! A filter is checked against the workspace's field definitions when it is written, and again
//! whenever it is evaluated: a field change that would break a segment is refused, so a stored
//! filter always compiles.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::people::{self, Definition, FieldType, TEXT_MAX};

/// Conditions a filter may hold.
pub const CONDITIONS_MAX: usize = 20;
/// Values an `in` condition may list.
pub const VALUES_MAX: usize = 100;

/// A segment's filter, as written and stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// `all` (the default): a person matches when every condition holds; `any`: when at least
    /// one does.
    #[serde(default, rename = "match")]
    pub combine: Combine,
    /// The conditions, 1 to 20.
    #[schema(min_items = 1, max_items = 20)]
    pub conditions: Vec<Condition>,
}

/// How a filter's conditions combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    #[default]
    All,
    Any,
}

/// One condition of a filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// `email`, `email_domain`, `given_name`, `family_name`, `company`, `created_at`, or a
    /// custom field as `fields.<key>`.
    pub field: String,
    pub operator: Operator,
    /// What the field is compared with: absent for `exists` and `not_exists`, a list of 1 to
    /// 100 values for `in`, one value of the field's type otherwise (an RFC 3339 instant for
    /// `created_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Value)]
    pub value: Option<Value>,
}

/// How a condition compares.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Equals,
    NotEquals,
    /// Equal to one of the listed values.
    In,
    StartsWith,
    /// The person has a value (not null).
    Exists,
    /// The person has no value.
    NotExists,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// What a condition's field holds, which decides its operators and its operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Kind {
    /// `email`: the address key, lowercase.
    Address,
    /// `email_domain`: lowercase.
    Domain,
    /// A name, the company, or a text field: compared exactly.
    Text,
    /// `created_at`.
    Instant,
    /// A number field.
    Number,
    /// A boolean field.
    Boolean,
    /// An enum field.
    Choice,
    /// A date field (`YYYY-MM-DD`, ordered as text).
    Date,
}

/// Whether `operator` applies to a field of `kind`. Every pair has an answer here, so a new
/// operator or kind does not compile until it is decided.
#[must_use]
pub fn allowed(kind: Kind, operator: Operator) -> bool {
    use Operator::{Equals, Exists, Gt, Gte, In, Lt, Lte, NotEquals, NotExists, StartsWith};
    match kind {
        Kind::Address => matches!(operator, Equals | NotEquals | In | StartsWith),
        Kind::Domain => matches!(operator, Equals | NotEquals | In),
        Kind::Text => matches!(
            operator,
            Equals | NotEquals | In | StartsWith | Exists | NotExists
        ),
        Kind::Instant => matches!(operator, Gt | Gte | Lt | Lte),
        Kind::Number | Kind::Date => !matches!(operator, StartsWith),
        Kind::Boolean | Kind::Choice => {
            matches!(operator, Equals | NotEquals | In | Exists | NotExists)
        }
    }
}

/// A compiled filter: the path predicate and its variables, as `jsonb_path_match` takes them.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub path: String,
    pub vars: Value,
}

/// One problem of a filter, at an RFC 6901 pointer inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError {
    pub pointer: String,
    pub problem: String,
}

/// Checks `filter` against the workspace's `definitions` and compiles it.
///
/// # Errors
///
/// Every problem of the filter: too few or too many conditions, an unknown field, an operator
/// that does not fit the field, an operand of the wrong type or outside an enum's options.
pub fn compile(filter: &Filter, definitions: &[Definition]) -> Result<Compiled, Vec<FilterError>> {
    let mut errors = Vec::new();
    if filter.conditions.is_empty() || filter.conditions.len() > CONDITIONS_MAX {
        errors.push(FilterError {
            pointer: "/conditions".to_owned(),
            problem: "a filter has 1 to 20 conditions".to_owned(),
        });
    }
    let mut vars = Map::new();
    let mut predicates = Vec::with_capacity(filter.conditions.len());
    for (index, condition) in filter.conditions.iter().enumerate() {
        match predicate(condition, definitions, &mut vars) {
            Ok(predicate) => predicates.push(format!("({predicate})")),
            Err((member, problem)) => errors.push(FilterError {
                pointer: format!("/conditions/{index}/{member}"),
                problem,
            }),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let joint = match filter.combine {
        Combine::All => " && ",
        Combine::Any => " || ",
    };
    Ok(Compiled {
        path: predicates.join(joint),
        vars: Value::Object(vars),
    })
}

/// The path of a condition's field in the person's document, and its kind.
fn subject<'a>(
    field: &str,
    definitions: &'a [Definition],
) -> Option<(String, Kind, Option<&'a Definition>)> {
    let fixed = |name: &str, kind| Some((format!("$.{name}"), kind, None));
    match field {
        "email" => fixed(field, Kind::Address),
        "email_domain" => fixed(field, Kind::Domain),
        "given_name" | "family_name" | "company" => fixed(field, Kind::Text),
        "created_at" => fixed(field, Kind::Instant),
        _ => {
            let key = field.strip_prefix("fields.")?;
            let definition = definitions
                .iter()
                .find(|definition| definition.key == key)?;
            let kind = match definition.field_type {
                FieldType::Text => Kind::Text,
                FieldType::Number => Kind::Number,
                FieldType::Boolean => Kind::Boolean,
                FieldType::Enum => Kind::Choice,
                FieldType::Date => Kind::Date,
            };
            // A key is `[a-z][a-z0-9_]*` (checked when it was defined), so quoting it is safe.
            Some((format!("$.fields.\"{key}\""), kind, Some(definition)))
        }
    }
}

/// The path predicate of one condition; its operands are added to `vars`. An error names the
/// condition's member at fault.
fn predicate(
    condition: &Condition,
    definitions: &[Definition],
    vars: &mut Map<String, Value>,
) -> Result<String, (&'static str, String)> {
    let (path, kind, definition) = subject(&condition.field, definitions).ok_or((
        "field",
        "not a person attribute, nor `fields.` with a defined key".to_owned(),
    ))?;
    if !allowed(kind, condition.operator) {
        let operator: &'static str = condition.operator.into();
        return Err((
            "operator",
            format!("`{operator}` does not apply to this field"),
        ));
    }
    let value = condition.value.as_ref();
    let presence = matches!(condition.operator, Operator::Exists | Operator::NotExists);
    if presence && value.is_some() {
        return Err(("value", "this operator takes no value".to_owned()));
    }
    // One operand, read for the field and bound as the next variable.
    let mut single = || {
        let value = value.ok_or(("value", "this operator needs a value".to_owned()))?;
        let operand = operand(kind, definition, value).map_err(|problem| ("value", problem))?;
        Ok::<_, (&'static str, String)>(bind(vars, operand))
    };
    Ok(match condition.operator {
        Operator::Exists => format!("exists({path} ? (@ != null))"),
        Operator::NotExists => format!("!exists({path} ? (@ != null))"),
        Operator::Equals => format!("{path} == {}", single()?),
        Operator::NotEquals => format!("!({path} == {})", single()?),
        Operator::StartsWith => format!("{path} starts with {}", single()?),
        Operator::Gt => format!("{path} > {}", single()?),
        Operator::Gte => format!("{path} >= {}", single()?),
        Operator::Lt => format!("{path} < {}", single()?),
        Operator::Lte => format!("{path} <= {}", single()?),
        Operator::In => {
            let values = value
                .and_then(Value::as_array)
                .filter(|values| !values.is_empty() && values.len() <= VALUES_MAX)
                .ok_or(("value", "a list of 1 to 100 values".to_owned()))?;
            let mut alternatives = Vec::with_capacity(values.len());
            for value in values {
                let operand =
                    operand(kind, definition, value).map_err(|problem| ("value", problem))?;
                alternatives.push(format!("{path} == {}", bind(vars, operand)));
            }
            alternatives.join(" || ")
        }
    })
}

/// Adds `value` to `vars` as the next variable (`v0`, `v1`, …) and returns its reference in a
/// path (`$v0`).
fn bind(vars: &mut Map<String, Value>, value: Value) -> String {
    let name = format!("v{}", vars.len());
    vars.insert(name.clone(), value);
    format!("${name}")
}

/// Reads one operand for a field of `kind`, as the person's document holds that field.
fn operand(kind: Kind, definition: Option<&Definition>, value: &Value) -> Result<Value, String> {
    let text = |max: usize| match value {
        Value::String(text) if text.chars().count() <= max => Ok(text.clone()),
        Value::String(_) => Err(format!("at most {max} characters")),
        _ => Err("expected a string".to_owned()),
    };
    match kind {
        Kind::Address => Ok(Value::String(text(254)?.to_ascii_lowercase())),
        Kind::Domain => Ok(Value::String(text(253)?.to_ascii_lowercase())),
        Kind::Text => Ok(Value::String(text(TEXT_MAX)?)),
        Kind::Instant => {
            let instant = text(64)?
                .parse::<jiff::Timestamp>()
                .map_err(|_| "expected an RFC 3339 instant".to_owned())?;
            // Seconds since 1970 with the microseconds as a fraction, as the document holds them.
            let micros = instant.as_microsecond();
            let seconds = format!(
                "{}.{:06}",
                micros.div_euclid(1_000_000),
                micros.rem_euclid(1_000_000)
            );
            seconds
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| "expected an RFC 3339 instant".to_owned())
        }
        Kind::Number | Kind::Boolean | Kind::Choice | Kind::Date => {
            let definition = definition.ok_or_else(|| "expected a custom field".to_owned())?;
            people::check_value(definition, value)
                .map(|()| value.clone())
                .map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::{Compiled, Filter, Kind, Operator, allowed, compile};
    use crate::domain::people::{Definition, FieldType};

    fn definitions() -> Vec<Definition> {
        vec![
            Definition {
                key: "tier".to_owned(),
                field_type: FieldType::Enum,
                options: vec!["gold".to_owned(), "silver".to_owned()],
            },
            Definition {
                key: "employees".to_owned(),
                field_type: FieldType::Number,
                options: Vec::new(),
            },
        ]
    }

    fn filter(value: serde_json::Value) -> Filter {
        serde_json::from_value(value).unwrap()
    }

    /// The operators of each kind of field, written out: ordering only where values are
    /// ordered, prefixes only on text, presence only where a value can be absent. Generated over
    /// both enums, so a new operator or kind fails here until it is decided.
    #[test]
    fn every_kind_has_its_operators() {
        for kind in Kind::iter() {
            let operators: Vec<Operator> = Operator::iter()
                .filter(|operator| allowed(kind, *operator))
                .collect();
            use Operator::{
                Equals, Exists, Gt, Gte, In, Lt, Lte, NotEquals, NotExists, StartsWith,
            };
            let expected = match kind {
                Kind::Address => vec![Equals, NotEquals, In, StartsWith],
                Kind::Domain => vec![Equals, NotEquals, In],
                Kind::Text => vec![Equals, NotEquals, In, StartsWith, Exists, NotExists],
                Kind::Instant => vec![Gt, Gte, Lt, Lte],
                Kind::Number | Kind::Date => {
                    vec![Equals, NotEquals, In, Exists, NotExists, Gt, Gte, Lt, Lte]
                }
                Kind::Boolean | Kind::Choice => vec![Equals, NotEquals, In, Exists, NotExists],
            };
            assert_eq!(operators, expected, "{kind:?}");
        }
    }

    /// A filter compiles into one path predicate whose values are all variables: addresses
    /// lowercased, instants as seconds since 1970, custom fields quoted under `fields`,
    /// conditions joined by the filter's `match`.
    #[test]
    fn a_filter_compiles_into_a_path_with_variables() {
        let compiled = compile(
            &filter(json!({ "match": "any", "conditions": [
                { "field": "email_domain", "operator": "equals", "value": "Example.COM" },
                { "field": "fields.tier", "operator": "in", "value": ["gold", "silver"] },
                { "field": "created_at", "operator": "gte", "value": "1970-01-01T00:00:01Z" },
                { "field": "company", "operator": "not_exists" }
            ] })),
            &definitions(),
        )
        .unwrap();
        assert_eq!(
            compiled,
            Compiled {
                path: "($.email_domain == $v0) || ($.fields.\"tier\" == $v1 || $.fields.\"tier\" == $v2) || ($.created_at >= $v3) || (!exists($.company ? (@ != null)))".to_owned(),
                vars: json!({ "v0": "example.com", "v1": "gold", "v2": "silver", "v3": 1.0 }),
            }
        );
    }

    /// Every problem of a filter is reported at its pointer: an unknown field, an operator that
    /// does not fit, a value outside an enum's options or of the wrong type, a missing or an
    /// unexpected value, and an empty filter.
    #[test]
    fn filter_problems_are_reported_at_their_pointers() {
        let errors = compile(
            &filter(json!({ "conditions": [
                { "field": "phone", "operator": "equals", "value": "1" },
                { "field": "fields.employees", "operator": "starts_with", "value": "1" },
                { "field": "fields.tier", "operator": "equals", "value": "bronze" },
                { "field": "fields.employees", "operator": "gt", "value": "ten" },
                { "field": "company", "operator": "equals" },
                { "field": "company", "operator": "exists", "value": "x" }
            ] })),
            &definitions(),
        )
        .unwrap_err();
        let pointers: Vec<&str> = errors.iter().map(|error| error.pointer.as_str()).collect();
        assert_eq!(
            pointers,
            [
                "/conditions/0/field",
                "/conditions/1/operator",
                "/conditions/2/value",
                "/conditions/3/value",
                "/conditions/4/value",
                "/conditions/5/value"
            ]
        );
        let empty = compile(&filter(json!({ "conditions": [] })), &definitions()).unwrap_err();
        assert_eq!(empty[0].pointer, "/conditions");
    }
}
