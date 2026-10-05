//! Preflight: the checks on an address before mail is submitted to it, never a mailbox probe.
//!
//! For each address: its syntax ([`EmailAddress::parse`]), the routing of its domain in DNS
//! (MX, else A/AAAA, a null MX refusing all mail; the decision table is
//! `domain::preflight`), and what the workspace itself knows about it: a suppression (no mail
//! is sent to it, ever) and an active hold (mail waits until the hold is lifted or its re-check
//! time passes, exactly as the sender's Start reads it). Nothing is sent to the address. The
//! sender reads an address's route through [`routes`], which keeps what DNS answered in the
//! preflight cache; `POST /preflight` ([`check`]) reads the same cache and writes nothing.
//!
//! A workspace in test mode checks the syntax only, here as in the sender: its mail never leaves
//! the fake transport, and its developers write to domains that accept no mail
//! (`example.com`), which a DNS check would report unroutable. It neither reads nor writes the
//! cache.
//!
//! # The cache
//!
//! `recipient_validations` keeps, per workspace and address, the verdict DNS answered for its
//! domain until the verdict's expiry (`domain::preflight::Reason::ttl`: a day). Both callers
//! read the unexpired verdicts first and ask DNS only for the addresses without one; the sender
//! then stores those answers, in key order so two writers never wait on each other in a cycle.
//! A lookup DNS could not answer is returned as `dns_unavailable` and never stored, so the next
//! check asks again; a null MX is stored like any answer and re-checked once it expires, the day
//! a `no_route` hold waits. The cache is read in one short transaction and written in another,
//! with no transaction open while DNS is asked; `retention.prune` deletes expired rows.
//!
//! # DNS
//!
//! Lookups go through the process's one resolver ([`crate::dns`]), with its cache and bounds.
//! A batch looks up each distinct domain once, eight at a time, within a 10-second budget: a
//! domain not answered by then is `unknown`, as is any lookup that times out or fails, so a
//! slow or broken resolver delays nothing and refuses nobody.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::pin::pin;
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::stream;
use hickory_resolver::lookup::Lookup;
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::net::NetError;
use hickory_resolver::proto::rr::RData;
use serde::Serialize;

use crate::db::Database;
use crate::dns::Resolver;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Suppression, WorkspaceId};
use crate::domain::preflight::{self as decide, Addresses, Mx, Reason, Step};
use crate::domain::time::Timestamp;

/// Addresses one request checks.
pub const ADDRESSES_MAX: usize = 100;
/// Lookups in flight at once for one batch.
const CONCURRENCY: usize = 8;
/// The DNS budget of one batch.
const BUDGET: Duration = Duration::from_secs(10);
/// The routing of one domain: its MX records, and its address records when it has none.
pub async fn route(resolver: &Resolver, domain: &str) -> Reason {
    match decide::after_mx(classify_mx(resolver.mx(domain).await)) {
        Step::Decided(reason) => reason,
        Step::LookUpAddresses => {
            decide::after_addresses(classify_addresses(resolver.addresses(domain).await))
        }
    }
}

/// The routing of each domain, looked up eight at a time within the batch's budget; a domain
/// without an answer by then is [`Reason::DnsUnavailable`].
pub async fn route_all(resolver: &Resolver, domains: BTreeSet<String>) -> HashMap<String, Reason> {
    let mut routes: HashMap<String, Reason> = domains
        .iter()
        .map(|domain| (domain.clone(), Reason::DnsUnavailable))
        .collect();
    let mut lookups = pin!(
        stream::iter(domains)
            .map(|domain| async move {
                let reason = route(resolver, &domain).await;
                (domain, reason)
            })
            .buffer_unordered(CONCURRENCY)
    );
    let deadline = tokio::time::Instant::now() + BUDGET;
    while let Ok(Some((domain, reason))) = tokio::time::timeout_at(deadline, lookups.next()).await {
        routes.insert(domain, reason);
    }
    routes
}

/// The sender's read of the mail route of each address of `workspace`, by its key
/// ([`EmailAddress::key`]): its unexpired verdict in the preflight cache, else its domain's
/// answer from DNS ([`route_all`]), which is then cached for its time ([`Reason::ttl`]); see the
/// module. Commits the cache's new rows, and nothing else.
///
/// # Errors
///
/// The database is unavailable: the cache could not be read or written.
pub async fn routes(
    db: &Database,
    resolver: &Resolver,
    workspace: WorkspaceId,
    addresses: &[EmailAddress],
) -> Result<HashMap<String, Reason>, sqlx::Error> {
    let (mut routes, fresh) = look_up(db, resolver, workspace, addresses).await?;
    remember(db, workspace, &fresh).await?;
    routes.extend(fresh);
    Ok(routes)
}

