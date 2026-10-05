//! DNS: the one resolver of a process, shared by everything that asks DNS a question.
//!
//! A role builds one [`Resolver`] at start and hands it to what needs it: preflight reads an
//! address's mail routing (MX, then A/AAAA), the sending-domain checks read ownership TXT
//! records and tracking CNAMEs, and mail sessions resolve the hosts tenants typed (through
//! `norbelys-mail`'s connector, which takes the underlying resolver, [`Resolver::hickory`]).
//! One resolver means one cache and one set of bounds for the whole process: an answer one
//! caller fetched serves the next for its TTL, and no caller waits longer than the others.
//!
//! The resolver uses the host's DNS configuration with 3 seconds per query, two attempts, and a
//! cache of 4,096 answers, each kept for its record's TTL. Every name is looked up fully
//! qualified (`example.com.`), so the host's search domains can never turn a typo into an
//! internal name. A lookup answers what DNS said, including "no such domain" and "no records
//! of this type" as errors the caller reads as answers (`NetError::is_nx_domain`,
//! `NetError::is_no_records_found`); a timeout or a server failure is a failed lookup, which
//! no caller may read as a refusal.

use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::lookup::Lookup;
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::net::NetError;
use hickory_resolver::proto::rr::RecordType;

/// One query's timeout, and how many times it is tried.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const QUERY_ATTEMPTS: usize = 2;
/// Answers the resolver keeps, each for its record's TTL.
const CACHE_SIZE: u64 = 4_096;

/// The process's DNS resolver (see the module).
#[derive(Clone)]
pub struct Resolver {
    inner: TokioResolver,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Resolver(..)")
    }
}

/// `name` fully qualified: with its trailing dot.
fn fqdn(name: &str) -> String {
    format!("{}.", name.trim_end_matches('.'))
}

impl Resolver {
    /// A resolver with the host's DNS configuration and the process's bounds.
    ///
    /// # Errors
    ///
    /// The host's DNS configuration cannot be read.
    pub fn system() -> Result<Self, NetError> {
        let mut builder = TokioResolver::builder_tokio()?;
        builder.options_mut().timeout = QUERY_TIMEOUT;
        builder.options_mut().attempts = QUERY_ATTEMPTS;
        builder.options_mut().cache_size = CACHE_SIZE;
        Ok(Self {
            inner: builder.build()?,
        })
    }

    /// A resolver without name servers: every lookup of a name fails at once, while an IP
    /// literal still resolves to itself. For tests, which never depend on the network and reach
    /// their fakes by IP literal.
    #[cfg(test)]
    #[must_use]
    pub fn offline() -> Self {
        let inner = TokioResolver::builder_with_config(
            hickory_resolver::config::ResolverConfig::default(),
            hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
        )
        .build()
        .expect("a resolver without name servers builds");
        Self { inner }
    }

    /// The underlying resolver, for a library that takes one (the mail sessions' connector).
    /// It shares this resolver's cache and bounds.
    #[must_use]
    pub fn hickory(&self) -> TokioResolver {
        self.inner.clone()
    }

    /// The MX records of `domain`.
    ///
    /// # Errors
    ///
    /// What DNS answered when it was not records (see the module).
    pub async fn mx(&self, domain: &str) -> Result<Lookup, NetError> {
        self.inner.mx_lookup(fqdn(domain).as_str()).await
    }

    /// The addresses (A and AAAA) of `host`.
    ///
    /// # Errors
    ///
    /// What DNS answered when it was not records (see the module).
    pub async fn addresses(&self, host: &str) -> Result<LookupIp, NetError> {
        self.inner.lookup_ip(fqdn(host).as_str()).await
    }

    /// The TXT records at `name`.
    ///
    /// # Errors
    ///
    /// What DNS answered when it was not records (see the module).
    pub async fn txt(&self, name: &str) -> Result<Lookup, NetError> {
        self.inner.txt_lookup(fqdn(name).as_str()).await
    }

    /// The CNAME record at `name`.
    ///
    /// # Errors
    ///
    /// What DNS answered when it was not records (see the module).
    pub async fn cname(&self, name: &str) -> Result<Lookup, NetError> {
        self.inner
            .lookup(fqdn(name).as_str(), RecordType::CNAME)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{Resolver, fqdn};

    /// Every name is asked fully qualified, whether or not it was written with its trailing
    /// dot, so the host's search domains never complete it.
    #[test]
    fn names_are_asked_fully_qualified() {
        assert_eq!(fqdn("example.com"), "example.com.");
        assert_eq!(fqdn("example.com."), "example.com.");
    }

    /// A resolver without name servers fails a lookup at once instead of waiting out its
    /// timeouts, which is what keeps the tests that build one fast and independent of the
    /// network.
    #[tokio::test]
    async fn an_offline_resolver_fails_at_once() {
        let started = std::time::Instant::now();
        assert!(Resolver::offline().mx("example.com").await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
