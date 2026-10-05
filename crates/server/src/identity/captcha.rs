//! The captcha on the email-code challenge: the one anonymous request that makes us send mail.
//!
//! # When it is on
//!
//! The captcha is on when a provider and its credentials are configured (`CAPTCHA_PROVIDER`,
//! `CAPTCHA_SITE_KEY`, `CAPTCHA_SECRET`, and for reCAPTCHA `RECAPTCHA_PROJECT`), off otherwise.
//! `GET /v1/auth/config` gives the dashboard the provider and the site key, so the widget it
//! shows and the check made here always agree. A token is single-use and short-lived (120
//! seconds at Google and hCaptcha, 300 at Turnstile), so the dashboard fetches one at submit.
//!
//! # The checks, per provider
//!
//! - **Turnstile** (`siteverify`, <https://developers.cloudflare.com/turnstile/get-started/server-side-validation/>):
//!   the secret, the token, the client address and an idempotency key; requires `success`, a
//!   `hostname` that is one of the dashboard's and the action `sign_in`.
//! - **hCaptcha** (`siteverify`, form-encoded, <https://docs.hcaptcha.com/#verify-the-user-response-server-side>):
//!   the secret, the token, the expected site key and the client address; requires `success`. Its
//!   `hostname` is reported by the browser and may be `not-provided`, so it is logged, never
//!   trusted.
//! - **reCAPTCHA Enterprise** (`projects.assessments.create`,
//!   <https://cloud.google.com/recaptcha/docs/create-assessment-website>): the project and an API key
//!   allowed to create assessments; requires `tokenProperties.valid`, a dashboard hostname, the
//!   action `sign_in`, then a score of at least 0.5.
//!
//! # Outcomes
//!
//! - An invalid, expired, reused or mismatched token is [`Verdict::Invalid`]: the challenge answers
//!   `422 captcha_failed` and is never let through.
//! - A timeout (3 seconds), a network error, a provider `5xx` or a quota refusal (`429`) is
//!   [`Verdict::Unavailable`] with [`Outage::Unreachable`]: the challenge proceeds under a tighter
//!   rate limit (one per email address and per client address per 15 minutes) and the outage is
//!   logged as `captcha.unavailable` and counted, for the alert. The email code itself still proves
//!   the inbox; failing open is the deployment's documented choice.
//! - A refused secret, key or project is [`Outage::Configuration`]: also unavailable for the
//!   request, and logged at error level because only an operator can fix it.

use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use url::Url;

/// How long the provider may take.
const DEADLINE: Duration = Duration::from_secs(3);
/// The action the dashboard's widget declares, checked where the provider reports it.
pub const ACTION: &str = "sign_in";
/// The lowest reCAPTCHA score admitted.
const MIN_SCORE: f64 = 0.5;

static OUTAGES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_captcha_unavailable_total")
        .with_description(
            "Captcha verifications that could not reach a verdict, by provider and reason.",
        )
        .build()
});

/// A captcha provider.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    clap::ValueEnum,
    strum::EnumIter,
    strum::IntoStaticStr,
    serde::Serialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = CaptchaProvider)]
pub enum Provider {
    /// Cloudflare Turnstile.
    Turnstile,
    /// hCaptcha.
    Hcaptcha,
    /// Google reCAPTCHA Enterprise.
    Recaptcha,
}

impl Provider {
    /// The provider's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The provider's verification endpoint (reCAPTCHA's carries the project).
    fn endpoint(self, project: Option<&str>) -> Option<Url> {
        let url = match self {
            Self::Turnstile => {
                "https://challenges.cloudflare.com/turnstile/v0/siteverify".to_owned()
            }
            Self::Hcaptcha => "https://api.hcaptcha.com/siteverify".to_owned(),
            Self::Recaptcha => format!(
                "https://recaptchaenterprise.googleapis.com/v1/projects/{}/assessments",
                project?
            ),
        };
        Url::parse(&url).ok()
    }
}

/// Why the provider could not give a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Outage {
    /// A timeout, a network error, a `5xx` or a quota refusal.
    Unreachable,
    /// The provider refused our secret, key or project.
    Configuration,
}

