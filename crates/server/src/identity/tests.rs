//! Store and API tests of identity, through the shared harness: the sign-in flows end to end in
//! process (email codes read back from the transactional mail, passkeys through a software
//! authenticator, OpenID Connect and SSO against a fake provider on the loopback interface),
//! sessions, workspace tokens and their revocation, the dashboard surface's credential rules, and
//! the team: workspaces, members, invitations, API keys and the audit log.
//!
//! This module holds what the tests share: reading a sign-in mail, signing a browser in, minting
//! a workspace token, the software authenticator and the fake provider.

mod bounds;
mod dashboard;
mod impersonation;
mod recovery;
mod sign_in;
mod sso;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _,
};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse as _;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use uuid::Uuid;

use super::oidc::{Provider, Providers};
use crate::crypto;
use crate::domain::ids::{Id, WorkspaceId};
use crate::testing::{DASHBOARD, Reply, TestApp, TestDb, TestSession};

/// The relying party id of the tests' passkeys.
const RP_ID: &str = "norbelys.test";

/// The `name=value` pair of the cookie `name` that `reply` sets, if it sets one.
pub(super) fn set_cookie(reply: &Reply, name: &str) -> Option<String> {
    reply
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|pair| pair.starts_with(&format!("{name}=")))
        .map(str::to_owned)
}

/// The sign-in code and the link token of the last sign-in mail to `email`, read from the
/// transactional message the `system` workspace accepted for it (not the welcome or an
/// invitation the same address may also have been sent).
pub(super) async fn mail_to(test: &TestDb, email: &str) -> (String, String) {
    let body: String = sqlx::query_scalar(
        "SELECT text_body FROM messages WHERE workspace_id = $1 AND to_addresses[1] = $2
            AND subject LIKE '% is your Norbelys sign-in code'
          ORDER BY id DESC LIMIT 1",
    )
    .bind(crate::jobs::SYSTEM_WORKSPACE.uuid())
    .bind(email)
    .fetch_one(test.system.pool())
    .await
    .expect("a sign-in mail");
    let bytes = body.as_bytes();
    let code = bytes
        .windows(6)
        .position(|window| window.iter().all(u8::is_ascii_digit))
        .and_then(|at| body.get(at..at + 6))
        .expect("a 6-digit code")
        .to_owned();
    let token = body
        .split("token=")
        .nth(1)
        .and_then(|rest| rest.split(['&', '\n', ' ']).next())
        .expect("a link token")
        .to_owned();
    (code, token)
}

/// A distinct client address per call, so the per-address rate limits of one test never mix the
/// sign-ins it makes.
pub(super) fn client_address() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let [_, b, c, d] = n.to_be_bytes();
    format!("10.{b}.{c}.{d}")
}

/// Signs `email` in with an email code, from one browser, as the dashboard does: the challenge,
/// the code read from the mail, the finish. The `system` workspace's transactional sender must
/// exist.
pub(super) async fn sign_in(test: &TestDb, app: &TestApp, email: &str) -> TestSession {
    let address = client_address();
    let challenge = app
        .post("/v1/auth/challenges")
        .dashboard()
        .header("x-forwarded-for", &address)
        .json(json!({ "method": "email_code", "email": email }))
        .send()
        .await;
    assert_eq!(challenge.status, StatusCode::CREATED, "{}", challenge.json);
    let ceremony = set_cookie(&challenge, "__Host-nb_ceremony").expect("the ceremony cookie");
    let (code, _) = mail_to(test, email).await;
    let reply = app
        .post("/v1/auth/sessions")
        .dashboard()
        .header("x-forwarded-for", &address)
        .header("cookie", &ceremony)
        .json(json!({ "challenge_id": challenge.json["id"], "code": code }))
        .send()
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    signed_in(&reply)
}

