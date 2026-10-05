//! The HTTP side of tracking: the tracking role's own router (opens and clicks), the api's routes
//! for recipients outside `/v1` (the unsubscribe page and its one-click request, public images),
//! and the `images` resource under `/v1`.
//!
//! # The tracking role
//!
//! `GET /t/o/{token}` and `GET /t/c/{token}` read no database: the token is verified with the
//! deployment's keys, the event classified (`domain::tracking`), appended to the spool, and the
//! request answered: the open with a 1×1 GIF that no cache keeps, the click with a `302` to the
//! destination its token signs (never anywhere else) and no `Referer` passed on. What the routes
//! answer does not depend on whether the event could be recorded: a full spool loses the event
//! (counted in `norbelys_tracking_events_lost_total`), never the recipient's image or link.
//! `GET /brand/v1/email-mark.png` serves the mark of the platform's own mail from the binary
//! (`images::email_mark`), the one image the role serves itself: it needs no object store.
//! `/health/live` answers while the process runs and
//! `/health/ready` while the spool takes events; the database is not part of either, since the
//! role answers without it. Every request is counted in `norbelys_http_requests_total` by its
//! route template, method and status class; no request is logged one by one (the drain's batches
//! are).
//!
//! # Unsubscribe
//!
//! `GET /u/{token}` shows a small page naming the address, with one button and no script;
//! `POST /u/{token}` (the button, or a mail provider's one-click request with the body
//! `List-Unsubscribe=One-Click`) unsubscribes at once and answers `200` with a page, never a
//! redirect, which RFC 8058 forbids for the one-click request
//! (<https://www.rfc-editor.org/rfc/rfc8058#section-3.1>). The `GET` changes nothing, because
//! scanners fetch every link they find. A token that does not verify answers `404`; a database
//! that cannot take the unsubscribe answers `503` with `Retry-After`. Where the write goes and
//! why is `tracking::unsubscribe`.
//!
//! # Images
//!
//! `POST /v1/images` takes the file itself as the body, with its own `Content-Type` (`image/png`,
//! `image/jpeg`, `image/gif` or `image/webp`), at most 16 MiB, and refuses a file whose first
//! bytes are not that format (`domain::images`); it answers `201` with the image and its public
//! URL. `DELETE /v1/images/{id}` answers `204`. Both need `campaigns:write`. `GET
//! /images/{workspace}/{file}` serves the file to anyone, streamed from the object store, with
//! `Cache-Control: public, max-age=31536000, immutable` (a URL's content never changes) and
//! `nosniff`; anything else answers `404`.

use std::sync::LazyLock;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::images::{self, ImageObject};
use super::spool::{Event, Spool, SpoolError};
use super::token::Token;
use super::unsubscribe;
use crate::crypto::{self, Keys};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Image, Message, WorkspaceId};
use crate::domain::images::{Format, SIZE_MAX};
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::domain::tracking::{self as decide, Age, Agent, Answer, Check, EventKind, Fetch};
use crate::http::AppState;
use crate::http::extract::{Json, Path};
use crate::http::ratelimit::ClientAddress;
use crate::identity::authority::Principal;
use crate::problem::{ApiResult, Code, Problem};
use crate::rendering::footer::escape;
use crate::storage::StorageError;

/// A transparent 1×1 GIF: the smallest image every mail client shows (43 bytes).
const PIXEL: [u8; 43] = [
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];
/// The longest `User-Agent` kept with an event, in characters.
const USER_AGENT_MAX: usize = 512;
/// A response no cache may keep: every open and click must reach us.
const NO_STORE: &str = "no-store, no-cache, must-revalidate, max-age=0, private";
/// What the unsubscribe pages may load: their own inline style, nothing else; their form posts
/// only to this host.
const PAGE_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

static REQUESTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_http_requests_total")
        .with_description("HTTP requests answered, by route template, method and status class.")
        .build()
});

static EVENTS_LOST: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_tracking_events_lost_total")
        .with_description(
            "Opens and clicks answered but not recorded: the spool was full, or refused the write.",
        )
        .build()
});

// ───────────────────────────── the tracking role ─────────────────────────────

/// What the tracking role's routes need.
#[derive(Clone, Debug)]
pub struct Tracker {
    /// The deployment's keys: they verify tokens and hash client addresses.
    pub keys: Keys,
    /// Where events wait for the drain.
    pub spool: Spool,
    /// Take the client's address from the leftmost `X-Forwarded-For`, which the proxy in front of
    /// the role sets.
    pub trust_forwarded_for: bool,
    /// TCP peers authorized to forward client addresses.
    pub trusted_proxy_ips: Vec<std::net::IpAddr>,
}

