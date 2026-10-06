//! Workspace tokens: the short-lived bearer the dashboard mints from its session cookie to call
//! the API in one workspace, and the keys that sign them.
//!
//! # The token
//!
//! `nbs_` followed by a compact JWT (RFC 7519, <https://www.rfc-editor.org/rfc/rfc7519>) signed with
//! EdDSA over Ed25519 (RFC 8037). Its claims: `sub` (the user, `usr_…`), `sid` (the session,
//! `ses_…`), `ws` (the workspace, `ws_…`), `role` (the membership role at minting), `scope` (the
//! role's scopes, space-separated), `aud` = `api`, `typ` = `workspace`, `iat`, `exp` (five
//! minutes later) and a random `jti`. The prefix lets secret scanners recognise the token and the
//! authority pick its verifier; the audience and the type keep a token minted for one purpose from
//! being accepted for another (an MCP or CLI token has its own type). A token proves nothing alone:
//! the authority checks its session and the membership on every use, through caches that see a
//! revocation within 60 seconds.
//!
//! # OAuth access tokens
//!
//! The OAuth authorization server's access tokens are signed by the same keys: `nbc_` (the
//! command-line client's tokens for the API, `typ` = `cli`, `aud` = `api`) and `nbo_` (MCP
//! clients' tokens, `typ` = `mcp`, `aud` = the MCP resource's URL), followed by a compact JWT
//! whose claims are `sub` (the user), `gid` (the grant, `grt_…`), `ws` (the workspace),
//! `client_id`, `scope` (the grant's scopes), `aud`, `typ`, `iat`, `exp` (ten minutes later) and
//! `jti`. The API's audience is shared by workspace and CLI tokens and the type keeps them apart;
//! the MCP audience is the resource's URL itself, as the MCP specification requires a resource
//! server to refuse tokens not issued for it. Like a workspace token, an access token proves
//! nothing alone: the authority checks its grant and the membership on every use.
//!
//! # Keys and rotation
//!
//! `signing_keys` holds one row per key: its `kid`, the PKCS#8 private key sealed with the
//! deployment key (bound to the `kid`), the public key as a JWK, and `retired_at`. The newest key
//! without `retired_at` signs; every key whose `retired_at` is absent or still ahead verifies, and
//! is what a key set (JWKS) shows. `norbelys-server admin keys rotate` (the operator's monthly
//! step, and the first one of a new deployment) adds a key and sets the others' `retired_at` 24
//! hours ahead, far beyond a token's five minutes, so no token in flight is lost. The api's login
//! may only read the table: a compromised api process cannot plant a key.
//!
//! Each process keeps the keys in a [`KeyRing`], read on first use and again every five minutes,
//! or sooner when a token names a `kid` the ring does not know (another replica signed with a key
//! rotated in since), at most once every ten seconds so forged `kid`s cannot hammer the database.
//! Without any signing key, minting answers `503` and logs that the operator must rotate one in.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::crypto::{self, CryptoError, Keys};
use crate::db::{Database, Tx};
use crate::domain::ids::{self, Id, Session, User, Workspace};
use crate::domain::oauth::{ACCESS_LIFETIME, Audience, Resources};
use crate::domain::scope::{MembershipRole, ScopeSet};
use crate::domain::time::Timestamp;

/// The prefix of a workspace token.
pub const PREFIX: &str = "nbs_";
/// How long a workspace token lives.
pub const LIFETIME: Duration = Duration::from_secs(5 * 60);
/// The audience of tokens for the API.
const AUDIENCE: &str = "api";
/// The type of a workspace token.
const TYPE: &str = "workspace";
/// How often the ring is read again whatever happens.
const RELOAD_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often an unknown `kid` may make the ring read again.
const RELOAD_AT_MOST: Duration = Duration::from_secs(10);
/// How long a rotated-out key keeps verifying.
const RETIRE_AFTER: Duration = Duration::from_secs(24 * 3600);

