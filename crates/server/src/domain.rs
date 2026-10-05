//! Pure domain types and decisions: the rules of the product, separate from how they are
//! stored or served.
//!
//! Nothing here performs I/O, reads the environment or the clock: decisions take facts and
//! `now` as arguments. The one exception is minting a new id (`ids`): a UUIDv7 reads the clock
//! and a random source by design, since an id's instant is when its row was created. Storage
//! encodings of these types live in `db/types.rs`.

pub mod ai;
pub mod allocation;
pub mod analytics;
pub mod campaigns;
pub mod email;
pub mod identity;
pub mod ids;
pub mod images;
pub mod import;
pub mod inbox;
pub mod messages;
pub mod oauth;
pub mod people;
pub mod policy;
pub mod preflight;
pub mod receipts;
pub mod retry;
pub mod schedule;
pub mod scope;
pub mod segments;
pub mod senders;
pub mod suppressions;
pub mod telemetry;
pub mod time;
pub mod tracking;

pub mod webhooks;
