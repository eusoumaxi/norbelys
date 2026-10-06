//! Prepare publication instructions before ownership is proven. Preparation never
//! grants SMTP or IMAP access; the managed MTA still verifies ownership itself.

use hickory_resolver::proto::rr::RData;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{DomainPurpose, Env, Settings};
use crate::domain::ids::{Id, SendingDomain};
use crate::domain::time::Timestamp;
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue};

/// Publication preparation is independent of DNS ownership verification.
#[derive(Debug, Clone, Copy, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DnsPreparation {
    Preparing,
    Ready,
    Unavailable,
}

pub(super) fn preparation(purpose: DomainPurpose, checks: &Value) -> DnsPreparation {
    if purpose == DomainPurpose::Tracking {
        return DnsPreparation::Ready;
    }
    match checks.get("preparation").and_then(Value::as_str) {
        Some("ready") => DnsPreparation::Ready,
        Some("unavailable") => DnsPreparation::Unavailable,
        _ if checks
            .get("dkim_record")
            .is_some_and(|value| !value.is_null()) =>
        {
            DnsPreparation::Ready
        }
        _ => DnsPreparation::Preparing,
    }
}

/// A published incoming-mail route, including the provider's MX preference.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MailExchange {
    pub hostname: String,
    pub priority: u16,
}

pub(super) fn exchanges(checks: &Value) -> Vec<MailExchange> {
    let mut records: Vec<MailExchange> = checks
        .get("existing_mx")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    records.truncate(64);
    records
}

pub(super) fn warnings(
    purpose: DomainPurpose,
    name: &str,
    settings: &Settings,
    checks: &Value,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mx = exchanges(checks);
    let external: Vec<_> = mx
        .iter()
        .filter(|record| record.hostname != settings.mta_submission_host)
        .collect();
    if purpose.receives() && !external.is_empty() {
        let provider = if external.iter().any(|record| {
            record.hostname.ends_with(".google.com")
                || record.hostname.ends_with(".googlemail.com")
                || record.hostname == "smtp.google.com"
        }) {
            "Google Workspace"
        } else if external
            .iter()
            .any(|record| record.hostname.ends_with(".mail.protection.outlook.com"))
        {
            "Microsoft 365"
        } else {
            "another mail provider"
        };
        warnings.push(format!("{name} already receives mail through {provider}. Keep its MX for send-only use, connect that provider's mailbox, or choose a separate receiving subdomain. Replacing its MX moves incoming mail."));
    }
    if purpose == DomainPurpose::Tracking && !mx.is_empty() {
        warnings.push(format!("{name} has MX records. A tracking CNAME cannot coexist with them; choose another tracking hostname or deliberately move its existing mail routing."));
    }
    if purpose != DomainPurpose::Tracking
        && checks
            .get("existing_cname")
            .is_some_and(|value| !value.is_null())
    {
        warnings.push(format!("{name} is a CNAME. Mail TXT and MX records need a hostname without that CNAME; keep the alias for tracking or choose another mail hostname."));
    }
    if checks
        .get("existing_spf")
        .and_then(Value::as_array)
        .is_some_and(|records| records.len() > 1)
    {
        warnings.push("Multiple SPF records were found. Merge all authorized senders into one SPF record; do not add another.".to_owned());
    }
    warnings
}

