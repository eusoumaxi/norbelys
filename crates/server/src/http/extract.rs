//! Extractors: a request is parsed once, into types, before the handler runs, and every
//! parsing failure becomes a [`Problem`] with the same shape as any other error.
//!
//! [`Json`] reads a JSON body with `serde_path_to_error`, so a type error names its RFC 6901
//! pointer, then runs `garde` validation, answering every invalid field in one
//! `validation_failed` problem.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::domain::time::Timestamp;
use crate::problem::{Code, FieldError, Problem};

/// A JSON body, deserialized and validated.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    S: Send + Sync,
    T: DeserializeOwned + garde::Validate<Context = ()>,
{
    type Rejection = Problem;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                let essence = value.split(';').next().unwrap_or_default().trim();
                essence.eq_ignore_ascii_case("application/json")
            });
        if !is_json {
            return Err(Problem::new(
                Code::UnsupportedMediaType,
                "Send the body as `Content-Type: application/json`.",
            ));
        }
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Problem::new(
                        Code::PayloadTooLarge,
                        "The body is larger than the operation accepts.",
                    )
                } else {
                    Problem::bad_request("The body could not be read.")
                }
            })?;
        parse(&bytes).map(Json)
    }
}

/// Deserializes and validates a JSON document.
///
/// # Errors
///
/// `400 invalid_request` for malformed JSON, `422 validation_failed` with pointers otherwise.
pub fn parse<T>(bytes: &[u8]) -> Result<T, Problem>
where
    T: DeserializeOwned + garde::Validate<Context = ()>,
{
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value: T = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let inner = error.inner();
        if inner.is_syntax() || inner.is_eof() {
            return Problem::bad_request("The body is not valid JSON.");
        }
        Problem::invalid_field(
            &json_pointer(&error.path().to_string()),
            "invalid",
            inner.to_string(),
        )
    })?;
    value.validate().map_err(|report| {
        Problem::validation(
            report
                .iter()
                .map(|(path, error)| FieldError {
                    pointer: json_pointer(&path.to_string()),
                    code: "invalid".to_owned(),
                    detail: error.message().to_owned(),
                })
                .collect(),
        )
    })?;
    Ok(value)
}

/// A JSON object read whole, for an operation whose body takes one of several forms: the handler
/// tells the form by the members the body names, then reads it as that form with
/// [`Object::parse`]. A mistake is then named as that form's own (its pointer and why), where an
/// untagged enum could only say that no form matched.
#[derive(Debug, Clone, Default, serde::Deserialize, garde::Validate)]
#[serde(transparent)]
pub struct Object(#[garde(skip)] pub serde_json::Map<String, serde_json::Value>);

impl Object {
    /// Reads the body as the form `T`, exactly as [`parse`] reads a body.
    ///
    /// # Errors
    ///
    /// `422 validation_failed` with pointers, as [`parse`].
    pub fn parse<T>(&self) -> Result<T, Problem>
    where
        T: DeserializeOwned + garde::Validate<Context = ()>,
    {
        let bytes = serde_json::to_vec(&self.0).map_err(|error| Problem::internal(&error))?;
        parse(&bytes)
    }
}

/// Turns a dotted path (`steps[0].delay_seconds`) into an RFC 6901 pointer
/// (`/steps/0/delay_seconds`).
fn json_pointer(path: &str) -> String {
    if path.is_empty() || path == "." {
        return String::new();
    }
    let mut pointer = String::new();
    for segment in path.split('.') {
        for part in segment.split(['[', ']']).filter(|part| !part.is_empty()) {
            pointer.push('/');
            pointer.push_str(&part.replace('~', "~0").replace('/', "~1"));
        }
    }
    pointer
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Path parameters. A malformed id answers `404 not_found`, as an absent one does: an id of
/// the wrong resource (a `msg_` id where a `ws_` id belongs) fails before any query, and an
/// absent row and a malformed id are indistinguishable to the client.
#[derive(Debug, Clone, Copy)]
pub struct Path<T>(pub T);

impl<S, T> FromRequestParts<S> for Path<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Path(value))
            .map_err(|_| Problem::not_found("resource"))
    }
}

/// Query parameters, refused with `422 validation_failed` when malformed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<S, T> FromRequestParts<S> for Query<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Query(value))
            .map_err(|rejection| Problem::invalid_field("?query", "invalid", rejection.body_text()))
    }
}

/// Creation-time bounds shared by list and export filters. Each supplied bound applies,
/// and flattening keeps the wire keys identical wherever the range is offered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct CreatedRange {
    /// Created at or after this instant.
    #[serde(
        default,
        rename = "created_at[gte]",
        skip_serializing_if = "Option::is_none"
    )]
    pub gte: Option<Timestamp>,
    /// Created after this instant.
    #[serde(
        default,
        rename = "created_at[gt]",
        skip_serializing_if = "Option::is_none"
    )]
    pub gt: Option<Timestamp>,
    /// Created at or before this instant.
    #[serde(
        default,
        rename = "created_at[lte]",
        skip_serializing_if = "Option::is_none"
    )]
    pub lte: Option<Timestamp>,
    /// Created before this instant.
    #[serde(
        default,
        rename = "created_at[lt]",
        skip_serializing_if = "Option::is_none"
    )]
    pub lt: Option<Timestamp>,
}
