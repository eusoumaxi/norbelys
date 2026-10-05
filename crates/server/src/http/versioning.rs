//! Optimistic concurrency: every resource that can be updated carries `version`, sent again as
//! the `ETag` of each response that returns the resource, and an update may send `If-Match`
//! with the version its client last read. A stale `If-Match` answers `412
//! precondition_failed` and changes nothing, so two people editing the same resource cannot
//! silently overwrite each other (the "lost update" of RFC 9110 §13.1.1,
//! <https://www.rfc-editor.org/rfc/rfc9110#section-13.1.1>). `If-Match` stays optional:
//! a client that sends none takes the last write.
//!
//! **What a version is.** The row's `updated_at` in microseconds since the Unix epoch. Every
//! table that can change has a trigger that sets `updated_at` to the writing transaction's
//! start on each `UPDATE`, so a version needs no column, no counter and no code in any write
//! path, and a change made by a background role (a health check, a counter) moves it too. Two
//! writes inside one transaction give one version, which is right: nobody can read the state
//! between them. Two transactions that change the same row run one after the other (the row
//! lock serialises them) and give two versions, unless both began in the same microsecond.
//!
//! **Lists embedded in an object.** A version covers what the object shows, and some objects
//! show rows of other tables (a person's `group_ids`, a campaign's `steps`). Every write that
//! adds to such a list from outside the object's own update (an import adding memberships, a job
//! creating steps) also updates the parent row in the same transaction (any `UPDATE` of the row
//! fires the trigger), so the parent's version moves. Without that, a client that read the list,
//! then replaces it whole with a matching `If-Match`, would silently erase what the other write
//! added: the lost update this module exists to prevent. An import does it through its upsert:
//! `ON CONFLICT … DO UPDATE` runs the `UPDATE` trigger of every person who existed already, in
//! the transaction that adds their memberships.
//!
//! **Removals that leave the version alone.** Deleting a group removes it from every member's
//! `group_ids` without touching the members' rows, deliberately. Touching them would rewrite one
//! row per member (and add an entry to every index of `people`, since `updated_at` is indexed),
//! and the deletion would lock the group's row and then its members' rows, while an import locks
//! people first and then needs the group's row for the foreign key of the memberships it adds:
//! opposite orders, a deadlock by construction. No update can be lost to the removal: a client
//! that replaces `group_ids` with a list naming the deleted group gets `404` (the group no longer
//! exists), and one whose list does not name it ends with exactly the list it sent, which is what
//! it would have asked for had it seen the deletion. Counts an object shows (a group's
//! `people_count`, a connection's `usage`) are computed when it is read and written by no update,
//! so they are not part of the version either.
//!
//! **How an update uses it.** The operation locks the row (`SELECT … FOR UPDATE`), computes the
//! current version with [`of`], calls [`IfMatch::check`] and only then writes. Checking under
//! the lock is what makes the precondition exact: no other write can land between the check
//! and the update. Each resource's module has a `lock_version` that takes the row's lock (after
//! any lock its lock order puts first) and returns the version; the handler checks it, `404`
//! first when the row does not exist. The response returns the new version in the body and in
//! `ETag` ([`Tagged`]).
//!
//! **The header.** `If-Match` is `*` (any current version: the precondition always holds for
//! a resource that exists) or a comma-separated list of entity tags. Versions are strong tags
//! (`"1790000000000000"`); `If-Match` compares strongly, so a weak tag (`W/"…"`) never matches,
//! and a tag that is not one of our versions never matches either. A header that is not valid
//! syntax answers `400 invalid_request`.
//!
//! **In the OpenAPI document.** Every update lists [`IfMatch`] among its `params` (one
//! declaration of the optional header, shared by all of them) and declares `412`; every response
//! that returns one versioned resource declares its `ETag`. The document's invariants test holds
//! every operation to that.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::openapi::Required;
use utoipa::openapi::path::{Parameter, ParameterBuilder, ParameterIn};
use utoipa::openapi::schema::{ObjectBuilder, Type};

use crate::domain::time::Timestamp;
use crate::problem::{Code, Problem};

/// The version of a row whose `updated_at` is `updated_at`.
#[must_use]
pub fn of(updated_at: Timestamp) -> i64 {
    updated_at.0.as_microsecond()
}

/// The request's `If-Match` precondition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IfMatch {
    /// No `If-Match`: the update applies to whatever version is current.
    #[default]
    Absent,
    /// `If-Match: *`: the resource must exist, which the update checks anyway.
    Any,
    /// The versions the client accepts; tags that are not versions are dropped, since they
    /// can never match.
    Versions(Vec<i64>),
}

impl IfMatch {
    /// Parses an `If-Match` value.
    ///
    /// # Errors
    ///
    /// `400 invalid_request` when the value is neither `*` nor a list of entity tags.
    pub fn parse(value: &str) -> Result<Self, Problem> {
        let value = value.trim();
        if value == "*" {
            return Ok(Self::Any);
        }
        let mut versions = Vec::new();
        for tag in value.split(',').map(str::trim) {
            let (weak, opaque) = match tag.strip_prefix("W/") {
                Some(rest) => (true, rest),
                None => (false, tag),
            };
            let inner = opaque
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .filter(|inner| !inner.contains('"'))
                .ok_or_else(malformed)?;
            if !weak && let Ok(version) = inner.parse::<i64>() {
                versions.push(version);
            }
        }
        Ok(Self::Versions(versions))
    }

