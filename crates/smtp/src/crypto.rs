//! Signatures, sealing, random tokens and password credentials: the one module for them in
//! this crate. Every primitive comes from `aws-lc-rs`; nothing cryptographic is hand-written,
//! only composed.
//!
//! - **Standard Webhooks** (<https://www.standardwebhooks.com/>), in both directions: the core
//!   signs its control requests with the per-installation secret, and the outbox signs evidence
//!   with each route's secret. A secret is `whsec_` + base64 of 24 to 64 bytes; a signature is
//!   `v1,` + base64 of HMAC-SHA256 over `{webhook-id}.{webhook-timestamp}.{body}`; several may be
//!   sent space-separated during a rotation and one matching is enough; comparisons are
//!   constant-time; a timestamp is accepted within five minutes of now.
//! - **Sealing**: route secrets are stored as AES-256-GCM (`nonce ‖ ciphertext ‖ tag`, the route
//!   id as associated data, so a sealed secret cannot be moved to another route) under a key
//!   derived with HKDF-SHA256 from the installation secret, so a leaked database snapshot cannot
//!   sign evidence. Rotating the installation secret therefore requires the core to register its
//!   routes again.
//! - **Login credentials**: Dovecot's `{SCRAM-SHA-256}` scheme, `iterations,salt,StoredKey,
//!   ServerKey` with standard base64, a 16-byte random salt and 4096 iterations (Dovecot's
//!   minimum and default), as Dovecot's passdb parses it
//!   (<https://doc.dovecot.org/latest/core/config/auth/schemes.html>). PBKDF2-HMAC-SHA256 gives the
//!   salted password; StoredKey is SHA-256 of HMAC(salted, "Client Key") and ServerKey is
//!   HMAC(salted, "Server Key") (RFC 5802, RFC 7677). Dovecot verifies a plaintext login against
//!   it; neither the password nor a reversible form of it is kept anywhere.

use std::num::NonZeroU32;

use aws_lc_rs::rand::SecureRandom as _;
use aws_lc_rs::{aead, constant_time, digest, hkdf, hmac, pbkdf2, rand};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};

/// How far a request's `webhook-timestamp` may be from now, in seconds (Standard Webhooks).
pub const TOLERANCE_SECONDS: i64 = 300;

const NONCE_LEN: usize = 12;
const SCRAM_ITERATIONS: NonZeroU32 = match NonZeroU32::new(4096) {
    Some(n) => n,
    None => NonZeroU32::MIN,
};

/// Why a cryptographic operation failed. Never carries key material.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The secret is not `whsec_` followed by base64 of 24 to 64 bytes.
    #[error("a Standard Webhooks secret is `whsec_` followed by base64 of 24 to 64 bytes")]
    InvalidSecret,
    /// A sealed value was not sealed under this key and associated data, or is corrupted.
    #[error("a sealed value could not be opened (wrong key or corrupted bytes)")]
    Unseal,
    /// The operating system's random source failed.
    #[error("the system random source failed")]
    Random,
    /// HKDF could not derive a key.
    #[error("key derivation failed")]
    Derivation,
}

/// Decodes a Standard Webhooks secret (`whsec_…`) into its key bytes.
///
/// # Errors
///
/// The prefix is missing, the base64 is invalid, or the key is not 24 to 64 bytes.
pub fn decode_secret(secret: &str) -> Result<Vec<u8>, CryptoError> {
    let encoded = secret
        .trim()
        .strip_prefix("whsec_")
        .ok_or(CryptoError::InvalidSecret)?;
    let bytes = STANDARD
        .decode(encoded)
        .or_else(|_| STANDARD_NO_PAD.decode(encoded))
        .map_err(|_| CryptoError::InvalidSecret)?;
    if (24..=64).contains(&bytes.len()) {
        Ok(bytes)
    } else {
        Err(CryptoError::InvalidSecret)
    }
}

/// A Standard Webhooks signing key.
pub struct WebhookKey(hmac::Key);

impl WebhookKey {
    /// The key of decoded secret bytes.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Self {
        Self(hmac::Key::new(hmac::HMAC_SHA256, bytes))
    }

    fn tag(&self, id: &str, timestamp: i64, body: &[u8]) -> hmac::Tag {
        let mut context = hmac::Context::with_key(&self.0);
        context.update(id.as_bytes());
        context.update(b".");
        context.update(timestamp.to_string().as_bytes());
        context.update(b".");
        context.update(body);
        context.sign()
    }

    /// The `webhook-signature` value of a message.
    #[must_use]
    pub fn sign(&self, id: &str, timestamp: i64, body: &[u8]) -> String {
        format!(
            "v1,{}",
            STANDARD.encode(self.tag(id, timestamp, body).as_ref())
        )
    }

    /// True when one of the space-separated `v1,` signatures in `header` matches, compared in
    /// constant time.
    #[must_use]
    pub fn verify(&self, id: &str, timestamp: i64, body: &[u8], header: &str) -> bool {
        let expected = self.tag(id, timestamp, body);
        header.split_ascii_whitespace().any(|candidate| {
            candidate
                .strip_prefix("v1,")
                .and_then(|encoded| STANDARD.decode(encoded).ok())
                .is_some_and(|given| {
                    constant_time::verify_slices_are_equal(expected.as_ref(), &given).is_ok()
                })
        })
    }
}

