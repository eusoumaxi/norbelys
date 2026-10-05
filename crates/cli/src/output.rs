//! What the CLI prints, and the exit code of each class of failure.
//!
//! Answers go to standard output: with `--json` the API's JSON as it is (pretty-printed), so
//! scripts read exactly what the API returned; otherwise a readable rendering, one `key: value`
//! per line with nested objects and lists indented, `id` first. A page of a list prints its
//! items, and a hint about the next page goes to standard error, so piping the items stays
//! clean. A resource that can be updated is answered with its `ETag` (its `version`, quoted),
//! which the readable rendering shows on standard error with the `--if-match` that changes only
//! that version. Messages for people (prompts, progress, errors) go to standard error.
//!
//! A problem (an RFC 9457 error answer, <https://www.rfc-editor.org/rfc/rfc9457>) is printed with
//! what each reader needs: the `code` a program branches on, the `detail` a person reads, every
//! invalid field with its pointer, and the `request_id` support asks for. With `--json` the
//! problem document itself is printed on standard output instead.
//!
//! The exit code tells a script what kind of failure happened without parsing text: one code per
//! [`Class`], listed in the help of `norbelys`.

use std::io::{self, Write};

use serde_json::{Map, Value};
use strum::IntoEnumIterator as _;

/// Where the CLI meets its user: the two output streams, and how a URL is opened in a browser.
pub struct Terminal<'a> {
    /// Answers: what a script reads.
    pub out: &'a mut dyn Write,
    /// Messages for a person: prompts, progress, errors, hints.
    pub err: &'a mut dyn Write,
    /// Opens a URL in the user's browser, as well as it can; tests pass one that does nothing.
    pub open: fn(&str),
}

/// A class of outcome, each with its own exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Class {
    /// A failure on this machine: the configuration file, a file to read, an answer the CLI
    /// cannot read.
    Failure,
    /// Arguments the CLI cannot turn into a request.
    Usage,
    /// No valid credential: not logged in, a session that ended, or `401`.
    Unauthenticated,
    /// The credential lacks a scope or the surface: `403`.
    Forbidden,
    /// No such resource in the workspace: `404`, `410`.
    NotFound,
    /// A conflict with the resource's state or version: `409`, `412`.
    Conflict,
    /// The request was refused as invalid: `400`, `413`, `415`, `422` and the other `4xx`.
    Invalid,
    /// Rate limited or unavailable for now; the same request may succeed later: `429`, `503`,
    /// `504`.
    RetryLater,
    /// The server failed: `500` and the other `5xx`.
    Server,
    /// The API could not be reached, or its answer was lost.
    Unreachable,
}

impl Class {
    /// The process's exit code for this class; `0` is success.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Failure => 1,
            Self::Usage => 2,
            Self::Unauthenticated => 3,
            Self::Forbidden => 4,
            Self::NotFound => 5,
            Self::Conflict => 6,
            Self::Invalid => 7,
            Self::RetryLater => 8,
            Self::Server => 9,
            Self::Unreachable => 10,
        }
    }

    /// The class's meaning, as the help lists it.
    #[must_use]
    pub fn meaning(self) -> &'static str {
        match self {
            Self::Failure => "a local failure (the configuration, a file, an unreadable answer)",
            Self::Usage => "invalid arguments",
            Self::Unauthenticated => "no valid credential: not logged in, or 401",
            Self::Forbidden => "the credential lacks the scope or the surface (403)",
            Self::NotFound => "no such resource in this workspace (404, 410)",
            Self::Conflict => "a conflict with the resource's state or version (409, 412)",
            Self::Invalid => "the request was refused as invalid (400, 413, 415, 422)",
            Self::RetryLater => "rate limited or unavailable: retry later (429, 503, 504)",
            Self::Server => "the server failed (500 and other 5xx)",
            Self::Unreachable => "the API could not be reached",
        }
    }

    /// The class of an HTTP error status.
    #[must_use]
    pub fn of_status(status: u16) -> Self {
        match status {
            401 => Self::Unauthenticated,
            403 => Self::Forbidden,
            404 | 410 => Self::NotFound,
            409 | 412 => Self::Conflict,
            429 | 503 | 504 => Self::RetryLater,
            400..=499 => Self::Invalid,
            500..=599 => Self::Server,
            _ => Self::Failure,
        }
    }
}

/// The exit codes, as the help of `norbelys` lists them.
#[must_use]
pub fn exit_codes() -> String {
    let mut text = String::from("Exit codes:\n   0  success\n");
    for class in Class::iter() {
        text.push_str(&format!("  {:>2}  {}\n", class.code(), class.meaning()));
    }
    text
}

