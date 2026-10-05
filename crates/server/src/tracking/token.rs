//! The signed tokens of the tracking and unsubscribe links: compact, URL-safe and
//! tamper-evident.
//!
//! # What a token carries
//!
//! - an **open** (`/t/o/{token}`): the workspace and the message;
//! - a **click** (`/t/c/{token}`): the workspace, the message, the link's position in the body
//!   and the destination URL, so the redirect needs no database and cannot be pointed anywhere
//!   else (a signed destination is not an open redirect);
//! - an **unsubscribe** (`/u/{token}`): the workspace, the message and the address to suppress.
//!   The address travels in the token because the unsubscribe must keep working long after the
//!   message's row has been archived, and because a message can have several recipients while
//!   the token is minted for one of them.
//!
//! Nothing in a token is secret; it is signed, not encrypted. The workspace inside a verified
//! token is trusted the way a credential's is: only this deployment could have signed it.
//!
//! # Encoding
//!
//! `base64url(payload || tag)` without padding, where `payload` is
//! `version (1) | kind (1) | workspace uuid (16) | message uuid (16) | rest` and `rest` is empty
//! for an open, a big-endian `u16` link position followed by the URL's UTF-8 bytes for a click,
//! and the address's UTF-8 bytes for an unsubscribe. `tag` is the 32-byte signed tag of the
//! payload (`crypto::Keys`): the signing key's 4-byte id, then its HMAC-SHA256 truncated to 28
//! bytes, so a token signed before a key rotation still names the key that verifies it. Opens and
//! clicks are signed under the deployment's tracking key, unsubscribes under its link key. The
//! link key also signs local download links, whose payloads start with the text `files`; every
//! token payload starts with the version byte `1`, so a signature of one can never verify as the
//! other. An open token is 88 characters.
//!
//! The version byte lets a later format (a rotated key, more fields) be read beside this one;
//! a token of an unknown version is refused.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::Uuid;

use crate::crypto::Keys;
use crate::domain::ids::{Id, Message, WorkspaceId};

/// The format this module writes.
const VERSION: u8 = 1;
/// The bytes of a signed tag: the key id, then the truncated HMAC-SHA256.
const TAG_LEN: usize = 32;
/// Version, kind, workspace and message.
const HEAD_LEN: usize = 34;
/// The longest destination a click token carries, in bytes; a longer link is left as written.
pub const URL_MAX: usize = 2_048;

/// What a token says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// The open pixel of `message`.
    Open {
        workspace: WorkspaceId,
        message: Id<Message>,
    },
    /// The `link`-th link (0-based, in document order) of `message`, leading to `url`.
    Click {
        workspace: WorkspaceId,
        message: Id<Message>,
        link: u16,
        url: String,
    },
    /// The unsubscribe link of `email`, a recipient of `message`.
    Unsubscribe {
        workspace: WorkspaceId,
        message: Id<Message>,
        email: String,
    },
}

/// Why a token was refused. Neither case says which part was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Not base64url, too short, an unknown version or kind, or a body that is not UTF-8.
    #[error("the token is malformed")]
    Malformed,
    /// The signature does not match: the token was altered or not made here.
    #[error("the token's signature does not match")]
    Signature,
}

impl Token {
    /// The token's kind byte.
    fn kind(&self) -> u8 {
        match self {
            Self::Open { .. } => 1,
            Self::Click { .. } => 2,
            Self::Unsubscribe { .. } => 3,
        }
    }

    /// The token as it appears in a URL.
    #[must_use]
    pub fn encode(&self, keys: &Keys) -> String {
        let (workspace, message) = match self {
            Self::Open { workspace, message }
            | Self::Click {
                workspace, message, ..
            }
            | Self::Unsubscribe {
                workspace, message, ..
            } => (workspace, message),
        };
        let mut payload = Vec::with_capacity(HEAD_LEN + TAG_LEN);
        payload.push(VERSION);
        payload.push(self.kind());
        payload.extend_from_slice(workspace.uuid().as_bytes());
        payload.extend_from_slice(message.uuid().as_bytes());
        match self {
            Self::Open { .. } => {}
            Self::Click { link, url, .. } => {
                payload.extend_from_slice(&link.to_be_bytes());
                payload.extend_from_slice(url.as_bytes());
            }
            Self::Unsubscribe { email, .. } => payload.extend_from_slice(email.as_bytes()),
        }
        let tag = match self {
            Self::Open { .. } | Self::Click { .. } => keys.sign_tracking(&payload),
            Self::Unsubscribe { .. } => keys.sign_link(&payload),
        };
        payload.extend_from_slice(&tag);
        URL_SAFE_NO_PAD.encode(payload)
    }

    /// The absolute URL of the token's route on `origin` (`https://host`, no path).
    #[must_use]
    pub fn url(&self, keys: &Keys, origin: &str) -> String {
        let route = match self {
            Self::Open { .. } => "t/o",
            Self::Click { .. } => "t/c",
            Self::Unsubscribe { .. } => "u",
        };
        format!(
            "{}/{route}/{}",
            origin.trim_end_matches('/'),
            self.encode(keys)
        )
    }

