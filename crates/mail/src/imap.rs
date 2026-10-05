//! IMAP, for mailboxes connected with a password or an app password (and for Google and
//! Microsoft mailboxes read over IMAP with `XOAUTH2`): reading a folder through `UIDVALIDITY`
//! and the last UID read, fetching messages, finding the Sent folder (RFC 6154 special-use) and
//! searching it by `Message-ID` to settle an `uncertain` submission.
//!
//! A session is opened per poll or check with [`connect`] and every command ends by the
//! session's deadline, which the caller sets inside the lease that protects the poll, so no
//! request outlives the lease it was made under. Each command is also bounded by
//! [`STEP_TIMEOUT`].
//!
//! Reading (RFC 9051, <https://www.rfc-editor.org/rfc/rfc9051>):
//! - The cursor is `{uid_validity, last_uid}`. A UID identifies a message within a folder only
//!   while the folder's `UIDVALIDITY` stays the same; when it changes, every UID was reassigned,
//!   so the page restarts at the caller's `since` and reports a [`Reset`].
//! - Pages are found with `UID SEARCH UID n:m` over a numeric window of [`SEARCH_WINDOW`] UIDs,
//!   so even a large backlog never makes the server list a whole folder; UIDs are not dense, so
//!   an empty window just moves the cursor forward.
//! - Restarting "at `since`" uses `SEARCH SINCE` (date granularity, the server's time zone, so
//!   the search starts a day earlier) and then the exact `INTERNALDATE` of at most
//!   [`RESYNC_LIMIT`] candidates; older candidates beyond that bound are a visible gap.
//! - Messages are fetched with `BODY.PEEK[]<0.n>`: `PEEK` never sets `\Seen`, and the partial
//!   fetch reads at most `n` bytes of a large message. Folders are opened with `EXAMINE`
//!   (read-only).
//!
//! Security: the host is resolved by [`Connector`] under its address policy; TLS is implicit
//! (port 993) and verified against the host name; plaintext requires [`AddressPolicy::Any`]
//! (non-public hosts). `STARTTLS` on port 143 is not offered. Every literal the server
//! declares is bounded before it is buffered (see `bounded`).

mod bounded;

use std::collections::HashSet;
use std::fmt::Debug;
use std::time::Duration;

use async_imap::imap_proto::{Response, Status};
use async_imap::types::NameAttribute;
use futures_util::TryStreamExt as _;
use jiff::{SignedDuration, Timestamp};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, timeout_at};

use crate::net::{AddressPolicy, Connector, ResolveError};
use crate::receive::{Page, RawMessage, Reset, ResetReason, TransportIdentity};

/// The longest one IMAP command may take, within the session's deadline.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// How many UIDs one page's search covers.
pub const SEARCH_WINDOW: u32 = 1_000;
/// How many of the newest candidates a restart at `since` examines.
pub const RESYNC_LIMIT: usize = 500;
/// The most messages one [`ImapSession::fetch`] reads.
pub const MAX_FETCH: usize = 100;
/// The most folders [`ImapSession::sent_folder`] looks through.
const MAX_FOLDERS: usize = 2_000;

trait Stream: AsyncRead + AsyncWrite + Unpin + Send + Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug> Stream for T {}

/// Where a session connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImapServer<'a> {
    /// The host name (verified by TLS) or an IP address.
    pub host: &'a str,
    /// The port, 993 for implicit TLS.
    pub port: u16,
    /// How the session is secured.
    pub security: ImapSecurity,
}

/// How a session is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSecurity {
    /// TLS from the first byte (port 993).
    Tls,
    /// No TLS: only through a connector with [`AddressPolicy::Any`].
    Plain,
}

