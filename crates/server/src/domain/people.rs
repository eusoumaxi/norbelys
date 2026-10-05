//! People's attributes and custom fields: the bounds of a person's own attributes, the types a
//! workspace gives its custom fields, and the rules a value must meet to be stored under one.
//!
//! A person has a fixed set of attributes (`email`, `given_name`, `family_name`, `company`)
//! and a JSON object of custom fields. A workspace declares each custom field once, with a key
//! and a [`FieldType`]; a value is accepted only when it fits its definition, and a key without
//! a definition is refused. These checks are the first net; the database's
//! `people_field_definition` trigger repeats the type check on every write as the second, so a
//! writer that skipped this module still cannot store a value of the wrong type.
//!
//! Two shapes reach these checks. A JSON value (the API, a JSON import) must already have the
//! field's type ([`check_value`]). A CSV cell is text and is first read as the field's type
//! ([`read_cell`]): `42` becomes a number, `yes` a boolean, so a spreadsheet's columns import
//! without quoting rules of their own.

use jiff::civil;
use serde_json::{Map, Value};

/// The longest `given_name`, `family_name` and `company`, in characters.
pub const NAME_MAX: usize = 200;
/// The longest text value of a custom field, in characters.
pub const TEXT_MAX: usize = 1_000;
/// Custom field definitions a workspace may hold: every person write checks every definition.
pub const FIELDS_MAX: i64 = 100;
/// The options an enum field may have.
pub const OPTIONS_MAX: usize = 100;
/// The longest option of an enum field, in characters.
pub const OPTION_MAX: usize = 100;
/// The longest label of a field, in characters.
pub const LABEL_MAX: usize = 100;

/// Names a field key may not take: the person's own attributes and the names imports and
/// segments read as them, which would make a column or a condition ambiguous.
const RESERVED_KEYS: [&str; 18] = [
    "id",
    "email",
    "email_address",
    "email_domain",
    "e_mail",
    "given_name",
    "first_name",
    "firstname",
    "family_name",
    "last_name",
    "lastname",
    "surname",
    "company",
    "company_name",
    "organization",
    "organisation",
    "created_at",
    "updated_at",
];

/// The type of a custom field.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// A string of at most 1,000 characters.
    Text,
    /// A JSON number.
    Number,
    /// `true` or `false`.
    Boolean,
    /// One of the field's options, exactly as written there.
    Enum,
    /// A calendar date, `YYYY-MM-DD`.
    Date,
}

impl FieldType {
    /// The type as stored in `person_field_definitions.field_type` and shown on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// What a value of this type looks like, for error messages.
    #[must_use]
    pub fn expected(self) -> &'static str {
        match self {
            Self::Text => "a string",
            Self::Number => "a number",
            Self::Boolean => "true or false",
            Self::Enum => "one of the field's options",
            Self::Date => "a date as YYYY-MM-DD",
        }
    }
}

/// A workspace's definition of one custom field, as the checks need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The key the value is stored under in a person's `fields`.
    pub key: String,
    pub field_type: FieldType,
    /// The allowed values of an enum field; empty for every other type.
    pub options: Vec<String>,
}

/// Why a value cannot be stored under a field.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    /// No definition has this key.
    #[error("no custom field has this key; define it first")]
    Unknown,
    /// The value has another type.
    #[error("expected {0}")]
    Type(&'static str),
    /// The value is not one of an enum field's options.
    #[error("`{0}` is not one of the field's options")]
    NotAnOption(String),
    /// The text is longer than [`TEXT_MAX`].
    #[error("at most 1,000 characters")]
    TooLong,
}

/// Checks that `value` (not null) can be stored under `definition`.
///
/// # Errors
///
/// The value's type or content does not fit the field.
pub fn check_value(definition: &Definition, value: &Value) -> Result<(), ValueError> {
    let wrong = || ValueError::Type(definition.field_type.expected());
    match definition.field_type {
        FieldType::Text => match value {
            Value::String(text) if text.chars().count() > TEXT_MAX => Err(ValueError::TooLong),
            Value::String(_) => Ok(()),
            _ => Err(wrong()),
        },
        FieldType::Number => value.is_number().then_some(()).ok_or_else(wrong),
        FieldType::Boolean => value.is_boolean().then_some(()).ok_or_else(wrong),
        FieldType::Enum => match value {
            Value::String(text) if definition.options.contains(text) => Ok(()),
            Value::String(text) => Err(ValueError::NotAnOption(text.clone())),
            _ => Err(wrong()),
        },
        FieldType::Date => match value {
            Value::String(text) if is_date(text) => Ok(()),
            _ => Err(wrong()),
        },
    }
}

