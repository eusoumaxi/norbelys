//! Logging in: the OAuth 2.0 device authorization grant (RFC 8628,
//! <https://www.rfc-editor.org/rfc/rfc8628>) against the API's own authorization server, the
//! refresh of the CLI session it creates, and storing an API key instead.
//!
//! # The device flow
//!
//! `POST /oauth/device_authorization`, form-encoded with the CLI's client id and the API as the
//! resource the token is for (RFC 8707), answers a device code, a short user code and a
//! verification URL. The CLI shows the code, opens the URL in the browser, where the person
//! signs in, chooses a workspace and approves, and polls `POST /oauth/token` with
//! `grant_type=urn:ietf:params:oauth:grant-type:device_code` as RFC 8628 §3.4 and §3.5 say:
//!
//! - it waits the server's `interval` (5 seconds when none is given) before each poll, the first
//!   one included, since nobody approves a code faster;
//! - `authorization_pending` polls again; `slow_down` adds 5 seconds to the interval for this
//!   and every later poll;
//! - `access_denied` and `expired_token` end the attempt, and so does the code's own lifetime
//!   (`expires_in`), counted locally in case the server never says so.
//!
//! # The session
//!
//! The grant answers an access token (`nbc_…`, valid for minutes) and a refresh token, kept in
//! the profile with the access token's expiry. A command whose access token expires within a
//! minute first refreshes it (`grant_type=refresh_token`) and stores the new pair: refresh
//! tokens are rotated, each one used once. The server revokes the whole session when a refresh
//! token is presented twice, so the refresh runs under the configuration lock and a process that
//! waited for it uses the token the other stored (see `config`). A refused refresh
//! (`invalid_grant`: expired, revoked, or the grant was withdrawn) ends the session, which is
//! removed from the profile, and the command asks for `norbelys login`.
//!
//! # API keys
//!
//! `login --api-key …` stores the key in the profile instead, the way to log in where no browser
//! is at hand. A profile holds an API key or a session, never both.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ArgMatches;
use clap::parser::ValueSource;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

use crate::api::{self, Context};
use crate::config::{self, ConfigError, Session};
use crate::output::{Class, Terminal};

/// The CLI's client id at the authorization server: a public client (it holds no secret), the
/// only one allowed the device grant.
pub const CLIENT_ID: &str = "norbelys-cli";

/// The grant type of a device code poll (RFC 8628 §3.4).
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The poll interval when the server gives none, in seconds (RFC 8628 §3.2).
const DEFAULT_INTERVAL: u64 = 5;

/// What `slow_down` adds to the interval, in seconds (RFC 8628 §3.5).
const SLOW_DOWN_STEP: u64 = 5;

/// An access token expiring within this many seconds is refreshed before use, so it cannot
/// expire on its way to the server.
const REFRESH_MARGIN: i64 = 60;

/// Why logging in, or staying logged in, failed.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The person refused the device in the browser.
    #[error("the login was denied in the browser")]
    Denied,
    /// The code expired before it was approved.
    #[error("the login code expired before it was approved; run `norbelys login` again")]
    Expired,
    /// The session is over: there is none, or its refresh was refused.
    #[error("profile `{profile}` has no CLI session any more; run `norbelys login` again")]
    Ended { profile: String },
    /// The authorization server refused with an OAuth error.
    #[error("the authorization server refused: {error}{}", description.as_deref().map(|text| format!(" ({text})")).unwrap_or_default())]
    Refused {
        error: String,
        description: Option<String>,
    },
    /// The authorization server's answer is not an OAuth answer.
    #[error("the authorization server answered {status}, not in OAuth's format: {excerpt}")]
    Unexpected { status: u16, excerpt: String },
    /// The authorization server could not be reached.
    #[error("cannot reach the authorization server: {0}")]
    Transport(String),
    /// The profile could not be read or saved.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl LoginError {
    /// The exit class of the failure.
    #[must_use]
    pub fn class(&self) -> Class {
        match self {
            Self::Denied | Self::Expired | Self::Ended { .. } | Self::Refused { .. } => {
                Class::Unauthenticated
            }
            Self::Unexpected { status, .. } => Class::of_status(*status),
            Self::Transport(_) => Class::Unreachable,
            Self::Config(_) => Class::Failure,
        }
    }
}

