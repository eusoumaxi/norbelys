//! Delivery: getting a message from its row to a provider's acceptance, one fenced attempt at a
//! time, and recording what is learnt about its fate afterwards.
//!
//! The modules, in the order of a message's life:
//!
//! | Module | What it holds |
//! |---|---|
//! | [`accept`] | the one creation contract every message goes through (direct, reply, a campaign step's, transactional): its thread, row and queue row, `message.queued`, our Message-ID and its correlation |
//! | [`claim`] | which connections are due and which of their queued messages a sender takes, under a lease, with their budget reserved |
//! | [`limits`] | the in-process token buckets of the providers' published short-window limits, charged per submission and per mailbox read |
//! | [`preflight`] | each envelope address's syntax and DNS route, cached per workspace and address, never a mailbox probe; also `POST /preflight` |
//! | [`start`] | the last decision before a submission, under the locks every removal also takes, and the submission marker |
//! | [`submit`] | the transports (Gmail API, Microsoft Graph, SMTP to mailboxes, relays and the managed MTA) and how a provider's answer becomes facts |
//! | [`finish`] | what a connection's claimed messages came to, recorded in one micro-batch with once-only settlement |
//! | [`evidence`] | the one recorder of observations about a message's fate (answers, callbacks, reports, a person's decision) and of their effects on the message and its recipients |
//! | [`recover`] | lost leases taken back: queued again before the submission marker, `uncertain` after it |
//! | [`reconcile`] | settling an `uncertain` message from evidence (a person, the mailbox's Sent folder, a provider's events), never by sending it again |
//! | [`expire`] | ending queued messages without a submission: past their deadline (`delivery.expire`), or cancelled by a person |
//! | [`projection`] | how the next twelve five-minute slots are loaded with known sends, as a gauge |
//! | [`http`] | the `messages` and `delivery_events` resources under `/v1` |
//!
//! The delivery runner ([`runner`]) drives a claimed message through rendering
//! (`crate::rendering`: the MIME bytes and the envelope), preflight, a session, its Start, its
//! submission and the Finish.

pub mod accept;
pub mod attachments;
pub mod claim;
pub mod content;
#[cfg(test)]
mod content_tests;
pub mod evidence;
pub mod expire;
#[cfg(test)]
mod fanout_tests;
pub mod finish;
pub mod http;
pub mod limits;
pub mod preflight;
pub mod projection;
pub mod reconcile;
pub mod recover;
pub(crate) mod runner;
pub mod start;
pub mod submit;
