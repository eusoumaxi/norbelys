//! SMTP session ownership: credential keys, capacity permits, parked-session reuse and eviction.
//!
//! Opening an SMTP session costs a TCP and TLS handshake, `EHLO` and `AUTH`; a relay with
//! continuous traffic reuses its sessions, while a mailbox that sends infrequently mostly opens
//! a fresh one (servers close idle sessions on their own schedule).
//!
//! Invariants:
//! - Sessions are keyed by a SHA-256 digest of the server, the security mode and the
//!   credential, so a changed password or a refreshed token never reuses another credential's
//!   session, and no secret is kept as a key.
//! - At most `max_open` sessions per key are open at once, parked or in use: each holds a
//!   permit of the key's semaphore until it is closed.
//! - Pools made with [`SmtpPool::capped`] share one [`SessionCap`], so each SMTP worker process
//!   holds at most that many sockets across all its pools. Each open session, parked or in use,
//!   holds a place; when none is free, the longest-parked session of any pool sharing the cap is
//!   closed to free one (an idle pool gives way to a busy one), then the request waits for a
//!   place until its deadline.
//! - A session is parked only after a transaction whose every reply was positive, while under
//!   `max_messages` and `max_age`; anything else closes it (`QUIT`, then the socket), off the
//!   caller's path. A parked session is tested with `NOOP` before reuse and closed once idle
//!   for `idle_timeout`.
//! - No lock is held across an `.await`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use aws_lc_rs::digest;
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use secrecy::ExposeSecret as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, timeout_at};

use super::auth::check_credential;
use super::protocol::within;
use super::{COMMAND_TIMEOUT, NOOP_TIMEOUT, PoolConfig, SmtpAuth, SmtpSecurity, SmtpServer};
use crate::net::Connector;
use crate::submission::{Cause, Envelope, Failure, Phase, Rejection, Scope, Submission};

/// How many hosts' TLS parameters are cached; each holds a platform verifier.
const TLS_CACHE: usize = 256;
/// How often parked sessions are swept for their idle timeout.
const SWEEP_EVERY: Duration = Duration::from_secs(1);
/// How long closing a session (`QUIT`) may take, off the caller's path.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

type Key = Vec<u8>;

/// SMTP sessions keyed by credential and security context. Cheap to clone; the caller typically
/// keeps one per transport kind, each with its own [`PoolConfig`].
#[derive(Clone)]
pub struct SmtpPool {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SmtpPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpPool")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

/// The SMTP sessions one process may hold open at once, shared by its capped pools
/// ([`SmtpPool::capped`]): a place per open session, parked or in use, and the pools it may close
/// an idle session of to free one.
pub struct SessionCap {
    places: Arc<Semaphore>,
    pools: Mutex<Vec<Weak<Inner>>>,
}

impl std::fmt::Debug for SessionCap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCap")
            .field("free", &self.places.available_permits())
            .finish_non_exhaustive()
    }
}

impl SessionCap {
    /// A cap of `sessions` open sessions (at least one).
    #[must_use]
    pub fn new(sessions: usize) -> Arc<Self> {
        Arc::new(Self {
            places: Arc::new(Semaphore::new(sessions.clamp(1, Semaphore::MAX_PERMITS))),
            pools: Mutex::default(),
        })
    }

    /// Closes the longest-parked session of all the pools sharing the cap, so its place frees:
    /// the session idle the longest is the one least likely to be reused.
    fn evict_oldest(&self) {
        let pools: Vec<Arc<Inner>> = self
            .pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let oldest = pools
            .iter()
            .filter_map(|pool| pool.oldest_parked().map(|parked_at| (pool, parked_at)))
            .min_by_key(|(_, parked_at)| *parked_at);
        if let Some(idle) = oldest.and_then(|(pool, _)| pool.take_oldest_parked()) {
            close(idle);
        }
    }
}

struct Inner {
    config: PoolConfig,
    connector: Connector,
    /// The cap this pool shares with the other pools of its process, if any.
    cap: Option<Arc<SessionCap>>,
    hello: ClientId,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    parked: HashMap<Key, Vec<Parked>>,
    slots: HashMap<Key, Arc<Semaphore>>,
    tls: HashMap<String, TlsParameters>,
    swept_at: Option<Instant>,
}

struct Parked {
    live: Live,
    parked_at: Instant,
}

/// An open, authenticated session, its permit and, in a capped pool, its place.
struct Live {
    connection: AsyncSmtpConnection,
    permit: OwnedSemaphorePermit,
    place: Option<OwnedSemaphorePermit>,
    opened_at: Instant,
    sent: u32,
}

