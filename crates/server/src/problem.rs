//! Problems: how every error reaches a client.
//!
//! An error response is an RFC 9457 problem document
//! (<https://www.rfc-editor.org/rfc/rfc9457>) served as `application/problem+json`:
//!
//! - `type` is a stable URL per code, `title` is fixed per code and `status` equals the HTTP
//!   status, so clients can branch on either;
//! - `code` comes from a closed registry ([`Code`]): programs read the code, never the text;
//! - `detail` is written for a person and is always safe to show: it never carries an
//!   internal cause, SQL or a provider's response body;
//! - `instance` is the request path and `request_id` the request's id (also in
//!   `X-Request-Id`), so support can find the log lines of a failed request;
//! - `errors[]` appears only with `validation_failed`, one entry per invalid field with an
//!   RFC 6901 JSON pointer (<https://www.rfc-editor.org/rfc/rfc6901>), all of them in one
//!   response;
//! - `retry_after` mirrors the `Retry-After` header on `429` and `503`.
//!
//! This module is the only place an error is given an HTTP status: handlers return a
//! [`Problem`], or a module error that converts into one, and database errors are mapped
//! here once (a unique violation is a conflict, a check violation or a restricted delete an
//! invalid state, a missing reference a not-found). Because no operation can answer an error
//! any other way, the OpenAPI document gives every `4xx` and `5xx` response of every operation
//! the schema of the wire document ([`Body`], named `Problem`) in one place, the router's
//! [`openapi`](crate::http::router::openapi), rather than in each handler.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use crate::db::Tx;
use crate::http::context;

/// The closed registry of problem codes: programs branch on a problem's `code`, never on its
/// text. A code may be added, never repurposed.
// The OpenAPI document publishes it as `ProblemCode`, the type of every problem's `code`, so a
// generated client can branch on each code by name; like every enum a response carries it is
// open there (`x-open-enum`), since a code may be added.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = ProblemCode, rename_all = "snake_case")]
pub enum Code {
    InvalidRequest,
    Unauthorized,
    Forbidden,
    SessionRequired,
    NotFound,
    Archived,
    MethodNotAllowed,
    Conflict,
    InvalidState,
    IdempotencyInProgress,
    PreconditionFailed,
    PayloadTooLarge,
    UnsupportedMediaType,
    ValidationFailed,
    IdempotencyMismatch,
    Suppressed,
    CaptchaFailed,
    InsufficientScope,
    RateLimited,
    QuotaExceeded,
    InternalError,
    ServiceUnavailable,
    Timeout,
}

impl Code {
    /// The code's HTTP status.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::SessionRequired | Self::InsufficientScope => {
                StatusCode::FORBIDDEN
            }
            Self::NotFound | Self::Archived => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Conflict | Self::InvalidState | Self::IdempotencyInProgress => {
                StatusCode::CONFLICT
            }
            Self::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::ValidationFailed
            | Self::IdempotencyMismatch
            | Self::Suppressed
            | Self::CaptchaFailed => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited | Self::QuotaExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// The code's fixed title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid request",
            Self::Unauthorized => "Unauthorized",
            Self::Forbidden => "Forbidden",
            Self::SessionRequired => "Session required",
            Self::NotFound => "Not found",
            Self::Archived => "Archived",
            Self::MethodNotAllowed => "Method not allowed",
            Self::Conflict => "Conflict",
            Self::InvalidState => "Invalid state",
            Self::IdempotencyInProgress => "Idempotent request in progress",
            Self::PreconditionFailed => "Precondition failed",
            Self::PayloadTooLarge => "Payload too large",
            Self::UnsupportedMediaType => "Unsupported media type",
            Self::ValidationFailed => "Validation failed",
            Self::IdempotencyMismatch => "Idempotency key reused with another request",
            Self::Suppressed => "Recipient suppressed",
            Self::CaptchaFailed => "Captcha failed",
            Self::InsufficientScope => "Insufficient scope",
            Self::RateLimited => "Rate limited",
            Self::QuotaExceeded => "Quota exceeded",
            Self::InternalError => "Internal error",
            Self::ServiceUnavailable => "Service unavailable",
            Self::Timeout => "Timeout",
        }
    }

    /// The code as written on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// One invalid field of a `validation_failed` problem.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FieldError {
    /// RFC 6901 pointer into the body, or `?name` for a query parameter.
    pub pointer: String,
    /// What is wrong (`required`, `range`, `length`, `format`, `invalid`, …).
    pub code: String,
    /// A safe, human-readable explanation.
    pub detail: String,
}