/// The credential a session logs in with.
#[derive(Clone, Copy)]
pub enum ImapAuth<'a> {
    /// `LOGIN` with a password or app password.
    Password {
        /// The login.
        username: &'a str,
        /// The password.
        password: &'a SecretString,
    },
    /// `AUTHENTICATE XOAUTH2` with an OAuth access token for IMAP.
    Xoauth2 {
        /// The mailbox the token belongs to.
        username: &'a str,
        /// The access token.
        token: &'a SecretString,
    },
}

impl Debug for ImapAuth<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, username) = match self {
            Self::Password { username, .. } => ("Password", username),
            Self::Xoauth2 { username, .. } => ("Xoauth2", username),
        };
        formatter
            .debug_struct(kind)
            .field("username", username)
            .finish_non_exhaustive()
    }
}

/// The receive cursor of an IMAP binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImapCursor {
    /// The folder's `UIDVALIDITY` the UIDs belong to.
    pub uid_validity: u32,
    /// The highest UID already read.
    pub last_uid: u32,
}

/// Why an IMAP session failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImapError {
    /// The host did not resolve, or resolves to an address the policy refuses.
    #[error("the IMAP host cannot be used: {0}")]
    Resolve(#[from] ResolveError),
    /// Plaintext was asked for through a connector that is not [`AddressPolicy::Any`].
    #[error("plaintext IMAP requires AddressPolicy::Any (non-public hosts)")]
    Plaintext,
    /// The TCP connection failed.
    #[error("the IMAP connection failed: {0}")]
    Connect(String),
    /// The TLS handshake failed (a wrong port, a certificate that does not match the host).
    #[error("the IMAP TLS handshake failed: {0}")]
    Tls(String),
    /// The server refused the login: the credential is lost (`[AUTHENTICATIONFAILED]`, or any
    /// `NO` to `LOGIN` or `AUTHENTICATE` that does not say `[UNAVAILABLE]`).
    #[error("the IMAP server refused the login: {0}")]
    Unauthorized(String),
    /// The server refused a command (`NO`, `BAD`), or the login for now (`[UNAVAILABLE]`).
    #[error("the IMAP server refused the command: {0}")]
    Refused(String),
    /// No answer before the deadline.
    #[error("the IMAP server did not answer before the deadline")]
    Timeout,
    /// The session broke: a reset, a malformed response, a response over its bound.
    #[error("the IMAP session failed: {0}")]
    Protocol(String),
    /// The folder reports no `UIDVALIDITY`, so it cannot be read by UID.
    #[error("the folder reports no UIDVALIDITY")]
    NoUidValidity,
}

/// An authenticated IMAP session.
pub struct ImapSession {
    session: async_imap::Session<Box<dyn Stream>>,
    deadline: Instant,
    max_message_bytes: usize,
    selected: Option<Selected>,
}

impl Debug for ImapSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImapSession")
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct Selected {
    folder: String,
    uid_validity: u32,
    uid_next: Option<u32>,
    exists: u32,
}

