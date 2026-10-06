//! Activate a tracking hostname only after valid HTTPS reaches its ingress route.
//! Certificates belong to the reverse proxy; this job observes their readiness.

use std::net::SocketAddr;
use std::time::Duration;

use norbelys_mail::net::is_public;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

use super::Env;
use crate::domain::ids::{Id, SendingDomain};
use crate::domain::time::Timestamp;
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue};

#[derive(FromRow)]
struct Domain {
    hostname: String,
    updated_at: Timestamp,
}

/// Bounded certificate readiness polling, restarted by a person's or daily DNS check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainCertificate {
    pub domain: Id<SendingDomain>,
}

impl Job for DomainCertificate {
    const KIND: &'static str = "domain.certificate";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.domain.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let env = cx.env::<Env>()?.clone();
        let mut tx = cx.db().begin_in(workspace).await?;
        let domain = sqlx::query_as::<_, Domain>("SELECT hostname, updated_at FROM sending_domains WHERE workspace_id = $1 AND id = $2 AND purpose = 'tracking' AND status = 'pending_certificate' AND dns_checks @> '{\"ownership\":true,\"tracking\":true}'::jsonb")
            .bind(workspace.uuid()).bind(self.domain.uuid()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some(domain) = domain else {
            return Ok(Outcome::Done);
        };
        let active = reachable(&env, &domain.hostname, self.domain).await;
        let attempts = cx
            .progress()
            .and_then(|progress| progress.get("attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut chunk = cx.begin().await?;
        let error = (!active).then(|| json!({"code":"tracking_https_pending","detail":"The DNS records are verified. HTTPS has not reached the tracking service yet; check the proxy certificate configuration and DNS-only CNAME.","at":crate::process::now()}));
        sqlx::query("UPDATE sending_domains SET status = CASE WHEN $3 THEN 'active' ELSE status END, activated_at = CASE WHEN $3 THEN coalesce(activated_at, now()) ELSE activated_at END, last_error = $4, dns_checks = dns_checks || jsonb_build_object('https', $3::boolean) WHERE workspace_id = $1 AND id = $2 AND purpose = 'tracking' AND status = 'pending_certificate' AND updated_at = $5")
            .bind(workspace.uuid()).bind(self.domain.uuid()).bind(active).bind(error).bind(domain.updated_at).execute(&mut **chunk.tx()).await?;
        cx.checkpoint(
            chunk,
            json!({"attempts":attempts.saturating_add(1),"active":active}),
        )
        .await?;
        if !active && attempts < 12 {
            Ok(Outcome::Yield {
                after: Duration::from_secs(30 * (attempts + 1).min(10)),
            })
        } else {
            Ok(Outcome::Done)
        }
    }
}

/// Pin public addresses shared with the configured tracking target. DNS rebinding,
/// private addresses, redirects, proxies and invalid TLS certificates are refused.
async fn reachable(env: &Env, host: &str, id: Id<SendingDomain>) -> bool {
    let Ok(customer) = env.resolver.addresses(host).await else {
        return false;
    };
    let customer: Vec<_> = customer.iter().collect();
    if customer.is_empty() || customer.iter().any(|ip| !is_public(*ip)) {
        return false;
    }
    let Ok(target) = env
        .resolver
        .addresses(&env.settings.tracking_cname_target)
        .await
    else {
        return false;
    };
    let addresses: Vec<_> = target
        .iter()
        .filter(|ip| is_public(*ip) && customer.contains(ip))
        .map(|ip| SocketAddr::new(ip, 443))
        .collect();
    if addresses.is_empty() {
        return false;
    }
    let Ok(client) = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .build()
    else {
        return false;
    };
    let Ok(mut response) = client
        .get(format!("https://{host}/.well-known/norbelys-tracking"))
        .send()
        .await
    else {
        return false;
    };
    if response.status() != reqwest::StatusCode::OK {
        return false;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= 64 => body.extend_from_slice(&chunk),
            Ok(None) => return body == id.uuid().to_string().as_bytes(),
            _ => return false,
        }
    }
}
