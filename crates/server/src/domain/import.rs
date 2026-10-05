//! Reading import rows: which CSV column means what, and whether a row (a CSV record or an
//! element of a JSON `people` array) describes a person that can be stored.
//!
//! # Columns
//!
//! A CSV file's first record is its header. Each header names a person's attribute or a custom
//! field's key; names are compared after trimming, lowercasing and turning spaces, hyphens and
//! dots into underscores, so `First Name`, `first-name` and `first_name` are one column. The
//! common spellings exported by spreadsheets and CRMs are read as the attribute they mean
//! (`first_name` is `given_name`, `last_name` and `surname` are `family_name`, `organization`
//! is `company`). A column that names nothing is ignored, so a file exported from elsewhere
//! imports without editing; a file without an email column, or with two columns meaning the
//! same thing, is refused whole, because no row of it could be read unambiguously.
//!
//! # Rows
//!
//! A row is valid when its email is an address and every value it carries fits: names at most
//! 200 characters, each custom value of its field's type. An empty cell is absent: an import
//! fills and overwrites, it never clears what a person already has. Every problem of a row is
//! reported, each with the column it is in, so a person can fix the file in one pass.

use serde_json::{Map, Value};

use super::email::EmailAddress;
use super::people::{self, Definition, ValueError};

/// What a column of a CSV file is read as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Email,
    GivenName,
    FamilyName,
    Company,
    /// The custom field with this key.
    Field(String),
}

impl Target {
    /// The name errors report for this column's values.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Email => "email".to_owned(),
            Self::GivenName => "given_name".to_owned(),
            Self::FamilyName => "family_name".to_owned(),
            Self::Company => "company".to_owned(),
            Self::Field(key) => format!("fields.{key}"),
        }
    }
}

/// The meaning of each column of a header, in order; `None` for a column that is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Columns(Vec<Option<Target>>);

/// Why a header cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    /// No column is the email.
    #[error("the file has no email column (a header named `email`)")]
    MissingEmail,
    /// Two columns mean the same attribute or field.
    #[error("two columns are read as `{0}`; keep one")]
    Duplicate(String),
}

impl HeaderError {
    /// A short code for the import's `last_error`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingEmail => "missing_email_column",
            Self::Duplicate(_) => "duplicate_column",
        }
    }
}

/// The comparable form of a header name: trimmed, ASCII lowercase, with spaces, hyphens and
/// dots as underscores.
fn normalize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            ' ' | '-' | '.' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// The person attribute a normalized header name means, with its common spellings.
fn attribute(name: &str) -> Option<Target> {
    match name {
        "email" | "email_address" | "e_mail" => Some(Target::Email),
        "given_name" | "first_name" | "firstname" => Some(Target::GivenName),
        "family_name" | "last_name" | "lastname" | "surname" => Some(Target::FamilyName),
        "company" | "company_name" | "organization" | "organisation" => Some(Target::Company),
        _ => None,
    }
}

/// Reads a header against the workspace's field definitions.
///
/// # Errors
///
/// No email column, or two columns that mean the same thing.
pub fn map_header<S: AsRef<str>>(
    header: &[S],
    definitions: &[Definition],
) -> Result<Columns, HeaderError> {
    let mut targets: Vec<Option<Target>> = Vec::with_capacity(header.len());
    for name in header {
        let name = normalize(name.as_ref());
        let name = name.strip_prefix("fields_").unwrap_or(&name);
        let target = attribute(name).or_else(|| {
            definitions
                .iter()
                .find(|definition| definition.key == name)
                .map(|definition| Target::Field(definition.key.clone()))
        });
        if let Some(target) = &target
            && targets.iter().flatten().any(|seen| seen == target)
        {
            return Err(HeaderError::Duplicate(target.name()));
        }
        targets.push(target);
    }
    if !targets
        .iter()
        .flatten()
        .any(|target| *target == Target::Email)
    {
        return Err(HeaderError::MissingEmail);
    }
    Ok(Columns(targets))
}

/// A person a row describes: what an import creates, or merges into the person with the same
/// address.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonRow {
    pub email: EmailAddress,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub company: Option<String>,
    /// The custom values the row carries, already of their fields' types.
    pub fields: Map<String, Value>,
}

/// One problem of a row: the column (an attribute, or `fields.<key>`) and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowError {
    pub field: String,
    pub problem: String,
}

impl RowError {
    fn new(field: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            problem: problem.into(),
        }
    }
}

/// Collects a row's attributes and fields, then checks the whole.
#[derive(Default)]
struct Builder {
    email: Option<Result<EmailAddress, String>>,
    given_name: Option<String>,
    family_name: Option<String>,
    company: Option<String>,
    fields: Map<String, Value>,
    errors: Vec<RowError>,
}

