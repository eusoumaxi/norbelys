//! OAuth decisions: the pure rules of Norbelys's own authorization server, separate from how its
//! codes, grants and tokens are stored or served.
//!
//! The server implements a deliberately small profile of OAuth 2.1
//! (<https://datatracker.ietf.org/doc/draft-ietf-oauth-v2-1/>), small enough to be tested
//! completely:
//!
//! - **Audiences** ([`Audience`]). A grant is for exactly one resource (RFC 8707,
//!   <https://www.rfc-editor.org/rfc/rfc8707>): the MCP server, whose access tokens are `nbo_`
//!   tokens of type `mcp`, or, for the command-line client only, the API itself, whose access
//!   tokens are `nbc_` tokens of type `cli`. Each surface accepts only its own type, so a token an
//!   MCP client holds never calls the API directly and the CLI's token never reaches the MCP
//!   server ([`audience_for`]).
//! - **PKCE** ([`pkce_holds`]). Every authorization code is bound to an S256 code challenge
//!   (RFC 7636, <https://www.rfc-editor.org/rfc/rfc7636>); `plain` is refused, so a stolen code is
//!   useless without the verifier only the client that started the flow holds.
//! - **Redirects** ([`redirect_allowed`]). A redirect URI must equal one the client registered,
//!   character for character, except that a loopback redirect (`http` to `127.0.0.1`, `[::1]` or
//!   `localhost`) may name any port, as native applications bind an ephemeral one (RFC 8252 §7.3,
//!   <https://www.rfc-editor.org/rfc/rfc8252#section-7.3>).
//! - **Scopes** ([`grantable`], [`requested_scopes`]). A grant carries API scopes only, never the
//!   dashboard's `workspace:manage`, so no credential a program holds can manage members or keys;
//!   what was asked is narrowed to that set and, at consent and on every use, to the person's
//!   current role.
//! - **Device codes** ([`poll`], [`user_code`]). The device authorization grant (RFC 8628,
//!   <https://www.rfc-editor.org/rfc/rfc8628>) answers each poll by the code's state: consumed,
//!   expired, denied, approved, polled too soon (`slow_down`) or still pending, in that order, so
//!   a code is redeemed once and the most final state wins. The code a person types is eight
//!   letters from an alphabet without vowels (no words, no look-alike characters), shown as
//!   `XXXX-XXXX` and accepted with any case, spaces or dash.
//! - **Refresh tokens** ([`refresh`]). A refresh token is used once; presenting a used or revoked
//!   one again is taken as theft (RFC 9700 §4.14.2,
//!   <https://www.rfc-editor.org/rfc/rfc9700#section-4.14.2>) and revokes the whole grant. A
//!   refresh token never outlives its grant, which ends 90 days after consent.
//!
//! Nothing here reads the clock: every decision takes `now` as an argument.

use std::time::Duration;

use aws_lc_rs::digest;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::domain::scope::{Scope, ScopeSet};

/// How long an access token lives.
pub const ACCESS_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// How long a refresh token lives, at most (never past its grant).
pub const REFRESH_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long a grant lives from consent, whatever the use.
pub const GRANT_LIFETIME: Duration = Duration::from_secs(90 * 24 * 3600);
/// How long an authorization code may wait for its exchange.
pub const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// How long a device code may wait for its approval.
pub const DEVICE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// How long a consent request handed to the dashboard stays valid.
pub const CONSENT_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// The poll interval a device code starts with, in seconds.
pub const DEVICE_INTERVAL: i16 = 5;
/// What `slow_down` adds to a device code's interval, in seconds (RFC 8628 §3.5).
pub const SLOW_DOWN_STEP: i16 = 5;
/// The letters of a user code: consonants only, so no code spells a word and none is confused
/// with a digit (RFC 8628 §6.1).
pub const USER_CODE_ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";
/// The length of a user code, without its dash.
pub const USER_CODE_LENGTH: usize = 8;

/// The resource a grant is for, which decides its access tokens' type and prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Audience {
    /// The API, for the command-line client's device grant: `nbc_` tokens of type `cli`.
    Cli,
    /// The MCP server: `nbo_` tokens of type `mcp`.
    Mcp,
}

impl Audience {
    /// The token's `typ` claim.
    #[must_use]
    pub fn typ(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mcp => "mcp",
        }
    }

    /// The prefix of its access tokens.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Cli => "nbc_",
            Self::Mcp => "nbo_",
        }
    }
}

