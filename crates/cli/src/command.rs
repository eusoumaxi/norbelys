//! The command line: the global flags, the built-in commands (`login`, `listen`, `trigger`),
//! one command per public operation, and the translation of a parsed operation command into
//! an HTTP request.
//!
//! The global flags work before or after the command (`norbelys --json people list` and
//! `norbelys people list --json` are one command line). The command of an operation has the
//! positionals and flags `spec` derives, and:
//!
//! - `--data` (`-d`) when the operation takes a body: JSON text, `@file` to read a file or `@-`
//!   to read standard input. Flags set members on top of it, so `-d @person.json --email …`
//!   sends the file with another address; `--data` is also how a member is set to `null`.
//! - `--content-type` when the body may be sent as more than one media type (an import as JSON
//!   or as a CSV file); JSON by default.
//! - `--all` on a list, to follow `next_cursor` to the last page.
//! - `--if-match` on an update takes the `version` the resource showed: it is sent quoted, as
//!   the entity tag the API compares (`"1790000000000000"`), so a change made meanwhile by
//!   someone else answers `412` instead of being overwritten. `*` and a tag already quoted are
//!   sent as written.
//!
//! A boolean flag alone means `true`; `--enabled=false` sets `false` (the `=` is required, so a
//! positional after the flag is never taken for its value).

use std::io::Read as _;

use clap::builder::PossibleValuesParser;
use clap::{Arg, ArgAction, ArgMatches, Command};
use reqwest::Method;
use serde_json::{Map, Value};

use crate::output;
use crate::spec::{self, Flag, Kind, Operation};

/// A command line that cannot become a request. It exits with the code of clap's own usage
/// errors.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

/// One HTTP request to the API, relative to its base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// The path's segments, not percent-encoded: `["v1", "people", "per_…"]`.
    pub path: Vec<String>,
    /// The query parameters in order; a repeated flag gives several pairs of one name.
    pub query: Vec<(String, String)>,
    /// Headers from header parameters (`If-Match`).
    pub headers: Vec<(String, String)>,
    pub body: Option<Payload>,
}

