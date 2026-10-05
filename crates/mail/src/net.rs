//! Where SMTP and IMAP sessions connect: one [`Connector`] resolves a host through the
//! process's `hickory-resolver` and applies the address policy before any socket opens.
//!
//! A tenant types the SMTP and IMAP host of its mailbox, so a host is a request-forgery risk:
//! with [`AddressPolicy::PublicOnly`] every address the name resolves to must be public
//! unicast, or the connection is refused, and the session then connects to that checked
//! address while TLS still verifies the certificate against the host name (no rebinding
//! window). [`AddressPolicy::Any`] allows non-public addresses (loopback, RFC 1918) for
//! caller-controlled hosts such as a managed MTA, and for local development; only it allows
//! plaintext sessions.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use hickory_resolver::TokioResolver;
use rustls_platform_verifier::BuilderVerifierExt as _;

/// Which addresses a session may connect to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressPolicy {
    /// Public unicast only, and TLS always: hosts typed by tenants.
    PublicOnly,
    /// Any address, plaintext allowed: caller-controlled hosts (for example a managed MTA) and
    /// local development.
    Any,
}

/// Resolves hosts under an [`AddressPolicy`] and holds the TLS client configuration of IMAP
/// sessions (SMTP sessions use `lettre`'s, built per host in [`crate::smtp`]).
#[derive(Clone)]
pub struct Connector {
    resolver: TokioResolver,
    policy: AddressPolicy,
    tls: tokio_rustls::TlsConnector,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connector")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// Why a connector cannot be built.
#[derive(Debug, thiserror::Error)]
#[error("the TLS client configuration could not be built: {0}")]
pub struct ConnectorError(#[from] rustls::Error);

/// Why a host cannot be connected to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The name did not resolve.
    #[error("`{0}` did not resolve")]
    Unresolved(String),
    /// The name resolves to a private or reserved address under [`AddressPolicy::PublicOnly`].
    #[error("`{0}` resolves to a private or reserved address")]
    NotPublic(String),
}

impl Connector {
    /// A connector over `resolver` (typically the process's one resolver, also used for DNS
    /// checks before connect) with the platform's certificate verifier and `aws-lc-rs`.
    ///
    /// # Errors
    ///
    /// The platform verifier or the TLS versions could not be configured.
    pub fn new(resolver: TokioResolver, policy: AddressPolicy) -> Result<Self, ConnectorError> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_platform_verifier()?
            .with_no_client_auth();
        Ok(Self {
            resolver,
            policy,
            tls: tokio_rustls::TlsConnector::from(Arc::new(config)),
        })
    }

    /// The policy this connector applies.
    #[must_use]
    pub fn policy(&self) -> AddressPolicy {
        self.policy
    }

    /// One checked address of `host`: IPv4 first, because container hosts often have no IPv6
    /// egress and a session tries one address.
    pub(crate) async fn resolve(&self, host: &str, port: u16) -> Result<SocketAddr, ResolveError> {
        let unresolved = || ResolveError::Unresolved(host.to_owned());
        let addresses: Vec<IpAddr> = self
            .resolver
            .lookup_ip(host)
            .await
            .map_err(|_| unresolved())?
            .iter()
            .collect();
        if self.policy == AddressPolicy::PublicOnly && !addresses.iter().all(|ip| is_public(*ip)) {
            return Err(ResolveError::NotPublic(host.to_owned()));
        }
        let ip = addresses
            .iter()
            .find(|ip| ip.is_ipv4())
            .or(addresses.first())
            .ok_or_else(unresolved)?;
        Ok(SocketAddr::new(*ip, port))
    }

    pub(crate) fn tls(&self) -> &tokio_rustls::TlsConnector {
        &self.tls
    }
}

/// Public unicast only. IPv4-mapped, well-known NAT64 and 6to4 addresses inherit the IPv4
/// rule; stable Rust has no `IpAddr::is_global`, so this follows IANA's special-purpose address
/// registries conservatively, excluding whole blocks where practical.
#[must_use]
pub fn is_public(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V4(ip) => return is_public_v4(ip),
        IpAddr::V6(ip) => ip,
    };
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    let v4 = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    match ip.segments() {
        [0x64, 0xff9b, 0, 0, 0, 0, high, low] => is_public_v4(v4(high, low)),
        [0x2002, high, low, ..] => is_public_v4(v4(high, low)),
        [0x2001, second, ..] if second < 0x200 => false,
        [0x2001, 0xdb8, ..] | [0x3ffe, ..] => false,
        [0x3fff, second, ..] if second < 0x1000 => false,
        [first, ..] => (0x2000..=0x3fff).contains(&first),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    let shared = first == 100 && (64..=127).contains(&second);
    let benchmark = first == 198 && (second == 18 || second == 19);
    let protocol = first == 192 && second == 0 && (third == 0 || third == 2);
    let relay = first == 192 && second == 88 && third == 99;
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || shared
        || benchmark
        || protocol
        || relay
        || first == 0
        || first >= 240)
}

#[cfg(test)]
mod tests {
    use super::{AddressPolicy, ResolveError, is_public};
    use crate::testing::connector;

    /// Tenant-typed hosts may only reach public unicast addresses: loopback, private and
    /// link-local ranges (the cloud metadata service), CGNAT, documentation and benchmark
    /// ranges, multicast, and IPv6 forms that embed a private IPv4 address (mapped, NAT64,
    /// 6to4) are all refused, while ordinary public addresses pass.
    #[test]
    fn only_public_unicast_addresses_are_public() {
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "142.250.72.14",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(public.parse().expect("an address")), "{public}");
        }
        for private in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.3.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.2.10",
            "198.18.0.1",
            "192.88.99.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:a00:1::",
            "2001:db8::1",
            "2001::1",
            "3fff::1",
        ] {
            assert!(
                !is_public(private.parse().expect("an address")),
                "{private}"
            );
        }
    }

    /// The policy applies to what a name resolves to, before any socket opens: under
    /// `PublicOnly` a host on the loopback interface is refused, under `Any` (the private
    /// network) it resolves.
    #[tokio::test]
    async fn the_address_policy_applies_at_resolution() {
        let public = connector(AddressPolicy::PublicOnly);
        assert_eq!(
            public.resolve("127.0.0.1", 25).await,
            Err(ResolveError::NotPublic("127.0.0.1".to_owned()))
        );
        let private = connector(AddressPolicy::Any);
        assert_eq!(
            private
                .resolve("127.0.0.1", 25)
                .await
                .map(|address| address.port()),
            Ok(25)
        );
    }
}