/// Why a token could not be minted or verified.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The token is malformed, forged, expired, or for another audience or type.
    #[error("the token is not valid")]
    Invalid,
    /// No signing key exists yet: an operator must run `admin keys rotate`.
    #[error("no signing key exists; run `norbelys-server admin keys rotate`")]
    NoKey,
    /// The database failed.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// A key could not be sealed, opened or generated.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// A stored key is not a valid Ed25519 key.
    #[error("a stored signing key is not a valid Ed25519 key")]
    Key,
}

/// The claims of a workspace token.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    sub: Id<User>,
    sid: Id<Session>,
    ws: Id<Workspace>,
    role: String,
    scope: String,
    aud: String,
    typ: String,
    iat: i64,
    exp: i64,
    jti: String,
}

/// The claims of an OAuth access token (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccessClaims {
    sub: Id<User>,
    gid: Id<ids::Grant>,
    ws: Id<Workspace>,
    client_id: String,
    scope: String,
    aud: String,
    typ: String,
    iat: i64,
    exp: i64,
    jti: String,
}

/// What an OAuth access token is minted for: a grant's user, workspace, client and scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessGrant {
    /// The grant.
    pub grant: Id<ids::Grant>,
    /// The user who consented.
    pub user: Id<User>,
    /// The workspace the grant acts in.
    pub workspace: Id<Workspace>,
    /// The client the grant was given to.
    pub client_id: String,
    /// The grant's scopes.
    pub scopes: ScopeSet,
}

/// What a verified OAuth access token says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedAccess {
    /// The grant it was issued from.
    pub grant: Id<ids::Grant>,
    /// The user.
    pub user: Id<User>,
    /// The workspace it acts in.
    pub workspace: Id<Workspace>,
    /// The scopes it carries; the authority narrows them to the grant and the current role.
    pub scopes: ScopeSet,
}

/// The `aud` claim of an access token for `audience`: the API's own audience for the CLI, the MCP
/// resource's URL for MCP clients.
fn audience_claim(audience: Audience, resources: &Resources) -> &str {
    match audience {
        Audience::Cli => AUDIENCE,
        Audience::Mcp => &resources.mcp,
    }
}

/// What a verified workspace token says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// The user.
    pub user: Id<User>,
    /// The session it was minted from.
    pub session: Id<Session>,
    /// The workspace it acts in.
    pub workspace: Id<Workspace>,
    /// The scopes it was minted with; the authority narrows them to the current role.
    pub scopes: ScopeSet,
    /// When it expires.
    pub expires_at: Timestamp,
}

/// What a workspace token is minted for: a user's session in a workspace, with the role's scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// The user.
    pub user: Id<User>,
    /// The session the token is minted from.
    pub session: Id<Session>,
    /// The workspace it acts in.
    pub workspace: Id<Workspace>,
    /// The user's role there.
    pub role: MembershipRole,
    /// The scopes it carries.
    pub scopes: ScopeSet,
}

/// A minted token.
#[derive(Debug, Clone)]
pub struct Minted {
    /// `nbs_…`, for the `Authorization: Bearer` header.
    pub token: String,
    /// When it expires.
    pub expires_at: Timestamp,
}

struct LoadedKey {
    kid: String,
    signing: Option<EncodingKey>,
    verifying: DecodingKey,
    public_jwk: serde_json::Value,
}

#[derive(Default)]
struct Ring {
    keys: Vec<LoadedKey>,
    /// When the keys were last read.
    loaded: Option<Instant>,
    /// When an unknown `kid` last made the ring read them.
    forced: Option<Instant>,
}

/// The signing keys of one process (see the module).
#[derive(Clone, Default)]
pub struct KeyRing {
    ring: Arc<RwLock<Ring>>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("KeyRing(..)")
    }
}

