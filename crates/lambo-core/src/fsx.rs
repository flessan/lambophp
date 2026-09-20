//! Small filesystem helpers with the same behaviour on every platform.
//!
//! Two rules matter here:
//!
//! 1. **Writes of configuration and state are atomic.** A crash or a
//!    Ctrl+C during `lambo config set` must never leave a truncated
//!    `lambo.yml` behind, because that file is the only record of what the
//!    user asked for.
//! 2. **Permissions follow the platform.** Unix gets mode `0600` for files
//!    that hold credentials; Windows inherits the ACL of the user profile
//!    directory, which is the platform's own answer to the same problem.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// How many names [`create_temp`] tries before giving up.
///
/// A collision means another process created the exact same name in the same
/// microsecond; one retry would be enough in practice, and sixteen is enough
/// that the failure can only mean the directory is not writable.
const TEMP_ATTEMPTS: usize = 16;

/// Writes `contents` to `path` atomically, creating parent directories.
///
/// The data lands in a sibling temporary file first and is then renamed over
/// the destination; `fs::rename` replaces an existing file on Windows as
/// well as on Unix, so the operation is atomic on both.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    write_atomic_bytes(path, contents.as_bytes())
}

/// Byte-level variant of [`write_atomic`].
pub fn write_atomic_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
    }

    // A per-process unique temporary name so two concurrent writers cannot
    // clobber each other's temporary file.
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "lambo".to_owned());
    let temporary = path.with_file_name(format!(
        "{file_name}.{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));

    fs::write(&temporary, contents).map_err(|source| Error::io(&temporary, source))?;
    restrict_to_owner(&temporary);

    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(source) => {
            let _ = fs::remove_file(&temporary);
            Err(Error::io(path, source))
        }
    }
}

/// Restricts a file to its owner on Unix; a no-op elsewhere.
///
/// Called for every file that can contain a database password.
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = fs::set_permissions(path, permissions);
    }
}

/// Restricts a file to its owner on Unix; a no-op elsewhere.
///
/// On Windows the file inherits the ACL of the Lambo home directory, which
/// lives in the user's profile and is already user-private.
#[cfg(not(unix))]
pub fn restrict_to_owner(_path: &Path) {}

/// Creates a directory (and parents) unless it already exists.
pub fn ensure_dir(path: &Path) -> Result<()> {
    match fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) => Err(Error::io(path, source)),
    }
}

/// Removes a directory tree, treating "already gone" as success.
///
/// Windows keeps files open briefly after a process exits, so callers that
/// clean up after `lambo down` must tolerate a failure here rather than
/// reporting a stop as failed.
pub fn remove_dir_all_if_exists(path: &Path) -> Result<bool> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::io(path, source)),
    }
}

/// Creates a uniquely named temporary file in `dir`.
///
/// The name starts with `prefix` and ends in `.tmp`, so these files are
/// recognisable as Lambo's own scratch space rather than as documents. The
/// returned file is open and empty; removing it is the caller's job, because
/// the callers that matter rename it into place instead (that is what makes a
/// configuration write atomic), and a guard that must not run its cleanup is
/// not a guard.
pub fn create_temp(dir: &Path, prefix: &str) -> Result<(PathBuf, File)> {
    ensure_dir(dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);

    for attempt in 0..TEMP_ATTEMPTS {
        let name = format!("{prefix}{}-{nanos}-{attempt}.tmp", std::process::id());
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(source) if source.kind() == ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(Error::io(&path, source)),
        }
    }

    Err(Error::io(
        dir,
        std::io::Error::other("could not create a unique temporary file"),
    ))
}

/// Copies a file, creating the destination's parent directory.
///
/// A truncating copy, which is what an installer needs: the destination is a
/// file the previous attempt may have left behind, and leaving a stale suffix
/// of it in place would corrupt a smaller replacement.
pub fn copy_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        ensure_dir(parent)?;
    }
    let bytes = fs::read(from).map_err(|source| Error::io(from, source))?;
    fs::write(to, bytes).map_err(|source| Error::io(to, source))
}

