//! Subscription filters and custom header validation, without transport or storage access.
//!
//! Values within a filter are alternatives; populated filters must all match. Empty filters
//! subscribe to every resource. Custom headers cannot replace signature or framing headers.

use super::ids::{Campaign, Connection, Id};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Resource restrictions on a webhook subscription.
#[derive(Debug, Clone, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    /// Connections whose events are wanted; empty means any connection.
    #[serde(default)]
    pub connection_ids: Vec<Id<Connection>>,
    /// Campaigns whose events are wanted; empty means any campaign.
    #[serde(default)]
    pub campaign_ids: Vec<Id<Campaign>>,
}

impl Filters {
    /// Whether both configured dimensions match the event's frozen routing data.
    #[must_use]
    pub fn matches(&self, data: &serde_json::Value) -> bool {
        let connection = data
            .get("connection_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| id.parse::<Id<Connection>>().ok());
        let campaign = data
            .get("campaign_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| id.parse::<Id<Campaign>>().ok());
        (self.connection_ids.is_empty()
            || connection.is_some_and(|id| self.connection_ids.contains(&id)))
            && (self.campaign_ids.is_empty()
                || campaign.is_some_and(|id| self.campaign_ids.contains(&id)))
    }

    /// Refuses unbounded subscription documents.
    ///
    /// # Errors
    /// Either dimension contains more than 100 ids.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.connection_ids.len() > 100 || self.campaign_ids.len() > 100 {
            return Err("Each filter accepts at most 100 ids.");
        }
        Ok(())
    }
}

/// Checks and normalizes custom headers, including credentials that will be sealed at rest.
///
/// # Errors
/// Invalid HTTP characters, duplicate names, reserved transport headers or excessive size.
pub fn headers(input: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, &'static str> {
    if input.len() > 16 || input.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > 8192 {
        return Err("At most 16 headers and 8,192 bytes are accepted.");
    }
    let mut normalized = BTreeMap::new();
    for (name, value) in input {
        let name = name.to_ascii_lowercase();
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || !value.bytes().all(|b| b == b'\t' || (32..=126).contains(&b))
        {
            return Err("Headers must have valid HTTP names and printable ASCII values.");
        }
        if name.starts_with("webhook-")
            || matches!(
                name.as_str(),
                "host"
                    | "content-type"
                    | "content-length"
                    | "content-encoding"
                    | "transfer-encoding"
                    | "connection"
                    | "trailer"
                    | "te"
                    | "upgrade"
                    | "proxy-authorization"
                    | "proxy-authenticate"
                    | "cookie"
                    | "expect"
            )
        {
            return Err("Signature, routing and framing headers are reserved.");
        }
        if normalized.insert(name, value.clone()).is_some() {
            return Err("Header names must be unique without regard to case.");
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmatched dimensions cannot broaden a subscription, while absent filters accept it.
    #[test]
    fn filters_require_every_dimension() {
        let connection = Id::<Connection>::new();
        let campaign = Id::<Campaign>::new();
        let filters = Filters {
            connection_ids: vec![connection],
            campaign_ids: vec![campaign],
        };
        assert!(!filters.matches(&serde_json::json!({"connection_id":connection})));
        assert!(
            filters
                .matches(&serde_json::json!({"connection_id":connection,"campaign_id":campaign}))
        );
        assert!(Filters::default().matches(&serde_json::json!({})));
    }

    /// Custom authentication works without allowing header injection or signature replacement.
    #[test]
    fn header_validation() {
        for (name, value) in [
            ("Host", "example.com"),
            ("Webhook-Id", "bad"),
            ("X-Test", "a\r\nb"),
        ] {
            assert!(headers(&BTreeMap::from([(name.into(), value.into())])).is_err());
        }
        let valid = headers(&BTreeMap::from([(
            "Authorization".into(),
            "Bearer token".into(),
        )]))
        .unwrap();
        assert_eq!(valid.get("authorization").unwrap(), "Bearer token");
        assert!(
            headers(&BTreeMap::from([
                ("X-Key".into(), "a".into()),
                ("x-key".into(), "b".into())
            ]))
            .is_err()
        );
    }
}
