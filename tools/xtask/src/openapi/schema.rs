//! Validation of a JSON value against a schema of an OpenAPI 3.1 document, which is JSON Schema
//! (draft 2020-12) with the document's own components as the place `$ref` points into.
//!
//! This is the subset the API's documents use, checked strictly: `$ref` (JSON pointers into the
//! document), `allOf`, `anyOf`, `oneOf` and `not`; `type` (one or several, `integer` being a
//! number without a fraction) and OpenAPI 3.0's `nullable`; `enum` and `const`; for strings
//! `minLength`, `maxLength`, `pattern` and the formats `date-time`, `date`, `uuid`, `email` and
//! `uri`; for numbers `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` (numbers in
//! 3.1, flags in 3.0), `multipleOf` and the ranges of the integer formats `int32`, `int64`,
//! `uint32` and `uint64`; for arrays `items`, `prefixItems`, `minItems`, `maxItems` and
//! `uniqueItems`; for objects `required`, `properties`, `patternProperties`,
//! `additionalProperties`, `minProperties` and `maxProperties`. Other keywords (annotations such
//! as `description`, `default`, `readOnly`, `discriminator`) do not constrain a value and are
//! ignored.
//!
//! Patterns are compiled with the `regex` crate, whose syntax is ECMA-262's for everything an API
//! pattern needs; a pattern it cannot compile (look-around, back-references) is reported as a
//! finding rather than skipped, so an unchecked pattern is never mistaken for a passing one.
//! A type mismatch stops the checks of that schema, which would only repeat it.

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::{Map, Value};

/// How deep `$ref` and the combinators may nest before the schema is taken for a cycle.
const MAX_DEPTH: usize = 64;

/// The errors of `value` against `schema`, each prefixed with the location in the value
/// (`$`, `$.email`, `$.items[2]`); empty when the value is valid.
pub fn validate(document: &Value, schema: &Value, value: &Value) -> Vec<String> {
    let mut validator = Validator {
        document,
        patterns: BTreeMap::new(),
    };
    let mut errors = Vec::new();
    validator.check(schema, value, "$", 0, &mut errors);
    errors
}

/// The document being checked, and the patterns compiled so far.
struct Validator<'a> {
    document: &'a Value,
    patterns: BTreeMap<String, Option<Regex>>,
}