/// The associated data a signing key's private half is sealed with: its table and `kid`.
/// `admin secrets rotate` re-seals with it.
pub(crate) fn context(kid: &str) -> String {
    format!("signing_keys.private_key:{kid}")
}

impl KeyRing {
    /// An empty ring; keys are read on first use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the usable keys: when they were never read or were read more than five minutes ago,
    /// or, with `force` (a token named an unknown `kid`), unless an unknown `kid` already forced a
    /// read in the last ten seconds.
    async fn reload(&self, db: &Database, keys: &Keys, force: bool) -> Result<(), TokenError> {
        let fresh = |ring: &Ring| {
            if force {
                ring.forced
                    .is_some_and(|forced| forced.elapsed() < RELOAD_AT_MOST)
            } else {
                ring.loaded
                    .is_some_and(|loaded| loaded.elapsed() < RELOAD_EVERY)
            }
        };
        if fresh(&*self.ring.read().await) {
            return Ok(());
        }
        let mut ring = self.ring.write().await;
        if fresh(&ring) {
            return Ok(());
        }
        let rows = sqlx::query!(
            r#"SELECT kid, private_key, public_jwk, retired_at IS NULL AS "signs!"
                 FROM signing_keys
                WHERE algorithm = 'EdDSA' AND (retired_at IS NULL OR retired_at > now())
                ORDER BY created_at DESC, kid"#
        )
        .fetch_all(db.pool())
        .await?;
        let mut loaded = Vec::with_capacity(rows.len());
        for row in rows {
            let pkcs8 = keys.open(&row.private_key, context(&row.kid).as_bytes())?;
            let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| TokenError::Key)?;
            loaded.push(LoadedKey {
                verifying: DecodingKey::from_ed_der(pair.public_key().as_ref()),
                signing: row.signs.then(|| EncodingKey::from_ed_der(&pkcs8)),
                kid: row.kid,
                public_jwk: row.public_jwk,
            });
        }
        ring.keys = loaded;
        ring.loaded = Some(Instant::now());
        if force {
            ring.forced = ring.loaded;
        }
        Ok(())
    }

    /// Mints a token for `grant`.
    ///
    /// # Errors
    ///
    /// No signing key exists, or the keys cannot be read.
    pub async fn mint(
        &self,
        db: &Database,
        keys: &Keys,
        grant: &Grant,
    ) -> Result<Minted, TokenError> {
        let Grant {
            user,
            session,
            workspace,
            role,
            scopes,
        } = *grant;
        let now = crate::process::now();
        let expires_at = now.plus(LIFETIME);
        let claims = Claims {
            sub: user,
            sid: session,
            ws: workspace,
            role: role.as_str().to_owned(),
            scope: scopes.to_strings().join(" "),
            aud: AUDIENCE.to_owned(),
            typ: TYPE.to_owned(),
            iat: now.0.as_second(),
            exp: expires_at.0.as_second(),
            jti: crypto::random_token(16)?,
        };
        let jwt = self.sign(db, keys, &claims).await?;
        Ok(Minted {
            token: format!("{PREFIX}{jwt}"),
            expires_at,
        })
    }

    /// Verifies a workspace token: its prefix, signature, audience, type and expiry.
    ///
    /// # Errors
    ///
    /// [`TokenError::Invalid`] for anything that is not a valid token of ours; a database or key
    /// failure otherwise.
    pub async fn verify(
        &self,
        db: &Database,
        keys: &Keys,
        token: &str,
    ) -> Result<Verified, TokenError> {
        let jwt = token.strip_prefix(PREFIX).ok_or(TokenError::Invalid)?;
        let claims: Claims = self.decode(db, keys, jwt, AUDIENCE).await?;
        if claims.typ != TYPE {
            return Err(TokenError::Invalid);
        }
        let expires_at = jiff::Timestamp::from_second(claims.exp)
            .map(Timestamp)
            .map_err(|_| TokenError::Invalid)?;
        Ok(Verified {
            user: claims.sub,
            session: claims.sid,
            workspace: claims.ws,
            scopes: ScopeSet::parse(claims.scope.split_whitespace())
                .map_err(|_| TokenError::Invalid)?,
            expires_at,
        })
    }

    /// Mints an OAuth access token for `grant`, of `audience`'s type and prefix (see the module).
    ///
    /// # Errors
    ///
    /// No signing key exists, or the keys cannot be read.
    pub async fn mint_access(
        &self,
        db: &Database,
        keys: &Keys,
        audience: Audience,
        resources: &Resources,
        grant: &AccessGrant,
    ) -> Result<Minted, TokenError> {
        let now = crate::process::now();
        let expires_at = now.plus(ACCESS_LIFETIME);
        let claims = AccessClaims {
            sub: grant.user,
            gid: grant.grant,
            ws: grant.workspace,
            client_id: grant.client_id.clone(),
            scope: grant.scopes.to_strings().join(" "),
            aud: audience_claim(audience, resources).to_owned(),
            typ: audience.typ().to_owned(),
            iat: now.0.as_second(),
            exp: expires_at.0.as_second(),
            jti: crypto::random_token(16)?,
        };
        let jwt = self.sign(db, keys, &claims).await?;
        Ok(Minted {
            token: format!("{}{jwt}", audience.prefix()),
            expires_at,
        })
    }

    /// Verifies an OAuth access token of `audience`: its prefix, signature, `aud`, `typ` and
    /// expiry. A token of the other audience is refused, whatever its signature.
    ///
    /// # Errors
    ///
    /// [`TokenError::Invalid`] for anything that is not a valid token of this audience; a database
    /// or key failure otherwise.
    pub async fn verify_access(
        &self,
        db: &Database,
        keys: &Keys,
        token: &str,
        audience: Audience,
        resources: &Resources,
    ) -> Result<VerifiedAccess, TokenError> {
        let jwt = token
            .strip_prefix(audience.prefix())
            .ok_or(TokenError::Invalid)?;
        let claims: AccessClaims = self
            .decode(db, keys, jwt, audience_claim(audience, resources))
            .await?;
        if claims.typ != audience.typ() {
            return Err(TokenError::Invalid);
        }
        Ok(VerifiedAccess {
            grant: claims.gid,
            user: claims.sub,
            workspace: claims.ws,
            scopes: ScopeSet::parse(claims.scope.split_whitespace())
                .map_err(|_| TokenError::Invalid)?,
        })
    }

    /// Signs `claims` as a compact JWT with the newest signing key.
    async fn sign(
        &self,
        db: &Database,
        keys: &Keys,
        claims: &impl Serialize,
    ) -> Result<String, TokenError> {
        self.reload(db, keys, false).await?;
        let ring = self.ring.read().await;
        let (kid, signing) = ring
            .keys
            .iter()
            .find_map(|key| {
                key.signing
                    .as_ref()
                    .map(|signing| (key.kid.clone(), signing))
            })
            .ok_or(TokenError::NoKey)?;
        let header = Header {
            kid: Some(kid),
            ..Header::new(Algorithm::EdDSA)
        };
        jsonwebtoken::encode(&header, claims, signing).map_err(|_| TokenError::Key)
    }

    /// Checks a compact JWT's algorithm, its signature by the key its `kid` names, its `aud` and
    /// its expiry, and decodes its claims; the type is the caller's to check.
    async fn decode<C: DeserializeOwned>(
        &self,
        db: &Database,
        keys: &Keys,
        jwt: &str,
        audience: &str,
    ) -> Result<C, TokenError> {
        let header = jsonwebtoken::decode_header(jwt).map_err(|_| TokenError::Invalid)?;
        if header.alg != Algorithm::EdDSA {
            return Err(TokenError::Invalid);
        }
        let kid = header.kid.ok_or(TokenError::Invalid)?;
        self.reload(db, keys, false).await?;
        if !self.knows(&kid).await {
            self.reload(db, keys, true).await?;
        }
        let ring = self.ring.read().await;
        let key = ring
            .keys
            .iter()
            .find(|key| key.kid == kid)
            .ok_or(TokenError::Invalid)?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iat", "aud", "sub"]);
        validation.leeway = crate::domain::identity::CLOCK_SKEW.as_secs();
        jsonwebtoken::decode::<C>(jwt, &key.verifying, &validation)
            .map(|data| data.claims)
            .map_err(|_| TokenError::Invalid)
    }

    async fn knows(&self, kid: &str) -> bool {
        self.ring.read().await.keys.iter().any(|key| key.kid == kid)
    }

    /// The public keys that verify tokens now, as a JSON Web Key Set (RFC 7517).
    ///
    /// # Errors
    ///
    /// The keys cannot be read.
    pub async fn jwks(&self, db: &Database, keys: &Keys) -> Result<serde_json::Value, TokenError> {
        self.reload(db, keys, false).await?;
        let ring = self.ring.read().await;
        Ok(serde_json::json!({
            "keys": ring.keys.iter().map(|key| key.public_jwk.clone()).collect::<Vec<_>>()
        }))
    }
}