/// The tracking role's router (see the module).
pub fn tracker(state: Tracker) -> Router {
    Router::new()
        .route("/t/o/{token}", get(open))
        .route("/t/c/{token}", get(click))
        .route(images::EMAIL_MARK_PATH, get(images::email_mark))
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .fallback(nothing)
        .layer(middleware::from_fn(count))
        .with_state(state)
}

/// `GET /t/o/{token}`: records the open its token names and answers the pixel, whatever the token.
async fn open(State(tracker): State<Tracker>, Path(token): Path<String>, parts: Parts) -> Response {
    let (check, _) = observe(&tracker, EventKind::Open, &token, &parts).await;
    respond(decide::answer(EventKind::Open, check), None)
}

/// `GET /t/c/{token}`: records the click its token names and redirects to the destination the
/// token signs; a token that does not verify answers `404`.
async fn click(
    State(tracker): State<Tracker>,
    Path(token): Path<String>,
    parts: Parts,
) -> Response {
    let (check, destination) = observe(&tracker, EventKind::Click, &token, &parts).await;
    respond(decide::answer(EventKind::Click, check), destination)
}

/// Verifies a tracking request's token for the route of `kind`, records the event when the
/// decisions say so, and returns the check with a click's destination.
async fn observe(
    tracker: &Tracker,
    kind: EventKind,
    token: &str,
    parts: &Parts,
) -> (Check, Option<String>) {
    let now = crate::process::now();
    let named = match (kind, Token::decode(&tracker.keys, token)) {
        (EventKind::Open, Ok(Token::Open { workspace, message })) => {
            Some((workspace, message, None))
        }
        (
            EventKind::Click,
            Ok(Token::Click {
                workspace,
                message,
                link,
                url,
            }),
        ) if decide::redirectable(&url) => Some((workspace, message, Some((link, url)))),
        _ => None,
    };
    let Some((workspace, message, click)) = named else {
        return (Check::Refused, None);
    };
    let age = Age::of(decide::created_at(message.uuid()), now.0);
    if decide::recorded(Check::Valid, age) {
        let event = event(
            tracker,
            kind,
            (workspace, message),
            click.as_ref(),
            parts,
            age,
            now,
        );
        match tracker.spool.append(&event).await {
            Ok(()) => {}
            // A full spool shows in the readiness probe, in `norbelys_tracking_spool_bytes` and in
            // the count of lost events; a line per lost event would only flood the logs at the
            // pixel's rate.
            Err(SpoolError::Full(bytes)) => {
                EVENTS_LOST.add(1, &[KeyValue::new("reason", "full")]);
                tracing::debug!(
                    bytes,
                    "the tracking spool is full: an event was not recorded"
                );
            }
            Err(error) => {
                EVENTS_LOST.add(1, &[KeyValue::new("reason", "error")]);
                tracing::error!(error = %error, "a tracking event was not recorded");
            }
        }
    }
    (Check::Valid, click.map(|(_, url)| url))
}

/// The event a verified request records.
fn event(
    tracker: &Tracker,
    kind: EventKind,
    (workspace, message): (WorkspaceId, Id<Message>),
    click: Option<&(u16, String)>,
    parts: &Parts,
    age: Age,
    now: Timestamp,
) -> Event {
    let user_agent = parts
        .headers
        .get(header::USER_AGENT)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    let fetch = if parts.method == Method::HEAD {
        Fetch::Head
    } else {
        Fetch::Get
    };
    let address = ClientAddress::of(
        &parts.headers,
        &parts.extensions,
        tracker.trust_forwarded_for,
        &tracker.trusted_proxy_ips,
    );
    Event {
        id: Uuid::now_v7(),
        workspace,
        message,
        kind,
        link: click.and_then(|(link, _)| i16::try_from(*link).ok()),
        url_hash: click.map(|(_, url)| crypto::sha256(url.as_bytes())),
        actor: decide::classify(kind, fetch, Agent::of(user_agent.as_deref()), age),
        ip_hash: address
            .0
            .map(|ip| tracker.keys.hash_address(&ip.to_string())),
        user_agent: user_agent.map(|text| text.chars().take(USER_AGENT_MAX).collect()),
        occurred_at: now,
    }
}

