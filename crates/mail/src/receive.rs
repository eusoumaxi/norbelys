//! What every mailbox reader returns: a bounded [`Page`] of new message ids after a
//! provider-owned cursor, and each message as a [`RawMessage`] under its transport identity.
//!
//! Three cursor models exist: IMAP's `UIDVALIDITY` plus the last UID read, Gmail's history id,
//! and Microsoft Graph's delta link. Each reader module defines its own cursor type and returns
//! the shared [`Page`], so the caller's poll loop has one shape for all of them.
//!
//! Invariants:
//! - The caller stores a page's cursor only after it stored the page's messages, so a crash in
//!   between re-reads the page instead of skipping it; a page is never larger than the caller's
//!   limit.
//! - A cursor the provider no longer honours (`UIDVALIDITY` changed, a history gap, an expired
//!   delta) is reported as a [`Reset`]: the page restarts at the caller's `since` instant, an
//!   overlapping resync. Gmail and Graph ids are stable, so the transport identity absorbs the
//!   overlap; after an IMAP `UIDVALIDITY` change every UID is new, so re-read messages get new
//!   identities and only a content hint can flag them as duplicates. A resync never jumps to the
//!   newest message, which would silently drop every reply that arrived during the gap; when
//!   the resync could not read everything since `since`, [`Reset::truncated`] makes the gap
//!   visible.
//! - The transport identity ([`TransportIdentity`]) is the only hard key of an inbound message:
//!   two reads of the same message have the same identity, so the caller deduplicates on it.
//!   A content hash is only a hint, because two different messages can have the same content.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The action a mailbox reader's failure permits, independent of its wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The credential is refused and must be replaced or refreshed.
    Credential,
    /// The account or its permission is refused.
    Permission,
    /// Wait for the provider's budget before reading again.
    Throttled,
    /// Temporary network, TLS, service or command failure.
    Unavailable,
    /// The caller's deadline expired.
    Deadline,
    /// The provider's response or cursor cannot be understood safely.
    InvalidResponse,
    /// A local read bound was exceeded.
    TooLarge,
    /// The address/security policy refuses this endpoint.
    UnsafeEndpoint,
}

/// Whose read budget is exhausted; this describes a provider account or API client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitScope {
    Account,
    Client,
}

/// Common receive failure. Remote free text, credentials and URLs are never in its diagnostic.
/// HTTP status is retained for races such as a message deleted between listing and reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub failure: Failure,
    pub status: Option<u16>,
    pub retry_after: Option<Timestamp>,
    pub scope: LimitScope,
}

impl Error {
    /// Whether continuing with the same credential/account requires a connection check.
    #[must_use]
    pub fn refused_credential(&self) -> bool {
        matches!(self.failure, Failure::Credential | Failure::Permission)
    }

    /// A finite diagnostic suitable for status rows and logs, without provider-controlled text.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self.failure {
            Failure::Credential => "receive_credential_refused",
            Failure::Permission => "receive_permission_refused",
            Failure::Throttled => "receive_throttled",
            Failure::Unavailable => "receive_unavailable",
            Failure::Deadline => "receive_deadline",
            Failure::InvalidResponse => "receive_invalid_response",
            Failure::TooLarge => "receive_too_large",
            Failure::UnsafeEndpoint => "receive_unsafe_endpoint",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}
impl std::error::Error for Error {}