/// The verdict on one token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A person solved it, for this site and this action.
    Valid,
    /// Invalid, expired, reused or for another site or action.
    Invalid,
    /// No verdict (see [`Outage`]).
    Unavailable(Outage),
}

/// Why the captcha configuration was refused at start.
#[derive(Debug, thiserror::Error)]
pub enum CaptchaError {
    /// A provider is named without its site key or secret, or reCAPTCHA without its project.
    #[error(
        "the captcha needs CAPTCHA_SITE_KEY and CAPTCHA_SECRET (and RECAPTCHA_PROJECT for reCAPTCHA)"
    )]
    Incomplete,
    /// The HTTP client could not be built.
    #[error("the captcha HTTP client could not be built: {0}")]
    Client(#[from] reqwest::Error),
}

/// A configured captcha.
#[derive(Clone)]
pub struct Captcha {
    provider: Provider,
    site_key: String,
    secret: SecretString,
    hostnames: Vec<String>,
    endpoint: Url,
    client: reqwest::Client,
}

impl std::fmt::Debug for Captcha {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Captcha")
            .field("provider", &self.provider)
            .field("site_key", &self.site_key)
            .finish_non_exhaustive()
    }
}

impl Captcha {
    /// The captcha `provider` with its credentials, checking tokens for the dashboard's
    /// `hostnames`; `None` when no provider is configured.
    ///
    /// # Errors
    ///
    /// A provider is named but its credentials are incomplete, or the client cannot be built.
    pub fn new(
        provider: Option<Provider>,
        site_key: Option<String>,
        secret: Option<SecretString>,
        project: Option<&str>,
        hostnames: Vec<String>,
    ) -> Result<Option<Self>, CaptchaError> {
        let Some(provider) = provider else {
            return Ok(None);
        };
        let (Some(site_key), Some(secret)) = (site_key, secret) else {
            return Err(CaptchaError::Incomplete);
        };
        let endpoint = provider.endpoint(project).ok_or(CaptchaError::Incomplete)?;
        Ok(Some(Self {
            provider,
            site_key,
            secret,
            hostnames,
            endpoint,
            client: reqwest::Client::builder()
                .timeout(DEADLINE)
                .connect_timeout(DEADLINE)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        }))
    }

    /// A captcha that verifies at `endpoint` instead of the provider's own: tests point it at a
    /// fake provider.
    #[cfg(test)]
    pub(crate) fn at(mut self, endpoint: Url) -> Self {
        self.endpoint = endpoint;
        self
    }

    /// The provider.
    #[must_use]
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The public site key the dashboard's widget uses.
    #[must_use]
    pub fn site_key(&self) -> &str {
        &self.site_key
    }

    /// Verifies `token` for a request from `address`; an outage is logged and counted.
    pub async fn verify(&self, token: &str, address: Option<IpAddr>) -> Verdict {
        let verdict = match self.ask(token, address).await {
            Ok((status, body)) => decide(self.provider, status, body.as_ref(), &self.hostnames),
            Err(()) => Verdict::Unavailable(Outage::Unreachable),
        };
        if let Verdict::Unavailable(outage) = verdict {
            let reason: &'static str = outage.into();
            OUTAGES.add(
                1,
                &[
                    KeyValue::new("provider", self.provider.as_str()),
                    KeyValue::new("reason", reason),
                ],
            );
            match outage {
                Outage::Unreachable => tracing::warn!(
                    event = "captcha.unavailable",
                    provider = self.provider.as_str(),
                    reason,
                    "the captcha provider gave no verdict; the challenge proceeds under the tighter limit"
                ),
                Outage::Configuration => tracing::error!(
                    event = "captcha.unavailable",
                    provider = self.provider.as_str(),
                    reason,
                    "the captcha provider refused our configuration; an operator must fix it"
                ),
            }
        }
        verdict
    }

