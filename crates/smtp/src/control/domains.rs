//! Domains: the sending domains the MTA signs for.
//!
//! A domain is registered with an ownership token and its public DKIM key is prepared.
//! SMTP and IMAP accounts remain unavailable until the
//! TXT record `_norbelys.<domain>` in public DNS carries that token: the value the core already
//! asks the domain's owner to publish, when the core names it (so one record proves the domain
//! to both), else a random one. That check gates every login, so nobody can send as a domain
//! they do not control. Once verified, a domain stays verified (removing the record later
//! changes nothing). Preparing the key grants no account access; the provisioning helper
//! records only the public half when the key exists.
//!
//! Customer zones are never written by us: the records are rendered for the customer to
//! publish, each with a note on merging it with what the zone already holds. They are what
//! the large mailbox providers require of bulk senders: SPF authorising the MTA's address
//! (RFC 7208), a DKIM key (RFC 6376), a DMARC policy of at least `p=none` (RFC 7489), and an MX
//! only when replies should come back through the MTA.

use crate::db::{Connection, OptionalExtension as _, Row, params};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State as Extract};
use axum::http::StatusCode;
use hickory_resolver::proto::rr::RData;
use serde::{Deserialize, Serialize};

use super::{ApiError, List, Settings, State, is_domain, parse, trigger};
use crate::crypto;
use crate::db;
use crate::provision::{self, Change, DkimKey};

/// `POST /v1/domains`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDomain {
    /// The lowercase domain name.
    pub name: String,
    /// The value the `_norbelys.<name>` TXT record must hold (1 to 255 visible ASCII
    /// characters), when the core names the one it already asks the owner to publish. Until the
    /// domain is verified, registering it again with another value replaces the value (the core
    /// proves ownership itself before it registers); once verified, the value no longer matters
    /// and is kept.
    #[serde(default)]
    pub ownership_token: Option<String>,
}

/// A domain as stored.
#[derive(Debug, Serialize)]
pub struct Domain {
    /// The domain name.
    pub name: String,
    /// The value of the `_norbelys.<name>` TXT record.
    pub ownership_token: String,
    /// When the ownership record was found.
    pub verified_at: Option<String>,
    /// The DKIM selector.
    pub dkim_selector: String,
    /// The DKIM public key (base64), once the helper created or adopted the key.
    pub dkim_public: Option<String>,
    /// When the domain was registered.
    pub created_at: String,
}

/// A domain with the records to publish.
#[derive(Debug, Serialize)]
pub struct Rendered {
    /// The domain.
    #[serde(flatten)]
    pub domain: Domain,
    /// The DNS records.
    pub records: Vec<Record>,
}

/// One DNS record to publish.
#[derive(Debug, Serialize)]
pub struct Record {
    /// `TXT` or `MX`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The owner name.
    pub name: String,
    /// The record's value.
    pub content: String,
    /// The MX preference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u16>,
    /// How to publish it alongside existing records.
    pub note: &'static str,
}

const COLUMNS: &str = "name, ownership_token, verified_at, dkim_selector, dkim_public, created_at";