/// True for a real calendar date written `YYYY-MM-DD`: exactly the shape the database's check
/// accepts, and a day that exists (no 30 February).
#[must_use]
pub fn is_date(text: &str) -> bool {
    let shaped = text.len() == 10
        && text.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        });
    shaped && text.parse::<civil::Date>().is_ok()
}

/// Reads a CSV cell (trimmed, not empty) as a value of `definition`'s type: a number is an
/// integer when it is written as one; a boolean is `true`/`false`, `yes`/`no` or `1`/`0` in any
/// case; an enum option matches exactly, else ignoring case, and is stored as the option is
/// written.
///
/// # Errors
///
/// The cell cannot be read as the field's type.
pub fn read_cell(definition: &Definition, cell: &str) -> Result<Value, ValueError> {
    let wrong = || ValueError::Type(definition.field_type.expected());
    let value = match definition.field_type {
        FieldType::Text | FieldType::Date => Value::String(cell.to_owned()),
        FieldType::Number => match cell.parse::<i64>() {
            Ok(integer) => Value::from(integer),
            Err(_) => cell
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(wrong)?,
        },
        FieldType::Boolean => match cell.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Value::Bool(true),
            "false" | "no" | "0" => Value::Bool(false),
            _ => return Err(wrong()),
        },
        FieldType::Enum => definition
            .options
            .iter()
            .find(|option| option.as_str() == cell)
            .or_else(|| {
                definition
                    .options
                    .iter()
                    .find(|option| option.eq_ignore_ascii_case(cell))
            })
            .map(|option| Value::String(option.clone()))
            .ok_or_else(|| ValueError::NotAnOption(cell.to_owned()))?,
    };
    check_value(definition, &value)?;
    Ok(value)
}

/// Checks a person's `fields` as an API request writes them: every key must be defined and every
/// value fit its definition; `null` passes (it removes the field). Returns each refused key with
/// its reason, in the object's order.
#[must_use]
pub fn check_fields(
    definitions: &[Definition],
    fields: &Map<String, Value>,
) -> Vec<(String, ValueError)> {
    fields
        .iter()
        .filter_map(|(key, value)| {
            let Some(definition) = definitions.iter().find(|definition| &definition.key == key)
            else {
                return Some((key.clone(), ValueError::Unknown));
            };
            if value.is_null() {
                return None;
            }
            check_value(definition, value)
                .err()
                .map(|error| (key.clone(), error))
        })
        .collect()
}

/// Checks a new field key: a lowercase letter, then up to 63 lowercase letters, digits or
/// underscores (the database's own check), and not one of the names that mean a person's own
/// attribute.
///
/// # Errors
///
/// What is wrong with the key, for a person to read.
pub fn check_key(key: &str) -> Result<(), &'static str> {
    let mut bytes = key.bytes();
    let shaped = bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && key.len() <= 64
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if !shaped {
        return Err(
            "a key is a lowercase letter followed by up to 63 lowercase letters, digits or underscores",
        );
    }
    if RESERVED_KEYS.contains(&key) {
        return Err("this key names a person's own attribute");
    }
    Ok(())
}

