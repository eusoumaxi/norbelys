//! The MCP server: the public API as tools for AI clients, at `/mcp` (streamable HTTP, `rmcp`).
//!
//! # One implementation of every operation
//!
//! A tool is a public operation of the API ([`catalogue`]), and calling it **is** calling the
//! operation: the server builds the HTTP request the operation's contract describes (path and
//! query parameters, headers, the JSON body, an `Idempotency-Key`) and sends it through the
//! product router in process, with the token's principal already attached, so authorization,
//! validation, idempotency, problems and every effect are those of the API itself. The answer is
//! the operation's JSON body as structured content, or its problem document as an error result.
//!
//! # Who may call
//!
//! An `nbo_` access token issued by our authorization server for this deployment's MCP resource
//! proves a principal here ([`crate::identity::authority::Authority::verify_mcp`]); its scopes are
//! the grant's, narrowed to the person's current role. While the authorization server is off
//! (`OAUTH_SERVER_DISABLED`) no such token can be issued, so an API key is accepted instead, with
//! its own scopes. Without one the answer is `401` with `WWW-Authenticate: Bearer
//! resource_metadata="…"` pointing at the resource's metadata (RFC 9728 §5.1), which is how an MCP
//! client discovers where to authorize. `tools/list` shows only
//! the tools the principal holds a scope of (most have one; an export's open to the scope of any
//! resource it may hold, the API then checking the one the request names); calling another
//! answers `insufficient_scope`.
//!
//! # Bounds
//!
//! - At most 32 requests are in flight per process; beyond them the answer is `429 rate_limited`
//!   with `Retry-After`, before any work.
//! - An operation's answer above 2 MiB is not returned: the tool answers an error asking for a
//!   smaller page (`limit`).
//! - The server is stateless (no MCP sessions, nothing kept between requests), answering JSON
//!   rather than event streams, so any replica serves any request; `GET` and `DELETE` on `/mcp`,
//!   which only sessions use, answer `405`.
//! - The `Host` check the library applies against DNS rebinding of local servers is off: the
//!   server is public and every request needs a bearer token, which a rebinding page cannot
//!   attach.

pub mod catalogue;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use tower::ServiceExt as _;

use self::catalogue::{BODY, CATALOGUE, IDEMPOTENCY_KEY, Operation};
use crate::domain::oauth::Resources;
use crate::http::AppState;
use crate::http::ratelimit::ClientAddress;
use crate::identity::authority::Principal;
use crate::problem::{Code, Problem};

/// The most requests in flight per process.
const MAX_IN_FLIGHT: usize = 32;
/// The largest operation answer a tool returns.
pub(crate) const MAX_RESPONSE: usize = 2 << 20;

/// The `/mcp` route, executing tools through `api` (the product router without this route).
pub fn routes(state: &AppState, api: Router) -> Router<AppState> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .disable_allowed_hosts();
    let service = StreamableHttpService::new(
        move || Ok(Server { api: api.clone() }),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    // The layer added last runs first: the in-flight bound before any verification.
    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .route_layer(middleware::from_fn_with_state(
            Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            limit_in_flight,
        ))
}

/// Holds one of the process's in-flight permits for the request's duration, or answers
/// `429 rate_limited` with `Retry-After` at once when none is left (see the module).
async fn limit_in_flight(
    State(permits): State<Arc<Semaphore>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = permits.try_acquire_owned() else {
        return Problem {
            retry_after: Some(1),
            ..Problem::new(
                Code::RateLimited,
                "The MCP server is busy; retry in a second.",
            )
        }
        .into_response();
    };
    next.run(request).await
}

/// Verifies the bearer token as an MCP access token and leaves its principal in the request's
/// extensions for the tools (see the module).
async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let state = &state;
    let credential = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let Some(credential) = credential else {
        return challenge(state, Problem::unauthorized());
    };
    let client = ClientAddress::of(
        request.headers(),
        request.extensions(),
        state.limits.trust_forwarded_for(),
        state.limits.trusted_proxy_ips(),
    );
    let principal = match state.authority.verify_mcp(state, &credential, client).await {
        Ok(principal) => principal,
        Err(problem) => return challenge(state, problem),
    };
    request.extensions_mut().insert(principal);
    next.run(request).await
}

