//! Preflight's decision: whether an address's domain can receive mail, from what DNS said.
//!
//! An address is checked in two steps and never by talking to its mailbox (no `RCPT` probe, no
//! verification mail): its syntax, then the routing of its domain. Routing follows RFC 5321
//! §5.1 (<https://www.rfc-editor.org/rfc/rfc5321#section-5.1>): mail goes to the domain's MX
//! hosts; a domain without MX records is its own implicit MX when it has an address record (A
//! or AAAA); a domain whose only MX is the root (`0 .`) publishes a null MX, RFC 7505
//! (<https://www.rfc-editor.org/rfc/rfc7505>), and accepts no mail at all.
//!
//! The decision is a table over what each lookup answered ([`after_mx`], [`after_addresses`]);
//! the lookups themselves live with the resolver, outside `domain/`. A lookup that could not be
//! answered (a timeout, `SERVFAIL`) is `unknown`, never `invalid`: a DNS outage must not turn
//! a good address into a refusal. Who acts on the verdict decides what it means for them: the
//! sender holds a message to a null-MX domain and checks again later, because a domain's
//! records can change, while `POST /preflight` only reports it.
//!
//! # The cache
//!
//! A verdict DNS answered the sender is kept per workspace and address in the preflight cache
//! (`recipient_validations`) for [`CACHE_TTL`], a day ([`Reason::ttl`]): the same day a
//! `no_route` hold waits before its re-check, so the re-check of a null MX finds the cached
//! verdict expired and asks DNS again. A lookup DNS could not answer is never cached (the next
//! check asks again, so an outage is never remembered as an answer), and neither is a syntax
//! verdict, which is decided without DNS.

use std::time::Duration;

/// How long the preflight cache keeps a verdict DNS answered (see the module).
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The verdict on an address: what `POST /preflight` reports and, for a verdict DNS answered,
/// what the preflight cache keeps (`recipient_validations.status`); `unknown` is never cached.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = PreflightStatus, rename_all = "snake_case")]
pub enum Status {
    /// Mail has a route: MX hosts, or the domain itself.
    Routable,
    /// No mail can reach the address: its syntax, or its domain says so.
    Invalid,
    /// DNS did not answer; check again later.
    Unknown,
}

/// Why an address got its verdict; parsed back from the cache (`recipient_validations.reason`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::IntoStaticStr,
    strum::EnumIter,
    strum::EnumString,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = PreflightReason, rename_all = "snake_case")]
pub enum Reason {
    /// The domain has MX hosts.
    Mx,
    /// The domain has no MX record but has an address record: it is its own mail host.
    ImplicitMx,
    /// The address is not an address.
    Syntax,
    /// The domain does not exist (`NXDOMAIN`).
    NoDomain,
    /// The domain publishes a null MX: it accepts no mail.
    NullMx,
    /// The domain exists but has neither MX nor address records.
    NoRoute,
    /// A lookup timed out or failed.
    DnsUnavailable,
}

impl Reason {
    /// The verdict this reason carries.
    #[must_use]
    pub fn status(self) -> Status {
        match self {
            Self::Mx | Self::ImplicitMx => Status::Routable,
            Self::Syntax | Self::NoDomain | Self::NullMx | Self::NoRoute => Status::Invalid,
            Self::DnsUnavailable => Status::Unknown,
        }
    }

    /// The reason as written on the wire and in the cache.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// How long the preflight cache keeps this verdict, or `None` when it is never cached: a
    /// lookup DNS could not answer, and a syntax verdict, decided without DNS (see the module).
    #[must_use]
    pub fn ttl(self) -> Option<Duration> {
        match self {
            Self::Mx | Self::ImplicitMx | Self::NoDomain | Self::NullMx | Self::NoRoute => {
                Some(CACHE_TTL)
            }
            Self::Syntax | Self::DnsUnavailable => None,
        }
    }
}

impl Status {
    /// The status as written on the wire and in the cache.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What the domain's MX lookup answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Mx {
    /// One or more MX hosts.
    Exchanges,
    /// A null MX: the root as the exchange.
    Null,
    /// The domain exists and has no MX record.
    Empty,
    /// The domain does not exist.
    NoDomain,
    /// No answer: a timeout or a server failure.
    Failed,
}

