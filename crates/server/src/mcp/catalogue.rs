//! The MCP tool catalogue: one tool per public operation of the API, derived from the OpenAPI
//! document the handlers declare (the same document the CLI embeds), never written by hand.
//!
//! # How an operation becomes a tool
//!
//! - **Name**: the operation id (`people.create`, `webhook_endpoints.rotate_secret`).
//! - **Description**: the operation's summary and description, the scope it needs, and for the
//!   operations that take an `Idempotency-Key` ([`takes_key`]: an effectful `POST` and a
//!   `PATCH`) the idempotency note: such a tool takes an optional `idempotency_key` (the
//!   caller's stable request id); without one each `POST` gets a fresh key, so repeating a call
//!   whose answer was lost may repeat its effect. A `DELETE` takes none: the API ignores the
//!   header there, deleting twice being deleting once, and so does a preflight check, which
//!   stores nothing.
//! - **Input schema**: an object whose properties are the operation's path and query parameters
//!   under their own names, its header parameters in snake case (`If-Match` is `if_match`), the
//!   JSON request body as `body`, and `idempotency_key` where it is taken. The document's
//!   component schemas the operation references are copied into the schema's `$defs`, so each
//!   tool's schema stands alone.
//! - **Output schema**: the success response's JSON schema, when the operation answers a body.
//! - **Annotations**: reads are read-only and idempotent; deletes are destructive and
//!   idempotent; nothing reaches beyond the workspace except sending mail, which the description
//!   says.
//! - **Scope**: the scopes the operation's handler accepts ([`scopes_of`]), any one of which
//!   opens the tool. Mostly one, from the resource and the method ([`scope_of`]): a read needs
//!   the area's read scope, anything else its write scope; but a preflight check, which writes
//!   nothing, needs `people:read`, and an export takes the scope of the resource it holds (the
//!   handler checks the one the request names), so its tools open to any of those. A tool none of
//!   whose scopes the caller holds is not listed.
//!
//! An operation whose request body is not JSON (an image upload) cannot be called with JSON
//! arguments and is left out.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};

use axum::http::Method;
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use strum::IntoEnumIterator as _;

use crate::domain::scope::{Scope, ScopeSet};
use crate::people::exports::Resource;

/// The argument that carries an operation's idempotency key, where it takes one ([`takes_key`]).
pub const IDEMPOTENCY_KEY: &str = "idempotency_key";
/// The argument that carries the JSON request body.
pub const BODY: &str = "body";
/// How a reference to a component schema begins in the document.
const COMPONENT: &str = "#/components/schemas/";
/// How it begins in a tool's schema.
const DEFINITION: &str = "#/$defs/";

/// One tool and how to call its operation.
#[derive(Debug, Clone)]
pub struct Operation {
    /// The tool, as `tools/list` shows it.
    pub tool: Tool,
    /// The HTTP method.
    pub method: Method,
    /// The path template (`/v1/people/{id}`).
    pub path: String,
    /// The path parameters, by name.
    pub path_params: Vec<String>,
    /// The query parameters, by name.
    pub query_params: Vec<String>,
    /// The header parameters: the argument's name and the header's.
    pub header_params: Vec<(String, String)>,
    /// Whether it takes a JSON body.
    pub body: bool,
    /// The scopes its handler accepts, any one of them (see [`scopes_of`]).
    pub scopes: ScopeSet,
}

impl Operation {
    /// Whether a caller holding `held` may call it: it holds one of the operation's scopes.
    #[must_use]
    pub fn opened_by(&self, held: ScopeSet) -> bool {
        held.intersect(self.scopes).iter().next().is_some()
    }
}

