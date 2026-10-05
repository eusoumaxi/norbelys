//! The operations of the public API, derived from the OpenAPI document this binary embeds.
//!
//! # Why derived
//!
//! The API's contract is its OpenAPI document, generated from the server's handlers and
//! committed beside them as `crates/server/openapi.json`, with the public surface only: the
//! dashboard's operations, which accept a browser session alone, are never in it. The CLI
//! embeds that file at build time and turns every operation in it into one command, so a new
//! operation becomes a command by rebuilding, and no command can describe a request the server
//! does not serve.
//!
//! # The mapping
//!
//! - The operation id `<resource>.<action>` is the command `norbelys <resource> <action>`, in
//!   the API's own words: `people.create` is `norbelys people create`,
//!   `webhook_endpoints.rotate_secret` is `norbelys webhook_endpoints rotate_secret`.
//! - Path parameters are positional arguments, in the order the path names them.
//! - Query parameters, header parameters and the members of a JSON body are flags, named in
//!   kebab case: `created_at[gte]` is `--created-at-gte`, `If-Match` is `--if-match`,
//!   `event_types` is `--event-types`. An array member is a repeated flag
//!   (`--event-types a --event-types b`), and an object member takes JSON text.
//!   A body member that would take a reserved flag gains `body-`: `content_type` becomes
//!   `--body-content-type`, leaving `--content-type` to select the request's media type.
//! - A query parameter and a body member of the same name share one flag. It sets the body
//!   member when the body is JSON and the query parameter otherwise: an import takes its group
//!   in the JSON body, or as a query parameter beside an uploaded CSV file, which cannot carry
//!   it.
//! - `Idempotency-Key` is the global `--idempotency-key`, never a flag of one command.
//! - A list (an operation with a `cursor` query parameter) also takes `--all`.
//!
//! Values are typed by the schema (an integer member is sent as a JSON number), but request
//! enums are not checked here: the server's answer names an invalid value, and a CLI built
//! before a value was added must still be able to send it.
//!
//! # Invariants
//!
//! Every operation becomes exactly one command. No two flags of a command share a name, none
//! takes the name of a global flag or of the flags a body or a list adds (`--data`,
//! `--content-type`, `--all`), and no resource takes the name of a built-in command (`login`,
//! `listen`, `trigger`). Reserved body names are prefixed before checking; a remaining clash
//! still fails derivation. A test derives the embedded document,
//! so a clash shows when the document is regenerated, not when someone runs the command.

use std::collections::BTreeSet;

use reqwest::Method;
use serde_json::{Map, Value};

/// The public OpenAPI document of the API, committed beside the server and embedded at build
/// time.
pub const DOCUMENT: &str = include_str!("../../server/openapi.json");

/// Flag names every command may carry already: the global flags, the flags a body or a list
/// adds, and clap's own.
pub const RESERVED_FLAGS: [&str; 11] = [
    "profile",
    "api-key",
    "api-url",
    "config",
    "json",
    "idempotency-key",
    "data",
    "content-type",
    "all",
    "help",
    "version",
];

/// Command names the built-in commands take.
pub const BUILT_IN_COMMANDS: [&str; 4] = ["login", "listen", "trigger", "help"];

/// The keys under which an OpenAPI path item holds its operations.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// How a reference to a component schema begins.
const SCHEMA_REF: &str = "#/components/schemas/";

/// The largest number of references followed to resolve one schema; the document never nests
/// that deep, and the bound stops a reference cycle.
const MAX_REFERENCES: u8 = 8;

/// Why the document could not be turned into commands.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// The embedded document is not JSON.
    #[error("the embedded OpenAPI document is not JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// An operation's id is not `<resource>.<action>`.
    #[error("the operation `{method} {path}` has no `<resource>.<action>` id")]
    OperationId { method: String, path: String },
    /// Two operations would be the same command.
    #[error("two operations are both `{resource} {action}`")]
    Duplicate { resource: String, action: String },
    /// A resource takes the name of a built-in command.
    #[error("the resource `{resource}` takes the name of a built-in command")]
    ReservedResource { resource: String },
    /// A command would have two flags of one name, or one of a name every command has.
    #[error("`{operation}` would have the flag `--{flag}` twice")]
    Collision { operation: String, flag: String },
}

