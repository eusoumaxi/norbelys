//! Mail protocol library used by Norbelys: submission, receive, webhooks, DSN/ARF/DKIM, OAuth
//! and relays.
//!
//! This library speaks to mail providers on behalf of the caller; it never decides anything
//! about workspaces, campaigns or persistence. The caller hands it a credential, a message or a
//! cursor, and gets back typed facts: whether a provider accepted a submission and what it
//! answered, the next page of a mailbox's new messages, a verified webhook event, the fields of
//! a bounce or complaint report. Credentials arrive per call and are kept no longer than the
//! session they open; cursors and receipts are returned for the caller to store.
//!
//! The modules:
//!
//! - [`submission`] and [`status`]: the shared vocabulary of a submission. An [`submission::Envelope`]
//!   goes in; a [`submission::Submission`] (accepted) or a [`submission::Rejection`] (transient,
//!   permanent or uncertain, with the protocol phase, the SMTP code, the RFC 3463 enhanced
//!   status, the scope it concerns and the provider's wait) comes out.
//! - [`smtp`] and [`net`]: SMTP submission for mailboxes, relays and the managed MTA, over
//!   pooled sessions with a deadline per protocol phase; host resolution under an address
//!   policy that keeps tenant-typed hosts away from non-public addresses.
//! - [`verify`]: bounded SMTP recipient checks against public MX hosts, without submitting mail.
//! - [`gmail`], [`graph`] and [`http`]: the Gmail API and Microsoft Graph, both for submission
//!   and for reading a mailbox; the one HTTP client policy (no automatic retries, no redirects).
//! - [`compose`]: MIME composition with the headers Norbelys owns (its own `Message-ID`,
//!   threading, RFC 8058 one-click unsubscribe, relay metadata).
//! - [`receive`] and [`imap`]: reading mailboxes through provider-owned cursors in bounded
//!   pages, with explicit resets when a cursor stops being honoured.
//! - [`webhooks`]: verification and parsing of provider callbacks (Mailgun, SendGrid, Amazon SES
//!   through SNS, the managed MTA through Standard Webhooks).
//! - [`dsn`], [`arf`] and [`inbound`]: delivery status notifications (RFC 3464), abuse reports
//!   (RFC 5965) and the header facts of any inbound message, parsed over `mail-parser`.
//! - [`dkim`]: DKIM signature verification (RFC 6376, RFC 8463), which proves who wrote a report
//!   and that a message returned in one was the caller's.
//! - [`oauth`]: the Google and Microsoft authorization-code flow for mailbox connections, token
//!   refresh, granted-scope checks and the refresh errors that mean a lost grant.
//! - [`relays`]: the relays' own HTTP APIs, read and never sent through: Amazon SES's account
//!   and identities (AWS Signature Version 4), SendGrid's key scopes and Email Activity,
//!   Mailgun's events.

pub mod arf;
pub mod compose;
pub mod dkim;
pub mod dsn;
pub mod gmail;
pub mod graph;
pub mod http;
pub mod imap;
pub mod inbound;
pub mod net;
pub mod oauth;
pub mod receive;
pub mod relays;
pub mod smtp;
pub mod status;
pub mod submission;
pub mod verify;
pub mod webhooks;

mod text;

#[cfg(test)]
mod testing;

// Compile-time proof of what the caller relies on: the resources it shares between tasks are
// `Send + Sync`, and the sessions it moves into a task are `Send`.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    const fn owned<T: Send>() {}
    shared::<smtp::SmtpPool>();
    shared::<http::HttpClient>();
    shared::<net::Connector>();
    shared::<verify::Verifier>();
    shared::<webhooks::ses::SnsCertificates>();
    shared::<webhooks::sendgrid::SendgridKey>();
    shared::<webhooks::mailgun::MailgunKey>();
    shared::<webhooks::norbelys::NorbelysKey>();
    owned::<smtp::SmtpSession>();
    owned::<imap::ImapSession>();
};

/// Compile-time proof that the futures the caller spawns are `Send`: each is built and handed to
/// a function that requires it. Never called.
#[expect(
    dead_code,
    reason = "a compile-time check of the futures' Send bound; never called"
)]
fn spawnable(
    pool: &smtp::SmtpPool,
    session: smtp::SmtpSession,
    mut mailbox: imap::ImapSession,
    connector: &net::Connector,
    client: &http::HttpClient,
    certificates: &webhooks::ses::SnsCertificates,
    topic: &webhooks::ses::SnsTopic,
) {
    fn send<T: Send>(_: T) {}
    let token = secrecy::SecretString::from(String::new());
    let deadline = tokio::time::Instant::now();
    let since = jiff::Timestamp::UNIX_EPOCH;
    let server = smtp::SmtpServer {
        host: "",
        port: 0,
        security: smtp::SmtpSecurity::Tls,
    };
    let auth = smtp::SmtpAuth::Password {
        username: "",
        password: &token,
    };
    send(pool.session(&server, &auth, deadline));
    send(pool.probe(&server, &auth, deadline));
    if let Ok(envelope) = submission::Envelope::new("", [""]) {
        send(session.submit(&envelope, b"", deadline));
    }
    send(gmail::send(client, &token, b"", deadline));
    send(gmail::changes(client, &token, "", None, since, 1, deadline));
    send(gmail::message(client, &token, "", 0, deadline));
    send(graph::send_mail(client, &token, b"", deadline));
    send(graph::changes(client, &token, "", None, since, 1, deadline));
    let imap_server = imap::ImapServer {
        host: "",
        port: 0,
        security: imap::ImapSecurity::Tls,
    };
    let imap_auth = imap::ImapAuth::Password {
        username: "",
        password: &token,
    };
    send(imap::connect(
        connector,
        &imap_server,
        &imap_auth,
        0,
        deadline,
    ));
    send(mailbox.changes("", None, since, 1));
    send(webhooks::ses::verify(
        topic,
        certificates,
        client,
        b"",
        since,
    ));
    let Ok(redirect_uri) = url::Url::parse("https://localhost/") else {
        return;
    };
    let app = oauth::App {
        client_id: String::new(),
        client_secret: token.clone(),
        redirect_uri,
    };
    send(oauth::refresh(
        client,
        &oauth::Provider::Google,
        &app,
        &token,
        &[],
        deadline,
    ));
}
