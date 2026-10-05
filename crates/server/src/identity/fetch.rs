//! The bounded fetcher of identity documents: OpenID Connect discovery documents, key sets
//! (JWKS) and token endpoints of the identity providers workspaces configure.
//!
//! An SSO connection's issuer is a URL an administrator typed, so every request to it is a request
//! to a place a customer chose. The fetcher therefore applies the outbound address guard every
//! customer URL goes through (`webhooks::deliver::Guard`): `https` only, a literal inward address
//! refused before the request, and every name resolved by the guard, which refuses the whole
//! answer when any address is private, loopback or reserved; the check runs when the connection
//! is made, so DNS rebinding cannot slip an inward address in after a check. On top of the guard,
//! each request is bounded: 5 seconds in all, at most 64 KiB of answer, no redirects followed
//! (a redirect is an error, as an identity provider's documents are at stable URLs), no proxy
//! from the environment. A development deployment may allow private addresses and plain `http`
//! (`IDENTITY_ALLOW_PRIVATE_ISSUERS`), which is also how tests reach a fake provider on the
//! loopback interface.
//!
//! [`Fetcher::execute`] is also the HTTP client of the OpenID Connect library (discovery, the
//! token exchange), so no request of a sign-in leaves through another client.

use std::time::Duration;

use axum::http::{self, HeaderValue, header};

use crate::webhooks::deliver::{Guard, Refused, check_target};

/// The deadline of one request, connection included.
const DEADLINE: Duration = Duration::from_secs(5);
/// The largest answer read.
const MAX_BODY: usize = 64 * 1024;

/// Why a fetch failed. Never carries the answer's body.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The URL is malformed, not `https`, or names an inward address.
    #[error("the URL is not allowed: {0}")]
    Refused(#[from] Refused),
    /// The request could not be built.
    #[error("the request is malformed")]
    Request,
    /// The request failed: no connection, a refused name, a timeout.
    #[error("the request failed: {0}")]
    Transport(String),
    /// The answer is larger than 64 KiB.
    #[error("the answer is larger than 64 KiB")]
    TooLarge,
    /// The answer is not a success (`2xx`) or is not JSON where JSON was asked for.
    #[error("the answer was HTTP {status}")]
    Status {
        /// The answer's status.
        status: u16,
    },
    /// The answer is not a JSON document.
    #[error("the answer is not a JSON document")]
    NotJson,
}

/// The client of identity documents.
#[derive(Clone, Debug)]
pub struct Fetcher {
    client: reqwest::Client,
    allow_private: bool,
}

impl Fetcher {
    /// Builds the client. `allow_private` admits private and loopback addresses and plain
    /// `http`: development and tests only.
    ///
    /// # Errors
    ///
    /// The TLS backend cannot be initialised.
    pub fn new(allow_private: bool) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(DEADLINE)
            .connect_timeout(DEADLINE)
            .user_agent(concat!("Norbelys-Identity/", env!("CARGO_PKG_VERSION")))
            .dns_resolver(Guard { allow_private })
            .build()?;
        Ok(Self {
            client,
            allow_private,
        })
    }

    /// GETs `url` and parses the answer as JSON.
    ///
    /// # Errors
    ///
    /// See [`FetchError`].
    pub async fn json(&self, url: &url::Url) -> Result<serde_json::Value, FetchError> {
        let request = http::Request::get(url.as_str())
            .header(header::ACCEPT, HeaderValue::from_static("application/json"))
            .body(Vec::new())
            .map_err(|_| FetchError::Request)?;
        let response = self.execute(request).await?;
        if !response.status().is_success() {
            return Err(FetchError::Status {
                status: response.status().as_u16(),
            });
        }
        serde_json::from_slice(response.body()).map_err(|_| FetchError::NotJson)
    }

    /// Sends `request` through the guard and the bounds, and reads at most 64 KiB of answer.
    /// Any status is returned as it is; the caller decides what a non-success means.
    ///
    /// # Errors
    ///
    /// The URL is refused, the request fails or times out, or the answer is too large.
    pub async fn execute(
        &self,
        request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, FetchError> {
        let url =
            reqwest::Url::parse(&request.uri().to_string()).map_err(|_| FetchError::Request)?;
        check_target(&url, self.allow_private)?;
        let (parts, body) = request.into_parts();
        let mut outgoing = self.client.request(parts.method, url).body(body);
        for (name, value) in &parts.headers {
            outgoing = outgoing.header(name, value);
        }
        let mut response = outgoing
            .send()
            .await
            .map_err(|error| FetchError::Transport(describe(&error)))?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| FetchError::Transport(describe(&error)))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let mut answer = http::Response::builder().status(status);
        if let Some(target) = answer.headers_mut() {
            target.extend(headers);
        }
        answer.body(body).map_err(|_| FetchError::Request)
    }
}

/// A transport error's description, with its source chain (a refused name says why).
fn describe(error: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut text = if error.is_timeout() {
        "the request timed out".to_owned()
    } else {
        error.to_string()
    };
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sink;

    /// A document a customer's URL points at is read when the URL is allowed, and the fetcher
    /// refuses inward addresses and plain `http` unless private targets are allowed: the guard
    /// that keeps an administrator's issuer URL from reaching our own network.
    #[tokio::test]
    async fn documents_are_read_only_from_allowed_places() {
        let sink = Sink::start().await;
        let url = url::Url::parse(&sink.url("/200")).unwrap();
        let open = Fetcher::new(true).unwrap();
        let guarded = Fetcher::new(false).unwrap();
        assert!(matches!(open.json(&url).await, Err(FetchError::NotJson),));
        assert!(matches!(
            guarded.json(&url).await,
            Err(FetchError::Refused(Refused::Scheme))
        ));
        let inward = url::Url::parse("https://127.0.0.1/.well-known/openid-configuration").unwrap();
        assert!(matches!(
            guarded.json(&inward).await,
            Err(FetchError::Refused(Refused::Private))
        ));
        let missing = url::Url::parse(&sink.url("/404")).unwrap();
        assert!(matches!(
            open.json(&missing).await,
            Err(FetchError::Status { status: 404 })
        ));
    }
}
