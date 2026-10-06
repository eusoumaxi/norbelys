//! The OpenAPI document as it is published: the whole document of `/v1`, or its public
//! surface, as stable text; and the check that keeps the committed copy of the public
//! surface, `crates/server/openapi.json`, equal to what the handlers declare.
//!
//! # Two surfaces, one document
//!
//! The handlers declare every `/v1` operation (the router derives the document from them), and
//! two surfaces share it. The public surface is what API keys, the CLI and the dashboard call;
//! the dashboard surface (sign-in, the account, the team, API keys, SSO, the audit log) accepts
//! only a browser session. A dashboard operation carries the tag `Dashboard`, or the extension
//! `x-surface: dashboard`. The public document leaves those operations out, a path left
//! without operations, the `Dashboard` tag, and the schemas only those operations use, so a
//! reader of the reference, the SDK, the CLI or the MCP catalogue never meets an operation an
//! API key would be refused. Schemas are kept by reachability from the remaining operations
//! and other components. This keeps every dependency of a public shape while removing unused
//! copies of dashboard shapes that would otherwise refer to removed schemas.
//!
//! # Stable text
//!
//! The document is printed with its object keys sorted, indented by two spaces and ending in
//! a newline, so regenerating it changes the file only when an operation changed and a review
//! reads the contract's diff. The sorting comes from `serde_json`'s map, which is ordered by
//! key unless a crate in the build enables its `preserve_order` feature; none does, and should
//! one ever do, the order stays deterministic and the committed file is simply regenerated
//! once.
//!
//! # The committed copy
//!
//! `crates/server/openapi.json` is the public document, committed so the `norbelys` CLI can
//! embed it and turn every operation into a command at build time. Both SDK generators read
//! this committed public document through tools/codegen/contract.ts. HTTP and MCP derive
//! their served documents from the handlers. The file is never edited by hand: the read-only
//! comparison fails when it differs from the handlers and provides the regeneration command
//! from the repository root:
//!
//! ```text
//! cargo run -q -p norbelys-server -- admin openapi --public --out crates/server/openapi.json
//! ```
//!
//! Without `--out`, `admin openapi` writes the document to standard output, where the role's
//! local telemetry also writes its JSON lines: nothing on that path may log, or a redirected
//! file would carry log lines.
//!
//! # HTTP publication
//!
//! The product router serves the public document at `/openapi.json` and `/openapi.yaml`
//! without authentication. Both representations are generated from this build's handlers
//! once per process and shared as immutable bytes. Requests do not query the database or
//! read a file; serialization failures answer through the common problem mechanism.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use bytes::Bytes;
use serde_json::{Map, Value};

use super::AppState;
use crate::problem::Problem;

/// The public JSON document, shared without copying its body on each request.
static JSON: LazyLock<Result<Bytes, String>> = LazyLock::new(|| {
    document(true)
        .map(Bytes::from)
        .map_err(|error| error.to_string())
});

/// The same public document serialized with the YAML library used by OpenAPI.
static YAML: LazyLock<Result<Bytes, String>> = LazyLock::new(|| {
    let json = JSON.as_ref().map_err(Clone::clone)?;
    let document: Value = serde_json::from_slice(json).map_err(|error| error.to_string())?;
    yaml_serde::to_string(&document)
        .map(Bytes::from)
        .map_err(|error| error.to_string())
});

/// Public contract downloads, outside the authenticated `/v1` operations they describe.
/// Neither route reads or writes database state; generation errors become `500` problems.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/openapi.json", get(json))
        .route("/openapi.yaml", get(yaml))
}

/// Serves the public contract as JSON, or a problem if serialization failed.
async fn json() -> Result<impl IntoResponse, Problem> {
    let body = JSON.as_ref().map_err(|error| Problem::internal(error))?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body.clone()))
}

/// Serves the public contract as YAML, or a problem if serialization failed.
async fn yaml() -> Result<impl IntoResponse, Problem> {
    let body = YAML.as_ref().map_err(|error| Problem::internal(error))?;
    Ok(([(header::CONTENT_TYPE, "application/yaml")], body.clone()))
}

/// The tag every dashboard operation carries.
const DASHBOARD_TAG: &str = "Dashboard";
/// The keys under which a path item holds its operations (the OpenAPI 3.1 path item object);
/// its other members (`summary`, `parameters`, `servers`) are not operations.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];
/// How a reference to a component schema begins.
const SCHEMA_REF: &str = "#/components/schemas/";

/// The OpenAPI document of `/v1` as stable text (sorted keys, two-space indentation, a final
/// newline): the whole document, or with `public` only the public surface, which is what
/// `crates/server/openapi.json` holds.
///
/// # Errors
///
/// Serializing the document fails only if a handler declared a value JSON cannot represent.
pub fn document(public: bool) -> Result<String, serde_json::Error> {
    let mut document = serde_json::to_value(super::router::openapi())?;
    if public {
        document = public_surface(document);
    }
    let mut text = serde_json::to_string_pretty(&document)?;
    text.push('\n');
    Ok(text)
}