/// Opens a session: resolve, connect (TLS when asked), read the greeting, log in. Every step
/// ends by `deadline`, which also bounds every later command of the session. Messages fetched
/// through it are read up to `max_message_bytes`.
///
/// # Errors
///
/// [`ImapError::Unauthorized`] when the server refused the credential; the other variants for
/// what failed before.
pub async fn connect(
    connector: &Connector,
    server: &ImapServer<'_>,
    auth: &ImapAuth<'_>,
    max_message_bytes: usize,
    deadline: Instant,
) -> Result<ImapSession, ImapError> {
    if server.security == ImapSecurity::Plain && connector.policy() != AddressPolicy::Any {
        return Err(ImapError::Plaintext);
    }
    let address = step(deadline, connector.resolve(server.host, server.port)).await??;
    let tcp = step(deadline, tokio::net::TcpStream::connect(address))
        .await?
        .map_err(|error| ImapError::Connect(error.to_string()))?;
    let stream: Box<dyn Stream> = match server.security {
        ImapSecurity::Tls => {
            let name = rustls::pki_types::ServerName::try_from(server.host.to_owned())
                .map_err(|error| ImapError::Tls(error.to_string()))?;
            let tls = step(deadline, connector.tls().connect(name, tcp))
                .await?
                .map_err(|error| ImapError::Tls(error.to_string()))?;
            Box::new(tls)
        }
        ImapSecurity::Plain => Box::new(tcp),
    };
    let literal = max_message_bytes.max(64 * 1024);
    let stream: Box<dyn Stream> = Box::new(bounded::Bounded::new(stream, literal));
    let mut client = async_imap::Client::new(stream);
    let greeting = step(deadline, client.read_response())
        .await?
        .map_err(|error| ImapError::Protocol(error.to_string()))?
        .ok_or_else(|| {
            ImapError::Protocol("the server closed the connection before its greeting".to_owned())
        })?;
    if !matches!(
        greeting.parsed(),
        Response::Data {
            status: Status::Ok | Status::PreAuth,
            ..
        }
    ) {
        return Err(ImapError::Refused(
            "the server's greeting is not OK".to_owned(),
        ));
    }
    let session = match auth {
        ImapAuth::Password { username, password } => {
            step(deadline, client.login(username, password.expose_secret())).await?
        }
        ImapAuth::Xoauth2 { username, token } => {
            let valid = |text: &str| !text.is_empty() && !text.chars().any(char::is_control);
            if !valid(username) || !valid(token.expose_secret()) {
                return Err(ImapError::Unauthorized(
                    "the credential contains characters SASL cannot carry".to_owned(),
                ));
            }
            let authenticator = Xoauth2 {
                username,
                token,
                answered: false,
            };
            step(deadline, client.authenticate("XOAUTH2", authenticator)).await?
        }
    }
    .map_err(|(error, _client)| login_error(&error))?;
    Ok(ImapSession {
        session,
        deadline,
        max_message_bytes,
        selected: None,
    })
}

impl ImapSession {
    /// The folder the server marks `\Sent` (RFC 6154, <https://www.rfc-editor.org/rfc/rfc6154>),
    /// such as Gmail's `[Gmail]/Sent Mail`; `None` when the server marks none, and the caller
    /// then uses a configured folder.
    ///
    /// # Errors
    ///
    /// The `LIST` failed ([`ImapError`]).
    pub async fn sent_folder(&mut self) -> Result<Option<String>, ImapError> {
        let deadline = self.deadline;
        let session = &mut self.session;
        let names = step(deadline, async {
            let mut stream = session.list(Some(""), Some("*")).await?;
            let mut sent = None;
            let mut seen = 0usize;
            while let Some(name) = stream.try_next().await? {
                seen += 1;
                if sent.is_none()
                    && name
                        .attributes()
                        .iter()
                        .any(|attribute| matches!(attribute, NameAttribute::Sent))
                {
                    sent = Some(name.name().to_owned());
                }
                if seen >= MAX_FOLDERS {
                    break;
                }
            }
            Ok::<_, async_imap::error::Error>(sent)
        })
        .await?
        .map_err(|error| command_error(&error))?;
        self.drain();
        Ok(names)
    }