/// Prints an answer: its JSON with `json`, else its readable rendering; a page of a list as its
/// items, with a hint about the next page on standard error; a resource with its `ETag` on
/// standard error, as the value `--if-match` takes to change only that version. An answer
/// without a body prints nothing.
///
/// # Errors
///
/// When a stream cannot be written (a closed pipe).
pub fn answer(
    terminal: &mut Terminal<'_>,
    json: bool,
    body: Option<&Value>,
    etag: Option<&str>,
) -> io::Result<()> {
    let Some(body) = body else {
        return Ok(());
    };
    if json {
        return pretty(terminal.out, body);
    }
    match page_items(body) {
        Some(items) => {
            items_readable(terminal, items)?;
            let next = body
                .pointer("/meta/next_cursor")
                .and_then(Value::as_str)
                .filter(|_| body.pointer("/meta/has_more") == Some(&Value::Bool(true)));
            match next {
                Some(cursor) => writeln!(
                    terminal.err,
                    "More: --cursor {cursor} for the next page, or --all for every page."
                ),
                None => Ok(()),
            }
        }
        None => {
            terminal.out.write_all(render(body).as_bytes())?;
            match etag {
                Some(etag) => writeln!(
                    terminal.err,
                    "ETag {etag}: --if-match {} changes only this version.",
                    etag.trim_matches('"')
                ),
                None => Ok(()),
            }
        }
    }
}

/// Prints the items of a page in the readable rendering, as `--all` does page after page.
///
/// # Errors
///
/// When standard output cannot be written.
pub fn items_readable(terminal: &mut Terminal<'_>, items: &[Value]) -> io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    terminal
        .out
        .write_all(render(&Value::Array(items.to_vec())).as_bytes())
}

/// Prints a value as pretty JSON and a newline.
///
/// # Errors
///
/// When the stream cannot be written.
pub fn pretty(out: &mut dyn Write, value: &Value) -> io::Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    writeln!(out, "{text}")
}

/// The items of a list page (`{ "data": [...], "meta": {...} }`), if `body` is one.
fn page_items(body: &Value) -> Option<&[Value]> {
    body.get("meta")?;
    body.get("data")?.as_array().map(Vec::as_slice)
}

/// The readable rendering of a JSON value: `key: value` lines, nested objects and lists indented
/// by two spaces, list items after `- `, an object's `id` first.
#[must_use]
pub fn render(value: &Value) -> String {
    let mut text = String::new();
    write_value(&mut text, value, 0);
    text
}

fn write_value(text: &mut String, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(members) if !members.is_empty() => {
            for (key, member) in ordered(members) {
                text.push_str(&pad);
                text.push_str(key);
                text.push(':');
                if is_block(member) {
                    text.push('\n');
                    write_value(text, member, indent + 2);
                } else {
                    text.push(' ');
                    text.push_str(&scalar(member, indent + 2));
                    text.push('\n');
                }
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                text.push_str(&pad);
                text.push_str("- ");
                if is_block(item) {
                    let mut nested = String::new();
                    write_value(&mut nested, item, indent + 2);
                    text.push_str(nested.trim_start_matches(' '));
                } else {
                    text.push_str(&scalar(item, indent + 2));
                    text.push('\n');
                }
            }
        }
        other => {
            text.push_str(&pad);
            text.push_str(&scalar(other, indent));
            text.push('\n');
        }
    }
}