/// An error answered to a client.
#[derive(Debug, Clone)]
pub struct Problem {
    /// The registry code.
    pub code: Code,
    /// Safe for the client to read; never an internal cause.
    pub detail: String,
    /// The invalid fields of a `validation_failed` problem.
    pub errors: Vec<FieldError>,
    /// Seconds to wait, for `429` and `503`.
    pub retry_after: Option<u64>,
}

impl Problem {
    /// A problem with a code and a detail.
    #[must_use]
    pub fn new(code: Code, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            errors: Vec::new(),
            retry_after: None,
        }
    }

    /// `404 not_found`: foreign and absent ids are indistinguishable.
    #[must_use]
    pub fn not_found(what: &str) -> Self {
        Self::new(Code::NotFound, format!("No such {what}."))
    }

    /// `404 archived`: the row's period left the online database, and an export is the only way
    /// to read it.
    #[must_use]
    pub fn archived(what: &str) -> Self {
        Self::new(
            Code::Archived,
            format!(
                "This {what} belongs to a period that was archived; `POST /v1/exports` reads archived periods."
            ),
        )
    }

    /// `422 validation_failed` with its field errors.
    #[must_use]
    pub fn validation(errors: Vec<FieldError>) -> Self {
        Self {
            code: Code::ValidationFailed,
            detail: "The request has invalid fields; see `errors`.".to_owned(),
            errors,
            retry_after: None,
        }
    }

    /// `422 validation_failed` for one field.
    #[must_use]
    pub fn invalid_field(pointer: &str, code: &str, detail: impl Into<String>) -> Self {
        Self::validation(vec![FieldError {
            pointer: pointer.to_owned(),
            code: code.to_owned(),
            detail: detail.into(),
        }])
    }

    /// `400 invalid_request`.
    #[must_use]
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(Code::InvalidRequest, detail)
    }

    /// `409 invalid_state`.
    #[must_use]
    pub fn invalid_state(detail: impl Into<String>) -> Self {
        Self::new(Code::InvalidState, detail)
    }

    /// `409 conflict`.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(Code::Conflict, detail)
    }

    /// `401 unauthorized`.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self::new(Code::Unauthorized, "A valid credential is required.")
    }

    /// `403 forbidden`.
    #[must_use]
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(Code::Forbidden, detail)
    }

    /// `500 internal_error`; the cause is logged with the request id, never returned.
    #[must_use]
    pub fn internal(cause: &dyn std::fmt::Display) -> Self {
        tracing::error!(request_id = %context::current().map(|context| context.request_id).unwrap_or_default(), error = %cause, "internal error");
        Self::new(
            Code::InternalError,
            "Something went wrong on our side; the request id identifies it.",
        )
    }

    /// `503 service_unavailable` with a retry hint.
    #[must_use]
    pub fn unavailable(retry_after: u64) -> Self {
        Self {
            retry_after: Some(retry_after),
            ..Self::new(
                Code::ServiceUnavailable,
                "A dependency is unavailable; retry later.",
            )
        }
    }
}