/// Observe customer DNS without changing it or treating a resolver failure as absence.
pub(super) async fn discover(env: &Env, name: &str) -> Result<Value, JobError> {
    let mx = match env.resolver.mx(name).await {
        Ok(answer) => answer
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::MX(mx) => Some(MailExchange {
                    hostname: mx
                        .exchange
                        .to_utf8()
                        .trim_end_matches('.')
                        .to_ascii_lowercase(),
                    priority: mx.preference,
                }),
                _ => None,
            })
            .collect::<Vec<_>>(),
        Err(error) if error.is_nx_domain() || error.is_no_records_found() => Vec::new(),
        Err(error) => return Err(JobError::Failed(format!("MX lookup for {name}: {error}"))),
    };
    let spf = match env.resolver.txt(name).await {
        Ok(answer) => answer
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::TXT(txt) => String::from_utf8(
                    txt.txt_data
                        .iter()
                        .flat_map(|part| part.iter().copied())
                        .collect(),
                )
                .ok()
                .filter(|text| text.starts_with("v=spf1 ")),
                _ => None,
            })
            .collect::<Vec<_>>(),
        Err(error) if error.is_nx_domain() || error.is_no_records_found() => Vec::new(),
        Err(error) => return Err(JobError::Failed(format!("TXT lookup for {name}: {error}"))),
    };
    let cname = match env.resolver.cname(name).await {
        Ok(answer) => answer
            .answers()
            .iter()
            .find_map(|record| match &record.data {
                RData::CNAME(target) => Some(
                    target
                        .0
                        .to_ascii()
                        .trim_end_matches('.')
                        .to_ascii_lowercase(),
                ),
                _ => None,
            }),
        Err(error) if error.is_nx_domain() || error.is_no_records_found() => None,
        Err(error) => {
            return Err(JobError::Failed(format!(
                "CNAME lookup for {name}: {error}"
            )));
        }
    };
    let managed_mx = mx
        .iter()
        .any(|record| record.hostname == env.settings.mta_submission_host)
        && mx
            .iter()
            .all(|record| record.hostname == env.settings.mta_submission_host);
    Ok(json!({"existing_mx":mx,"existing_spf":spf,"existing_cname":cname,"mx":managed_mx}))
}

/// Render mail instructions without requiring the customer to publish ownership first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainPrepare {
    pub domain: Id<SendingDomain>,
}

impl Job for DomainPrepare {
    const KIND: &'static str = "domain.prepare";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.domain.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let env = cx.env::<Env>()?.clone();
        let mut tx = cx.db().begin_in(workspace).await?;
        let row = sqlx::query!(r#"SELECT hostname, ownership_token, purpose, updated_at AS "updated_at: Timestamp" FROM sending_domains WHERE workspace_id = $1 AND id = $2"#,
            workspace.uuid(), self.domain.uuid()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(Outcome::Done);
        };
        let purpose = DomainPurpose::stored(&row.purpose)?;
        let attempts = cx
            .progress()
            .and_then(|progress| progress.get("attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let Value::Object(mut checks) = discover(&env, &row.hostname).await? else {
            return Err(JobError::Failed(
                "DNS observations must be an object".to_owned(),
            ));
        };
        let mut waiting = false;
        if purpose != DomainPurpose::Tracking {
            if let Some(control) = &env.control {
                let prepared = control
                    .prepare_domain(
                        &row.hostname,
                        &format!("norbelys-verification={}", row.ownership_token),
                    )
                    .await
                    .map_err(|error| JobError::Failed(error.to_string()))?;
                waiting = purpose.sends() && prepared.dkim.is_none();
                checks.insert("spf_record".to_owned(), json!(prepared.spf));
                checks.insert("dmarc_record".to_owned(), json!(prepared.dmarc));
                checks.insert(
                    "dkim_record".to_owned(),
                    prepared.dkim.map_or(
                        Value::Null,
                        |(name, value)| json!({"name":name,"value":value}),
                    ),
                );
                checks.insert(
                    "preparation".to_owned(),
                    json!(if waiting && attempts >= 12 {
                        "unavailable"
                    } else if waiting {
                        "preparing"
                    } else {
                        "ready"
                    }),
                );
                checks.insert("mta_configured".to_owned(), json!(true));
            } else {
                checks.insert("preparation".to_owned(), json!("unavailable"));
                checks.insert("mta_configured".to_owned(), json!(false));
            }
        } else {
            checks.insert("preparation".to_owned(), json!("ready"));
        }
        let mut chunk = cx.begin().await?;
        let changed = sqlx::query("UPDATE sending_domains SET dns_checks = dns_checks || $3 WHERE workspace_id = $1 AND id = $2 AND updated_at = $4")
            .bind(workspace.uuid()).bind(self.domain.uuid()).bind(Value::Object(checks)).bind(row.updated_at)
            .execute(&mut **chunk.tx()).await?.rows_affected();
        cx.checkpoint(
            chunk,
            json!({"preparing":waiting,"attempts":attempts.saturating_add(1)}),
        )
        .await?;
        if (waiting && attempts < 12) || changed == 0 {
            Ok(Outcome::Yield {
                after: std::time::Duration::from_secs(5 * (attempts + 1).min(12)),
            })
        } else {
            Ok(Outcome::Done)
        }
    }
}