    /// Calls the provider; the answer's status and its JSON body when it has one.
    async fn ask(&self, token: &str, address: Option<IpAddr>) -> Result<(u16, Option<Value>), ()> {
        let remote_ip = address.map(|address| address.to_string());
        let request = match self.provider {
            Provider::Turnstile => self.client.post(self.endpoint.clone()).json(&json!({
                "secret": self.secret.expose_secret(),
                "response": token,
                "remoteip": remote_ip,
                "idempotency_key": uuid::Uuid::new_v4().to_string(),
            })),
            Provider::Hcaptcha => {
                let mut form = vec![
                    ("secret", self.secret.expose_secret().to_owned()),
                    ("response", token.to_owned()),
                    ("sitekey", self.site_key.clone()),
                ];
                if let Some(remote_ip) = remote_ip {
                    form.push(("remoteip", remote_ip));
                }
                self.client.post(self.endpoint.clone()).form(&form)
            }
            Provider::Recaptcha => {
                let mut url = self.endpoint.clone();
                url.query_pairs_mut()
                    .append_pair("key", self.secret.expose_secret());
                self.client.post(url).json(&json!({
                    "event": {
                        "token": token,
                        "siteKey": self.site_key,
                        "expectedAction": ACTION,
                        "userIpAddress": remote_ip,
                    }
                }))
            }
        };
        let response = request.send().await.map_err(|_| ())?;
        let status = response.status().as_u16();
        let body = response.json::<Value>().await.ok();
        Ok((status, body))
    }
}