/// A request body with its media type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl Request {
    /// A request for `path` (such as `/v1/events`) without parameters or body.
    #[must_use]
    pub fn new(method: Method, path: &str) -> Self {
        Self {
            method,
            path: path
                .split('/')
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
                .collect(),
            query: Vec::new(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Sets a query parameter, replacing every earlier value of it.
    pub fn set_query(&mut self, name: &str, value: &str) {
        self.query.retain(|(key, _)| key != name);
        self.query.push((name.to_owned(), value.to_owned()));
    }

    /// Whether the request has an effect that a retry must not repeat: a `POST` or a `PATCH`,
    /// which take an `Idempotency-Key`.
    #[must_use]
    pub fn is_effectful(&self) -> bool {
        self.method == Method::POST || self.method == Method::PATCH
    }
}

/// The whole command line: global flags, built-in commands and one command per operation,
/// grouped by resource.
#[must_use]
pub fn root(operations: &[Operation]) -> Command {
    let mut root = Command::new("norbelys")
        .about(
            "The Norbelys API from the command line: one command per operation of the public \
             API (`norbelys <resource> <action>`), and `login`, `listen` and `trigger`.",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .after_help(output::exit_codes())
        .args(global_flags())
        .subcommand(
            Command::new("login").about(
                "Log in: approve this device in the browser (OAuth device authorization), or \
                 store the API key given with --api-key in the profile.",
            ),
        )
        .subcommand(
            Command::new("listen")
                .about(
                    "Forward the workspace's events to a local URL, signed like webhooks, until \
                     interrupted.",
                )
                .arg(
                    Arg::new("forward-to")
                        .long("forward-to")
                        .value_name("URL")
                        .required(true)
                        .help(
                            "Where each event is POSTed, such as `localhost:3000/hooks` \
                             (`http://` unless a scheme is given).",
                        ),
                ),
        )
        .subcommand(
            Command::new("trigger")
                .about("Create a synthetic event of a type, delivered like a real one.")
                .arg(
                    Arg::new("type")
                        .value_name("EVENT_TYPE")
                        .required(true)
                        .help("The event type, such as `message.sent`."),
                )
                .arg(
                    Arg::new("webhook-endpoint-id")
                        .long("webhook-endpoint-id")
                        .value_name("ID")
                        .help("Address it to this endpoint only, whatever its subscriptions (`whe_…`)."),
                ),
        );
    let mut resources: Vec<&str> = operations
        .iter()
        .map(|operation| operation.resource.as_str())
        .collect();
    resources.sort_unstable();
    resources.dedup();
    for resource in resources {
        let actions: Vec<&Operation> = operations
            .iter()
            .filter(|operation| operation.resource == resource)
            .collect();
        let names: Vec<&str> = actions
            .iter()
            .map(|operation| operation.action.as_str())
            .collect();
        root = root.subcommand(
            Command::new(resource.to_owned())
                .about(names.join(", "))
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommands(actions.into_iter().map(operation_command)),
        );
    }
    root
}

/// The help heading of the global flags, so a command's own flags come first in its help.
const GLOBAL: &str = "Global options";

/// The global flags, accepted before or after any command.
fn global_flags() -> [Arg; 6] {
    [
        Arg::new("profile")
            .long("profile")
            .value_name("NAME")
            .default_value("default")
            .global(true)
            .help_heading(GLOBAL)
            .help("The profile whose API URL and credential to use."),
        Arg::new("api-key")
            .long("api-key")
            .value_name("KEY")
            .env("NORBELYS_API_KEY")
            .hide_env_values(true)
            .global(true)
            .help_heading(GLOBAL)
            .help(
                "An API key (`nb_live_…`, `nb_test_…`) to use instead of the profile's \
                 credential; with `login`, stored in the profile.",
            ),
        Arg::new("api-url")
            .long("api-url")
            .value_name("URL")
            .global(true)
            .help_heading(GLOBAL)
            .help("The API's base URL: the profile's by default, else https://api.norbelys.com."),
        Arg::new("config")
            .long("config")
            .value_name("FILE")
            .global(true)
            .help_heading(GLOBAL)
            .help(
                "The configuration file: `$XDG_CONFIG_HOME/norbelys/config.json` by default, \
                 else `~/.config/norbelys/config.json`.",
            ),
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .global(true)
            .help_heading(GLOBAL)
            .help("Print the API's JSON as it is, instead of a readable rendering."),
        Arg::new("idempotency-key")
            .long("idempotency-key")
            .value_name("KEY")
            .global(true)
            .help_heading(GLOBAL)
            .help(
                "The `Idempotency-Key` of a POST or PATCH; a new one for each run by default. \
                 Run a request again with the same key to apply it at most once.",
            ),
    ]
}

/// The command of one operation.
fn operation_command(operation: &Operation) -> Command {
    let about = operation
        .summary
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut command = Command::new(operation.action.clone())
        .about(about)
        .long_about(format!(
            "{}\n\nThe operation `{}`: {} {}",
            operation.summary, operation.id, operation.method, operation.path
        ));
    for positional in &operation.positionals {
        command = command.arg(
            Arg::new(positional_id(&positional.name))
                .value_name(positional.name.to_uppercase())
                .required(true)
                .help(positional.help.clone()),
        );
    }
    for flag in &operation.flags {
        command = command.arg(flag_arg(flag));
    }
    if let Some(body) = &operation.body {
        command = command.arg(
            Arg::new("data")
                .short('d')
                .long("data")
                .value_name("BODY")
                .help(
                    "The request body: JSON text, `@file` to read a file, `@-` to read standard \
                     input. Flags set members on top of it; it is also how a member is set to \
                     null.",
                ),
        );
        if let (true, Some(default)) = (body.media_types.len() > 1, body.media_types.first()) {
            command = command.arg(
                Arg::new("content-type")
                    .long("content-type")
                    .value_name("MEDIA_TYPE")
                    .value_parser(PossibleValuesParser::new(body.media_types.clone()))
                    .default_value(default.clone())
                    .help("The media type of --data."),
            );
        }
    }
    if operation.paginated {
        command = command.arg(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Follow `next_cursor` to the last page."),
        );
    }
    command
}

/// The clap id of a path parameter: apart from the flags' ids, so a body member named like a
/// path parameter (`id`) cannot clash with it.
fn positional_id(name: &str) -> String {
    format!("path.{name}")
}

/// The clap argument of a derived flag.
fn flag_arg(flag: &Flag) -> Arg {
    let value_name = match flag.kind {
        Kind::Text => "VALUE",
        Kind::Integer => "INTEGER",
        Kind::Number => "NUMBER",
        Kind::Boolean => "BOOL",
        Kind::Json => "JSON",
    };
    let help = match flag.header.as_deref() {
        Some(name) if name.eq_ignore_ascii_case("if-match") => {
            format!("{} A bare version is quoted for you.", flag.help)
        }
        _ => flag.help.clone(),
    };
    let arg = Arg::new(flag.name.clone())
        .long(flag.name.clone())
        .value_name(value_name)
        .required(flag.required)
        .help(help);
    match (flag.repeated, flag.kind) {
        (true, _) => arg.action(ArgAction::Append),
        (false, Kind::Boolean) => arg
            .action(ArgAction::Set)
            .num_args(0..=1)
            .require_equals(true)
            .default_missing_value("true"),
        (false, Kind::Text | Kind::Integer | Kind::Number | Kind::Json) => {
            arg.action(ArgAction::Set)
        }
    }
}

/// A string argument's value, if the command has the argument and it was given (or defaulted).
pub fn value<'a>(matches: &'a ArgMatches, id: &str) -> Option<&'a String> {
    matches.try_get_one::<String>(id).ok().flatten()
}

/// The request a parsed operation command describes.
///
/// # Errors
///
/// [`UsageError`] when a value does not fit its flag's type, `--data` cannot be read or is not
/// JSON where JSON is needed, or a body member is given for a body that is not JSON.
pub fn request(operation: &Operation, matches: &ArgMatches) -> Result<Request, UsageError> {
    let content_type = value(matches, "content-type")
        .cloned()
        .or_else(|| operation.body.as_ref()?.media_types.first().cloned());
    let json_body = content_type.as_deref().is_some_and(spec::is_json);

    let mut path = Vec::new();
    for segment in operation
        .path
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        match segment
            .strip_prefix('{')
            .and_then(|name| name.strip_suffix('}'))
        {
            Some(name) => path.push(
                value(matches, &positional_id(name))
                    .cloned()
                    .ok_or_else(|| UsageError(format!("the argument <{name}> is required")))?,
            ),
            None => path.push(segment.to_owned()),
        }
    }

    let mut query = Vec::new();
    let mut headers = Vec::new();
    let mut members = Map::new();
    for flag in &operation.flags {
        let Some(values) = matches.try_get_many::<String>(&flag.name).ok().flatten() else {
            continue;
        };
        let values: Vec<&String> = values.collect();
        match (&flag.member, &flag.query, &flag.header) {
            (Some(member), _, _) if json_body => {
                members.insert(member.clone(), member_value(flag, &values)?);
            }
            (_, Some(name), _) => {
                query.extend(values.iter().map(|value| (name.clone(), (*value).clone())));
            }
            (_, _, Some(name)) if name.eq_ignore_ascii_case("if-match") => {
                headers.extend(
                    values
                        .iter()
                        .map(|value| (name.clone(), entity_tags(value))),
                );
            }
            (_, _, Some(name)) => {
                headers.extend(values.iter().map(|value| (name.clone(), (*value).clone())));
            }
            (Some(_), None, None) | (None, None, None) => {
                return Err(UsageError(format!(
                    "--{} sets a member of a JSON body; with --content-type {} put it in the \
                     file instead",
                    flag.name,
                    content_type.as_deref().unwrap_or_default()
                )));
            }
        }
    }

    let data = value(matches, "data")
        .map(|data| read_body(data))
        .transpose()?;
    let body = match (&operation.body, content_type) {
        (Some(body), Some(content_type)) if json_body => {
            if data.is_none() && members.is_empty() && !body.required {
                None
            } else {
                let mut json = match data {
                    Some(bytes) => serde_json::from_slice(&bytes)
                        .map_err(|error| UsageError(format!("--data is not JSON: {error}")))?,
                    None => Value::Object(Map::new()),
                };
                if !members.is_empty() {
                    let Value::Object(object) = &mut json else {
                        return Err(UsageError(
                            "--data must be a JSON object to be combined with flags".to_owned(),
                        ));
                    };
                    object.extend(members);
                }
                Some(Payload {
                    content_type,
                    bytes: json.to_string().into_bytes(),
                })
            }
        }
        (Some(body), Some(content_type)) => match data {
            Some(bytes) => Some(Payload {
                content_type,
                bytes,
            }),
            None if body.required => {
                return Err(UsageError(format!(
                    "a {content_type} body is required: pass it with --data @file"
                )));
            }
            None => None,
        },
        (Some(_), None) | (None, _) => None,
    };

    Ok(Request {
        method: operation.method.clone(),
        path,
        query,
        headers,
        body,
    })
}