/// `problem`, and for a `401` the challenge that names the resource's metadata (RFC 9728 §5.1).
fn challenge(state: &AppState, problem: Problem) -> Response {
    let unauthorized = problem.code == Code::Unauthorized;
    let mut response = problem.into_response();
    if unauthorized {
        // With the authorization server off there is no metadata to point at: a client
        // authenticates with an API key as its bearer token.
        let value = if state.identity.oauth_server {
            let metadata = format!(
                "{}/.well-known/oauth-protected-resource/mcp",
                Resources::of(&state.settings.public_api_url).api
            );
            HeaderValue::from_str(&format!("Bearer resource_metadata=\"{metadata}\"")).ok()
        } else {
            Some(HeaderValue::from_static("Bearer"))
        };
        if let Some(value) = value {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
    }
    response
}

/// The MCP handler of one request: the product router it executes tools through.
#[derive(Clone)]
struct Server {
    api: Router,
}

/// The principal the authentication middleware left on the HTTP request.
fn principal(context: &RequestContext<RoleServer>) -> Result<Principal, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Principal>())
        .copied()
        .ok_or_else(|| ErrorData::internal_error("the request has no principal", None))
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("norbelys", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Norbelys's API as tools, acting in the workspace you authorized: people, \
                 campaigns, messages, connections, webhooks and analytics. Lists are cursor pages \
                 (`limit`, `cursor`); creates, actions and updates take an optional \
                 `idempotency_key`, deletes need none.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let principal = principal(&context)?;
        Ok(ListToolsResult::with_all_items(
            CATALOGUE
                .visible(principal.scopes)
                .map(|operation| operation.tool.clone())
                .collect(),
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        CATALOGUE.get(name).map(|operation| operation.tool.clone())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let principal = principal(&context)?;
        let operation = CATALOGUE.get(&request.name).ok_or_else(|| {
            ErrorData::invalid_params(format!("There is no tool `{}`.", request.name), None)
        })?;
        if !operation.opened_by(principal.scopes) {
            return Ok(problem_result(Problem::new(
                Code::InsufficientScope,
                format!(
                    "The authorization lacks the {} scope this tool needs; authorize again with it.",
                    catalogue::named(operation.scopes)
                ),
            ))
            .await
            .into());
        }
        let arguments = request.arguments.unwrap_or_default();
        let request = match http_request(operation, &arguments) {
            Ok(mut request) => {
                request.extensions_mut().insert(principal);
                request
            }
            Err(problem) => return Ok(problem_result(problem).await.into()),
        };
        Ok(execute(self.api.clone(), request).await.into())
    }
}

/// The HTTP request of `operation` with `arguments` (see the module).
fn http_request(operation: &Operation, arguments: &Map<String, Value>) -> Result<Request, Problem> {
    let mut path = operation.path.clone();
    for name in &operation.path_params {
        let value = arguments.get(name).map(scalar).ok_or_else(|| {
            Problem::invalid_field(
                &format!("/{name}"),
                "required",
                format!("`{name}` is required."),
            )
        })?;
        let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
        path = path.replace(&format!("{{{name}}}"), &encoded);
    }
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for name in &operation.query_params {
        match arguments.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                for item in items {
                    query.append_pair(name, &scalar(item));
                }
            }
            Some(value) => {
                query.append_pair(name, &scalar(value));
            }
        }
    }
    let query = query.finish();
    let uri = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    let mut builder = http::request::Builder::new()
        .method(operation.method.clone())
        .uri(uri);
    for (argument, name) in &operation.header_params {
        if let Some(value) = arguments.get(argument) {
            builder = builder.header(name.as_str(), scalar(value));
        }
    }
    let key = arguments.get(IDEMPOTENCY_KEY).map(scalar);
    if operation.method == Method::POST || key.is_some() {
        let key = key.unwrap_or_else(|| format!("mcp-{}", uuid::Uuid::now_v7()));
        builder = builder.header("idempotency-key", key);
    }
    let body = match arguments.get(BODY) {
        Some(body) if operation.body => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(body).map_err(|error| Problem::internal(&error))?)
        }
        _ => Body::empty(),
    };
    builder.body(body).map_err(|_| {
        Problem::invalid_field("", "invalid", "The arguments do not make a valid request.")
    })
}