/// `scopes` as a description names them: `` `people:read` ``, or, for scopes any one of which
/// opens a tool, `` `people:write` or `messages:read` ``.
#[must_use]
pub fn named(scopes: ScopeSet) -> String {
    scopes
        .iter()
        .map(|scope| format!("`{scope}`"))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The catalogue: every tool, by name.
#[derive(Debug, Default)]
pub struct Catalogue {
    operations: BTreeMap<String, Operation>,
    /// The public operations left out, with why (for the test that keeps the catalogue whole).
    left_out: Vec<(String, &'static str)>,
}

impl Catalogue {
    /// The tool named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Operation> {
        self.operations.get(name)
    }

    /// The tools a caller holding `scopes` may see, by name.
    pub fn visible(&self, scopes: ScopeSet) -> impl Iterator<Item = &Operation> {
        self.operations
            .values()
            .filter(move |operation| operation.opened_by(scopes))
    }

    /// Builds the catalogue of an OpenAPI `document` (see the module).
    #[must_use]
    pub fn from_document(document: &Value) -> Self {
        let mut catalogue = Self::default();
        let schemas = document
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let paths = document
            .get("paths")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (path, item) in &paths {
            let Some(item) = item.as_object() else {
                continue;
            };
            for (method, operation) in item {
                let Ok(method) = method.to_ascii_uppercase().parse::<Method>() else {
                    continue;
                };
                let Some(id) = operation.get("operationId").and_then(Value::as_str) else {
                    continue;
                };
                match build(path, &method, id, operation, &schemas) {
                    Ok(built) => {
                        catalogue.operations.insert(id.to_owned(), built);
                    }
                    Err(reason) => catalogue.left_out.push((id.to_owned(), reason)),
                }
            }
        }
        for (id, reason) in &catalogue.left_out {
            tracing::debug!(operation = %id, reason, "left out of the MCP catalogue");
        }
        catalogue
    }
}

/// The catalogue of this build's public OpenAPI document, built once.
pub static CATALOGUE: LazyLock<Catalogue> =
    LazyLock::new(|| {
        match crate::http::openapi::document(true)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            Some(document) => Catalogue::from_document(&document),
            None => {
                tracing::error!(
                    "the OpenAPI document could not be read: the MCP server has no tools"
                );
                Catalogue::default()
            }
        }
    });

/// The scopes the handler of operation `id` accepts, any one of them: for an export, the scope
/// each resource an export may hold takes (to request one [`Resource::create_scope`], to read one
/// [`Resource::read_scope`]). Search and attachment reads similarly choose their scope from the
/// requested direction or stored reference. The handler checks that choice; mixed history
/// needs both read scopes. Other operations use [`scope_of`]. Unknown resources return none.
#[must_use]
pub fn scopes_of(id: &str, method: &Method) -> Option<ScopeSet> {
    if id == "messages.search" {
        return Some(
            [Scope::MessagesRead, Scope::InboxRead]
                .into_iter()
                .collect(),
        );
    }
    if id == "attachments.retrieve" {
        return Some(
            [Scope::MessagesRead, Scope::InboxRead, Scope::MessagesSend]
                .into_iter()
                .collect(),
        );
    }
    if id.starts_with("exports.") {
        let scope: fn(Resource) -> Scope = if *method == Method::GET {
            Resource::read_scope
        } else {
            Resource::create_scope
        };
        return Some(Resource::iter().map(scope).collect());
    }
    scope_of(id, method).map(|scope| std::iter::once(scope).collect())
}

/// The scope the handler of operation `id` requires: the read scope of its resource's area for a
/// `GET`, the write scope otherwise; a preflight check writes nothing and needs `people:read`.
/// `None` for a resource no area claims, and for exports, whose scopes are [`scopes_of`]'s.
#[must_use]
pub fn scope_of(id: &str, method: &Method) -> Option<Scope> {
    if id == "preflight.create" {
        return Some(Scope::PeopleRead);
    }
    if id == "smtp_authorization.retrieve" {
        return Some(Scope::MessagesSend);
    }
    let (resource, _) = id.split_once('.')?;
    let (read, write) = match resource {
        "connections" | "quota_scopes" | "sending_domains" => {
            (Scope::ConnectionsRead, Scope::ConnectionsManage)
        }
        "people" | "fields" | "groups" | "segments" | "suppressions" | "imports" => {
            (Scope::PeopleRead, Scope::PeopleWrite)
        }
        "campaigns" | "enrollments" | "images" => (Scope::CampaignsRead, Scope::CampaignsWrite),
        "messages" | "delivery_events" => (Scope::MessagesRead, Scope::MessagesSend),
        "attachments" => (Scope::MessagesRead, Scope::MessagesSend),
        "threads" | "inbound_messages" => (Scope::InboxRead, Scope::InboxWrite),
        "jobs" | "events" | "webhook_endpoints" | "webhook_deliveries" => {
            (Scope::AutomationRead, Scope::AutomationManage)
        }
        "analytics" | "metrics" => (Scope::AnalyticsRead, Scope::AnalyticsRead),
        "workspaces" => (Scope::WorkspaceRead, Scope::WorkspaceManage),
        _ => return None,
    };
    Some(if *method == Method::GET { read } else { write })
}