/// What the domain's address lookup (A and AAAA) answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Addresses {
    /// At least one address.
    Found,
    /// No address record.
    Empty,
    /// The domain does not exist.
    NoDomain,
    /// No answer.
    Failed,
}

/// The step after the MX lookup: a verdict, or the address lookup an empty MX answer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Decided(Reason),
    LookUpAddresses,
}

/// Decides from the MX answer, or asks for the address lookup (RFC 5321's implicit MX).
#[must_use]
pub fn after_mx(mx: Mx) -> Step {
    match mx {
        Mx::Exchanges => Step::Decided(Reason::Mx),
        Mx::Null => Step::Decided(Reason::NullMx),
        Mx::NoDomain => Step::Decided(Reason::NoDomain),
        Mx::Failed => Step::Decided(Reason::DnsUnavailable),
        Mx::Empty => Step::LookUpAddresses,
    }
}

/// Decides from the address lookup of a domain without MX records.
#[must_use]
pub fn after_addresses(addresses: Addresses) -> Reason {
    match addresses {
        Addresses::Found => Reason::ImplicitMx,
        Addresses::Empty => Reason::NoRoute,
        Addresses::NoDomain => Reason::NoDomain,
        Addresses::Failed => Reason::DnsUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{Addresses, CACHE_TTL, Mx, Reason, Status, Step, after_addresses, after_mx};
    use crate::domain::policy::delivery::HoldReason;

    /// Every verdict DNS answered is cached, and for no longer than a `no_route` hold waits, so
    /// the hold's re-check of a null MX asks DNS again instead of reading the cache; a failed
    /// lookup is never cached (an outage is not an answer), nor a syntax verdict. Every reason
    /// also reads back from its cached spelling.
    #[test]
    fn every_reason_has_its_cache_time() {
        for reason in Reason::iter() {
            let expected = match reason {
                Reason::Mx
                | Reason::ImplicitMx
                | Reason::NoDomain
                | Reason::NullMx
                | Reason::NoRoute => Some(CACHE_TTL),
                Reason::Syntax | Reason::DnsUnavailable => None,
            };
            assert_eq!(reason.ttl(), expected, "{reason:?}");
            assert_eq!(reason.as_str().parse::<Reason>(), Ok(reason));
        }
        assert!(
            i64::try_from(CACHE_TTL.as_secs()).unwrap()
                <= HoldReason::NoRoute.review_after().as_secs()
        );
    }

    /// Every MX answer has its step, and only an empty answer asks for the address lookup: a
    /// null MX is a refusal by the domain itself, a failed lookup is never a refusal.
    #[test]
    fn every_mx_answer_has_its_step() {
        for mx in Mx::iter() {
            let expected = match mx {
                Mx::Exchanges => Step::Decided(Reason::Mx),
                Mx::Null => Step::Decided(Reason::NullMx),
                Mx::Empty => Step::LookUpAddresses,
                Mx::NoDomain => Step::Decided(Reason::NoDomain),
                Mx::Failed => Step::Decided(Reason::DnsUnavailable),
            };
            assert_eq!(after_mx(mx), expected, "{mx:?}");
        }
    }

    /// Without MX records, an address record makes the domain its own mail host (RFC 5321
    /// §5.1); no record at all is no route; a failed lookup stays unknown.
    #[test]
    fn every_address_answer_has_its_reason() {
        for addresses in Addresses::iter() {
            let expected = match addresses {
                Addresses::Found => Reason::ImplicitMx,
                Addresses::Empty => Reason::NoRoute,
                Addresses::NoDomain => Reason::NoDomain,
                Addresses::Failed => Reason::DnsUnavailable,
            };
            assert_eq!(after_addresses(addresses), expected, "{addresses:?}");
        }
    }

    /// Each reason carries one status: only DNS failing is unknown, only a route is routable.
    #[test]
    fn every_reason_has_its_status() {
        for reason in Reason::iter() {
            let expected = match reason {
                Reason::Mx | Reason::ImplicitMx => Status::Routable,
                Reason::DnsUnavailable => Status::Unknown,
                Reason::Syntax | Reason::NoDomain | Reason::NullMx | Reason::NoRoute => {
                    Status::Invalid
                }
            };
            assert_eq!(reason.status(), expected, "{reason:?}");
        }
    }
}