/// The resources this server issues tokens for, as their canonical URLs (no trailing slash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resources {
    /// The API: the public API URL.
    pub api: String,
    /// The MCP server: the public API URL followed by `/mcp`.
    pub mcp: String,
}

impl Resources {
    /// The resources of an API published at `public_api_url`.
    #[must_use]
    pub fn of(public_api_url: &url::Url) -> Self {
        let api = public_api_url.as_str().trim_end_matches('/').to_owned();
        Self {
            mcp: format!("{api}/mcp"),
            api,
        }
    }
}

/// Why a `resource` parameter is refused (`invalid_target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrongResource;

/// The audience of a grant asked for `resource` by a client: the API for the command-line client
/// (`cli` true) and the MCP server for every other client; anything else is refused. A trailing
/// slash is ignored.
///
/// # Errors
///
/// [`WrongResource`] when the resource is not the one the client may ask for.
pub fn audience_for(
    resource: &str,
    cli: bool,
    resources: &Resources,
) -> Result<Audience, WrongResource> {
    let resource = resource.trim_end_matches('/');
    match cli {
        true if resource == resources.api => Ok(Audience::Cli),
        false if resource == resources.mcp => Ok(Audience::Mcp),
        _ => Err(WrongResource),
    }
}

/// Whether `challenge` is a well-formed S256 code challenge: the base64url (unpadded) encoding of
/// a SHA-256 digest, 43 characters.
#[must_use]
pub fn valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Whether `verifier` is the code verifier of `challenge` under S256 (RFC 7636 §4.6): 43 to 128
/// characters of the unreserved set whose SHA-256, base64url-encoded, is the challenge.
#[must_use]
pub fn pkce_holds(challenge: &str, verifier: &str) -> bool {
    let well_formed = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'));
    if !well_formed {
        return false;
    }
    let digest = digest::digest(&digest::SHA256, verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(digest.as_ref());
    aws_lc_rs::constant_time::verify_slices_are_equal(computed.as_bytes(), challenge.as_bytes())
        .is_ok()
}

/// Whether `requested` may be used as a redirect for a client that registered `registered`:
/// an exact match, or a loopback `http` redirect that differs only in its port.
#[must_use]
pub fn redirect_allowed(registered: &[String], requested: &str) -> bool {
    if !valid_redirect_uri(requested) {
        return false;
    }
    if registered.iter().any(|uri| uri == requested) {
        return true;
    }
    let Ok(requested) = url::Url::parse(requested) else {
        return false;
    };
    if requested.scheme() != "http" || !is_loopback(&requested) {
        return false;
    }
    registered.iter().any(|uri| {
        url::Url::parse(uri).is_ok_and(|mut uri| {
            let mut requested = requested.clone();
            uri.scheme() == "http"
                && is_loopback(&uri)
                && uri.set_port(None).is_ok()
                && requested.set_port(None).is_ok()
                && uri == requested
        })
    })
}

/// HTTPS callbacks, or HTTP loopback callbacks for native clients. Executable schemes,
/// credentials and fragments are refused even when a legacy client registered them.
#[must_use]
pub fn valid_redirect_uri(uri: &str) -> bool {
    uri.len() <= 2048
        && url::Url::parse(uri).is_ok_and(|url| {
            url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && (url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url)))
        })
}

fn is_loopback(url: &url::Url) -> bool {
    matches!(
        url.host(),
        Some(url::Host::Ipv4(address)) if address.is_loopback()
    ) || matches!(url.host(), Some(url::Host::Ipv6(address)) if address.is_loopback())
        || url.host_str() == Some("localhost")
}

/// The scopes a grant may carry: every API scope, but never the dashboard's `workspace:manage`.
#[must_use]
pub fn grantable() -> ScopeSet {
    ScopeSet::all()
        .iter()
        .filter(|scope| *scope != Scope::WorkspaceManage)
        .collect()
}

/// The scopes a client asked for with `scope` (space-separated; every grantable scope when
/// absent or empty), narrowed to [`grantable`].
///
/// # Errors
///
/// The name of an unknown scope (`invalid_scope`).
pub fn requested_scopes(scope: Option<&str>) -> Result<ScopeSet, String> {
    match scope.map(str::trim).filter(|scope| !scope.is_empty()) {
        None => Ok(grantable()),
        Some(scope) => Ok(ScopeSet::parse(scope.split_whitespace())?.intersect(grantable())),
    }
}