fn from_row(row: &Row) -> db::Result<Domain> {
    Ok(Domain {
        name: row.get(0)?,
        ownership_token: row.get(1)?,
        verified_at: row.get(2)?,
        dkim_selector: row.get(3)?,
        dkim_public: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn load(conn: &Connection, name: &str) -> db::Result<Option<Domain>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM domains WHERE name = ?1"),
        [name],
        from_row,
    )
    .optional()
}

/// The records to publish: ownership, SPF, DMARC, MX and, once the key exists, DKIM.
#[must_use]
pub fn render(settings: &Settings, domain: Domain) -> Rendered {
    let name = &domain.name;
    let mut records = vec![
        Record {
            kind: "TXT",
            name: format!("_norbelys.{name}"),
            content: domain.ownership_token.clone(),
            priority: None,
            note: "Proves ownership; keep it while the domain sends through the managed MTA",
        },
        Record {
            kind: "TXT",
            name: name.clone(),
            content: settings.spf_include.as_ref().map_or_else(
                || format!("v=spf1 ip4:{} ~all", settings.public_ipv4),
                |name| format!("v=spf1 include:{name} ~all"),
            ),
            priority: None,
            note: "Merge the managed sender mechanism into an existing SPF record: a domain has one SPF record",
        },
        Record {
            kind: "TXT",
            name: format!("_dmarc.{name}"),
            content: "v=DMARC1; p=none".to_owned(),
            priority: None,
            note: "Keep an existing DMARC policy; p=none is the minimum the receivers require",
        },
        Record {
            kind: "MX",
            name: name.clone(),
            content: settings.mail_host.clone(),
            priority: Some(10),
            note: "Only when replies are received through the managed MTA; keep an existing MX otherwise",
        },
    ];
    if let Some(public) = &domain.dkim_public {
        records.push(Record {
            kind: "TXT",
            name: format!("{}._domainkey.{name}", domain.dkim_selector),
            content: format!("v=DKIM1; k=rsa; p={public}"),
            priority: None,
            note: "Publish as one TXT value; DNS providers split values longer than 255 characters themselves",
        });
    }
    Rendered { domain, records }
}

/// `POST /v1/domains`: registers a domain (`201`), or returns the registered one (`200`), its
/// ownership value replaced by a given one while it is not verified.
///
/// # Errors
///
/// The name is not a lowercase fully qualified domain name, or the ownership value is not 1 to
/// 255 visible ASCII characters.
pub async fn create(
    Extract(state): Extract<State>,
    body: Bytes,
) -> Result<(StatusCode, Json<Rendered>), ApiError> {
    let CreateDomain {
        name,
        ownership_token,
    } = parse(&body)?;
    if !is_domain(&name) {
        return Err(ApiError::Invalid(
            "name must be a lowercase fully qualified domain name".to_owned(),
        ));
    }
    if ownership_token.as_deref().is_some_and(|token| {
        !(1..=255).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic())
    }) {
        return Err(ApiError::Invalid(
            "ownership_token must be 1 to 255 visible ASCII characters".to_owned(),
        ));
    }
    let given = ownership_token.is_some();
    let token = match ownership_token {
        Some(token) => token,
        None => format!("norbelys-{}", crypto::random_token(24)?),
    };
    let selector = state.settings.dkim_selector.clone();
    let (created, domain, queued) = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let existing = load(&tx, &name)?;
            match &existing {
                None => {
                    tx.execute(
                        "INSERT INTO domains (name, ownership_token, dkim_selector, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![name, token, selector, db::now_text()],
                    )?;
                }
                Some(domain)
                    if given && domain.verified_at.is_none() && domain.ownership_token != token =>
                {
                    tx.execute(
                        "UPDATE domains SET ownership_token = ?1 WHERE name = ?2",
                        params![token, name],
                    )?;
                }
                Some(_) => {}
            }
            let domain = load(&tx, &name)?
                .ok_or_else(|| ApiError::Internal("a registered domain vanished".to_owned()))?;
            let queued = domain.dkim_public.is_none() && !provision::dkim_pending(&tx, &name)?;
            if queued {
                provision::enqueue(&tx, &Change::DkimCreate(DkimKey {domain:name.clone(),selector:domain.dkim_selector.clone()}))?;
            }
            tx.commit()?;
            Ok::<_, ApiError>((existing.is_none(), domain, queued))
        })
        .await?;
    if queued {
        trigger(&state);
    }
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(render(&state.settings, domain))))
}

/// `GET /v1/domains`.
///
/// # Errors
///
/// The database fails.
pub async fn list(Extract(state): Extract<State>) -> Result<Json<List<Rendered>>, ApiError> {
    let domains = state
        .db
        .call(|conn| {
            let stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM domains ORDER BY name"))?;
            let rows = stmt.query_map([], from_row)?;
            rows.collect::<db::Result<Vec<_>>>().map_err(ApiError::from)
        })
        .await?;
    let data = domains
        .into_iter()
        .map(|d| render(&state.settings, d))
        .collect();
    Ok(Json(List { data }))
}