/// The response of a decided answer.
fn respond(answer: Answer, destination: Option<String>) -> Response {
    match answer {
        Answer::Pixel => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("image/gif")),
                (header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE)),
                (header::PRAGMA, HeaderValue::from_static("no-cache")),
                (header::EXPIRES, HeaderValue::from_static("0")),
            ],
            PIXEL.as_slice(),
        )
            .into_response(),
        Answer::Redirect => match destination.and_then(|url| HeaderValue::from_str(&url).ok()) {
            Some(location) => (
                StatusCode::FOUND,
                [
                    (header::LOCATION, location),
                    (header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE)),
                    (
                        header::REFERRER_POLICY,
                        HeaderValue::from_static("no-referrer"),
                    ),
                ],
            )
                .into_response(),
            None => missing_link(),
        },
        Answer::NotFound => missing_link(),
    }
}

/// `404` for a link that leads nowhere, in words a recipient can read.
fn missing_link() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE)),
        ],
        "This link is not valid.\n",
    )
        .into_response()
}

/// Any other path of the tracking role.
async fn nothing() -> Response {
    missing_link()
}

/// `/health/live`: the process runs.
async fn live() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `/health/ready`: the spool takes events.
async fn ready(State(tracker): State<Tracker>) -> StatusCode {
    if tracker.spool.ready() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Counts every request by its route template, its method and its status class.
async fn count(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());
    let method = match *request.method() {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        _ => "other",
    };
    let response = next.run(request).await;
    let class = match response.status().as_u16() / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        _ => "5xx",
    };
    REQUESTS.add(
        1,
        &[
            KeyValue::new("route", route),
            KeyValue::new("method", method),
            KeyValue::new("status_class", class),
        ],
    );
    response
}

// ───────────────────────────── the api's routes for recipients ─────────────────────────────

/// The api's routes for recipients outside `/v1`: unsubscribes and public images. The product
/// router serves them where one process serves every surface, the ingress router on the public
/// host.
pub fn public() -> Router<AppState> {
    Router::new()
        .route("/u/{token}", get(unsubscribe_page).post(unsubscribe))
        .route("/images/{workspace}/{file}", get(image))
}

/// What a verified unsubscribe token names: the workspace, the message and the address.
fn unsubscribe_token(keys: &Keys, token: &str) -> Option<(WorkspaceId, Id<Message>, EmailAddress)> {
    match Token::decode(keys, token) {
        Ok(Token::Unsubscribe {
            workspace,
            message,
            email,
        }) => Some((workspace, message, EmailAddress::parse(&email).ok()?)),
        _ => None,
    }
}

/// `GET /u/{token}`: the page that asks before unsubscribing; it changes nothing.
async fn unsubscribe_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some((_, _, email)) = unsubscribe_token(&state.keys, &token) else {
        return invalid_unsubscribe();
    };
    page(
        StatusCode::OK,
        "Unsubscribe",
        &format!(
            "<h1>Unsubscribe</h1>\
             <p>Stop all mail from this sender to <strong>{}</strong>?</p>\
             <form method=\"post\"><input type=\"hidden\" name=\"List-Unsubscribe\" value=\"One-Click\">\
             <button type=\"submit\">Unsubscribe</button></form>",
            escape(email.as_str())
        ),
    )
}

/// `POST /u/{token}`: unsubscribes the address the token names, at once (RFC 8058's one-click).
async fn unsubscribe(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some((workspace, message, email)) = unsubscribe_token(&state.keys, &token) else {
        return invalid_unsubscribe();
    };
    match unsubscribe::record(&state.db, workspace, message, &email).await {
        Ok(_) => page(
            StatusCode::OK,
            "Unsubscribed",
            &format!(
                "<h1>You are unsubscribed</h1><p>This sender will not mail <strong>{}</strong> again.</p>",
                escape(email.as_str())
            ),
        ),
        Err(error) => {
            tracing::error!(error = %error, "an unsubscribe could not be recorded");
            let mut response = page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again",
                "<h1>Please try again</h1><p>Your request could not be recorded just now. Please use the link again in a minute.</p>",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            response
        }
    }
}

/// `404` for an unsubscribe link this deployment did not make.
fn invalid_unsubscribe() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "Link not valid",
        "<h1>This link is not valid</h1><p>Use the unsubscribe link of the message you received.</p>",
    )
}

/// A small HTML page with `content` (already escaped), that no cache keeps and that loads nothing.
fn page(status: StatusCode, title: &str, content: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{title}</title>\
         <style>body{{font-family:system-ui,-apple-system,\"Segoe UI\",sans-serif;max-width:32rem;\
         margin:4rem auto;padding:0 1rem;line-height:1.5;color:#1a1a1a}}\
         button{{font:inherit;padding:.6rem 1.2rem;border:0;border-radius:.4rem;background:#1a1a1a;\
         color:#fff;cursor:pointer}}</style></head><body><main>{content}</main></body></html>"
    );
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(PAGE_POLICY),
            ),
            (
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        html,
    )
        .into_response()
}

