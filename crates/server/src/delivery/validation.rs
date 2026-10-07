//! Optional mailbox checks through the managed MTA's signed control API. Only the remote mail
//! host opens TCP/25; an OSS installation without that service keeps syntax/DNS checks alone.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, timeout_at};

use norbelys_mail::verify::{Outcome, RecipientCheck};

use crate::senders::provision::{Control, ControlError};

pub(crate) use norbelys_mail::verify::BATCH_MAX as BATCH;

/// Capacity is temporary: campaign jobs yield instead of persisting skipped checks.
#[derive(Debug)]
pub struct Busy;

/// What the optional SMTP check established; acceptance does not prove delivery or ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MailboxStatus {
    Accepted,
    Invalid,
    Unknown,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MailboxFinding {
    pub status: MailboxStatus,
    /// A bounded explanation of the SMTP result, or why this check was skipped.
    pub detail: String,
}

impl MailboxFinding {
    pub(crate) fn skipped(detail: &str) -> Self {
        Self {
            status: MailboxStatus::Skipped,
            detail: detail.to_owned(),
        }
    }
}

impl From<RecipientCheck> for MailboxFinding {
    fn from(value: RecipientCheck) -> Self {
        Self {
            status: match value.status {
                Outcome::Accepted => MailboxStatus::Accepted,
                Outcome::Invalid => MailboxStatus::Invalid,
                Outcome::Unknown => MailboxStatus::Unknown,
            },
            detail: value.detail,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Validation(Option<Arc<Remote>>);

#[derive(Debug)]
struct Remote {
    control: Control,
    retry_after: Mutex<Option<Instant>>,
}

impl Validation {
    pub(crate) fn new(control: Option<Control>) -> Self {
        Self(control.map(|control| {
            Arc::new(Remote {
                control,
                retry_after: Mutex::new(None),
            })
        }))
    }

    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Enrich the read-only address check without extending its HTTP deadline indefinitely.
    /// The dashboard sends batches of eight; larger API requests report any unchecked rows.
    pub async fn preflight(
        &self,
        findings: &mut [super::preflight::Finding],
        test_mode: bool,
    ) -> Result<(), Busy> {
        let mut candidates = Vec::new();
        for (index, finding) in findings.iter_mut().enumerate() {
            let skip = if test_mode {
                "SMTP is not checked in test mode."
            } else if !self.enabled() {
                "SMTP checking is not configured on this installation."
            } else if finding.status != "routable" {
                "SMTP needs an address with a mail route."
            } else if finding.suppression.is_some() || finding.hold.is_some() {
                "The workspace suppresses or holds this address."
            } else {
                candidates.push((index, finding.email.clone()));
                "The SMTP check budget expired; retry in batches of at most eight addresses."
            };
            finding.smtp = Some(MailboxFinding::skipped(skip));
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        for batch in candidates.chunks(BATCH) {
            if Instant::now() >= deadline {
                break;
            }
            let emails = batch
                .iter()
                .map(|(_, email)| email.clone())
                .collect::<Vec<_>>();
            let checked = self.check(&emails, deadline).await.inspect_err(|_| {
                for (index, _) in &candidates {
                    if let Some(finding) = findings.get_mut(*index)
                        && finding
                            .smtp
                            .as_ref()
                            .is_some_and(|smtp| smtp.status == MailboxStatus::Skipped)
                    {
                        finding.smtp = Some(MailboxFinding::skipped(
                            "SMTP checking is busy; try again shortly.",
                        ));
                    }
                }
            })?;
            for ((index, _), smtp) in batch.iter().zip(checked) {
                if let Some(finding) = findings.get_mut(*index) {
                    finding.smtp = Some(smtp);
                }
            }
        }
        Ok(())
    }

    /// One bounded batch, in input order. A missing, old or unreachable MTA is skipped, never
    /// interpreted as an invalid recipient. The normal bounce handling remains authoritative.
    async fn check(
        &self,
        emails: &[String],
        deadline: Instant,
    ) -> Result<Vec<MailboxFinding>, Busy> {
        let Some(remote) = &self.0 else {
            return Ok(vec![
                MailboxFinding::skipped(
                    "SMTP checking is not configured on this installation."
                );
                emails.len()
            ]);
        };
        // A down/old mail host should cost one timeout, not one timeout for every bulk batch.
        let unavailable = || {
            vec![
                MailboxFinding::skipped(
                    "The remote SMTP checker is unavailable; normal sending can continue."
                );
                emails.len()
            ]
        };
        if remote
            .retry_after
            .lock()
            .await
            .is_some_and(|until| until > Instant::now())
        {
            return Ok(unavailable());
        }
        let answer = timeout_at(deadline, remote.control.check_recipients(emails))
            .await
            .unwrap_or_else(|_| {
                Err(ControlError::Network(
                    "the recipient check deadline passed".to_owned(),
                ))
            });
        match answer {
            Ok(found) => Ok(found.into_iter().map(MailboxFinding::from).collect()),
            Err(ControlError::Answer { status: 429, .. }) => Err(Busy),
            Err(error) => {
                tracing::warn!(error = %error, "the remote recipient checker is unavailable; SMTP validation skipped");
                *remote.retry_after.lock().await = Some(Instant::now() + Duration::from_secs(60));
                Ok(unavailable())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn finding(email: &str) -> super::super::preflight::Finding {
        super::super::preflight::Finding {
            email: email.to_owned(),
            status: "routable",
            reason: "mx",
            detail: None,
            suppression: None,
            hold: None,
            smtp: None,
        }
    }

    #[tokio::test]
    async fn absent_configuration_skips_smtp_without_changing_dns() {
        let validation = Validation::default();
        assert!(!validation.enabled());
        let mut findings = [finding("ada@example.com")];
        validation.preflight(&mut findings, false).await.unwrap();
        assert_eq!(findings[0].status, "routable");
        assert_eq!(
            findings[0].smtp.as_ref().unwrap().status,
            MailboxStatus::Skipped
        );
    }

    #[tokio::test]
    async fn only_unavailable_remotes_are_skipped_and_put_into_cooldown() {
        for (status, expected_calls) in [
            (http::StatusCode::SERVICE_UNAVAILABLE, 1),
            (http::StatusCode::TOO_MANY_REQUESTS, 2),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let app = axum::Router::new().fallback(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                async move { status }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            let secret = secrecy::SecretString::from("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw");
            let validation = Validation::new(Some(Control::new(url, &secret, false).unwrap()));
            let mut findings = [finding("ada@example.com")];
            validation.preflight(&mut findings, true).await.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "test mode must never probe"
            );
            for _ in 0..2 {
                let result = validation.preflight(&mut findings, false).await;
                assert_eq!(
                    result.is_err(),
                    status == http::StatusCode::TOO_MANY_REQUESTS,
                    "campaigns must retry capacity limits, not record them as completed checks"
                );
                assert_eq!(
                    findings[0].smtp.as_ref().unwrap().status,
                    MailboxStatus::Skipped
                );
                assert_eq!(findings[0].status, "routable");
            }
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            server.abort();
        }
    }
}
