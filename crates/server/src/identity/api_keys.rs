//! API keys: long-lived credentials a member delegates to a program.
//!
//! A key is `nb_live_` or `nb_test_`, then 32 base32 characters of 20 random bytes, then a
//! 6-character base62 CRC-32 of everything before it, so a typo or a pasted fragment is
//! refused before any query and secret scanners can recognise the format. Only the SHA-256
//! of the key is stored; it is found with `api_key_by_hash()`, the lookup that runs before
//! any workspace is known. A key is delegated by a member: its scopes are those it was
//! created with, intersected with its creator's current role, and it dies with the
//! creator's membership.

use serde::Serialize;
use uuid::Uuid;

use crate::crypto::{self, CryptoError};
use crate::db::Tx;
use crate::domain::ids::{ApiKey, Id, User, WorkspaceId};
use crate::domain::scope::{MembershipRole, ScopeSet};
use crate::domain::time::Timestamp;

const LIVE: &str = "nb_live_";
const TEST: &str = "nb_test_";
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A workspace's mode, which its keys share: `live` reaches providers, `test` sends through a fake
/// transport only.
// The OpenAPI document names it `WorkspaceMode`: a workspace's mode and its keys' are one value.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[schema(as = WorkspaceMode)]
pub enum KeyMode {
    Live,
    Test,
}

impl KeyMode {
    fn prefix(self) -> &'static str {
        match self {
            Self::Live => LIVE,
            Self::Test => TEST,
        }
    }

    /// The workspace mode the key's workspace must be in.
    #[must_use]
    pub fn workspace_mode(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Test => "test",
        }
    }
}

/// A fresh key: the secret, shown once, and what is stored.
pub struct NewKey {
    pub secret: String,
    pub hash: Vec<u8>,
    pub display_prefix: String,
}

/// Generates a key for `mode`.
///
/// # Errors
///
/// The random source failed.
pub fn generate(mode: KeyMode) -> Result<NewKey, CryptoError> {
    let random = crypto::random_bytes(20)?;
    let body = format!("{}{}", mode.prefix(), base32(&random));
    let secret = format!("{body}{}", base62_6(crc32(body.as_bytes())));
    let display_prefix = secret.chars().take(12).collect();
    Ok(NewKey {
        hash: crypto::sha256(secret.as_bytes()),
        secret,
        display_prefix,
    })
}

/// The mode of a well-formed key whose checksum matches; `None` for anything else.
#[must_use]
pub fn parse(secret: &str) -> Option<KeyMode> {
    let mode = if secret.starts_with(LIVE) {
        KeyMode::Live
    } else if secret.starts_with(TEST) {
        KeyMode::Test
    } else {
        return None;
    };
    let split = secret.len().checked_sub(6)?;
    let (body, checksum) = (secret.get(..split)?, secret.get(split..)?);
    let random = body.get(mode.prefix().len()..)?;
    let well_formed = random.len() == 32 && random.bytes().all(|byte| BASE32.contains(&byte));
    (well_formed && base62_6(crc32(body.as_bytes())) == checksum).then_some(mode)
}

/// The stored hash of a key.
#[must_use]
pub fn hash(secret: &str) -> Vec<u8> {
    crypto::sha256(secret.as_bytes())
}

/// What `api_key_by_hash()` returns.
#[derive(Debug, Clone)]
pub struct StoredKey {
    pub workspace: Uuid,
    pub id: Id<ApiKey>,
    pub scopes: Vec<String>,
    pub created_by: Id<User>,
    pub expires_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

/// Finds a key by its hash, before any workspace is known.
///
/// # Errors
///
/// The database is unavailable.
pub async fn find(tx: &mut Tx, hash: &[u8]) -> Result<Option<StoredKey>, sqlx::Error> {
    sqlx::query_as!(
        StoredKey,
        r#"SELECT workspace_id AS "workspace!", id AS "id!: Id<ApiKey>", scopes AS "scopes!",
                  created_by AS "created_by!: Id<User>", expires_at AS "expires_at: Timestamp",
                  revoked_at AS "revoked_at: Timestamp"
             FROM api_key_by_hash($1)"#,
        hash
    )
    .fetch_optional(&mut **tx)
    .await
}

/// The creator's membership and the workspace, read inside the key's workspace.
pub struct Standing {
    pub role: MembershipRole,
    pub active: bool,
    pub workspace_mode: String,
    pub workspace_deleted: bool,
}

/// Reads what decides whether a key still acts: its creator's membership and its
/// workspace.
///
/// # Errors
///
/// The database is unavailable.
pub async fn standing(
    tx: &mut Tx,
    workspace: WorkspaceId,
    creator: Id<User>,
) -> Result<Option<Standing>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT m.role, m.status, w.mode, w.deleted_at IS NOT NULL AS "deleted!"
             FROM memberships m JOIN workspaces w ON w.id = m.workspace_id
            WHERE m.workspace_id = $1 AND m.user_id = $2"#,
        workspace.uuid(),
        creator.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|row| {
        let role = row.role.parse::<MembershipRole>().ok()?;
        Some(Standing {
            role,
            active: row.status == "active",
            workspace_mode: row.mode,
            workspace_deleted: row.deleted,
        })
    }))
}

