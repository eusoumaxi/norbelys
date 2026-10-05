//! Suppressions' vocabulary and the decisions about them that do not need the database: which
//! reasons a person may lift, and how a list of addresses a person gives is read.
//!
//! A suppression is irreversible, with one exception: a person may remove a `manual`
//! suppression (theirs to take back). Every other reason records what the recipient or their
//! provider said (an unsubscribe, a complaint, a bounce, a closed account) and is read-only:
//! lifting it would mail someone who refused mail, or an address that does not exist, on
//! nobody's evidence.
//!
//! # A list of addresses
//!
//! A person may suppress up to [`LIST_MAX`] addresses in one request ([`read_list`]). Each
//! entry is parsed as an address on its own: an entry that is not one is refused alone, with
//! its position and the reason, and the others are suppressed, because one typo in a pasted
//! list should not cost the rest. An address given twice, in any ASCII case, is one address (its
//! key, the suppressions' own uniqueness), kept as first spelled. The addresses come out in the
//! order of their keys, the order in which their rows are written, so two requests whose lists
//! overlap wait for each other's rows in the same order and never deadlock.

use std::collections::BTreeMap;

use super::email::{EmailAddress, EmailError};

/// The most addresses one request suppresses. A list this long is at most about 256 KiB of
/// JSON, well inside a request's body limit, and its suppressions, their events and the
/// enrollments they stop are written in one transaction well inside a request's time bound.
pub const LIST_MAX: usize = 1_000;

/// A list of addresses to suppress, as [`read_list`] reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct List {
    /// The addresses, each once by its key (as first spelled), in the order of their keys.
    pub addresses: Vec<EmailAddress>,
    /// The entries that are not addresses, in the order given.
    pub refused: Vec<Refused>,
}

/// An entry of a list that is not an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// Its position in the list, from 0.
    pub index: usize,
    /// The entry as given.
    pub value: String,
    /// Why it is not an address.
    pub error: EmailError,
}

/// Reads a list of addresses to suppress (see the module): the addresses, each once, in key
/// order, and the entries that are not addresses.
#[must_use]
pub fn read_list<S: AsRef<str>>(entries: &[S]) -> List {
    let mut addresses = BTreeMap::new();
    let mut refused = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match EmailAddress::parse(entry.as_ref()) {
            Ok(address) => {
                addresses.entry(address.key()).or_insert(address);
            }
            Err(error) => refused.push(Refused {
                index,
                value: entry.as_ref().to_owned(),
                error,
            }),
        }
    }
    List {
        addresses: addresses.into_values().collect(),
        refused,
    }
}

/// Why an address is suppressed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = SuppressionReason)]
pub enum Reason {
    /// The recipient unsubscribed.
    Unsubscribe,
    /// The address does not exist, as an authenticated bounce said.
    Bounce,
    /// The recipient reported the mail as unwanted.
    Complaint,
    /// A person suppressed it.
    Manual,
    /// The recipient moved to another address.
    AddressChanged,
    /// The mailbox was closed.
    AccountClosed,
    /// The recipient's domain accepts no mail.
    NoMailService,
}

impl Reason {
    /// The reason as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Whether a person may lift a suppression of this reason (see the module).
    #[must_use]
    pub fn liftable(self) -> bool {
        match self {
            Self::Manual => true,
            Self::Unsubscribe
            | Self::Bounce
            | Self::Complaint
            | Self::AddressChanged
            | Self::AccountClosed
            | Self::NoMailService => false,
        }
    }
}

/// Who or what created a suppression: a person, the recipient's unsubscribe, or the source of
/// the delivery event that proved it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = SuppressionSource)]
pub enum Source {
    Manual,
    Unsubscribe,
    Smtp,
    ProviderApi,
    ProviderWebhook,
    Dsn,
    Arf,
    InboundNotice,
}

impl Source {
    /// The source as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{Reason, Refused, read_list};
    use crate::domain::email::EmailError;

    /// Only a manual suppression may be lifted: every reason that records what a recipient or
    /// their provider said stays, so no one is mailed again on nobody's evidence. Generated over
    /// the reasons, so a new one fails until it is decided.
    #[test]
    fn only_a_manual_suppression_is_liftable() {
        let liftable: Vec<Reason> = Reason::iter().filter(|reason| reason.liftable()).collect();
        assert_eq!(liftable, [Reason::Manual]);
    }

    /// A list is read entry by entry: an address given again in another ASCII case is the same
    /// address, kept as first spelled, so the request suppresses it once and counts it once; an
    /// entry that is not an address is refused alone with its position, the entry as given and
    /// why, so the person can find and fix it while the rest are suppressed; and the addresses
    /// come out in key order, the order every list's rows are written in, so two overlapping
    /// requests cannot deadlock on each other's rows.
    #[test]
    fn a_list_counts_each_address_once_and_refuses_entries_alone() {
        let list = read_list(&[
            "zed@example.com",
            "Ada@Example.com",
            "no-at-sign",
            "ada@example.com",
            " bob@example.com ",
            "a b@example.com",
        ]);
        let addresses: Vec<&str> = list.addresses.iter().map(|a| a.as_str()).collect();
        assert_eq!(
            addresses,
            ["Ada@Example.com", "bob@example.com", "zed@example.com"]
        );
        assert_eq!(
            list.refused,
            [
                Refused {
                    index: 2,
                    value: "no-at-sign".to_owned(),
                    error: EmailError::Shape,
                },
                Refused {
                    index: 5,
                    value: "a b@example.com".to_owned(),
                    error: EmailError::Characters,
                },
            ]
        );
    }
}