/// Whether operation `path`, called with `method`, takes an `Idempotency-Key`, as the API's
/// idempotency layer reads one: every effectful `POST` (a preflight check stores nothing and
/// takes none) and every `PATCH`; never a `GET` or a `DELETE`, whose repeats are harmless.
#[must_use]
pub fn takes_key(path: &str, method: &Method) -> bool {
    crate::idempotency::takes_key(method.as_str(), path)
}

fn build(
    path: &str,
    method: &Method,
    id: &str,
    operation: &Value,
    schemas: &Map<String, Value>,
) -> Result<Operation, &'static str> {
    let scopes = scopes_of(id, method).ok_or("no scope is known for its resource")?;
    let request = operation.get("requestBody");
    let json_body = request.and_then(|body| body.pointer("/content/application~1json/schema"));
    if request.is_some() && json_body.is_none() {
        return Err("its request body is not JSON");
    }
    let mut references = BTreeSet::new();
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut path_params = Vec::new();
    let mut query_params = Vec::new();
    let mut header_params = Vec::new();
    for parameter in operation
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(location)) = (
            parameter.get("name").and_then(Value::as_str),
            parameter.get("in").and_then(Value::as_str),
        ) else {
            continue;
        };
        // MCP offers a caller-stable key but can generate one when it is absent. Its dedicated
        // argument below must not also become a required HTTP-header argument from OpenAPI.
        if location == "header" && name == "Idempotency-Key" {
            continue;
        }
        let argument = match location {
            "path" => {
                path_params.push(name.to_owned());
                name.to_owned()
            }
            "query" => {
                query_params.push(name.to_owned());
                name.to_owned()
            }
            "header" => {
                let argument = name.to_ascii_lowercase().replace('-', "_");
                header_params.push((argument.clone(), name.to_owned()));
                argument
            }
            _ => continue,
        };
        let mut schema = rewrite(
            parameter.get("schema").cloned().unwrap_or(json!({})),
            &mut references,
        );
        describe(&mut schema, parameter.get("description"));
        properties.insert(argument.clone(), schema);
        if parameter.get("required").and_then(Value::as_bool) == Some(true) {
            required.push(argument);
        }
    }
    if let Some(schema) = json_body {
        let mut schema = rewrite(schema.clone(), &mut references);
        describe(
            &mut schema,
            request
                .and_then(|body| body.get("description"))
                .or(Some(&json!("The request body."))),
        );
        properties.insert(BODY.to_owned(), schema);
        if request
            .and_then(|body| body.get("required"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            required.push(BODY.to_owned());
        }
    }
    let write = *method != Method::GET;
    let keyed = takes_key(path, method);
    if keyed {
        properties.insert(
            IDEMPOTENCY_KEY.to_owned(),
            json!({
                "type": "string",
                "minLength": 1,
                "maxLength": 128,
                "pattern": "^[!-~]+$",
                "description": "Your stable id for this request (1 to 128 visible ASCII characters). \
                    Repeating a call with the same key returns the first answer instead of acting twice.",
            }),
        );
    }
    let mut input = Map::new();
    input.insert("type".to_owned(), json!("object"));
    input.insert("properties".to_owned(), Value::Object(properties));
    input.insert("required".to_owned(), json!(required));
    input.insert("additionalProperties".to_owned(), json!(false));
    let output = success_schema(operation).map(|schema| rewrite(schema.clone(), &mut references));
    let definitions = definitions(&references, schemas);
    if !definitions.is_empty() {
        input.insert("$defs".to_owned(), Value::Object(definitions.clone()));
    }
    let summary = operation
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_owned();
    let mut description = summary.clone();
    if let Some(more) = operation.get("description").and_then(Value::as_str) {
        description.push_str("\n\n");
        description.push_str(more);
    }
    description.push_str(&format!("\n\nRequires the {} scope.", named(scopes)));
    if keyed {
        description.push_str(
            "\n\nPass `idempotency_key` to make a retry safe: without it, repeating a call whose \
             answer was lost may repeat its effect.",
        );
    }
    let mut tool = Tool::new(id.to_owned(), description, Arc::new(input))
        .with_title(summary.clone())
        .with_annotations(
            ToolAnnotations::with_title(summary)
                .read_only(!write)
                .destructive(*method == Method::DELETE)
                .idempotent(*method == Method::GET || *method == Method::DELETE)
                .open_world(id == "messages.create"),
        );
    if let Some(output) = output {
        let mut schema = JsonObject::new();
        schema.insert("type".to_owned(), json!("object"));
        schema.insert("allOf".to_owned(), json!([output]));
        if !definitions.is_empty() {
            schema.insert("$defs".to_owned(), Value::Object(definitions));
        }
        tool = tool.with_raw_output_schema(Arc::new(schema));
    }
    Ok(Operation {
        tool,
        method: method.clone(),
        path: path.to_owned(),
        path_params,
        query_params,
        header_params,
        body: json_body.is_some(),
        scopes,
    })
}

