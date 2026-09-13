//! Test utilities shared by the module unit tests.
//!
//! Only compiled under `cfg(test)`. Integration tests under `tests/` cannot
//! use this module (it is crate-internal); they carry their own tiny copy.
//!
//! The archive writers below are the reason the runtime manager can be tested
//! without downloading anything: they produce real `.zip` and `.tar.gz` files -
//! correct local headers, CRC-32 values, tar checksums - so extraction is
//! exercised against genuine inputs rather than mocks.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::paths::Paths;
use crate::platform::Os;
use crate::runtime::RuntimeKind;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary directory that deletes itself on drop.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a new, empty temporary directory.
    pub(crate) fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lambo-core-test-{}-{nanos}-{count}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create temporary test directory");
        Self { path }
    }

    /// Path of the temporary directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Joins a path onto the temporary directory.
    pub(crate) fn join(&self, child: impl AsRef<Path>) -> PathBuf {
        self.path.join(child)
    }

    /// A fresh Lambo home rooted inside this directory.
    pub(crate) fn home(&self) -> Paths {
        let paths = Paths::from_root(self.path.join("lambo-home"));
        paths
            .ensure_layout()
            .expect("failed to create the Lambo layout");
        paths
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes a file inside `dir`, creating parent directories.
pub(crate) fn fixture(dir: &Path, relative: &str, contents: &str) {
    let path = dir.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directory");
    }
    fs::write(&path, contents).expect("failed to write fixture");
}

/// Writes an executable file, with the executable bit set on Unix.
pub(crate) fn fixture_executable(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directory");
    }
    let body = if cfg!(windows) {
        b"MZ\x90\x00".to_vec()
    } else {
        b"#!/bin/sh\nexit 0\n".to_vec()
    };
    fs::write(path, body).expect("failed to write fixture executable");
    mark_executable(path);
}

/// Sets the executable bit on Unix; a no-op elsewhere.
#[cfg(unix)]
pub(crate) fn mark_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .expect("fixture must exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("failed to set the executable bit");
}

/// Sets the executable bit on Unix; a no-op elsewhere.
#[cfg(not(unix))]
pub(crate) fn mark_executable(_path: &Path) {}

/// Installs a fake runtime of `kind` and `version` into a Lambo home.
///
/// Produces the directory shape the real archives unpack to, including the
/// family's primary executable, so discovery, activation and service start-up
/// can all be tested on a machine with nothing installed.
pub(crate) fn install_fake_runtime(paths: &Paths, kind: RuntimeKind, version: &str, os: Os) {
    let root = paths.runtime_version_dir(kind, version);
    fs::create_dir_all(&root).expect("failed to create runtime directory");
    for name in kind.server_executables() {
        let Some(last) = name.rsplit('/').next() else {
            continue;
        };
        let relative = PathBuf::from(name.replace('/', std::path::MAIN_SEPARATOR_STR))
            .with_file_name(os.executable_name(last));
        fixture_executable(&root.join(relative));
    }
    if kind.is_database() {
        for name in kind
            .client_executables()
            .iter()
            .chain(kind.admin_executables())
            .chain(kind.init_executables())
        {
            let Some(last) = name.rsplit('/').next() else {
                continue;
            };
            let relative = PathBuf::from(name.replace('/', std::path::MAIN_SEPARATOR_STR))
                .with_file_name(os.executable_name(last));
            fixture_executable(&root.join(relative));
        }
    }
}

/// Writes a real (store-only) ZIP archive.
///
/// `entries` holds `(name, content)` pairs; a `None` content with a trailing
/// `/` in the name creates a directory entry.
pub(crate) fn write_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
    let mut body: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut count: u16 = 0;

    for (name, content) in entries {
        let data = content.unwrap_or(b"");
        let crc = crc32(data);
        let offset = body.len() as u32;
        let name_bytes = name.as_bytes();

        body.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        body.extend_from_slice(&20u16.to_le_bytes()); // version needed
        body.extend_from_slice(&0u16.to_le_bytes()); // flags
        body.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        body.extend_from_slice(&0u16.to_le_bytes()); // modification time
        body.extend_from_slice(&0x21u16.to_le_bytes()); // modification date
        body.extend_from_slice(&crc.to_le_bytes());
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // extra length
        body.extend_from_slice(name_bytes);
        body.extend_from_slice(data);

        let external = if name.ends_with('/') { 0x10u32 } else { 0u32 };
        central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0x21u16.to_le_bytes()); // date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&external.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);

        count += 1;
    }

    let central_offset = body.len() as u32;
    body.extend_from_slice(&central);
    body.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    body.extend_from_slice(&0u16.to_le_bytes()); // disk number
    body.extend_from_slice(&0u16.to_le_bytes()); // disk with the directory
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&(central.len() as u32).to_le_bytes());
    body.extend_from_slice(&central_offset.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // comment length

    fs::write(path, &body).expect("failed to write the ZIP fixture");
}

