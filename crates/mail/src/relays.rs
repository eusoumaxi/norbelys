//! The relays' own HTTP APIs, which Norbelys reads and never sends through (submission to a relay
//! is SMTP, [`crate::smtp`]):
//!
//! - [`ses`]: Amazon SES's account and identity reads for a connection's daily check, signed with
//!   AWS Signature Version 4 ([`sigv4`]);
//! - [`sendgrid`]: the permissions of a SendGrid API key, and its Email Activity for the
//!   reconciliation of messages whose submission answer was lost;
//! - [`mailgun`]: a Mailgun domain's events, for the reconciliation of the events its webhooks
//!   did not deliver (Mailgun never retries a delivery notification).
//!
//! The server calls them with a credential a connection holds and stores what they return;
//! nothing here keeps anything. Every call goes through the one HTTP client policy
//! ([`crate::http`]: no automatic retries, no redirects, bounded bodies, a deadline per request)
//! and fails with a [`crate::http::ApiError`]. These are low-rate keys, called a few times a day
//! per connection or hourly per webhook, so no limiter of ours holds them: a `429` is the
//! provider's answer to wait.

pub mod mailgun;
pub mod sendgrid;
pub mod ses;
pub mod sigv4;

#[cfg(test)]
mod tests;