/// The keys `serve` derives from the installation secret.
pub struct Keys {
    control: WebhookKey,
    seal: aead::LessSafeKey,
}

impl Keys {
    /// Derives the control API's verification key (the secret itself, as the core signs with
    /// it) and the sealing key.
    ///
    /// # Errors
    ///
    /// The secret is not a valid Standard Webhooks secret.
    pub fn from_secret(secret: &str) -> Result<Self, CryptoError> {
        let bytes = decode_secret(secret)?;
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"norbelys-smtp").extract(&bytes);
        let info: [&[u8]; 1] = [b"route secret seal v1"];
        let okm = prk
            .expand(&info, &aead::AES_256_GCM)
            .map_err(|_| CryptoError::Derivation)?;
        Ok(Self {
            control: WebhookKey::new(&bytes),
            seal: aead::LessSafeKey::new(aead::UnboundKey::from(okm)),
        })
    }

    /// The key every control request is verified with.
    #[must_use]
    pub fn control(&self) -> &WebhookKey {
        &self.control
    }

    /// Seals `plaintext` bound to `aad`.
    ///
    /// # Errors
    ///
    /// The random source failed.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut nonce = [0_u8; NONCE_LEN];
        rand::SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| CryptoError::Random)?;
        let mut in_out = plaintext.to_vec();
        self.seal
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad),
                &mut in_out,
            )
            .map_err(|_| CryptoError::Random)?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + in_out.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&in_out);
        Ok(sealed)
    }

    /// Opens a value sealed by [`Keys::seal`] for the same `aad`.
    ///
    /// # Errors
    ///
    /// The key, the associated data or the bytes are wrong.
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (nonce, ciphertext) = sealed
            .split_at_checked(NONCE_LEN)
            .ok_or(CryptoError::Unseal)?;
        let nonce =
            aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| CryptoError::Unseal)?;
        let mut buffer = ciphertext.to_vec();
        let plaintext = self
            .seal
            .open_in_place(nonce, aead::Aad::from(aad), &mut buffer)
            .map_err(|_| CryptoError::Unseal)?;
        Ok(plaintext.to_vec())
    }
}

/// `n` random bytes.
///
/// # Errors
///
/// The system random source failed.
pub fn random_bytes(n: usize) -> Result<Vec<u8>, CryptoError> {
    let mut bytes = vec![0_u8; n];
    rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| CryptoError::Random)?;
    Ok(bytes)
}

/// `n` random bytes as base64url without padding: passwords and ownership tokens.
///
/// # Errors
///
/// The system random source failed.
pub fn random_token(n: usize) -> Result<String, CryptoError> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(n)?))
}

/// The Dovecot credential of `password`: `{SCRAM-SHA-256}4096,<salt>,<StoredKey>,<ServerKey>`.
///
/// # Errors
///
/// The system random source failed.
pub fn scram_sha256(password: &str) -> Result<String, CryptoError> {
    Ok(scram_sha256_salted(password, &random_bytes(16)?))
}

/// The Dovecot credential of `password` under a given `salt`; [`scram_sha256`] draws the salt.
#[must_use]
pub fn scram_sha256_salted(password: &str, salt: &[u8]) -> String {
    let mut salted = [0_u8; digest::SHA256_OUTPUT_LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        SCRAM_ITERATIONS,
        salt,
        password.as_bytes(),
        &mut salted,
    );
    let key = hmac::Key::new(hmac::HMAC_SHA256, &salted);
    let client_key = hmac::sign(&key, b"Client Key");
    let stored_key = digest::digest(&digest::SHA256, client_key.as_ref());
    let server_key = hmac::sign(&key, b"Server Key");
    format!(
        "{{SCRAM-SHA-256}}{},{},{},{}",
        SCRAM_ITERATIONS,
        STANDARD.encode(salt),
        STANDARD.encode(stored_key.as_ref()),
        STANDARD.encode(server_key.as_ref()),
    )
}

/// SHA-256 of `data`, lowercase hex.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