/// One public operation as a command.
#[derive(Debug, Clone)]
pub struct Operation {
    /// The operation id, `<resource>.<action>`.
    pub id: String,
    /// The command group, the part of the id before the dot.
    pub resource: String,
    /// The command, the part of the id after the dot.
    pub action: String,
    /// The HTTP method.
    pub method: Method,
    /// The path template, such as `/v1/people/{id}`.
    pub path: String,
    /// The operation's summary and description, for the command's help.
    pub summary: String,
    /// The path parameters, in the order of the path: the command's positional arguments.
    pub positionals: Vec<Positional>,
    /// Query parameters, header parameters and JSON body members.
    pub flags: Vec<Flag>,
    /// The request body the operation takes, if any.
    pub body: Option<Body>,
    /// Whether the operation is a list that `--all` can follow to its end.
    pub paginated: bool,
}

/// A path parameter.
#[derive(Debug, Clone)]
pub struct Positional {
    /// The parameter's name in the path template.
    pub name: String,
    /// Its description.
    pub help: String,
}

/// A flag: a query parameter, a header or a JSON body member, or a query parameter and a body
/// member of one name.
#[derive(Debug, Clone)]
pub struct Flag {
    /// The flag's name without its dashes.
    pub name: String,
    /// Its help: the parameter's or member's description, with the allowed values of an enum and
    /// whether the body requires it.
    pub help: String,
    /// What one value is.
    pub kind: Kind,
    /// Whether the flag repeats, one array item per occurrence.
    pub repeated: bool,
    /// Whether clap requires it: a required query or header parameter. A required body member
    /// is not, since `--data` may carry it and the server names every missing member in one
    /// answer.
    pub required: bool,
    /// The query parameter it sets.
    pub query: Option<String>,
    /// The header it sets.
    pub header: Option<String>,
    /// The JSON body member it sets.
    pub member: Option<String>,
}

/// What a flag's value is, and so how it is written into a JSON body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A JSON string, taken as written.
    Text,
    /// A JSON integer.
    Integer,
    /// A JSON number.
    Number,
    /// `true` or `false`; the flag alone means `true`.
    Boolean,
    /// JSON text, for objects and anything without a simpler shape.
    Json,
}

/// The request body of an operation.
#[derive(Debug, Clone)]
pub struct Body {
    /// The media types it may be sent as, `application/json` first when it is one of them.
    pub media_types: Vec<String>,
    /// Whether the operation requires a body.
    pub required: bool,
}

/// Whether a media type is JSON.
pub fn is_json(media_type: &str) -> bool {
    media_type.eq_ignore_ascii_case("application/json")
}

/// The operations of the embedded document.
///
/// # Errors
///
/// When the document breaks one of the invariants above; a test derives it, so a release never
/// does.
pub fn embedded() -> Result<Vec<Operation>, SpecError> {
    operations(&serde_json::from_str(DOCUMENT)?)
}

/// The operations of an OpenAPI document, in the order of its paths.
///
/// # Errors
///
/// When an operation has no `<resource>.<action>` id, two operations would be one command, a
/// resource takes a built-in command's name, or a command would have two flags of one name.
pub fn operations(document: &Value) -> Result<Vec<Operation>, SpecError> {
    let no_schemas = Map::new();
    let schemas = document
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .unwrap_or(&no_schemas);
    let mut operations = Vec::new();
    let mut commands = BTreeSet::new();
    let paths = document.get("paths").and_then(Value::as_object);
    for (path, item) in paths.into_iter().flatten() {
        for method in METHODS {
            let Some(operation) = item.get(method) else {
                continue;
            };
            let operation = derive(method, path, operation, schemas)?;
            if BUILT_IN_COMMANDS.contains(&operation.resource.as_str()) {
                return Err(SpecError::ReservedResource {
                    resource: operation.resource,
                });
            }
            if !commands.insert((operation.resource.clone(), operation.action.clone())) {
                return Err(SpecError::Duplicate {
                    resource: operation.resource,
                    action: operation.action,
                });
            }
            operations.push(operation);
        }
    }
    Ok(operations)
}