/// The route of each address by its key, in two parts: the unexpired verdicts the preflight
/// cache holds, and, for the other addresses, what DNS answered now (each once, in key order).
/// A cached reason this build does not know counts as no verdict. Writes nothing.
///
/// # Errors
///
/// The database is unavailable: the cache could not be read.
async fn look_up(
    db: &Database,
    resolver: &Resolver,
    workspace: WorkspaceId,
    addresses: &[EmailAddress],
) -> Result<(HashMap<String, Reason>, BTreeMap<String, Reason>), sqlx::Error> {
    if addresses.is_empty() {
        return Ok((HashMap::new(), BTreeMap::new()));
    }
    let keys: Vec<String> = addresses.iter().map(EmailAddress::key).collect();
    let mut tx = db.begin_in(workspace).await?;
    let cached: HashMap<String, Reason> = sqlx::query!(
        "SELECT email_key, reason FROM recipient_validations
          WHERE workspace_id = $1 AND email_key = ANY($2) AND expires_at > now()",
        workspace.uuid(),
        &keys,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .filter_map(|row| Some((row.email_key, row.reason.parse::<Reason>().ok()?)))
    .collect();
    tx.commit().await?;

    // The addresses without a verdict, each once, in key order.
    let missing: BTreeMap<String, String> = addresses
        .iter()
        .map(|address| (address.key(), address.domain()))
        .filter(|(key, _)| !cached.contains_key(key))
        .collect();
    if missing.is_empty() {
        return Ok((cached, BTreeMap::new()));
    }
    let answers = route_all(resolver, missing.values().cloned().collect()).await;
    let fresh: BTreeMap<String, Reason> = missing
        .into_iter()
        .map(|(key, domain)| {
            let reason = answers
                .get(&domain)
                .copied()
                .unwrap_or(Reason::DnsUnavailable);
            (key, reason)
        })
        .collect();
    Ok((cached, fresh))
}

/// Stores `verdicts` (by address key) in `workspace`'s preflight cache, each for its time
/// ([`Reason::ttl`]), replacing what an address had; a verdict that is never cached (a failed
/// lookup) is left out. Written in key order, so two writers never wait on each other in a
/// cycle. Commits its own transaction when there is anything to store.
///
/// # Errors
///
/// The database is unavailable.
async fn remember(
    db: &Database,
    workspace: WorkspaceId,
    verdicts: &BTreeMap<String, Reason>,
) -> Result<(), sqlx::Error> {
    let checked_at = crate::process::now();
    let (mut cached, mut statuses, mut reasons, mut expiries) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (key, reason) in verdicts {
        if let Some(ttl) = reason.ttl() {
            cached.push(key.clone());
            statuses.push(reason.status().as_str());
            reasons.push(reason.as_str());
            expiries.push(checked_at.plus(ttl));
        }
    }
    if !cached.is_empty() {
        let mut tx = db.begin_in(workspace).await?;
        sqlx::query!(
            "INSERT INTO recipient_validations (workspace_id, email_key, status, reason, checked_at, expires_at)
             SELECT $1, v.email_key, v.status, v.reason, $5::timestamptz, v.expires_at
               FROM unnest($2::text[], $3::text[], $4::text[], $6::timestamptz[])
                    AS v(email_key, status, reason, expires_at)
             ON CONFLICT (workspace_id, email_key) DO UPDATE
                SET status = EXCLUDED.status, reason = EXCLUDED.reason,
                    checked_at = EXCLUDED.checked_at, expires_at = EXCLUDED.expires_at",
            workspace.uuid(),
            &cached,
            &statuses as _,
            &reasons as _,
            checked_at as _,
            &expiries as _,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// What an MX lookup answered: a null MX (RFC 7505: the root as the exchange) wherever it
/// appears, since a domain that publishes one must publish nothing else and a sender must not
/// guess past a broken record set.
#[must_use]
pub fn classify_mx(answer: Result<Lookup, NetError>) -> Mx {
    match answer {
        Ok(lookup) => {
            let exchanges: Vec<bool> = lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    RData::MX(mx) => Some(mx.exchange.is_root()),
                    _ => None,
                })
                .collect();
            if exchanges.iter().any(|root| *root) {
                Mx::Null
            } else if exchanges.is_empty() {
                Mx::Empty
            } else {
                Mx::Exchanges
            }
        }
        Err(error) if error.is_nx_domain() => Mx::NoDomain,
        Err(error) if error.is_no_records_found() => Mx::Empty,
        Err(_) => Mx::Failed,
    }
}

/// What an address lookup (A and AAAA) answered.
#[must_use]
pub fn classify_addresses(answer: Result<LookupIp, NetError>) -> Addresses {
    match answer {
        Ok(addresses) if addresses.iter().next().is_some() => Addresses::Found,
        Ok(_) => Addresses::Empty,
        Err(error) if error.is_nx_domain() => Addresses::NoDomain,
        Err(error) if error.is_no_records_found() => Addresses::Empty,
        Err(_) => Addresses::Failed,
    }
}

/// What preflight found for one address.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Finding {
    /// The address as given, trimmed.
    pub email: String,
    /// `routable` (mail has a route), `invalid` (no mail can reach it) or `unknown` (DNS did not
    /// answer; check again later). New values may be added.
    #[schema(value_type = decide::Status)]
    pub status: &'static str,
    /// Why: `mx`, `implicit_mx` (no MX record, the domain is its own mail host), `syntax`,
    /// `no_domain`, `null_mx` (the domain accepts no mail), `no_route` or `dns_unavailable`.
    /// New values may be added.
    #[schema(value_type = Reason)]
    pub reason: &'static str,
    /// What is wrong with the address, when `reason` is `syntax`.
    pub detail: Option<String>,
    /// The workspace's suppression of the address: no mail is ever sent to it.
    pub suppression: Option<SuppressionSummary>,
    /// The address's active hold: mail to it waits until the hold is lifted.
    pub hold: Option<HoldSummary>,
}

/// A suppression, as preflight shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SuppressionSummary {
    pub id: Id<Suppression>,
    /// Why the address is suppressed. New values may be added.
    #[schema(value_type = crate::domain::suppressions::Reason)]
    pub reason: String,
}

