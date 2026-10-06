//! The api role's routers: the product surface (the `/v1` API) and the ingress surface (provider
//! webhooks, unsubscribes with their page and RFC 8058's one-click `POST`, and public images),
//! served by separate processes on the public host so that a flood of webhooks or recipients
//! cannot starve the API.
//!
//! Every `/v1` module exposes `routes() -> OpenApiRouter<AppState>`; [`v1`] merges them, so
//! the OpenAPI document is derived from the handlers themselves ([`openapi`]). The middleware
//! is the same for both surfaces: the request context (id, path, canonical event), a
//! 30-second deadline, a 1 MiB default body limit, and panics turned into problems; `/v1`
//! adds authentication, then the rate limits (`ratelimit`), then idempotency.
//!
//! # The document's rules
//!
//! What holds for every operation is written into the document once, after the handlers'
//! paths are merged, rather than repeated (and forgettable) in each handler: every `4xx` and
//! `5xx` response is a problem document, an operation tagged `Dashboard` carries
//! `x-surface: dashboard`, and every enum a response carries is open (`x-open-enum: true`). The
//! rules every operation must keep in its own declaration (its id, tag, summary, examples,
//! security, typed ids, `Location` on `202`, versions and `If-Match` on updates, the shape of
//! public paths, a declared bound on every list of objects it answers, its place in the
//! surface, and which answer each form of a body receives where it says so) are held by the
//! invariants test at the end of this file, which reads the whole document and names every
//! operation that breaks one; a second test reads the handlers' source and holds that each one
//! authorizes before anything else.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use utoipa::OpenApi as _;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa_axum::router::OpenApiRouter;

use super::{AppState, context};
use crate::db::Database;
use crate::problem::{Code, Problem};

/// The default body limit for JSON requests; operations that accept more raise their own.
const BODY_LIMIT: usize = 1 << 20;
/// The request deadline; database work is bounded more tightly by the role's timeouts.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Norbelys API",
        version = "v1",
        description = "One `/v1` of resources. Errors are RFC 9457 problems; lists are cursor pages; effectful requests take an `Idempotency-Key`."
    ),
    // Schemas no response or request body reaches, which the handlers' paths therefore do not
    // register: the problem document, and the list parameters every list shares.
    components(schemas(
        crate::problem::Body,
        crate::pagination::Order,
        crate::pagination::Include
    )),
    modifiers(&Security)
)]
struct ApiDoc;

struct Security;

impl utoipa::Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).description(Some("An API key (`nb_live_…`, `nb_test_…`), a workspace token (`nbs_…`) or a CLI token (`nbc_…`).")).build()),
        );
        // The dashboard's session operations (the account, minting workspace tokens, listing and
        // creating workspaces) take the browser's session cookie instead of a bearer.
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(utoipa::openapi::security::ApiKey::Cookie(
                utoipa::openapi::security::ApiKeyValue::with_description(
                    "__Host-nb_session",
                    "The browser's session cookie, with the `X-CSRF-Token` header on every change.",
                ),
            )),
        );
    }
}

/// The `/v1` operations, with their OpenAPI paths.
pub fn v1() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(crate::analytics::http::routes())
        .merge(crate::analytics::metrics::routes())
        .merge(crate::campaigns::http::routes())
        .merge(crate::delivery::http::routes())
        .merge(crate::delivery::content::routes())
        .merge(crate::delivery::attachments::routes())
        .merge(crate::identity::workspaces::routes())
        .merge(crate::identity::http::routes())
        .merge(crate::identity::ceremonies::routes())
        .merge(crate::identity::recovery::routes())
        .merge(crate::inbox::http::routes())
        .merge(crate::jobs::http::routes())
        .merge(crate::people::http::routes())
        .merge(crate::senders::http::routes())
        .merge(crate::tracking::http::routes())
        .merge(crate::webhooks::http::routes())
}

/// The OpenAPI document of `/v1`, derived from the handlers and completed with what holds for
/// every operation ([`complete`]).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    let (_, mut document) = OpenApiRouter::<AppState>::with_openapi(ApiDoc::openapi())
        .nest("/v1", v1())
        .split_for_parts();
    complete(&mut document);
    document
}

/// The tag every dashboard operation carries.
const DASHBOARD: &str = "Dashboard";