/// Records a key and returns its secret, shown once. The caller has checked that the
/// creator may delegate `scopes` and runs inside the workspace.
///
/// # Errors
///
/// The random source failed or the database refused the row.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    created_by: Id<User>,
    name: &str,
    scopes: ScopeSet,
    mode: KeyMode,
    expires_at: Option<Timestamp>,
) -> Result<(Id<ApiKey>, NewKey), CreateError> {
    let key = generate(mode)?;
    let id = Id::<ApiKey>::new();
    sqlx::query!(
        "INSERT INTO api_keys (workspace_id, id, name, prefix, secret_hash, scopes, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        workspace.uuid(),
        id.uuid(),
        name,
        key.display_prefix,
        key.hash,
        &scopes.to_strings(),
        created_by.uuid(),
        // `as _` is sqlx's override syntax: the column's own type decides the encoding.
        expires_at as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok((id, key))
}

/// Why a key could not be created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A key's state, derived from its row.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum KeyStatus {
    /// It authenticates.
    Active,
    /// Revoked by a person or with its creator's membership.
    Revoked,
    /// Past its `expires_at`.
    Expired,
}

/// An API key as the dashboard shows it; the secret only in the answer that created it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ApiKeyObject {
    pub id: Id<ApiKey>,
    /// A name for people.
    pub name: String,
    /// The key's first 12 characters, to recognise it.
    pub prefix: String,
    /// `live` or `test`.
    pub mode: KeyMode,
    /// The scopes it was created with; it acts with these intersected with its creator's current
    /// role.
    pub scopes: Vec<String>,
    /// The member who delegated it; it dies with their membership.
    pub created_by: Id<User>,
    /// `active`, `revoked` or `expired`.
    pub status: KeyStatus,
    pub expires_at: Option<Timestamp>,
    pub last_used_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The version `If-Match` names.
    pub version: i64,
    /// The secret, in the answer that created the key and never again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

struct KeyRow {
    id: Id<ApiKey>,
    name: String,
    prefix: String,
    scopes: Vec<String>,
    created_by: Id<User>,
    expires_at: Option<Timestamp>,
    last_used_at: Option<Timestamp>,
    revoked_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl KeyRow {
    fn object(self, now: Timestamp) -> ApiKeyObject {
        let status = if self.revoked_at.is_some() {
            KeyStatus::Revoked
        } else if self.expires_at.is_some_and(|expires| expires <= now) {
            KeyStatus::Expired
        } else {
            KeyStatus::Active
        };
        ApiKeyObject {
            mode: if self.prefix.starts_with(TEST) {
                KeyMode::Test
            } else {
                KeyMode::Live
            },
            id: self.id,
            name: self.name,
            prefix: self.prefix,
            scopes: self.scopes,
            created_by: self.created_by,
            status,
            expires_at: self.expires_at,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
            version: crate::http::versioning::of(self.updated_at),
            updated_at: self.updated_at,
            secret: None,
        }
    }
}

/// One page of `workspace`'s keys by id, optionally of one `status`.
///
/// # Errors
///
/// The database failed.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<KeyStatus>,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<ApiKeyObject>, sqlx::Error> {
    let status: Option<&'static str> = status.map(Into::into);
    let rows = sqlx::query_as!(
        KeyRow,
        r#"SELECT id AS "id: Id<ApiKey>", name, prefix, scopes, created_by AS "created_by: Id<User>",
                  expires_at AS "expires_at: Timestamp", last_used_at AS "last_used_at: Timestamp",
                  revoked_at AS "revoked_at: Timestamp", created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
             FROM api_keys
            WHERE workspace_id = $1
              AND ($2::text IS NULL OR $2 = CASE WHEN revoked_at IS NOT NULL THEN 'revoked'
                                                 WHEN expires_at <= now() THEN 'expired'
                                                 ELSE 'active' END)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN id > $3 ELSE id < $3 END)
            ORDER BY CASE WHEN $4 THEN id END, id DESC LIMIT $5"#,
        workspace.uuid(),
        status,
        after,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    let now = crate::process::now();
    Ok(rows.into_iter().map(|row| row.object(now)).collect())
}

