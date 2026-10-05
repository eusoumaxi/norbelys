//! `norbelys-smtp dms-patch`: a fix to docker-mailserver's `accounts.sh` helper (release
//! 16.0.1, <https://github.com/docker-mailserver/docker-mailserver>), applied inside the mail
//! container at every start, from its `user-patches.sh` hook, until upstream ships an
//! equivalent. Two defects:
//!
//! 1. `_create_accounts` truncates Dovecot's userdb and Postfix's `vmailbox` and refills them
//!    line by line, so during every rebuild (each account change) a concurrent login can read an
//!    empty or partial userdb and fail with "unknown user", and enough failures ban the client
//!    in Fail2Ban. The patch builds both maps in temporary files beside their targets and
//!    publishes each complete file with a rename.
//! 2. `_create_dovecot_alias_dummy_accounts` finds accounts with `grep` on the full address
//!    used as a regular expression, so a dot can match another account and give an alias that
//!    account's password hash. The patch compares the first `|`-separated field exactly, with
//!    `awk`.
//!
//! Safety: every fragment the patch rewrites must occur exactly once, or nothing is written and
//! the command fails, so an upstream change is noticed rather than mis-patched; the result must
//! pass `bash -n`; it replaces the file atomically and keeps its mode. A marker makes a second
//! run a no-op. The input's SHA-256 is logged with whether it equals the reviewed upstream
//! file's, the first thing to check when upgrading the image.

use std::path::Path;
use std::process::Command;
use std::{fs, io};

use crate::crypto;

/// The line the patch adds; its presence means the file is patched.
const MARKER: &str = "# Norbelys: publish complete account maps atomically.";
/// SHA-256 of `target/scripts/helpers/accounts.sh` at docker-mailserver v16.0.1.
const UPSTREAM_SHA256: &str = "dd703f54024f356df8b22d291579125606626ac51b8b7177cefd09f2d0ae9031";

/// The truncation at the top of `_create_accounts`, replaced by temporary maps.
const TRUNCATE: &str = "  : >/etc/postfix/vmailbox
  : >\"${DOVECOT_USERDB_FILE}\"

  [[ ${ACCOUNT_PROVISIONER} == 'FILE' ]] || return 0
";
const TEMPORARY_MAPS: &str = "  [[ ${ACCOUNT_PROVISIONER} == 'FILE' ]] || return 0
  # Norbelys: publish complete account maps atomically.
  local DOVECOT_USERDB_TARGET=\"${DOVECOT_USERDB_FILE}\"
  local DOVECOT_USERDB_FILE POSTFIX_VMAILBOX_FILE
  DOVECOT_USERDB_FILE=$(mktemp \"${DOVECOT_USERDB_TARGET}.XXXXXX\")
  POSTFIX_VMAILBOX_FILE=$(mktemp /etc/postfix/vmailbox.XXXXXX)
";
/// Where `_create_accounts` ends.
const BLOCK_START: &str = "function _create_accounts()";
const BLOCK_END: &str = "\n# Required when using Dovecot Quotas";

/// Rewrites inside `_create_accounts`: the writes go to the temporary maps, which are published
/// at the end (or removed when there is no accounts file).
const BLOCK: [(&str, &str); 4] = [
    ("> /etc/postfix/vmailbox", "> \"${POSTFIX_VMAILBOX_FILE}\""),
    (">>/etc/postfix/vmailbox", ">>\"${POSTFIX_VMAILBOX_FILE}\""),
    (
        "\"${POSTFIX_VMAILBOX_LINE}\" /etc/postfix/vmailbox;",
        "\"${POSTFIX_VMAILBOX_LINE}\" \"${POSTFIX_VMAILBOX_FILE}\";",
    ),
    (
        "    _create_dovecot_alias_dummy_accounts\n  fi\n}",
        "    _create_dovecot_alias_dummy_accounts
    chmod 644 \"${POSTFIX_VMAILBOX_FILE}\"
    mv -f \"${POSTFIX_VMAILBOX_FILE}\" /etc/postfix/vmailbox
    mv -f \"${DOVECOT_USERDB_FILE}\" \"${DOVECOT_USERDB_TARGET}\"
  else
    rm -f \"${POSTFIX_VMAILBOX_FILE}\" \"${DOVECOT_USERDB_FILE}\"
  fi
}",
    ),
];

/// Exact account lookups instead of regular-expression `grep`s.
const LOOKUPS: [(&str, &str); 3] = [
    (
        "grep -q \"${FQUN}\" \"${DATABASE_ACCOUNTS}\"",
        "awk -F '|' -v name=\"${FQUN}\" '$1 == name { found=1; exit } END { exit !found }' \"${DATABASE_ACCOUNTS}\"",
    ),
    (
        "grep -q \"${REAL_FQUN}\" \"${DATABASE_ACCOUNTS}\"",
        "awk -F '|' -v name=\"${REAL_FQUN}\" '$1 == name { found=1; exit } END { exit !found }' \"${DATABASE_ACCOUNTS}\"",
    ),
    (
        "grep \"${REAL_FQUN}\" \"${DATABASE_ACCOUNTS}\"",
        "awk -F '|' -v name=\"${REAL_FQUN}\" '$1 == name { print; exit }' \"${DATABASE_ACCOUNTS}\"",
    ),
];

