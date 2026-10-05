//! The inbox: reading the mailboxes a workspace connected, and what was read.
//!
//! A connection's mailbox is read through its receive bindings (one per folder,
//! `senders::bindings`). The inbox role polls each binding under a fenced lease ([`poll`]),
//! stores what it reads once (deduplicated by transport identity), and for each new message
//! decides three things, judged before anything is written; the stop rules then commit each in a
//! transaction of its own and everything else in the page's transaction (see [`poll`]):
//!
//! - **What it concerns** ([`correlate`]): our thread and outbound message, from our own signed
//!   Message-ID or, for a provider that replaced it, from the directory of provider ids under
//!   strict conditions.
//! - **What it is** (`domain::inbox`, by authority: a delivery report, an abuse report, an
//!   automatic reply, an unsubscribe request, a notice, a human reply), and the evidence a report
//!   is ([`classify`]), recorded through the one evidence path (`delivery::evidence`).
//! - **What it changes**: a person's answer ends their enrollments under the campaign's stop
//!   rules; the thread shows the new activity; a notice waits for a person's review; the customer
//!   hears `inbound_message.received`; AI is asked when the rules were silent and the workspace
//!   wants it (`inbox.classify`).
//!
//! The API ([`http`]) serves threads and inbound messages: their reads, a thread's status, a
//! person's correction of a classification, and the review of what a notice proposed. Replies
//! are sent through `POST /v1/messages` with a `thread_id` (`delivery::http`), from the thread's
//! own identity.

pub mod classify;
pub mod correlate;
pub mod http;
pub mod poll;
#[cfg(test)]
mod tests;

pub(crate) mod runner;