    /// The next page of messages in `folder` after `cursor`, at most `limit` UIDs, oldest
    /// first. Without a cursor, or when the folder's `UIDVALIDITY` changed, the page starts at
    /// the first message received at or after `since` (the change is reported as a [`Reset`]).
    ///
    /// # Errors
    ///
    /// A credential, command or deadline failed as a shared [`crate::receive::Error`];
    /// a folder without `UIDVALIDITY` is an invalid response.
    pub async fn changes(
        &mut self,
        folder: &str,
        cursor: Option<&ImapCursor>,
        since: Timestamp,
        limit: u32,
    ) -> Result<Page<u32, ImapCursor>, crate::receive::Error> {
        let result: Result<Page<u32, ImapCursor>, ImapError> = async {
            let limit = usize::try_from(limit.clamp(1, SEARCH_WINDOW)).unwrap_or(1);
            let selected = self.examine(folder).await?;
            let newest = self.newest(&selected).await?;
            let (start, reset) = match cursor {
                Some(cursor) if cursor.uid_validity == selected.uid_validity => {
                    (cursor.last_uid, None)
                }
                Some(_) => {
                    let (start, truncated) = self.position_since(since, newest).await?;
                    (
                        start,
                        Some(Reset {
                            reason: ResetReason::UidValidity,
                            since,
                            truncated,
                        }),
                    )
                }
                None => (self.position_since(since, newest).await?.0, None),
            };
            let page = |ids: Vec<u32>, last_uid: u32, more: bool| Page {
                ids,
                cursor: ImapCursor {
                    uid_validity: selected.uid_validity,
                    last_uid,
                },
                more,
                reset,
            };
            if start >= newest {
                return Ok(page(Vec::new(), start, false));
            }
            let end = start.saturating_add(SEARCH_WINDOW).min(newest);
            let found = self
                .search(&format!("UID {}:{end}", start.saturating_add(1)))
                .await?;
            let mut uids: Vec<u32> = found
                .into_iter()
                .filter(|uid| *uid > start && *uid <= end)
                .collect();
            uids.sort_unstable();
            if uids.len() > limit {
                uids.truncate(limit);
                let last = uids.last().copied().unwrap_or(start);
                return Ok(page(uids, last, true));
            }
            Ok(page(uids, end, end < newest))
        }
        .await;
        result.map_err(Into::into)
    }