impl Validator<'_> {
    /// Appends to `errors` what is wrong with `value` (at `at`) under `schema`.
    fn check(
        &mut self,
        schema: &Value,
        value: &Value,
        at: &str,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        if depth > MAX_DEPTH {
            errors.push(format!(
                "{at}: the schema nests deeper than {MAX_DEPTH} levels"
            ));
            return;
        }
        let schema = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                errors.push(format!("{at}: no value is allowed here"));
                return;
            }
            Value::Object(schema) => schema,
            _ => {
                errors.push(format!(
                    "{at}: the schema is neither an object nor a boolean"
                ));
                return;
            }
        };
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            match resolve(self.document, reference) {
                Some(target) => self.check(target, value, at, depth + 1, errors),
                None => errors.push(format!("{at}: `$ref` {reference} points to nothing")),
            }
        }
        for sub in array(schema, "allOf") {
            self.check(sub, value, at, depth + 1, errors);
        }
        self.alternatives(schema, value, at, depth, errors);
        if let Some(not) = schema.get("not")
            && self.passes(not, value, depth)
        {
            errors.push(format!("{at}: matches the schema under `not`"));
        }
        if !self.types(schema, value, at, errors) {
            return;
        }
        if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
            && !allowed.contains(value)
        {
            errors.push(format!(
                "{at}: {value} is not one of {}",
                Value::Array(allowed.clone())
            ));
        }
        if let Some(constant) = schema.get("const")
            && constant != value
        {
            errors.push(format!("{at}: {value} is not {constant}"));
        }
        match value {
            Value::String(text) => self.string(schema, text, at, errors),
            Value::Number(_) => number(schema, value, at, errors),
            Value::Array(items) => self.items(schema, items, at, depth, errors),
            Value::Object(object) => self.object(schema, object, at, depth, errors),
            Value::Null | Value::Bool(_) => {}
        }
    }

    /// Whether `value` is valid under `schema`.
    fn passes(&mut self, schema: &Value, value: &Value, depth: usize) -> bool {
        let mut errors = Vec::new();
        self.check(schema, value, "$", depth + 1, &mut errors);
        errors.is_empty()
    }

    /// `anyOf` (at least one alternative) and `oneOf` (exactly one).
    fn alternatives(
        &mut self,
        schema: &Map<String, Value>,
        value: &Value,
        at: &str,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        for (keyword, exactly_one) in [("anyOf", false), ("oneOf", true)] {
            let alternatives = array(schema, keyword);
            if alternatives.is_empty() {
                continue;
            }
            let mut first_errors = Vec::new();
            let mut matched = 0_usize;
            for (index, alternative) in alternatives.iter().enumerate() {
                let mut branch = Vec::new();
                self.check(alternative, value, at, depth + 1, &mut branch);
                if branch.is_empty() {
                    matched += 1;
                } else if index == 0 {
                    first_errors = branch;
                }
            }
            if matched == 0 {
                let first = first_errors
                    .first()
                    .map_or_else(String::new, |error| format!(" (the first: {error})"));
                errors.push(format!(
                    "{at}: matches none of the {} alternatives of `{keyword}`{first}",
                    alternatives.len()
                ));
            } else if exactly_one && matched > 1 {
                errors.push(format!(
                    "{at}: matches {matched} alternatives of `oneOf`, where exactly one must match"
                ));
            }
        }
    }

    /// Checks `type` (and 3.0's `nullable`); false when the value has none of the allowed types.
    fn types(
        &self,
        schema: &Map<String, Value>,
        value: &Value,
        at: &str,
        errors: &mut Vec<String>,
    ) -> bool {
        let mut allowed: Vec<&str> = match schema.get("type") {
            Some(Value::String(name)) => vec![name.as_str()],
            Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
            _ => return true,
        };
        if schema.get("nullable") == Some(&Value::Bool(true)) {
            allowed.push("null");
        }
        if allowed.iter().any(|name| has_type(value, name)) {
            return true;
        }
        errors.push(format!(
            "{at}: {value} is not of type {}",
            allowed.join(" or ")
        ));
        false
    }

    /// Length, pattern and format of a string.
    fn string(
        &mut self,
        schema: &Map<String, Value>,
        text: &str,
        at: &str,
        errors: &mut Vec<String>,
    ) {
        let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
            && length < min
        {
            errors.push(format!("{at}: \"{text}\" is shorter than {min} characters"));
        }
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
            && length > max
        {
            errors.push(format!("{at}: \"{text}\" is longer than {max} characters"));
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            let compiled = self
                .patterns
                .entry(pattern.to_owned())
                .or_insert_with(|| Regex::new(pattern).ok());
            match compiled {
                Some(regex) if regex.is_match(text) => {}
                Some(_) => errors.push(format!("{at}: \"{text}\" does not match {pattern}")),
                None => errors.push(format!(
                    "{at}: the pattern {pattern} cannot be checked (not supported by the `regex` \
                     crate)"
                )),
            }
        }
        if let Some(format) = schema.get("format").and_then(Value::as_str)
            && let Some(valid) = string_format(format, text)
            && !valid
        {
            errors.push(format!("{at}: \"{text}\" is not a valid {format}"));
        }
    }

    /// Items of an array: `prefixItems`, `items`, counts and uniqueness.
    fn items(
        &mut self,
        schema: &Map<String, Value>,
        items: &[Value],
        at: &str,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        let prefix = array(schema, "prefixItems");
        for (index, item) in items.iter().enumerate() {
            let sub = prefix.get(index).or_else(|| schema.get("items"));
            if let Some(sub) = sub {
                self.check(sub, item, &format!("{at}[{index}]"), depth + 1, errors);
            }
        }
        let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
            && count < min
        {
            errors.push(format!("{at}: {count} items, fewer than {min}"));
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
            && count > max
        {
            errors.push(format!("{at}: {count} items, more than {max}"));
        }
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
            let duplicate = items
                .iter()
                .enumerate()
                .any(|(index, item)| items.iter().skip(index + 1).any(|other| other == item));
            if duplicate {
                errors.push(format!("{at}: the items are not unique"));
            }
        }
    }

    /// Members of an object: required names, the schemas of named and patterned members, the rest.
    fn object(
        &mut self,
        schema: &Map<String, Value>,
        object: &Map<String, Value>,
        at: &str,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        for name in array(schema, "required").iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                errors.push(format!("{at}: the required member `{name}` is missing"));
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        let patterned = schema.get("patternProperties").and_then(Value::as_object);
        for (name, member) in object {
            let place = format!("{at}.{name}");
            let mut described = false;
            if let Some(sub) = properties.and_then(|properties| properties.get(name)) {
                described = true;
                self.check(sub, member, &place, depth + 1, errors);
            }
            for (pattern, sub) in patterned.into_iter().flatten() {
                if Regex::new(pattern).is_ok_and(|regex| regex.is_match(name)) {
                    described = true;
                    self.check(sub, member, &place, depth + 1, errors);
                }
            }
            if !described {
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        errors.push(format!("{at}: the member `{name}` is not allowed"));
                    }
                    Some(sub @ Value::Object(_)) => {
                        self.check(sub, member, &place, depth + 1, errors);
                    }
                    _ => {}
                }
            }
        }
        let count = u64::try_from(object.len()).unwrap_or(u64::MAX);
        if let Some(min) = schema.get("minProperties").and_then(Value::as_u64)
            && count < min
        {
            errors.push(format!("{at}: {count} members, fewer than {min}"));
        }
        if let Some(max) = schema.get("maxProperties").and_then(Value::as_u64)
            && count > max
        {
            errors.push(format!("{at}: {count} members, more than {max}"));
        }
    }
}