/// `GET /.well-known/jwks.json`: the public keys that verify tokens now, as a JSON Web Key Set
/// (RFC 7517), for whoever verifies our tokens outside this process. It holds every key that is not
/// retired, so a token signed just before a rotation keeps verifying.
///
/// # Errors
///
/// The keys cannot be read (`503` when the database is unavailable).
pub async fn key_set(
    axum::extract::State(app): axum::extract::State<crate::http::AppState>,
) -> Result<axum::Json<serde_json::Value>, crate::problem::Problem> {
    app.identity
        .tokens
        .jwks(&app.db, &app.keys)
        .await
        .map(axum::Json)
        .map_err(|error| match error {
            TokenError::Db(error) => error.into(),
            other => crate::problem::Problem::internal(&other),
        })
}

/// Adds a new signing key and retires the current ones 24 hours from now (see the module).
/// Runs as the operator (`norbelys_system`), the only login that may write the table; returns the
/// new key's `kid`.
///
/// # Errors
///
/// The random source failed or the database refused the rows.
pub async fn rotate(tx: &mut Tx, keys: &Keys) -> Result<String, TokenError> {
    lock_rotation(tx).await?;
    let pkcs8 =
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| TokenError::Key)?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| TokenError::Key)?;
    let kid = crypto::random_token(12)?;
    let public_jwk = serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "x": URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
        "kid": kid,
        "alg": "EdDSA",
        "use": "sig",
    });
    let sealed = keys.seal(pkcs8.as_ref(), context(&kid).as_bytes())?;
    sqlx::query!(
        "UPDATE signing_keys SET retired_at = now() + make_interval(secs => $1)
          WHERE retired_at IS NULL",
        RETIRE_AFTER.as_secs_f64()
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO signing_keys (kid, algorithm, private_key, public_jwk) VALUES ($1, 'EdDSA', $2, $3)",
        kid,
        sealed,
        public_jwk
    )
    .execute(&mut **tx)
    .await?;
    Ok(kid)
}