/// An `If-Match` value with each bare version quoted as the entity tag it is (RFC 9110 §8.8.3,
/// <https://www.rfc-editor.org/rfc/rfc9110#section-8.8.3>): `--if-match 1790000000000000`, the
/// `version` a resource shows, is sent as `"1790000000000000"`, the `ETag` that carried it. `*`
/// and tags already quoted (or weak, `W/"…"`) are sent as written.
fn entity_tags(value: &str) -> String {
    if value.trim() == "*" {
        return value.trim().to_owned();
    }
    value
        .split(',')
        .map(str::trim)
        .map(|tag| match tag.starts_with('"') || tag.starts_with("W/") {
            true => tag.to_owned(),
            false => format!("\"{tag}\""),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The JSON value of a body member's flag: its values typed by the flag's kind, an array for a
/// repeated flag.
fn member_value(flag: &Flag, values: &[&String]) -> Result<Value, UsageError> {
    let one = |raw: &str| -> Result<Value, UsageError> {
        let invalid = |expected: &str| {
            UsageError(format!(
                "invalid value `{raw}` for --{}: expected {expected}",
                flag.name
            ))
        };
        Ok(match flag.kind {
            Kind::Text => Value::String(raw.to_owned()),
            Kind::Integer => Value::from(raw.parse::<i64>().map_err(|_| invalid("an integer"))?),
            Kind::Number => raw
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| invalid("a number"))?,
            Kind::Boolean => Value::Bool(raw.parse().map_err(|_| invalid("true or false"))?),
            Kind::Json => serde_json::from_str(raw).map_err(|_| invalid("JSON text"))?,
        })
    };
    match (flag.repeated, values.last()) {
        (true, _) => values
            .iter()
            .map(|value| one(value))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        (false, Some(last)) => one(last),
        (false, None) => Ok(Value::Null),
    }
}

/// The bytes of `--data`: the text itself, a file (`@path`) or standard input (`@-`).
fn read_body(data: &str) -> Result<Vec<u8>, UsageError> {
    match data.strip_prefix('@') {
        Some("-") => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|error| UsageError(format!("cannot read standard input: {error}")))?;
            Ok(bytes)
        }
        Some(path) => std::fs::read(path)
            .map_err(|error| UsageError(format!("cannot read `{path}`: {error}"))),
        None => Ok(data.as_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use clap::ArgMatches;
    use serde_json::json;

    use super::{Request, request, root};
    use crate::spec::{Operation, embedded};
    use crate::testing::Scratch;

    /// Parses `norbelys <args>` and builds the request of the operation command it names.
    fn parse(args: &[&str]) -> Result<Request, String> {
        let operations = embedded().map_err(|error| error.to_string())?;
        let matches = root(&operations)
            .try_get_matches_from(std::iter::once("norbelys").chain(args.iter().copied()))
            .map_err(|error| error.to_string())?;
        let (operation, leaf) = find(&operations, &matches);
        request(operation, leaf).map_err(|error| error.0)
    }

    fn find<'a>(
        operations: &'a [Operation],
        matches: &'a ArgMatches,
    ) -> (&'a Operation, &'a ArgMatches) {
        let (resource, group) = matches.subcommand().unwrap();
        let (action, leaf) = group.subcommand().unwrap();
        let operation = operations
            .iter()
            .find(|operation| operation.resource == resource && operation.action == action)
            .unwrap();
        (operation, leaf)
    }

    fn body(request: &Request) -> serde_json::Value {
        serde_json::from_slice(&request.body.as_ref().unwrap().bytes).unwrap()
    }

    /// Body flags become typed JSON members (a repeated flag an array, a JSON flag its value),
    /// path parameters fill the path, and filters with brackets keep their API names in the
    /// query: the three ways an operation's parameters travel, each as the server reads them.
    #[test]
    fn flags_and_positionals_become_the_request() {
        let created = parse(&[
            "people",
            "create",
            "--email",
            "ada@example.com",
            "--given-name",
            "Ada",
            "--group-ids",
            "grp_1",
            "--group-ids",
            "grp_2",
            "--fields",
            r#"{"tier":"gold"}"#,
        ])
        .unwrap();
        assert_eq!(created.method, reqwest::Method::POST);
        assert_eq!(created.path, ["v1", "people"]);
        assert!(created.is_effectful());
        assert_eq!(
            created.body.as_ref().unwrap().content_type,
            "application/json"
        );
        assert_eq!(
            body(&created),
            json!({ "email": "ada@example.com", "given_name": "Ada", "group_ids": ["grp_1", "grp_2"], "fields": { "tier": "gold" } })
        );

        let retrieved =
            parse(&["people", "retrieve", "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"]).unwrap();
        assert_eq!(
            retrieved.path,
            ["v1", "people", "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"]
        );
        assert!(retrieved.body.is_none() && !retrieved.is_effectful());

        let listed = parse(&[
            "people",
            "list",
            "--limit",
            "5",
            "--created-at-gte",
            "2026-10-01T00:00:00Z",
            "--json",
        ])
        .unwrap();
        assert_eq!(
            listed.query,
            [
                ("limit".to_owned(), "5".to_owned()),
                (
                    "created_at[gte]".to_owned(),
                    "2026-10-01T00:00:00Z".to_owned()
                )
            ]
        );
    }

    /// Integer and boolean members are sent as JSON numbers and booleans, a bare boolean flag is
    /// `true` and `=false` is `false`; a value that does not fit its type is refused before any
    /// request, naming the flag.
    #[test]
    fn values_are_typed_by_the_schema() {
        let created = parse(&[
            "webhook_endpoints",
            "create",
            "--url",
            "https://example.com/hooks",
            "--event-types",
            "message.sent",
            "--enabled=false",
        ])
        .unwrap();
        assert_eq!(
            body(&created),
            json!({ "url": "https://example.com/hooks", "event_types": ["message.sent"], "enabled": false })
        );
        let bare = parse(&["webhook_endpoints", "list", "--enabled"]).unwrap();
        assert_eq!(bare.query, [("enabled".to_owned(), "true".to_owned())]);
        let limit = parse(&[
            "connections",
            "create",
            "--provider",
            "smtp",
            "--daily-limit",
            "500",
        ])
        .unwrap();
        assert_eq!(body(&limit)["daily_limit"], json!(500));
        let refused = parse(&[
            "connections",
            "create",
            "--provider",
            "smtp",
            "--daily-limit",
            "many",
        ]);
        assert!(refused.unwrap_err().contains("--daily-limit"));
    }

    /// `--data` gives the whole body, from text or a file, and flags override its members; a
    /// non-object body cannot take flags. This is how nested values and nulls are sent.
    #[test]
    fn data_is_the_body_and_flags_override_it() {
        let scratch = Scratch::new();
        let file = scratch.dir.join("person.json");
        std::fs::write(&file, r#"{"email": "old@example.com", "given_name": null}"#).unwrap();
        let data = format!("@{}", file.display());
        let request = parse(&[
            "people",
            "create",
            "-d",
            &data,
            "--email",
            "new@example.com",
        ])
        .unwrap();
        assert_eq!(
            body(&request),
            json!({ "email": "new@example.com", "given_name": null })
        );
        let inline = parse(&[
            "people",
            "update",
            "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
            "--data",
            r#"{"company": null}"#,
        ])
        .unwrap();
        assert_eq!(inline.method, reqwest::Method::PATCH);
        assert_eq!(body(&inline), json!({ "company": null }));
        assert!(parse(&["people", "create", "-d", "[1]", "--email", "a@example.com"]).is_err());
        assert!(parse(&["people", "create", "-d", "{not json"]).is_err());
    }

    /// An import is sent as JSON with its group in the body, or as a CSV file with its group in
    /// the query, through one `--group-id` flag: the server reads the group from exactly one of
    /// the two places depending on the body, so the flag must follow the media type.
    #[test]
    fn the_import_group_follows_the_media_type() {
        let json = parse(&[
            "imports",
            "create",
            "-d",
            r#"{"people":[{"email":"a@example.com"}]}"#,
            "--group-id",
            "grp_1",
        ])
        .unwrap();
        assert!(json.query.is_empty());
        assert_eq!(body(&json)["group_id"], "grp_1");

        let scratch = Scratch::new();
        let file = scratch.dir.join("people.csv");
        std::fs::write(&file, "email\nada@example.com\n").unwrap();
        let data = format!("@{}", file.display());
        let csv = parse(&[
            "imports",
            "create",
            "--content-type",
            "text/csv",
            "-d",
            &data,
            "--group-id",
            "grp_1",
        ])
        .unwrap();
        assert_eq!(csv.query, [("group_id".to_owned(), "grp_1".to_owned())]);
        let payload = csv.body.unwrap();
        assert_eq!(payload.content_type, "text/csv");
        assert_eq!(payload.bytes, b"email\nada@example.com\n");

        let missing = parse(&["imports", "create", "--content-type", "text/csv"]);
        assert!(missing.unwrap_err().contains("--data"));
        let wrong = parse(&["imports", "create", "--content-type", "application/xml"]);
        assert!(wrong.is_err());
    }

    /// An attachment's media type is a JSON member, independent of the HTTP body's media type;
    /// the generated flag must not collide with the CLI's request selector.
    #[test]
    fn attachment_content_type_is_distinct_from_the_request_media_type() {
        let request = parse(&[
            "attachments",
            "create",
            "--body-content-type",
            "text/plain",
            "--filename",
            "notes.txt",
            "--content-base64",
            "SGVsbG8=",
        ])
        .unwrap();
        assert_eq!(
            body(&request),
            json!({"filename":"notes.txt", "content_type":"text/plain", "content_base64":"SGVsbG8="})
        );
        assert_eq!(request.body.unwrap().content_type, "application/json");
    }

    /// `--if-match` takes the `version` a resource shows and sends the quoted entity tag the API
    /// compares, while `*` and a tag already quoted go as written: an unquoted version never
    /// matches, so every guarded update would fail with `412`.
    #[test]
    fn if_match_takes_the_version_and_sends_its_entity_tag() {
        let id = "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4";
        for (given, sent) in [
            ("1790000000000000", "\"1790000000000000\""),
            ("\"1790000000000000\"", "\"1790000000000000\""),
            ("*", "*"),
            ("1, 2", "\"1\", \"2\""),
        ] {
            let request = parse(&[
                "people",
                "update",
                id,
                "--if-match",
                given,
                "--company",
                "X",
            ])
            .unwrap();
            assert_eq!(request.headers, [("If-Match".to_owned(), sent.to_owned())]);
        }
    }

    /// A body member named like a path parameter (`id`) is a flag beside the positional: clap
    /// refuses two arguments of one id when the command line is built, which would make every
    /// command of the CLI fail, not only this one.
    #[test]
    fn a_member_named_like_a_path_parameter_is_a_flag_of_its_own() {
        let document = json!({ "paths": { "/v1/things/{id}": { "patch": {
            "operationId": "things.update",
            "parameters": [{ "name": "id", "in": "path", "required": true }],
            "requestBody": { "required": true, "content": { "application/json": { "schema": {
                "type": "object", "properties": { "id": { "type": "string" } }
            } } } }
        } } } });
        let operations = crate::spec::operations(&document).unwrap();
        let matches = root(&operations)
            .try_get_matches_from(["norbelys", "things", "update", "thg_1", "--id", "thg_2"])
            .unwrap();
        let (operation, leaf) = find(&operations, &matches);
        let request = request(operation, leaf).unwrap();
        assert_eq!(request.path, ["v1", "things", "thg_1"]);
        assert_eq!(body(&request), json!({ "id": "thg_2" }));
    }

    /// Global flags are accepted before and after the command, and an operation's required
    /// positional is required by the parser, so a forgotten id is a usage error, not a request
    /// to the wrong path.
    #[test]
    fn global_flags_go_anywhere_and_positionals_are_required() {
        let operations = embedded().unwrap();
        for args in [
            ["norbelys", "--json", "--profile", "ci", "people", "list"],
            ["norbelys", "people", "list", "--json", "--profile", "ci"],
        ] {
            let matches = root(&operations).try_get_matches_from(args).unwrap();
            let (_, leaf) = find(&operations, &matches);
            assert!(leaf.get_flag("json"));
            assert_eq!(leaf.get_one::<String>("profile").unwrap(), "ci");
        }
        assert!(parse(&["people", "retrieve"]).is_err());
    }
}
