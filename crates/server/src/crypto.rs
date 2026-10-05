//! Secrets, sealing and hashing: the one module for them, so every key, nonce and MAC in
//! the product is produced and checked the same way.
//!
//! # Subkeys, and what each role holds
//!
//! A deployment has one 32-byte deployment key. Every purpose gets its own subkey derived from it
//! with HKDF-SHA256 ([`Subkey`]), so a key used to sign cursors can never open a sealed
//! credential. A role holds only the subkeys its own work uses: `admin deployment-key --for
//! <role>` encodes them as a role key ([`role_key`]), which the role reads in place of the
//! deployment key, and a role given the deployment key itself keeps only its subkeys of it
//! ([`Keys::only`]). So the tracking role, on the public host, never holds the sealing key, and no
//! background role holds the identity secrets (the keys of session tokens, sign-in codes and CSRF
//! tokens). A role checks at start that its key holds every subkey it needs ([`Keys::require`]),
//! so a role key made for another role stops it there, naming the missing subkey.
//!
//! Should a call still reach a subkey its role does not hold (a role whose list of subkeys missed
//! a use), it fails closed rather than quietly: sealing and opening answer
//! [`CryptoError::MissingSubkey`]; a verification is refused; and a MAC or hash, which has no
//! error to return, is made under a stand-in key drawn at random when the keys are loaded, with an
//! error event naming the subkey. What the stand-in made verifies nowhere else and matches no
//! stored row, so a missing subkey can break a feature but never open one.
//!
//! # Key ids and rotation
//!
//! A deployment key has a key id: four bytes derived from the key itself, so an id can never be
//! attached to the wrong key. What a key makes names it: a sealed value starts with its key's id,
//! and a signed tag is the key id followed by the HMAC truncated to 28 bytes, 32 bytes in all, the
//! length of a plain HMAC-SHA256, so no format that carries a tag changes. During a rotation a
//! process also holds the previous deployment key (`NORBELYS_PREVIOUS_DEPLOYMENT_KEY`, or a role
//! key made while both were configured): it seals and signs with the new key, and opens or
//! verifies with whichever key a value names. `admin secrets rotate` re-seals the stored rows
//! under the new key in batches; the previous key may be dropped once it reports no row left and
//! the links signed with the old key (tracking and unsubscribe links in mail already sent) no
//! longer need to work.
//!
//! Two kinds of values cannot name their key. A Message-ID tag is 8 bytes, too short to spare
//! four for an id, so it verifies under every key held. A keyed hash of a bearer secret (session
//! and refresh tokens, sign-in codes and links, invitations, registered clients' secrets) is
//! looked up by equality, so a row is found only under the key that hashed it: a rotation ends
//! browser sessions, OAuth refresh chains and pending codes and invitations, and their holders
//! sign in or authorise again. API keys are hashed with plain SHA-256 and survive it.
//!
//! # Sealing
//!
//! Sealed values are AES-256-GCM with a random 96-bit nonce, stored as
//! `key id || nonce || ciphertext || tag`, and bound to their context (the table, the workspace,
//! the row) through the associated data. Everything uses `aws-lc-rs`; nothing is hand-written.

use std::collections::BTreeMap;
use std::sync::Arc;

use aws_lc_rs::rand::SecureRandom as _;
use aws_lc_rs::{aead, constant_time, digest, hkdf, hmac, rand};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator as _;

const NONCE_LEN: usize = 12;
/// The bytes of a key id.
pub const KEY_ID_LEN: usize = 4;
/// The bytes of a signed tag: the key id, then the truncated MAC.
const TAG_LEN: usize = 32;
/// The bytes of the MAC inside a signed tag.
const MAC_LEN: usize = TAG_LEN - KEY_ID_LEN;
/// The bytes of a Message-ID tag ([`Keys::message_id_tag`]).
pub const MESSAGE_ID_TAG_LEN: usize = 8;
/// The bytes of every subkey.
const SUBKEY_LEN: usize = 32;
/// What a role key starts with, so a leaked one is recognisable to secret scanners.
const ROLE_KEY_PREFIX: &str = "nbrk_";
/// The version of a role key's encoding.
const ROLE_KEY_VERSION: u8 = 1;
/// The HKDF label of the key id.
const KEY_ID_LABEL: &[u8] = b"norbelys key id v1";

/// Why a cryptographic operation failed. Never carries key material.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("the deployment key must be base64 of exactly 32 bytes")]
    InvalidDeploymentKey,
    #[error("the role key is not one `admin deployment-key --for <role>` printed")]
    InvalidRoleKey,
    #[error(
        "the key holds no `{0}` subkey: give the role its own key (`admin deployment-key --for <role>`)"
    )]
    MissingSubkey(&'static str),
    #[error("a sealed value names a key this process does not hold")]
    UnknownKey,
    #[error("a sealed value could not be opened (wrong key, context or corrupted bytes)")]
    Unseal,
    #[error("the system random source failed")]
    Random,
    #[error("key derivation failed")]
    Derivation,
}