/// Initializes an empty installation's signing key without rotating an existing current key.
/// Retired-only installations require the operator's explicit rotation instead.
///
/// # Errors
///
/// The database or random source failed, the sealing key is unavailable, or every key is retired.
pub async fn ensure(tx: &mut Tx, keys: &Keys) -> Result<String, TokenError> {
    lock_rotation(tx).await?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT kid FROM signing_keys WHERE retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(kid) = current {
        return Ok(kid);
    }
    let existing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM signing_keys)")
        .fetch_one(&mut **tx)
        .await?;
    if existing {
        return Err(TokenError::NoKey);
    }
    rotate(tx, keys).await
}

/// Serializes first initialization with deliberate rotation in the same installation.
async fn lock_rotation(tx: &mut Tx) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('norbelys:signing-keys', 0))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestDb, keys};

    fn ids() -> (Id<User>, Id<Session>, Id<Workspace>) {
        (Id::new(), Id::new(), Id::new())
    }

    /// A minted token verifies to exactly what it was minted for, and anything else does not: a
    /// token without our prefix, a tampered one, one signed by a key the deployment never had, and
    /// a token of another type or audience, so a token can only ever be what it claims.
    #[tokio::test]
    async fn tokens_verify_only_as_minted() {
        let test = TestDb::new().await;
        test.signing_key().await;
        let ring = KeyRing::new();
        let (user, session, workspace) = ids();
        let scopes = MembershipRole::Viewer.scopes();
        let grant = Grant {
            user,
            session,
            workspace,
            role: MembershipRole::Viewer,
            scopes,
        };
        let minted = ring.mint(&test.app, &keys(), &grant).await.unwrap();
        assert!(minted.token.starts_with(PREFIX));
        let verified = ring
            .verify(&test.app, &keys(), &minted.token)
            .await
            .unwrap();
        assert_eq!(
            (
                verified.user,
                verified.session,
                verified.workspace,
                verified.scopes
            ),
            (user, session, workspace, scopes)
        );

        let unprefixed = minted.token.trim_start_matches(PREFIX);
        // One character in the middle of the signature changes: every one of its 6 bits counts.
        let mut tampered = minted.token.clone().into_bytes();
        let at = tampered.len() - 10;
        tampered[at] = if tampered[at] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        for token in [unprefixed.to_owned(), tampered] {
            assert!(matches!(
                ring.verify(&test.app, &keys(), &token).await,
                Err(TokenError::Invalid)
            ));
        }

        let foreign = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let header = Header {
            kid: Some("unknown".to_owned()),
            ..Header::new(Algorithm::EdDSA)
        };
        let now = crate::process::now().0.as_second();
        let claims = |typ: &str, aud: &str| Claims {
            sub: user,
            sid: session,
            ws: workspace,
            role: "viewer".to_owned(),
            scope: String::new(),
            aud: aud.to_owned(),
            typ: typ.to_owned(),
            iat: now,
            exp: now + 300,
            jti: "j".to_owned(),
        };
        let forged = jsonwebtoken::encode(
            &header,
            &claims("workspace", "api"),
            &EncodingKey::from_ed_der(foreign.as_ref()),
        )
        .unwrap();
        assert!(matches!(
            ring.verify(&test.app, &keys(), &format!("{PREFIX}{forged}"))
                .await,
            Err(TokenError::Invalid)
        ));

        let (kid, signing) = {
            let guard = ring.ring.read().await;
            (
                guard.keys[0].kid.clone(),
                guard.keys[0].signing.clone().unwrap(),
            )
        };
        for (typ, aud) in [("cli", "api"), ("workspace", "mcp")] {
            let header = Header {
                kid: Some(kid.clone()),
                ..Header::new(Algorithm::EdDSA)
            };
            let token = format!(
                "{PREFIX}{}",
                jsonwebtoken::encode(&header, &claims(typ, aud), &signing).unwrap()
            );
            assert!(
                matches!(
                    ring.verify(&test.app, &keys(), &token).await,
                    Err(TokenError::Invalid)
                ),
                "{typ} {aud}"
            );
        }
    }

    /// An OAuth access token verifies only as the audience it was minted for: the same JWT under
    /// the other prefix, or verified for another deployment's MCP resource, is refused, so an MCP
    /// client's token never calls the API and the CLI's never reaches the MCP server.
    #[tokio::test]
    async fn access_tokens_verify_only_for_their_audience() {
        use strum::IntoEnumIterator as _;

        let test = TestDb::new().await;
        test.signing_key().await;
        let ring = KeyRing::new();
        let resources = Resources::of(&url::Url::parse("https://api.norbelys.test").unwrap());
        let elsewhere = Resources::of(&url::Url::parse("https://api.elsewhere.test").unwrap());
        let grant = AccessGrant {
            grant: Id::new(),
            user: Id::new(),
            workspace: Id::new(),
            client_id: "norbelys-cli".to_owned(),
            scopes: MembershipRole::Viewer.scopes(),
        };
        for audience in Audience::iter() {
            let minted = ring
                .mint_access(&test.app, &keys(), audience, &resources, &grant)
                .await
                .unwrap();
            let verified = ring
                .verify_access(&test.app, &keys(), &minted.token, audience, &resources)
                .await
                .unwrap();
            assert_eq!(
                (
                    verified.grant,
                    verified.user,
                    verified.workspace,
                    verified.scopes
                ),
                (grant.grant, grant.user, grant.workspace, grant.scopes),
                "{audience:?}"
            );
            let jwt = minted.token.trim_start_matches(audience.prefix());
            for other in Audience::iter().filter(|other| *other != audience) {
                for token in [minted.token.clone(), format!("{}{jwt}", other.prefix())] {
                    assert!(
                        matches!(
                            ring.verify_access(&test.app, &keys(), &token, other, &resources)
                                .await,
                            Err(TokenError::Invalid)
                        ),
                        "{audience:?} as {other:?}"
                    );
                }
            }
            assert!(matches!(
                ring.verify(&test.app, &keys(), &format!("{PREFIX}{jwt}"))
                    .await,
                Err(TokenError::Invalid)
            ));
            if audience == Audience::Mcp {
                assert!(matches!(
                    ring.verify_access(&test.app, &keys(), &minted.token, audience, &elsewhere)
                        .await,
                    Err(TokenError::Invalid)
                ));
            }
        }
    }

    /// Rotation keeps every token in flight valid: the old key stops signing at once but verifies
    /// until its 24 hours are over, and the ring learns the new key from a token that names it.
    #[tokio::test]
    async fn rotation_keeps_tokens_in_flight() {
        let test = TestDb::new().await;
        test.signing_key().await;
        let (old_ring, new_ring) = (KeyRing::new(), KeyRing::new());
        let (user, session, workspace) = ids();
        let mint = |ring: KeyRing| {
            let db = test.app.clone();
            async move {
                let grant = Grant {
                    user,
                    session,
                    workspace,
                    role: MembershipRole::Owner,
                    scopes: ScopeSet::all(),
                };
                ring.mint(&db, &keys(), &grant).await.unwrap()
            }
        };
        let before = mint(old_ring.clone()).await;
        let new_kid = test.signing_key().await;
        let after = mint(new_ring.clone()).await;
        let header = jsonwebtoken::decode_header(after.token.trim_start_matches(PREFIX)).unwrap();
        assert_eq!(header.kid.as_deref(), Some(new_kid.as_str()));
        for ring in [&old_ring, &new_ring] {
            for token in [&before.token, &after.token] {
                assert!(ring.verify(&test.app, &keys(), token).await.is_ok());
            }
        }
        let jwks = new_ring.jwks(&test.app, &keys()).await.unwrap();
        assert_eq!(jwks["keys"].as_array().unwrap().len(), 2);
    }

    /// Without a signing key, minting fails with the error that tells the operator what to do,
    /// instead of minting something unverifiable.
    #[tokio::test]
    async fn minting_needs_a_key() {
        let test = TestDb::new().await;
        let (user, session, workspace) = ids();
        let grant = Grant {
            user,
            session,
            workspace,
            role: MembershipRole::Owner,
            scopes: ScopeSet::all(),
        };
        let minted = KeyRing::new().mint(&test.app, &keys(), &grant).await;
        assert!(matches!(minted, Err(TokenError::NoKey)));
    }
}
