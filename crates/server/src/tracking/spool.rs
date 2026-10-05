//! The tracking role's spool: every open and click is written here before the request is
//! answered, and the drain moves events into PostgreSQL. The tracking routes therefore never wait
//! on the database, and nothing they answered is lost when the database is unavailable or the
//! process stops.
//!
//! The machinery (durability, the one writer thread, group commit, corrupt records set aside, the
//! bound) is the crate's one spool, [`crate::spool`]; this module names what the tracking role
//! keeps in it: an [`Event`], written as versioned JSON (`tracking.sqlite` in the role's spool
//! directory), so a later format can be read beside this one. The file holds recipients' hashed
//! addresses and their clients' `User-Agent`, which is why the spool's directory is private.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::domain::tracking::{ActorClass, EventKind};
pub use crate::spool::{Limits, SpoolError};

/// The tracking role's spool of events.
pub type Spool = crate::spool::Spool<Event>;
/// A batch of events handed to the drain.
pub type Batch = crate::spool::Batch<Event>;

/// The format of the records this module writes.
const VERSION: u8 = 1;

/// One observed open or click, as the tracking routes record it and the drain stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Minted when the request is answered; with `workspace` and `occurred_at`, the event's key
    /// in `tracking_events`, so a batch stored twice inserts nothing the second time.
    pub id: Uuid,
    /// The workspace the verified token names.
    pub workspace: WorkspaceId,
    /// The message the verified token names.
    pub message: Id<Message>,
    pub kind: EventKind,
    /// A click's link position in the message's body, when it fits the stored range.
    pub link: Option<i16>,
    /// The SHA-256 of a click's destination.
    pub url_hash: Option<Vec<u8>>,
    pub actor: ActorClass,
    /// The keyed hash of the client's address (`crypto::Keys::hash_address`).
    pub ip_hash: Option<Vec<u8>>,
    /// The client's `User-Agent`, bounded.
    pub user_agent: Option<String>,
    pub occurred_at: Timestamp,
}

/// An event as it is written in the spool: versioned, so a later format can be read beside this
/// one.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    v: u8,
    id: Uuid,
    workspace: Uuid,
    message: Uuid,
    kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url_hash: Option<Vec<u8>>,
    actor: ActorClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ip_hash: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_agent: Option<String>,
    occurred_at: Timestamp,
}

impl crate::spool::Record for Event {
    const NAME: &'static str = "tracking";

    fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&Record {
            v: VERSION,
            id: self.id,
            workspace: self.workspace.uuid(),
            message: self.message.uuid(),
            kind: self.kind,
            link: self.link,
            url_hash: self.url_hash.clone(),
            actor: self.actor,
            ip_hash: self.ip_hash.clone(),
            user_agent: self.user_agent.clone(),
            occurred_at: self.occurred_at,
        })
        .map_err(|error| error.to_string())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let record: Record = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if record.v != VERSION {
            return Err(format!("the record's version {} is unknown", record.v));
        }
        Ok(Self {
            id: record.id,
            // The token that named this workspace was verified before the event was spooled.
            workspace: WorkspaceId::trusted(record.workspace),
            message: Id::from_uuid(record.message),
            kind: record.kind,
            link: record.link,
            url_hash: record.url_hash,
            actor: record.actor,
            ip_hash: record.ip_hash,
            user_agent: record.user_agent,
            occurred_at: record.occurred_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use crate::spool::Record as _;

    /// An event written by this format reads back as itself, and a record of an unknown version
    /// is refused (the spool then sets it aside instead of misreading it).
    #[test]
    fn events_round_trip_and_unknown_versions_are_refused() {
        let event = Event {
            id: Uuid::now_v7(),
            workspace: WorkspaceId::trusted(Uuid::now_v7()),
            message: Id::new(),
            kind: EventKind::Click,
            link: Some(3),
            url_hash: Some(crypto::sha256(b"https://example.com")),
            actor: ActorClass::Human,
            ip_hash: Some(vec![7; 32]),
            user_agent: Some("Mozilla/5.0 (test)".to_owned()),
            occurred_at: crate::process::now(),
        };
        let bytes = event.encode().unwrap();
        assert_eq!(Event::decode(&bytes).unwrap(), event);
        let mut future: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        future["v"] = serde_json::json!(2);
        let refused = Event::decode(&serde_json::to_vec(&future).unwrap()).unwrap_err();
        assert!(refused.contains("version 2"), "{refused}");
    }
}