/// An authenticated session, ready for one transaction. Dropping it without submitting parks
/// it again, so a caller that decides not to submit after all (the message was paused or
/// suppressed in the meantime) loses nothing.
pub struct SmtpSession {
    live: Option<Live>,
    key: Key,
    pool: Arc<Inner>,
    clean: bool,
}

impl std::fmt::Debug for SmtpSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpSession")
            .field("clean", &self.clean)
            .finish_non_exhaustive()
    }
}

impl SmtpPool {
    /// A pool connecting through `connector`, the EHLO name being the host's name.
    #[must_use]
    pub fn new(connector: Connector, config: PoolConfig) -> Self {
        Self::build(connector, config, None)
    }

    /// A pool like [`SmtpPool::new`] whose open sessions also take a place of `cap`, which every
    /// capped pool of the process shares: the process then never holds more SMTP sockets than the
    /// cap has places, whatever the mix of credentials and transports.
    #[must_use]
    pub fn capped(connector: Connector, config: PoolConfig, cap: &Arc<SessionCap>) -> Self {
        let pool = Self::build(connector, config, Some(Arc::clone(cap)));
        cap.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&pool.inner));
        pool
    }

    fn build(connector: Connector, config: PoolConfig, cap: Option<Arc<SessionCap>>) -> Self {
        let inner = Inner {
            config,
            connector,
            cap,
            hello: ClientId::default(),
            state: Mutex::new(State::default()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// A session for `auth` on `server`: a parked one that answers `NOOP`, else a new one
    /// (connect, `EHLO`, `STARTTLS`, `AUTH`), every step ending by `deadline`, which the caller
    /// sets inside the lease that protects the message while no submission has started.
    ///
    /// # Errors
    ///
    /// A [`Rejection`] in the `connect` or `auth` phase: always `transient` and scoped to the
    /// connection, with [`Cause::Unauthorized`] when the server refused the credential,
    /// [`Cause::Deadline`] when no session of the credential freed before `deadline`.
    pub async fn session(
        &self,
        server: &SmtpServer<'_>,
        auth: &SmtpAuth<'_>,
        deadline: Instant,
    ) -> Result<SmtpSession, Rejection> {
        check_credential(auth)?;
        let key = key(server, auth);
        while let Some(mut live) = self.take_parked(&key) {
            let tested = within(deadline, NOOP_TIMEOUT, live.connection.test_connected()).await;
            if matches!(tested, Ok(true)) {
                return Ok(self.wrap(key, live));
            }
            close(live);
        }
        let permit = self.permit(&key, deadline).await?;
        let place = self.place(deadline).await?;
        let connection = self.open(server, auth, deadline).await?;
        let live = Live {
            connection,
            permit,
            place,
            opened_at: Instant::now(),
            sent: 0,
        };
        Ok(self.wrap(key, live))
    }

    /// The authentication probe of a connection check: a new session, never a parked one (a
    /// parked session proves an earlier login, not the current credential), closed with `QUIT`
    /// once `AUTH` succeeded.
    ///
    /// # Errors
    ///
    /// As [`SmtpPool::session`]: `535` and any other `5xx` to `AUTH` are
    /// [`Cause::Unauthorized`] (the credential is lost); `454` is a temporary refusal.
    pub async fn probe(
        &self,
        server: &SmtpServer<'_>,
        auth: &SmtpAuth<'_>,
        deadline: Instant,
    ) -> Result<(), Rejection> {
        check_credential(auth)?;
        let permit = self.permit(&key(server, auth), deadline).await?;
        let place = self.place(deadline).await?;
        let mut connection = self.open(server, auth, deadline).await?;
        // The probe's answer is the AUTH reply; a lost QUIT changes nothing.
        let _quit = within(deadline, COMMAND_TIMEOUT, connection.quit()).await;
        drop((permit, place));
        Ok(())
    }

    fn wrap(&self, key: Key, live: Live) -> SmtpSession {
        SmtpSession {
            live: Some(live),
            key,
            pool: Arc::clone(&self.inner),
            clean: true,
        }
    }

    fn take_parked(&self, key: &Key) -> Option<Live> {
        let (live, expired) = {
            let mut state = self.inner.state();
            let mut expired = self.inner.sweep(&mut state);
            let mut found = None;
            if let Some(parked) = state.parked.get_mut(key) {
                while let Some(candidate) = parked.pop() {
                    if self.inner.fresh(&candidate) {
                        found = Some(candidate.live);
                        break;
                    }
                    expired.push(candidate.live);
                }
            }
            (found, expired)
        };
        expired.into_iter().for_each(close);
        live
    }

    async fn permit(
        &self,
        key: &Key,
        deadline: Instant,
    ) -> Result<OwnedSemaphorePermit, Rejection> {
        let slots = {
            let mut state = self.inner.state();
            let max_open = self.inner.config.max_open.clamp(1, Semaphore::MAX_PERMITS);
            Arc::clone(
                state
                    .slots
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(Semaphore::new(max_open))),
            )
        };
        match timeout_at(deadline, slots.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) | Err(_) => Err(Rejection::local(
                Failure::Transient,
                Phase::Connect,
                Scope::Connection,
                Cause::Deadline,
                "no session of this credential freed before the deadline",
            )),
        }
    }

    /// A place among the process's sessions when the pool is capped ([`SmtpPool::capped`]): at
    /// once when one is free; otherwise the longest-parked session of any pool sharing the cap is
    /// closed to free one, and the wait ends at `deadline`. `None` for an uncapped pool.
    async fn place(&self, deadline: Instant) -> Result<Option<OwnedSemaphorePermit>, Rejection> {
        let Some(cap) = &self.inner.cap else {
            return Ok(None);
        };
        if let Ok(place) = Arc::clone(&cap.places).try_acquire_owned() {
            return Ok(Some(place));
        }
        cap.evict_oldest();
        match timeout_at(deadline, Arc::clone(&cap.places).acquire_owned()).await {
            Ok(Ok(place)) => Ok(Some(place)),
            // The process's own bound, not the connection's: no circuit breaker counts it.
            Ok(Err(_)) | Err(_) => Err(Rejection::local(
                Failure::Transient,
                Phase::Connect,
                Scope::Platform,
                Cause::Deadline,
                "no SMTP session of this process freed before the deadline",
            )),
        }
    }

    async fn open(
        &self,
        server: &SmtpServer<'_>,
        auth: &SmtpAuth<'_>,
        deadline: Instant,
    ) -> Result<AsyncSmtpConnection, Rejection> {
        super::session::open(
            &self.inner.connector,
            &self.inner.hello,
            server,
            auth,
            deadline,
            |host| self.tls_parameters(host),
        )
        .await
    }

    fn tls_parameters(&self, host: &str) -> Result<TlsParameters, Rejection> {
        if let Some(found) = self.inner.state().tls.get(host) {
            return Ok(found.clone());
        }
        let built = TlsParameters::new(host.to_owned()).map_err(|error| {
            Rejection::local(
                Failure::Transient,
                Phase::Connect,
                Scope::Connection,
                Cause::NoReply,
                &error.to_string(),
            )
        })?;
        let mut state = self.inner.state();
        if state.tls.len() >= TLS_CACHE {
            state.tls.clear();
        }
        state.tls.insert(host.to_owned(), built.clone());
        Ok(built)
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// When this pool's longest-parked session was parked, if it holds one.
    fn oldest_parked(&self) -> Option<Instant> {
        self.state()
            .parked
            .values()
            .filter_map(|parked| parked.first().map(|oldest| oldest.parked_at))
            .min()
    }

    /// Takes this pool's longest-parked session, whatever its credential, to be closed.
    fn take_oldest_parked(&self) -> Option<Live> {
        let mut state = self.state();
        let key = state
            .parked
            .iter()
            .filter_map(|(key, parked)| parked.first().map(|oldest| (key, oldest.parked_at)))
            .min_by_key(|(_, parked_at)| *parked_at)
            .map(|(key, _)| key.clone())?;
        let parked = state.parked.get_mut(&key)?;
        (!parked.is_empty()).then(|| parked.remove(0).live)
    }

    fn fresh(&self, parked: &Parked) -> bool {
        parked.parked_at.elapsed() < self.config.idle_timeout
            && parked.live.opened_at.elapsed() < self.config.max_age
    }

    /// Removes parked sessions past their idle timeout and the semaphores nobody holds, at most
    /// once per [`SWEEP_EVERY`]; returns the sessions to close.
    fn sweep(&self, state: &mut State) -> Vec<Live> {
        let due = state.swept_at.is_none_or(|at| at.elapsed() >= SWEEP_EVERY);
        if !due {
            return Vec::new();
        }
        state.swept_at = Some(Instant::now());
        let mut expired = Vec::new();
        for parked in state.parked.values_mut() {
            let (keep, stale): (Vec<Parked>, Vec<Parked>) = std::mem::take(parked)
                .into_iter()
                .partition(|candidate| self.fresh(candidate));
            *parked = keep;
            expired.extend(stale.into_iter().map(|candidate| candidate.live));
        }
        state.parked.retain(|_, parked| !parked.is_empty());
        state.slots.retain(|_, slots| Arc::strong_count(slots) > 1);
        expired
    }

    /// Parks a session after its transaction, or closes it.
    fn release(&self, key: Key, live: Live, clean: bool) {
        let reusable = clean
            && live.sent < self.config.max_messages
            && live.opened_at.elapsed() < self.config.max_age
            && !live.connection.has_broken();
        let mut closing = Vec::new();
        {
            let mut state = self.state();
            closing.extend(self.sweep(&mut state));
            let parked = state.parked.entry(key).or_default();
            if reusable && parked.len() < self.config.max_idle {
                parked.push(Parked {
                    live,
                    parked_at: Instant::now(),
                });
            } else {
                closing.push(live);
            }
        }
        closing.into_iter().for_each(close);
    }
}

impl Drop for SmtpSession {
    fn drop(&mut self) {
        if let Some(live) = self.live.take() {
            self.pool
                .release(std::mem::take(&mut self.key), live, self.clean);
        }
    }
}

impl SmtpSession {
    /// Submits one message: `MAIL FROM`, one `RCPT TO` per recipient, `DATA` and the content,
    /// every step ending by `deadline`, the submission's budget. Recipients
    /// refused while others were accepted are returned in [`Submission::refused`].
    ///
    /// # Errors
    ///
    /// A [`Rejection`]: `transient` with [`Cause::Deadline`] when the deadline passed before
    /// `MAIL FROM` or less than [`super::DATA_RESERVE`] remained for `DATA`; `uncertain` when
    /// the content was being sent and the final reply was not read; otherwise the meaning of
    /// the negative reply ([`crate::smtp::reply::classify`]) or `transient` for the phase that lost its
    /// reply. A throttle or a `421` at `RCPT TO` stops the submission before `DATA`, nothing sent.
    /// When every recipient was refused, or the submission stopped at `RCPT TO`, the rejection
    /// is the dominant refusal's (a throttle first, then a `4xx`) and lists every refusal read.
    pub async fn submit(
        mut self,
        envelope: &Envelope,
        mime: &[u8],
        deadline: Instant,
    ) -> Result<Submission, Rejection> {
        let reply_id = self.pool.config.reply_id;
        let Some(live) = self.live.as_mut() else {
            return Err(Rejection::local(
                Failure::Transient,
                Phase::MailFrom,
                Scope::Connection,
                Cause::NoReply,
                "the session is closed",
            ));
        };
        let result =
            super::transaction::run(&mut live.connection, envelope, mime, deadline, reply_id).await;
        match &result {
            Ok(submission) => {
                live.sent = live.sent.saturating_add(1);
                self.clean = submission.refused.is_empty();
            }
            Err(_) => self.clean = false,
        }
        result
    }
}

/// The pool key: a digest of everything that makes two sessions interchangeable.
fn key(server: &SmtpServer<'_>, auth: &SmtpAuth<'_>) -> Key {
    let (kind, username, secret) = match auth {
        SmtpAuth::Password { username, password } => {
            ("password", *username, password.expose_secret())
        }
        SmtpAuth::Xoauth2 { username, token } => ("xoauth2", *username, token.expose_secret()),
    };
    let security = match server.security {
        SmtpSecurity::Tls => "tls",
        SmtpSecurity::StartTls => "starttls",
        SmtpSecurity::Plain => "plain",
    };
    let host = server.host.to_ascii_lowercase();
    let port = server.port.to_be_bytes();
    let mut context = digest::Context::new(&digest::SHA256);
    for part in [
        host.as_bytes(),
        port.as_slice(),
        security.as_bytes(),
        kind.as_bytes(),
        username.as_bytes(),
        secret.as_bytes(),
    ] {
        context.update(&part.len().to_be_bytes());
        context.update(part);
    }
    context.finish().as_ref().to_vec()
}

/// Closes a session off the caller's path: `QUIT` when the connection is not broken, then the
/// socket; its permit is released afterwards.
fn close(live: Live) {
    let Live {
        mut connection,
        permit,
        place,
        ..
    } = live;
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let _closed = tokio::time::timeout(CLOSE_TIMEOUT, connection.abort()).await;
            drop((permit, place));
        });
    }
}

#[cfg(test)]
mod tests;