/// Whether a value is rendered on lines of its own: a non-empty object or list.
fn is_block(value: &Value) -> bool {
    match value {
        Value::Object(members) => !members.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// A value on one line; a string as written, its later lines indented under the first.
fn scalar(value: &Value, indent: usize) -> String {
    match value {
        Value::String(text) => text.replace('\n', &format!("\n{}", " ".repeat(indent))),
        Value::Object(_) => "{}".to_owned(),
        Value::Array(_) => "[]".to_owned(),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
    }
}

/// An object's members, `id` first and the others in the map's order.
fn ordered(members: &Map<String, Value>) -> impl Iterator<Item = (&String, &Value)> {
    let id = members.iter().filter(|(key, _)| *key == "id");
    id.chain(members.iter().filter(|(key, _)| *key != "id"))
}

/// A problem as people read it: `code (status): detail`, then each invalid field as
/// `pointer: code: detail`, the wait a retry needs, and the request id. An answer that is not a
/// problem document shows its status and the start of its body.
#[must_use]
pub fn problem(status: u16, body: &Value, request_id: Option<&str>, excerpt: &str) -> String {
    let field = |name: &str| body.get(name).and_then(Value::as_str);
    let detail = field("detail").or_else(|| field("title"));
    let mut text = match (field("code"), detail) {
        (Some(code), Some(detail)) => format!("{code} ({status}): {detail}"),
        (Some(code), None) => format!("{code} ({status})"),
        (None, _) if excerpt.trim().is_empty() => format!("the API answered {status}"),
        (None, _) => format!("the API answered {status}: {}", excerpt.trim()),
    };
    let errors = body.get("errors").and_then(Value::as_array);
    for error in errors.into_iter().flatten() {
        let part = |name: &str| error.get(name).and_then(Value::as_str).unwrap_or_default();
        text.push_str(&format!(
            "\n  {}: {}: {}",
            part("pointer"),
            part("code"),
            part("detail")
        ));
    }
    if let Some(seconds) = body.get("retry_after").and_then(Value::as_u64) {
        text.push_str(&format!("\n  retry after: {seconds} s"));
    }
    if let Some(id) = field("request_id").or(request_id) {
        text.push_str(&format!("\n  request id: {id}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::{Class, exit_codes, problem, render};

    /// Every class has its own non-zero exit code and is listed in the help, so a script can
    /// tell each outcome apart and a person can look it up; a new class fails here until it has
    /// a code and a meaning.
    #[test]
    fn every_class_has_its_own_documented_exit_code() {
        let help = exit_codes();
        let mut codes = BTreeSet::new();
        for class in Class::iter() {
            assert!(class.code() > 0);
            assert!(codes.insert(class.code()), "{class:?} reuses a code");
            assert!(
                help.contains(&format!("{:>2}  {}", class.code(), class.meaning())),
                "{class:?} is missing from the help"
            );
        }
    }

    /// HTTP statuses map to the class a script would act on: retry later on `429`, `503` and
    /// `504` only; log in again on `401`; every other `4xx` is a refused request and every other
    /// `5xx` a server failure.
    #[test]
    fn statuses_map_to_their_class() {
        for (status, class) in [
            (400, Class::Invalid),
            (401, Class::Unauthenticated),
            (403, Class::Forbidden),
            (404, Class::NotFound),
            (405, Class::Invalid),
            (409, Class::Conflict),
            (410, Class::NotFound),
            (412, Class::Conflict),
            (413, Class::Invalid),
            (415, Class::Invalid),
            (422, Class::Invalid),
            (429, Class::RetryLater),
            (500, Class::Server),
            (502, Class::Server),
            (503, Class::RetryLater),
            (504, Class::RetryLater),
        ] {
            assert_eq!(Class::of_status(status), class, "{status}");
        }
        for status in 400..=599 {
            assert_ne!(Class::of_status(status), Class::Failure, "{status}");
        }
    }

    /// A problem shows its code, status and detail, every invalid field with its pointer, the
    /// retry wait and the request id, which is everything a person or support needs from it;
    /// an answer that is no problem document shows its status and the start of its body.
    #[test]
    fn problems_show_code_detail_fields_and_request_id() {
        let body = json!({
            "type": "https://docs.norbelys.com/errors/validation_failed",
            "title": "Validation failed",
            "status": 422,
            "code": "validation_failed",
            "detail": "The body is invalid.",
            "errors": [
                { "pointer": "/email", "code": "format", "detail": "not an email address" },
                { "pointer": "?limit", "code": "range", "detail": "1 to 100" }
            ],
            "request_id": "req_123"
        });
        assert_eq!(
            problem(422, &body, None, ""),
            "validation_failed (422): The body is invalid.\n  \
             /email: format: not an email address\n  \
             ?limit: range: 1 to 100\n  \
             request id: req_123"
        );
        let limited = json!({ "code": "rate_limited", "detail": "Slow down.", "retry_after": 3 });
        assert_eq!(
            problem(429, &limited, Some("req_9"), ""),
            "rate_limited (429): Slow down.\n  retry after: 3 s\n  request id: req_9"
        );
        assert_eq!(
            problem(
                502,
                &serde_json::Value::Null,
                None,
                "<html>bad gateway</html>"
            ),
            "the API answered 502: <html>bad gateway</html>"
        );
    }

    /// The readable rendering puts `id` first, indents nested objects and list items, and keeps
    /// empty containers and nulls on their key's line, so a person scans a resource top-down.
    #[test]
    fn values_render_as_indented_lines_with_the_id_first() {
        let value = json!({
            "email": "ada@example.com",
            "fields": { "industry": "Computing" },
            "group_ids": [],
            "id": "per_1",
            "identities": [{ "email": "a@example.com", "id": "sid_1" }, { "email": "b@example.com", "id": "sid_2" }],
            "tags": ["x", "y"],
            "replied_at": null
        });
        assert_eq!(
            render(&value),
            "id: per_1\n\
             email: ada@example.com\n\
             fields:\n  industry: Computing\n\
             group_ids: []\n\
             identities:\n  - id: sid_1\n    email: a@example.com\n  - id: sid_2\n    email: b@example.com\n\
             replied_at: null\n\
             tags:\n  - x\n  - y\n"
        );
    }
}