/// The purposes a subkey is derived for. Adding one never changes the others.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum Subkey {
    /// Seals stored secrets and sealed requests (AES-256-GCM).
    Seal,
    /// Signs pagination cursors.
    Cursor,
    /// Hashes bearer secrets for their lookup: session and refresh tokens, sign-in links,
    /// invitations, registered clients' secrets.
    Token,
    /// Hashes one-time sign-in codes.
    Code,
    /// Signs open and click tracking tokens.
    Tracking,
    /// Signs internal links: unsubscribes and downloads.
    Link,
    /// Tags our Message-IDs.
    MessageId,
    /// Signs the CSRF tokens of browser sessions.
    Csrf,
    /// Hashes client addresses.
    Address,
}

impl Subkey {
    /// The subkey's name, in a role key and in errors.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The HKDF label it is derived under; never changed once keys exist.
    fn label(self) -> &'static [u8] {
        match self {
            Self::Seal => b"norbelys seal v1",
            Self::Cursor => b"norbelys cursor v1",
            Self::Token => b"norbelys token v1",
            Self::Code => b"norbelys code v1",
            Self::Tracking => b"norbelys tracking v1",
            Self::Link => b"norbelys webhook v1",
            Self::MessageId => b"norbelys message-id v1",
            Self::Csrf => b"norbelys csrf v1",
            Self::Address => b"norbelys address v1",
        }
    }
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// A deployment key's id and every subkey, derived.
struct Root {
    id: [u8; KEY_ID_LEN],
    subkeys: Vec<(Subkey, [u8; SUBKEY_LEN])>,
}

impl Root {
    /// Derives the id and the subkeys of `encoded`, base64 (standard or URL-safe) of 32 bytes.
    fn derive(encoded: &SecretString) -> Result<Self, CryptoError> {
        let text = encoded.expose_secret().trim();
        let bytes = STANDARD
            .decode(text)
            .or_else(|_| URL_SAFE_NO_PAD.decode(text))
            .map_err(|_| CryptoError::InvalidDeploymentKey)?;
        if bytes.len() != 32 {
            return Err(CryptoError::InvalidDeploymentKey);
        }
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"norbelys deployment key").extract(&bytes);
        let expand = |label: &[u8], out: &mut [u8]| -> Result<(), CryptoError> {
            prk.expand(&[label], Len(out.len()))
                .and_then(|okm| okm.fill(out))
                .map_err(|_| CryptoError::Derivation)
        };
        let mut id = [0_u8; KEY_ID_LEN];
        expand(KEY_ID_LABEL, &mut id)?;
        let subkeys = Subkey::iter()
            .map(|subkey| {
                let mut out = [0_u8; SUBKEY_LEN];
                expand(subkey.label(), &mut out)?;
                Ok((subkey, out))
            })
            .collect::<Result<Vec<_>, CryptoError>>()?;
        Ok(Self { id, subkeys })
    }
}

/// The subkeys of one deployment key that this process holds.
#[derive(Clone)]
struct Ring {
    id: [u8; KEY_ID_LEN],
    seal: Option<Arc<aead::LessSafeKey>>,
    macs: BTreeMap<Subkey, hmac::Key>,
}

impl Ring {
    /// The ring of key `id` holding `subkeys`.
    fn new(
        id: [u8; KEY_ID_LEN],
        subkeys: &[(Subkey, [u8; SUBKEY_LEN])],
    ) -> Result<Self, CryptoError> {
        let mut seal = None;
        let mut macs = BTreeMap::new();
        for (subkey, bytes) in subkeys {
            if *subkey == Subkey::Seal {
                let key = aead::UnboundKey::new(&aead::AES_256_GCM, bytes)
                    .map_err(|_| CryptoError::Derivation)?;
                seal = Some(Arc::new(aead::LessSafeKey::new(key)));
            } else {
                macs.insert(*subkey, hmac::Key::new(hmac::HMAC_SHA256, bytes));
            }
        }
        Ok(Self { id, seal, macs })
    }

    fn holds(&self, subkey: Subkey) -> bool {
        if subkey == Subkey::Seal {
            self.seal.is_some()
        } else {
            self.macs.contains_key(&subkey)
        }
    }

    fn mac(&self, subkey: Subkey) -> Option<&hmac::Key> {
        self.macs.get(&subkey)
    }

