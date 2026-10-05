//! Email codes: the default sign-in, and the way a new user signs up.
//!
//! # The challenge
//!
//! `POST /v1/auth/challenges { method: "email_code", email, captcha_token }` creates, in one
//! transaction:
//!
//! - an `email_code` ceremony bound to the browser that asked (its cookie), whose sealed state
//!   holds the address as typed;
//! - a `login_codes` row naming the ceremony: a 6-digit code, stored only as its HMAC under the
//!   deployment key bound to the ceremony (`crypto::hash_code`), and an independent 32-byte link
//!   token, stored only as its keyed hash; at most 5 attempts; 10 minutes;
//! - the transactional message carrying both (`delivery::accept::transactional`), sent by the
//!   `system` workspace.
//!
//! The answer is `201` with the challenge for every syntactically valid address, whether or not a
//! user holds it, so the challenge says nothing about accounts. Its rate limits, the captcha and
//! the routing to single sign-on are the handler's.
//!
//! # The two finishes
//!
//! - **The code** is accepted only from the browser holding the ceremony cookie: `POST
//!   /v1/auth/sessions { challenge_id, code }`. Every attempt counts, wrong ones included, and is
//!   committed even when the answer is a refusal; the fifth wrong one locks the code for good.
//!   The comparison is in constant time.
//! - **The link** works in any browser, because people read mail on another device: it opens a
//!   dashboard page that names the account ("Sign in as …?") and whose button posts `{ token,
//!   email }`. The address must be the one the code was sent to, so the page can only ever sign a
//!   person into the account it showed them, and a mail scanner that follows links consumes
//!   nothing (it does not post).
//!
//! Either finish consumes the code and its ceremony together, so a code cannot be used after its
//! link or the other way round. All refusals share one answer (`401 unauthorized`): a wrong code,
//! an expired or locked one, another browser, an unknown challenge.

use serde_json::json;
use url::Url;

use super::ceremonies::{self, CeremonyError, Kind};
use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::delivery::accept::{self, Transactional};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Challenge, Id};
use crate::domain::time::Timestamp;

/// The digits of a code.
const DIGITS: u32 = 6;
/// The attempts a code allows.
pub const MAX_ATTEMPTS: i16 = 5;
/// The dashboard page a sign-in link opens.
const LINK_PATH: &str = "/sign-in/link";

