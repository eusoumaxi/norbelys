//! Email addresses: the one place they are parsed and normalised.
//!
//! Parsing is deliberately practical rather than a full RFC 5322 grammar: one `@`, a local
//! part of 1–64 characters, a domain of dot-separated labels, at most 254 characters in
//! total (the limits of RFC 5321, <https://www.rfc-editor.org/rfc/rfc5321#section-4.5.3.1>),
//! and no whitespace, control characters or angle brackets that would let an address smuggle
//! extra header content.
//!
//! **The comparison key** is the whole address in ASCII lowercase and nothing else. Dots and
//! `+tags` are never folded: whether `a.b+x@example.com` reaches the same mailbox as
//! `ab@example.com` is the receiving provider's private rule, and guessing it would merge
//! people or suppressions that are not the same. The database computes the same key with
//! its `ascii_lower()` function, so uniqueness in SQL and equality in Rust agree.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A syntactically valid address, as written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmailAddress(String);

/// Why a string is not an address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmailError {
    #[error("an email address needs exactly one `@` with text on both sides")]
    Shape,
    #[error("an email address is at most 254 characters")]
    TooLong,
    #[error("the local part is at most 64 characters")]
    LocalTooLong,
    #[error("the domain is not a valid host name")]
    Domain,
    #[error("an email address cannot contain spaces, control characters or angle brackets")]
    Characters,
}

impl EmailAddress {
    /// Parses an address: one `@`, a local part of 1–64 characters, a domain of dot-separated
    /// labels (letters, digits, hyphens; IDNs in their ASCII form or as Unicode letters).
    ///
    /// # Errors
    ///
    /// The address is malformed.
    pub fn parse(value: &str) -> Result<Self, EmailError> {
        let value = value.trim();
        if value.len() > 254 {
            return Err(EmailError::TooLong);
        }
        if value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
        {
            return Err(EmailError::Characters);
        }
        let (local, domain) = value.rsplit_once('@').ok_or(EmailError::Shape)?;
        if local.is_empty() || domain.is_empty() || local.contains('@') && !local.starts_with('"') {
            return Err(EmailError::Shape);
        }
        if local.len() > 64 {
            return Err(EmailError::LocalTooLong);
        }
        let labels_ok = domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        });
        if !labels_ok || !domain.contains('.') {
            return Err(EmailError::Domain);
        }
        Ok(Self(value.to_owned()))
    }

    /// The address as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The comparison key: ASCII lowercase, nothing folded.
    #[must_use]
    pub fn key(&self) -> String {
        self.0.to_ascii_lowercase()
    }

    /// The domain, ASCII lowercase.
    #[must_use]
    pub fn domain(&self) -> String {
        self.0
            .rsplit_once('@')
            .map_or_else(String::new, |(_, domain)| domain.to_ascii_lowercase())
    }

    /// The local part, as written.
    #[must_use]
    pub fn local(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(local, _)| local)
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for EmailAddress {
    type Err = EmailError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for EmailAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EmailAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for EmailAddress {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                utoipa::openapi::KnownFormat::Email,
            )))
            .max_length(Some(254))
            .into()
    }
}

impl utoipa::ToSchema for EmailAddress {}