/// The array under `keyword`, or an empty one.
fn array<'a>(schema: &'a Map<String, Value>, keyword: &str) -> &'a [Value] {
    schema
        .get(keyword)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The value a JSON pointer reference (`#/components/schemas/Person`) points to in `document`.
pub fn resolve<'a>(document: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    document.pointer(pointer)
}

/// Whether `value` has the JSON Schema type `name`.
fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        _ => false,
    }
}

/// Bounds of a number, and the range of its integer format.
fn number(schema: &Map<String, Value>, value: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(n) = value.as_f64() else {
        return;
    };
    let bound = |keyword: &str| schema.get(keyword).and_then(Value::as_f64);
    // OpenAPI 3.0 writes exclusive bounds as flags on `minimum` and `maximum`.
    let flag = |keyword: &str| schema.get(keyword) == Some(&Value::Bool(true));
    if let Some(min) = bound("minimum") {
        if flag("exclusiveMinimum") && n <= min {
            errors.push(format!("{at}: {value} is not greater than {min}"));
        } else if n < min {
            errors.push(format!("{at}: {value} is less than {min}"));
        }
    }
    if let Some(max) = bound("maximum") {
        if flag("exclusiveMaximum") && n >= max {
            errors.push(format!("{at}: {value} is not less than {max}"));
        } else if n > max {
            errors.push(format!("{at}: {value} is greater than {max}"));
        }
    }
    if let Some(min) = bound("exclusiveMinimum")
        && n <= min
    {
        errors.push(format!("{at}: {value} is not greater than {min}"));
    }
    if let Some(max) = bound("exclusiveMaximum")
        && n >= max
    {
        errors.push(format!("{at}: {value} is not less than {max}"));
    }
    if let Some(step) = bound("multipleOf")
        && step > 0.0
        && (n / step).fract().abs() > 1e-9
    {
        errors.push(format!("{at}: {value} is not a multiple of {step}"));
    }
    let range = match schema.get("format").and_then(Value::as_str) {
        Some("int32") => value.as_i64().is_some_and(|i| i32::try_from(i).is_ok()),
        Some("int64") => value.is_i64(),
        Some("uint32") => value.as_u64().is_some_and(|u| u32::try_from(u).is_ok()),
        Some("uint64") => value.is_u64(),
        _ => true,
    };
    if !range {
        let format = schema
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or_default();
        errors.push(format!("{at}: {value} is outside the range of {format}"));
    }
}