/// A problem document as it is written on the wire, and as the OpenAPI document describes every
/// error answer of every operation (the schema `Problem`, served as `application/problem+json`).
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = Problem)]
pub(crate) struct Body<'a> {
    /// `https://docs.norbelys.com/errors/<code>`, a page that explains the code.
    #[serde(rename = "type")]
    kind: String,
    /// Fixed per code.
    title: &'static str,
    /// The HTTP status.
    status: u16,
    /// The code from the closed registry; programs branch on it, never on the text. New codes
    /// may be added.
    #[schema(value_type = Code)]
    code: &'static str,
    /// What went wrong, for a person; never an internal cause.
    detail: &'a str,
    /// The request's path.
    instance: String,
    /// The request's id, also in `X-Request-Id`: what support needs to find the request.
    request_id: String,
    /// With `validation_failed` only: every invalid field.
    #[serde(skip_serializing_if = "<[FieldError]>::is_empty")]
    #[schema(value_type = Vec<FieldError>)]
    errors: &'a [FieldError],
    /// With `429` and `503`: the seconds to wait, as in `Retry-After`.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.code.status();
        let request = context::current();
        let body = Body {
            kind: format!("https://docs.norbelys.com/errors/{}", self.code.as_str()),
            title: self.code.title(),
            status: status.as_u16(),
            code: self.code.as_str(),
            detail: &self.detail,
            instance: request.as_ref().map(|r| r.path.clone()).unwrap_or_default(),
            request_id: request.map(|r| r.request_id).unwrap_or_default(),
            errors: &self.errors,
            retry_after: self.retry_after,
        };
        let json = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
        let mut response = (status, json).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(seconds) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if self.code == Code::Unauthorized {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

impl From<sqlx::Error> for Problem {
    /// Maps database errors: a constraint names the client's mistake; anything else is ours.
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::RowNotFound => Self::not_found("resource"),
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => {
                tracing::error!(error = %error, "database unavailable");
                Self::unavailable(2)
            }
            sqlx::Error::Database(database) => match database.code().as_deref() {
                // unique_violation
                Some("23505") => Self::conflict(match database.constraint() {
                    Some(constraint) => format!(
                        "A resource with the same unique value already exists ({constraint})."
                    ),
                    None => "A resource with the same unique value already exists.".to_owned(),
                }),
                // foreign_key_violation: a referenced row is absent in this workspace
                Some("23503") => Self::new(Code::NotFound, "A referenced resource does not exist."),
                // restrict_violation: an `ON DELETE RESTRICT` reference keeps the row, because
                // other rows (history) still point at it
                Some("23001") => Self::invalid_state(
                    "Other resources still refer to this one, so it cannot be deleted.",
                ),
                // check_violation, not_null_violation: an invariant the request broke
                Some("23514" | "23502") => Self::invalid_state(match database.constraint() {
                    Some(constraint) => format!("The change breaks the rule `{constraint}`."),
                    None => "The change breaks a rule of the resource.".to_owned(),
                }),
                // lock_not_available, serialization_failure, deadlock_detected: retryable
                Some("55P03" | "40001" | "40P01") => Self::unavailable(1),
                // query_canceled (statement or transaction timeout)
                Some("57014" | "25P04") => Self::new(Code::Timeout, "The request took too long."),
                _ => Self::internal(&error),
            },
            _ => Self::internal(&error),
        }
    }
}

impl From<crate::crypto::CryptoError> for Problem {
    fn from(error: crate::crypto::CryptoError) -> Self {
        Self::internal(&error)
    }
}

/// A handler result.
pub type ApiResult<T> = Result<T, Problem>;

/// The answer to a read of `id` in the history table `table` that found no row: `404 archived`
/// when the id's period left the online database, else `404 not_found` naming `what`.
///
/// History tables (messages and their attempts, delivery events) are partitioned by the period
/// of their UUIDv7 ids, and a period older than the table's online window is exported to the
/// archive and dropped. A period is archived when the id's instant is older than that window
/// (`partition_policies.retention`), or when the leaf that covered it was dropped
/// (`partition_leaves.dropped_at`, which also covers a window lengthened after a drop). The
/// decision reads only the id's instant and the archive's record of periods, never a row of any
/// workspace, so a foreign id and an absent one still answer alike; a table without a partition
/// policy is never archived. The database function `period_archived` makes the decision with
/// its owner's rights, because the api's login may not read the record of periods itself.
pub async fn missing(tx: &mut Tx, table: &str, id: Uuid, what: &str) -> Problem {
    let archived = sqlx::query_scalar::<_, Option<bool>>("SELECT period_archived($1, $2)")
        .bind(table)
        .bind(id)
        .fetch_one(&mut **tx)
        .await;
    match archived {
        Ok(Some(true)) => Problem::archived(what),
        Ok(Some(false) | None) => Problem::not_found(what),
        Err(error) => Problem::from(error),
    }
}
