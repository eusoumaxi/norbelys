//! `cargo xtask openapi check`: the committed public OpenAPI document,
//! `crates/server/openapi.json`, is what the handlers declare, and every example in it is valid
//! against the schema it illustrates.
//!
//! **Equal to the handlers.** The server crate owns that comparison, as a test beside the code
//! that derives the document from the handlers (among the tests under [`COMPARISON_TESTS`]); this
//! command runs those tests rather than comparing a second time, and fails when they fail or
//! when none matches any more (moving the module would cause it).
//!
//! **Examples that validate.** The reference, the SDK, the CLI and every reader learn the API
//! from these examples, and deserializing an example proves only that it has the right shape,
//! not that it respects a pattern, a bound or an enum. Every example is found wherever OpenAPI
//! 3.1 allows one: in schemas at any depth (`example`, and `examples` as a list), and in
//! parameters, headers and request and response bodies (`example`, and `examples` as a map of
//! example objects whose `value` is checked, references to `components/examples` resolved). Each
//! is validated against its schema by [`schema::validate`]. Example values themselves, `enum`,
//! `const`, `default` and extensions (`x-…`) are data, never searched for further examples.

mod schema;

use std::path::Path;
use std::process::Command;

use anyhow::Context as _;
use serde_json::Value;

/// The committed public document, relative to the repository root.
const DOCUMENT: &str = "crates/server/openapi.json";

/// Pure contract tests, including handler drift. The separately named runtime tests exercise
/// anonymous routed downloads in the database suite; this command never opens a fixture.
const COMPARISON_TESTS: &str = "http::openapi::tests::";

/// Runs the comparison tests, then validates every example of the committed document.
///
/// # Errors
///
/// Cargo cannot be started, or the document cannot be read or is not JSON.
pub fn run(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut findings = comparison(root)?;
    let path = root.join(DOCUMENT);
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {DOCUMENT}"))?;
    let document: Value =
        serde_json::from_str(&text).with_context(|| format!("{DOCUMENT} is not JSON"))?;
    findings.extend(example_findings(&document));
    Ok(findings)
}

/// Runs the server's comparison tests; one finding when they fail or none exists.
fn comparison(root: &Path) -> anyhow::Result<Vec<String>> {
    // `cargo xtask` runs under Cargo, which names itself in `CARGO`.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "test",
            "--locked",
            "-p",
            "norbelys-server",
            "--lib",
            "--",
            COMPARISON_TESTS,
        ])
        .current_dir(root)
        .output()
        .context("cannot run cargo test")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|line| !line.trim().is_empty())
            .collect();
        let tail = tail
            .iter()
            .skip(tail.len().saturating_sub(12))
            .copied()
            .collect::<Vec<_>>()
            .join("\n    ");
        return Ok(vec![format!(
            "{DOCUMENT}: the comparison with the handlers failed ({COMPARISON_TESTS}):\n    {tail}"
        )]);
    }
    if stdout.contains("running 0 tests") {
        return Ok(vec![format!(
            "{DOCUMENT}: no server test matches {COMPARISON_TESTS} (moved?), so nothing compares \
             the document with the handlers; point COMPARISON_TESTS in tools/xtask/src/openapi.rs \
             at them"
        )]);
    }
    Ok(Vec::new())
}

/// Every example of the document that does not validate, as `pointer: error`.
pub fn example_findings(document: &Value) -> Vec<String> {
    let mut findings = Vec::new();
    walk(document, document, "", false, &mut findings);
    findings
}

/// Keywords whose values are data, never searched for examples.
const DATA: [&str; 5] = ["example", "examples", "enum", "const", "default"];

/// Keywords whose value is one schema.
const ONE_SCHEMA: [&str; 9] = [
    "items",
    "additionalProperties",
    "not",
    "contains",
    "propertyNames",
    "if",
    "then",
    "else",
    "unevaluatedProperties",
];

/// Keywords whose value is a map of schemas.
const SCHEMA_MAP: [&str; 4] = [
    "properties",
    "patternProperties",
    "$defs",
    "dependentSchemas",
];

/// Keywords whose value is a list of schemas.
const SCHEMA_LIST: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];