/// A user code from random bytes: bytes at or above the largest multiple of the alphabet's size
/// are skipped so every letter is equally likely; `None` when the bytes run out first.
#[must_use]
pub fn user_code(random: impl IntoIterator<Item = u8>) -> Option<String> {
    let size = USER_CODE_ALPHABET.len();
    let limit = 256 - 256 % size;
    let code: String = random
        .into_iter()
        .map(usize::from)
        .filter(|byte| *byte < limit)
        .filter_map(|byte| USER_CODE_ALPHABET.get(byte % size).copied().map(char::from))
        .take(USER_CODE_LENGTH)
        .collect();
    (code.len() == USER_CODE_LENGTH).then_some(code)
}

/// A typed user code in its stored form (eight upper-case letters of the alphabet), ignoring
/// case, spaces and dashes; `None` for anything else.
#[must_use]
pub fn normalize_user_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = code.len() == USER_CODE_LENGTH
        && code.bytes().all(|byte| USER_CODE_ALPHABET.contains(&byte));
    valid.then_some(code)
}

/// A stored user code as a person reads it: `XXXX-XXXX`.
#[must_use]
pub fn display_user_code(code: &str) -> String {
    let (head, tail) = code.split_at(code.len().min(USER_CODE_LENGTH / 2));
    format!("{head}-{tail}")
}

/// A device code's state when it is polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceState {
    /// When the code expires.
    pub expires_at: jiff::Timestamp,
    /// When it was last polled, if ever.
    pub last_polled_at: Option<jiff::Timestamp>,
    /// The interval the client must keep between polls, in seconds.
    pub interval_seconds: i16,
    /// Whether the person denied it.
    pub denied: bool,
    /// Whether the person approved it (a grant exists).
    pub approved: bool,
    /// Whether its tokens were already issued.
    pub consumed: bool,
}

/// The answer to a device code poll (RFC 8628 §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Poll {
    /// The tokens were issued already: `invalid_grant`.
    Consumed,
    /// The code expired: `expired_token`.
    Expired,
    /// The person denied it: `access_denied`.
    Denied,
    /// Approved: issue the tokens, once.
    Issue,
    /// Polled before the interval passed: `slow_down`, and the interval grows.
    SlowDown,
    /// Not decided yet: `authorization_pending`.
    Pending,
}

/// What a poll of `state` at `now` answers (see the module for the order).
#[must_use]
pub fn poll(state: &DeviceState, now: jiff::Timestamp) -> Poll {
    if state.consumed {
        return Poll::Consumed;
    }
    if now >= state.expires_at {
        return Poll::Expired;
    }
    if state.denied {
        return Poll::Denied;
    }
    if state.approved {
        return Poll::Issue;
    }
    let too_soon = state
        .last_polled_at
        .is_some_and(|last| now.duration_since(last).as_secs() < i64::from(state.interval_seconds));
    if too_soon {
        Poll::SlowDown
    } else {
        Poll::Pending
    }
}

/// A refresh token and its grant, as a refresh reads them under the grant's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshState {
    /// Whether the client presenting it is the grant's client.
    pub same_client: bool,
    /// Whether the token was used already.
    pub consumed: bool,
    /// Whether the token was revoked.
    pub revoked: bool,
    /// When the token expires.
    pub expires_at: jiff::Timestamp,
    /// Whether the grant was revoked.
    pub grant_revoked: bool,
    /// When the grant expires.
    pub grant_expires_at: jiff::Timestamp,
}

/// Why a refresh is refused (all answer `invalid_grant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RefreshRefusal {
    /// Another client presented the token: refused, nothing revoked (the token was not used).
    WrongClient,
    /// The grant was revoked or expired.
    GrantEnded,
    /// The token was used or revoked before: theft is assumed and the grant is revoked.
    Reused,
    /// The token expired unused.
    Expired,
}

/// Whether a refresh of `state` at `now` may rotate the token.
///
/// # Errors
///
/// The refusal, in the order of [`RefreshRefusal`]'s variants.
pub fn refresh(state: &RefreshState, now: jiff::Timestamp) -> Result<(), RefreshRefusal> {
    if !state.same_client {
        return Err(RefreshRefusal::WrongClient);
    }
    if state.grant_revoked || now >= state.grant_expires_at {
        return Err(RefreshRefusal::GrantEnded);
    }
    if state.consumed || state.revoked {
        return Err(RefreshRefusal::Reused);
    }
    if now >= state.expires_at {
        return Err(RefreshRefusal::Expired);
    }
    Ok(())
}

