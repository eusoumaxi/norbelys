//! TCP/TLS greeting, mandatory STARTTLS and authentication of a fresh SMTP session.
//! This module owns no pool state or permits; the caller supplies its cached TLS parameters.

use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use tokio::time::Instant;

use super::auth::authenticate;
use super::protocol::{failed, within};
use super::{COMMAND_TIMEOUT, CONNECT_TIMEOUT, SmtpAuth, SmtpSecurity, SmtpServer};
use crate::net::{AddressPolicy, Connector};
use crate::submission::{Cause, Failure, Phase, Rejection, Scope};

pub(super) async fn open(
    connector: &Connector,
    hello: &ClientId,
    server: &SmtpServer<'_>,
    auth: &SmtpAuth<'_>,
    deadline: Instant,
    tls_parameters: impl FnOnce(&str) -> Result<TlsParameters, Rejection>,
) -> Result<AsyncSmtpConnection, Rejection> {
    let unreachable = |detail: &str| {
        Rejection::local(
            Failure::Transient,
            Phase::Connect,
            Scope::Connection,
            Cause::NoReply,
            detail,
        )
    };
    if server.security == SmtpSecurity::Plain && connector.policy() != AddressPolicy::Any {
        return Err(Rejection::local(
            Failure::Transient,
            Phase::Connect,
            Scope::Connection,
            Cause::Refused,
            "plaintext SMTP requires AddressPolicy::Any (non-public hosts)",
        ));
    }
    let address = match within(
        deadline,
        CONNECT_TIMEOUT,
        connector.resolve(server.host, server.port),
    )
    .await
    {
        Ok(Ok(address)) => address,
        Ok(Err(error)) => return Err(unreachable(&error.to_string())),
        Err(_) => return Err(unreachable("the host did not resolve before the deadline")),
    };
    let tls = match server.security {
        SmtpSecurity::Tls | SmtpSecurity::StartTls => Some(tls_parameters(server.host)?),
        SmtpSecurity::Plain => None,
    };
    let implicit = tls.clone().filter(|_| server.security == SmtpSecurity::Tls);
    let connecting =
        AsyncSmtpConnection::connect_tokio1(address, Some(CONNECT_TIMEOUT), hello, implicit, None);
    let greeting = CONNECT_TIMEOUT.saturating_add(COMMAND_TIMEOUT);
    let mut connection = match within(deadline, greeting, connecting).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => return Err(failed(Phase::Connect, &error)),
        Err(_) => return Err(unreachable("no greeting or EHLO reply before the deadline")),
    };
    if let (SmtpSecurity::StartTls, Some(tls)) = (server.security, tls) {
        if !connection.can_starttls() {
            return Err(Rejection::local(
                Failure::Transient,
                Phase::Connect,
                Scope::Connection,
                Cause::Refused,
                "the server does not offer STARTTLS",
            ));
        }
        match within(deadline, COMMAND_TIMEOUT, connection.starttls(tls, hello)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(failed(Phase::Connect, &error)),
            Err(_) => return Err(unreachable("no STARTTLS reply before the deadline")),
        }
    }
    authenticate(&mut connection, auth, deadline).await?;
    Ok(connection)
}
