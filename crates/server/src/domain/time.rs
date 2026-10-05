//! UTC instants and calendar days shared by policy, wire contracts and persistence.
//! Arithmetic and serialization are pure; callers obtain the current instant at the process
//! boundary. SQLx encodings live in `db::types` in this same crate.

use jiff::civil;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// An instant, always in UTC. On the wire it is RFC 3339 with a `Z` suffix
/// (<https://www.rfc-editor.org/rfc/rfc3339>), for example `2026-10-01T12:00:00Z`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub jiff::Timestamp);

impl Timestamp {
    /// This instant plus a duration, saturating at the representable range.
    #[must_use]
    pub fn plus(self, duration: std::time::Duration) -> Self {
        let signed = jiff::SignedDuration::try_from(duration).unwrap_or(jiff::SignedDuration::MAX);
        Self(
            self.0
                .saturating_add(signed)
                .unwrap_or(jiff::Timestamp::MAX),
        )
    }

    /// This instant minus a duration, saturating at the representable range.
    #[must_use]
    pub fn minus(self, duration: std::time::Duration) -> Self {
        let signed = jiff::SignedDuration::try_from(duration).unwrap_or(jiff::SignedDuration::MAX);
        Self(
            self.0
                .saturating_sub(signed)
                .unwrap_or(jiff::Timestamp::MIN),
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value
            .parse::<jiff::Timestamp>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Timestamp {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                utoipa::openapi::KnownFormat::DateTime,
            )))
            .into()
    }
}

impl utoipa::ToSchema for Timestamp {}

/// A calendar day. Daily ledgers (sending budgets, usage counters) are keyed by the UTC day
/// computed explicitly from an instant, never by a server's or a user's local day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub civil::Date);

impl Date {
    /// The UTC day of `at`.
    #[must_use]
    pub fn utc_day(at: Timestamp) -> Self {
        Self(at.0.to_zoned(jiff::tz::TimeZone::UTC).date())
    }
}

impl fmt::Display for Date {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value
            .parse::<civil::Date>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Date {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                utoipa::openapi::KnownFormat::Date,
            )))
            .into()
    }
}

impl utoipa::ToSchema for Date {}