/// Trims a person's name or company and checks its length; an empty value is absent.
///
/// # Errors
///
/// The value is longer than [`NAME_MAX`].
pub fn read_name(value: &str) -> Result<Option<String>, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > NAME_MAX {
        return Err("at most 200 characters");
    }
    Ok(Some(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use strum::IntoEnumIterator as _;

    use super::{
        Definition, FieldType, ValueError, check_fields, check_key, check_value, is_date, read_cell,
    };

    fn definition(field_type: FieldType) -> Definition {
        Definition {
            key: "k".to_owned(),
            field_type,
            options: if field_type == FieldType::Enum {
                vec!["Gold".to_owned(), "Silver".to_owned()]
            } else {
                Vec::new()
            },
        }
    }

    /// Every field type accepts exactly its own JSON shape and refuses each other one, so a new
    /// type fails this table until its values are decided: the API refuses a mistyped value
    /// before the database's trigger has to.
    #[test]
    fn each_type_accepts_its_own_shape_only() {
        let samples = [
            json!("text"),
            json!(42),
            json!(true),
            json!("Gold"),
            json!("2026-10-01"),
        ];
        for field_type in FieldType::iter() {
            let accepted: Vec<&Value> = samples
                .iter()
                .filter(|value| check_value(&definition(field_type), value).is_ok())
                .collect();
            let expected: Vec<&Value> = match field_type {
                FieldType::Text => vec![&samples[0], &samples[3], &samples[4]],
                FieldType::Number => vec![&samples[1]],
                FieldType::Boolean => vec![&samples[2]],
                FieldType::Enum => vec![&samples[3]],
                FieldType::Date => vec![&samples[4]],
            };
            assert_eq!(accepted, expected, "{field_type:?}");
        }
    }

    /// The content rules beyond the type: an enum value is one of the options exactly, a date
    /// exists on the calendar in the `YYYY-MM-DD` shape, and text stops at 1,000 characters.
    #[test]
    fn values_meet_their_content_rules() {
        let enumeration = definition(FieldType::Enum);
        assert_eq!(
            check_value(&enumeration, &json!("gold")),
            Err(ValueError::NotAnOption("gold".to_owned()))
        );
        for date in [
            "2026-02-30",
            "2026-2-03",
            "20260203",
            "2026-02-03T00:00:00Z",
        ] {
            assert!(!is_date(date), "{date}");
        }
        assert!(is_date("2024-02-29"));
        let text = definition(FieldType::Text);
        assert!(check_value(&text, &json!("x".repeat(1_000))).is_ok());
        assert_eq!(
            check_value(&text, &json!("x".repeat(1_001))),
            Err(ValueError::TooLong)
        );
    }

    /// A CSV cell is read as its field's type: integers stay integers, decimals are numbers,
    /// booleans take the usual spellings, an option matches ignoring case and is stored as
    /// defined; anything else is refused with the type it should have had.
    #[test]
    fn cells_are_read_as_their_field_type() {
        let cases = [
            (FieldType::Number, "42", Ok(json!(42))),
            (FieldType::Number, "4.5", Ok(json!(4.5))),
            (
                FieldType::Number,
                "1,000",
                Err(ValueError::Type("a number")),
            ),
            (FieldType::Boolean, "Yes", Ok(json!(true))),
            (FieldType::Boolean, "0", Ok(json!(false))),
            (
                FieldType::Boolean,
                "maybe",
                Err(ValueError::Type("true or false")),
            ),
            (FieldType::Enum, "silver", Ok(json!("Silver"))),
            (
                FieldType::Enum,
                "Bronze",
                Err(ValueError::NotAnOption("Bronze".to_owned())),
            ),
            (FieldType::Date, "2026-10-01", Ok(json!("2026-10-01"))),
            (
                FieldType::Date,
                "01/10/2026",
                Err(ValueError::Type("a date as YYYY-MM-DD")),
            ),
            (FieldType::Text, "anything", Ok(json!("anything"))),
        ];
        for (field_type, cell, expected) in cases {
            assert_eq!(read_cell(&definition(field_type), cell), expected, "{cell}");
        }
    }

    /// A request's `fields` are refused key by key: an undefined key, a mistyped value; `null`
    /// passes because it removes the field.
    #[test]
    fn request_fields_need_a_definition_and_a_fitting_value() {
        let definitions = [definition(FieldType::Number)];
        let fields = json!({ "k": "many", "other": 1 });
        let refused = check_fields(&definitions, fields.as_object().unwrap());
        assert_eq!(
            refused,
            vec![
                ("k".to_owned(), ValueError::Type("a number")),
                ("other".to_owned(), ValueError::Unknown)
            ]
        );
        let cleared = json!({ "k": null });
        assert!(check_fields(&definitions, cleared.as_object().unwrap()).is_empty());
    }

    /// A key is a lowercase identifier the database's check accepts, and never a name that
    /// means a person's own attribute in imports and segments.
    #[test]
    fn keys_are_identifiers_that_name_no_attribute() {
        assert!(check_key("industry_2").is_ok());
        for key in [
            "",
            "2nd",
            "Industry",
            "has-dash",
            &"k".repeat(65),
            "email",
            "first_name",
        ] {
            assert!(check_key(key).is_err(), "{key}");
        }
    }
}