/// Why a challenge could not be started or finished (a refusal is not an error: see
/// [`finish_code`] and [`finish_link`]).
#[derive(Debug, thiserror::Error)]
pub enum CodeError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Ceremony(#[from] CeremonyError),
    /// The message could not be accepted (no transactional sender is configured, most likely).
    #[error(transparent)]
    Mail(#[from] accept::Error),
}

/// A started email-code challenge.
#[derive(Debug, Clone)]
pub struct Started {
    /// The challenge (the ceremony's id), which the code finish names.
    pub challenge: Id<Challenge>,
    /// The `Set-Cookie` value of the ceremony cookie.
    pub cookie: String,
    /// When the code and the link stop working.
    pub expires_at: Timestamp,
}

/// The link a sign-in mail carries: the dashboard's page, with the token and the address in the
/// fragment, which browsers never send to a server or a referrer.
fn link(dashboard: &Url, token: &str, email: &EmailAddress) -> String {
    let mut url = dashboard.clone();
    url.set_path(LINK_PATH);
    let fragment = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", token)
        .append_pair("email", email.as_str())
        .finish();
    url.set_fragment(Some(&fragment));
    url.to_string()
}

/// Starts an email-code challenge for `email` in the caller's transaction (see the module).
///
/// # Errors
///
/// The random source or the database failed, or the message could not be accepted.
pub async fn start(
    tx: &mut Tx,
    keys: &Keys,
    email: &EmailAddress,
    ip_hash: Option<&[u8]>,
    dashboard: &Url,
) -> Result<Started, CodeError> {
    let ceremony = ceremonies::start_for(
        tx,
        keys,
        Kind::EmailCode,
        None,
        &json!({ "email": email.as_str() }),
    )
    .await?;
    let code = crypto::random_digits(DIGITS)?;
    let token = crypto::random_token(32)?;
    sqlx::query!(
        "INSERT INTO login_codes (email_key, purpose, code_hash, link_token_hash, ceremony_id, ip_hash, expires_at)
         VALUES (ascii_lower($1), 'sign_in', $2, $3, $4, $5, $6)",
        email.as_str(),
        keys.hash_code(ceremony.id.uuid().as_bytes(), &code),
        keys.hash_token(&token),
        ceremony.id.uuid(),
        ip_hash,
        ceremony.expires_at as _,
    )
    .execute(&mut **tx)
    .await?;
    accept::transactional(
        tx,
        keys,
        &Transactional::SignInCode {
            to: email,
            code: &code,
            link: &link(dashboard, &token, email),
            expires_at: ceremony.expires_at,
        },
    )
    .await?;
    Ok(Started {
        challenge: ceremony.id,
        cookie: ceremony.cookie,
        expires_at: ceremony.expires_at,
    })
}

/// The address a ceremony's sealed state holds.
fn email_of(state: &serde_json::Value) -> Option<EmailAddress> {
    state
        .get("email")
        .and_then(serde_json::Value::as_str)
        .and_then(|email| EmailAddress::parse(email).ok())
}

/// Checks `code` for challenge `challenge` from the browser holding `secret`: the address it
/// proves, or `None` for a refusal. The attempt is counted in the caller's transaction, which
/// must commit even on a refusal; a correct code consumes the code and its ceremony.
///
/// # Errors
///
/// The database failed, or the ceremony's state does not open.
pub async fn finish_code(
    tx: &mut Tx,
    keys: &Keys,
    challenge: Id<Challenge>,
    secret: &str,
    code: &str,
) -> Result<Option<EmailAddress>, CodeError> {
    let Some(ceremony) = ceremonies::open(tx, keys, challenge, Some(secret)).await? else {
        return Ok(None);
    };
    if ceremony.kind != Kind::EmailCode {
        return Ok(None);
    }
    let row = sqlx::query!(
        "UPDATE login_codes SET attempts = attempts + 1
          WHERE ceremony_id = $1 AND purpose = 'sign_in' AND consumed_at IS NULL
            AND expires_at > now() AND attempts < $2
         RETURNING id, code_hash",
        challenge.uuid(),
        MAX_ATTEMPTS,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let presented = keys.hash_code(challenge.uuid().as_bytes(), code.trim());
    if aws_lc_rs::constant_time::verify_slices_are_equal(&row.code_hash, &presented).is_err() {
        return Ok(None);
    }
    sqlx::query!(
        "UPDATE login_codes SET consumed_at = now() WHERE id = $1",
        row.id
    )
    .execute(&mut **tx)
    .await?;
    ceremonies::consume_opened(tx, challenge).await?;
    Ok(email_of(&ceremony.state))
}

/// Checks a link's `token` for `email`, from any browser: the address it proves, or `None` for a
/// refusal (an unknown, expired, used or locked link, or another address). Success consumes the
/// code and its ceremony.
///
/// # Errors
///
/// The database failed, or the ceremony's state does not open.
pub async fn finish_link(
    tx: &mut Tx,
    keys: &Keys,
    token: &str,
    email: &EmailAddress,
) -> Result<Option<EmailAddress>, CodeError> {
    let row = sqlx::query!(
        r#"SELECT id, email_key, ceremony_id AS "ceremony_id!" FROM login_codes
            WHERE link_token_hash = $1 AND purpose = 'sign_in' AND consumed_at IS NULL
              AND expires_at > now() AND attempts < $2 AND ceremony_id IS NOT NULL
              FOR UPDATE"#,
        keys.hash_token(token),
        MAX_ATTEMPTS,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.email_key != email.key() {
        return Ok(None);
    }
    let challenge = Id::<Challenge>::from_uuid(row.ceremony_id);
    let Some(ceremony) = ceremonies::open(tx, keys, challenge, None).await? else {
        return Ok(None);
    };
    sqlx::query!(
        "UPDATE login_codes SET consumed_at = now() WHERE id = $1",
        row.id
    )
    .execute(&mut **tx)
    .await?;
    ceremonies::consume_opened(tx, challenge).await?;
    Ok(email_of(&ceremony.state))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The link of a sign-in mail carries the token and the address in its fragment, on the
    /// dashboard's link page, so neither reaches a server log or a referrer.
    #[test]
    fn the_link_keeps_its_secret_in_the_fragment() {
        let dashboard = Url::parse("https://app.norbelys.test").unwrap();
        let email = EmailAddress::parse("Ada+x@Example.com").unwrap();
        let link = link(&dashboard, "t0k3n", &email);
        assert_eq!(
            link,
            "https://app.norbelys.test/sign-in/link#token=t0k3n&email=Ada%2Bx%40Example.com"
        );
    }
}