    /// Checks the precondition against the current version, read under the row's lock.
    ///
    /// # Errors
    ///
    /// `412 precondition_failed` when the client named versions and none is the current one.
    pub fn check(&self, current: i64) -> Result<(), Problem> {
        match self {
            Self::Absent | Self::Any => Ok(()),
            Self::Versions(versions) if versions.contains(&current) => Ok(()),
            Self::Versions(_) => Err(Problem::new(
                Code::PreconditionFailed,
                "The resource changed since the version named in `If-Match`; read it again and \
                 reapply the change.",
            )),
        }
    }
}

fn malformed() -> Problem {
    Problem::bad_request(
        "`If-Match` must be `*` or a list of quoted versions, such as `\"1790000000000000\"`.",
    )
}

impl<S: Send + Sync> FromRequestParts<S> for IfMatch {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let mut values = parts.headers.get_all(header::IF_MATCH).iter().peekable();
        if values.peek().is_none() {
            return Ok(Self::Absent);
        }
        let mut versions = Vec::new();
        for value in values {
            match Self::parse(value.to_str().map_err(|_| malformed())?)? {
                Self::Any => return Ok(Self::Any),
                Self::Versions(more) => versions.extend(more),
                Self::Absent => {}
            }
        }
        Ok(Self::Versions(versions))
    }
}

/// The optional `If-Match` header as an update declares it: listed in the update's `params`
/// (`params(("id" = Id<Person>, Path, …), IfMatch)`), so every update documents it in the same
/// words.
impl utoipa::IntoParams for IfMatch {
    fn into_params(_parameter_in: impl Fn() -> Option<ParameterIn>) -> Vec<Parameter> {
        vec![
            ParameterBuilder::new()
                .name("If-Match")
                .parameter_in(ParameterIn::Header)
                .required(Required::False)
                .description(Some(
                    "The `version` the change applies to, quoted as the `ETag` that carried it \
                     (`\"1790000000000000\"`), or `*`. A version that is no longer current answers \
                     `412 precondition_failed` and changes nothing; without the header the change \
                     applies to whatever version is current.",
                ))
                .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
                .example(Some(serde_json::Value::String(
                    "\"1790000000000000\"".to_owned(),
                )))
                .build(),
        ]
    }
}

/// A JSON response that also carries its resource's version as a strong `ETag`.
#[derive(Debug, Clone)]
pub struct Tagged<T> {
    /// The resource's version, the same value as its `version` field.
    pub version: i64,
    /// The resource.
    pub body: T,
}

impl<T: Serialize> IntoResponse for Tagged<T> {
    fn into_response(self) -> Response {
        let mut response = axum::Json(self.body).into_response();
        if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", self.version)) {
            response.headers_mut().insert(header::ETAG, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forms a client may send parse to the precondition they mean: `*`, one or several
    /// strong versions, and weak or foreign tags, which are valid syntax but can never match.
    #[test]
    fn if_match_values_parse_to_what_they_mean() {
        let cases = [
            ("*", IfMatch::Any),
            ("\"17\"", IfMatch::Versions(vec![17])),
            (" \"17\" , \"18\" ", IfMatch::Versions(vec![17, 18])),
            ("W/\"17\"", IfMatch::Versions(vec![])),
            ("\"abc\"", IfMatch::Versions(vec![])),
        ];
        for (value, expected) in cases {
            assert_eq!(IfMatch::parse(value).ok(), Some(expected), "{value}");
        }
    }

    /// A value that is not entity-tag syntax is the client's mistake (400), not a failed
    /// precondition, so the client learns to fix the header rather than to re-read.
    #[test]
    fn malformed_if_match_values_are_refused() {
        for value in ["17", "\"17", "W/17", "\"1\"7\""] {
            let refused = IfMatch::parse(value).err().map(|problem| problem.code);
            assert_eq!(refused, Some(Code::InvalidRequest), "{value}");
        }
    }

    /// Only a named current version, `*` or no header at all lets the update through; any
    /// other list fails with `412`, including an empty one (every tag was weak or foreign).
    #[test]
    fn the_precondition_holds_only_for_the_current_version() {
        let current = 1_790_000_000_000_000;
        assert!(IfMatch::Absent.check(current).is_ok());
        assert!(IfMatch::Any.check(current).is_ok());
        assert!(IfMatch::Versions(vec![1, current]).check(current).is_ok());
        for stale in [
            IfMatch::Versions(vec![current - 1]),
            IfMatch::Versions(vec![]),
        ] {
            let failed = stale.check(current).err().map(|problem| problem.code);
            assert_eq!(failed, Some(Code::PreconditionFailed));
        }
    }
}
