//! SMTP submission: customer mailboxes (Gmail, Exchange Online, any SMTP server), the relays'
//! SMTP endpoints (Amazon SES, SendGrid, Mailgun) and the managed MTA. Built on `lettre`'s async
//! connection; every command awaits its reply (no pipelining).
//!
//! A submission takes two steps, because while a delivery is in flight the caller acquires the
//! session under a short lease, and only then fixes the submission's budget and marks the
//! message as started:
//! 1. [`SmtpPool::session`] reuses a parked session of the same credential and security context
//!    (after testing it with `NOOP`) or opens one: connect, `EHLO`, `STARTTLS`, `AUTH`.
//! 2. [`SmtpSession::submit`] runs `MAIL FROM`, `RCPT TO` and `DATA` within the budget.
//!
//! Deadlines per phase: connect 10 s; `EHLO`, `STARTTLS`, `AUTH`, `MAIL FROM` and each `RCPT TO`
//! 30 s; the `DATA` command 30 s; then [`CONTENT_TIMEOUT`] (150 s) covers writing the content
//! and awaiting the final reply as one deadline (historically budgeted as 30 s write + 120 s
//! reply). Every step also ends at the caller's deadline, the whole submission's budget (300 s
//! by default, fixed on the caller's monotonic clock so that it always ends before the lease
//! that protects the message). `DATA` is sent only while at least [`DATA_RESERVE`] of that
//! budget remains, so the budget can never cut a submission whose outcome would then be unknown;
//! otherwise the submission stops before `DATA` as `transient`. RFC 5321 §4.5.3.2
//! (<https://www.rfc-editor.org/rfc/rfc5321#section-4.5.3.2>) lists far longer server-side
//! timeouts; these are shorter on purpose, trading a few retries for sockets not held.
//!
//! Reply mapping (see [`reply`]): `250` after the content is `accepted`; `4xx` is `transient`
//! and `5xx` is `permanent`, with the phase, the enhanced status and a scope; throttle answers
//! and documented quota refusals are `transient` with the scope they name. A failure without
//! the final reply is `uncertain` only once the server answered `354` to `DATA` and the content
//! was being sent: a lost reply to any earlier command cannot hide an acceptance, so it is
//! `transient` for its phase.
//!
//! The pool is built here over `lettre`'s connection type, because `lettre`'s pool exposes only
//! a one-call `send_raw`, which can express neither the phase deadlines, nor the `DATA` rule,
//! nor per-recipient refusals, nor which phase failed. It keeps `lettre`'s pool semantics: a
//! parked session is tested with `NOOP` before reuse, an idle one is closed after the configured
//! idle timeout (60 s by default, as in `lettre`), and any negative reply closes the session
//! instead of parking it (no `RSET`), so a throttled relay costs a reconnect.

mod auth;
mod pool;
mod protocol;
pub mod reply;
mod session;
mod transaction;

use std::time::Duration;

use secrecy::SecretString;

pub use pool::{SessionCap, SmtpPool, SmtpSession};
pub use reply::ReplyId;

/// The connect phase's TCP deadline.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The deadline of one command: `EHLO`, `STARTTLS`, `AUTH`, `MAIL FROM`, each `RCPT TO`, `DATA`.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// One deadline covering the content write and the final reply after it (150 s).
pub const CONTENT_TIMEOUT: Duration = Duration::from_secs(150);
/// The budget that must remain before `DATA` is sent: the command, the content and the reply.
pub const DATA_RESERVE: Duration = Duration::from_secs(180);
/// The deadline of the `NOOP` that tests a parked session before reuse.
pub const NOOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a session connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SmtpServer<'a> {
    /// The host name (verified by TLS) or an IP address.
    pub host: &'a str,
    /// The port: 465 for implicit TLS, 587 for `STARTTLS`.
    pub port: u16,
    /// How the session is secured.
    pub security: SmtpSecurity,
}

/// How a session is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SmtpSecurity {
    /// TLS from the first byte (port 465).
    Tls,
    /// Plaintext greeting, then a required `STARTTLS` (port 587); never downgraded.
    StartTls,
    /// No TLS: only through a connector with [`crate::net::AddressPolicy::Any`] (non-public
    /// hosts such as loopback or RFC 1918).
    Plain,
}

/// The credential a session authenticates with.
#[derive(Clone, Copy)]
pub enum SmtpAuth<'a> {
    /// A password or app password, by `PLAIN`, else `LOGIN`.
    Password {
        /// The login (usually the mailbox address; `apikey` for SendGrid).
        username: &'a str,
        /// The password.
        password: &'a SecretString,
    },
    /// An OAuth access token for SMTP (`https://mail.google.com/`, Outlook's `SMTP.Send`), by
    /// SASL `XOAUTH2`.
    Xoauth2 {
        /// The mailbox the token belongs to.
        username: &'a str,
        /// The access token.
        token: &'a SecretString,
    },
}

impl std::fmt::Debug for SmtpAuth<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { username, .. } => formatter
                .debug_struct("Password")
                .field("username", username)
                .finish_non_exhaustive(),
            Self::Xoauth2 { username, .. } => formatter
                .debug_struct("Xoauth2")
                .field("username", username)
                .finish_non_exhaustive(),
        }
    }
}

/// A pool's limits. The caller typically keeps one pool per transport kind (mailboxes, each
/// relay, the managed MTA), each with its own limits: a mailbox that sends infrequently has
/// sessions that rarely survive the idle timeout, while a relay's traffic is continuous and its
/// provider asks for rotation.
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    /// Sessions open at once per credential, parked or in use. `lettre`'s own pool bounds only
    /// parked sessions and opens a new one whenever none is parked; this bound keeps the sockets
    /// per credential within what the provider allows (Exchange Online allows 3 submitting
    /// connections per mailbox; SendGrid 10,000 per server).
    pub max_open: usize,
    /// Sessions kept parked per credential.
    pub max_idle: usize,
    /// How long a parked session is kept: 60 s for mailboxes, below SES's "about 10 seconds"
    /// for SES.
    pub idle_timeout: Duration,
    /// Messages after which a session is closed instead of parked (SES asks for rotation;
    /// SendGrid allows 5,000 per connection).
    pub max_messages: u32,
    /// Age after which a session is closed instead of parked.
    pub max_age: Duration,
    /// Where the final `250` names the accepted message.
    pub reply_id: ReplyId,
}

impl Default for PoolConfig {
    /// A mailbox pool: `lettre`'s 60 s idle timeout, one parked session per credential.
    fn default() -> Self {
        Self {
            max_open: 4,
            max_idle: 1,
            idle_timeout: Duration::from_secs(60),
            max_messages: 1_000,
            max_age: Duration::from_secs(600),
            reply_id: ReplyId::None,
        }
    }
}
