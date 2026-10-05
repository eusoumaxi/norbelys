//! Customer webhooks: business facts recorded in an outbox, published to the customer's
//! endpoints, and delivered signed per the Standard Webhooks specification
//! (<https://www.standardwebhooks.com/>).
//!
//! # The path of an event
//!
//! 1. **Record.** A module that changes something a customer may want to hear about calls
//!    [`outbox::record`] in the same transaction as the change: the event exists exactly when
//!    the change commits, and never otherwise. Internal reactions never go through the outbox;
//!    a module calls another module's function in the same transaction instead.
//! 2. **Publish.** The `outbox.relay` job runs every 5 seconds. It reads which workspaces have
//!    unpublished events, and for each, in that workspace's transaction, creates one delivery
//!    per enabled endpoint subscribed to the event's type, marks the event published and
//!    enqueues one `webhook.deliver` job per delivery, atomically. An event nobody subscribes
//!    to is marked published with no delivery, so nothing stays unpublished.
//! 3. **Deliver.** The `webhook.deliver` job POSTs `{type, timestamp, data}` to the endpoint
//!    with the headers `webhook-id` (the delivery's id), `webhook-timestamp` and
//!    `webhook-signature`. Failures are retried on the published Standard Webhooks schedule
//!    (5 s, 5 min, 30 min, 2 h, 5 h, 10 h, 14 h, 20 h, 24 h; ten attempts), honouring
//!    `Retry-After` on `429`, `502`, `503` and `504`.
//!
//! # Delivery identity
//!
//! There is one delivery per event and endpoint (the database enforces it), and its id is the
//! `webhook-id`. Every automatic retry, manual retry and replay is another attempt of that same
//! delivery, with a fresh timestamp and signature: consumers deduplicate on one id whatever
//! happened. Delivery is at-least-once; the stable id is the contract that makes a repeat
//! harmless to a consumer that deduplicates.
//!
//! # Endpoint health
//!
//! An endpoint that answers `410 Gone` is disabled at once. An endpoint that fails without a
//! success for 5 days is disabled as `failing`. Either way, and when a person disables one, a
//! `webhook_endpoint.disabled` event is written for the endpoints that still work, and the
//! disabled endpoint's pending deliveries stop. Re-enabling an endpoint and replaying a window
//! of its events (`POST /webhook_endpoints/{id}/replay`) recovers what it was owed.
//!
//! # Safety of outbound requests
//!
//! The endpoint's URL is the customer's, so the worker refuses to call private, loopback,
//! link-local or otherwise reserved addresses, whether written literally or reached through
//! DNS (checked at connection time, so a name that later resolves inward is refused too), and
//! requires `https`. Redirects are never followed. A development deployment may allow private
//! targets and plain `http` (`WEBHOOK_ALLOW_PRIVATE_TARGETS`), to deliver to a local receiver.

pub mod deliver;
pub mod endpoints;
pub mod http;
pub mod ingress;
pub mod normalize;
pub mod outbox;
#[cfg(test)]
mod receipts_tests;
pub mod reconcile;
#[cfg(test)]
mod retention_tests;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The type of an outbox event: the closed vocabulary customers subscribe to. Requests refuse
/// any other value; readers must expect new values to appear over time.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
pub enum EventType {
    /// A message was accepted for sending.
    #[strum(serialize = "message.queued")]
    MessageQueued,
    /// A provider accepted a message.
    #[strum(serialize = "message.sent")]
    MessageSent,
    /// A message failed for good.
    #[strum(serialize = "message.failed")]
    MessageFailed,
    /// A message's submission ended without a readable answer: it may have been sent.
    #[strum(serialize = "message.uncertain")]
    MessageUncertain,
    /// A queued message was cancelled.
    #[strum(serialize = "message.cancelled")]
    MessageCancelled,
    /// A campaign step message that asks for personalisation snippets was created without them:
    /// its template's defaults are used, for the reason given.
    #[strum(serialize = "message.snippets_fallback")]
    MessageSnippetsFallback,
    /// Evidence about a message's fate after submission was recorded.
    #[strum(serialize = "delivery_event.recorded")]
    DeliveryEventRecorded,
    /// A connected inbox received a message.
    #[strum(serialize = "inbound_message.received")]
    InboundMessageReceived,
    /// An enrollment stopped before its last step.
    #[strum(serialize = "enrollment.stopped")]
    EnrollmentStopped,
    /// An enrollment went through its last step.
    #[strum(serialize = "enrollment.completed")]
    EnrollmentCompleted,
    /// A campaign started, paused, was archived or ran out of people.
    #[strum(serialize = "campaign.status_changed")]
    CampaignStatusChanged,
    /// A connection was paused, disabled, lost its authorization, was archived or recovered.
    #[strum(serialize = "connection.health_changed")]
    ConnectionHealthChanged,
    /// An import finished.
    #[strum(serialize = "import.completed")]
    ImportCompleted,
    /// An export is ready.
    #[strum(serialize = "export.completed")]
    ExportCompleted,
    /// An address was suppressed.
    #[strum(serialize = "suppression.created")]
    SuppressionCreated,
    /// The month's AI spend reached 80 % of its budget.
    #[strum(serialize = "ai.budget_warning")]
    AiBudgetWarning,
    /// The month's AI spend reached its budget.
    #[strum(serialize = "ai.budget_exceeded")]
    AiBudgetExceeded,
    /// A test sent to one endpoint on request.
    #[strum(serialize = "endpoint.test")]
    EndpointTest,
    /// A webhook endpoint was disabled (it answered `410`, kept failing, or a person disabled it).
    #[strum(serialize = "webhook_endpoint.disabled")]
    WebhookEndpointDisabled,
}

impl EventType {
    /// The type as written on the wire and in `outbox_events.type`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl Serialize for EventType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("`{value}` is not an event type")))
    }
}

impl utoipa::PartialSchema for EventType {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use strum::IntoEnumIterator as _;
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .description(Some(
                "The type of an event: the closed vocabulary webhook endpoints subscribe to. New \
                 types may be added.",
            ))
            .enum_values(Some(Self::iter().map(Self::as_str)))
            .into()
    }
}

impl utoipa::ToSchema for EventType {}