impl From<crate::http::ApiError> for Error {
    fn from(error: crate::http::ApiError) -> Self {
        use crate::http::ApiError;
        let (failure, status, retry_after, scope) = match error {
            ApiError::Unauthorized => (Failure::Credential, Some(401), None, LimitScope::Account),
            ApiError::Forbidden { .. } => {
                (Failure::Permission, Some(403), None, LimitScope::Account)
            }
            ApiError::Throttled {
                retry_after,
                platform,
                ..
            } => (
                Failure::Throttled,
                None,
                retry_after,
                if platform {
                    LimitScope::Client
                } else {
                    LimitScope::Account
                },
            ),
            ApiError::Status { status, .. } => (
                if status >= 500 {
                    Failure::Unavailable
                } else {
                    Failure::InvalidResponse
                },
                Some(status),
                None,
                LimitScope::Account,
            ),
            ApiError::Timeout => (Failure::Deadline, None, None, LimitScope::Account),
            ApiError::Network(_) => (Failure::Unavailable, None, None, LimitScope::Account),
            ApiError::TooLarge(_) => (Failure::TooLarge, None, None, LimitScope::Account),
            ApiError::InvalidResponse(_) => {
                (Failure::InvalidResponse, None, None, LimitScope::Account)
            }
        };
        Self {
            failure,
            status,
            retry_after,
            scope,
        }
    }
}

impl From<crate::imap::ImapError> for Error {
    fn from(error: crate::imap::ImapError) -> Self {
        use crate::imap::ImapError;
        let failure = match error {
            ImapError::Unauthorized(_) => Failure::Credential,
            ImapError::Timeout => Failure::Deadline,
            ImapError::Plaintext | ImapError::Resolve(crate::net::ResolveError::NotPublic(_)) => {
                Failure::UnsafeEndpoint
            }
            ImapError::NoUidValidity | ImapError::Protocol(_) => Failure::InvalidResponse,
            ImapError::Resolve(_)
            | ImapError::Connect(_)
            | ImapError::Tls(_)
            | ImapError::Refused(_) => Failure::Unavailable,
        };
        Self {
            failure,
            status: None,
            retry_after: None,
            scope: LimitScope::Account,
        }
    }
}

/// Provider ids already listed but not yet returned to the caller. Stored with the opaque
/// cursor so a bounded page never advances past messages it has not handed off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub ids: Vec<String>,
    pub more: bool,
}

impl Pending {
    pub(crate) const MAX: usize = 10_000;

    pub(crate) fn split(
        mut ids: Vec<String>,
        limit: u32,
        more: bool,
    ) -> (Vec<String>, bool, Option<Self>) {
        let limit = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
        if ids.len() <= limit {
            return (ids, more, None);
        }
        let remaining = ids.split_off(limit);
        (
            ids,
            true,
            Some(Self {
                ids: remaining,
                more,
            }),
        )
    }

    pub(crate) fn page(&self, limit: u32) -> (Vec<String>, bool, Option<Self>) {
        Self::split(self.ids.clone(), limit, self.more)
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use crate::http::ApiError;
    use crate::imap::ImapError;

    #[test]
    fn protocol_failures_share_actions_and_keep_no_remote_text() {
        let http = Error::from(ApiError::Unauthorized);
        let imap = Error::from(ImapError::Unauthorized("remote credential echo".into()));
        assert_eq!(http.failure, imap.failure);
        assert!(http.refused_credential() && imap.refused_credential());
        let permission = Error::from(ApiError::Forbidden {
            reason: "private reason".into(),
        });
        assert!(permission.refused_credential());
        let deadline = Error::from(ImapError::Timeout);
        assert_eq!(deadline.failure, Error::from(ApiError::Timeout).failure);
        assert!(!deadline.refused_credential());
        let at = Timestamp::now();
        for (platform, scope) in [(false, LimitScope::Account), (true, LimitScope::Client)] {
            let throttled = Error::from(ApiError::Throttled {
                retry_after: Some(at),
                platform,
                reason: "private account".into(),
            });
            assert_eq!(
                (throttled.failure, throttled.scope, throttled.retry_after),
                (Failure::Throttled, scope, Some(at))
            );
            assert!(!throttled.refused_credential());
            assert!(!format!("{throttled:?} {throttled}").contains("private"));
        }
        assert_eq!(
            Error::from(ApiError::Status {
                status: 404,
                reason: "private message".into()
            })
            .status,
            Some(404)
        );
        assert_eq!(
            Error::from(ApiError::Status {
                status: 503,
                reason: String::new()
            })
            .failure,
            Failure::Unavailable
        );
        assert_eq!(
            Error::from(ImapError::Plaintext).failure,
            Failure::UnsafeEndpoint
        );
    }
}

/// One bounded page of changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<I, C> {
    /// The new messages' ids, oldest first where the provider orders them.
    pub ids: Vec<I>,
    /// The cursor to store once these messages are.
    pub cursor: C,
    /// The page came back full: more changes are waiting, poll again at once.
    pub more: bool,
    /// The previous cursor was not honoured, and this page restarted at `since`.
    pub reset: Option<Reset>,
}