/// Visits `node` (at `pointer`); `schema` tells whether it is in a schema position.
fn walk(document: &Value, node: &Value, pointer: &str, schema: bool, findings: &mut Vec<String>) {
    match node {
        Value::Object(object) if schema => {
            if let Some(example) = object.get("example") {
                check(
                    document,
                    node,
                    example,
                    &format!("{pointer}/example"),
                    findings,
                );
            }
            if let Some(examples) = object.get("examples").and_then(Value::as_array) {
                for (index, example) in examples.iter().enumerate() {
                    check(
                        document,
                        node,
                        example,
                        &format!("{pointer}/examples/{index}"),
                        findings,
                    );
                }
            }
            for (key, child) in object {
                let at = format!("{pointer}/{}", escape(key));
                if ONE_SCHEMA.contains(&key.as_str()) {
                    walk(document, child, &at, true, findings);
                } else if SCHEMA_MAP.contains(&key.as_str()) {
                    for (name, sub) in child.as_object().into_iter().flatten() {
                        walk(
                            document,
                            sub,
                            &format!("{at}/{}", escape(name)),
                            true,
                            findings,
                        );
                    }
                } else if SCHEMA_LIST.contains(&key.as_str()) {
                    for (index, sub) in child.as_array().into_iter().flatten().enumerate() {
                        walk(document, sub, &format!("{at}/{index}"), true, findings);
                    }
                }
            }
        }
        Value::Object(object) => {
            // A parameter, a header or a media type: examples of its `schema`.
            if let Some(target) = object.get("schema") {
                if let Some(example) = object.get("example") {
                    check(
                        document,
                        target,
                        example,
                        &format!("{pointer}/example"),
                        findings,
                    );
                }
                for (name, example) in object
                    .get("examples")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let example = example
                        .get("$ref")
                        .and_then(Value::as_str)
                        .and_then(|reference| schema::resolve(document, reference))
                        .unwrap_or(example);
                    if let Some(value) = example.get("value") {
                        check(
                            document,
                            target,
                            value,
                            &format!("{pointer}/examples/{}/value", escape(name)),
                            findings,
                        );
                    }
                }
            }
            for (key, child) in object {
                if DATA.contains(&key.as_str()) || key.starts_with("x-") {
                    continue;
                }
                let at = format!("{pointer}/{}", escape(key));
                if key == "schema" {
                    walk(document, child, &at, true, findings);
                } else if key == "schemas" && pointer == "/components" {
                    for (name, sub) in child.as_object().into_iter().flatten() {
                        walk(
                            document,
                            sub,
                            &format!("{at}/{}", escape(name)),
                            true,
                            findings,
                        );
                    }
                } else {
                    walk(document, child, &at, false, findings);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk(
                    document,
                    item,
                    &format!("{pointer}/{index}"),
                    schema,
                    findings,
                );
            }
        }
        _ => {}
    }
}

/// Validates one example and records its errors.
fn check(
    document: &Value,
    schema: &Value,
    example: &Value,
    pointer: &str,
    findings: &mut Vec<String>,
) {
    for error in schema::validate(document, schema, example) {
        findings.push(format!("{DOCUMENT}#{pointer}: {error}"));
    }
}

/// A JSON pointer segment (RFC 6901): `~` and `/` escaped.
fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::example_findings;

    /// Examples are found in every place OpenAPI allows them (a component schema's list, a
    /// property, a request body's `example`, a parameter, a response's named examples, one of
    /// them by reference) and each is checked against the right schema; valid ones pass and
    /// invalid ones are named by their JSON pointer, so a wrong example in any place fails.
    #[test]
    fn examples_are_found_everywhere_and_checked() {
        let document = json!({
            "paths": {
                "/v1/people/{id}": {
                    "get": {
                        "parameters": [{
                            "name": "id", "in": "path",
                            "schema": { "$ref": "#/components/schemas/Id" },
                            "example": "per_bad"
                        }],
                        "responses": { "200": { "content": { "application/json": {
                            "schema": { "$ref": "#/components/schemas/Person" },
                            "examples": {
                                "ok": { "value": { "id": "per_0123456789abcdef0123456789abcdef" } },
                                "shared": { "$ref": "#/components/examples/Wrong" }
                            }
                        }}}}
                    },
                    "patch": {
                        "requestBody": { "content": { "application/json": {
                            "schema": { "$ref": "#/components/schemas/Person" },
                            "example": { "id": "per_0123456789abcdef0123456789abcdef", "x-extra": 1 }
                        }}}
                    }
                }
            },
            "components": {
                "examples": { "Wrong": { "value": { "id": 7 } } },
                "schemas": {
                    "Id": { "type": "string", "pattern": "^per_[0-9a-f]{32}$", "examples": ["per_0123456789abcdef0123456789abcdef", "nope"] },
                    "Person": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": { "id": { "$ref": "#/components/schemas/Id" }, "age": { "type": "integer", "minimum": 0, "example": -1 } }
                    }
                }
            }
        });
        assert_eq!(
            example_findings(&document),
            [
                "crates/server/openapi.json#/components/schemas/Id/examples/1: $: \"nope\" does not match ^per_[0-9a-f]{32}$",
                "crates/server/openapi.json#/components/schemas/Person/properties/age/example: $: -1 is less than 0",
                "crates/server/openapi.json#/paths/~1v1~1people~1{id}/get/parameters/0/example: $: \"per_bad\" does not match ^per_[0-9a-f]{32}$",
                "crates/server/openapi.json#/paths/~1v1~1people~1{id}/get/responses/200/content/application~1json/examples/shared/value: $.id: 7 is not of type string",
                "crates/server/openapi.json#/paths/~1v1~1people~1{id}/patch/requestBody/content/application~1json/example: $: the member `x-extra` is not allowed",
            ]
        );
    }
}