/// A parameter's text: a string as it is, anything else as JSON.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Sends `request` through the product router and turns its answer into a tool result.
async fn execute(api: Router, request: Request) -> CallToolResult {
    let Ok(response) = api.oneshot(request).await;
    let status = response.status();
    let Ok(bytes) = axum::body::to_bytes(response.into_body(), MAX_RESPONSE).await else {
        return problem_result(Problem::new(
            Code::PayloadTooLarge,
            "The answer exceeds 2 MiB; ask for a smaller page with `limit`.",
        ))
        .await;
    };
    let body: Option<Value> = serde_json::from_slice(&bytes).ok();
    match (status.is_success(), body) {
        (true, Some(body @ Value::Object(_))) => CallToolResult::structured(body),
        (true, Some(body)) => CallToolResult::structured(json!({ "value": body })),
        (true, None) => CallToolResult::success(vec![ContentBlock::text(format!(
            "Done ({}).",
            status_text(status)
        ))]),
        (false, Some(problem)) => CallToolResult::structured_error(problem),
        (false, None) => CallToolResult::error(vec![ContentBlock::text(format!(
            "The operation failed ({}).",
            status_text(status)
        ))]),
    }
}

fn status_text(status: StatusCode) -> String {
    format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or_default()
    )
}

/// An error result carrying `problem`'s document, exactly as the API answers it.
async fn problem_result(problem: Problem) -> CallToolResult {
    let response = problem.into_response();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_RESPONSE)
        .await
        .unwrap_or_default();
    CallToolResult::structured_error(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware;
    use axum::routing::{get, post};
    use serde_json::{Value, json};
    use tokio::sync::Semaphore;
    use tower::ServiceExt as _;

    use super::{MAX_RESPONSE, execute, limit_in_flight};
    use crate::domain::oauth::Audience;
    use crate::domain::scope::{Scope, ScopeSet};
    use crate::identity::oauth::tests::{API, grant_token};
    use crate::testing::{Reply, TestApp, TestDb};

    /// Beyond the in-flight bound a request is refused at once with `429 rate_limited` and
    /// `Retry-After`, before any work, and admitted again as soon as a permit is free, so a burst
    /// of MCP requests cannot exhaust the process.
    #[tokio::test]
    async fn requests_beyond_the_bound_are_refused() {
        let permits = Arc::new(Semaphore::new(1));
        let app = axum::Router::new()
            .route("/", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&permits),
                limit_in_flight,
            ));
        let request = || Request::post("/").body(Body::empty()).unwrap();
        let held = Arc::clone(&permits).try_acquire_owned().unwrap();
        let refused = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.headers()["retry-after"], "1");
        drop(held);
        let admitted = app.oneshot(request()).await.unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
        assert_eq!(permits.available_permits(), 1, "the permit is returned");
    }

    /// An operation's answer above 2 MiB is not returned: the tool answers an error asking for a
    /// smaller page, so one call can never flood a client's context.
    #[tokio::test]
    async fn answers_above_two_mebibytes_are_refused() {
        let api = axum::Router::new()
            .route(
                "/small",
                get(|| async { axum::Json(json!({ "ok": true })) }),
            )
            .route("/big", get(|| async { "x".repeat(MAX_RESPONSE + 1) }));
        let request = |path: &str| Request::get(path).body(Body::empty()).unwrap();
        let small = execute(api.clone(), request("/small")).await;
        assert_eq!(small.is_error, Some(false));
        assert_eq!(small.structured_content, Some(json!({ "ok": true })));
        let big = execute(api, request("/big")).await;
        assert_eq!(big.is_error, Some(true));
        let problem = big.structured_content.unwrap();
        assert_eq!(problem["code"], "payload_too_large");
        assert!(problem["detail"].as_str().unwrap().contains("smaller page"));
    }

    /// A JSON-RPC request to `/mcp`, as an MCP client sends it.
    async fn rpc(app: &TestApp, token: Option<&str>, method: &str, params: Value) -> Reply {
        let mut call = app
            .post("/mcp")
            .header("host", "127.0.0.1:3001")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
        if let Some(token) = token {
            call = call.bearer(token);
        }
        call.send().await
    }

    fn tool_names(reply: &Reply) -> Vec<String> {
        reply.json["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// With the authorization server on, only an MCP access token reaches the server: without
    /// one, or with an API key or a CLI token, the answer is `401` with the challenge that names
    /// the resource's metadata, which is how an MCP client learns where to authorize.
    #[tokio::test]
    async fn only_mcp_tokens_reach_the_server() {
        let test = TestDb::new().await;
        test.signing_key().await;
        let workspace = test.workspace("acme").await;
        let (_, cli) = grant_token(&test, &workspace, Audience::Cli, ScopeSet::all()).await;
        let app = test.app();
        for token in [None, Some(workspace.key.as_str()), Some(cli.as_str())] {
            let reply = rpc(&app, token, "tools/list", json!({})).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{token:?}");
            assert_eq!(
                reply.header("www-authenticate"),
                Some(
                    format!(
                        "Bearer resource_metadata=\"{API}/.well-known/oauth-protected-resource/mcp\""
                    )
                    .as_str()
                )
            );
        }
    }

    /// While the authorization server is off (`OAUTH_SERVER_DISABLED`) no MCP token can be issued,
    /// so an API key reaches the server with its own scopes, and a refusal's challenge names no
    /// metadata to authorize at; a CLI token is still refused. This is the deferred state a
    /// deployment runs in until its authorization server passes its gate.
    #[tokio::test]
    async fn an_api_key_reaches_the_server_while_the_authorization_server_is_off() {
        let test = TestDb::new().await;
        test.signing_key().await;
        let workspace = test.workspace("acme").await;
        let (_, cli) = grant_token(&test, &workspace, Audience::Cli, ScopeSet::all()).await;
        let app = test.app_with_identity(crate::identity::Identity {
            oauth_server: false,
            ..crate::identity::Identity::for_tests()
        });
        let listed = rpc(&app, Some(workspace.key.as_str()), "tools/list", json!({})).await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
        assert!(!tool_names(&listed).is_empty());
        let refused = rpc(&app, Some(cli.as_str()), "tools/list", json!({})).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused.header("www-authenticate"), Some("Bearer"));
    }

    /// The tools follow the grant: `tools/list` shows only what its scopes open, a call executes
    /// the operation itself (a page of people, a person created once for one idempotency key), a
    /// tool beyond the grant answers `insufficient_scope`, and an API problem comes back as the
    /// tool's error.
    #[tokio::test]
    async fn tools_follow_the_grant_and_call_the_api() {
        let test = TestDb::new().await;
        test.signing_key().await;
        let workspace = test.workspace("acme").await;
        let readers: ScopeSet = [Scope::PeopleRead].into_iter().collect();
        let (_, reader) = grant_token(&test, &workspace, Audience::Mcp, readers).await;
        let (_, writer) = grant_token(&test, &workspace, Audience::Mcp, ScopeSet::all()).await;
        let app = test.app();

        let init = rpc(
            &app,
            Some(&reader),
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }),
        )
        .await;
        assert_eq!(init.status, StatusCode::OK, "{}", init.json);
        assert_eq!(init.json["result"]["serverInfo"]["name"], "norbelys");

        let listed = tool_names(&rpc(&app, Some(&reader), "tools/list", json!({})).await);
        assert!(listed.contains(&"people.list".to_owned()), "{listed:?}");
        assert!(!listed.contains(&"people.create".to_owned()));
        let all = tool_names(&rpc(&app, Some(&writer), "tools/list", json!({})).await);
        assert!(all.contains(&"people.create".to_owned()) && all.len() > listed.len());

        let refused = rpc(
            &app,
            Some(&reader),
            "tools/call",
            json!({ "name": "people.create", "arguments": { "body": { "email": "ada@example.com" } } }),
        )
        .await;
        assert_eq!(refused.json["result"]["isError"], true, "{}", refused.json);
        assert_eq!(
            refused.json["result"]["structuredContent"]["code"],
            "insufficient_scope"
        );

        let create = json!({
            "name": "people.create",
            "arguments": { "body": { "email": "ada@example.com" }, "idempotency_key": "mcp-test-1" },
        });
        let created = rpc(&app, Some(&writer), "tools/call", create.clone()).await;
        let person = &created.json["result"]["structuredContent"];
        assert!(
            person["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("per_")),
            "{}",
            created.json
        );
        let again = rpc(&app, Some(&writer), "tools/call", create).await;
        assert_eq!(
            again.json["result"]["structuredContent"]["id"],
            person["id"]
        );

        let page = rpc(
            &app,
            Some(&reader),
            "tools/call",
            json!({ "name": "people.list", "arguments": { "limit": 10 } }),
        )
        .await;
        let data = &page.json["result"]["structuredContent"]["data"];
        assert_eq!(data.as_array().map(Vec::len), Some(1), "{}", page.json);

        let invalid = rpc(
            &app,
            Some(&writer),
            "tools/call",
            json!({ "name": "people.create", "arguments": { "body": { "email": "not an address" } } }),
        )
        .await;
        assert_eq!(invalid.json["result"]["isError"], true);
        assert_eq!(
            invalid.json["result"]["structuredContent"]["code"],
            "validation_failed"
        );
        let unknown = rpc(
            &app,
            Some(&writer),
            "tools/call",
            json!({ "name": "people.teleport", "arguments": {} }),
        )
        .await;
        assert!(unknown.json["error"].is_object(), "{}", unknown.json);
    }
}