    /// This ring without the subkeys outside `subkeys`.
    fn only(self, subkeys: &[Subkey]) -> Self {
        Self {
            id: self.id,
            seal: self.seal.filter(|_| subkeys.contains(&Subkey::Seal)),
            macs: self
                .macs
                .into_iter()
                .filter(|(subkey, _)| subkeys.contains(subkey))
                .collect(),
        }
    }
}

/// A role key as it is encoded (JSON, then base64url, after [`ROLE_KEY_PREFIX`]).
#[derive(Serialize, Deserialize)]
struct EncodedRoleKey {
    /// [`ROLE_KEY_VERSION`].
    v: u8,
    /// The current key's subkeys first, then the previous key's during a rotation.
    rings: Vec<EncodedRing>,
}

/// One deployment key's subkeys in a role key.
#[derive(Serialize, Deserialize)]
struct EncodedRing {
    /// The key id, hex.
    kid: String,
    /// Each subkey by name ([`Subkey::as_str`]), base64url.
    subkeys: BTreeMap<String, String>,
}

impl EncodedRing {
    fn of(root: &Root, subkeys: &[Subkey]) -> Self {
        Self {
            kid: hex(&root.id),
            subkeys: root
                .subkeys
                .iter()
                .filter(|(subkey, _)| subkeys.contains(subkey))
                .map(|(subkey, bytes)| (subkey.as_str().to_owned(), URL_SAFE_NO_PAD.encode(bytes)))
                .collect(),
        }
    }

    fn ring(&self) -> Result<Ring, CryptoError> {
        let id = parse_key_id(&self.kid).ok_or(CryptoError::InvalidRoleKey)?;
        let subkeys = self
            .subkeys
            .iter()
            .map(|(name, value)| {
                let subkey = name
                    .parse::<Subkey>()
                    .map_err(|_| CryptoError::InvalidRoleKey)?;
                let bytes = URL_SAFE_NO_PAD
                    .decode(value)
                    .ok()
                    .and_then(|bytes| <[u8; SUBKEY_LEN]>::try_from(bytes).ok())
                    .ok_or(CryptoError::InvalidRoleKey)?;
                Ok((subkey, bytes))
            })
            .collect::<Result<Vec<_>, CryptoError>>()?;
        Ring::new(id, &subkeys)
    }
}