/// Writes a ZIP archive holding a single symlink entry.
///
/// The link target is the entry's content and the Unix symlink mode is carried
/// in the external attributes - exactly the shape an archive uses to try to
/// escape the destination, so the extractor's refusal can be tested against a
/// real entry rather than a hypothetical one.
pub(crate) fn write_zip_symlink(path: &Path, name: &str, target: &str) {
    let data = target.as_bytes();
    let crc = crc32(data);
    let name_bytes = name.as_bytes();
    let mut body: Vec<u8> = Vec::new();

    body.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
    body.extend_from_slice(&20u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0x21u16.to_le_bytes());
    body.extend_from_slice(&crc.to_le_bytes());
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(name_bytes);
    body.extend_from_slice(data);

    // Unix symlink: mode 0o120777 in the high 16 bits.
    let external = (0o12_0777u32) << 16;

    let mut central: Vec<u8> = Vec::new();
    central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
    central.extend_from_slice(&20u16.to_le_bytes());
    central.extend_from_slice(&20u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0x21u16.to_le_bytes());
    central.extend_from_slice(&crc.to_le_bytes());
    central.extend_from_slice(&(data.len() as u32).to_le_bytes());
    central.extend_from_slice(&(data.len() as u32).to_le_bytes());
    central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes()); // name length
    central.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    central.extend_from_slice(&0u16.to_le_bytes()); // file comment length
    central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
    central.extend_from_slice(&0u16.to_le_bytes()); // internal file attributes
    central.extend_from_slice(&external.to_le_bytes()); // external file attributes
    central.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    central.extend_from_slice(name_bytes);

    let central_offset = body.len() as u32;
    body.extend_from_slice(&central);
    body.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&(central.len() as u32).to_le_bytes());
    body.extend_from_slice(&central_offset.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());

    fs::write(path, &body).expect("failed to write the symlink ZIP fixture");
}

/// Writes a real gzip-compressed tar archive.
///
/// `entries` holds `(name, content, mode)` triples; a `None` content with a
/// trailing `/` creates a directory entry. Passing a mode with tar's symlink
/// type bit set (`0o120000`) emits a symlink entry whose content is the link
/// target - the shape a malicious archive uses to escape the destination, so
/// the extractor's refusal can be tested against a genuine entry.
pub(crate) fn write_tar_gz(path: &Path, entries: &[(&str, Option<&[u8]>, u32)]) {
    let mut tar: Vec<u8> = Vec::new();

    for (name, content, mode) in entries {
        let data = content.unwrap_or(b"");
        let directory = name.ends_with('/');
        let mut header = [0u8; 512];

        header[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
        write_octal(&mut header[100..108], *mode as u64);
        write_octal(&mut header[108..116], 0); // uid
        write_octal(&mut header[116..124], 0); // gid
        write_octal(&mut header[124..136], data.len() as u64);
        write_octal(&mut header[136..148], 0); // mtime
        header[148..156].copy_from_slice(b"        "); // checksum placeholder
        // tar's own type flag: directory, symlink, or regular file.
        header[156] = if directory {
            b'5'
        } else if *mode & 0o17_0000 == 0o12_0000 {
            b'2'
        } else {
            b'0'
        };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
        write_octal(&mut header[148..155], checksum as u64);
        header[155] = b' ';

        tar.extend_from_slice(&header);
        tar.extend_from_slice(data);
        let padding = (512 - (data.len() % 512)) % 512;
        tar.extend(std::iter::repeat_n(0u8, padding));
    }
    tar.extend(std::iter::repeat_n(0u8, 1024)); // two end-of-archive blocks

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&tar)
        .expect("failed to compress the tar fixture");
    let compressed = encoder.finish().expect("failed to finish compression");
    fs::write(path, compressed).expect("failed to write the tar.gz fixture");
}

/// Writes an octal field of a tar header: digits, NUL, space padding.
fn write_octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0>width$o}");
    field[..width].copy_from_slice(text.as_bytes());
    field[width] = 0;
}

/// CRC-32 of a byte slice, as stored in a ZIP archive.
fn crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}