/// When a refresh token issued at `now` for a grant ending at `grant_expires_at` expires: 30 days
/// later, never past the grant.
#[must_use]
pub fn refresh_expiry(now: jiff::Timestamp, grant_expires_at: jiff::Timestamp) -> jiff::Timestamp {
    after(now, REFRESH_LIFETIME).min(grant_expires_at)
}

/// The instant `duration` after `at`, saturating at the end of time.
#[must_use]
pub fn after(at: jiff::Timestamp, duration: Duration) -> jiff::Timestamp {
    jiff::SignedDuration::try_from(duration)
        .ok()
        .and_then(|duration| at.checked_add(duration).ok())
        .unwrap_or(jiff::Timestamp::MAX)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsafe_callbacks_are_refused_even_when_registered_exactly() {
        for uri in [
            "javascript:alert(1)",
            "data:text/html,test",
            "file:///tmp/test",
            "http://public.example/cb",
            "https://user:pass@example.com/cb",
            "https://example.com/cb#fragment",
            "https://example.com/",
        ] {
            let safe = uri == "https://example.com/";
            assert_eq!(redirect_allowed(&[uri.to_owned()], uri), safe, "{uri}");
        }
    }

    use strum::IntoEnumIterator as _;

    use super::*;

    fn at(seconds: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_790_000_000 + seconds).unwrap()
    }

    fn resources() -> Resources {
        Resources::of(&url::Url::parse("https://api.norbelys.test/").unwrap())
    }

    /// Each audience is reached only by its own client kind and resource, and each has its own
    /// type and prefix, so a token minted for the MCP server can never be presented to the API as
    /// a CLI token or the other way round.
    #[test]
    fn each_audience_has_its_resource_type_and_prefix() {
        let resources = resources();
        for audience in Audience::iter() {
            let (cli, resource, typ, prefix) = match audience {
                Audience::Cli => (true, "https://api.norbelys.test", "cli", "nbc_"),
                Audience::Mcp => (false, "https://api.norbelys.test/mcp", "mcp", "nbo_"),
            };
            assert_eq!(audience_for(resource, cli, &resources), Ok(audience));
            assert_eq!(
                audience_for(&format!("{resource}/"), cli, &resources),
                Ok(audience)
            );
            assert_eq!(audience_for(resource, !cli, &resources), Err(WrongResource));
            assert_eq!((audience.typ(), audience.prefix()), (typ, prefix));
        }
        assert_eq!(
            audience_for("https://evil.test/mcp", false, &resources),
            Err(WrongResource)
        );
    }

    /// PKCE holds only for the verifier whose S256 digest is the challenge (the RFC 7636 appendix
    /// B example), and never for a malformed verifier, so a code is redeemable only by the client
    /// that started the flow.
    #[test]
    fn pkce_holds_only_for_the_right_verifier() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(valid_challenge(challenge));
        assert!(pkce_holds(challenge, verifier));
        assert!(!pkce_holds(challenge, &verifier.replace('d', "e")));
        assert!(!pkce_holds(challenge, "short"));
        assert!(!pkce_holds(challenge, &format!("{verifier}!")));
        assert!(!valid_challenge("plain-challenge"));
        assert!(!valid_challenge(&format!("{}=", &challenge[..42])));
    }

    /// Redirects match exactly, except loopback redirects, whose port may vary as a native
    /// application binds an ephemeral one; another path, host or scheme never matches.
    #[test]
    fn redirects_match_exactly_or_on_loopback_ports() {
        let registered = vec![
            "https://client.test/callback".to_owned(),
            "http://127.0.0.1/callback".to_owned(),
            "http://localhost:33418/cb".to_owned(),
        ];
        for (requested, allowed) in [
            ("https://client.test/callback", true),
            ("https://client.test/callback/", false),
            ("https://client.test:8443/callback", false),
            ("http://client.test/callback", false),
            ("http://127.0.0.1:51234/callback", true),
            ("http://127.0.0.1:51234/other", false),
            ("http://localhost:4000/cb", true),
            ("https://127.0.0.1:51234/callback", false),
            ("not a url", false),
        ] {
            assert_eq!(
                redirect_allowed(&registered, requested),
                allowed,
                "{requested}"
            );
        }
    }

    /// Every API scope is grantable and the dashboard's `workspace:manage` never is; a request
    /// names known scopes only and is narrowed to the grantable ones, so no program can obtain a
    /// credential that manages members or keys.
    #[test]
    fn grants_carry_api_scopes_only() {
        for scope in Scope::iter() {
            assert_eq!(
                grantable().contains(scope),
                scope != Scope::WorkspaceManage,
                "{scope}"
            );
        }
        assert_eq!(requested_scopes(None), Ok(grantable()));
        assert_eq!(requested_scopes(Some("  ")), Ok(grantable()));
        assert_eq!(
            requested_scopes(Some("people:read workspace:manage")),
            Ok([Scope::PeopleRead].into_iter().collect())
        );
        assert!(requested_scopes(Some("people:read admin")).is_err());
    }

    /// User codes use the alphabet only, skip the bytes that would bias it, read back from any
    /// case, spacing or dash, and refuse anything else, so the code a person types is the code
    /// that was shown.
    #[test]
    fn user_codes_are_unbiased_and_forgiving() {
        let code = user_code([0, 255, 19, 20, 239, 240, 1, 2, 3, 4]).unwrap();
        assert_eq!(code, "BZBZCDFG");
        assert!(user_code([255_u8; 64]).is_none());
        assert_eq!(display_user_code(&code), "BZBZ-CDFG");
        assert_eq!(
            normalize_user_code(" bzbz-cdfg ").as_deref(),
            Some("BZBZCDFG")
        );
        assert_eq!(
            normalize_user_code("BZBZ CDFG").as_deref(),
            Some("BZBZCDFG")
        );
        assert_eq!(normalize_user_code("BZBZ-CDFA"), None);
        assert_eq!(normalize_user_code("BZBZ-CDF"), None);
    }

    /// Each poll answer is reached by exactly the state that calls for it, and the more final
    /// state wins (consumed over expired over denied over approved over too soon), so a device
    /// code issues tokens once and a late approval cannot revive an expired code.
    #[test]
    fn each_poll_answer_has_its_state() {
        let pending = DeviceState {
            expires_at: at(600),
            last_polled_at: Some(at(0)),
            interval_seconds: 5,
            denied: false,
            approved: false,
            consumed: false,
        };
        for answer in Poll::iter() {
            let (state, now) = match answer {
                Poll::Consumed => (
                    DeviceState {
                        consumed: true,
                        approved: true,
                        ..pending
                    },
                    at(700),
                ),
                Poll::Expired => (
                    DeviceState {
                        approved: true,
                        ..pending
                    },
                    at(600),
                ),
                Poll::Denied => (
                    DeviceState {
                        denied: true,
                        ..pending
                    },
                    at(1),
                ),
                Poll::Issue => (
                    DeviceState {
                        approved: true,
                        ..pending
                    },
                    at(1),
                ),
                Poll::SlowDown => (pending, at(4)),
                Poll::Pending => (pending, at(5)),
            };
            assert_eq!(poll(&state, now), answer, "{answer:?}");
        }
        let first = DeviceState {
            last_polled_at: None,
            ..pending
        };
        assert_eq!(poll(&first, at(0)), Poll::Pending);
    }

    /// Each refusal is reached by exactly the state that calls for it, in order, and a fresh token
    /// of a live grant rotates; a refresh token never outlives its grant.
    #[test]
    fn each_refresh_refusal_has_its_state() {
        let fresh = RefreshState {
            same_client: true,
            consumed: false,
            revoked: false,
            expires_at: at(100),
            grant_revoked: false,
            grant_expires_at: at(200),
        };
        assert_eq!(refresh(&fresh, at(0)), Ok(()));
        for refusal in RefreshRefusal::iter() {
            let state = match refusal {
                RefreshRefusal::WrongClient => RefreshState {
                    same_client: false,
                    consumed: true,
                    ..fresh
                },
                RefreshRefusal::GrantEnded => RefreshState {
                    grant_revoked: true,
                    revoked: true,
                    ..fresh
                },
                RefreshRefusal::Reused => RefreshState {
                    consumed: true,
                    expires_at: at(0),
                    ..fresh
                },
                RefreshRefusal::Expired => RefreshState {
                    expires_at: at(0),
                    ..fresh
                },
            };
            assert_eq!(refresh(&state, at(0)), Err(refusal), "{refusal:?}");
        }
        assert_eq!(
            refresh(&fresh, at(200)),
            Err(RefreshRefusal::GrantEnded),
            "an expired grant"
        );
        assert_eq!(refresh_expiry(at(0), at(200)), at(200));
        let far = after(at(0), GRANT_LIFETIME);
        assert_eq!(refresh_expiry(at(0), far), after(at(0), REFRESH_LIFETIME));
    }
}