/// A key id written as hex, read back.
fn parse_key_id(text: &str) -> Option<[u8; KEY_ID_LEN]> {
    if text.len() != KEY_ID_LEN * 2 || !text.is_ascii() {
        return None;
    }
    let mut id = [0_u8; KEY_ID_LEN];
    for (slot, pair) in id.iter_mut().zip(text.as_bytes().chunks(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(id)
}

/// The role key holding `subkeys` of the deployment key `current` and, during a rotation, of the
/// `previous` one: what `admin deployment-key --for <role>` prints, for the role's
/// `NORBELYS_ROLE_KEY`.
///
/// # Errors
///
/// A key is not base64 of 32 bytes, or the derivation failed.
pub fn role_key(
    current: &SecretString,
    previous: Option<&SecretString>,
    subkeys: &[Subkey],
) -> Result<String, CryptoError> {
    let current = Root::derive(current)?;
    let mut rings = vec![EncodedRing::of(&current, subkeys)];
    if let Some(previous) = previous {
        let previous = Root::derive(previous)?;
        if previous.id != current.id {
            rings.push(EncodedRing::of(&previous, subkeys));
        }
    }
    let json = serde_json::to_vec(&EncodedRoleKey {
        v: ROLE_KEY_VERSION,
        rings,
    })
    .map_err(|_| CryptoError::InvalidRoleKey)?;
    Ok(format!("{ROLE_KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(json)))
}

/// The stand-in for a MAC subkey a role does not hold (see the module): random, so nothing it
/// makes verifies anywhere else or matches a stored row.
fn stand_in() -> Result<hmac::Key, CryptoError> {
    Ok(hmac::Key::new(
        hmac::HMAC_SHA256,
        &random_bytes(SUBKEY_LEN)?,
    ))
}

/// The first [`MESSAGE_ID_TAG_LEN`] bytes of `mac`.
fn truncated(mac: &hmac::Tag) -> [u8; MESSAGE_ID_TAG_LEN] {
    let mut tag = [0_u8; MESSAGE_ID_TAG_LEN];
    for (out, byte) in tag.iter_mut().zip(mac.as_ref()) {
        *out = *byte;
    }
    tag
}

/// The keys this process holds: the current deployment key's subkeys its role uses, the previous
/// key's during a rotation, and the stand-in (see the module).
#[derive(Clone)]
pub struct Keys {
    current: Ring,
    previous: Option<Ring>,
    stand_in: hmac::Key,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Keys(..)")
    }
}

impl Keys {
    /// Every subkey of the deployment key `encoded`.
    ///
    /// # Errors
    ///
    /// The key is not base64 of 32 bytes, or the random source failed.
    pub fn from_deployment_key(encoded: &SecretString) -> Result<Self, CryptoError> {
        let root = Root::derive(encoded)?;
        Ok(Self {
            current: Ring::new(root.id, &root.subkeys)?,
            previous: None,
            stand_in: stand_in()?,
        })
    }

    /// These keys, also holding the previous deployment key `encoded` during a rotation: values it
    /// sealed or signed open and verify; nothing new is made with it. The same subkeys are kept
    /// of it as of the current key.
    ///
    /// # Errors
    ///
    /// The key is not base64 of 32 bytes.
    pub fn with_previous(self, encoded: &SecretString) -> Result<Self, CryptoError> {
        let root = Root::derive(encoded)?;
        if root.id == self.current.id {
            return Ok(self);
        }
        let kept: Vec<Subkey> = Subkey::iter()
            .filter(|subkey| self.current.holds(*subkey))
            .collect();
        let previous = Ring::new(root.id, &root.subkeys)?.only(&kept);
        Ok(Self {
            previous: Some(previous),
            ..self
        })
    }

    /// The keys a role key holds (see [`role_key`]).
    ///
    /// # Errors
    ///
    /// [`CryptoError::InvalidRoleKey`] when `encoded` is not a role key of this release.
    pub fn from_role_key(encoded: &SecretString) -> Result<Self, CryptoError> {
        let text = encoded.expose_secret().trim();
        let body = text
            .strip_prefix(ROLE_KEY_PREFIX)
            .ok_or(CryptoError::InvalidRoleKey)?;
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| CryptoError::InvalidRoleKey)?;
        let decoded: EncodedRoleKey =
            serde_json::from_slice(&json).map_err(|_| CryptoError::InvalidRoleKey)?;
        if decoded.v != ROLE_KEY_VERSION {
            return Err(CryptoError::InvalidRoleKey);
        }
        let (current, previous) = match decoded.rings.as_slice() {
            [current] => (current.ring()?, None),
            [current, previous] => (current.ring()?, Some(previous.ring()?)),
            _ => return Err(CryptoError::InvalidRoleKey),
        };
        Ok(Self {
            current,
            previous,
            stand_in: stand_in()?,
        })
    }

    /// These keys without the subkeys outside `subkeys`: what a role keeps of a deployment key.
    #[must_use]
    pub fn only(self, subkeys: &[Subkey]) -> Self {
        Self {
            current: self.current.only(subkeys),
            previous: self.previous.map(|ring| ring.only(subkeys)),
            stand_in: self.stand_in,
        }
    }

    /// Checks that the current key's `subkeys` are all held: a role calls it at start.
    ///
    /// # Errors
    ///
    /// [`CryptoError::MissingSubkey`], naming the first one missing.
    pub fn require(&self, subkeys: &[Subkey]) -> Result<(), CryptoError> {
        match subkeys.iter().find(|subkey| !self.current.holds(**subkey)) {
            Some(missing) => Err(CryptoError::MissingSubkey(missing.as_str())),
            None => Ok(()),
        }
    }

    /// The current deployment key's id: what new sealed values and tags start with.
    #[must_use]
    pub fn key_id(&self) -> [u8; KEY_ID_LEN] {
        self.current.id
    }

    /// Whether `sealed` was sealed under the current key, which `admin secrets rotate` leaves
    /// as it is.
    #[cfg(test)]
    #[must_use]
    pub fn sealed_with_current(&self, sealed: &[u8]) -> bool {
        sealed.get(..KEY_ID_LEN) == Some(self.current.id.as_slice())
    }

    /// The held ring of key `id`.
    fn ring(&self, id: &[u8]) -> Option<&Ring> {
        std::iter::once(&self.current)
            .chain(self.previous.as_ref())
            .find(|ring| ring.id.as_slice() == id)
    }

    /// The current key's `subkey`, or the stand-in when this role does not hold it (see the
    /// module).
    fn mac(&self, subkey: Subkey) -> &hmac::Key {
        if let Some(key) = self.current.mac(subkey) {
            return key;
        }
        tracing::error!(
            subkey = subkey.as_str(),
            "a subkey this role does not hold was used; what it made will never verify"
        );
        &self.stand_in
    }

    /// The signed tag of `payload` under `subkey`: the current key's id, then the HMAC truncated
    /// to 28 bytes.
    fn sign(&self, subkey: Subkey, payload: &[u8]) -> Vec<u8> {
        let mac = hmac::sign(self.mac(subkey), payload);
        let mut tag = Vec::with_capacity(TAG_LEN);
        tag.extend_from_slice(&self.current.id);
        tag.extend(mac.as_ref().iter().take(MAC_LEN));
        tag
    }

    /// Whether `tag` is `payload`'s tag under `subkey` of the key it names, compared in constant
    /// time; false when that key or subkey is not held.
    fn verify(&self, subkey: Subkey, payload: &[u8], tag: &[u8]) -> bool {
        let Some((id, mac)) = tag.split_at_checked(KEY_ID_LEN) else {
            return false;
        };
        let Some(key) = self.ring(id).and_then(|ring| ring.mac(subkey)) else {
            return false;
        };
        let expected = hmac::sign(key, payload);
        expected
            .as_ref()
            .get(..MAC_LEN)
            .is_some_and(|expected| constant_time::verify_slices_are_equal(expected, mac).is_ok())
    }

    /// Seals `plaintext` for the context `aad` (for example `connections.credential:<ws>:<id>`)
    /// under the current key, whose id it starts with.
    ///
    /// # Errors
    ///
    /// The role holds no sealing key, or the random source failed.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let key = self
            .current
            .seal
            .as_ref()
            .ok_or(CryptoError::MissingSubkey(Subkey::Seal.as_str()))?;
        let mut nonce = [0_u8; NONCE_LEN];
        rand::SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| CryptoError::Random)?;
        let mut in_out = plaintext.to_vec();
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad),
            &mut in_out,
        )
        .map_err(|_| CryptoError::Random)?;
        let mut sealed = Vec::with_capacity(KEY_ID_LEN + NONCE_LEN + in_out.len());
        sealed.extend_from_slice(&self.current.id);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&in_out);
        Ok(sealed)
    }

    /// Opens a value sealed by [`Keys::seal`] for the same context, with the key it names.
    ///
    /// # Errors
    ///
    /// [`CryptoError::UnknownKey`] when the value names a key this process does not hold;
    /// [`CryptoError::MissingSubkey`] when the role holds no sealing key;
    /// [`CryptoError::Unseal`] when the context or the bytes are wrong.
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (id, rest) = sealed
            .split_at_checked(KEY_ID_LEN)
            .ok_or(CryptoError::Unseal)?;
        let ring = self.ring(id).ok_or(CryptoError::UnknownKey)?;
        let key = ring
            .seal
            .as_ref()
            .ok_or(CryptoError::MissingSubkey(Subkey::Seal.as_str()))?;
        let (nonce, ciphertext) = rest
            .split_at_checked(NONCE_LEN)
            .ok_or(CryptoError::Unseal)?;
        let nonce =
            aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| CryptoError::Unseal)?;
        let mut buffer = ciphertext.to_vec();
        let plaintext = key
            .open_in_place(nonce, aead::Aad::from(aad), &mut buffer)
            .map_err(|_| CryptoError::Unseal)?;
        Ok(plaintext.to_vec())
    }

    /// The tag that binds a pagination cursor to its query ([`crate::pagination`]).
    #[must_use]
    pub fn sign_cursor(&self, payload: &[u8]) -> Vec<u8> {
        self.sign(Subkey::Cursor, payload)
    }

    /// Verifies a cursor tag in constant time.
    #[must_use]
    pub fn verify_cursor(&self, payload: &[u8], tag: &[u8]) -> bool {
        self.verify(Subkey::Cursor, payload, tag)
    }

    /// The stored hash of a bearer secret (session and refresh tokens, sign-in links,
    /// invitations): looked up by equality, so it is a keyed MAC rather than a slow password hash.
    #[must_use]
    pub fn hash_token(&self, token: &str) -> Vec<u8> {
        hmac::sign(self.mac(Subkey::Token), token.as_bytes())
            .as_ref()
            .to_vec()
    }

    /// The MAC of a one-time sign-in code bound to its ceremony.
    #[must_use]
    pub fn hash_code(&self, ceremony: &[u8], code: &str) -> Vec<u8> {
        let mut context = hmac::Context::with_key(self.mac(Subkey::Code));
        context.update(ceremony);
        context.update(b":");
        context.update(code.as_bytes());
        context.sign().as_ref().to_vec()
    }

    /// The tag inside an open or click tracking token, so a token cannot be forged to record
    /// events for a message it does not name.
    #[must_use]
    pub fn sign_tracking(&self, payload: &[u8]) -> Vec<u8> {
        self.sign(Subkey::Tracking, payload)
    }

    /// Verifies a tracking tag in constant time.
    #[must_use]
    pub fn verify_tracking(&self, payload: &[u8], tag: &[u8]) -> bool {
        self.verify(Subkey::Tracking, payload, tag)
    }

    /// The tag of internal signed links (unsubscribes, downloads).
    #[must_use]
    pub fn sign_link(&self, payload: &[u8]) -> Vec<u8> {
        self.sign(Subkey::Link, payload)
    }

    /// Verifies a link tag in constant time.
    #[must_use]
    pub fn verify_link(&self, payload: &[u8], tag: &[u8]) -> bool {
        self.verify(Subkey::Link, payload, tag)
    }

    /// The CSRF token of a browser session: the signed tag of the session's id under a subkey of
    /// its own. It is bound to the session (another session's token never matches), needs no
    /// storage (any replica derives it again) and is useless without the session cookie it
    /// accompanies: the signed, session-bound form of the synchronizer token pattern
    /// (<https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html>).
    #[must_use]
    pub fn csrf_token(&self, session: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.sign(Subkey::Csrf, session))
    }

    /// Whether `token` is the CSRF token of `session`, compared in constant time.
    #[must_use]
    pub fn verify_csrf(&self, session: &[u8], token: &str) -> bool {
        URL_SAFE_NO_PAD
            .decode(token)
            .is_ok_and(|tag| self.verify(Subkey::Csrf, session, &tag))
    }

    /// The keyed hash of a client's IP address, for rate limits, the audit log and security
    /// events: stable within a deployment, so repeated attempts from one address correlate, and
    /// keyed, so the few billion IPv4 addresses cannot be enumerated back from a stored hash.
    #[must_use]
    pub fn hash_address(&self, address: &str) -> Vec<u8> {
        hmac::sign(self.mac(Subkey::Address), address.as_bytes())
            .as_ref()
            .to_vec()
    }

    /// The tag our Message-IDs carry: the HMAC of `payload` (the message's and its thread's ids),
    /// truncated to its first [`MESSAGE_ID_TAG_LEN`] bytes. A reply names the Message-ID it
    /// answers, and the tag proves we minted that id, so an inbound message cannot be steered into
    /// a thread by an id someone made up; 64 bits keep the header short and still leave no
    /// feasible guess.
    #[must_use]
    pub fn message_id_tag(&self, payload: &[u8]) -> [u8; MESSAGE_ID_TAG_LEN] {
        truncated(&hmac::sign(self.mac(Subkey::MessageId), payload))
    }

    /// Verifies a Message-ID tag in constant time, under every key held (the tag is too short to
    /// name its key).
    #[must_use]
    pub fn verify_message_id_tag(&self, payload: &[u8], tag: &[u8]) -> bool {
        std::iter::once(&self.current)
            .chain(self.previous.as_ref())
            .filter_map(|ring| ring.mac(Subkey::MessageId))
            .any(|key| {
                constant_time::verify_slices_are_equal(&truncated(&hmac::sign(key, payload)), tag)
                    .is_ok()
            })
    }
}