/// SHA-256 of `parts` joined, lowercase hex.
#[must_use]
pub fn sha256_hex_of(parts: &[&[u8]]) -> String {
    let mut context = digest::Context::new(&digest::SHA256);
    for part in parts {
        context.update(part);
    }
    hex(context.finish().as_ref())
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::SECRET;

    /// Signing reproduces the Standard Webhooks reference libraries' published example
    /// (`test_sign_function`): the core verifies evidence with an independent implementation,
    /// so the bytes signed and the encoding must be exactly the specification's.
    #[test]
    fn signs_the_standard_webhooks_example() {
        let key = WebhookKey::new(&decode_secret(SECRET).unwrap());
        let signature = key.sign(
            "msg_p5jXN8AQM9LWM0D4loKWxJek",
            1_614_265_330,
            br#"{"test": 2432232314}"#,
        );
        assert_eq!(signature, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
    }

    /// A header may carry several signatures during a key rotation; one valid `v1` signature is
    /// enough, and anything else (another body, another version, broken base64) is refused.
    #[test]
    fn verifies_any_matching_signature_and_nothing_else() {
        let key = WebhookKey::new(&decode_secret(SECRET).unwrap());
        let good = key.sign("msg_1", 1_700_000_000, b"body");
        let rotated = format!("v1,AAAA {good}");
        assert!(key.verify("msg_1", 1_700_000_000, b"body", &rotated));
        assert!(!key.verify("msg_1", 1_700_000_000, b"other body", &good));
        assert!(!key.verify("msg_1", 1_700_000_001, b"body", &good));
        assert!(!key.verify("msg_1", 1_700_000_000, b"body", &good.replace("v1,", "v2,")));
        assert!(!key.verify("msg_1", 1_700_000_000, b"body", "v1,%%%"));
    }

    /// Secrets follow the Standard Webhooks format: the `whsec_` prefix and 24 to 64 key bytes
    /// in base64, with or without padding; anything else is refused before it can sign.
    #[test]
    fn decodes_only_well_formed_secrets() {
        assert_eq!(decode_secret(SECRET).unwrap().len(), 24);
        let unpadded = format!("whsec_{}", STANDARD_NO_PAD.encode([7_u8; 32]));
        assert_eq!(decode_secret(&unpadded).unwrap(), vec![7_u8; 32]);
        for invalid in [
            "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".to_owned(),
            format!("whsec_{}", STANDARD.encode([1_u8; 23])),
            format!("whsec_{}", STANDARD.encode([1_u8; 65])),
            "whsec_not base64".to_owned(),
        ] {
            assert!(
                decode_secret(&invalid).is_err(),
                "{invalid} must be refused"
            );
        }
    }

    /// A sealed route secret opens only under the same key and the same route id: a snapshot
    /// alone cannot recover it, and a sealed value moved to another route does not open.
    #[test]
    fn seals_bound_to_their_route() {
        let keys = Keys::from_secret(SECRET).unwrap();
        let sealed = keys.seal(b"route secret", b"pwh_a").unwrap();
        assert_eq!(keys.open(&sealed, b"pwh_a").unwrap(), b"route secret");
        assert!(keys.open(&sealed, b"pwh_b").is_err());
        let other = Keys::from_secret(&format!("whsec_{}", STANDARD.encode([9_u8; 32]))).unwrap();
        assert!(other.open(&sealed, b"pwh_a").is_err());
        assert!(keys.open(sealed.get(..20).unwrap(), b"pwh_a").is_err());
    }

    /// The credential written for Dovecot equals an independent computation (OpenSSL's PBKDF2
    /// and HMAC) for RFC 7677's example password and salt, which is also the verifier
    /// PostgreSQL documents for it: Dovecot recomputes exactly these keys at every login.
    #[test]
    fn scram_matches_an_independent_computation() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        assert_eq!(
            scram_sha256_salted("pencil", &salt),
            "{SCRAM-SHA-256}4096,W22ZaJ0SNY7soEsUEjb6gQ==,\
             WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=,\
             wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU="
        );
    }

    /// Fresh credentials carry a new 16-byte salt each time, so two logins with the same
    /// password never share a credential.
    #[test]
    fn scram_draws_a_fresh_salt() {
        let first = scram_sha256("same password").unwrap();
        let second = scram_sha256("same password").unwrap();
        assert_ne!(first, second);
        let salt = first.split(',').nth(1).unwrap();
        assert_eq!(STANDARD.decode(salt).unwrap().len(), 16);
    }

    /// Digests are lowercase hex of SHA-256: event ids and the maps' digest depend on it.
    #[test]
    fn hashes_to_lowercase_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex_of(&[b"a", b"bc"]), sha256_hex(b"abc"));
    }
}