impl Builder {
    fn email(&mut self, text: &str) {
        self.email = Some(EmailAddress::parse(text).map_err(|error| error.to_string()));
    }

    fn name(&mut self, target: &Target, text: &str) {
        match people::read_name(text) {
            Ok(value) => match target {
                Target::GivenName => self.given_name = value,
                Target::FamilyName => self.family_name = value,
                Target::Company => self.company = value,
                Target::Email | Target::Field(_) => {}
            },
            Err(problem) => self.errors.push(RowError::new(target.name(), problem)),
        }
    }

    fn finish(mut self) -> Result<PersonRow, Vec<RowError>> {
        let email = match self.email {
            Some(Ok(email)) => Some(email),
            Some(Err(problem)) => {
                self.errors.insert(0, RowError::new("email", problem));
                None
            }
            None => {
                self.errors
                    .insert(0, RowError::new("email", "an email is required"));
                None
            }
        };
        match email {
            Some(email) if self.errors.is_empty() => Ok(PersonRow {
                email,
                given_name: self.given_name,
                family_name: self.family_name,
                company: self.company,
                fields: self.fields,
            }),
            _ => Err(self.errors),
        }
    }
}

/// Reads one CSV record under `columns`; cells beyond the header are ignored, missing ones are
/// empty.
///
/// # Errors
///
/// Every problem of the row.
pub fn csv_row<S: AsRef<str>>(
    columns: &Columns,
    definitions: &[Definition],
    cells: &[S],
) -> Result<PersonRow, Vec<RowError>> {
    let mut row = Builder::default();
    for (target, cell) in columns.0.iter().zip(cells) {
        let Some(target) = target else { continue };
        let cell = cell.as_ref().trim();
        match target {
            Target::Email if cell.is_empty() => {}
            Target::Email => row.email(cell),
            Target::Field(key) => {
                if cell.is_empty() {
                    continue;
                }
                let Some(definition) = definitions.iter().find(|definition| &definition.key == key)
                else {
                    continue;
                };
                match people::read_cell(definition, cell) {
                    Ok(value) => {
                        row.fields.insert(key.clone(), value);
                    }
                    Err(error) => row
                        .errors
                        .push(RowError::new(target.name(), error.to_string())),
                }
            }
            Target::GivenName | Target::FamilyName | Target::Company => row.name(target, cell),
        }
    }
    row.finish()
}