/// Writes into `document` what holds for every operation. It runs on the merged document:
/// `ApiDoc`'s own modifiers run before the handlers' paths are added, so they never see an
/// operation.
///
/// - An error answer is always a problem document, because [`Problem`] is the only way an error
///   reaches a client: each `4xx` and `5xx` response an operation declares without content gets
///   `application/problem+json` with the `Problem` schema. A response declared with other
///   content keeps it, so the invariants test still refuses it.
/// - An operation tagged `Dashboard` belongs to the dashboard surface (a browser session only):
///   `x-surface: dashboard` tells the generators of the public reference, the SDK, the CLI and
///   the MCP catalogue to leave it out.
/// - The idempotency middleware's header is declared once: required on effectful POSTs and
///   optional on PATCHes, with the same excluded sign-in and preflight paths.
/// - Every enum a response carries is open, `x-open-enum: true`: a value added later must not
///   break a client generated today, so the SDK reads an unknown value as `unknown` instead of
///   failing. The mark is written on every enum reachable from a response's schema, through the
///   component schemas it refers to; a request's enums stay closed (the server refuses an
///   unknown value), which a shared component does not change, because the mark only tells
///   readers how to read.
fn complete(document: &mut utoipa::openapi::OpenApi) {
    use utoipa::openapi::schema::Ref;
    use utoipa::openapi::{Content, RefOr};

    // The component schemas the responses refer to, whose enums are opened below.
    let mut answered = Vec::new();
    for (path, item) in &mut document.paths.paths {
        let operations = [
            ("GET", &mut item.get),
            ("PUT", &mut item.put),
            ("POST", &mut item.post),
            ("DELETE", &mut item.delete),
            ("OPTIONS", &mut item.options),
            ("HEAD", &mut item.head),
            ("PATCH", &mut item.patch),
            ("TRACE", &mut item.trace),
        ];
        for (method, slot) in operations {
            let Some(operation) = slot else { continue };
            if crate::idempotency::takes_key(method, path) {
                use utoipa::openapi::path::{ParameterBuilder, ParameterIn};
                use utoipa::openapi::schema::{ObjectBuilder, Type};
                operation.parameters.get_or_insert_with(Vec::new).push(
                    ParameterBuilder::new()
                        .name("Idempotency-Key")
                        .parameter_in(ParameterIn::Header)
                        .required(if method == "POST" {
                            utoipa::openapi::Required::True
                        } else {
                            utoipa::openapi::Required::False
                        })
                        .description(Some(
                            "A stable key for this logical request; reuse it when retrying.",
                        ))
                        .schema(Some(
                            ObjectBuilder::new()
                                .schema_type(Type::String)
                                .min_length(Some(1))
                                .max_length(Some(128))
                                .pattern(Some("^[!-~]+$")),
                        ))
                        .build()
                        .into(),
                );
            }
            for (status, response) in &mut operation.responses.responses {
                let error = status.starts_with('4') || status.starts_with('5');
                if let RefOr::T(response) = response
                    && error
                    && response.content.is_empty()
                {
                    response.content.insert(
                        "application/problem+json".to_owned(),
                        RefOr::T(Content::new(Some(Ref::from_schema_name("Problem")))),
                    );
                }
                if let RefOr::T(response) = response {
                    for content in response.content.values_mut() {
                        if let RefOr::T(content) = content
                            && let Some(schema) = content.schema.as_mut()
                        {
                            open_enums(schema, &mut answered);
                        }
                    }
                }
            }
            let dashboard = operation
                .tags
                .as_ref()
                .is_some_and(|tags| tags.iter().any(|tag| tag == DASHBOARD));
            if dashboard {
                operation
                    .extensions
                    .get_or_insert_with(Default::default)
                    .insert("x-surface".to_owned(), serde_json::Value::from("dashboard"));
            }
        }
    }
    // Each component a response reaches, once: a schema may refer to itself.
    let mut opened = BTreeSet::new();
    while let Some(name) = answered.pop() {
        if !opened.insert(name.clone()) {
            continue;
        }
        if let Some(schema) = document
            .components
            .as_mut()
            .and_then(|components| components.schemas.get_mut(&name))
        {
            open_enums(schema, &mut answered);
        }
    }
}

/// Marks every enum of `schema` open (`x-open-enum: true`) and collects the names of the
/// component schemas it refers to into `refs`, for the caller to open in turn.
fn open_enums(
    schema: &mut utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
    refs: &mut Vec<String>,
) {
    use utoipa::openapi::RefOr;
    use utoipa::openapi::schema::{AdditionalProperties, ArrayItems, Schema};

    let schema = match schema {
        RefOr::Ref(reference) => {
            if let Some(name) = reference.ref_location.strip_prefix(SCHEMAS) {
                refs.push(name.to_owned());
            }
            return;
        }
        RefOr::T(schema) => schema,
    };
    match schema {
        Schema::Object(object) => {
            if object.enum_values.is_some() {
                object
                    .extensions
                    .get_or_insert_with(Default::default)
                    .insert("x-open-enum".to_owned(), serde_json::Value::Bool(true));
            }
            for property in object.properties.values_mut() {
                open_enums(property, refs);
            }
            if let Some(AdditionalProperties::RefOr(values)) =
                object.additional_properties.as_deref_mut()
            {
                open_enums(values, refs);
            }
        }
        Schema::Array(array) => {
            if let ArrayItems::RefOrSchema(items) = &mut array.items {
                open_enums(items, refs);
            }
        }
        Schema::OneOf(one_of) => {
            for member in &mut one_of.items {
                open_enums(member, refs);
            }
        }
        Schema::AllOf(all_of) => {
            for member in &mut all_of.items {
                open_enums(member, refs);
            }
        }
        Schema::AnyOf(any_of) => {
            for member in &mut any_of.items {
                open_enums(member, refs);
            }
        }
        // A kind of schema added to the library later carries no enum this walk knows about;
        // the invariants test, which reads the written document, would name it.
        _ => {}
    }
}

/// How a reference to a component schema begins.
const SCHEMAS: &str = "#/components/schemas/";