/// The verdict a provider's answer carries (see the module): `status` is the HTTP status and
/// `body` the JSON answer, absent when the answer was not JSON.
#[must_use]
pub fn decide(
    provider: Provider,
    status: u16,
    body: Option<&Value>,
    hostnames: &[String],
) -> Verdict {
    match status {
        401 | 403 => return Verdict::Unavailable(Outage::Configuration),
        400 if provider == Provider::Recaptcha => {
            return Verdict::Unavailable(Outage::Configuration);
        }
        200..=299 => {}
        _ => return Verdict::Unavailable(Outage::Unreachable),
    }
    let Some(body) = body else {
        return Verdict::Unavailable(Outage::Unreachable);
    };
    let text = |pointer: &str| body.pointer(pointer).and_then(Value::as_str);
    let ours =
        |pointer: &str| text(pointer).is_some_and(|host| hostnames.iter().any(|ours| ours == host));
    let errors: Vec<&str> = body
        .get("error-codes")
        .and_then(Value::as_array)
        .map(|codes| codes.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let success = body.get("success").and_then(Value::as_bool) == Some(true);
    match provider {
        Provider::Turnstile => {
            if errors
                .iter()
                .any(|code| matches!(*code, "missing-input-secret" | "invalid-input-secret"))
            {
                Verdict::Unavailable(Outage::Configuration)
            } else if errors.contains(&"internal-error") {
                Verdict::Unavailable(Outage::Unreachable)
            } else if success && ours("/hostname") && text("/action") == Some(ACTION) {
                Verdict::Valid
            } else {
                Verdict::Invalid
            }
        }
        Provider::Hcaptcha => {
            if errors.iter().any(|code| {
                matches!(
                    *code,
                    "missing-input-secret" | "invalid-input-secret" | "sitekey-secret-mismatch"
                )
            }) {
                Verdict::Unavailable(Outage::Configuration)
            } else if success {
                Verdict::Valid
            } else {
                Verdict::Invalid
            }
        }
        Provider::Recaptcha => {
            let valid = body
                .pointer("/tokenProperties/valid")
                .and_then(Value::as_bool)
                == Some(true);
            let score = body
                .pointer("/riskAnalysis/score")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if valid
                && ours("/tokenProperties/hostname")
                && text("/tokenProperties/action") == Some(ACTION)
                && score >= MIN_SCORE
            {
                Verdict::Valid
            } else {
                Verdict::Invalid
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;
    use crate::testing::Sink;

    fn hosts() -> Vec<String> {
        vec!["app.norbelys.test".to_owned()]
    }

    /// Each provider's answers map to the verdict the module states: a solved token for our host
    /// and action is valid; a failed, foreign or low-score one is invalid and never let through; a
    /// refused secret or project is a configuration outage; an internal error, a `5xx`, a quota
    /// refusal or an answer that is not JSON is an outage the challenge survives.
    #[test]
    fn provider_answers_map_to_verdicts() {
        let turnstile = |body: Value| decide(Provider::Turnstile, 200, Some(&body), &hosts());
        assert_eq!(
            turnstile(
                json!({"success": true, "hostname": "app.norbelys.test", "action": "sign_in"})
            ),
            Verdict::Valid
        );
        assert_eq!(
            turnstile(json!({"success": true, "hostname": "evil.test", "action": "sign_in"})),
            Verdict::Invalid
        );
        assert_eq!(
            turnstile(json!({"success": true, "hostname": "app.norbelys.test", "action": "other"})),
            Verdict::Invalid
        );
        assert_eq!(
            turnstile(json!({"success": false, "error-codes": ["timeout-or-duplicate"]})),
            Verdict::Invalid
        );
        assert_eq!(
            turnstile(json!({"success": false, "error-codes": ["invalid-input-secret"]})),
            Verdict::Unavailable(Outage::Configuration)
        );
        assert_eq!(
            turnstile(json!({"success": false, "error-codes": ["internal-error"]})),
            Verdict::Unavailable(Outage::Unreachable)
        );

        let hcaptcha = |body: Value| decide(Provider::Hcaptcha, 200, Some(&body), &hosts());
        assert_eq!(
            hcaptcha(json!({"success": true, "hostname": "not-provided"})),
            Verdict::Valid,
            "hCaptcha's hostname is browser-reported and never trusted"
        );
        assert_eq!(
            hcaptcha(json!({"success": false, "error-codes": ["already-seen-response"]})),
            Verdict::Invalid
        );
        assert_eq!(
            hcaptcha(json!({"success": false, "error-codes": ["sitekey-secret-mismatch"]})),
            Verdict::Unavailable(Outage::Configuration)
        );

        let recaptcha = |valid: bool, host: &str, action: &str, score: f64| {
            let body = json!({
                "tokenProperties": {"valid": valid, "hostname": host, "action": action},
                "riskAnalysis": {"score": score},
            });
            decide(Provider::Recaptcha, 200, Some(&body), &hosts())
        };
        assert_eq!(
            recaptcha(true, "app.norbelys.test", "sign_in", 0.9),
            Verdict::Valid
        );
        assert_eq!(
            recaptcha(true, "app.norbelys.test", "sign_in", 0.4),
            Verdict::Invalid
        );
        assert_eq!(
            recaptcha(false, "app.norbelys.test", "sign_in", 0.9),
            Verdict::Invalid
        );
        assert_eq!(
            recaptcha(true, "other.test", "sign_in", 0.9),
            Verdict::Invalid
        );
        assert_eq!(
            recaptcha(true, "app.norbelys.test", "login", 0.9),
            Verdict::Invalid
        );
        assert_eq!(
            decide(Provider::Recaptcha, 400, None, &hosts()),
            Verdict::Unavailable(Outage::Configuration)
        );

        for provider in Provider::iter() {
            for status in [429, 500, 503] {
                assert_eq!(
                    decide(provider, status, None, &hosts()),
                    Verdict::Unavailable(Outage::Unreachable),
                    "{provider:?} {status}"
                );
            }
            assert_eq!(
                decide(provider, 403, None, &hosts()),
                Verdict::Unavailable(Outage::Configuration),
                "{provider:?}"
            );
            assert_eq!(
                decide(provider, 200, None, &hosts()),
                Verdict::Unavailable(Outage::Unreachable),
                "{provider:?}"
            );
        }
    }

    /// The call reaches the configured endpoint and turns what comes back into a verdict: a
    /// provider answering `503` or something that is not JSON leaves the request without a
    /// verdict, never with a pass.
    #[tokio::test]
    async fn an_unreachable_provider_gives_no_verdict() {
        let sink = Sink::start().await;
        for provider in Provider::iter() {
            let captcha = Captcha::new(
                Some(provider),
                Some("site".to_owned()),
                Some(SecretString::from("secret")),
                Some("project"),
                hosts(),
            )
            .unwrap()
            .unwrap();
            for path in ["/503", "/200"] {
                let at = captcha.clone().at(Url::parse(&sink.url(path)).unwrap());
                assert_eq!(
                    at.verify("token", None).await,
                    Verdict::Unavailable(Outage::Unreachable),
                    "{provider:?} {path}"
                );
            }
        }
        assert_eq!(sink.requests().len(), 6);
    }
}