/// Reads one element of a JSON `people` array: an object with `email`, optional
/// `given_name`, `family_name` and `company` (strings or null) and optional `fields` (an object
/// of typed values).
///
/// # Errors
///
/// Every problem of the row, including members the object should not have.
pub fn json_row(definitions: &[Definition], value: &Value) -> Result<PersonRow, Vec<RowError>> {
    let Some(object) = value.as_object() else {
        return Err(vec![RowError::new("", "each person is a JSON object")]);
    };
    let mut row = Builder::default();
    for (member, value) in object {
        match (member.as_str(), value) {
            (_, Value::Null) if member != "email" => {}
            ("email", Value::String(text)) => row.email(text),
            ("email", _) => row.email = Some(Err("an email is a string".to_owned())),
            ("given_name" | "family_name" | "company", Value::String(text)) => {
                let target = match member.as_str() {
                    "given_name" => Target::GivenName,
                    "family_name" => Target::FamilyName,
                    _ => Target::Company,
                };
                row.name(&target, text);
            }
            ("given_name" | "family_name" | "company", _) => {
                row.errors
                    .push(RowError::new(member.clone(), "expected a string"));
            }
            ("fields", Value::Object(fields)) => {
                for (key, value) in fields {
                    if value.is_null() {
                        continue;
                    }
                    let field = format!("fields.{key}");
                    match definitions.iter().find(|definition| &definition.key == key) {
                        None => row
                            .errors
                            .push(RowError::new(field, ValueError::Unknown.to_string())),
                        Some(definition) => match people::check_value(definition, value) {
                            Ok(()) => {
                                row.fields.insert(key.clone(), value.clone());
                            }
                            Err(error) => row.errors.push(RowError::new(field, error.to_string())),
                        },
                    }
                }
            }
            ("fields", _) => row.errors.push(RowError::new(
                "fields",
                "expected an object of custom values",
            )),
            _ => row.errors.push(RowError::new(
                member.clone(),
                "a person has no such attribute",
            )),
        }
    }
    row.finish()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{HeaderError, PersonRow, RowError, Target, csv_row, json_row, map_header};
    use crate::domain::email::EmailAddress;
    use crate::domain::people::{Definition, FieldType};

    fn definitions() -> Vec<Definition> {
        vec![
            Definition {
                key: "employees".to_owned(),
                field_type: FieldType::Number,
                options: Vec::new(),
            },
            Definition {
                key: "tier".to_owned(),
                field_type: FieldType::Enum,
                options: vec!["Gold".to_owned()],
            },
        ]
    }

    /// Headers are read whatever their spelling: common names of the attributes, field keys
    /// with or without `fields.`, ignored columns kept as gaps so cells stay aligned.
    #[test]
    fn headers_map_their_common_spellings() {
        let columns = map_header(
            &[
                " E-mail ",
                "First Name",
                "surname",
                "Organization",
                "Phone",
                "fields.tier",
                "EMPLOYEES",
            ],
            &definitions(),
        )
        .unwrap();
        assert_eq!(
            columns.0,
            vec![
                Some(Target::Email),
                Some(Target::GivenName),
                Some(Target::FamilyName),
                Some(Target::Company),
                None,
                Some(Target::Field("tier".to_owned())),
                Some(Target::Field("employees".to_owned())),
            ]
        );
    }

    /// A header without an email column, or with two columns read as the same thing, refuses
    /// the whole file: no row of it could be read unambiguously.
    #[test]
    fn a_header_needs_one_email_and_no_twins() {
        assert_eq!(
            map_header(&["name", "company"], &definitions()),
            Err(HeaderError::MissingEmail)
        );
        assert_eq!(
            map_header(&["email", "first_name", "given_name"], &definitions()),
            Err(HeaderError::Duplicate("given_name".to_owned()))
        );
        assert_eq!(
            map_header(&["email", "tier", "Fields.Tier"], &definitions()),
            Err(HeaderError::Duplicate("fields.tier".to_owned()))
        );
    }

    /// A valid record becomes a person: cells trimmed, empty cells absent, values typed; a
    /// missing trailing cell is empty and an extra one ignored.
    #[test]
    fn a_valid_record_becomes_a_person() {
        let defs = definitions();
        let columns = map_header(
            &["email", "given_name", "company", "employees", "tier"],
            &defs,
        )
        .unwrap();
        let row = csv_row(
            &columns,
            &defs,
            &[" Ada@Example.com ", "Ada", "", "12", "gold", "x"],
        );
        assert_eq!(
            row,
            Ok(PersonRow {
                email: EmailAddress::parse("Ada@Example.com").unwrap(),
                given_name: Some("Ada".to_owned()),
                family_name: None,
                company: None,
                fields: json!({ "employees": 12, "tier": "Gold" })
                    .as_object()
                    .unwrap()
                    .clone(),
            })
        );
        assert!(csv_row(&columns, &defs, &["ada@example.com"]).is_ok());
    }

    /// An invalid record reports every problem with its column, the email first: a person
    /// fixes the whole row in one pass.
    #[test]
    fn an_invalid_record_reports_every_problem() {
        let defs = definitions();
        let columns = map_header(&["email", "given_name", "employees", "tier"], &defs).unwrap();
        let long = "x".repeat(201);
        let errors = csv_row(
            &columns,
            &defs,
            &["not-an-address", long.as_str(), "many", "Bronze"],
        )
        .unwrap_err();
        let fields: Vec<&str> = errors.iter().map(|error| error.field.as_str()).collect();
        assert_eq!(
            fields,
            ["email", "given_name", "fields.employees", "fields.tier"]
        );
        assert_eq!(
            csv_row(&columns, &defs, &["", "Ada"]).unwrap_err(),
            vec![RowError {
                field: "email".to_owned(),
                problem: "an email is required".to_owned()
            }]
        );
    }

    /// A JSON person is read with the API's types: typed custom values, null meaning absent,
    /// and members a person does not have refused rather than dropped.
    #[test]
    fn json_people_are_read_with_their_types() {
        let defs = definitions();
        let row = json_row(
            &defs,
            &json!({ "email": "ada@example.com", "company": null, "fields": { "employees": 3, "tier": null } }),
        )
        .unwrap();
        assert_eq!(row.fields, *json!({ "employees": 3 }).as_object().unwrap());
        let errors = json_row(
            &defs,
            &json!({ "email": 7, "phone": "1", "fields": { "employees": "3", "unknown": 1 } }),
        )
        .unwrap_err();
        let fields: Vec<&str> = errors.iter().map(|error| error.field.as_str()).collect();
        assert_eq!(
            fields,
            ["email", "fields.employees", "fields.unknown", "phone"]
        );
        assert!(json_row(&defs, &json!(["ada@example.com"])).is_err());
    }
}