/// `n` random bytes, base64url without padding: tokens, secrets, nonces.
///
/// # Errors
///
/// The system random source failed.
pub fn random_token(n: usize) -> Result<String, CryptoError> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(n)?))
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

/// A uniformly random number of `digits` decimal digits, zero-padded (sign-in codes).
///
/// # Errors
///
/// The system random source failed.
pub fn random_digits(digits: u32) -> Result<String, CryptoError> {
    let bound = 10_u64.pow(digits);
    // Rejection sampling keeps the distribution uniform.
    let limit = u64::MAX - (u64::MAX % bound);
    loop {
        let bytes = random_bytes(8)?;
        let mut array = [0_u8; 8];
        array.copy_from_slice(&bytes);
        let value = u64::from_le_bytes(array);
        if value < limit {
            let width = usize::try_from(digits).map_err(|_| CryptoError::Random)?;
            return Ok(format!("{:0width$}", value % bound, width = width));
        }
    }
}

/// SHA-256 of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, data).as_ref().to_vec()
}

/// SHA-256 of data that arrives in pieces (a large file streamed from or to object storage), the
/// same digest as [`sha256`] of the pieces joined.
pub struct Sha256(digest::Context);

impl Sha256 {
    /// An empty digest.
    #[must_use]
    pub fn new() -> Self {
        Self(digest::Context::new(&digest::SHA256))
    }