/// The product surface: `/v1`, the dashboard surface and the protocols, the MCP server included.
pub fn product(state: AppState) -> Router {
    let (v1, _) = v1().split_for_parts();
    let v1 = v1
        .layer(middleware::from_fn_with_state(
            state.db.clone(),
            crate::idempotency::layer,
        ))
        // After authentication (the credential decides the budget), before idempotency (a
        // replayed answer spends too): see `ratelimit`.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            super::ratelimit::layer,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::identity::authority::authenticate,
        ));
    let routes = Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .merge(super::openapi::routes())
        // The public keys of workspace tokens and OAuth access tokens.
        .route(
            "/.well-known/jwks.json",
            get(crate::identity::tokens::key_set),
        )
        // The OAuth authorization server: metadata, authorize, consent, device, token, revoke;
        // while a deployment keeps it off (`OAUTH_SERVER_DISABLED`), none of its routes exists.
        .merge(if state.identity.oauth_server {
            crate::identity::oauth::routes()
        } else {
            Router::new()
        })
        // Downloads from a local object store, by signed link (see `storage`).
        .route("/files/{*key}", get(crate::storage::serve_local))
        // Unsubscribes and public images; the ingress mode serves them on the public host.
        .merge(crate::tracking::http::public())
        .merge(crate::tracking::domains::routes())
        // Provider webhooks; a deployment points providers at the ingress mode.
        .merge(crate::webhooks::ingress::routes())
        .nest("/v1", v1);
    // The MCP server executes each tool through the surface without itself, in process.
    let api = with_middleware(routes.clone()).with_state(state.clone());
    with_middleware(routes.merge(crate::mcp::routes(&state, api))).with_state(state)
}

/// The ingress surface: provider webhooks, unsubscribes (the page and the one-click `POST`) and
/// public images only, beside the health probes.
pub fn ingress(state: AppState) -> Router {
    with_middleware(
        Router::new()
            .route("/health/live", get(live))
            .route("/health/ready", get(ready))
            .merge(crate::tracking::http::public())
            .merge(crate::tracking::domains::routes())
            .merge(crate::webhooks::ingress::routes()),
    )
    .with_state(state)
}

fn with_middleware(router: Router<AppState>) -> Router<AppState> {
    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn(deadline))
        .layer(CatchPanicLayer::custom(
            |_panic: Box<dyn std::any::Any + Send>| {
                Problem::internal(&"a handler panicked").into_response()
            },
        ))
        // Outermost: a panic turned into a problem is still measured and recorded, with the
        // request's id in the problem.
        .layer(middleware::from_fn(context::layer))
}

async fn deadline(request: Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_DEADLINE, next.run(request)).await {
        Ok(response) => response,
        Err(_) => Problem::new(Code::Timeout, "The request took too long.").into_response(),
    }
}

async fn live() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn ready(State(db): State<Database>) -> Response {
    if db.ping().await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Problem::unavailable(2).into_response()
    }
}

async fn not_found() -> Problem {
    Problem::new(Code::NotFound, "No operation at this path.")
}