    /// The messages `uids` of `folder` (at most [`MAX_FETCH`]), each read up to the session's
    /// `max_message_bytes`, oldest first. A UID the server no longer has is simply missing from
    /// the result (it was expunged).
    ///
    /// # Errors
    ///
    /// A command failed ([`ImapError`]), more than [`MAX_FETCH`] UIDs were asked for, or the
    /// folder's `UIDVALIDITY` is no longer `uid_validity` (the page must be read again).
    pub async fn fetch(
        &mut self,
        folder: &str,
        uid_validity: u32,
        uids: &[u32],
    ) -> Result<Vec<RawMessage>, crate::receive::Error> {
        let result: Result<Vec<RawMessage>, ImapError> = async {
            if uids.is_empty() {
                return Ok(Vec::new());
            }
            if uids.len() > MAX_FETCH {
                return Err(ImapError::Protocol(format!(
                    "at most {MAX_FETCH} messages are fetched at once"
                )));
            }
            let selected = match &self.selected {
                Some(selected) if selected.folder == folder => selected.clone(),
                _ => self.examine(folder).await?,
            };
            if selected.uid_validity != uid_validity {
                return Err(ImapError::Refused(
                    "the folder's UIDVALIDITY changed; read the page again".to_owned(),
                ));
            }
            let set = uids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "(UID INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.{}>)",
                self.max_message_bytes
            );
            let wanted: HashSet<u32> = uids.iter().copied().collect();
            let max = self.max_message_bytes;
            let deadline = self.deadline;
            let session = &mut self.session;
            let mut messages = step(deadline, async {
                let mut stream = session.uid_fetch(set, query).await?;
                let mut messages = Vec::new();
                while let Some(item) = stream.try_next().await? {
                    let Some(uid) = item.uid.filter(|uid| wanted.contains(uid)) else {
                        continue;
                    };
                    let body = item.body().unwrap_or_default();
                    let mut raw = body.to_vec();
                    raw.truncate(max);
                    let size = item.size.map(u64::from);
                    let truncated = size
                        .is_some_and(|size| u64::try_from(raw.len()).is_ok_and(|read| size > read));
                    let received_at = item
                        .internal_date()
                        .and_then(|date| Timestamp::from_second(date.timestamp()).ok());
                    messages.push(RawMessage {
                        identity: TransportIdentity::Imap { uid_validity, uid },
                        raw,
                        size,
                        truncated,
                        received_at,
                    });
                }
                Ok::<_, async_imap::error::Error>(messages)
            })
            .await?
            .map_err(|error| command_error(&error))?;
            self.drain();
            messages.sort_by_key(|message| match message.identity {
                TransportIdentity::Imap { uid, .. } => uid,
                TransportIdentity::Provider { .. } => 0,
            });
            Ok(messages)
        }
        .await;
        result.map_err(Into::into)
    }

    /// Whether `folder` holds a message whose `Message-ID` is `internet_message_id`
    /// (`UID SEARCH HEADER Message-ID`): the read-only way to settle an `uncertain`
    /// submission. A server that copies submitted mail to the Sent folder makes this proof of
    /// acceptance; finding nothing proves nothing (a generic SMTP server need not copy anything).
    ///
    /// # Errors
    ///
    /// A command failed ([`ImapError`]), or the id is not printable ASCII.
    pub async fn find_message_id(
        &mut self,
        folder: &str,
        internet_message_id: &str,
    ) -> Result<bool, crate::receive::Error> {
        let result: Result<bool, ImapError> = async {
            let id = internet_message_id.trim();
            if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(ImapError::Protocol(
                    "a Message-ID is printable ASCII".to_owned(),
                ));
            }
            self.examine(folder).await?;
            let quoted = format!("\"{}\"", id.replace('\\', "\\\\").replace('"', "\\\""));
            Ok(!self
                .search(&format!("HEADER Message-ID {quoted}"))
                .await?
                .is_empty())
        }
        .await;
        result.map_err(Into::into)
    }

    /// Ends the session with `LOGOUT`, bounded by a few seconds; a failure changes nothing.
    pub async fn logout(mut self) {
        let _logged_out = tokio::time::timeout(Duration::from_secs(5), self.session.logout()).await;
    }

    async fn examine(&mut self, folder: &str) -> Result<Selected, ImapError> {
        let mailbox = step(self.deadline, self.session.examine(folder))
            .await?
            .map_err(|error| command_error(&error))?;
        let uid_validity = mailbox.uid_validity.ok_or(ImapError::NoUidValidity)?;
        let selected = Selected {
            folder: folder.to_owned(),
            uid_validity,
            uid_next: mailbox.uid_next,
            exists: mailbox.exists,
        };
        self.selected = Some(selected.clone());
        self.drain();
        Ok(selected)
    }

    /// The highest UID in the folder: `UIDNEXT − 1` when the server said, else the last
    /// message's UID (`UID FETCH *`), or 0 for an empty folder.
    async fn newest(&mut self, selected: &Selected) -> Result<u32, ImapError> {
        if let Some(next) = selected.uid_next {
            return Ok(next.saturating_sub(1));
        }
        if selected.exists == 0 {
            return Ok(0);
        }
        let deadline = self.deadline;
        let session = &mut self.session;
        let highest = step(deadline, async {
            let mut stream = session.uid_fetch("*", "(UID)").await?;
            let mut highest = 0;
            while let Some(item) = stream.try_next().await? {
                highest = highest.max(item.uid.unwrap_or_default());
            }
            Ok::<_, async_imap::error::Error>(highest)
        })
        .await?
        .map_err(|error| command_error(&error))?;
        self.drain();
        Ok(highest)
    }

    /// Where a restart at `since` begins: the UID before the oldest message whose
    /// `INTERNALDATE` is at or after `since`, among the [`RESYNC_LIMIT`] newest candidates of
    /// `SEARCH SINCE` (a day earlier, for the server's time zone). Also whether older
    /// candidates were left unexamined while the oldest examined one was still recent enough:
    /// a visible gap.
    async fn position_since(
        &mut self,
        since: Timestamp,
        newest: u32,
    ) -> Result<(u32, bool), ImapError> {
        let day = since
            .checked_sub(SignedDuration::from_hours(24))
            .unwrap_or(since);
        let found = self.search(&format!("SINCE {}", imap_date(day))).await?;
        let mut candidates: Vec<u32> = found.into_iter().collect();
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let Some(&latest) = candidates.first() else {
            return Ok((newest, false));
        };
        let more_than_examined = candidates.len() > RESYNC_LIMIT;
        candidates.truncate(RESYNC_LIMIT);
        let set = candidates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let deadline = self.deadline;
        let session = &mut self.session;
        let dated = step(deadline, async {
            let mut stream = session.uid_fetch(set, "(UID INTERNALDATE)").await?;
            let mut dated = Vec::new();
            while let Some(item) = stream.try_next().await? {
                if let (Some(uid), Some(date)) = (item.uid, item.internal_date()) {
                    dated.push((uid, date.timestamp()));
                }
            }
            Ok::<_, async_imap::error::Error>(dated)
        })
        .await?
        .map_err(|error| command_error(&error))?;
        self.drain();
        let since_second = since.as_second();
        let recent = dated
            .iter()
            .filter(|(_, second)| *second >= since_second)
            .map(|(uid, _)| *uid)
            .min();
        let oldest_examined = candidates.last().copied();
        let gap = more_than_examined
            && oldest_examined.is_some_and(|oldest| {
                dated
                    .iter()
                    .any(|(uid, second)| *uid == oldest && *second >= since_second)
            });
        Ok(match recent {
            Some(first) => (first.saturating_sub(1), gap),
            None => (latest, false),
        })
    }

    async fn search(&mut self, query: &str) -> Result<HashSet<u32>, ImapError> {
        let found = step(self.deadline, self.session.uid_search(query))
            .await?
            .map_err(|error| command_error(&error))?;
        self.drain();
        Ok(found)
    }

    /// Drops the unsolicited responses the session queued (`async-imap` keeps up to 100).
    fn drain(&mut self) {
        while self.session.unsolicited_responses.try_recv().is_ok() {}
    }
}