/// `GET /v1/domains/{name}`.
///
/// # Errors
///
/// The domain is not registered.
pub async fn retrieve(
    Extract(state): Extract<State>,
    Path(name): Path<String>,
) -> Result<Json<Rendered>, ApiError> {
    let domain = find(&state, name).await?;
    Ok(Json(render(&state.settings, domain)))
}

async fn find(state: &State, name: String) -> Result<Domain, ApiError> {
    state
        .db
        .call(move |conn| {
            load(conn, &name)?.ok_or_else(|| ApiError::NotFound(format!("no domain {name}")))
        })
        .await
}

/// `POST /v1/domains/{name}/verify`: reads `_norbelys.<name>` from public DNS; when it holds
/// the token, the domain is verified. Any DKIM preparation missing from registration is queued.
///
/// # Errors
///
/// The domain is not registered (`404`), the record does not hold the token (`409`), or DNS
/// cannot answer (`503`).
pub async fn verify(
    Extract(state): Extract<State>,
    Path(name): Path<String>,
) -> Result<Json<Rendered>, ApiError> {
    let domain = find(&state, name.clone()).await?;
    if domain.verified_at.is_none() {
        let fqdn = format!("_norbelys.{name}.");
        let found = match state.resolver.txt_lookup(fqdn.as_str()).await {
            Ok(lookup) => lookup.answers().iter().any(|record| match &record.data {
                RData::TXT(txt) => txt.txt_data.concat() == domain.ownership_token.as_bytes(),
                _ => false,
            }),
            Err(error) if error.is_no_records_found() || error.is_nx_domain() => false,
            Err(error) => {
                tracing::warn!(error = %error, domain = %name, "ownership lookup failed");
                return Err(ApiError::Unavailable(format!(
                    "the DNS lookup of {fqdn} failed; try again"
                )));
            }
        };
        if !found {
            return Err(ApiError::Conflict(format!(
                "the TXT record _norbelys.{name} does not hold the ownership token yet"
            )));
        }
    }
    let (domain, queued) = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE domains SET verified_at = COALESCE(verified_at, ?1) WHERE name = ?2",
                params![db::now_text(), name],
            )?;
            let domain =
                load(&tx, &name)?.ok_or_else(|| ApiError::NotFound(format!("no domain {name}")))?;
            let queued = domain.dkim_public.is_none() && !provision::dkim_pending(&tx, &name)?;
            if queued {
                provision::enqueue(
                    &tx,
                    &Change::DkimCreate(DkimKey {
                        domain: name.clone(),
                        selector: domain.dkim_selector.clone(),
                    }),
                )?;
            }
            tx.commit()?;
            Ok::<_, ApiError>((domain, queued))
        })
        .await?;
    if queued {
        trigger(&state);
    }
    Ok(Json(render(&state.settings, domain)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, call, router, signed, verify_domain};

    fn domain(dkim_public: Option<&str>) -> Domain {
        Domain {
            name: "example.com".to_owned(),
            ownership_token: "norbelys-token".to_owned(),
            verified_at: None,
            dkim_selector: "norbelys".to_owned(),
            dkim_public: dkim_public.map(str::to_owned),
            created_at: "2026-10-01T00:00:00Z".to_owned(),
        }
    }

    /// The records a customer publishes: the ownership token, SPF with the MTA's address,
    /// DMARC `p=none`, an MX to the MTA, and the DKIM key only once it exists.
    #[test]
    fn renders_the_records_to_publish() {
        let dir = TempDir::new();
        let settings = crate::testing::state(&dir).settings;
        let records = |public| {
            render(&settings, domain(public))
                .records
                .iter()
                .map(|r| (r.kind, r.name.clone(), r.content.clone(), r.priority))
                .collect::<Vec<_>>()
        };
        let base = vec![
            (
                "TXT",
                "_norbelys.example.com".to_owned(),
                "norbelys-token".to_owned(),
                None,
            ),
            (
                "TXT",
                "example.com".to_owned(),
                "v=spf1 ip4:192.0.2.10 ~all".to_owned(),
                None,
            ),
            (
                "TXT",
                "_dmarc.example.com".to_owned(),
                "v=DMARC1; p=none".to_owned(),
                None,
            ),
            (
                "MX",
                "example.com".to_owned(),
                "mail.example.com".to_owned(),
                Some(10),
            ),
        ];
        assert_eq!(records(None), base);
        let mut with_key = base;
        with_key.push((
            "TXT",
            "norbelys._domainkey.example.com".to_owned(),
            "v=DKIM1; k=rsa; p=MIIB".to_owned(),
            None,
        ));
        assert_eq!(records(Some("MIIB")), with_key);
    }

    /// Registering a domain twice answers the same domain and token (`201`, then `200`), so
    /// the core can retry its job without minting a second token.
    #[tokio::test]
    async fn registration_is_idempotent() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        let body = r#"{"name":"example.com"}"#;
        let (created, first) = call(&app, signed("POST", "/v1/domains", body)).await;
        let (again, second) = call(&app, signed("POST", "/v1/domains", body)).await;
        assert_eq!((created, again), (StatusCode::CREATED, StatusCode::OK));
        assert_eq!(first["ownership_token"], second["ownership_token"]);
        let (invalid, _) = call(
            &app,
            signed("POST", "/v1/domains", r#"{"name":"Example.com"}"#),
        )
        .await;
        assert_eq!(invalid, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// The core may name the ownership value its customer already publishes, so one TXT record
    /// proves the domain to both: it is stored at registration and rendered as the ownership
    /// record, replaced while the domain is not verified, and kept once it is; a value that is
    /// not visible ASCII is refused.
    #[tokio::test]
    async fn registration_takes_the_cores_ownership_value() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        let register = |token: &str| {
            signed(
                "POST",
                "/v1/domains",
                &format!(r#"{{"name":"example.com","ownership_token":"{token}"}}"#),
            )
        };
        let (status, first) = call(&app, register("norbelys-verification=a1")).await;
        assert_eq!(
            (status, first["ownership_token"].as_str()),
            (StatusCode::CREATED, Some("norbelys-verification=a1"))
        );
        assert_eq!(first["records"][0]["content"], "norbelys-verification=a1");
        let (_, replaced) = call(&app, register("norbelys-verification=b2")).await;
        assert_eq!(replaced["ownership_token"], "norbelys-verification=b2");
        verify_domain(&dir, "example.com");
        let (status, kept) = call(&app, register("norbelys-verification=c3")).await;
        assert_eq!(
            (status, kept["ownership_token"].as_str()),
            (StatusCode::OK, Some("norbelys-verification=b2"))
        );
        let (invalid, _) = call(&app, register("has space")).await;
        assert_eq!(invalid, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Verifying a verified domain queues its DKIM key exactly once, however often the core
    /// asks, and the key is not queued again while one is pending.
    #[tokio::test]
    async fn verification_queues_the_dkim_key_once() {
        let dir = TempDir::new();
        let (app, state) = router(&dir);
        call(
            &app,
            signed("POST", "/v1/domains", r#"{"name":"example.com"}"#),
        )
        .await;
        verify_domain(&dir, "example.com");
        for _ in 0..2 {
            let (status, _) =
                call(&app, signed("POST", "/v1/domains/example.com/verify", "")).await;
            assert_eq!(status, StatusCode::OK);
        }
        let queued: i64 = state
            .db
            .call(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM pending_changes WHERE kind = 'dkim.create'",
                    [],
                    |r| r.get(0),
                )
                .map_err(ApiError::from)
            })
            .await
            .unwrap();
        assert_eq!(queued, 1);
        assert!(dir.join("provision.trigger").exists());
    }
}