/// One operation as a command.
fn derive(
    method: &str,
    path: &str,
    operation: &Value,
    schemas: &Map<String, Value>,
) -> Result<Operation, SpecError> {
    let id = operation
        .get("operationId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let unnamed = || SpecError::OperationId {
        method: method.to_uppercase(),
        path: path.to_owned(),
    };
    let (resource, action) = id
        .split_once('.')
        .filter(|(resource, action)| {
            !resource.is_empty() && !action.is_empty() && !action.contains('.')
        })
        .ok_or_else(unnamed)?;
    let method = Method::from_bytes(method.to_uppercase().as_bytes()).map_err(|_| unnamed())?;

    let mut positionals = Vec::new();
    let mut flags: Vec<Flag> = Vec::new();
    let parameters = operation.get("parameters").and_then(Value::as_array);
    for parameter in parameters.into_iter().flatten() {
        let Some(name) = parameter.get("name").and_then(Value::as_str) else {
            continue;
        };
        let place = parameter.get("in").and_then(Value::as_str);
        let help = Some(text(parameter.get("description")))
            .filter(|help| !help.is_empty())
            .unwrap_or_else(|| format!("The `{name}` {} parameter.", place.unwrap_or_default()));
        let required = parameter.get("required").and_then(Value::as_bool) == Some(true);
        let schema = parameter.get("schema").unwrap_or(&Value::Null);
        if place == Some("path") {
            positionals.push(Positional {
                name: name.to_owned(),
                help,
            });
            continue;
        }
        let (query, header) = match place {
            Some("query") => (Some(name.to_owned()), None),
            Some("header") if !name.eq_ignore_ascii_case("idempotency-key") => {
                (None, Some(name.to_owned()))
            }
            _ => continue,
        };
        let shape = shape(schema, schemas, 0);
        flags.push(Flag {
            name: flag_name(name),
            help: shape.describe(help, false),
            kind: shape.kind,
            repeated: shape.repeated,
            required,
            query,
            header,
            member: None,
        });
    }
    positionals.sort_by_key(|positional| path.find(&format!("{{{}}}", positional.name)));

    let body = operation.get("requestBody").map(|body| {
        let content = body.get("content").and_then(Value::as_object);
        let mut media_types: Vec<String> = content
            .into_iter()
            .flatten()
            .map(|(media_type, _)| media_type.clone())
            .collect();
        media_types.sort_by_key(|media_type| !is_json(media_type));
        Body {
            media_types,
            required: body.get("required").and_then(Value::as_bool) == Some(true),
        }
    });
    let json_schema = operation
        .pointer("/requestBody/content/application~1json/schema")
        .map(|schema| resolve(schema, schemas, 0));
    if let Some(schema) = json_schema {
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let members = schema.get("properties").and_then(Value::as_object);
        for (member, member_schema) in members.into_iter().flatten() {
            let name = flag_name(member);
            let name = if RESERVED_FLAGS.contains(&name.as_str()) {
                format!("body-{name}")
            } else {
                name
            };
            let shape = shape(member_schema, schemas, 0);
            let help = shape.describe(
                Some(description(member_schema))
                    .filter(|help| !help.is_empty())
                    .unwrap_or_else(|| format!("The body's `{member}`.")),
                required.contains(&member.as_str()),
            );
            match flags.iter_mut().find(|flag| flag.name == name) {
                // A query parameter of the same name: one flag, routed by the body's media type.
                Some(flag) if flag.query.is_some() && flag.member.is_none() => {
                    flag.member = Some(member.clone());
                    flag.kind = shape.kind;
                    flag.repeated = shape.repeated;
                    if flag.help.is_empty() {
                        flag.help = help;
                    }
                }
                Some(_) => {
                    return Err(SpecError::Collision {
                        operation: id.to_owned(),
                        flag: name,
                    });
                }
                None => flags.push(Flag {
                    name,
                    help,
                    kind: shape.kind,
                    repeated: shape.repeated,
                    required: false,
                    query: None,
                    header: None,
                    member: Some(member.clone()),
                }),
            }
        }
    }

    let mut names = BTreeSet::new();
    for flag in &flags {
        if RESERVED_FLAGS.contains(&flag.name.as_str()) || !names.insert(flag.name.as_str()) {
            return Err(SpecError::Collision {
                operation: id.to_owned(),
                flag: flag.name.clone(),
            });
        }
    }
    let paginated = flags
        .iter()
        .any(|flag| flag.query.as_deref() == Some("cursor"));
    let summary = [operation.get("summary"), operation.get("description")]
        .into_iter()
        .map(text)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Operation {
        id: id.to_owned(),
        resource: resource.to_owned(),
        action: action.to_owned(),
        method,
        path: path.to_owned(),
        summary,
        positionals,
        flags,
        body,
        paginated,
    })
}

