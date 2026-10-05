//! Profiles: each one's API URL, credential and `listen` state, kept between runs.
//!
//! One JSON file holds every profile: `$XDG_CONFIG_HOME/norbelys/config.json`, else
//! `~/.config/norbelys/config.json`, else `%APPDATA%\norbelys\config.json`, or the file
//! `--config` names. A profile (`--profile`, `default` when not given) holds an API URL when it
//! is not the default one, then either an API key stored by `login --api-key` or the CLI session
//! of a device login, and the secret and cursor of `listen`.
//!
//! # Secrets
//!
//! The file holds API keys, session tokens and signing secrets. On Unix it is written with
//! permissions `0600` in a directory created `0700`, so no other user can read it, and always
//! through a temporary file renamed over it, so a crash never leaves it half written and a
//! reader sees the old file or the new one.
//!
//! # Concurrent processes
//!
//! Several `norbelys` processes may run at once, typically a long `listen` beside one-off
//! commands. Every change is a read-modify-write under an exclusive lock on `<file>.lock`, a
//! file that is never renamed (a lock on the data file itself would be lost when the rename
//! replaces it), and the change is applied to what the file holds once the lock is taken, never
//! to a copy read before, so no process overwrites another's change. The refresh of a CLI
//! session runs under the same lock: refresh tokens are single use and the server revokes a
//! session whose refresh token is presented twice, so two processes must never refresh with the
//! same token; the one that waited for the lock finds the token the other stored.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The configuration file: the profiles by name.
#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// One profile. Its credential is an API key or a CLI session, never both: storing one removes
/// the other, so which one a command uses never depends on precedence.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    /// The API's base URL, when it is not the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// An API key (`nb_live_…`, `nb_test_…`) stored by `login --api-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The CLI session of a device login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    /// The state of `listen`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<Listen>,
}

/// A CLI session: the OAuth tokens of a device login.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    /// The access token (`nbc_…`), sent as the bearer credential.
    pub access_token: String,
    /// The refresh token that renews the access token once; each refresh returns a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// When the access token expires, in Unix seconds.
    pub expires_at: i64,
}

/// What `listen` keeps between runs.
#[derive(Clone, Serialize, Deserialize)]
pub struct Listen {
    /// The Standard Webhooks secret (`whsec_…`) forwarded events are signed with.
    pub secret: String,
    /// The id of the last event forwarded; the next run resumes after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Why the configuration file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No directory to keep the file in.
    #[error("no configuration directory: set XDG_CONFIG_HOME or HOME, or pass --config")]
    NoDirectory,
    /// The file or its directory could not be read, written or locked.
    #[error("cannot use the configuration file {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    /// The file is not a configuration.
    #[error("the configuration file {} is not valid: {source}", path.display())]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// The configuration file's default place.
///
/// # Errors
///
/// [`ConfigError::NoDirectory`] when the environment names no home or configuration directory.
pub fn default_path() -> Result<PathBuf, ConfigError> {
    let variable = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let directory = variable("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| variable("HOME").map(|home| PathBuf::from(home).join(".config")))
        .or_else(|| variable("APPDATA").map(PathBuf::from))
        .ok_or(ConfigError::NoDirectory)?;
    Ok(directory.join("norbelys").join("config.json"))
}

/// Reads the configuration; a file that does not exist yet is an empty one.
///
/// # Errors
///
/// When the file exists but cannot be read or is not a configuration.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Applies `change` to the configuration under the lock and saves it, returning what `change`
/// returns.
///
/// # Errors
///
/// When the file cannot be locked, read or written.
pub fn update<T>(path: &Path, change: impl FnOnce(&mut Config) -> T) -> Result<T, ConfigError> {
    let mut locked = Locked::acquire(path)?;
    let result = change(&mut locked.config);
    locked.save()?;
    Ok(result)
}

/// The configuration read under the exclusive lock, which is held until this value is dropped.
pub struct Locked {
    /// The lock file; closing it releases the lock.
    _lock: File,
    path: PathBuf,
    /// The configuration as the file held it when the lock was taken.
    pub config: Config,
}

impl Locked {
    /// Takes the lock, waiting for another process that holds it, then reads the file.
    ///
    /// # Errors
    ///
    /// When the directory or the lock file cannot be created, or the file cannot be read.
    pub fn acquire(path: &Path) -> Result<Self, ConfigError> {
        let failed = |source| ConfigError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(directory) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(directory).map_err(failed)?;
        }
        let lock = private(OpenOptions::new().create(true).truncate(false).write(true))
            .open(sibling(path, "lock"))
            .map_err(failed)?;
        lock.lock().map_err(failed)?;
        let config = load(path)?;
        Ok(Self {
            _lock: lock,
            path: path.to_owned(),
            config,
        })
    }

    /// Writes the configuration: a private temporary file, synced, then renamed over the file.
    ///
    /// # Errors
    ///
    /// When the temporary file cannot be written or renamed.
    pub fn save(&self) -> Result<(), ConfigError> {
        let failed = |source| ConfigError::Io {
            path: self.path.clone(),
            source,
        };
        let mut text = serde_json::to_string_pretty(&self.config)
            .map_err(|source| failed(io::Error::other(source)))?;
        text.push('\n');
        let temporary = sibling(&self.path, "tmp");
        let mut file = private(OpenOptions::new().create(true).truncate(true).write(true))
            .open(&temporary)
            .map_err(failed)?;
        // A file left by an earlier crash keeps its old mode when reopened: set it again.
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .map_err(failed)?;
        file.write_all(text.as_bytes()).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        fs::rename(&temporary, &self.path).map_err(failed)
    }
}

/// `options` creating files readable by their owner only.
fn private(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options
}

/// The file beside `path` named `<its name>.<suffix>`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, Listen, Locked, load, update};
    use crate::testing::Scratch;