/// Whether `text` is valid for a string format this checker knows; `None` for other formats,
/// which are annotations.
fn string_format(format: &str, text: &str) -> Option<bool> {
    let shape = |pattern: &str| Regex::new(pattern).is_ok_and(|regex| regex.is_match(text));
    match format {
        "date-time" => Some(shape(
            r"^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$",
        )),
        "date" => Some(shape(r"^\d{4}-\d{2}-\d{2}$")),
        "uuid" => Some(shape(
            r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
        )),
        "email" => Some(shape(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")),
        "uri" => Some(url::Url::parse(text).is_ok()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::validate;

    /// A document with one component schema, as the API's documents reference them.
    fn document() -> Value {
        json!({
            "components": { "schemas": {
                "Id_Person": { "type": "string", "pattern": "^per_[0-9a-f]{32}$" },
                "Person": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "email"],
                    "properties": {
                        "id": { "$ref": "#/components/schemas/Id_Person" },
                        "email": { "type": "string", "format": "email" },
                        "given_name": { "type": ["string", "null"], "maxLength": 5 },
                        "status": { "type": "string", "enum": ["active", "archived"] },
                        "limit": { "type": "integer", "format": "int32", "minimum": 1, "maximum": 100 },
                        "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 2, "uniqueItems": true }
                    }
                }
            }}
        })
    }

    fn person(document: &Value, value: &Value) -> Vec<String> {
        validate(
            document,
            &json!({ "$ref": "#/components/schemas/Person" }),
            value,
        )
    }

    /// A valid example passes, `null` included where the type allows it: the checker must not
    /// fail the document on what it describes correctly.
    #[test]
    fn a_valid_example_passes() {
        let document = document();
        let value = json!({
            "id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
            "email": "ada@example.com",
            "given_name": null,
            "status": "active",
            "limit": 100,
            "tags": ["a", "b"]
        });
        assert_eq!(person(&document, &value), Vec::<String>::new());
    }

    /// Each kind of constraint the API's schemas use is enforced through a `$ref`, and named at
    /// its place in the value: a pattern, a length, a bound, an integer, an enum, an unknown
    /// member, a required member, an item count, uniqueness and a format. A serde round trip
    /// would accept all of these.
    #[test]
    fn every_constraint_is_enforced() {
        let document = document();
        let cases = [
            (
                json!({"id": "per_1", "email": "a@b.co"}),
                "$.id: \"per_1\" does not match ^per_[0-9a-f]{32}$",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "given_name": "Augusta"}),
                "$.given_name: \"Augusta\" is longer than 5 characters",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "limit": 0}),
                "$.limit: 0 is less than 1",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "limit": 2.5}),
                "$.limit: 2.5 is not of type integer",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "status": "gone"}),
                "$.status: \"gone\" is not one of [\"active\",\"archived\"]",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "nickname": "x"}),
                "$: the member `nickname` is not allowed",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"}),
                "$: the required member `email` is missing",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "tags": ["a", "b", "c"]}),
                "$.tags: 3 items, more than 2",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "a@b.co", "tags": ["a", "a"]}),
                "$.tags: the items are not unique",
            ),
            (
                json!({"id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "email": "not-an-address"}),
                "$.email: \"not-an-address\" is not a valid email",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                person(&document, &value),
                vec![expected.to_owned()],
                "{value}"
            );
        }
    }

    /// `oneOf` needs exactly one match and `anyOf` at least one; a dangling `$ref` and a pattern
    /// the checker cannot compile are reported, never taken as passing.
    #[test]
    fn combinators_and_unknowns() {
        let document = json!({});
        let one_of = json!({ "oneOf": [{ "type": "integer" }, { "type": "number" }] });
        assert_eq!(
            validate(&document, &one_of, &json!(3)),
            ["$: matches 2 alternatives of `oneOf`, where exactly one must match"]
        );
        assert_eq!(
            validate(&document, &one_of, &json!(3.5)),
            Vec::<String>::new()
        );
        let any_of = json!({ "anyOf": [{ "type": "string" }, { "type": "boolean" }] });
        assert_eq!(
            validate(&document, &any_of, &json!(true)),
            Vec::<String>::new()
        );
        assert_eq!(
            validate(&document, &any_of, &json!(1)),
            [
                "$: matches none of the 2 alternatives of `anyOf` (the first: $: 1 is not of type string)"
            ]
        );
        assert_eq!(
            validate(
                &document,
                &json!({ "$ref": "#/components/schemas/Gone" }),
                &json!(1)
            ),
            ["$: `$ref` #/components/schemas/Gone points to nothing"]
        );
        assert_eq!(
            validate(
                &document,
                &json!({ "type": "string", "pattern": "^(?=a)" }),
                &json!("a")
            ),
            ["$: the pattern ^(?=a) cannot be checked (not supported by the `regex` crate)"]
        );
    }

    /// Exclusive bounds in both spellings (3.1 numbers, 3.0 flags) and integer formats' ranges
    /// are enforced, since the API's integer fields are documented that way.
    #[test]
    fn exclusive_bounds_and_integer_ranges() {
        let document = json!({});
        assert_eq!(
            validate(
                &document,
                &json!({ "type": "number", "exclusiveMinimum": 0 }),
                &json!(0)
            ),
            ["$: 0 is not greater than 0"]
        );
        assert_eq!(
            validate(
                &document,
                &json!({ "type": "number", "maximum": 1, "exclusiveMaximum": true }),
                &json!(1)
            ),
            ["$: 1 is not less than 1"]
        );
        assert_eq!(
            validate(
                &document,
                &json!({ "type": "integer", "format": "uint32" }),
                &json!(4_294_967_296_u64)
            ),
            ["$: 4294967296 is outside the range of uint32"]
        );
    }
}