/// The answer of `POST /oauth/device_authorization` (RFC 8628 §3.2).
#[derive(Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    /// The code the person checks in the browser.
    pub user_code: String,
    /// Where the person approves.
    pub verification_uri: String,
    /// The same, with the code filled in.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// The device code's lifetime, in seconds.
    pub expires_in: u64,
    /// The least wait between polls, in seconds.
    #[serde(default)]
    pub interval: Option<u64>,
}

/// A successful token answer (RFC 6749 §5.1).
#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub token_type: String,
    /// The access token's lifetime, in seconds.
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// An OAuth error answer (RFC 6749 §5.2).
#[derive(Deserialize)]
struct OauthError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// `norbelys login`: stores the API key given on the command line, or runs the device flow and
/// stores the session it yields.
///
/// # Errors
///
/// When the flow is denied or expires, the server refuses or cannot be reached, or the profile
/// cannot be saved.
pub async fn run(
    context: &Context,
    matches: &ArgMatches,
    terminal: &mut Terminal<'_>,
) -> Result<(), crate::Error> {
    let profile = config::load(&context.config)?
        .profiles
        .remove(&context.profile)
        .unwrap_or_default();
    let base = context.base_url(&profile)?;
    let stored_url = context.api_url.as_ref().map(|_| base.to_string());
    let given_key = context
        .api_key
        .clone()
        .filter(|_| matches.value_source("api-key") == Some(ValueSource::CommandLine));
    if let Some(key) = given_key {
        config::update(&context.config, |config| {
            let profile = config.profiles.entry(context.profile.clone()).or_default();
            profile.api_key = Some(key);
            profile.session = None;
            if stored_url.is_some() {
                profile.api_url.clone_from(&stored_url);
            }
        })?;
        writeln!(
            terminal.err,
            "Stored the API key in profile `{}`.",
            context.profile
        )?;
        return Ok(());
    }

    let http = api::http_client()?;
    let open = terminal.open;
    let mut announced = Ok(());
    let tokens = device_flow(&http, &base, tokio::time::sleep, |authorization| {
        let url = authorization
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&authorization.verification_uri);
        announced = writeln!(
            terminal.err,
            "To log in, approve this device in your browser.\n  Code: {}\n  URL:  {url}\n\
             Waiting for the approval (Ctrl-C to cancel)…",
            authorization.user_code
        );
        open(url);
    })
    .await?;
    announced?;
    let session = session(tokens, None, now());
    config::update(&context.config, |config| {
        let profile = config.profiles.entry(context.profile.clone()).or_default();
        profile.session = Some(session);
        profile.api_key = None;
        if stored_url.is_some() {
            profile.api_url.clone_from(&stored_url);
        }
    })?;
    writeln!(
        terminal.err,
        "Logged in: profile `{}` now holds a CLI session for {base}.",
        context.profile
    )?;
    Ok(())
}

/// Runs the device flow against the authorization server at `base`: asks for a code, lets
/// `announce` show it, then polls with `sleep` between polls until the person approves.
///
/// # Errors
///
/// [`LoginError::Denied`], [`LoginError::Expired`], a refusal, an answer that is not OAuth, or
/// an unreachable server.
pub async fn device_flow<S, F>(
    http: &Client,
    base: &Url,
    mut sleep: S,
    announce: impl FnOnce(&DeviceAuthorization),
) -> Result<Tokens, LoginError>
where
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    let resource = base.as_str().trim_end_matches('/');
    let started = post_form(
        http,
        endpoint(base, &["oauth", "device_authorization"]),
        &[("client_id", CLIENT_ID), ("resource", resource)],
    )
    .await?;
    let authorization: DeviceAuthorization = match started {
        Answer::Ok(status, body) => {
            serde_json::from_slice(&body).map_err(|_| unexpected(status, &body))?
        }
        Answer::Refused(error) => return Err(refused(error)),
    };
    announce(&authorization);
    let mut interval = authorization.interval.unwrap_or(DEFAULT_INTERVAL);
    let mut waited = 0_u64;
    loop {
        waited = waited.saturating_add(interval);
        if waited > authorization.expires_in {
            return Err(LoginError::Expired);
        }
        sleep(Duration::from_secs(interval)).await;
        let polled = token(
            http,
            base,
            &[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", &authorization.device_code),
                ("client_id", CLIENT_ID),
            ],
        )
        .await?;
        match polled {
            Ok(tokens) => return Ok(tokens),
            Err(error) => match error.error.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval = interval.saturating_add(SLOW_DOWN_STEP),
                "access_denied" => return Err(LoginError::Denied),
                "expired_token" => return Err(LoginError::Expired),
                _ => return Err(refused(error)),
            },
        }
    }
}

