//! An optional recipient probe against a domain's public MX hosts, with no message sent.
//!
//! This is separate from authenticated submission: it sends `EHLO`, opportunistic `STARTTLS`,
//! `MAIL FROM`, one `RCPT TO`, then `QUIT`, never `AUTH` or `DATA`. DNS and every socket share
//! one deadline; at most three MX hosts are tried, in preference order. Hosts are fully
//! qualified and resolved through the public-only connector before connecting to their checked
//! IP, so an address supplied by a tenant cannot reach the private network.
//!
//! A `250`/`251` means only that this RCPT was accepted, including on catch-all domains. Only
//! an explicit `5.1.1` or `5.1.3` at RCPT is invalid (RFC 3463 §3.2); a generic `550`, policy
//! block, greylisting, sender rejection, TLS failure or timeout is unknown. The caller decides
//! whether to skip a message; a probe is never evidence of delivery or a bounce.

use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use lettre::Address;
use lettre::transport::smtp::Error;
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::commands::{Mail, Rcpt};
use lettre::transport::smtp::extension::{ClientId, Extension, MailParameter};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, timeout_at};

use crate::net::{AddressPolicy, Connector, ConnectorError};
use crate::smtp::{reply_text, response_parts};
use crate::status::EnhancedStatus;
use crate::text;

/// A probe's whole budget, including DNS and alternate MX hosts.
pub const BUDGET: Duration = Duration::from_secs(10);
const HOST_BUDGET: Duration = Duration::from_secs(3);
const MX_LIMIT: usize = 3;

/// The signed control protocol's maximum recipient batch, shared by the core and mail host.
pub const BATCH_MAX: usize = 8;

/// The control request. Only the authenticated mail host executes its recipient checks.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub emails: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchResponse {
    pub data: Vec<RecipientCheck>,
}

/// The control response preserves input order and echoes the address to prevent misattribution.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecipientCheck {
    pub email: String,
    pub status: Outcome,
    pub detail: String,
}

/// What the remote host said about this recipient, not whether a message would be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Invalid,
    Unknown,
}

/// Bounded facts from one check; no submission has occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub outcome: Outcome,
    pub code: Option<u16>,
    pub status: Option<EnhancedStatus>,
    pub diagnostic: String,
}

impl Finding {
    #[must_use]
    pub fn unknown(detail: &str) -> Self {
        Self {
            outcome: Outcome::Unknown,
            code: None,
            status: None,
            diagnostic: text::bounded(detail, text::DIAGNOSTIC_CHARS),
        }
    }
}

/// The deployment's probe client. It never inherits the development setting that lets
/// submission connections reach private hosts.
pub struct Verifier {
    resolver: TokioResolver,
    connector: Connector,
    hello: ClientId,
}

impl Verifier {
    /// `hello` is the operator's validated public hostname, sent in EHLO.
    ///
    /// # Errors
    ///
    /// The public-only connector's TLS configuration could not be built.
    pub fn new(resolver: TokioResolver, hello: String) -> Result<Self, ConnectorError> {
        Ok(Self {
            connector: Connector::new(resolver.clone(), AddressPolicy::PublicOnly)?,
            resolver,
            hello: ClientId::Domain(hello),
        })
    }

    /// Checks one recipient using the operator's probe From address. The caller already checked
    /// syntax and the DNS route. A deadline or any inconclusive answer stays unknown.
    pub async fn check(&self, from: &str, recipient: &str) -> Finding {
        let (Ok(from), Ok(recipient)) = (from.parse::<Address>(), recipient.parse::<Address>())
        else {
            return Finding::unknown("the probe envelope could not be parsed");
        };
        let deadline = Instant::now() + BUDGET;
        timeout_at(deadline, self.lookup(&from, &recipient, deadline))
            .await
            .unwrap_or_else(|_| Finding::unknown("the recipient probe deadline passed"))
    }

