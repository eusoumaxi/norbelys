//! Ids: one prefixed UUIDv7 per resource.
//!
//! Every row's primary key is a UUIDv7 (<https://www.rfc-editor.org/rfc/rfc9562#section-5.7>),
//! which sorts by creation time, so "newest first" is an index scan on the key itself and
//! cursors can page by id.
//!
//! `Id<R>` is the uuid of a row of resource `R`. On the wire it is the resource's prefix plus
//! the uuid's 32 hexadecimal digits (`msg_0190…`), so an id of the wrong resource fails to
//! parse before any query. In SQL it is the plain `uuid`. [`WorkspaceId`] is different: it is
//! the tenant a transaction runs as, built only from a verified credential or a claimed row.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// A resource with an id prefix.
pub trait Resource {
    /// The prefix, including its underscore.
    const PREFIX: &'static str;
}

macro_rules! resources {
    ($($(#[$doc:meta])* $name:ident => $prefix:literal),* $(,)?) => {
        $(
            $(#[$doc])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub enum $name {}
            impl Resource for $name {
                const PREFIX: &'static str = $prefix;
            }
            impl utoipa::__dev::ComposeSchema for $name {
                fn compose(_generics: Vec<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
                    utoipa::openapi::ObjectBuilder::new().into()
                }
            }
            impl utoipa::ToSchema for $name {}
        )*
    };
}

resources! {
    /// A workspace.
    Workspace => "ws_",
    /// A user.
    User => "usr_",
    /// A membership of a user in a workspace.
    Membership => "mem_",
    /// An invitation to a workspace.
    Invitation => "inv_",
    /// An API key.
    ApiKey => "key_",
    /// A workspace SSO connection.
    SsoConnection => "sso_",
    /// A sending connection.
    Connection => "con_",
    /// A provider webhook route.
    ProviderWebhook => "pwh_",
    /// A quota scope.
    QuotaScope => "qsc_",
    /// A sender identity (a From address on a connection).
    SenderIdentity => "sid_",
    /// A receive binding.
    ReceiveBinding => "rcv_",
    /// A sending domain.
    SendingDomain => "dom_",
    /// A person.
    Person => "per_",
    /// A custom field definition.
    Field => "fld_",
    /// An import.
    Import => "imp_",
    /// An export.
    Export => "exp_",
    /// A group of people.
    Group => "grp_",
    /// A segment.
    Segment => "seg_",
    /// A suppression.
    Suppression => "sup_",
    /// A campaign.
    Campaign => "cmp_",
    /// A step of a campaign.
    Step => "stp_",
    /// A variant of a step.
    Variant => "var_",
    /// An enrollment of a person in a campaign.
    Enrollment => "enr_",
    /// A message.
    Message => "msg_",
    /// A file attached to a message.
    Attachment => "fil_",
    /// A delivery attempt.
    Attempt => "att_",
    /// A delivery event (evidence).
    DeliveryEvent => "dev_",
    /// An inbound message.
    InboundMessage => "inb_",
    /// A thread.
    Thread => "thr_",
    /// A job.
    Job => "job_",
    /// A webhook endpoint of the customer.
    WebhookEndpoint => "whe_",
    /// A delivery of an outbound webhook.
    WebhookDelivery => "whd_",
    /// An outbox event.
    OutboxEvent => "evt_",
    /// An OAuth grant.
    Grant => "grt_",
    /// A browser session.
    Session => "ses_",
    /// A passkey.
    Passkey => "pky_",
    /// An authentication challenge (a ceremony).
    Challenge => "chl_",
    /// A link between a user and an external identity (an OpenID Connect issuer and subject).
    IdentityLink => "idn_",
    /// An entry of a workspace's audit log.
    AuditEntry => "aud_",
    /// An image uploaded for mail content, served at a public URL.
    Image => "img_",
    /// One call to an AI provider: its reservation and settlement (`ai_calls`). Not an API
    /// resource; the id names the call in telemetry and in the provider's request.
    AiCall => "aic_",
}

/// The id of a row of resource `R`.
pub struct Id<R> {
    uuid: Uuid,
    resource: PhantomData<fn() -> R>,
}

impl<R> Id<R> {
    /// A new time-ordered id.
    #[must_use]
    pub fn new() -> Self {
        Self::from_uuid(Uuid::now_v7())
    }

    /// The id of an existing row.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self {
            uuid,
            resource: PhantomData,
        }
    }

    /// The row's uuid.
    #[must_use]
    pub const fn uuid(&self) -> Uuid {
        self.uuid
    }
}

impl<R> Default for Id<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> Clone for Id<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for Id<R> {}

impl<R> PartialEq for Id<R> {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl<R> Eq for Id<R> {}

impl<R> PartialOrd for Id<R> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<R> Ord for Id<R> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.uuid.cmp(&other.uuid)
    }
}

impl<R> Hash for Id<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.uuid.hash(state);
    }
}

impl<R: Resource> fmt::Display for Id<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", R::PREFIX, self.uuid.simple())
    }
}

impl<R: Resource> fmt::Debug for Id<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// Why a string is not an id of the expected resource.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected an id starting with `{prefix}` followed by 32 hexadecimal characters")]
pub struct IdError {
    /// The prefix that was expected.
    pub prefix: &'static str,
}

impl<R: Resource> FromStr for Id<R> {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let error = IdError { prefix: R::PREFIX };
        let hex = value.strip_prefix(R::PREFIX).ok_or(error.clone())?;
        if hex.len() != 32
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(error);
        }
        Uuid::parse_str(hex).map(Self::from_uuid).map_err(|_| error)
    }
}

impl<R: Resource> Serialize for Id<R> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de, R: Resource> Deserialize<'de> for Id<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl<R: Resource> utoipa::__dev::ComposeSchema for Id<R> {
    fn compose(
        _generics: Vec<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>,
    ) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .pattern(Some(format!("^{}[0-9a-f]{{32}}$", R::PREFIX)))
            .examples([serde_json::Value::String(format!(
                "{}0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
                R::PREFIX
            ))])
            .into()
    }
}

impl<R: Resource> utoipa::ToSchema for Id<R> {}

/// The workspace a transaction runs as. It is built only from a verified credential
/// (`identity::authority`) or from a row a background role claimed, never from a request
/// parameter, so a forged or guessed id can never select a tenant: tenancy is a property of
/// the credential, not of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(Uuid);

impl WorkspaceId {
    /// Trusts `uuid` as the tenant. Callers: credential verification and claimed rows.
    #[must_use]
    pub const fn trusted(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The workspace's uuid.
    #[must_use]
    pub const fn uuid(self) -> Uuid {
        self.0
    }

    /// The workspace's public id.
    #[must_use]
    pub const fn id(self) -> Id<Workspace> {
        Id::from_uuid(self.0)
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.id(), formatter)
    }
}