/// Whether an operation belongs to the dashboard surface.
fn is_dashboard(operation: &Value) -> bool {
    operation.get("x-surface").and_then(Value::as_str) == Some("dashboard")
        || operation
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(DASHBOARD_TAG)))
}

/// The public surface of `document`: without the dashboard operations, the paths they leave
/// empty, the `Dashboard` tag, and the schemas only they reference (directly or through other
/// schemas).
fn public_surface(mut document: Value) -> Value {
    if let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) {
        for item in paths.values_mut() {
            let Some(item) = item.as_object_mut() else {
                continue;
            };
            for method in METHODS {
                if item.get(method).is_some_and(is_dashboard) {
                    item.remove(method);
                }
            }
        }
        paths.retain(|_, item| METHODS.iter().any(|method| item.get(method).is_some()));
    }
    if let Some(tags) = document.get_mut("tags").and_then(Value::as_array_mut) {
        tags.retain(|tag| tag.get("name").and_then(Value::as_str) != Some(DASHBOARD_TAG));
    }
    let Some(slot) = document.pointer_mut("/components/schemas") else {
        return document;
    };
    let Value::Object(mut schemas) = slot.take() else {
        return document;
    };
    // With the schemas taken out, every reference left in the document is one the public
    // surface makes: from its operations, and from any other component.
    let mut kept = Vec::new();
    collect_references(&document, &mut kept);
    let kept = closure(kept, &schemas);
    schemas.retain(|name, _| kept.contains(name));
    if let Some(slot) = document.pointer_mut("/components/schemas") {
        *slot = Value::Object(schemas);
    }
    document
}

/// The schemas `roots` name together with every schema those reference in turn.
fn closure(mut pending: Vec<String>, schemas: &Map<String, Value>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if let Some(schema) = schemas.get(&name)
            && seen.insert(name)
        {
            collect_references(schema, &mut pending);
        }
    }
    seen
}