/// A cursor the provider no longer honoured, and how the page recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reset {
    /// Why the cursor failed.
    pub reason: ResetReason,
    /// The instant the resync restarted at: the caller's `since`.
    pub since: Timestamp,
    /// More messages arrived since `since` than the resync read: a visible gap.
    pub truncated: bool,
}

/// Why a cursor was not honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// The IMAP folder's `UIDVALIDITY` changed: every UID was reassigned.
    UidValidity,
    /// Gmail answered `404` to `history.list`: the history id is too old.
    History,
    /// Graph no longer knows the delta token (`410`, `syncStateNotFound`).
    Delta,
}

/// One message as the provider holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    /// Its transport identity.
    pub identity: TransportIdentity,
    /// Its MIME bytes, at most the caller's bound.
    pub raw: Vec<u8>,
    /// The size the provider reports, when it does.
    pub size: Option<u64>,
    /// `raw` stops at the bound (or holds only the headers) because the message is larger.
    pub truncated: bool,
    /// When the provider received it: IMAP `INTERNALDATE`, Gmail `internalDate`, Graph
    /// `receivedDateTime`.
    pub received_at: Option<Timestamp>,
}

/// The identity of a message within its receive binding: `{uid_validity, uid}` for IMAP,
/// `{provider_message_id}` for Gmail and Graph — the transport identity the caller stores for
/// this mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransportIdentity {
    /// An IMAP message.
    Imap {
        /// The folder's `UIDVALIDITY` when the message was read.
        uid_validity: u32,
        /// The message's UID.
        uid: u32,
    },
    /// A Gmail message id or a Graph immutable id.
    Provider {
        /// The provider's id.
        provider_message_id: String,
    },
}

impl TransportIdentity {
    /// The canonical string of the transport identity the caller stores for this mailbox:
    /// `imap:<uid_validity>:<uid>` or `id:<provider_message_id>`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Imap { uid_validity, uid } => format!("imap:{uid_validity}:{uid}"),
            Self::Provider {
                provider_message_id,
            } => format!("id:{provider_message_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TransportIdentity;

    /// The transport identity is stored as JSON and deduplicated on its key, so its two shapes
    /// and its canonical string are a storage contract: `{uid_validity, uid}` for IMAP and
    /// `{provider_message_id}` for the APIs, and keys that never collide between the two.
    #[test]
    fn transport_identities_keep_their_stored_shapes_and_keys() {
        let imap = TransportIdentity::Imap {
            uid_validity: 7,
            uid: 42,
        };
        let api = TransportIdentity::Provider {
            provider_message_id: "18c2".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&imap).ok(),
            Some(serde_json::json!({"uid_validity": 7, "uid": 42}))
        );
        assert_eq!(
            serde_json::to_value(&api).ok(),
            Some(serde_json::json!({"provider_message_id": "18c2"}))
        );
        let parsed: TransportIdentity =
            serde_json::from_value(serde_json::json!({"uid_validity": 7, "uid": 42}))
                .expect("an IMAP identity");
        assert_eq!(parsed, imap);
        assert_eq!(imap.key(), "imap:7:42");
        assert_eq!(api.key(), "id:18c2");
    }
}