/// A parameter or member name as a flag: lower case, words joined by hyphens, brackets
/// dropped (`created_at[gte]` is `created-at-gte`, `If-Match` is `if-match`).
pub fn flag_name(name: &str) -> String {
    name.chars()
        .filter(|character| *character != ']')
        .map(|character| match character {
            '_' | '[' => '-',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// A schema's description: its own, else that of its first `oneOf` or `anyOf` variant that has
/// one, which is where the document describes a nullable member.
fn description(schema: &Value) -> String {
    let own = text(schema.get("description"));
    if !own.is_empty() {
        return own;
    }
    ["oneOf", "anyOf"]
        .into_iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_array))
        .flatten()
        .map(|variant| text(variant.get("description")))
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// A description on one line: the document wraps long ones.
fn text(description: Option<&Value>) -> String {
    description
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A schema with its references followed.
fn resolve<'a>(schema: &'a Value, schemas: &'a Map<String, Value>, depth: u8) -> &'a Value {
    let target = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix(SCHEMA_REF))
        .and_then(|name| schemas.get(name));
    match target {
        Some(target) if depth < MAX_REFERENCES => resolve(target, schemas, depth + 1),
        _ => schema,
    }
}

/// What a schema's values look like on the command line.
struct Shape {
    kind: Kind,
    repeated: bool,
    /// The allowed values of an enum (of its items, for an array).
    values: Vec<String>,
}

impl Shape {
    /// The flag's help: the description, then the allowed values, the repetition and whether
    /// the body requires it.
    fn describe(&self, description: String, required: bool) -> String {
        let mut help = description;
        let mut add = |sentence: String| {
            if !help.is_empty() {
                help.push(' ');
            }
            help.push_str(&sentence);
        };
        if !self.values.is_empty() {
            add(format!("One of: {}.", self.values.join(", ")));
        }
        if self.repeated {
            add("Repeat the flag for each value.".to_owned());
        } else if self.kind == Kind::Json {
            add("JSON text.".to_owned());
        }
        if required {
            add("Required in the body.".to_owned());
        }
        help
    }
}

/// The shape of a schema's values: its type, after references and a nullable `oneOf`.
fn shape(schema: &Value, schemas: &Map<String, Value>, depth: u8) -> Shape {
    let schema = resolve(schema, schemas, 0);
    let single = |kind| Shape {
        kind,
        repeated: false,
        values: Vec::new(),
    };
    let variant = ["oneOf", "anyOf"]
        .into_iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_array))
        .flatten()
        .find(|variant| variant.get("type").and_then(Value::as_str) != Some("null"));
    if let Some(variant) = variant
        && depth < MAX_REFERENCES
    {
        return shape(variant, schemas, depth + 1);
    }
    let kind = match schema.get("type") {
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null"),
        _ => None,
    };
    let values = schema.get("enum").and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    match kind {
        Some("string") => Shape {
            kind: Kind::Text,
            repeated: false,
            values: values.unwrap_or_default(),
        },
        Some("integer") => single(Kind::Integer),
        Some("number") => single(Kind::Number),
        Some("boolean") => single(Kind::Boolean),
        Some("array") => {
            let items = schema.get("items").unwrap_or(&Value::Null);
            let item = shape(items, schemas, depth.saturating_add(1));
            match item.repeated {
                // An array of arrays has no flag form: each occurrence is JSON text.
                true => Shape {
                    kind: Kind::Json,
                    repeated: true,
                    values: Vec::new(),
                },
                false => Shape {
                    repeated: true,
                    ..item
                },
            }
        }
        _ => match values {
            Some(values) if !values.is_empty() => Shape {
                kind: Kind::Text,
                repeated: false,
                values,
            },
            _ => single(Kind::Json),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{
        BUILT_IN_COMMANDS, Kind, RESERVED_FLAGS, SpecError, embedded, flag_name, operations,
    };

    /// Every operation of the embedded public document becomes exactly one command named after
    /// its id, none of them a dashboard operation, with no flag or resource clashing with the
    /// CLI's own names. This is the test that runs when the document is regenerated: a new
    /// operation that cannot be a command fails it before any release.
    #[test]
    fn every_public_operation_is_exactly_one_command() {
        let document: serde_json::Value = serde_json::from_str(super::DOCUMENT).unwrap();
        let mut ids = Vec::new();
        for item in document["paths"].as_object().unwrap().values() {
            for (_, operation) in item
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| super::METHODS.contains(&key.as_str()))
            {
                let tags = operation["tags"].as_array().cloned().unwrap_or_default();
                assert!(
                    !tags.contains(&json!("Dashboard")) && operation.get("x-surface").is_none(),
                    "the public document holds a dashboard operation: {operation}"
                );
                ids.push(operation["operationId"].as_str().unwrap().to_owned());
            }
        }
        let derived = embedded().unwrap();
        let mut commands = derived
            .iter()
            .map(|operation| format!("{}.{}", operation.resource, operation.action))
            .collect::<Vec<_>>();
        ids.sort();
        commands.sort();
        assert_eq!(commands, ids);
        assert!(!ids.is_empty());
        for operation in &derived {
            assert!(!BUILT_IN_COMMANDS.contains(&operation.resource.as_str()));
            let mut names = BTreeSet::new();
            for flag in &operation.flags {
                assert!(
                    !RESERVED_FLAGS.contains(&flag.name.as_str()),
                    "{}",
                    flag.name
                );
                assert!(
                    names.insert(flag.name.clone()),
                    "{} --{}",
                    operation.id,
                    flag.name
                );
            }
        }
    }

    /// The embedded document's own hard cases map as specified: path parameters are positional
    /// in path order, filters with brackets are kebab-case flags, lists take `--all`, an array
    /// member repeats, and the import's `group_id` is one flag for both the query parameter of a
    /// CSV upload and the member of a JSON body. These are the operations the CLI's users meet
    /// first, so a regression in them is a broken CLI.
    #[test]
    fn the_embedded_operations_map_their_parameters_as_specified() {
        let derived = embedded().unwrap();
        let find = |id: &str| derived.iter().find(|operation| operation.id == id).unwrap();

        let retrieve = find("people.retrieve");
        assert_eq!(retrieve.method, reqwest::Method::GET);
        assert_eq!(retrieve.path, "/v1/people/{id}");
        assert_eq!(retrieve.positionals.len(), 1);
        assert_eq!(retrieve.positionals[0].name, "id");

        let list = find("people.list");
        assert!(list.paginated && list.body.is_none());
        let after = list
            .flags
            .iter()
            .find(|flag| flag.name == "created-at-gte")
            .unwrap();
        assert_eq!(after.query.as_deref(), Some("created_at[gte]"));
        assert!(list.flags.iter().any(|flag| flag.name == "limit"));
        assert!(list.flags.iter().any(|flag| flag.name == "cursor"));

        let create = find("people.create");
        assert!(!create.paginated);
        let email = create
            .flags
            .iter()
            .find(|flag| flag.name == "email")
            .unwrap();
        assert_eq!(email.member.as_deref(), Some("email"));
        assert!(email.help.contains("Required in the body."));
        let groups = create
            .flags
            .iter()
            .find(|flag| flag.name == "group-ids")
            .unwrap();
        assert!(groups.repeated);
        assert_eq!(groups.kind, Kind::Text);
        let fields = create
            .flags
            .iter()
            .find(|flag| flag.name == "fields")
            .unwrap();
        assert_eq!(fields.kind, Kind::Json);

        let endpoint = find("webhook_endpoints.create");
        let types = endpoint
            .flags
            .iter()
            .find(|flag| flag.name == "event-types")
            .unwrap();
        assert!(types.repeated && types.help.contains("message.sent"));
        let enabled = endpoint
            .flags
            .iter()
            .find(|flag| flag.name == "enabled")
            .unwrap();
        assert_eq!(enabled.kind, Kind::Boolean);

        let import = find("imports.create");
        let body = import.body.as_ref().unwrap();
        assert_eq!(body.media_types, ["application/json", "text/csv"]);
        let group = import
            .flags
            .iter()
            .find(|flag| flag.name == "group-id")
            .unwrap();
        assert_eq!(group.query.as_deref(), Some("group_id"));
        assert_eq!(group.member.as_deref(), Some("group_id"));

        let rotate = find("webhook_endpoints.rotate_secret");
        assert_eq!(rotate.resource, "webhook_endpoints");
        assert_eq!(rotate.action, "rotate_secret");
    }

    /// The rules the embedded document does not exercise today, proven on a small document: a
    /// header parameter is a flag (and `Idempotency-Key` is not), a required query parameter is
    /// required, positionals follow the path's order whatever the parameters' order, a nullable
    /// `oneOf` takes its non-null type, and integers, numbers and enums keep their types.
    #[test]
    fn headers_types_and_positional_order_follow_the_schema() {
        let document = json!({
            "paths": {
                "/v1/things/{thing}/parts/{part}": {
                    "patch": {
                        "operationId": "parts.update",
                        "parameters": [
                            { "name": "part", "in": "path", "required": true },
                            { "name": "thing", "in": "path", "required": true },
                            { "name": "If-Match", "in": "header" },
                            { "name": "Idempotency-Key", "in": "header" },
                            { "name": "mode", "in": "query", "required": true, "schema": { "type": "string" } }
                        ],
                        "requestBody": { "required": true, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/UpdatePart" } } } }
                    }
                }
            },
            "components": { "schemas": {
                "UpdatePart": {
                    "type": "object",
                    "required": ["count"],
                    "properties": {
                        "count": { "type": "integer" },
                        "ratio": { "type": ["number", "null"] },
                        "color": { "oneOf": [{ "$ref": "#/components/schemas/Color" }, { "type": "null" }] },
                        "window": { "oneOf": [{ "$ref": "#/components/schemas/Window" }, { "type": "null" }] }
                    }
                },
                "Color": { "type": "string", "enum": ["red", "blue"] },
                "Window": { "type": "object", "properties": { "days": { "type": "array" } } }
            } }
        });
        let derived = operations(&document).unwrap();
        let operation = &derived[0];
        let positionals: Vec<_> = operation
            .positionals
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(positionals, ["thing", "part"]);
        let flag = |name: &str| operation.flags.iter().find(|flag| flag.name == name);
        assert_eq!(
            flag("if-match").unwrap().header.as_deref(),
            Some("If-Match")
        );
        assert!(flag("idempotency-key").is_none());
        assert!(flag("mode").unwrap().required);
        assert_eq!(flag("count").unwrap().kind, Kind::Integer);
        assert!(!flag("count").unwrap().required);
        assert_eq!(flag("ratio").unwrap().kind, Kind::Number);
        assert_eq!(flag("color").unwrap().kind, Kind::Text);
        assert!(flag("color").unwrap().help.contains("One of: red, blue."));
        assert_eq!(flag("window").unwrap().kind, Kind::Json);
    }

    /// Derivation refuses what would make an ambiguous command line: an id without a dot, two
    /// operations with one id, a resource named like a built-in command, and two members whose
    /// flags collide after reserved names are prefixed. Any would make a command ambiguous.
    #[test]
    fn ambiguous_documents_are_refused() {
        let one = |path: &str, method: &str, operation: serde_json::Value| json!({ "paths": { path: { method: operation } } });
        assert!(matches!(
            operations(&one("/v1/x", "get", json!({ "operationId": "x" }))),
            Err(SpecError::OperationId { .. })
        ));
        assert!(matches!(
            operations(&one(
                "/v1/login",
                "post",
                json!({ "operationId": "login.create" })
            )),
            Err(SpecError::ReservedResource { .. })
        ));
        let clash = json!({
            "operationId": "things.create",
            "requestBody": { "content": { "application/json": { "schema": {
                "type": "object", "properties": { "json": { "type": "boolean" }, "body_json": { "type": "boolean" } }
            } } } }
        });
        assert!(matches!(
            operations(&one("/v1/things", "post", clash)),
            Err(SpecError::Collision { .. })
        ));
        let duplicate = json!({ "paths": {
            "/v1/a": { "get": { "operationId": "things.list" } },
            "/v1/b": { "get": { "operationId": "things.list" } }
        } });
        assert!(matches!(
            operations(&duplicate),
            Err(SpecError::Duplicate { .. })
        ));
    }

    /// Flag names are the API's names in kebab case, brackets dropped, so a reader of the API
    /// reference can guess every flag.
    #[test]
    fn flag_names_are_kebab_case() {
        assert_eq!(flag_name("created_at[gte]"), "created-at-gte");
        assert_eq!(flag_name("If-Match"), "if-match");
        assert_eq!(flag_name("event_types"), "event-types");
        assert_eq!(flag_name("q"), "q");
    }
}