/// Copies a file only when the destination differs in size or timestamp.
///
/// The check is the one the previous implementation used, and it is what makes
/// the self-heal paths cheap: mirroring three runtime DLLs into every PHP
/// installation on every launch would otherwise rewrite hundreds of megabytes.
///
/// The destination's timestamp is set from the source's afterwards, so a
/// second run recognises its own work and skips it.
pub fn copy_file_if_changed(from: &Path, to: &Path) -> Result<bool> {
    let source = fs::metadata(from).map_err(|error| Error::io(from, error))?;
    if let Ok(existing) = fs::metadata(to) {
        let same_size = existing.len() == source.len();
        let same_time = match (existing.modified(), source.modified()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        };
        if same_size && same_time {
            return Ok(false);
        }
    }

    if let Some(parent) = to.parent() {
        ensure_dir(parent)?;
    }
    let bytes = fs::read(from).map_err(|error| Error::io(from, error))?;
    fs::write(to, bytes).map_err(|error| Error::io(to, error))?;

    // Best effort, and deliberately not an error: a filesystem that cannot
    // store the timestamp simply means the next run copies the file again.
    if let Ok(modified) = source.modified() {
        if let Ok(file) = File::options().write(true).open(to) {
            let _ = file.set_modified(modified);
        }
    }
    Ok(true)
}

/// Reads a UTF-8 text file, mapping "missing" to `None`.
pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io(path, source)),
    }
}

/// Whether a path can be written to, checked by actually touching it.
///
/// `metadata().permissions()` says very little on Windows (NTFS ACLs are not
/// expressible through it), so the only portable answer is to try. The probe
/// file is removed immediately afterwards.
/// Moves every entry of `from` into `to`, then removes the now-empty `from`.
///
/// Upstream archives are inconsistent: some wrap their contents in one
/// directory (`Apache24/`, `php-8.4.2/`), some do not. Installed runtimes are
/// always flattened to one shape, so this is on every install path. Renaming
/// entries rather than copying keeps an install of a 300 MB archive fast.
pub fn move_children(from: &Path, to: &Path) -> Result<()> {
    let entries: Vec<PathBuf> = fs::read_dir(from)
        .map_err(|source| Error::io(from, source))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    for entry in entries {
        let name = entry
            .file_name()
            .map(|name| name.to_owned())
            .ok_or_else(|| Error::io(&entry, std::io::Error::other("entry has no name")))?;
        fs::rename(&entry, to.join(name)).map_err(|source| Error::io(&entry, source))?;
    }
    fs::remove_dir(from).map_err(|source| Error::io(from, source))
}

pub fn is_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".lambo-write-probe-{}", std::process::id()));
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn atomic_write_creates_parents_and_content() {
        let temp = TempDir::new();
        let path = temp.path().join("config/lambo.yml");

        write_atomic(&path, "php:\n  default: 8.4\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "php:\n  default: 8.4\n");
        assert!(!path.with_file_name("lambo.yml.tmp").exists());

        // Overwriting keeps working, which is the case `fs::rename` has to
        // handle on Windows.
        write_atomic(&path, "php:\n  default: 8.3\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "php:\n  default: 8.3\n");
    }

    #[test]
    fn atomic_write_leaves_no_temporary_files_behind() {
        let temp = TempDir::new();
        let path = temp.path().join("state.yml");
        write_atomic(&path, "services: []\n").unwrap();

        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover temporary files: {leftovers:?}"
        );
    }

    #[test]
    fn read_optional_distinguishes_missing_from_empty() {
        let temp = TempDir::new();
        let path = temp.path().join("notes.txt");
        assert_eq!(read_optional(&path).unwrap(), None);
        fs::write(&path, "").unwrap();
        assert_eq!(read_optional(&path).unwrap().as_deref(), Some(""));
    }

    #[test]
    fn writable_probes_clean_up_after_themselves() {
        let temp = TempDir::new();
        assert!(is_writable(temp.path()));
        assert!(!is_writable(&temp.path().join("missing")));

        let probes: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("write-probe"))
            .collect();
        assert!(probes.is_empty(), "probe file was not removed: {probes:?}");
    }

    #[test]
    fn removing_a_missing_directory_is_success() {
        let temp = TempDir::new();
        let path = temp.path().join("database/data");
        assert!(!remove_dir_all_if_exists(&path).unwrap());

        fs::create_dir_all(path.join("mysql")).unwrap();
        assert!(remove_dir_all_if_exists(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn flattening_an_archive_wrapper_moves_every_entry() {
        let temp = TempDir::new();
        let staging = temp.join("2.4.62.installing");
        let wrapper = staging.join("Apache24");
        fs::create_dir_all(wrapper.join("bin")).unwrap();
        fs::write(wrapper.join("bin/httpd.exe"), b"MZ").unwrap();
        fs::write(wrapper.join("LICENSE"), b"text").unwrap();

        move_children(&wrapper, &staging).unwrap();

        assert!(staging.join("bin/httpd.exe").is_file());
        assert!(staging.join("LICENSE").is_file());
        assert!(!wrapper.exists(), "the wrapper directory must be gone");
    }
}