/// `GET /images/{workspace}/{file}`: the image, streamed from the object store; `404` for anything
/// that names no image.
async fn image(
    State(state): State<AppState>,
    Path((workspace, file)): Path<(String, String)>,
) -> Response {
    let Some((workspace, image, format)) = images::locate(&workspace, &file) else {
        return Problem::not_found("image").into_response();
    };
    match state
        .storage
        .body(&images::key(workspace.uuid(), image, format))
        .await
    {
        Ok((size, body)) => (
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(format.media_type()),
                ),
                (header::CONTENT_LENGTH, HeaderValue::from(size)),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(images::IMMUTABLE),
                ),
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
            ],
            body,
        )
            .into_response(),
        Err(StorageError::NotFound(_)) => Problem::not_found("image").into_response(),
        Err(error) => Problem::from(error).into_response(),
    }
}

// ───────────────────────────── /v1/images ─────────────────────────────

/// The `images` resource under `/v1`. An upload's file is the body, bounded at 16 MiB rather than
/// the default 1 MiB.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(
            OpenApiRouter::new()
                .routes(routes!(create_image))
                .layer(DefaultBodyLimit::max(SIZE_MAX)),
        )
        .routes(routes!(delete_image))
}

impl From<images::Error> for Problem {
    fn from(error: images::Error) -> Self {
        match error {
            images::Error::NotFound => Problem::not_found("image"),
            images::Error::Db(error) => Problem::from(error),
            images::Error::Storage(error) => Problem::from(error),
        }
    }
}

/// Upload an image to show in mail.
///
/// The file is the body, sent with its own `Content-Type` (`image/png`, `image/jpeg`, `image/gif`
/// or `image/webp`), at most 16 MiB; its first bytes must be that format. The answer carries the
/// image's public URL, to use in a message's or a variant's HTML.
#[utoipa::path(
    post,
    path = "/images",
    tag = "Content",
    operation_id = "images.create",
    request_body(content(
        (String = "image/png", example = json!("(the PNG file's bytes)")),
        (String = "image/jpeg", example = json!("(the JPEG file's bytes)")),
        (String = "image/gif", example = json!("(the GIF file's bytes)")),
        (String = "image/webp", example = json!("(the WebP file's bytes)"))
    )),
    responses(
        (status = 201, description = "The image, with its public URL.", body = ImageObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 413, description = "The file is larger than 16 MiB."),
        (status = 415, description = "The body is not sent as a PNG, JPEG, GIF or WebP image."),
        (status = 422, description = "The file is empty, or its content is not the format its `Content-Type` names."),
        (status = 503, description = "Object storage is unavailable; retry with the same key."),
    ),
    security(("bearer" = []))
)]
async fn create_image(
    principal: Principal,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<ImageObject>)> {
    principal.require(Scope::CampaignsWrite)?;
    let format = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(Format::from_media_type)
        .ok_or_else(|| {
            Problem::new(
                Code::UnsupportedMediaType,
                "Send the image with its own `Content-Type`: `image/png`, `image/jpeg`, `image/gif` or `image/webp`.",
            )
        })?;
    if body.is_empty() {
        return Err(Problem::invalid_field(
            "",
            "required",
            "the image file is empty",
        ));
    }
    if !format.matches(&body) {
        return Err(Problem::invalid_field(
            "",
            "format",
            format!("the file's content is not a {} image", format.media_type()),
        ));
    }
    let origin = state
        .settings
        .public_tracking_url
        .origin()
        .ascii_serialization();
    let image = images::create(
        &state.db,
        &state.storage,
        &origin,
        principal.workspace,
        format,
        body,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(image)))
}

/// Delete an image.
///
/// Its public URL stops answering; mail already sent shows it no more, except where a cache kept a
/// copy.
#[utoipa::path(
    delete,
    path = "/images/{id}",
    tag = "Content",
    operation_id = "images.delete",
    params(("id" = Id<Image>, Path, description = "The image id (`img_…`).")),
    responses(
        (status = 204, description = "The image is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `campaigns:write`."),
        (status = 404, description = "No such image in this workspace."),
        (status = 503, description = "Object storage is unavailable; retry."),
    ),
    security(("bearer" = []))
)]
async fn delete_image(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Image>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::CampaignsWrite)?;
    images::delete(&state.db, &state.storage, principal.workspace, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