    /// Reads and verifies a token.
    ///
    /// # Errors
    ///
    /// [`TokenError::Malformed`] or [`TokenError::Signature`].
    #[allow(dead_code, reason = "called by the tracking routes")]
    pub fn decode(keys: &Keys, token: &str) -> Result<Self, TokenError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| TokenError::Malformed)?;
        let split = bytes
            .len()
            .checked_sub(TAG_LEN)
            .filter(|split| *split >= HEAD_LEN)
            .ok_or(TokenError::Malformed)?;
        let (payload, tag) = bytes.split_at_checked(split).ok_or(TokenError::Malformed)?;
        let (head, rest) = payload
            .split_at_checked(HEAD_LEN)
            .ok_or(TokenError::Malformed)?;
        let [version, kind, ids @ ..] = head else {
            return Err(TokenError::Malformed);
        };
        let (workspace, message) = ids.split_at_checked(16).ok_or(TokenError::Malformed)?;
        let (version, kind, workspace, message) =
            (*version, *kind, uuid(workspace)?, uuid(message)?);
        if version != VERSION {
            return Err(TokenError::Malformed);
        }
        let verified = match kind {
            1 | 2 => keys.verify_tracking(payload, tag),
            3 => keys.verify_link(payload, tag),
            _ => return Err(TokenError::Malformed),
        };
        if !verified {
            return Err(TokenError::Signature);
        }
        let workspace = WorkspaceId::trusted(workspace);
        let message = Id::from_uuid(message);
        let text =
            |bytes: &[u8]| String::from_utf8(bytes.to_vec()).map_err(|_| TokenError::Malformed);
        match (kind, rest) {
            (1, []) => Ok(Self::Open { workspace, message }),
            (2, [high, low, url @ ..]) => Ok(Self::Click {
                workspace,
                message,
                link: u16::from_be_bytes([*high, *low]),
                url: text(url)?,
            }),
            (3, email) if !email.is_empty() => Ok(Self::Unsubscribe {
                workspace,
                message,
                email: text(email)?,
            }),
            _ => Err(TokenError::Malformed),
        }
    }
}

fn uuid(bytes: &[u8]) -> Result<Uuid, TokenError> {
    Uuid::from_slice(bytes).map_err(|_| TokenError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::keys;

    fn samples() -> Vec<Token> {
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        let message = Id::new();
        vec![
            Token::Open { workspace, message },
            Token::Click {
                workspace,
                message,
                link: 7,
                url: "https://example.com/pricing?plan=pro&ref=mail#top".to_owned(),
            },
            Token::Unsubscribe {
                workspace,
                message,
                email: "Ada.Lovelace+news@Example.com".to_owned(),
            },
        ]
    }

    /// Every kind of token reads back as what was written, and its text is URL-safe (no `+`,
    /// `/` or `=`), so it can sit in a path segment as it is.
    #[test]
    fn tokens_round_trip_and_are_url_safe() {
        let keys = keys();
        for token in samples() {
            let encoded = token.encode(&keys);
            assert!(
                encoded
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{encoded}"
            );
            assert_eq!(Token::decode(&keys, &encoded), Ok(token));
        }
    }

    /// An open token is short (88 characters), and the URL names each kind's route on the
    /// origin, whatever trailing slash the origin was written with.
    #[test]
    fn urls_name_each_route() {
        let keys = keys();
        let [open, click, unsubscribe] = samples().try_into().unwrap();
        assert_eq!(open.encode(&keys).len(), 88);
        assert!(
            open.url(&keys, "https://t.example/")
                .starts_with("https://t.example/t/o/")
        );
        assert!(
            click
                .url(&keys, "https://t.example")
                .starts_with("https://t.example/t/c/")
        );
        assert!(
            unsubscribe
                .url(&keys, "https://t.example")
                .starts_with("https://t.example/u/")
        );
    }

    /// Changing any single byte of a token, or relabelling an open as an unsubscribe, is refused:
    /// the destination of a click or the address of an unsubscribe cannot be swapped.
    #[test]
    fn altered_tokens_are_refused() {
        let keys = keys();
        for token in samples() {
            let mut bytes = URL_SAFE_NO_PAD.decode(token.encode(&keys)).unwrap();
            for at in 0..bytes.len() {
                bytes[at] ^= 0x01;
                let refused = Token::decode(&keys, &URL_SAFE_NO_PAD.encode(&bytes));
                assert!(refused.is_err(), "byte {at} of {token:?}");
                bytes[at] ^= 0x01;
            }
        }
        let open = samples().remove(0).encode(&keys);
        let mut relabelled = URL_SAFE_NO_PAD.decode(open).unwrap();
        relabelled[1] = 3;
        assert_eq!(
            Token::decode(&keys, &URL_SAFE_NO_PAD.encode(relabelled)),
            Err(TokenError::Signature)
        );
    }

    /// A token signed by another deployment, a truncated one and text that is not a token are
    /// all refused, without saying which part was wrong.
    #[test]
    fn foreign_and_malformed_tokens_are_refused() {
        let other = Keys::from_deployment_key(&secrecy::SecretString::from(
            base64::engine::general_purpose::STANDARD.encode([9_u8; 32]),
        ))
        .unwrap();
        let foreign = samples().remove(0).encode(&other);
        assert_eq!(Token::decode(&keys(), &foreign), Err(TokenError::Signature));
        let short = &foreign[..40];
        assert_eq!(Token::decode(&keys(), short), Err(TokenError::Malformed));
        for junk in ["", "not a token", "%%%%"] {
            assert_eq!(Token::decode(&keys(), junk), Err(TokenError::Malformed));
        }
    }
}
