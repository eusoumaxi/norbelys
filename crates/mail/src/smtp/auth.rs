//! SMTP authentication and SASL input validation, including the XOAUTH2 final-reply exchange.
//! The workaround is limited to the AUTH wire sequence; pooling and submission never inspect tokens.

use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::AsyncSmtpConnection;
use lettre::transport::smtp::commands::Auth;
use secrecy::ExposeSecret as _;
use tokio::time::Instant;

use super::protocol::{failed, negative, response_parts, within};
use super::{COMMAND_TIMEOUT, SmtpAuth};
use crate::submission::{Cause, Failure, Phase, Rejection, Scope};

pub(super) async fn authenticate(
    connection: &mut AsyncSmtpConnection,
    auth: &SmtpAuth<'_>,
    deadline: Instant,
) -> Result<(), Rejection> {
    let no_mechanism = |name: &str| {
        Rejection::local(
            Failure::Transient,
            Phase::Auth,
            Scope::Connection,
            Cause::Unauthorized,
            &format!("the server offers no {name} authentication"),
        )
    };
    let no_reply = || {
        Rejection::local(
            Failure::Transient,
            Phase::Auth,
            Scope::Connection,
            Cause::NoReply,
            "no AUTH reply before the deadline",
        )
    };
    match auth {
        SmtpAuth::Password { username, password } => {
            let mechanisms = [Mechanism::Plain, Mechanism::Login];
            if connection
                .server_info()
                .get_auth_mechanism(&mechanisms)
                .is_none()
            {
                return Err(no_mechanism("PLAIN or LOGIN"));
            }
            let credentials =
                Credentials::new((*username).to_owned(), password.expose_secret().to_owned());
            match within(
                deadline,
                COMMAND_TIMEOUT,
                connection.auth(&mechanisms, &credentials),
            )
            .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(failed(Phase::Auth, &error)),
                Err(_) => Err(no_reply()),
            }
        }
        SmtpAuth::Xoauth2 { username, token } => {
            if !connection
                .server_info()
                .supports_auth_mechanism(Mechanism::Xoauth2)
            {
                return Err(no_mechanism("XOAUTH2"));
            }
            let credentials =
                Credentials::new((*username).to_owned(), token.expose_secret().to_owned());
            let initial = Auth::new(Mechanism::Xoauth2, credentials, None)
                .map_err(|error| failed(Phase::Auth, &error))?;
            let first = match within(deadline, COMMAND_TIMEOUT, connection.command(initial)).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(failed(Phase::Auth, &error)),
                Err(_) => return Err(no_reply()),
            };
            if !first.has_code(334) {
                return Ok(());
            }
            // The server put its error in the challenge (a JSON status); an empty line asks for
            // the final reply, which `lettre`'s own AUTH loop would never read.
            match within(deadline, COMMAND_TIMEOUT, connection.command("\r\n")).await {
                Ok(Ok(response)) if response.code().is_positive() && !response.has_code(334) => {
                    Ok(())
                }
                Ok(Ok(response)) => {
                    let (code, text) = response_parts(&response);
                    Err(negative(Phase::Auth, code.max(535), &text))
                }
                Ok(Err(error)) => Err(failed(Phase::Auth, &error)),
                Err(_) => Err(no_reply()),
            }
        }
    }
}

/// Rejects credentials that could inject SASL fields: control characters in the login, a
/// `NUL` in a password (the `PLAIN` separator), anything but printable ASCII in a token.
pub(super) fn check_credential(auth: &SmtpAuth<'_>) -> Result<(), Rejection> {
    let (username, valid) = match auth {
        SmtpAuth::Password { username, password } => {
            (username, !password.expose_secret().contains('\0'))
        }
        SmtpAuth::Xoauth2 { username, token } => {
            let token = token.expose_secret();
            (
                username,
                !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()),
            )
        }
    };
    if !username.is_empty() && !username.chars().any(char::is_control) && valid {
        return Ok(());
    }
    Err(Rejection::local(
        Failure::Transient,
        Phase::Auth,
        Scope::Connection,
        Cause::Unauthorized,
        "the credential contains characters SASL cannot carry",
    ))
}