    async fn lookup(&self, from: &Address, recipient: &Address, deadline: Instant) -> Finding {
        let domain = format!("{}.", recipient.domain().trim_end_matches('.'));
        let hosts = match self.resolver.mx_lookup(domain.as_str()).await {
            Ok(lookup) => {
                let exchanges = lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        RData::MX(mx) => Some((mx.preference, mx.exchange.to_utf8())),
                        _ => None,
                    });
                match mail_hosts(&domain, exchanges) {
                    Some(hosts) => hosts,
                    None => return Finding::unknown("the domain publishes a null MX"),
                }
            }
            Err(error) if error.is_nx_domain() => {
                return Finding::unknown("the recipient domain no longer exists");
            }
            Err(error) if error.is_no_records_found() => vec![domain],
            Err(_) => return Finding::unknown("the recipient MX lookup did not answer"),
        };
        let mut finding = Finding::unknown("no public MX host answered the recipient probe");
        for host in hosts {
            let until = deadline.min(Instant::now() + HOST_BUDGET);
            finding = timeout_at(until, self.host(&host, 25, from, recipient))
                .await
                .unwrap_or_else(|_| {
                    Finding::unknown("the MX host did not answer before the deadline")
                });
            if finding.outcome != Outcome::Unknown || Instant::now() >= deadline {
                break;
            }
        }
        finding
    }

    async fn host(&self, host: &str, port: u16, from: &Address, recipient: &Address) -> Finding {
        let address = match self.connector.resolve(host, port).await {
            Ok(address) => address,
            Err(error) => return Finding::unknown(&error.to_string()),
        };
        let mut connection = match AsyncSmtpConnection::connect_tokio1(
            address,
            Some(HOST_BUDGET),
            &self.hello,
            None,
            None,
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => return inconclusive(&error),
        };
        let finding = self.envelope(&mut connection, host, from, recipient).await;
        // QUIT is best effort: losing its reply cannot change the RCPT answer.
        let _ = timeout_at(
            Instant::now() + Duration::from_millis(200),
            connection.quit(),
        )
        .await;
        finding
    }

    async fn envelope(
        &self,
        connection: &mut AsyncSmtpConnection,
        host: &str,
        from: &Address,
        recipient: &Address,
    ) -> Finding {
        if connection.can_starttls() {
            let Ok(tls) = TlsParameters::new(host.trim_end_matches('.').to_owned()) else {
                return Finding::unknown("the MX host's TLS configuration could not be built");
            };
            if let Err(error) = connection.starttls(tls, &self.hello).await {
                return inconclusive(&error);
            }
        }
        let mut parameters = Vec::new();
        if !from.to_string().is_ascii() || !recipient.to_string().is_ascii() {
            if !connection
                .server_info()
                .supports_feature(Extension::SmtpUtfEight)
            {
                return Finding::unknown("the MX host does not offer SMTPUTF8");
            }
            parameters.push(MailParameter::SmtpUtfEight);
        }
        if let Err(error) = connection
            .command(Mail::new(Some(from.clone()), parameters))
            .await
        {
            return inconclusive(&error);
        }
        match connection
            .command(Rcpt::new(recipient.clone(), Vec::new()))
            .await
        {
            Ok(response) => {
                let (code, text) = response_parts(&response);
                recipient_reply(code, &text)
            }
            Err(error) => match error.status() {
                Some(code) => recipient_reply(u16::from(code), &reply_text(&error)),
                None => inconclusive(&error),
            },
        }
    }
}

/// Follow the MX preferences and implicit-MX rule, but never guess past a null MX.
fn mail_hosts(domain: &str, exchanges: impl Iterator<Item = (u16, String)>) -> Option<Vec<String>> {
    let mut exchanges: Vec<_> = exchanges.collect();
    if exchanges.iter().any(|(_, host)| host == ".") {
        return None;
    }
    exchanges.sort();
    let mut hosts = Vec::new();
    for (_, host) in exchanges {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
        if hosts.len() == MX_LIMIT {
            break;
        }
    }
    if hosts.is_empty() {
        hosts.push(domain.to_owned());
    }
    Some(hosts)
}

fn recipient_reply(code: u16, reply: &str) -> Finding {
    // Enhanced status belongs at the beginning of an SMTP reply (RFC 2034). Do not search
    // arbitrary diagnostics: a policy refusal can quote another server's 5.1.1.
    let status = reply
        .split_whitespace()
        .next()
        .and_then(|word| word.parse::<EnhancedStatus>().ok());
    let invalid = (500..600).contains(&code)
        && status.is_some_and(|status| {
            status.class() == 5 && status.subject() == 1 && matches!(status.detail(), 1 | 3)
        });
    let outcome = if invalid {
        Outcome::Invalid
    } else if matches!(code, 250 | 251) {
        Outcome::Accepted
    } else {
        Outcome::Unknown
    };
    Finding {
        outcome,
        code: Some(code),
        status,
        diagnostic: text::bounded(&format!("{code} {reply}"), text::DIAGNOSTIC_CHARS),
    }
}

fn inconclusive(error: &Error) -> Finding {
    Finding {
        code: error.status().map(u16::from),
        ..Finding::unknown(&error.to_string())
    }
}

#[cfg(test)]
mod tests;