/// The JSON schema of the operation's first success response with a body.
fn success_schema(operation: &Value) -> Option<&Value> {
    let responses = operation.get("responses")?.as_object()?;
    responses
        .iter()
        .filter(|(status, _)| status.starts_with('2'))
        .find_map(|(_, response)| response.pointer("/content/application~1json/schema"))
}

/// Adds `description` to a schema that has none.
fn describe(schema: &mut Value, description: Option<&Value>) {
    if let (Some(object), Some(description)) = (schema.as_object_mut(), description)
        && !object.contains_key("description")
    {
        object.insert("description".to_owned(), description.clone());
    }
}

/// `value` with every component reference pointing into `$defs`, recording the names referenced.
fn rewrite(mut value: Value, references: &mut BTreeSet<String>) -> Value {
    match &mut value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if key == "$ref"
                    && let Some(name) = child.as_str().and_then(|r| r.strip_prefix(COMPONENT))
                {
                    references.insert(name.to_owned());
                    *child = Value::String(format!("{DEFINITION}{name}"));
                } else {
                    *child = rewrite(child.take(), references);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                *item = rewrite(item.take(), references);
            }
        }
        _ => {}
    }
    value
}

/// The component schemas `roots` name and every schema they reference in turn, rewritten.
fn definitions(roots: &BTreeSet<String>, schemas: &Map<String, Value>) -> Map<String, Value> {
    let mut pending: Vec<String> = roots.iter().cloned().collect();
    let mut definitions = Map::new();
    while let Some(name) = pending.pop() {
        if definitions.contains_key(&name) {
            continue;
        }
        let Some(schema) = schemas.get(&name) else {
            continue;
        };
        let mut found = BTreeSet::new();
        let schema = rewrite(schema.clone(), &mut found);
        definitions.insert(name, schema);
        pending.extend(
            found
                .into_iter()
                .filter(|name| !definitions.contains_key(name)),
        );
    }
    definitions
}

#[cfg(test)]
mod tests {

    use super::*;

    /// Every public operation is a tool, except those whose body is not JSON, and every tool has
    /// a scope its handler checks; a new resource fails here until it is given its area, so the
    /// catalogue never silently loses an operation or shows one to a caller who cannot call it.
    #[test]
    fn every_public_operation_is_a_tool() {
        let catalogue = &*CATALOGUE;
        assert!(catalogue.get("people.list").is_some());
        assert_eq!(
            catalogue.left_out,
            [("images.create".to_owned(), "its request body is not JSON")],
            "only an upload whose body is not JSON may be left out of the MCP catalogue"
        );
        for operation in catalogue.operations.values() {
            assert!(
                !operation.scopes.contains(Scope::WorkspaceManage),
                "{} needs a dashboard-only scope",
                operation.tool.name
            );
        }
    }