/// An active hold, as preflight shows it: the most recent of the address's holds.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct HoldSummary {
    /// Why the address is held; `invalid_recipient` is a reported invalid address waiting for a
    /// person's review. New values may be added.
    #[schema(value_type = crate::domain::policy::delivery::HoldReason)]
    pub reason: String,
    /// When the hold is checked again.
    pub review_after: Timestamp,
}

/// Checks `emails` for `workspace`, in order (see the module), and writes nothing. Routes come
/// from the preflight cache or DNS, as the sender reads them, but what DNS answers here is not
/// stored; then the workspace's suppressions and holds are read, in one short transaction of its
/// own, so no transaction is open while the network is. A workspace in `test_mode` asks DNS
/// nothing and leaves the cache alone: its mail never leaves the fake transport, so a
/// well-formed address is routable, as the sender judges it there.
///
/// # Errors
///
/// The database is unavailable.
pub async fn check(
    db: &Database,
    resolver: &Resolver,
    workspace: WorkspaceId,
    emails: &[String],
    test_mode: bool,
) -> Result<Vec<Finding>, sqlx::Error> {
    let parsed: Vec<(String, Result<EmailAddress, String>)> = emails
        .iter()
        .map(|email| {
            let email = email.trim().to_owned();
            let address = EmailAddress::parse(&email).map_err(|error| error.to_string());
            (email, address)
        })
        .collect();
    let addresses: Vec<EmailAddress> = parsed
        .iter()
        .filter_map(|(_, address)| address.as_ref().ok().cloned())
        .collect();
    // The route of each well-formed address, by its key.
    let found: HashMap<String, Reason> = if test_mode {
        addresses
            .iter()
            .map(|address| (address.key(), Reason::Mx))
            .collect()
    } else {
        let (mut found, fresh) = look_up(db, resolver, workspace, &addresses).await?;
        found.extend(fresh);
        found
    };
    let keys: Vec<String> = addresses.iter().map(EmailAddress::key).collect();

    let mut tx = db.begin_in(workspace).await?;
    let suppressions: HashMap<String, SuppressionSummary> = sqlx::query!(
        r#"SELECT id AS "id: Id<Suppression>", email_key, reason FROM suppressions
            WHERE workspace_id = $1 AND email_key = ANY($2)"#,
        workspace.uuid(),
        &keys,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.email_key,
            SuppressionSummary {
                id: row.id,
                reason: row.reason,
            },
        )
    })
    .collect();
    let holds: HashMap<String, HoldSummary> = sqlx::query!(
        r#"SELECT DISTINCT ON (email_key) email_key, reason, review_after AS "review_after: Timestamp"
             FROM recipient_holds
            WHERE workspace_id = $1 AND email_key = ANY($2) AND resolved_at IS NULL AND review_after > now()
            ORDER BY email_key, observed_at DESC"#,
        workspace.uuid(),
        &keys,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.email_key,
            HoldSummary {
                reason: row.reason,
                review_after: row.review_after,
            },
        )
    })
    .collect();
    tx.commit().await?;

    Ok(parsed
        .into_iter()
        .map(|(email, address)| match address {
            Err(detail) => Finding {
                email,
                status: Reason::Syntax.status().as_str(),
                reason: Reason::Syntax.as_str(),
                detail: Some(detail),
                suppression: None,
                hold: None,
            },
            Ok(address) => {
                let key = address.key();
                let reason = found.get(&key).copied().unwrap_or(Reason::DnsUnavailable);
                Finding {
                    email,
                    status: reason.status().as_str(),
                    reason: reason.as_str(),
                    detail: None,
                    suppression: suppressions.get(&key).cloned(),
                    hold: holds.get(&key).cloned(),
                }
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hickory_resolver::lookup::Lookup;
    use hickory_resolver::net::{DnsError, NetError, NoRecords};
    use hickory_resolver::proto::op::{Query, ResponseCode};
    use hickory_resolver::proto::rr::rdata::MX;
    use hickory_resolver::proto::rr::{Name, RData, RecordType};

    use super::{check, classify_mx, remember, routes};
    use crate::dns::Resolver;
    use crate::domain::email::EmailAddress;
    use crate::domain::ids::WorkspaceId;
    use crate::domain::preflight::{Mx, Reason};
    use crate::testing::TestDb;

    fn address(email: &str) -> EmailAddress {
        EmailAddress::parse(email).unwrap()
    }

    /// Caches `reason` for `email_key` in `workspace`, written through the system login, expiring
    /// `expires_in` from now (a negative interval: already expired).
    async fn cached(
        test: &TestDb,
        workspace: WorkspaceId,
        email_key: &str,
        reason: Reason,
        expires_in: &str,
    ) {
        sqlx::query(
            "INSERT INTO recipient_validations (workspace_id, email_key, status, reason, checked_at, expires_at)
             VALUES ($1, $2, $3, $4, now(), now() + $5::interval)",
        )
        .bind(workspace.uuid())
        .bind(email_key)
        .bind(reason.status().as_str())
        .bind(reason.as_str())
        .bind(expires_in)
        .execute(test.system.pool())
        .await
        .unwrap();
    }

    /// The cache's rows of `workspace`: key, status, reason, and the seconds from now to expiry.
    async fn rows(test: &TestDb, workspace: WorkspaceId) -> Vec<(String, String, String, f64)> {
        sqlx::query_as(
            "SELECT email_key, status, reason,
                    extract(epoch FROM expires_at - now())::float8
               FROM recipient_validations WHERE workspace_id = $1 ORDER BY email_key",
        )
        .bind(workspace.uuid())
        .fetch_all(test.system.pool())
        .await
        .unwrap()
    }

    /// An unexpired verdict in the cache answers without DNS, for the sender's login and through
    /// `POST /preflight`'s check alike, whatever the case the address is written in: the
    /// resolver here has no name servers, so any lookup would have answered `dns_unavailable`.
    #[tokio::test]
    async fn a_cached_verdict_answers_without_dns() {
        let test = TestDb::new().await;
        let workspace = test.workspace("acme").await.id;
        cached(&test, workspace, "ada@example.com", Reason::Mx, "1 hour").await;
        cached(
            &test,
            workspace,
            "null@example.org",
            Reason::NullMx,
            "1 hour",
        )
        .await;

        let found = routes(
            &test.worker,
            &Resolver::offline(),
            workspace,
            &[address("Ada@Example.com"), address("null@example.org")],
        )
        .await
        .unwrap();
        assert_eq!(found.get("ada@example.com"), Some(&Reason::Mx));
        assert_eq!(found.get("null@example.org"), Some(&Reason::NullMx));

        let findings = check(
            &test.app,
            &Resolver::offline(),
            workspace,
            &["Ada@Example.com".to_owned()],
            false,
        )
        .await
        .unwrap();
        assert_eq!((findings[0].status, findings[0].reason), ("routable", "mx"));
    }

    /// An expired verdict is not an answer: DNS is asked again (and, failing here, answers
    /// `dns_unavailable`). A lookup DNS could not answer is never cached, so the expired row is
    /// left as it was and an address seen for the first time gets no row: the next check asks
    /// DNS again instead of remembering an outage.
    #[tokio::test]
    async fn an_expired_verdict_is_checked_again_and_a_deferral_is_not_cached() {
        let test = TestDb::new().await;
        let workspace = test.workspace("acme").await.id;
        cached(&test, workspace, "ada@example.com", Reason::Mx, "-1 minute").await;

        let found = routes(
            &test.worker,
            &Resolver::offline(),
            workspace,
            &[address("ada@example.com"), address("grace@example.org")],
        )
        .await
        .unwrap();
        assert_eq!(found.get("ada@example.com"), Some(&Reason::DnsUnavailable));
        assert_eq!(
            found.get("grace@example.org"),
            Some(&Reason::DnsUnavailable)
        );
        let rows = rows(&test, workspace).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].0.as_str(), rows[0].2.as_str()),
            ("ada@example.com", "mx")
        );
        assert!(rows[0].3 < 0.0, "still expired: {rows:?}");
    }

    /// What DNS answered is stored for a day with its status, replacing an expired verdict, and a
    /// failed lookup among the answers is left out; the stored verdicts then answer the next
    /// check, a null MX included, until the day is over.
    #[tokio::test]
    async fn answers_are_cached_for_a_day_and_failures_are_left_out() {
        let test = TestDb::new().await;
        let workspace = test.workspace("acme").await.id;
        cached(&test, workspace, "ada@example.com", Reason::Mx, "-1 minute").await;

        let verdicts = BTreeMap::from([
            ("ada@example.com".to_owned(), Reason::NullMx),
            ("bob@example.net".to_owned(), Reason::ImplicitMx),
            ("grace@example.org".to_owned(), Reason::DnsUnavailable),
        ]);
        remember(&test.worker, workspace, &verdicts).await.unwrap();

        let rows = rows(&test, workspace).await;
        let stored: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|(key, status, reason, _)| (key.as_str(), status.as_str(), reason.as_str()))
            .collect();
        assert_eq!(
            stored,
            [
                ("ada@example.com", "invalid", "null_mx"),
                ("bob@example.net", "routable", "implicit_mx"),
            ]
        );
        // A second of slack above the day: the read measures from its own clock, which can trail
        // the write's by microseconds.
        for (_, _, _, expires_in) in &rows {
            assert!((86_000.0..=86_401.0).contains(expires_in), "{rows:?}");
        }

        let found = routes(
            &test.worker,
            &Resolver::offline(),
            workspace,
            &[address("ada@example.com"), address("bob@example.net")],
        )
        .await
        .unwrap();
        assert_eq!(found.get("ada@example.com"), Some(&Reason::NullMx));
        assert_eq!(found.get("bob@example.net"), Some(&Reason::ImplicitMx));
    }

    fn query() -> Query {
        Query::query(Name::from_ascii("example.com.").unwrap(), RecordType::MX)
    }

    /// An MX answer whose one record names `exchange`.
    fn answer(exchange: Name) -> Result<Lookup, NetError> {
        Ok(Lookup::from_rdata(query(), RData::MX(MX::new(0, exchange))))
    }

    fn no_records(code: ResponseCode) -> Result<Lookup, NetError> {
        Err(NetError::from(DnsError::NoRecordsFound(NoRecords::new(
            query(),
            code,
        ))))
    }

    /// DNS answers map to what preflight decides on, with the resolver's own types: a null MX
    /// (`0 .`, RFC 7505) is a refusal by the domain, `NXDOMAIN` a domain that does not exist,
    /// `NOERROR` without records an empty answer (the implicit MX case), and a timeout a failed
    /// lookup, never a refusal.
    #[test]
    fn dns_answers_map_to_what_preflight_decides_on() {
        assert_eq!(classify_mx(answer(Name::root())), Mx::Null);
        assert_eq!(
            classify_mx(answer(Name::from_ascii("mx.example.com.").unwrap())),
            Mx::Exchanges
        );
        assert_eq!(
            classify_mx(no_records(ResponseCode::NXDomain)),
            Mx::NoDomain
        );
        assert_eq!(classify_mx(no_records(ResponseCode::NoError)), Mx::Empty);
        assert_eq!(classify_mx(Err(NetError::Timeout)), Mx::Failed);
    }
}
