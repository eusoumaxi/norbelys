use std::time::Duration;

use jiff::Timestamp;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::json;
use tokio::time::Instant;
use url::Url;

use super::{
    App, AuthorizationRequest, GOOGLE_API_SCOPES, MICROSOFT_GRAPH_SCOPES, OAuthError, Provider,
    authorization_url, check_scopes, exchange, refresh, refusal,
};
use crate::http::HttpClient;
use crate::testing::{Request, Response, http_server};

/// The PKCE example of RFC 7636 Appendix B.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn app() -> App {
    App {
        client_id: "client-123".to_owned(),
        client_secret: SecretString::from("shh".to_owned()),
        redirect_uri: Url::parse("https://app.example.com/v1/auth/callback").expect("a URL"),
    }
}

fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn form(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

fn field(fields: &[(String, String)], name: &str) -> Option<String> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// Google's consent URL carries the authorization-code flow with PKCE (the S256 challenge of the
/// verifier, checked against RFC 7636's example), the state and nonce the callback checks, the
/// account hint of a reconnect, and the parameters that make Google issue a refresh token every
/// time.
#[test]
fn the_google_consent_url_asks_for_code_pkce_and_a_refresh_token() {
    let verifier = SecretString::from(VERIFIER.to_owned());
    let request = AuthorizationRequest {
        state: "st4te",
        nonce: "n0nce",
        code_verifier: &verifier,
        login_hint: Some("ada@example.com"),
        scopes: GOOGLE_API_SCOPES,
    };
    let url = authorization_url(&Provider::Google, &app(), &request).expect("a URL");
    assert_eq!(url.host_str(), Some("accounts.google.com"));
    assert_eq!(url.path(), "/o/oauth2/v2/auth");
    for (name, value) in [
        ("client_id", "client-123"),
        ("response_type", "code"),
        ("redirect_uri", "https://app.example.com/v1/auth/callback"),
        ("state", "st4te"),
        ("nonce", "n0nce"),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("login_hint", "ada@example.com"),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ] {
        assert_eq!(query(&url, name).as_deref(), Some(value), "{name}");
    }
    assert_eq!(query(&url, "scope"), Some(GOOGLE_API_SCOPES.join(" ")));
}

/// Microsoft's consent URL is the tenant's v2.0 endpoint with the code returned in the query; a
/// redirect URI that is not `https`, or a tenant that is not a plain segment, is refused.
#[test]
fn the_microsoft_consent_url_uses_the_tenant_and_refuses_bad_inputs() {
    let verifier = SecretString::from(VERIFIER.to_owned());
    let request = AuthorizationRequest {
        state: "s",
        nonce: "n",
        code_verifier: &verifier,
        login_hint: None,
        scopes: MICROSOFT_GRAPH_SCOPES,
    };
    let microsoft = Provider::Microsoft {
        tenant: "organizations".to_owned(),
    };
    let url = authorization_url(&microsoft, &app(), &request).expect("a URL");
    assert_eq!(url.path(), "/organizations/oauth2/v2.0/authorize");
    assert_eq!(query(&url, "response_mode").as_deref(), Some("query"));
    assert_eq!(query(&url, "access_type"), None);
    assert_eq!(query(&url, "login_hint"), None);

    let plain = App {
        redirect_uri: Url::parse("http://app.example.com/callback").expect("a URL"),
        ..app()
    };
    assert!(matches!(
        authorization_url(&Provider::Google, &plain, &request),
        Err(OAuthError::Invalid(_))
    ));
    let odd = Provider::Microsoft {
        tenant: "common/../x".to_owned(),
    };
    assert!(matches!(
        authorization_url(&odd, &app(), &request),
        Err(OAuthError::Invalid(_))
    ));
}

/// With granular consent a person can untick a permission, so the granted `scope` is checked:
/// Google's full URLs as listed, Microsoft's with or without the Graph prefix and in any case;
/// `openid`, `profile` and `offline_access` are never echoed and never required; what is missing
/// is named.
#[test]
fn granted_scopes_are_checked_in_each_providers_form() {
    assert_eq!(
        check_scopes(
            "openid https://www.googleapis.com/auth/gmail.send https://www.googleapis.com/auth/gmail.readonly",
            GOOGLE_API_SCOPES
        ),
        Ok(())
    );
    assert_eq!(
        check_scopes(
            "openid https://www.googleapis.com/auth/gmail.send",
            GOOGLE_API_SCOPES
        ),
        Err(OAuthError::ScopeMissing {
            missing: vec!["https://www.googleapis.com/auth/gmail.readonly".to_owned()]
        })
    );
    assert_eq!(
        check_scopes(
            "User.Read mail.send Mail.Read profile openid email",
            MICROSOFT_GRAPH_SCOPES
        ),
        Ok(())
    );
    assert_eq!(
        check_scopes(
            "https://graph.microsoft.com/Mail.Read https://graph.microsoft.com/Mail.Send https://graph.microsoft.com/User.Read",
            MICROSOFT_GRAPH_SCOPES
        ),
        Ok(())
    );
}

/// A refresh error decides the connection's fate, so only the OAuth `error` code is branched
/// on: `invalid_grant` is a lost grant, Google's `admin_policy_enforced` an admin block,
/// Microsoft's `interaction_required` a sign-in the person must redo, the configuration codes are
/// ours, and throttling or an outage is temporary. Microsoft's `AADSTS` numbers are kept for
/// support.
#[test]
fn refresh_errors_map_onto_what_they_mean() {
    let body = |value: serde_json::Value| value.to_string().into_bytes();
    assert_eq!(
        refusal(
            400,
            &body(
                json!({"error": "invalid_grant", "error_description": "AADSTS70000: revoked", "error_codes": [70000]})
            )
        ),
        OAuthError::InvalidGrant {
            description: "AADSTS70000: revoked".to_owned(),
            codes: vec![70000]
        }
    );
    assert!(matches!(
        refusal(400, &body(json!({"error": "admin_policy_enforced"}))),
        OAuthError::AdminPolicyEnforced { .. }
    ));
    assert!(matches!(
        refusal(400, &body(json!({"error": "interaction_required", "error_codes": [50076]}))),
        OAuthError::InteractionRequired { ref codes, .. } if codes == &[50076]
    ));
    assert!(matches!(
        refusal(400, &body(json!({"error": "temporarily_unavailable"}))),
        OAuthError::Unavailable(_)
    ));
    assert!(matches!(
        refusal(503, b"Service Unavailable"),
        OAuthError::Unavailable(_)
    ));
    assert!(matches!(refusal(429, b""), OAuthError::Unavailable(_)));
    assert!(matches!(
        refusal(401, &body(json!({"error": "invalid_client"}))),
        OAuthError::Refused { ref error, .. } if error == "invalid_client"
    ));
    assert!(matches!(refusal(400, b""), OAuthError::Refused { ref error, .. } if error == "400"));
}

/// A Microsoft refresh posts the refresh token with our client credentials and the same scopes,
/// and the rotated refresh token and the expiry come back; a Google refresh sends no scope.
#[tokio::test]
async fn a_refresh_returns_the_rotated_token_and_expiry() {
    let (origin, requests) = http_server(|_, _| {
        Response::json(200, &json!({
            "token_type": "Bearer",
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "expires_in": 3599,
            "scope": "https://graph.microsoft.com/Mail.Send https://graph.microsoft.com/Mail.Read https://graph.microsoft.com/User.Read",
        }))
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let refresh_token = SecretString::from("refresh-1".to_owned());
    let deadline = Instant::now() + Duration::from_secs(5);
    let microsoft = Provider::Microsoft {
        tenant: "common".to_owned(),
    };
    let before = Timestamp::now();
    let tokens = refresh(
        &http,
        &microsoft,
        &app(),
        &refresh_token,
        MICROSOFT_GRAPH_SCOPES,
        deadline,
    )
    .await
    .expect("refreshed");
    assert_eq!(tokens.access_token.expose_secret(), "access-2");
    assert_eq!(
        tokens
            .refresh_token
            .as_ref()
            .map(|token| token.expose_secret().to_owned())
            .as_deref(),
        Some("refresh-2")
    );
    let lifetime = tokens.expires_at.duration_since(before).as_secs();
    assert!((3_598..=3_600).contains(&lifetime), "{lifetime}");
    assert_eq!(check_scopes(&tokens.scope, MICROSOFT_GRAPH_SCOPES), Ok(()));

    refresh(
        &http,
        &Provider::Google,
        &app(),
        &refresh_token,
        GOOGLE_API_SCOPES,
        deadline,
    )
    .await
    .expect("refreshed");
    let sent = requests.all();
    let [to_microsoft, to_google] = sent.as_slice() else {
        panic!("two requests: {sent:?}")
    };
    assert_eq!(to_microsoft.path(), "/common/oauth2/v2.0/token");
    let fields = form(to_microsoft);
    assert_eq!(
        field(&fields, "grant_type").as_deref(),
        Some("refresh_token")
    );
    assert_eq!(
        field(&fields, "refresh_token").as_deref(),
        Some("refresh-1")
    );
    assert_eq!(field(&fields, "client_id").as_deref(), Some("client-123"));
    assert_eq!(field(&fields, "client_secret").as_deref(), Some("shh"));
    assert_eq!(
        field(&fields, "scope"),
        Some(MICROSOFT_GRAPH_SCOPES.join(" "))
    );
    assert_eq!(to_google.path(), "/token");
    assert_eq!(field(&form(to_google), "scope"), None);
}

/// The code exchange sends the code with its PKCE verifier and redirect URI, and returns the ID
/// token for the caller to verify; a lifetime written as a string is accepted, a token that is
/// not a bearer token is not, and a refused code maps like a refused refresh.
#[tokio::test]
async fn the_code_exchange_returns_tokens_and_refuses_bad_answers() {
    let (origin, requests) = http_server(|request, _| {
        let fields = form(request);
        match field(&fields, "code").as_deref() {
            Some("good") => Response::json(200, &json!({"token_type": "bearer", "access_token": "a", "expires_in": "3599", "id_token": "eyJ.x.y", "scope": "openid"})),
            Some("mac") => Response::json(200, &json!({"token_type": "mac", "access_token": "a", "expires_in": 3599})),
            _ => Response::json(400, &json!({"error": "invalid_grant", "error_description": "Bad Request"})),
        }
    })
    .await;
    let http = HttpClient::rebased(&origin).expect("a client");
    let verifier = SecretString::from(VERIFIER.to_owned());
    let deadline = Instant::now() + Duration::from_secs(5);
    let code = |text: &str| SecretString::from(text.to_owned());
    let tokens = exchange(
        &http,
        &Provider::Google,
        &app(),
        &code("good"),
        &verifier,
        deadline,
    )
    .await
    .expect("exchanged");
    assert_eq!(tokens.id_token.as_deref(), Some("eyJ.x.y"));
    assert!(tokens.refresh_token.is_none());
    let fields = form(requests.all().first().expect("a request"));
    assert_eq!(
        field(&fields, "grant_type").as_deref(),
        Some("authorization_code")
    );
    assert_eq!(field(&fields, "code_verifier").as_deref(), Some(VERIFIER));
    assert_eq!(
        field(&fields, "redirect_uri").as_deref(),
        Some("https://app.example.com/v1/auth/callback")
    );
    assert!(matches!(
        exchange(
            &http,
            &Provider::Google,
            &app(),
            &code("mac"),
            &verifier,
            deadline
        )
        .await,
        Err(OAuthError::Invalid(_))
    ));
    assert!(matches!(
        exchange(
            &http,
            &Provider::Google,
            &app(),
            &code("stale"),
            &verifier,
            deadline
        )
        .await,
        Err(OAuthError::InvalidGrant { .. })
    ));
}
