//! Atomic publication of one file into docker-mailserver's configuration directory: a
//! temporary file beside it, written and synced, given the mode and owner of the file it
//! replaces (a new file gets `mode` and its directory's owner), renamed over it, and the
//! directory synced. A reader sees the old file or the new one, never a part.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

/// Writes `content` to `path` unless it already holds exactly that; returns whether it wrote.
///
/// # Errors
///
/// The file cannot be read, written, given its owner or renamed; the temporary file is removed.
pub fn publish(path: &Path, content: &[u8], mode: u32) -> io::Result<bool> {
    match fs::read(path) {
        Ok(existing) if existing == content => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("a published file needs a parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("a published file needs a name"))?;
    let (mode, uid, gid) = match fs::metadata(path) {
        Ok(meta) => (meta.permissions().mode() & 0o7777, meta.uid(), meta.gid()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let meta = fs::metadata(dir)?;
            (mode, meta.uid(), meta.gid())
        }
        Err(error) => return Err(error),
    };
    let temporary = dir.join(format!(".{}.norbelys-new", name.to_string_lossy()));
    let written = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        let meta = file.metadata()?;
        if meta.uid() != uid || meta.gid() != gid {
            std::os::unix::fs::chown(&temporary, Some(uid), Some(gid))?;
        }
        fs::set_permissions(&temporary, Permissions::from_mode(mode))?;
        fs::rename(&temporary, path)?;
        File::open(dir)?.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    /// A new file gets the given mode; unchanged content is not rewritten (so the change
    /// detector sees no change); a replaced file keeps its mode; no temporary file is left.
    #[test]
    fn publishes_atomically_and_only_on_change() {
        let dir = TempDir::new();
        let path = dir.join("map");
        assert!(publish(&path, b"one\n", 0o640).unwrap());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!publish(&path, b"one\n", 0o600).unwrap());
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(publish(&path, b"two\n", 0o600).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"two\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
