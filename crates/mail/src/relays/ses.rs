//! Amazon SES's account API (SESv2, <https://docs.aws.amazon.com/ses/latest/APIReference-V2/>,
//! read 2026-10-02), with an access key the customer gives a connection besides its SMTP
//! credential:
//!
//! - `GET /v2/email/account` ([`account`]): whether sending is enabled in the Region, the
//!   account's reputation status (`HEALTHY`; `PROBATION`, under review while sending continues;
//!   `SHUTDOWN`, sending paused for the mail it sent) and whether it has production access or is
//!   still in the sandbox, where it may only mail verified identities.
//! - `GET /v2/email/identities/{identity}` ([`identity`]): whether an address or a domain is
//!   verified for sending (`VerifiedForSendingStatus`); `404` when the account holds no such
//!   identity, which for an address is not yet a refusal: SES also lets an address send when its
//!   domain is verified, so the caller asks for the domain next.
//!
//! The endpoint is `https://email.<region>.amazonaws.com`, every request signed for the service
//! `ses` ([`super::sigv4`]). SMTP credentials are per Region and so is everything here: a
//! connection's Region is its SMTP host's, `email-smtp.<region>.amazonaws.com` ([`region_of`]).

use jiff::Timestamp;
use reqwest::{Response, StatusCode};
use serde::Deserialize;
use tokio::time::Instant;
use url::Url;

use super::sigv4::{self, AccessKey, Scope};
use crate::http::{self, ApiError, HttpClient};

/// The name SES requests are signed for.
const SERVICE: &str = "ses";

/// What `GetAccount` says about the account in one Region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    /// Sending is enabled in the Region (`SendingEnabled`): `false` once AWS or the customer
    /// paused it.
    pub sending_enabled: bool,
    /// The account's reputation status (`EnforcementStatus`).
    pub enforcement: Enforcement,
    /// The account left the sandbox in this Region (`ProductionAccessEnabled`).
    pub production_access: bool,
}

/// An account's reputation status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// `HEALTHY`: no reputation issue affects the account.
    Healthy,
    /// `PROBATION`: AWS reviews the account while its issues are corrected; sending continues.
    Probation,
    /// `SHUTDOWN`: sending is paused because of the mail the account sent.
    Shutdown,
    /// A status this version does not know, or none.
    Unknown,
}

/// What `GetEmailIdentity` says about one identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The identity may be used to send (`VerifiedForSendingStatus`).
    pub verified_for_sending: bool,
}

/// The Region of an SES SMTP host (`email-smtp.us-east-1.amazonaws.com` gives `us-east-1`), or
/// `None` for any other host.
#[must_use]
pub fn region_of(smtp_host: &str) -> Option<String> {
    let host = smtp_host.trim().trim_end_matches('.').to_ascii_lowercase();
    let region = host
        .strip_prefix("email-smtp.")?
        .strip_suffix(".amazonaws.com")?;
    valid_region(region).then(|| region.to_owned())
}

/// The account's state in `region` (`GetAccount`).
///
/// # Errors
///
/// The call failed ([`ApiError`]): `403` when the key is unknown or may not read the account,
/// `429` when SES throttles the call, or a response that is not the account.
pub async fn account(
    http: &HttpClient,
    key: &AccessKey,
    region: &str,
    deadline: Instant,
) -> Result<Account, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Body {
        sending_enabled: bool,
        enforcement_status: Option<String>,
        production_access_enabled: bool,
    }
    let url = endpoint(region, &["v2", "email", "account"])?;
    let body: Body = http::json(get(http, key, region, url, deadline).await?).await?;
    Ok(Account {
        sending_enabled: body.sending_enabled,
        enforcement: match body.enforcement_status.as_deref() {
            Some("HEALTHY") => Enforcement::Healthy,
            Some("PROBATION") => Enforcement::Probation,
            Some("SHUTDOWN") => Enforcement::Shutdown,
            _ => Enforcement::Unknown,
        },
        production_access: body.production_access_enabled,
    })
}

/// The identity `identity` (an address or a domain) in `region` (`GetEmailIdentity`); `None`
/// when the account holds no such identity.
///
/// # Errors
///
/// The call failed ([`ApiError`]), or the response is not an identity.
pub async fn identity(
    http: &HttpClient,
    key: &AccessKey,
    region: &str,
    identity: &str,
    deadline: Instant,
) -> Result<Option<Identity>, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Body {
        verified_for_sending_status: bool,
    }
    if identity.is_empty() || identity.len() > 320 {
        return Err(ApiError::InvalidResponse(
            "an SES identity is an address or a domain".to_owned(),
        ));
    }
    let url = endpoint(region, &["v2", "email", "identities", identity])?;
    let response = get(http, key, region, url, deadline).await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body: Body = http::json(response).await?;
    Ok(Some(Identity {
        verified_for_sending: body.verified_for_sending_status,
    }))
}

/// The URL of `segments` on the Region's endpoint, each segment sent URI-encoded as AWS encodes
/// it (an address's `@` as `%40`), so the signature covers exactly the path that is sent.
fn endpoint(region: &str, segments: &[&str]) -> Result<Url, ApiError> {
    if !valid_region(region) {
        return Err(ApiError::InvalidResponse(format!(
            "`{region}` is not an AWS Region"
        )));
    }
    let path = segments
        .iter()
        .map(|segment| sigv4::uri_encode(segment))
        .collect::<Vec<_>>()
        .join("/");
    Url::parse(&format!("https://email.{region}.amazonaws.com/{path}"))
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))
}

/// A signed `GET` of `url`.
async fn get(
    http: &HttpClient,
    key: &AccessKey,
    region: &str,
    url: Url,
    deadline: Instant,
) -> Result<Response, ApiError> {
    let signed = sigv4::sign(
        key,
        Scope {
            region,
            service: SERVICE,
        },
        "GET",
        &url,
        &[],
        b"",
        Timestamp::now(),
    );
    http::send(
        http.get(url)
            .header("x-amz-date", signed.amz_date)
            .header("authorization", signed.authorization),
        deadline,
    )
    .await
}

/// A Region's name as AWS writes them (`us-east-1`, `eu-central-2`): it becomes part of a host
/// name, so nothing else is accepted.
fn valid_region(region: &str) -> bool {
    (1..=32).contains(&region.len())
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