/// Why the file was not patched.
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// An expected fragment is missing or repeated: the helper is not the reviewed one.
    #[error("unsupported accounts.sh ({0}); review the upstream helper before upgrading the image")]
    Unsupported(String),
    /// The patched script is not valid Bash.
    #[error("the patched accounts.sh fails `bash -n`")]
    Syntax,
    /// The file cannot be read, checked or replaced.
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Patches the helper at `path` in place; a patched helper is left as it is. Every run ends with
/// one `mta.dms_patch` event: patched, already patched, or failed.
///
/// # Errors
///
/// The helper is not the reviewed upstream one, the result is not valid Bash, or the file
/// cannot be replaced; the original stays untouched.
pub fn run(path: &Path) -> Result<(), PatchError> {
    let result = apply(path);
    if let Err(error) = &result {
        crate::telemetry::unit(crate::telemetry::Event::DmsPatch);
        tracing::error!(event = "mta.dms_patch", file = %path.display(), error = %error, outcome = "failed", "mta.dms_patch");
    }
    result
}

/// [`run`] without its failure's event.
fn apply(path: &Path) -> Result<(), PatchError> {
    let source = fs::read_to_string(path)?;
    let sha256 = crypto::sha256_hex(source.as_bytes());
    let upstream = sha256 == UPSTREAM_SHA256;
    if source.contains(MARKER) {
        crate::telemetry::unit(crate::telemetry::Event::DmsPatch);
        tracing::info!(event = "mta.dms_patch", file = %path.display(), %sha256, outcome = "already patched", "mta.dms_patch");
        return Ok(());
    }
    if !upstream {
        tracing::warn!(file = %path.display(), %sha256, "accounts.sh differs from the reviewed upstream file");
    }
    let patched = patch(&source)?;
    let temporary = path.with_extension("norbelys-new");
    let replaced = (|| -> Result<(), PatchError> {
        fs::write(&temporary, &patched)?;
        if !Command::new("bash")
            .arg("-n")
            .arg(&temporary)
            .status()?
            .success()
        {
            return Err(PatchError::Syntax);
        }
        fs::set_permissions(&temporary, fs::metadata(path)?.permissions())?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if let Err(error) = replaced {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    crate::telemetry::unit(crate::telemetry::Event::DmsPatch);
    tracing::info!(event = "mta.dms_patch", file = %path.display(), %sha256, upstream, outcome = "patched", "mta.dms_patch");
    Ok(())
}

fn replace_once(text: &str, old: &str, new: &str) -> Result<String, PatchError> {
    match text.matches(old).count() {
        1 => Ok(text.replacen(old, new, 1)),
        count => Err(PatchError::Unsupported(format!(
            "{count} occurrences of `{}`",
            old.lines().next().unwrap_or(old)
        ))),
    }
}

/// The patched text, or the first fragment that is not where the reviewed helper has it.
fn patch(source: &str) -> Result<String, PatchError> {
    let text = replace_once(source, TRUNCATE, TEMPORARY_MAPS)?;
    let start = text
        .find(BLOCK_START)
        .ok_or_else(|| PatchError::Unsupported(format!("no `{BLOCK_START}`")))?;
    let (head, rest) = text
        .split_at_checked(start)
        .ok_or_else(|| PatchError::Unsupported("an unexpected encoding".to_owned()))?;
    let end = rest
        .find(BLOCK_END)
        .ok_or_else(|| PatchError::Unsupported("no end of `_create_accounts`".to_owned()))?;
    let (block, tail) = rest
        .split_at_checked(end)
        .ok_or_else(|| PatchError::Unsupported("an unexpected encoding".to_owned()))?;
    let block = BLOCK
        .iter()
        .try_fold(block.to_owned(), |block, (old, new)| {
            replace_once(&block, old, new)
        })?;
    let text = format!("{head}{block}{tail}");
    LOOKUPS
        .iter()
        .try_fold(text, |text, (old, new)| replace_once(&text, old, new))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    /// docker-mailserver's own `accounts.sh` at v16.0.1 (MIT License, see
    /// `fixtures/docker-mailserver-LICENSE`), the file the patch was reviewed against.
    const UPSTREAM: &str = include_str!("../fixtures/docker-mailserver-v16.0.1-accounts.sh");

    /// The fixture is the reviewed upstream file, so the logged comparison is meaningful; the
    /// patch rewrites every fragment (temporary maps published by rename, exact `awk` lookups)
    /// and leaves no regular-expression lookup of an account behind.
    #[test]
    fn patches_the_reviewed_upstream_helper() {
        assert_eq!(crypto::sha256_hex(UPSTREAM.as_bytes()), UPSTREAM_SHA256);
        let patched = patch(UPSTREAM).unwrap();
        assert!(patched.contains(MARKER));
        assert!(patched.contains("mv -f \"${DOVECOT_USERDB_FILE}\" \"${DOVECOT_USERDB_TARGET}\""));
        assert_eq!(patched.matches("awk -F '|' -v name=").count(), 3);
        assert!(!patched.contains(": >/etc/postfix/vmailbox"));
        assert!(!patched.contains("grep -q \"${FQUN}\""));
    }

    /// Applied to a file it produces valid Bash and keeps the mode; a second run leaves the
    /// patched file alone; an unknown helper fails and stays untouched.
    #[test]
    fn patches_in_place_once_and_refuses_unknown_helpers() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new();
        let helper = dir.join("accounts.sh");
        fs::write(&helper, UPSTREAM).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        run(&helper).unwrap();
        let patched = fs::read_to_string(&helper).unwrap();
        assert!(patched.contains(MARKER));
        assert_eq!(
            fs::metadata(&helper).unwrap().permissions().mode() & 0o777,
            0o755
        );
        run(&helper).unwrap();
        assert_eq!(fs::read_to_string(&helper).unwrap(), patched);

        let other = dir.join("other.sh");
        fs::write(&other, "#!/bin/bash\necho other\n").unwrap();
        assert!(matches!(run(&other), Err(PatchError::Unsupported(_))));
        assert_eq!(
            fs::read_to_string(&other).unwrap(),
            "#!/bin/bash\necho other\n"
        );
    }
}