/// The IMAP `date` form (`1-Oct-2026`) of an instant's UTC day.
fn imap_date(at: Timestamp) -> String {
    let date = at.to_zoned(jiff::tz::TimeZone::UTC).date();
    let month = match date.month() {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    };
    format!("{}-{month}-{}", date.day(), date.year())
}

/// `future` bounded by [`STEP_TIMEOUT`] and `deadline`.
async fn step<F: Future>(deadline: Instant, future: F) -> Result<F::Output, ImapError> {
    let at = Instant::now()
        .checked_add(STEP_TIMEOUT)
        .map_or(deadline, |at| deadline.min(at));
    timeout_at(at, future).await.map_err(|_| ImapError::Timeout)
}

fn login_error(error: &async_imap::error::Error) -> ImapError {
    match error {
        async_imap::error::Error::No(text) if text.contains("UNAVAILABLE") => {
            ImapError::Refused("remote_unavailable".into())
        }
        async_imap::error::Error::No(_) => ImapError::Unauthorized("authentication_refused".into()),
        other => command_error(other),
    }
}

fn command_error(error: &async_imap::error::Error) -> ImapError {
    match error {
        async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_) => {
            ImapError::Refused("command_refused".into())
        }
        _ => ImapError::Protocol("protocol_error".into()),
    }
}

/// SASL `XOAUTH2` for `AUTHENTICATE`: the first challenge is answered with the token; a second
/// one carries the server's error, answered with an empty line so the server sends its `NO`.
struct Xoauth2<'a> {
    username: &'a str,
    token: &'a SecretString,
    answered: bool,
}

impl async_imap::Authenticator for Xoauth2<'_> {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if self.answered {
            return String::new();
        }
        self.answered = true;
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.username,
            self.token.expose_secret()
        )
    }
}

#[cfg(test)]
mod tests;
