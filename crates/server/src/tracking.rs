//! Tracking: opens, clicks and unsubscribes, observed through links inside the mail we send.
//!
//! A campaign message's HTML carries an open pixel (`/t/o/{token}`) and, in place of each of its
//! links, a click link (`/t/c/{token}`) that redirects to the original; every campaign message
//! carries an unsubscribe link (`/u/{token}`) in its `List-Unsubscribe` header and its footer.
//! The tracking role answers the first two from the token alone, with no database on the request
//! path (it spools the event and drains it later), and the unsubscribe page suppresses the
//! address the token names. What a token carries, and how it is signed, is [`token`]: the one
//! place these links are made and read. The rewriting of a body that inserts them is
//! `rendering::tracking_rewrite`.
//!
//! The rest of the folder serves and stores what those links report:
//!
//! - [`http`]: the tracking role's routes (`/t/o/{token}`, `/t/c/{token}`, the brand mark), the
//!   api's routes for recipients (`/u/{token}`, `/images/{workspace}/{file}`) and the `images`
//!   resource;
//! - [`spool`]: the role's local, durable buffer of events, written before a request is answered;
//! - [`drain`]: the role's task that moves spooled events into PostgreSQL, with the per-message
//!   rollup and the campaign counters' increments;
//! - [`unsubscribe`]: what an unsubscribe writes, and with which rights;
//! - [`images`]: images uploaded for mail, stored in the object store and served publicly, and
//!   the brand mark of the platform's own mail, served from the binary.

pub mod drain;
pub mod http;
pub mod images;
pub mod spool;
#[cfg(test)]
mod tests;
pub mod token;
pub mod unsubscribe;
