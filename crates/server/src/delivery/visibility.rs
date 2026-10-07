//! Recipient delivery evidence is independent of the submission lifecycle. Read all retained
//! events, not the truncated history embedded in a message response.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::ids::WorkspaceId;

/// Current delivery outcome, separate from whether a sending provider accepted the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Blocked,
    Delivered,
    Failed,
    Partial,
}

/// Distinct envelope recipients. The four counts are mutually exclusive; a confirmed delivery
/// takes precedence over earlier blocks or failures. Delivery does not imply inbox placement.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DeliverySummary {
    pub status: DeliveryStatus,
    pub delivered: i64,
    pub failed: i64,
    pub blocked: i64,
    pub unconfirmed: i64,
}

#[derive(Debug, FromRow)]
pub(super) struct Recipient {
    message_id: Uuid,
    email: Option<String>,
    delivered: bool,
    failed: bool,
    blocked: bool,
}

/// One indexed, tenant-bound query per message page, aggregating repeated provider notices.
pub(super) async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Recipient>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, Recipient>(
        "SELECT message_id, ascii_lower(recipient_email) AS email,
                bool_or(kind = 'delivered') AS delivered,
                bool_or(kind IN ('bounced', 'rejected')) AS failed,
                bool_or(category = 'policy' AND kind IN ('deferred', 'bounced', 'rejected')) AS blocked
           FROM delivery_events
          WHERE workspace_id = $1 AND message_id = ANY($2)
          GROUP BY message_id, ascii_lower(recipient_email)",
    )
    .bind(workspace.uuid())
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut messages: HashMap<Uuid, Vec<Recipient>> = HashMap::new();
    for row in rows {
        messages.entry(row.message_id).or_default().push(row);
    }
    Ok(messages)
}

/// Unknown-recipient delivery reports settle only a single-recipient envelope. An unknown
/// policy restriction can block the whole route, but never overrides known recipient delivery.
pub(super) fn summarize(addresses: &[String], evidence: &[Recipient]) -> DeliverySummary {
    let mut addresses: Vec<String> = addresses
        .iter()
        .map(|email| email.to_ascii_lowercase())
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    let single = addresses.len() == 1;
    let recipients = i64::try_from(addresses.len()).unwrap_or(i64::MAX);
    let mut summary = DeliverySummary {
        status: DeliveryStatus::Pending,
        delivered: 0,
        failed: 0,
        blocked: 0,
        unconfirmed: 0,
    };
    for address in &addresses {
        let relevant = |row: &&Recipient| {
            row.email.as_ref() == Some(address) || (single && row.email.is_none())
        };
        if evidence.iter().filter(relevant).any(|row| row.delivered) {
            summary.delivered += 1;
        } else if evidence.iter().filter(relevant).any(|row| row.failed) {
            summary.failed += 1;
        } else if evidence
            .iter()
            .any(|row| row.blocked && (row.email.as_ref() == Some(address) || row.email.is_none()))
        {
            summary.blocked += 1;
        } else {
            summary.unconfirmed += 1;
        }
    }
    summary.status = if !addresses.is_empty() && summary.delivered == recipients {
        DeliveryStatus::Delivered
    } else if !addresses.is_empty() && summary.failed == recipients {
        DeliveryStatus::Failed
    } else if summary.blocked > 0 {
        DeliveryStatus::Blocked
    } else if summary.delivered > 0 || summary.failed > 0 {
        DeliveryStatus::Partial
    } else {
        DeliveryStatus::Pending
    };
    summary
}

#[cfg(test)]
mod tests {
    use super::{DeliveryStatus, Recipient, summarize};

    fn report(email: Option<&str>, delivered: bool, failed: bool, blocked: bool) -> Recipient {
        Recipient {
            message_id: uuid::Uuid::nil(),
            email: email.map(str::to_owned),
            delivered,
            failed,
            blocked,
        }
    }

    #[test]
    fn policy_block_becomes_delivery_or_terminal_failure_without_losing_evidence() {
        let addresses = vec!["person@example.test".to_owned()];
        let blocked = [report(None, false, false, true)];
        assert_eq!(
            summarize(&addresses, &blocked).status,
            DeliveryStatus::Blocked
        );
        let delivered = [
            report(None, false, false, true),
            report(None, true, false, false),
        ];
        let summary = summarize(&addresses, &delivered);
        assert_eq!(summary.status, DeliveryStatus::Delivered);
        assert_eq!(
            (summary.delivered, summary.blocked, summary.unconfirmed),
            (1, 0, 0)
        );
        let failed = [
            report(None, false, false, true),
            report(None, false, true, false),
        ];
        assert_eq!(
            summarize(&addresses, &failed).status,
            DeliveryStatus::Failed
        );
    }

    #[test]
    fn mixed_recipients_and_unknown_delivery_never_claim_complete_delivery() {
        let addresses = vec!["a@example.test".to_owned(), "b@example.test".to_owned()];
        let unknown = [report(None, true, false, false)];
        assert_eq!(
            summarize(&addresses, &unknown).status,
            DeliveryStatus::Pending
        );
        let mixed = [
            report(Some("a@example.test"), true, false, false),
            report(None, false, false, true),
        ];
        let summary = summarize(&addresses, &mixed);
        assert_eq!(summary.status, DeliveryStatus::Blocked);
        assert_eq!((summary.delivered, summary.blocked), (1, 1));
        let partial = [report(Some("a@example.test"), true, false, false)];
        assert_eq!(
            summarize(&addresses, &partial).status,
            DeliveryStatus::Partial
        );
    }

    #[test]
    fn duplicate_addresses_and_unrelated_recipient_reports_do_not_inflate_outcomes() {
        let addresses = vec!["A@example.test".to_owned(), "a@example.test".to_owned()];
        let evidence = [
            report(Some("a@example.test"), true, false, false),
            report(Some("other@example.test"), false, false, true),
        ];
        let summary = summarize(&addresses, &evidence);
        assert_eq!(summary.status, DeliveryStatus::Delivered);
        assert_eq!(summary.delivered, 1);
        assert_eq!(summary.blocked, 0);
    }
}