/// Key `id` of `workspace`, without its secret.
///
/// # Errors
///
/// The database failed.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<ApiKey>,
) -> Result<Option<ApiKeyObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        KeyRow,
        r#"SELECT id AS "id: Id<ApiKey>", name, prefix, scopes, created_by AS "created_by: Id<User>",
                  expires_at AS "expires_at: Timestamp", last_used_at AS "last_used_at: Timestamp",
                  revoked_at AS "revoked_at: Timestamp", created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
             FROM api_keys WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| row.object(crate::process::now())))
}

/// Locks key `id` and answers its current version, for an update's `If-Match`.
///
/// # Errors
///
/// The database failed.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<ApiKey>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM api_keys
            WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(crate::http::versioning::of))
}

/// Renames key `id` (the caller holds its lock).
///
/// # Errors
///
/// The database failed.
pub async fn rename(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<ApiKey>,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE api_keys SET name = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
        name,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Revokes key `id` when it is live; its hash, for the authority to forget, or `None` when no live
/// key was revoked (absent, or revoked already).
///
/// # Errors
///
/// The database failed.
pub async fn revoke(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<ApiKey>,
) -> Result<Option<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar!(
        "UPDATE api_keys SET revoked_at = now()
          WHERE workspace_id = $1 AND id = $2 AND revoked_at IS NULL
         RETURNING secret_hash",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await
}

fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buffer: u32 = 0;
    let mut bits = 0_u32;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(base32_char(buffer >> bits));
        }
    }
    if bits > 0 {
        out.push(base32_char(buffer << (5 - bits)));
    }
    out
}

fn base32_char(value: u32) -> char {
    usize::try_from(value & 31)
        .ok()
        .and_then(|index| BASE32.get(index))
        .map_or('a', |byte| char::from(*byte))
}

fn base62_6(mut value: u32) -> String {
    let mut digits = [b'0'; 6];
    for slot in digits.iter_mut().rev() {
        let index = usize::try_from(value % 62).unwrap_or_default();
        *slot = BASE62.get(index).copied().unwrap_or(b'0');
        value /= 62;
    }
    digits.iter().map(|byte| char::from(*byte)).collect()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Counts matching rows without materializing their data, bounded to `cap + 1`.
///
/// # Errors
///
/// The database failed.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<KeyStatus>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM api_keys WHERE workspace_id = $1 AND ($2::text IS NULL OR $2 = CASE WHEN revoked_at IS NOT NULL THEN 'revoked' WHEN expires_at <= now() THEN 'expired' ELSE 'active' END) LIMIT $3) counted").bind(workspace.uuid()).bind(status.map(<&'static str>::from)).bind(cap.saturating_add(1)).fetch_one(&mut **tx).await
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;

    /// A generated key parses back to its mode, carries its prefix, a 12-character display prefix
    /// and the hash it is stored under; a key with one character changed (its checksum no longer
    /// matches), another prefix, or the wrong length is refused before any lookup, so a typo or a
    /// pasted fragment never reaches the database.
    #[test]
    fn keys_parse_only_as_generated() {
        for mode in KeyMode::iter() {
            let key = generate(mode).unwrap();
            assert_eq!(parse(&key.secret), Some(mode));
            assert!(key.secret.starts_with(mode.prefix()));
            assert_eq!(key.display_prefix.chars().count(), 12);
            assert_eq!(key.hash, hash(&key.secret));
            let mut altered: Vec<char> = key.secret.chars().collect();
            let at = mode.prefix().len() + 3;
            altered[at] = if altered[at] == 'a' { 'b' } else { 'a' };
            assert_eq!(parse(&altered.into_iter().collect::<String>()), None);
            assert_eq!(parse(&key.secret[..key.secret.len() - 1]), None);
        }
        for refused in [
            "",
            "nb_live_",
            "nbs_abc",
            "nb_prod_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa000000",
        ] {
            assert_eq!(parse(refused), None, "{refused}");
        }
    }
}