    /// Reads need their area's read scope and writes its write scope; an export's tools open to
    /// the scope of any resource it may hold, as its handler accepts (a messages reader may export
    /// messages without any people scope); and every scope a grant can carry opens at least one
    /// tool, so `tools/list` follows the grant exactly.
    #[test]
    fn tools_follow_the_scopes() {
        let set = |scopes: &[Scope]| -> ScopeSet { scopes.iter().copied().collect() };
        assert_eq!(
            scopes_of("exports.create", &Method::POST),
            Some(set(&[
                Scope::PeopleWrite,
                Scope::MessagesRead,
                Scope::InboxRead
            ]))
        );
        assert_eq!(
            scopes_of("exports.retrieve", &Method::GET),
            Some(set(&[
                Scope::PeopleRead,
                Scope::MessagesRead,
                Scope::InboxRead
            ]))
        );
        assert_eq!(
            scopes_of("people.create", &Method::POST),
            Some(set(&[Scope::PeopleWrite]))
        );
        assert_eq!(
            scope_of("people.list", &Method::GET),
            Some(Scope::PeopleRead)
        );
        assert_eq!(
            scope_of("people.create", &Method::POST),
            Some(Scope::PeopleWrite)
        );
        assert_eq!(
            scope_of("preflight.create", &Method::POST),
            Some(Scope::PeopleRead)
        );
        assert_eq!(
            scope_of("messages.cancel", &Method::POST),
            Some(Scope::MessagesSend)
        );
        assert_eq!(scope_of("unknown.list", &Method::GET), None);
        assert_eq!(
            scopes_of("messages.search", &Method::GET),
            Some(set(&[Scope::MessagesRead, Scope::InboxRead]))
        );
        assert_eq!(
            scopes_of("attachments.retrieve", &Method::GET),
            Some(set(&[
                Scope::MessagesRead,
                Scope::InboxRead,
                Scope::MessagesSend
            ]))
        );
        assert_eq!(
            scope_of("attachments.create", &Method::POST),
            Some(Scope::MessagesSend)
        );
        assert_eq!(
            scope_of("metrics.retrieve", &Method::GET),
            Some(Scope::AnalyticsRead)
        );
        let reachable: BTreeSet<Scope> = CATALOGUE
            .operations
            .values()
            .flat_map(|operation| operation.scopes.iter())
            .collect();
        for scope in
            Scope::iter().filter(|scope| crate::domain::oauth::grantable().contains(*scope))
        {
            if matches!(scope, Scope::InboxRead | Scope::InboxWrite) && !reachable.contains(&scope)
            {
                // The inbox has no public operation yet.
                continue;
            }
            assert!(reachable.contains(&scope), "no tool needs `{scope}`");
        }
        let readers: ScopeSet = [Scope::PeopleRead].into_iter().collect();
        let visible: Vec<&str> = CATALOGUE
            .visible(readers)
            .map(|operation| operation.tool.name.as_ref())
            .collect();
        assert!(visible.contains(&"people.list") && visible.contains(&"preflight.create"));
        assert!(!visible.contains(&"people.create"));
        let historians = set(&[Scope::MessagesRead]);
        assert!(
            CATALOGUE
                .visible(historians)
                .any(|operation| operation.tool.name == "exports.create")
        );
    }

    /// A tool's schemas stand alone: every reference points into its own `$defs`, exactly the
    /// operations whose request takes an `Idempotency-Key` (an effectful `POST`, a `PATCH`) take
    /// an idempotency key, never a read, a delete or a preflight check, which the API would
    /// ignore it on, headers become snake-case arguments, and reads are annotated read-only.
    #[test]
    fn tool_schemas_stand_alone() {
        for operation in CATALOGUE.operations.values() {
            let input = Value::Object((*operation.tool.input_schema).clone());
            let text = input.to_string();
            assert!(!text.contains(COMPONENT), "{}", operation.tool.name);
            let mut seen = BTreeSet::new();
            rewrite(input.clone(), &mut seen);
            let mut names = BTreeSet::new();
            collect_definitions(&input, &mut names);
            for name in &seen {
                assert!(
                    names.contains(name),
                    "{} lacks `{name}`",
                    operation.tool.name
                );
            }
            let has_key = input.pointer("/properties/idempotency_key").is_some();
            assert_eq!(
                has_key,
                takes_key(&operation.path, &operation.method),
                "{}",
                operation.tool.name
            );
            let read_only = operation
                .tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint);
            assert_eq!(read_only, Some(operation.method == Method::GET));
        }
        let keyed = |name: &str| {
            CATALOGUE
                .get(name)
                .unwrap()
                .tool
                .input_schema
                .get("properties")
                .and_then(|properties| properties.get(IDEMPOTENCY_KEY))
                .is_some()
        };
        assert!(keyed("people.create") && keyed("people.update"));
        assert!(!keyed("people.delete") && !keyed("preflight.create") && !keyed("people.list"));
        let update = CATALOGUE.get("people.update").unwrap();
        assert_eq!(
            update.header_params,
            vec![("if_match".to_owned(), "If-Match".to_owned())]
        );
        assert_eq!(update.path_params, vec!["id".to_owned()]);
        assert!(update.body && update.tool.output_schema.is_some());
    }

    fn collect_definitions(schema: &Value, names: &mut BTreeSet<String>) {
        if let Some(definitions) = schema.get("$defs").and_then(Value::as_object) {
            names.extend(definitions.keys().cloned());
        }
    }
}