    /// Adds the next piece.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// The digest of every piece added.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.0.finish().as_ref().to_vec()
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// Hex of `bytes`, lowercase.
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

/// A fresh deployment key, base64 of 32 random bytes.
///
/// # Errors
///
/// The system random source failed.
pub fn new_deployment_key() -> Result<String, CryptoError> {
    Ok(STANDARD.encode(random_bytes(32)?))
}

/// The Standard Webhooks signature of one delivery attempt
/// (<https://www.standardwebhooks.com/>): `v1,` followed by the base64 of the HMAC-SHA256,
/// keyed with the endpoint's secret bytes, over `{webhook_id}.{timestamp}.{body}`. The
/// timestamp is the attempt's own (unix seconds), so every attempt carries a fresh signature
/// for the same stable `webhook_id`, which consumers use to deduplicate.
#[must_use]
pub fn sign_webhook(secret: &[u8], webhook_id: &str, timestamp: i64, body: &[u8]) -> String {
    let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, secret));
    context.update(webhook_id.as_bytes());
    context.update(b".");
    context.update(timestamp.to_string().as_bytes());
    context.update(b".");
    context.update(body);
    format!("v1,{}", STANDARD.encode(context.sign().as_ref()))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use secrecy::SecretString;
    use strum::IntoEnumIterator as _;

    use super::{CryptoError, KEY_ID_LEN, Keys, Subkey, role_key};

    /// The deployment key made of the byte `byte` repeated.
    fn deployment(byte: u8) -> SecretString {
        SecretString::from(STANDARD.encode([byte; 32]))
    }

    /// One signed value per subkey, made with `keys`, checked with `verifier`: true when the
    /// verifier accepts what `keys` made. `Seal` opens a sealed value; the hashes compare.
    fn accepts(keys: &Keys, verifier: &Keys, subkey: Subkey) -> bool {
        let payload = b"payload";
        match subkey {
            Subkey::Seal => keys
                .seal(payload, b"context")
                .is_ok_and(|sealed| verifier.open(&sealed, b"context").is_ok()),
            Subkey::Cursor => verifier.verify_cursor(payload, &keys.sign_cursor(payload)),
            Subkey::Tracking => verifier.verify_tracking(payload, &keys.sign_tracking(payload)),
            Subkey::Link => verifier.verify_link(payload, &keys.sign_link(payload)),
            Subkey::Csrf => verifier.verify_csrf(payload, &keys.csrf_token(payload)),
            Subkey::MessageId => {
                verifier.verify_message_id_tag(payload, &keys.message_id_tag(payload))
            }
            Subkey::Token => keys.hash_token("secret") == verifier.hash_token("secret"),
            Subkey::Code => keys.hash_code(b"c", "123456") == verifier.hash_code(b"c", "123456"),
            Subkey::Address => {
                keys.hash_address("203.0.113.9") == verifier.hash_address("203.0.113.9")
            }
        }
    }

    /// A role key made for one subkey holds that subkey and no other, for every subkey: what it
    /// holds works exactly as the deployment key's own, what it lacks is refused at start by
    /// `require` and, if a call reaches it anyway, fails closed (sealing errs, a verification is
    /// refused, a MAC or hash made with the stand-in matches nothing the deployment key made). So
    /// the tracking role can hold no sealing key and the sender no identity secret.
    #[test]
    fn a_role_key_holds_its_subkeys_and_nothing_else() {
        let full = Keys::from_deployment_key(&deployment(1)).unwrap();
        for held in Subkey::iter() {
            let encoded = role_key(&deployment(1), None, &[held]).unwrap();
            assert!(encoded.starts_with("nbrk_"));
            let role = Keys::from_role_key(&SecretString::from(encoded)).unwrap();
            assert!(role.require(&[held]).is_ok(), "{held:?}");
            assert!(accepts(&role, &full, held), "{held:?} made by the role");
            assert!(accepts(&full, &role, held), "{held:?} checked by the role");
            for other in Subkey::iter().filter(|other| *other != held) {
                assert!(
                    matches!(role.require(&[held, other]), Err(CryptoError::MissingSubkey(name)) if name == other.as_str()),
                    "{other:?} beside {held:?}"
                );
                assert!(
                    !accepts(&role, &full, other),
                    "{other:?} made without the subkey"
                );
                assert!(
                    !accepts(&full, &role, other),
                    "{other:?} checked without the subkey"
                );
            }
        }
    }

    /// A deployment key kept to a role's subkeys behaves like that role's key: what the role
    /// does not use is gone from the process, not only from the configuration.
    #[test]
    fn a_deployment_key_keeps_only_the_roles_subkeys() {
        let full = Keys::from_deployment_key(&deployment(1)).unwrap();
        let tracking = full.clone().only(&[Subkey::Tracking, Subkey::Link]);
        assert!(tracking.require(&[Subkey::Tracking, Subkey::Link]).is_ok());
        assert!(matches!(
            tracking.seal(b"secret", b"context"),
            Err(CryptoError::MissingSubkey("seal"))
        ));
        let sealed = full.seal(b"secret", b"context").unwrap();
        assert!(matches!(
            tracking.open(&sealed, b"context"),
            Err(CryptoError::MissingSubkey("seal"))
        ));
        assert!(accepts(&full, &tracking, Subkey::Tracking));
    }

    /// During a rotation everything made with the previous key still opens and verifies, because
    /// it names its key, while everything new is made with the new key; without the previous
    /// key the old values name a key nobody holds. The role key made during the rotation carries
    /// both keys.
    #[test]
    fn values_name_their_key_and_open_during_a_rotation() {
        let old = Keys::from_deployment_key(&deployment(1)).unwrap();
        let sealed = old.seal(b"credential", b"context").unwrap();
        let rotated = Keys::from_deployment_key(&deployment(2))
            .unwrap()
            .with_previous(&deployment(1))
            .unwrap();
        assert_eq!(sealed.get(..KEY_ID_LEN), Some(old.key_id().as_slice()));
        assert_eq!(rotated.open(&sealed, b"context").unwrap(), b"credential");
        assert!(!rotated.sealed_with_current(&sealed));
        let resealed = rotated.seal(b"credential", b"context").unwrap();
        assert!(rotated.sealed_with_current(&resealed));
        assert_ne!(rotated.key_id(), old.key_id());
        for subkey in [
            Subkey::Cursor,
            Subkey::Tracking,
            Subkey::Link,
            Subkey::Csrf,
            Subkey::MessageId,
        ] {
            assert!(
                accepts(&old, &rotated, subkey),
                "{subkey:?} of the previous key"
            );
        }
        let new_only = Keys::from_deployment_key(&deployment(2)).unwrap();
        assert!(matches!(
            new_only.open(&sealed, b"context"),
            Err(CryptoError::UnknownKey)
        ));
        assert!(!accepts(&old, &new_only, Subkey::Tracking));
        let during = role_key(&deployment(2), Some(&deployment(1)), &[Subkey::Seal]).unwrap();
        let role = Keys::from_role_key(&SecretString::from(during)).unwrap();
        assert_eq!(role.open(&sealed, b"context").unwrap(), b"credential");
        assert!(role.sealed_with_current(&role.seal(b"x", b"context").unwrap()));
    }

    /// A signed tag is as long as a plain HMAC-SHA256 (32 bytes, 43 characters in base64url), so
    /// naming the key changed no format that carries one; a tag that names no held key, or that is
    /// cut short, is refused.
    #[test]
    fn tags_keep_their_length_and_refuse_alterations() {
        let keys = Keys::from_deployment_key(&deployment(1)).unwrap();
        let tag = keys.sign_tracking(b"payload");
        assert_eq!(tag.len(), 32);
        assert_eq!(keys.csrf_token(b"session").len(), 43);
        assert!(!keys.verify_tracking(b"payload", tag.get(..31).unwrap()));
        let mut renamed = tag.clone();
        renamed[0] ^= 1;
        assert!(!keys.verify_tracking(b"payload", &renamed));
        let mut altered = tag;
        altered[31] ^= 1;
        assert!(!keys.verify_tracking(b"payload", &altered));
    }

    /// Anything but a role key of this release is refused before it is used: another prefix, a
    /// body that is not base64url JSON, an unknown subkey, a short subkey, or more than the
    /// current and the previous key.
    #[test]
    fn malformed_role_keys_are_refused() {
        let encode = |json: &str| format!("nbrk_{}", URL_SAFE_NO_PAD.encode(json));
        let subkey = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let ring = format!(r#"{{"kid":"0a0b0c0d","subkeys":{{"seal":"{subkey}"}}}}"#);
        assert!(
            Keys::from_role_key(&SecretString::from(encode(&format!(
                r#"{{"v":1,"rings":[{ring}]}}"#
            ))))
            .is_ok()
        );
        for wrong in [
            format!("nbk_{}", URL_SAFE_NO_PAD.encode("{}")),
            "nbrk_!!!".to_owned(),
            encode(&format!(r#"{{"v":2,"rings":[{ring}]}}"#)),
            encode(r#"{"v":1,"rings":[]}"#),
            encode(&format!(r#"{{"v":1,"rings":[{ring},{ring},{ring}]}}"#)),
            encode(&format!(
                r#"{{"v":1,"rings":[{{"kid":"0a0b0c0d","subkeys":{{"sealing":"{subkey}"}}}}]}}"#
            )),
            encode(r#"{"v":1,"rings":[{"kid":"0a0b0c0d","subkeys":{"seal":"AAAA"}}]}"#),
            encode(&format!(
                r#"{{"v":1,"rings":[{{"kid":"zz","subkeys":{{"seal":"{subkey}"}}}}]}}"#
            )),
        ] {
            assert!(
                matches!(
                    Keys::from_role_key(&SecretString::from(wrong.clone())),
                    Err(CryptoError::InvalidRoleKey)
                ),
                "{wrong}"
            );
        }
    }
}