/// The profile's access token, refreshed first when it expires within a minute.
///
/// # Errors
///
/// [`LoginError::Ended`] when the profile has no session or its refresh is refused (the session
/// is then removed), or the failures of the refresh itself.
pub async fn access_token(
    http: &Client,
    base: &Url,
    path: &Path,
    profile: &str,
) -> Result<String, LoginError> {
    let mut locked = config::Locked::acquire(path)?;
    let ended = || LoginError::Ended {
        profile: profile.to_owned(),
    };
    let current = locked
        .config
        .profiles
        .get(profile)
        .and_then(|profile| profile.session.clone())
        .ok_or_else(ended)?;
    if current.expires_at > now().saturating_add(REFRESH_MARGIN) {
        return Ok(current.access_token);
    }
    let refreshed = match &current.refresh_token {
        Some(refresh_token) => {
            token(
                http,
                base,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", CLIENT_ID),
                ],
            )
            .await?
        }
        None => Err(OauthError {
            error: "invalid_grant".to_owned(),
            error_description: None,
        }),
    };
    let stored = locked
        .config
        .profiles
        .entry(profile.to_owned())
        .or_default();
    match refreshed {
        Ok(tokens) => {
            let fresh = session(tokens, current.refresh_token, now());
            let access_token = fresh.access_token.clone();
            stored.session = Some(fresh);
            locked.save()?;
            Ok(access_token)
        }
        Err(error) if error.error == "invalid_grant" => {
            stored.session = None;
            locked.save()?;
            Err(ended())
        }
        Err(error) => Err(refused(error)),
    }
}

/// The session a token answer gives at `now`. A refresh answer without a new refresh token
/// keeps the previous one; one without a lifetime is refreshed at its next use.
fn session(tokens: Tokens, previous_refresh: Option<String>, now: i64) -> Session {
    Session {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.or(previous_refresh),
        expires_at: now.saturating_add(tokens.expires_in.unwrap_or(0)),
    }
}

/// An answer of the authorization server.
enum Answer {
    /// A `2xx`, with its status and body.
    Ok(u16, Vec<u8>),
    /// An OAuth error.
    Refused(OauthError),
}

/// `POST /oauth/token`: tokens, or the OAuth error that refused them.
async fn token(
    http: &Client,
    base: &Url,
    form: &[(&str, &str)],
) -> Result<Result<Tokens, OauthError>, LoginError> {
    match post_form(http, endpoint(base, &["oauth", "token"]), form).await? {
        Answer::Ok(status, body) => {
            let tokens: Tokens =
                serde_json::from_slice(&body).map_err(|_| unexpected(status, &body))?;
            if !tokens.token_type.eq_ignore_ascii_case("bearer") {
                return Err(unexpected(status, &body));
            }
            Ok(Ok(tokens))
        }
        Answer::Refused(error) => Ok(Err(error)),
    }
}

/// POSTs a form and reads the answer: a `2xx`, an OAuth error (RFC 6749 §5.2 answers `400`, or
/// `401` for a client), or anything else as unexpected.
async fn post_form(http: &Client, url: Url, form: &[(&str, &str)]) -> Result<Answer, LoginError> {
    let response = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|error| LoginError::Transport(api::chain(&error)))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| LoginError::Transport(api::chain(&error)))?
        .to_vec();
    if (200..300).contains(&status) {
        return Ok(Answer::Ok(status, body));
    }
    match serde_json::from_slice::<OauthError>(&body) {
        Ok(error) if status == 400 || status == 401 => Ok(Answer::Refused(error)),
        _ => Err(unexpected(status, &body)),
    }
}

fn refused(error: OauthError) -> LoginError {
    LoginError::Refused {
        error: error.error,
        description: error.error_description,
    }
}

fn unexpected(status: u16, body: &[u8]) -> LoginError {
    LoginError::Unexpected {
        status,
        excerpt: api::excerpt(body),
    }
}