/// Appends the name of every component schema `value` references, at any depth.
fn collect_references(value: &Value, into: &mut Vec<String>) {
    match value {
        Value::Object(members) => {
            for (key, member) in members {
                match member
                    .as_str()
                    .and_then(|target| target.strip_prefix(SCHEMA_REF))
                {
                    Some(name) if key == "$ref" => into.push(name.to_owned()),
                    _ => collect_references(member, into),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_references(item, into);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{document, public_surface};

    /// The public surface drops exactly the dashboard operations (by tag or by `x-surface`), the
    /// path they leave empty, the `Dashboard` tag and the schemas only they reach, even through
    /// another schema; a schema a public operation also reaches stays, and unused schemas leave.
    /// Leaving a dashboard schema in would show API-key holders a shape they can
    /// never use; removing a shared one would break the public operations' references.
    #[test]
    fn the_public_surface_leaves_out_dashboard_operations_and_the_schemas_only_they_use() {
        let reference = |name: &str| json!({ "$ref": format!("#/components/schemas/{name}") });
        let whole = json!({
            "openapi": "3.1.0",
            "tags": [{ "name": "Dashboard" }, { "name": "Audience" }],
            "paths": {
                "/v1/me": {
                    "get": {
                        "tags": ["Dashboard"],
                        "responses": { "200": { "content": { "application/json": { "schema": reference("Me") } } } }
                    }
                },
                "/v1/people": {
                    "get": {
                        "tags": ["Audience"],
                        "responses": { "200": { "content": { "application/json": { "schema": reference("Page") } } } }
                    },
                    "post": {
                        "tags": ["Audience"],
                        "x-surface": "dashboard",
                        "requestBody": { "content": { "application/json": { "schema": reference("CreateSecret") } } }
                    }
                }
            },
            "components": {
                "schemas": {
                    "Me": { "properties": { "membership": reference("Membership"), "email": reference("Email") } },
                    "Membership": { "type": "object" },
                    "CreateSecret": { "type": "object" },
                    "Page": { "properties": { "data": { "type": "array", "items": reference("Person") } } },
                    "Person": { "properties": { "email": reference("Email") } },
                    "Email": { "type": "string" },
                    "UnusedDashboardCopy": { "properties": { "membership": reference("Membership") } }
                },
                "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } }
            }
        });
        let public = public_surface(whole);
        let paths = public["paths"].as_object().unwrap();
        assert_eq!(paths.keys().collect::<Vec<_>>(), ["/v1/people"]);
        assert_eq!(
            paths["/v1/people"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["get"]
        );
        assert_eq!(public["tags"], json!([{ "name": "Audience" }]));
        let mut schemas = public["components"]["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        schemas.sort();
        assert_eq!(schemas, ["Email", "Page", "Person"]);
        assert!(public["components"]["securitySchemes"]["bearer"].is_object());
    }

    /// Each component reference resolves in both published surfaces. Removing a dashboard-only
    /// dependency while retaining an unused wrapper used to make SDK generation fail.
    #[test]
    fn every_published_schema_reference_resolves() {
        for public in [false, true] {
            let value: serde_json::Value =
                serde_json::from_str(&document(public).unwrap()).unwrap();
            let mut references = Vec::new();
            super::collect_references(&value, &mut references);
            for name in references {
                assert!(
                    value["components"]["schemas"].get(&name).is_some(),
                    "missing {name}, public={public}"
                );
            }
        }
    }

    /// Both documents are stable text: pretty-printed with sorted keys and a final newline, so
    /// reading one back and printing it again gives the same bytes, and regenerating the file
    /// changes it only where an operation changed, which keeps the contract's diffs reviewable.
    #[test]
    fn documents_are_sorted_pretty_text_with_a_final_newline() {
        for public in [true, false] {
            let text = document(public).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            let again = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
            assert_eq!(again, text);
            let at = |key: &str| text.find(&format!("\n  \"{key}\":")).unwrap();
            assert!(
                at("components") < at("info")
                    && at("info") < at("openapi")
                    && at("openapi") < at("paths")
            );
        }
    }

    /// `crates/server/openapi.json` is exactly the public document the handlers declare. The CLI
    /// embeds that file, so a handler changed without regenerating it would ship commands for a
    /// contract the server no longer serves. The failure names the one command that brings it
    /// up to date.
    #[test]
    fn the_committed_public_document_matches_the_handlers() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");
        let expected = document(true).unwrap();
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        if committed != expected {
            if let Some(directory) = std::env::var_os("RUNNER_TEMP") {
                std::fs::write(
                    std::path::PathBuf::from(directory).join("declared-openapi.json"),
                    &expected,
                )
                .unwrap();
            }
            let line = committed
                .lines()
                .zip(expected.lines())
                .position(|(committed, expected)| committed != expected)
                .unwrap_or_else(|| committed.lines().count().min(expected.lines().count()))
                + 1;
            panic!(
                "{path} is not the public OpenAPI document the handlers declare (it differs from \
                 line {line}). Never edit it by hand: regenerate it from the repository root and \
                 review the diff, then rebuild the CLI, which embeds it:\n\n    \
                 cargo run -q -p norbelys-server -- admin openapi --public --out crates/server/openapi.json\n"
            );
        }
    }

    /// Every type that derives `ToSchema` has a schema name of its own. The document keys schemas
    /// by name and `utoipa` keeps one of two types that share a name without a warning, so a
    /// second `DomainObject` silently replaces the first in every operation that returns it: the
    /// document then describes a different object than the handler serves. The source is scanned
    /// rather than the document because the document no longer shows the type that lost.
    #[test]
    fn every_schema_name_belongs_to_one_type() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut owners: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        let mut pending = vec![std::path::PathBuf::from(root)];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    std::fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let mut deriving = false;
            let mut renamed: Option<String> = None;
            for line in source.lines().map(str::trim_start) {
                if line.starts_with("#[derive(") && line.contains("ToSchema") {
                    deriving = true;
                } else if deriving && line.starts_with("#[schema(") && line.contains("as = ") {
                    renamed = line
                        .split("as = ")
                        .nth(1)
                        .and_then(|rest| rest.split([')', ',']).next())
                        .and_then(|name| name.rsplit("::").next())
                        .map(|name| name.trim().to_owned());
                } else if deriving
                    && let Some(name) = [
                        "pub struct ",
                        "pub enum ",
                        "pub(crate) struct ",
                        "pub(crate) enum ",
                        "struct ",
                        "enum ",
                    ]
                    .iter()
                    .find_map(|prefix| line.strip_prefix(prefix))
                {
                    let declared: String = name
                        .chars()
                        .take_while(|character| character.is_alphanumeric() || *character == '_')
                        .collect();
                    let schema = renamed.take().unwrap_or(declared);
                    owners
                        .entry(schema)
                        .or_default()
                        .push(path.strip_prefix(root).unwrap().display().to_string());
                    deriving = false;
                }
            }
        }
        let shared: Vec<String> = owners
            .into_iter()
            .filter(|(_, files)| files.len() > 1)
            .map(|(name, files)| format!("{name}: {}", files.join(", ")))
            .collect();
        assert!(
            shared.is_empty(),
            "schema names declared by more than one type (rename one, or give it \
             `#[schema(as = UniqueName)]`):\n{}",
            shared.join("\n")
        );
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::document;
    use axum::http::StatusCode;

    /// Anonymous downloads use the running build's public contract in both formats, with
    /// their media types and request context. The YAML conversion must preserve every
    /// field, including extensions, so consumers of either representation see the same API.
    #[tokio::test]
    async fn public_contract_downloads_need_no_credentials() {
        let db = crate::testing::TestDb::new().await;
        let app = db.app();
        let expected = document(true).unwrap();
        let json = app.get("/openapi.json").send().await;
        assert_eq!(json.status, StatusCode::OK);
        assert_eq!(json.header("content-type"), Some("application/json"));
        assert!(json.header("x-request-id").is_some());
        assert_eq!(json.body.as_ref(), expected.as_bytes());

        let yaml = app.get("/openapi.yaml").send().await;
        assert_eq!(yaml.status, StatusCode::OK);
        assert_eq!(yaml.header("content-type"), Some("application/yaml"));
        let parsed: serde_json::Value = yaml_serde::from_slice(&yaml.body).unwrap();
        assert_eq!(parsed, json.json);
    }
}