/// The browser a successful sign-in answer describes.
pub(super) fn signed_in(reply: &Reply) -> TestSession {
    TestSession {
        user: reply.json["user"]["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .expect("the user's id"),
        session: reply.json["session"]["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .expect("the session's id"),
        cookie: set_cookie(reply, "__Host-nb_session").expect("the session cookie"),
        csrf: reply.json["csrf_token"]
            .as_str()
            .expect("the CSRF token")
            .to_owned(),
    }
}

/// Mints a workspace token for `session` in `workspace` (a signing key must exist).
pub(super) async fn mint(app: &TestApp, session: &TestSession, workspace: WorkspaceId) -> String {
    let reply = app
        .post("/v1/auth/tokens")
        .browser(session)
        .json(json!({ "workspace_id": workspace.to_string() }))
        .send()
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.json);
    reply.json["token"].as_str().expect("a token").to_owned()
}

/// The value of query parameter `name` in `url`.
pub(super) fn parameter(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .expect("a URL")
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// The parts of CBOR (RFC 8949) a WebAuthn authenticator's answer needs.
enum Cbor<'a> {
    Unsigned(u64),
    Negative(i64),
    Bytes(&'a [u8]),
    Text(&'a str),
    Map(Vec<(Cbor<'a>, Cbor<'a>)>),
}

fn cbor_head(major: u8, value: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    if let Ok(small) = u8::try_from(value)
        && small < 24
    {
        out.push(major | small);
    } else if let Ok(byte) = u8::try_from(value) {
        out.push(major | 24);
        out.push(byte);
    } else if let Ok(short) = u16::try_from(value) {
        out.push(major | 25);
        out.extend_from_slice(&short.to_be_bytes());
    } else {
        out.push(major | 26);
        out.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes());
    }
}

fn cbor(value: &Cbor<'_>, out: &mut Vec<u8>) {
    match value {
        Cbor::Unsigned(value) => cbor_head(0, *value, out),
        Cbor::Negative(value) => cbor_head(1, u64::try_from(-1 - value).unwrap(), out),
        Cbor::Bytes(bytes) => {
            cbor_head(2, u64::try_from(bytes.len()).unwrap(), out);
            out.extend_from_slice(bytes);
        }
        Cbor::Text(text) => {
            cbor_head(3, u64::try_from(text.len()).unwrap(), out);
            out.extend_from_slice(text.as_bytes());
        }
        Cbor::Map(pairs) => {
            cbor_head(5, u64::try_from(pairs.len()).unwrap(), out);
            for (key, value) in pairs {
                cbor(key, out);
                cbor(value, out);
            }
        }
    }
}

/// A software passkey authenticator: a P-256 key and a credential id, answering registration and
/// authentication options as a platform authenticator does (user present and verified, `none`
/// attestation), for the dashboard's origin.
pub(super) struct Authenticator {
    key: EcdsaKeyPair,
    credential_id: Vec<u8>,
    counter: u32,
}

impl Authenticator {
    /// A new authenticator with a fresh key and credential id.
    pub(super) fn new() -> Self {
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
                .unwrap();
        Self {
            key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref()).unwrap(),
            credential_id: crypto::random_bytes(16).unwrap(),
            counter: 0,
        }
    }

    fn client_data(kind: &str, options: &Value) -> String {
        json!({
            "type": kind,
            "challenge": options["publicKey"]["challenge"],
            "origin": DASHBOARD,
            "crossOrigin": false,
        })
        .to_string()
    }

    /// The `RegisterPublicKeyCredential` answering creation `options`.
    pub(super) fn register(&self, options: &Value) -> Value {
        let client_data = Self::client_data("webauthn.create", options);
        let public = self.key.public_key().as_ref();
        let (x, y) = public[1..].split_at(32);
        let mut key = Vec::new();
        cbor(
            &Cbor::Map(vec![
                (Cbor::Unsigned(1), Cbor::Unsigned(2)),
                (Cbor::Unsigned(3), Cbor::Negative(-7)),
                (Cbor::Negative(-1), Cbor::Unsigned(1)),
                (Cbor::Negative(-2), Cbor::Bytes(x)),
                (Cbor::Negative(-3), Cbor::Bytes(y)),
            ]),
            &mut key,
        );
        let mut data = crypto::sha256(RP_ID.as_bytes());
        data.push(0x45); // user present, user verified, attested credential data
        data.extend_from_slice(&0_u32.to_be_bytes());
        data.extend_from_slice(&[0; 16]);
        data.extend_from_slice(
            &u16::try_from(self.credential_id.len())
                .unwrap()
                .to_be_bytes(),
        );
        data.extend_from_slice(&self.credential_id);
        data.extend_from_slice(&key);
        let mut attestation = Vec::new();
        cbor(
            &Cbor::Map(vec![
                (Cbor::Text("fmt"), Cbor::Text("none")),
                (Cbor::Text("attStmt"), Cbor::Map(Vec::new())),
                (Cbor::Text("authData"), Cbor::Bytes(&data)),
            ]),
            &mut attestation,
        );
        let id = URL_SAFE_NO_PAD.encode(&self.credential_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "attestationObject": URL_SAFE_NO_PAD.encode(attestation),
                "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data),
            },
            "extensions": {},
        })
    }

    /// The `PublicKeyCredential` answering request `options` as `user`'s discoverable credential.
    pub(super) fn assert(&mut self, options: &Value, user: Uuid) -> Value {
        self.counter += 1;
        let client_data = Self::client_data("webauthn.get", options);
        let mut data = crypto::sha256(RP_ID.as_bytes());
        data.push(0x05); // user present, user verified
        data.extend_from_slice(&self.counter.to_be_bytes());
        let mut signed = data.clone();
        signed.extend_from_slice(&crypto::sha256(client_data.as_bytes()));
        let signature = self.key.sign(&SystemRandom::new(), &signed).unwrap();
        let id = URL_SAFE_NO_PAD.encode(&self.credential_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "authenticatorData": URL_SAFE_NO_PAD.encode(&data),
                "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data),
                "signature": URL_SAFE_NO_PAD.encode(signature.as_ref()),
                "userHandle": URL_SAFE_NO_PAD.encode(user.as_bytes()),
            },
            "extensions": {},
        })
    }
}