async fn method_not_allowed() -> Problem {
    Problem::new(
        Code::MethodNotAllowed,
        "The operation at this path does not accept this method.",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{Value, json};

    use super::{DASHBOARD, openapi};

    /// The keys under which a path item holds its operations.
    const METHODS: [&str; 8] = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];
    /// How a reference to a component schema begins.
    const SCHEMA_REF: &str = "#/components/schemas/";
    /// The longest summary, in characters: a title on one line.
    const SUMMARY_MAX: usize = 100;

    /// `schema` itself, or the component schema it references, followed through references.
    fn resolve<'a>(document: &'a Value, schema: &'a Value) -> &'a Value {
        let mut schema = schema;
        for _ in 0..8 {
            let Some(next) = schema["$ref"]
                .as_str()
                .and_then(|target| target.strip_prefix(SCHEMA_REF))
                .and_then(|name| document["components"]["schemas"].get(name))
            else {
                break;
            };
            schema = next;
        }
        schema
    }

    /// Whether `schema` is an id (`Id<R>`): a string of one resource's prefix and 32 hexadecimal
    /// digits, alone or as one alternative among several (an id or `current`, say).
    fn is_id(document: &Value, schema: &Value) -> bool {
        let schema = resolve(document, schema);
        let prefixed = schema["type"] == "string"
            && schema["pattern"]
                .as_str()
                .and_then(|pattern| pattern.strip_prefix('^'))
                .and_then(|pattern| pattern.strip_suffix("_[0-9a-f]{32}$"))
                .is_some_and(|prefix| {
                    !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_lowercase())
                });
        prefixed
            || ["oneOf", "anyOf"]
                .iter()
                .filter_map(|key| schema[*key].as_array())
                .flatten()
                .any(|alternative| is_id(document, alternative))
    }

    /// Whether `text` is a snake_case name: a lowercase letter, then lowercase letters, digits
    /// and underscores.
    fn is_name(text: &str) -> bool {
        text.starts_with(|first: char| first.is_ascii_lowercase())
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }

    /// Whether `response` declares the header `name`, whose case does not matter.
    fn declares(response: &Value, name: &str) -> bool {
        response["headers"]
            .as_object()
            .is_some_and(|headers| headers.keys().any(|key| key.eq_ignore_ascii_case(name)))
    }

    /// The schema of `response`'s JSON body, resolved; `None` without a JSON body.
    fn body<'a>(document: &'a Value, response: &'a Value) -> Option<&'a Value> {
        response
            .pointer("/content/application~1json/schema")
            .map(|schema| resolve(document, schema))
    }

    /// Whether `schema` is an object with a `version` property: a resource that can be updated.
    fn versioned(schema: Option<&Value>) -> bool {
        schema.is_some_and(|schema| schema["properties"].get("version").is_some())
    }

    /// The ids a public operation may have at `path` with `method`: `GET /r` lists (or reads a
    /// resource that is one object, such as a report), `POST /r` creates, `GET`, `PATCH` and
    /// `DELETE /r/{id}` retrieve, update and delete, and `POST /r/{id}/v` runs the action `v`.
    /// `None` where no public operation may be: deeper paths, other methods.
    fn public_ids(path: &str, method: &str) -> Option<Vec<String>> {
        let segments: Vec<&str> = path.strip_prefix("/v1/")?.split('/').collect();
        let parameter = |segment: &str| segment.starts_with('{') && segment.ends_with('}');
        match (segments.as_slice(), method) {
            (["messages", "search"], "get") => Some(vec!["messages.search".to_owned()]),
            ([resource @ ("messages" | "inbound_messages"), id, "content"], "get")
                if parameter(id) =>
            {
                Some(vec![format!("{resource}.content")])
            }
            ([resource], "get") if is_name(resource) => Some(vec![
                format!("{resource}.list"),
                format!("{resource}.retrieve"),
            ]),
            ([resource], "post") if is_name(resource) => Some(vec![format!("{resource}.create")]),
            ([resource, id], "get") if is_name(resource) && parameter(id) => {
                Some(vec![format!("{resource}.retrieve")])
            }
            ([resource, id], "patch") if is_name(resource) && parameter(id) => {
                Some(vec![format!("{resource}.update")])
            }
            ([resource, id], "delete") if is_name(resource) && parameter(id) => {
                Some(vec![format!("{resource}.delete")])
            }
            ([resource, id, action], "post")
                if is_name(resource) && parameter(id) && is_name(action) =>
            {
                Some(vec![format!("{resource}.{action}")])
            }
            _ => None,
        }
    }

    /// Every rule the operation at `method` on `path` breaks, in words.
    fn broken(document: &Value, path: &str, method: &str, operation: &Value) -> Vec<String> {
        let mut broken = Vec::new();
        let id = operation["operationId"].as_str().unwrap_or_default();
        let shaped = id
            .split_once('.')
            .is_some_and(|(resource, action)| is_name(resource) && is_name(action));
        if !shaped {
            broken.push("its id is not `<resource>.<action>`".to_owned());
        }
        let tags: Vec<&str> = operation["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if tags.is_empty() {
            broken.push("it has no tag".to_owned());
        }
        let summary = operation["summary"].as_str().unwrap_or_default();
        if summary.trim().is_empty() {
            broken.push(
                "it has no summary (the first paragraph of the handler's documentation)".to_owned(),
            );
        } else if summary.contains('\n') || summary.chars().count() > SUMMARY_MAX {
            // The summary is a title: of a reference's entry, a generated method's first line,
            // an MCP tool. Its details belong in the description, the paragraphs after it.
            broken.push(format!(
                "its summary is not one line of at most {SUMMARY_MAX} characters: end the \
                 handler's first paragraph after one sentence and move the rest below it"
            ));
        }

        // The surface and the credential it takes.
        let dashboard = tags.contains(&DASHBOARD);
        match operation.get("x-surface") {
            None if dashboard => {
                broken.push("it is tagged `Dashboard` without `x-surface: dashboard`".to_owned());
            }
            Some(surface) if *surface != "dashboard" => broken.push(format!(
                "its `x-surface` is {surface}; `dashboard` is the only surface marked"
            )),
            Some(_) if !dashboard => {
                broken.push("it has `x-surface: dashboard` without the tag `Dashboard`".to_owned());
            }
            _ => {}
        }
        match operation.get("security") {
            None => broken.push(
                "it declares no `security` (`security(())` when it takes no credential)".to_owned(),
            ),
            Some(security) if !dashboard && *security != json!([{ "bearer": [] }]) => broken.push(
                "a public operation takes a bearer credential: `security((\"bearer\" = []))`"
                    .to_owned(),
            ),
            Some(_) => {}
        }

        // What it takes.
        if let Some(content) = operation
            .pointer("/requestBody/content")
            .and_then(Value::as_object)
        {
            for (media, entry) in content {
                let example = entry.get("example").is_some()
                    || entry["examples"]
                        .as_object()
                        .is_some_and(|examples| !examples.is_empty());
                if !example {
                    broken.push(format!("its `{media}` request body has no example"));
                }
            }
        }
        let parameters: Vec<&Value> = operation["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        let keyed = crate::idempotency::takes_key(&method.to_ascii_uppercase(), path);
        let keys: Vec<_> = parameters
            .iter()
            .filter(|parameter| parameter["name"] == "Idempotency-Key")
            .collect();
        if keyed {
            if keys.len() != 1
                || !keys.iter().all(|key| {
                    key["in"] == "header"
                        && key["required"] == (method == "post")
                        && key["schema"]["minLength"] == 1
                        && key["schema"]["maxLength"] == 128
                        && key["schema"]["pattern"] == "^[!-~]+$"
                })
            {
                broken
                    .push("its idempotency header differs from the middleware contract".to_owned());
            }
        } else if !keys.is_empty() {
            broken.push("it declares an idempotency key the middleware does not accept".to_owned());
        }
        for parameter in &parameters {
            if parameter["in"] == "path" && !is_id(document, &parameter["schema"]) {
                broken.push(format!(
                    "its path parameter `{}` is not typed as an id (`Id<R>`)",
                    parameter["name"].as_str().unwrap_or_default()
                ));
            }
        }

        // What it answers.
        let responses = operation["responses"].as_object();
        for (status, response) in responses.into_iter().flatten() {
            if status.starts_with('4') || status.starts_with('5') {
                let problem = response["content"].as_object().is_some_and(|content| {
                    content.len() == 1
                        && content
                            .get("application/problem+json")
                            .and_then(|media| media.pointer("/schema/$ref"))
                            == Some(&json!("#/components/schemas/Problem"))
                });
                if !problem {
                    broken.push(format!(
                        "its `{status}` is not a problem document (`application/problem+json`, the `Problem` schema)"
                    ));
                }
            }
            let schema = body(document, response);
            let consent = schema.is_some_and(|schema| {
                schema["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&json!("authorization")))
            });
            if status == "202" && !declares(response, "Location") && !consent {
                broken.push("its `202` declares no `Location` for the client to poll".to_owned());
            }
            if status.starts_with('2') && versioned(schema) && !declares(response, "ETag") {
                broken.push(format!(
                    "its `{status}` returns a versioned resource without declaring its `ETag`"
                ));
            }
        }
        // Which answer each form of the body receives (`x-norbelys-overloads`, read by the SDK's
        // generator): every form named is a form of the body, every answer one it declares.
        if let Some(overloads) = operation.get("x-norbelys-overloads") {
            let forms: Vec<&Value> = operation
                .pointer("/requestBody/content/application~1json/schema")
                .and_then(|schema| resolve(document, schema)["oneOf"].as_array())
                .into_iter()
                .flatten()
                .collect();
            let answers: Vec<&Value> = responses
                .into_iter()
                .flatten()
                .filter(|(status, _)| status.starts_with('2'))
                .filter_map(|(_, response)| response.pointer("/content/application~1json/schema"))
                .flat_map(|schema| {
                    let members = resolve(document, schema)["oneOf"].as_array();
                    std::iter::once(schema).chain(members.into_iter().flatten())
                })
                .collect();
            let overloads = overloads.as_array().into_iter().flatten();
            for (index, overload) in overloads.enumerate() {
                if !forms.contains(&&overload["request"]) {
                    broken.push(format!(
                        "its overload {index} names a request that is not a form of its body"
                    ));
                }
                if !answers.contains(&&overload["response"]) {
                    broken.push(format!(
                        "its overload {index} names an answer it does not declare"
                    ));
                }
            }
        }
        if method == "patch" {
            let if_match = parameters.iter().any(|parameter| {
                parameter["in"] == "header"
                    && parameter["name"] == "If-Match"
                    && parameter["required"] != true
            });
            if !if_match {
                broken.push(
                    "an update takes the optional `If-Match` header: list `IfMatch` in its `params`"
                        .to_owned(),
                );
            }
            if !responses.is_some_and(|responses| responses.contains_key("412")) {
                broken.push("an update declares `412` (a stale `If-Match`)".to_owned());
            }
            let answer = responses
                .and_then(|responses| responses.get("200"))
                .and_then(|response| body(document, response));
            if !versioned(answer) {
                broken
                    .push("an update answers `200` with the resource and its `version`".to_owned());
            }
        }

        // Where a public operation may be.
        if !dashboard {
            match public_ids(path, method) {
                None => broken.push(
                    "a public path is a resource, its id and an action at most, with the standard methods (`POST` for an action)"
                        .to_owned(),
                ),
                Some(ids) if !ids.iter().any(|expected| expected == id) => broken.push(format!(
                    "its id at this path and method is `{}`",
                    ids.join("` or `")
                )),
                Some(_) => {}
            }
        }
        broken
    }

    /// Every operation the API serves: the public resources' and the dashboard's. The surface grows
    /// only by decision, so an operation is listed here when it is meant to exist, and the
    /// invariants test refuses one that is not listed, as well as a listed one that nothing serves.
    const SURFACE: &[&str] = &[
        // Sending.
        "smtp_authorization.retrieve",
        "connections.list",
        "connections.create",
        "connections.retrieve",
        "connections.update",
        "connections.delete",
        "connections.verify",
        "quota_scopes.list",
        "quota_scopes.create",
        "quota_scopes.retrieve",
        "quota_scopes.update",
        "quota_scopes.delete",
        "sending_domains.list",
        "sending_domains.create",
        "sending_domains.retrieve",
        "sending_domains.update",
        "sending_domains.delete",
        "sending_domains.verify",
        // Audience.
        "people.list",
        "people.create",
        "people.retrieve",
        "people.update",
        "people.delete",
        "fields.list",
        "fields.create",
        "fields.update",
        "fields.delete",
        "groups.list",
        "groups.create",
        "groups.retrieve",
        "groups.update",
        "groups.delete",
        "segments.list",
        "segments.create",
        "segments.retrieve",
        "segments.update",
        "segments.delete",
        "imports.list",
        "imports.create",
        "imports.retrieve",
        "exports.list",
        "exports.create",
        "exports.retrieve",
        "suppressions.list",
        "suppressions.create",
        "suppressions.retrieve",
        "suppressions.delete",
        "preflight.create",
        // Campaigns.
        "campaigns.list",
        "campaigns.create",
        "campaigns.retrieve",
        "campaigns.update",
        "campaigns.delete",
        "campaigns.start",
        "campaigns.pause",
        "enrollments.list",
        "enrollments.create",
        "enrollments.retrieve",
        "enrollments.stop",
        // Messages.
        "messages.list",
        "messages.create",
        "messages.search",
        "messages.content",
        "attachments.create",
        "attachments.retrieve",
        "attachments.delete",
        "messages.retrieve",
        "messages.cancel",
        "messages.resolve",
        "messages.release_holds",
        "delivery_events.list",
        "delivery_events.retrieve",
        // Inbox.
        "threads.list",
        "threads.retrieve",
        "threads.update",
        "inbound_messages.list",
        "inbound_messages.content",
        "inbound_messages.retrieve",
        "inbound_messages.update",
        "inbound_messages.review",
        // Automation.
        "jobs.retrieve",
        "jobs.cancel",
        "events.list",
        "events.retrieve",
        "events.create",
        "webhook_endpoints.list",
        "webhook_endpoints.create",
        "webhook_endpoints.retrieve",
        "webhook_endpoints.update",
        "webhook_endpoints.delete",
        "webhook_endpoints.replay",
        "webhook_endpoints.rotate_secret",
        "webhook_deliveries.list",
        "webhook_deliveries.retrieve",
        "webhook_deliveries.retry",
        // Reports, content and the workspace.
        "analytics.retrieve",
        "metrics.retrieve",
        "images.create",
        "images.delete",
        "workspaces.retrieve",
        // The dashboard: signing in.
        "auth.config",
        "challenges.create",
        "sessions.create",
        "auth.callback",
        "tokens.create",
        // The dashboard: the signed-in user.
        "me.retrieve",
        "me.update",
        "passkeys.create",
        "passkeys.delete",
        "sessions.delete",
        "grants.delete",
        "identities.delete",
        "memberships.create",
        "recovery_codes.create",
        // The dashboard: the workspace.
        "workspaces.list",
        "workspaces.create",
        "workspaces.update",
        "workspaces.delete",
        "members.list",
        "members.update",
        "members.delete",
        "invitations.list",
        "invitations.create",
        "invitations.delete",
        "api_keys.list",
        "api_keys.create",
        "api_keys.update",
        "api_keys.delete",
        "sso_connections.list",
        "sso_connections.create",
        "sso_connections.update",
        "sso_connections.delete",
        "sso_connections.verify",
        "audit_log.list",
    ];

    /// The routes the roles serve outside `/v1`, each one's contract a protocol's rather than the
    /// document's: health, Prometheus metrics, the OAuth authorization server and its metadata, the MCP endpoint,
    /// provider webhooks, the tracking role's opens and clicks, one-click unsubscribes, public
    /// images and the signed downloads of a local object store.
    const PROTOCOL: &[&str] = &[
        "/health/live",
        "/health/ready",
        "/metrics",
        "/.well-known/jwks.json",
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource/mcp",
        "/oauth/authorize",
        "/oauth/consent",
        "/oauth/device_authorization",
        "/oauth/token",
        "/oauth/revoke",
        "/mcp",
        "/webhooks/{provider_webhook_id}",
        "/t/o/{token}",
        "/t/c/{token}",
        "/u/{token}",
        "/images/{workspace}/{file}",
        "/files/{*key}",
    ];

    /// The source files (under `src/`) that build the routers the roles serve: the api's product
    /// and ingress surfaces and what they merge, the tracking role's, and the health listener of
    /// the roles without an API.
    const ROUTERS: &[&str] = &[
        "http/router.rs",
        "identity/oauth.rs",
        "mcp.rs",
        "process.rs",
        "tracking/http.rs",
        "webhooks/ingress.rs",
    ];

    /// The text of the crate's source file `file`, a path under `src/`.
    fn source(file: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    /// The paths `source` routes with `.route("…"` or `.route_service("…"`, leaving out its inline
    /// test module, whose fake servers are not served by any role.
    fn routed(source: &str) -> Vec<String> {
        let served = source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or_default();
        let mut paths = Vec::new();
        for call in [".route(", ".route_service("] {
            for (at, _) in served.match_indices(call) {
                let rest = served[at + call.len()..].trim_start();
                if let Some(literal) = rest.strip_prefix('"')
                    && let Some(end) = literal.find('"')
                {
                    paths.push(literal[..end].to_owned());
                }
            }
        }
        paths
    }

    /// The names of the component schemas `value` references anywhere in it, into `names`.
    fn references(value: &Value, names: &mut BTreeSet<String>) {
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    match member
                        .as_str()
                        .and_then(|target| target.strip_prefix(SCHEMA_REF))
                    {
                        Some(name) if key == "$ref" => {
                            names.insert(name.to_owned());
                        }
                        _ => references(member, names),
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    references(item, names);
                }
            }
            _ => {}
        }
    }

    /// Whether `schema` describes an object: by its type, its properties, or an alternative or
    /// part that does.
    fn is_object(document: &Value, schema: &Value, depth: u8) -> bool {
        let schema = resolve(document, schema);
        depth < 8
            && (schema["type"] == "object"
                || schema.get("properties").is_some()
                || ["oneOf", "anyOf", "allOf"]
                    .iter()
                    .filter_map(|key| schema[*key].as_array())
                    .flatten()
                    .any(|member| is_object(document, member, depth + 1)))
    }

    /// Checks `schema`, which a response carries (directly or through other schemas), at `at`:
    /// every enum in it is open, and every list of objects in it declares its bound. Pushes the
    /// names of the component schemas it refers to onto `refs`, for the caller to check once each.
    /// A list of `FieldError`s is the problem's own report of a request's invalid fields, bounded
    /// by that request, so it declares none.
    fn answered(
        document: &Value,
        schema: &Value,
        at: &str,
        refs: &mut Vec<String>,
        broken: &mut Vec<String>,
    ) {
        if let Some(name) = schema["$ref"]
            .as_str()
            .and_then(|target| target.strip_prefix(SCHEMA_REF))
        {
            refs.push(name.to_owned());
            return;
        }
        if schema.get("enum").is_some() && schema["x-open-enum"] != true {
            broken.push(format!(
                "{at}: an enum a response carries is not open (`x-open-enum: true`)"
            ));
        }
        let list = schema["type"] == "array"
            || schema["type"]
                .as_array()
                .is_some_and(|types| types.contains(&json!("array")));
        let items = &schema["items"];
        if list
            && schema.get("maxItems").is_none()
            && is_object(document, items, 0)
            && items["$ref"] != "#/components/schemas/FieldError"
        {
            broken.push(format!(
                "{at}: a list of objects without a declared bound (`#[schema(max_items = …)]`)"
            ));
        }
        for (name, property) in schema["properties"].as_object().into_iter().flatten() {
            answered(document, property, &format!("{at}.{name}"), refs, broken);
        }
        if items.is_object() {
            answered(document, items, &format!("{at}[]"), refs, broken);
        }
        if schema["additionalProperties"].is_object() {
            answered(
                document,
                &schema["additionalProperties"],
                &format!("{at}.*"),
                refs,
                broken,
            );
        }
        for key in ["oneOf", "anyOf", "allOf"] {
            for (index, member) in schema[key].as_array().into_iter().flatten().enumerate() {
                answered(
                    document,
                    member,
                    &format!("{at}.{key}[{index}]"),
                    refs,
                    broken,
                );
            }
        }
    }

    /// The OpenAPI document keeps, for every operation, the rules that make one contract of many
    /// handlers: an id `<resource>.<action>`, unique, and for a public operation the one its
    /// path and method imply (`GET /r` lists, `POST /r` creates, `GET`, `PATCH` and
    /// `DELETE /r/{id}` retrieve, update and delete, `POST /r/{id}/v` runs the action `v`, and
    /// no public path goes deeper); a tag; a summary of one line (the details go in the
    /// description); an example per request body; a problem document for every `4xx` and `5xx`
    /// it declares; its security (a bearer credential on the public surface, while the dashboard
    /// surface is tagged `Dashboard` and marked `x-surface: dashboard`); path ids typed as ids; a
    /// `Location` on every `202` that is not a consent for the browser; an `ETag` on every
    /// response that returns one versioned resource; on every `PATCH` the optional `If-Match`,
    /// the `412` and the resource's `version`; and, where it declares which answer each form of
    /// its body receives (`x-norbelys-overloads`), forms of its body and answers it declares, so
    /// a generated method's overloads are true.
    ///
    /// Across the document: every enum a response carries is open (`x-open-enum: true`), so a
    /// value added later cannot break a generated client; every list of objects a response
    /// carries declares its bound, so a response's size and the work behind it are bounded; every
    /// schema reference resolves, so a generator never meets a type it cannot find; and nothing
    /// is served outside the surface: the operations are exactly [`SURFACE`], and the routers
    /// serve no route outside `/v1` but [`PROTOCOL`].
    ///
    /// The SDK, the CLI, the MCP catalogue and the reference are generated from this document, so
    /// an operation that breaks a rule ships the break to every client; the failure names every
    /// broken rule of every operation at once.
    #[test]
    fn every_operation_keeps_the_documents_rules() {
        let document = serde_json::to_value(openapi()).unwrap();
        let mut broken = Vec::new();
        if document.pointer("/components/schemas/Problem").is_none() {
            broken.push("the document has no `Problem` schema".to_owned());
        }
        let mut ids = BTreeSet::new();
        let mut refs = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for method in METHODS {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                let id = operation["operationId"].as_str().unwrap_or("no id");
                let name = format!("{} {path} ({id})", method.to_uppercase());
                if !ids.insert(id.to_owned()) {
                    broken.push(format!("{name}: another operation has its id"));
                }
                for rule in self::broken(&document, path, method, operation) {
                    broken.push(format!("{name}: {rule}"));
                }
                for (status, response) in operation["responses"].as_object().into_iter().flatten() {
                    for (media, content) in response["content"].as_object().into_iter().flatten() {
                        let at = format!("{name} {status} {media}");
                        answered(&document, &content["schema"], &at, &mut refs, &mut broken);
                    }
                }
            }
        }

        // The component schemas the responses reach, each once.
        let mut checked = BTreeSet::new();
        while let Some(schema) = refs.pop() {
            if checked.insert(schema.clone()) {
                let at = format!("the schema `{schema}`");
                let found = &document["components"]["schemas"][schema.as_str()];
                answered(&document, found, &at, &mut refs, &mut broken);
            }
        }

        // Every reference names a schema the document holds: a type used only by a parameter
        // is not registered by its handler's path, and must be listed in `ApiDoc`'s components.
        let mut named = BTreeSet::new();
        references(&document, &mut named);
        for name in named {
            if document["components"]["schemas"]
                .get(name.as_str())
                .is_none()
            {
                broken.push(format!(
                    "`#/components/schemas/{name}` is referenced but not in the document"
                ));
            }
        }

        // The surface: exactly the listed operations, and no other route outside `/v1`.
        let surface: BTreeSet<&str> = SURFACE.iter().copied().collect();
        for id in &ids {
            if !surface.contains(id.as_str()) {
                broken.push(format!(
                    "`{id}` is not an operation of the surface (`SURFACE` lists one only when the surface is meant to grow)"
                ));
            }
        }
        for id in &surface {
            if !ids.contains(*id) {
                broken.push(format!("the surface's `{id}` is not in the document"));
            }
        }
        let routes: BTreeSet<String> = ROUTERS
            .iter()
            .flat_map(|file| routed(&source(file)))
            .collect();
        for route in &routes {
            if !PROTOCOL.contains(&route.as_str()) {
                broken.push(format!(
                    "`{route}` is served outside `/v1` without being a protocol route"
                ));
            }
        }
        for route in PROTOCOL {
            if !routes.contains(*route) {
                broken.push(format!(
                    "the protocol route `{route}` is not served by any router"
                ));
            }
        }
        assert!(
            broken.is_empty(),
            "{} rules of the OpenAPI document are broken:\n{}",
            broken.len(),
            broken.join("\n")
        );
    }

    /// The calls that may open the body of a handler that takes a `Principal`: the only
    /// authorization call and its documented variants (a member-owned resource, the dashboard's
    /// session surface, the owner's actions), and the dashboard's workspace check, which begins
    /// with the session check.
    const AUTHORIZE: &[&str] = &[
        "principal.require(",
        "principal.require_owned(",
        "principal.require_session(",
        "principal.require_owner(",
        "dashboard(&principal,",
    ];

    /// The operations whose handlers take no `Principal`: the anonymous steps of signing in, and
    /// the operations of the browser's session itself (its account, the workspaces it lists or
    /// creates before it holds a workspace token, and minting that token), which the session
    /// extractor (`SignedIn`) authorizes.
    const WITHOUT_PRINCIPAL: &[&str] = &[
        "auth.config",
        "auth.callback",
        "challenges.create",
        "sessions.create",
        "me.retrieve",
        "me.update",
        "passkeys.create",
        "passkeys.delete",
        "sessions.delete",
        "grants.delete",
        "identities.delete",
        "memberships.create",
        "recovery_codes.create",
        "tokens.create",
        "workspaces.list",
        "workspaces.create",
    ];

    /// A handler as its source declares it.
    struct Handler {
        /// Its operation id.
        id: String,
        /// Its file under `src/`.
        file: String,
        /// Its parameters, as written.
        parameters: String,
        /// The first statement of its body, comments skipped.
        first: String,
    }

    /// Every Rust source file of the crate: its path under `src/` and its text.
    fn sources() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut directories = vec![root.clone()];
        let mut files = Vec::new();
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let name = path.strip_prefix(&root).unwrap().display().to_string();
                    files.push((name, std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        files.sort();
        files
    }

    /// Every handler of `source` (the file `file`): from each `#[utoipa::path]` attribute, its
    /// operation id, the parameters of the function it documents and the first statement of
    /// that function's body. The parameters end at the parenthesis that closes them, since a
    /// destructuring pattern among them may hold braces.
    fn handlers(file: &str, source: &str) -> Vec<Handler> {
        // Built from two pieces so that this file's own text never reads as an attribute.
        let attribute = concat!("#[utoipa::", "path(");
        let mut found = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find(attribute) {
            rest = &rest[at + attribute.len()..];
            let Some(start) = rest.find("async fn ") else {
                break;
            };
            let id = rest[..start]
                .split("operation_id = \"")
                .nth(1)
                .and_then(|tail| tail.split('"').next())
                .unwrap_or("no id")
                .to_owned();
            let signature = &rest[start..];
            let Some(open) = signature.find('(') else {
                break;
            };
            let mut depth = 0_i32;
            let mut close = signature.len();
            for (index, character) in signature[open..].char_indices() {
                match character {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = open + index;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let parameters = signature[open + 1..close].to_owned();
            let body = signature[close..]
                .find('{')
                .map_or("", |brace| &signature[close + brace + 1..]);
            let first = body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with("//"))
                .unwrap_or_default()
                .to_owned();
            found.push(Handler {
                id,
                file: file.to_owned(),
                parameters,
                first,
            });
            rest = signature;
        }
        found
    }

    /// Every handler authorizes before it does anything else: a handler that takes a
    /// `Principal` opens with `Principal::require` or one of its documented variants, so no read,
    /// parse or effect happens for a credential that may not ask; and a handler that takes none
    /// is one of the listed anonymous or session operations, taking the session (`SignedIn`) or
    /// declaring itself anonymous (`security(())`). The handlers are read from the source, and
    /// the scan must find exactly the document's operations, so a declaration it cannot read
    /// fails the test rather than escaping it.
    #[test]
    fn every_handler_authorizes_before_anything_else() {
        let document = serde_json::to_value(openapi()).unwrap();
        let mut security = BTreeMap::new();
        for item in document["paths"].as_object().unwrap().values() {
            for method in METHODS {
                if let Some(operation) = item.get(method) {
                    let id = operation["operationId"].as_str().unwrap_or("no id");
                    security.insert(id.to_owned(), operation["security"].clone());
                }
            }
        }
        let anonymous = |id: &str| {
            security
                .get(id)
                .is_some_and(|declared| *declared == json!([{}]) || *declared == json!([]))
        };
        let mut broken = Vec::new();
        let mut scanned = BTreeSet::new();
        for (file, text) in sources() {
            for handler in handlers(&file, &text) {
                let name = format!("`{}` ({})", handler.id, handler.file);
                if handler.parameters.contains("Principal") {
                    if !AUTHORIZE.iter().any(|call| handler.first.starts_with(call)) {
                        broken.push(format!(
                            "{name} does not authorize first: its body opens with `{}`",
                            handler.first
                        ));
                    }
                } else if !WITHOUT_PRINCIPAL.contains(&handler.id.as_str()) {
                    broken.push(format!(
                        "{name} takes no `Principal` and is not an anonymous or session operation"
                    ));
                } else if !handler.parameters.contains("SignedIn") && !anonymous(&handler.id) {
                    broken.push(format!(
                        "{name} takes neither a `Principal` nor the session (`SignedIn`), so it must declare `security(())`"
                    ));
                }
                scanned.insert(handler.id);
            }
        }
        for id in security.keys() {
            if !scanned.contains(id) {
                broken.push(format!("`{id}`: the scan found no handler for it"));
            }
        }
        for id in &scanned {
            if !security.contains_key(id) {
                broken.push(format!(
                    "`{id}`: a handler whose operation is not in the document"
                ));
            }
        }
        assert!(
            broken.is_empty(),
            "{} handlers break the authorization rule:\n{}",
            broken.len(),
            broken.join("\n")
        );
    }
}