/// An endpoint of the authorization server, which shares the API's base URL.
fn endpoint(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    url
}

/// The current time in Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Opens `url` in the user's browser, if it is an `http` or `https` URL: a server-given URL of
/// another scheme could start any program. Nothing is waited for, and a failure is ignored,
/// since the URL is printed anyway.
pub fn open_browser(url: &str) {
    let Ok(parsed) = Url::parse(url) else {
        return;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return;
    }
    let (program, prefix): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(windows) {
        // Not `cmd /c start`: `cmd` would read `&` in the URL as a command separator.
        ("rundll32", &["url.dll,FileProtocolHandler"])
    } else {
        ("xdg-open", &[])
    };
    let _ = std::process::Command::new(program)
        .args(prefix)
        .arg(parsed.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use url::Url;

    use super::{CLIENT_ID, DEVICE_CODE_GRANT, LoginError, access_token, device_flow};
    use crate::config::{self, Session};
    use crate::testing::{Fake, Reply, Scratch, run};

    fn started(interval: Option<u64>, expires_in: u64) -> Reply {
        Reply::json(
            200,
            json!({
                "device_code": "dev-123",
                "user_code": "WDJB-MJHT",
                "verification_uri": "https://app.norbelys.com/activate",
                "verification_uri_complete": "https://app.norbelys.com/activate?user_code=WDJB-MJHT",
                "expires_in": expires_in,
                "interval": interval
            }),
        )
    }

    fn oauth_error(error: &str) -> Reply {
        Reply::json(400, json!({ "error": error }))
    }

    fn tokens(access: &str, refresh: &str) -> Reply {
        Reply::json(
            200,
            json!({ "access_token": access, "token_type": "Bearer", "expires_in": 600, "refresh_token": refresh }),
        )
    }

    /// Runs the device flow against `fake`, recording the waits instead of sleeping.
    async fn flow(fake: &Fake) -> (Result<super::Tokens, LoginError>, Vec<u64>, Option<String>) {
        let mut waits = Vec::new();
        let mut shown = None;
        let base = Url::parse(&fake.url).unwrap();
        let result = device_flow(
            &reqwest::Client::new(),
            &base,
            |wait: Duration| {
                waits.push(wait.as_secs());
                std::future::ready(())
            },
            |authorization| shown = Some(authorization.user_code.clone()),
        )
        .await;
        (result, waits, shown)
    }

    /// A pending poll waits the interval again and `slow_down` adds five seconds to it for every
    /// later poll, as RFC 8628 §3.5 requires; the requests are form-encoded with the device code
    /// grant, the CLI's client id and the API as the resource. Polling faster than the server
    /// allows would get the device refused.
    #[tokio::test]
    async fn the_device_flow_polls_at_the_interval_and_slows_down_when_told() {
        let fake = Fake::start(vec![
            started(Some(5), 600),
            oauth_error("authorization_pending"),
            oauth_error("slow_down"),
            oauth_error("authorization_pending"),
            tokens("nbc_access", "refresh-1"),
        ])
        .await;
        let (result, waits, shown) = flow(&fake).await;
        let tokens = result.unwrap();
        assert_eq!(tokens.access_token, "nbc_access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(waits, [5, 5, 10, 10]);
        assert_eq!(shown.as_deref(), Some("WDJB-MJHT"));
        let seen = fake.seen();
        assert_eq!(seen[0].path, "/oauth/device_authorization");
        assert_eq!(
            seen[0].form(),
            [
                ("client_id".to_owned(), CLIENT_ID.to_owned()),
                ("resource".to_owned(), fake.url.clone())
            ]
        );
        for poll in &seen[1..] {
            assert_eq!(poll.path, "/oauth/token");
            assert_eq!(
                poll.header("content-type"),
                Some("application/x-www-form-urlencoded")
            );
            assert_eq!(
                poll.form(),
                [
                    ("grant_type".to_owned(), DEVICE_CODE_GRANT.to_owned()),
                    ("device_code".to_owned(), "dev-123".to_owned()),
                    ("client_id".to_owned(), CLIENT_ID.to_owned())
                ]
            );
        }
    }

    /// A denial and an expiry end the flow at once with their own error, and the code's own
    /// lifetime ends it even when the server keeps answering "pending": the CLI never polls a
    /// dead code forever. Without an interval the RFC's default of five seconds applies.
    #[tokio::test]
    async fn denied_and_expired_codes_end_the_flow() {
        let denied = Fake::start(vec![started(None, 600), oauth_error("access_denied")]).await;
        let (result, waits, _) = flow(&denied).await;
        assert!(matches!(result, Err(LoginError::Denied)));
        assert_eq!(waits, [5]);

        let expired = Fake::start(vec![started(Some(5), 600), oauth_error("expired_token")]).await;
        assert!(matches!(flow(&expired).await.0, Err(LoginError::Expired)));

        let lapsed = Fake::start(vec![
            started(Some(5), 10),
            oauth_error("authorization_pending"),
            oauth_error("authorization_pending"),
        ])
        .await;
        let (result, waits, _) = flow(&lapsed).await;
        assert!(matches!(result, Err(LoginError::Expired)));
        assert_eq!(waits, [5, 5]);
        assert_eq!(lapsed.seen().len(), 3);

        let refused = Fake::start(vec![Reply::json(
            400,
            json!({ "error": "invalid_client", "error_description": "unknown client" }),
        )])
        .await;
        let (result, _, shown) = flow(&refused).await;
        assert!(
            matches!(result, Err(LoginError::Refused { ref error, .. }) if error == "invalid_client")
        );
        assert!(shown.is_none());
    }

    /// `norbelys login` stores the session in the profile with the access token's expiry, and
    /// drops a stored API key, so later commands use the session; `login --api-key` stores the
    /// key and drops the session.
    #[tokio::test]
    async fn login_stores_the_session_or_the_key_in_the_profile() {
        let fake = Fake::start(vec![
            started(Some(0), 600),
            tokens("nbc_access", "refresh-1"),
        ])
        .await;
        let scratch = Scratch::new();
        config::update(&scratch.config(), |config| {
            config
                .profiles
                .entry("default".to_owned())
                .or_default()
                .api_key = Some("nb_test_old".to_owned());
        })
        .unwrap();
        let ran = run(&scratch, &["--api-url", &fake.url, "login"]).await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        assert!(ran.err.contains("WDJB-MJHT"));
        let profile = config::load(&scratch.config())
            .unwrap()
            .profiles
            .remove("default")
            .unwrap();
        let session = profile.session.unwrap();
        assert_eq!(session.access_token, "nbc_access");
        assert_eq!(session.refresh_token.as_deref(), Some("refresh-1"));
        assert!(session.expires_at > super::now() + 500);
        assert!(profile.api_key.is_none());
        assert_eq!(
            profile.api_url.as_deref(),
            Some(format!("{}/", fake.url).as_str())
        );

        let ran = run(&scratch, &["login", "--api-key", "nb_test_new"]).await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        let profile = config::load(&scratch.config())
            .unwrap()
            .profiles
            .remove("default")
            .unwrap();
        assert_eq!(profile.api_key.as_deref(), Some("nb_test_new"));
        assert!(profile.session.is_none());
    }

    fn expired_session(scratch: &Scratch, refresh: &str) {
        config::update(&scratch.config(), |config| {
            config
                .profiles
                .entry("default".to_owned())
                .or_default()
                .session = Some(Session {
                access_token: "nbc_stale".to_owned(),
                refresh_token: Some(refresh.to_owned()),
                expires_at: 0,
            });
        })
        .unwrap();
    }

    #[tokio::test]
    async fn malformed_tokens_and_non_oauth_errors_are_not_stored_as_sessions() {
        for reply in [
            Reply::json(
                200,
                json!({"access_token": "unusable", "token_type": "Basic"}),
            ),
            Reply::json(200, json!({"token_type": "Bearer"})),
            Reply::json(503, json!({"error": "temporarily_unavailable"})),
            Reply::json(400, json!({"unexpected": "body"})),
        ] {
            let fake = Fake::start(vec![reply]).await;
            let base = Url::parse(&fake.url).unwrap();
            let result = super::token(&reqwest::Client::new(), &base, &[]).await;
            assert!(matches!(result, Err(LoginError::Unexpected { .. })));
            assert_eq!(fake.seen().len(), 1);
        }
    }

    #[tokio::test]
    async fn refused_refresh_preserves_the_session_for_retryable_oauth_errors() {
        let fake = Fake::start(vec![Reply::json(
            400,
            json!({"error": "temporarily_unavailable"}),
        )])
        .await;
        let scratch = Scratch::new();
        expired_session(&scratch, "refresh-original");
        let ran = run(&scratch, &["--api-url", &fake.url, "people", "list"]).await;
        assert_ne!(ran.code, 0);
        assert!(ran.err.contains("temporarily_unavailable"));
        let session = config::load(&scratch.config()).unwrap().profiles["default"]
            .session
            .clone()
            .unwrap();
        assert_eq!(session.refresh_token.as_deref(), Some("refresh-original"));
        assert_eq!(fake.seen().len(), 1);
    }

    #[test]
    fn a_server_cannot_open_local_files_or_execute_a_program_as_a_login_url() {
        // These inputs must return before starting a browser process on any platform.
        for url in [
            "not a URL",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,test",
        ] {
            super::open_browser(url);
        }
    }

    /// An expired access token is refreshed before the request, the request carries the new
    /// token and the rotated refresh token replaces the used one in the profile: a refresh token
    /// is single use, so keeping the old one would end the session at the next refresh.
    #[tokio::test]
    async fn an_expired_session_is_refreshed_and_rotated_before_the_request() {
        let fake = Fake::start(vec![
            tokens("nbc_fresh", "refresh-2"),
            Reply::json(
                200,
                json!({ "data": [], "meta": { "has_more": false, "next_cursor": null } }),
            ),
        ])
        .await;
        let scratch = Scratch::new();
        expired_session(&scratch, "refresh-1");
        let ran = run(&scratch, &["--api-url", &fake.url, "people", "list"]).await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        let seen = fake.seen();
        assert_eq!(seen[0].path, "/oauth/token");
        assert_eq!(
            seen[0].form(),
            [
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), "refresh-1".to_owned()),
                ("client_id".to_owned(), CLIENT_ID.to_owned())
            ]
        );
        assert_eq!(seen[1].header("authorization"), Some("Bearer nbc_fresh"));
        let session = config::load(&scratch.config()).unwrap().profiles["default"]
            .session
            .clone()
            .unwrap();
        assert_eq!(session.access_token, "nbc_fresh");
        assert_eq!(session.refresh_token.as_deref(), Some("refresh-2"));
    }

    /// A refused refresh (`invalid_grant`) ends the session: it is removed from the profile and
    /// the command exits as unauthenticated, asking for a new login, without calling the API
    /// with a token that cannot work.
    #[tokio::test]
    async fn a_refused_refresh_ends_the_session() {
        let fake = Fake::start(vec![oauth_error("invalid_grant")]).await;
        let scratch = Scratch::new();
        expired_session(&scratch, "refresh-1");
        let ran = run(&scratch, &["--api-url", &fake.url, "people", "list"]).await;
        assert_eq!(ran.code, 3);
        assert!(ran.err.contains("norbelys login"), "{}", ran.err);
        assert_eq!(fake.seen().len(), 1);
        assert!(
            config::load(&scratch.config()).unwrap().profiles["default"]
                .session
                .is_none()
        );
    }

    /// Two processes that find the same expired token refresh it once: the second waits for the
    /// configuration lock and then uses the token the first stored. Refreshing twice with one
    /// refresh token is what the server treats as theft, revoking the session for both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_processes_refresh_a_session_once() {
        let fake = Fake::start(vec![
            tokens("nbc_fresh", "refresh-2").after(Duration::from_millis(300)),
        ])
        .await;
        let scratch = Scratch::new();
        expired_session(&scratch, "refresh-1");
        let base = Url::parse(&fake.url).unwrap();
        let processes = (0..2).map(|_| {
            let (base, path) = (base.clone(), scratch.config());
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(access_token(
                    &reqwest::Client::new(),
                    &base,
                    &path,
                    "default",
                ))
            })
        });
        let processes: Vec<_> = processes.collect();
        let tokens = tokio::task::spawn_blocking(move || {
            processes
                .into_iter()
                .map(|process| process.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        assert_eq!(tokens, ["nbc_fresh", "nbc_fresh"]);
        assert_eq!(fake.seen().len(), 1);
    }
}