/// A fake OpenID Connect provider on the loopback interface: its discovery document, its key set
/// (one Ed25519 key) and a token endpoint answering every code with the ID token
/// [`FakeIdp::issue`] prepared, signed with its key.
pub(super) struct FakeIdp {
    /// Its issuer, `http://127.0.0.1:<port>`.
    pub(super) issuer: String,
    next: Arc<Mutex<Option<Value>>>,
}

impl FakeIdp {
    /// Starts the provider in the test's runtime.
    pub(super) async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwks = json!({ "keys": [{
            "kty": "OKP", "crv": "Ed25519", "kid": "k1", "use": "sig", "alg": "EdDSA",
            "x": URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
        }]});
        let discovery = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["EdDSA"],
        });
        let next: Arc<Mutex<Option<Value>>> = Arc::default();
        let prepared = Arc::clone(&next);
        let signing = EncodingKey::from_ed_der(pkcs8.as_ref());
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(move || async move { axum::Json(discovery) }),
            )
            .route(
                "/jwks",
                axum::routing::get(move || async move { axum::Json(jwks) }),
            )
            .route(
                "/token",
                axum::routing::post(move || {
                    let claims = prepared.lock().unwrap().take();
                    let signing = signing.clone();
                    async move {
                        let Some(claims) = claims else {
                            return (
                                StatusCode::BAD_REQUEST,
                                axum::Json(json!({ "error": "invalid_grant" })),
                            )
                                .into_response();
                        };
                        let header = Header {
                            kid: Some("k1".to_owned()),
                            ..Header::new(Algorithm::EdDSA)
                        };
                        let token = jsonwebtoken::encode(&header, &claims, &signing).unwrap();
                        axum::Json(json!({
                            "access_token": "at", "token_type": "Bearer", "expires_in": 3600,
                            "id_token": token,
                        }))
                        .into_response()
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { issuer, next }
    }

    /// The provider as a sign-in method offered to everyone, under the client `client`.
    pub(super) fn provider(&self) -> Provider {
        Provider {
            issuer: self.issuer.clone(),
            client_id: "client".to_owned(),
            client_secret: Some(secrecy::SecretString::from("secret")),
        }
    }

    /// Prepares the next code exchange's ID token: `subject`, the address and whether it is
    /// verified, the ceremony's `nonce`, and the authentication time (seconds ago).
    pub(super) fn issue(
        &self,
        subject: &str,
        email: Option<&str>,
        verified: bool,
        nonce: &str,
        authenticated_ago: Option<i64>,
    ) {
        let now = jiff::Timestamp::now().as_second();
        let mut claims = json!({
            "iss": self.issuer, "sub": subject, "aud": "client", "iat": now, "exp": now + 300,
            "nonce": nonce, "email_verified": verified,
        });
        if let Some(email) = email {
            claims["email"] = json!(email);
        }
        if let Some(ago) = authenticated_ago {
            claims["auth_time"] = json!(now - ago);
        }
        *self.next.lock().unwrap() = Some(claims);
    }
}

/// The identity module of a test whose provider `test` is `idp`.
pub(super) fn identity_with(idp: &FakeIdp) -> super::Identity {
    let base = super::Identity::for_tests();
    let redirect = base
        .providers
        .redirect()
        .cloned()
        .expect("the tests' callback URL");
    super::Identity {
        providers: Providers::for_tests(vec![("test".to_owned(), idp.provider())], redirect),
        ..base
    }
}

/// The id of an object in `reply`'s JSON body at `pointer`.
pub(super) fn id_at<R: crate::domain::ids::Resource>(reply: &Reply, pointer: &str) -> Id<R> {
    reply
        .json
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("an id at {pointer} in {}", reply.json))
}