    /// The file and its directory are private to their owner and a saved change reads back,
    /// beside other profiles that stay untouched: the file holds credentials, and one
    /// profile's change must not lose another's.
    #[test]
    fn changes_are_saved_privately_and_keep_other_profiles() {
        let scratch = Scratch::new();
        let path = scratch.config();
        update(&path, |config| {
            config.profiles.entry("ci".to_owned()).or_default().api_key =
                Some("nb_test_1".to_owned());
        })
        .unwrap();
        update(&path, |config| {
            config
                .profiles
                .entry("default".to_owned())
                .or_default()
                .listen = Some(Listen {
                secret: "whsec_x".to_owned(),
                cursor: Some("evt_1".to_owned()),
            });
        })
        .unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.profiles["ci"].api_key.as_deref(), Some("nb_test_1"));
        let listen = config.profiles["default"].listen.as_ref().unwrap();
        assert_eq!(listen.cursor.as_deref(), Some("evt_1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &std::path::Path| {
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777
            };
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    #[test]
    fn malformed_and_unreadable_configs_are_reported_without_overwriting_them() {
        let scratch = Scratch::new();
        let path = scratch.config();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{broken").unwrap();
        assert!(matches!(load(&path), Err(ConfigError::Invalid { .. })));
        assert!(matches!(
            update(&path, |_| ()),
            Err(ConfigError::Invalid { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        assert!(matches!(load(&scratch.dir), Err(ConfigError::Io { .. })));
    }

    #[test]
    fn a_blocked_parent_or_lock_is_a_local_failure() {
        let scratch = Scratch::new();
        let blocked = scratch.dir.join("not-a-directory");
        std::fs::write(&blocked, "keep").unwrap();
        assert!(matches!(
            Locked::acquire(&blocked.join("config.json")),
            Err(ConfigError::Io { .. })
        ));
        let path = scratch.config();
        std::fs::create_dir_all(super::sibling(&path, "lock")).unwrap();
        assert!(matches!(
            Locked::acquire(&path),
            Err(ConfigError::Io { .. })
        ));
        assert_eq!(std::fs::read_to_string(blocked).unwrap(), "keep");
    }

    #[test]
    fn a_failed_atomic_save_keeps_the_previous_configuration() {
        let scratch = Scratch::new();
        let path = scratch.config();
        update(&path, |config| {
            config
                .profiles
                .entry("default".to_owned())
                .or_default()
                .api_key = Some("nb_test_original".to_owned());
        })
        .unwrap();
        let mut locked = Locked::acquire(&path).unwrap();
        locked.config.profiles.clear();
        let temporary = super::sibling(&path, "tmp");
        std::fs::create_dir(&temporary).unwrap();
        assert!(matches!(locked.save(), Err(ConfigError::Io { .. })));
        assert_eq!(
            load(&path).unwrap().profiles["default"].api_key.as_deref(),
            Some("nb_test_original")
        );
        std::fs::remove_dir(&temporary).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(locked.save(), Err(ConfigError::Io { .. })));
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_crash_left_temporary_file_is_made_private_before_reuse() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new();
        let path = scratch.config();
        let locked = Locked::acquire(&path).unwrap();
        let temporary = super::sibling(&path, "tmp");
        std::fs::write(&temporary, "partial").unwrap();
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644)).unwrap();
        locked.save().unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(load(&path).unwrap().profiles.is_empty());
        assert!(!temporary.exists());
    }
}
