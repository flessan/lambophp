//! Safe extraction of `.zip` and `.tar.gz` archives.
//!
//! Lambo unpacks PHP, Apache, MariaDB and Adminer archives, so this module is
//! on the critical path of "install a runtime". Two things have to be true:
//!
//! 1. **It works on Windows without help.** PHP for Windows ships as a `.zip`,
//!    and requiring the user to have `tar`, 7-Zip or PowerShell available
//!    would put a hole in the "just run `lambo php install`" story. Extraction
//!    is therefore implemented on top of [`flate2`] only.
//! 2. **It cannot be tricked into writing outside the destination.** A
//!    malicious or merely broken archive with entries like `../../evil.dll` or
//!    `C:\Windows\System32\x` is rejected *before* anything is written, and
//!    symlinks are never created. See [`safe_join`].
//!
//! Both formats are read from the archive itself: ZIP through its central
//! directory (so a truncated download is detected rather than half-extracted),
//! tar through its 512-byte headers, including the GNU long-name extension.

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use crc32fast::Hasher as Crc32;
use flate2::read::{DeflateDecoder, GzDecoder};

use crate::error::{Error, Result};

/// Upper bound on the total uncompressed size of one archive (8 GiB).
///
/// A runtime archive never comes close; the cap exists so a hostile or
/// corrupt archive cannot fill a user's disk.
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Upper bound on the number of entries in one archive.
const MAX_ENTRIES: usize = 200_000;

/// How far from the end of a file to look for the ZIP end-of-central-directory
/// record (the record is 22 bytes plus up to 64 KiB of comment).
const EOCD_SCAN: u64 = 66_000;

/// The kind of archive, decided by extension and confirmed by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A ZIP archive (`.zip`) - PHP and MariaDB on Windows.
    Zip,
    /// A gzip-compressed tar (`.tar.gz`, `.tgz`) - Apache and MariaDB on Unix.
    TarGz,
}

impl Kind {
    /// Guesses the kind from a file name.
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".zip") {
            Some(Self::Zip)
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            Some(Self::TarGz)
        } else {
            None
        }
    }
}

/// What an extraction produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extraction {
    /// Number of files written.
    pub files: usize,
    /// Number of directories created.
    pub directories: usize,
    /// Top-level entries, which is what runtime discovery needs: archives
    /// differ in whether they wrap everything in one directory.
    pub top_level: Vec<String>,
    /// Total uncompressed bytes written.
    pub bytes: u64,
}

impl Extraction {
    /// The single top-level directory, when the archive has exactly one.
    ///
    /// `php-8.4.2-Win32-vs17-x64.zip` unpacks to one directory; `lambo php
    /// install` flattens it so the layout inside `$LAMBO_HOME/php/8.4.2` is
    /// predictable no matter how upstream packaged it.
    pub fn single_top_level_dir(&self) -> Option<&str> {
        if self.top_level.len() == 1 && self.directories > 0 {
            Some(&self.top_level[0])
        } else {
            None
        }
    }
}

/// Extracts an archive into `destination`, creating the directory.
///
/// The kind is inferred from the file name; use [`extract_kind`] when the
/// name is not trustworthy (a cached download, for instance).
pub fn extract(archive: &Path, destination: &Path) -> Result<Extraction> {
    let kind = Kind::from_path(archive).ok_or_else(|| Error::Archive {
        path: archive.to_path_buf(),
        reason: "unsupported archive type (expected .zip, .tar.gz or .tgz)".to_owned(),
    })?;
    extract_kind(archive, destination, kind)
}

/// Extracts an archive of a known kind.
pub fn extract_kind(archive: &Path, destination: &Path, kind: Kind) -> Result<Extraction> {
    fs::create_dir_all(destination).map_err(|source| Error::io(destination, source))?;
    match kind {
        Kind::Zip => extract_zip(archive, destination),
        Kind::TarGz => extract_tar_gz(archive, destination),
    }
}

/// Joins an archive entry onto the destination, refusing to escape it.
///
/// Rejects:
/// - absolute paths and drive letters (`/etc/passwd`, `C:\Windows\x`),
/// - `..` components,
/// - empty names and names made only of separators,
/// - UNC paths (`\\server\share`).
///
/// The result is additionally checked to still live under `destination` after
/// normalization, which catches anything the component scan could miss.
pub fn safe_join(destination: &Path, entry: &str) -> Result<PathBuf> {
    let normalized = entry.replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return Err(unsafe_entry(destination, entry));
    }
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(unsafe_entry(destination, entry));
    }
    // A NUL byte terminates a path at the syscall boundary, so the path that
    // was validated and the path the kernel actually opens would not be the
    // same file. Rust's own path handling would reject it later, but by then
    // the decision to write has already been made.
    if entry.contains('\0') {
        return Err(unsafe_entry(destination, entry));
    }

    let mut joined = destination.to_path_buf();
    for component in trimmed.split('/') {
        match component {
            "" | "." => continue,
            ".." => return Err(unsafe_entry(destination, entry)),
            part => joined.push(part),
        }
    }

    // Belt and braces: every component of the result that lies below the
    // destination must be a normal component.
    let relative = joined
        .strip_prefix(destination)
        .map_err(|_| unsafe_entry(destination, entry))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_entry(destination, entry));
    }
    Ok(joined)
}

/// Builds an [`Error::UnsafeArchiveEntry`].
fn unsafe_entry(archive: &Path, entry: &str) -> Error {
    Error::UnsafeArchiveEntry {
        archive: archive.to_path_buf(),
        entry: entry.to_owned(),
    }
}

/// Records the top-level entry an archive entry belongs to.
///
/// Takes the *entry name* rather than the joined path: the joined path starts
/// at the destination, whose own components are not part of the archive.
fn record_top_level(extraction: &mut Extraction, entry: &str) {
    let normalized = entry.replace('\\', "/");
    let Some(name) = normalized.trim_matches('/').split('/').next() else {
        return;
    };
    if name.is_empty() {
        return;
    }
    let name = name.to_owned();
    if !extraction.top_level.contains(&name) {
        extraction.top_level.push(name);
    }
}

// ---------------------------------------------------------------------------
// ZIP
// ---------------------------------------------------------------------------

/// A ZIP central-directory entry.
#[derive(Debug, Clone)]
struct ZipEntry {
    name: String,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
    external_attributes: u32,
}

/// Extracts a ZIP archive.
fn extract_zip(archive: &Path, destination: &Path) -> Result<Extraction> {
    let file = File::open(archive).map_err(|source| Error::io(archive, source))?;
    let mut reader = BufReader::new(file);
    let entries = read_central_directory(&mut reader, archive)?;

    let mut extraction = Extraction::default();
    let mut total: u64 = 0;
    if entries.len() > MAX_ENTRIES {
        return Err(archive_error(
            archive,
            &format!("more than {MAX_ENTRIES} entries"),
        ));
    }

    for entry in entries {
        let relative = safe_join(destination, &entry.name)?;
        record_top_level(&mut extraction, &entry.name);

        if entry.name.ends_with('/') || is_zip_directory(&entry) {
            fs::create_dir_all(&relative).map_err(|source| Error::io(&relative, source))?;
            extraction.directories += 1;
            continue;
        }
        // Symlinks are skipped rather than created: a link that points
        // outside the destination would defeat `safe_join`.
        if is_zip_symlink(&entry) {
            continue;
        }

        let written = write_zip_entry(&mut reader, archive, &entry, &relative)?;
        total = total.saturating_add(written);
        if total > MAX_TOTAL_BYTES {
            return Err(archive_error(
                archive,
                "archive is larger than the accepted limit",
            ));
        }
        extraction.files += 1;
        extraction.bytes = total;
    }

    extraction.top_level.sort();
    Ok(extraction)
}

/// Whether the external attributes mark a directory (Unix mode in the high
/// half, MS-DOS attribute byte in the low half).
fn is_zip_directory(entry: &ZipEntry) -> bool {
    let unix_mode = entry.external_attributes >> 16;
    (unix_mode & 0o17_0000) == 0o04_0000 || (entry.external_attributes & 0x10) != 0
}

/// Whether the external attributes mark a symbolic link.
fn is_zip_symlink(entry: &ZipEntry) -> bool {
    (entry.external_attributes >> 16) & 0o17_0000 == 0o12_0000
}

/// Reads the end-of-central-directory record and the entries it points at.
fn read_central_directory<R: Read + Seek>(reader: &mut R, archive: &Path) -> Result<Vec<ZipEntry>> {
    let size = reader
        .seek(SeekFrom::End(0))
        .map_err(|source| archive_io(archive, source))?;
    if size < 22 {
        return Err(archive_error(
            archive,
            "file is too small to be a ZIP archive",
        ));
    }

    let scan = size.min(EOCD_SCAN);
    let mut tail = vec![0u8; scan as usize];
    reader
        .seek(SeekFrom::End(-(scan as i64)))
        .map_err(|source| archive_io(archive, source))?;
    reader
        .read_exact(&mut tail)
        .map_err(|source| archive_io(archive, source))?;

    let offset_in_tail = tail
        .windows(4)
        .rposition(|window| window == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| archive_error(archive, "no ZIP end-of-central-directory record found"))?;
    let eocd = &tail[offset_in_tail..];
    if eocd.len() < 22 {
        return Err(archive_error(
            archive,
            "truncated end-of-central-directory record",
        ));
    }

    let entries = u16::from_le_bytes([eocd[10], eocd[11]]) as usize;
    let directory_size = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]);
    let directory_offset = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]);
    if directory_offset == u32::MAX || directory_size == u32::MAX {
        return Err(archive_error(archive, "ZIP64 archives are not supported"));
    }

    reader
        .seek(SeekFrom::Start(directory_offset as u64))
        .map_err(|source| archive_io(archive, source))?;
    let mut directory = vec![0u8; directory_size as usize];
    reader
        .read_exact(&mut directory)
        .map_err(|source| archive_io(archive, source))?;

    parse_central_directory(&directory, entries, archive)
}

/// Parses the raw central directory.
///
/// `expected` is the entry count from the end-of-central-directory record:
/// when fewer entries are found the archive is truncated and the download is
/// rejected instead of half-extracted.
fn parse_central_directory(
    directory: &[u8],
    expected: usize,
    archive: &Path,
) -> Result<Vec<ZipEntry>> {
    let mut entries = Vec::new();
    let mut cursor = 0usize;

    while cursor + 46 <= directory.len() {
        if directory[cursor..cursor + 4] != [0x50, 0x4b, 0x01, 0x02] {
            break;
        }
        let method = u16::from_le_bytes([directory[cursor + 10], directory[cursor + 11]]);
        let crc32 = u32::from_le_bytes([
            directory[cursor + 16],
            directory[cursor + 17],
            directory[cursor + 18],
            directory[cursor + 19],
        ]);
        let compressed_size = u32::from_le_bytes([
            directory[cursor + 20],
            directory[cursor + 21],
            directory[cursor + 22],
            directory[cursor + 23],
        ]);
        let uncompressed_size = u32::from_le_bytes([
            directory[cursor + 24],
            directory[cursor + 25],
            directory[cursor + 26],
            directory[cursor + 27],
        ]);
        let name_length =
            u16::from_le_bytes([directory[cursor + 28], directory[cursor + 29]]) as usize;
        let extra_length =
            u16::from_le_bytes([directory[cursor + 30], directory[cursor + 31]]) as usize;
        let comment_length =
            u16::from_le_bytes([directory[cursor + 32], directory[cursor + 33]]) as usize;
        let external_attributes = u32::from_le_bytes([
            directory[cursor + 38],
            directory[cursor + 39],
            directory[cursor + 40],
            directory[cursor + 41],
        ]);
        let local_header_offset = u32::from_le_bytes([
            directory[cursor + 42],
            directory[cursor + 43],
            directory[cursor + 44],
            directory[cursor + 45],
        ]);

        let name_start = cursor + 46;
        let name_end = name_start
            .checked_add(name_length)
            .filter(|end| *end <= directory.len())
            .ok_or_else(|| archive_error(archive, "central directory entry is truncated"))?;
        let name = String::from_utf8_lossy(&directory[name_start..name_end]).into_owned();

        if compressed_size == u32::MAX || uncompressed_size == u32::MAX {
            return Err(archive_error(archive, "ZIP64 archives are not supported"));
        }

        entries.push(ZipEntry {
            name,
            method,
            crc32,
            compressed_size: compressed_size as u64,
            uncompressed_size: uncompressed_size as u64,
            local_header_offset: local_header_offset as u64,
            external_attributes,
        });

        cursor = name_end
            .checked_add(extra_length + comment_length)
            .ok_or_else(|| archive_error(archive, "central directory is corrupt"))?;
    }

    if entries.is_empty() {
        return Err(archive_error(archive, "the archive contains no entries"));
    }
    if entries.len() != expected {
        return Err(archive_error(
            archive,
            &format!(
                "the archive declares {expected} entries but {} could be read; download it again",
                entries.len()
            ),
        ));
    }
    Ok(entries)
}

/// Writes one ZIP entry, verifying its CRC-32.
fn write_zip_entry<R: Read + Seek>(
    reader: &mut R,
    archive: &Path,
    entry: &ZipEntry,
    destination: &Path,
) -> Result<u64> {
    reader
        .seek(SeekFrom::Start(entry.local_header_offset))
        .map_err(|source| archive_io(archive, source))?;

    let mut header = [0u8; 30];
    reader
        .read_exact(&mut header)
        .map_err(|source| archive_io(archive, source))?;
    if header[..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(archive_error(
            archive,
            &format!("bad local header for `{}`", entry.name),
        ));
    }
    // The local header repeats the name and extra-field lengths, and those -
    // not the central directory's - decide where the data starts.
    let name_length = u16::from_le_bytes([header[26], header[27]]) as u64;
    let extra_length = u16::from_le_bytes([header[28], header[29]]) as u64;
    reader
        .seek(SeekFrom::Current((name_length + extra_length) as i64))
        .map_err(|source| archive_io(archive, source))?;

    let mut compressed = Vec::with_capacity(entry.compressed_size.min(MAX_TOTAL_BYTES) as usize);
    reader
        .take(entry.compressed_size)
        .read_to_end(&mut compressed)
        .map_err(|source| archive_io(archive, source))?;

    let data = match entry.method {
        0 => compressed,
        8 => {
            let mut decoded = Vec::with_capacity(entry.uncompressed_size as usize);
            DeflateDecoder::new(&compressed[..])
                .read_to_end(&mut decoded)
                .map_err(|source| Error::Archive {
                    path: archive.to_path_buf(),
                    reason: format!("cannot inflate `{}`: {source}", entry.name),
                })?;
            decoded
        }
        other => {
            return Err(archive_error(
                archive,
                &format!(
                    "`{}` uses unsupported compression method {other}",
                    entry.name
                ),
            ));
        }
    };

    let mut hasher = Crc32::new();
    hasher.update(&data);
    if hasher.finalize() != entry.crc32 {
        return Err(archive_error(
            archive,
            &format!(
                "`{}` failed its CRC-32 check; the download is corrupt",
                entry.name
            ),
        ));
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
    }
    fs::write(destination, &data).map_err(|source| Error::io(destination, source))?;
    Ok(data.len() as u64)
}

// ---------------------------------------------------------------------------
// tar.gz
// ---------------------------------------------------------------------------

/// Size of one tar header block.
const TAR_BLOCK: usize = 512;

/// Extracts a gzip-compressed tar archive.
fn extract_tar_gz(archive: &Path, destination: &Path) -> Result<Extraction> {
    let file = File::open(archive).map_err(|source| Error::io(archive, source))?;
    let mut decoder = GzDecoder::new(BufReader::new(file));

    let mut extraction = Extraction::default();
    let mut total: u64 = 0;
    let mut long_name: Option<String> = None;
    let mut header = [0u8; TAR_BLOCK];

    loop {
        let read =
            read_full(&mut decoder, &mut header).map_err(|source| archive_io(archive, source))?;
        if read == 0 {
            break; // clean end of archive
        }
        if read < TAR_BLOCK {
            return Err(archive_error(
                archive,
                "archive ends in the middle of a header",
            ));
        }
        if header.iter().all(|byte| *byte == 0) {
            // Two consecutive zero blocks end a tar archive; a single one is
            // padding, which is simply skipped.
            continue;
        }

        let typeflag = header[156];
        let size = parse_octal(&header[124..136])
            .ok_or_else(|| archive_error(archive, "invalid size field in tar header"))?;

        // GNU long names: the next header describes the file whose name is the
        // content of this entry.
        if typeflag == b'L' {
            let mut name = vec![0u8; size as usize];
            decoder
                .read_exact(&mut name)
                .map_err(|source| archive_io(archive, source))?;
            skip_padding(&mut decoder, size).map_err(|source| archive_io(archive, source))?;
            long_name = Some(
                String::from_utf8_lossy(&name)
                    .trim_end_matches('\0')
                    .to_owned(),
            );
            continue;
        }

        let raw_name = String::from_utf8_lossy(&header[..100])
            .trim_end_matches('\0')
            .to_owned();
        let prefix = String::from_utf8_lossy(&header[345..500])
            .trim_end_matches('\0')
            .to_owned();
        let name = long_name.take().unwrap_or_else(|| {
            if prefix.is_empty() {
                raw_name.clone()
            } else {
                format!("{prefix}/{raw_name}")
            }
        });
        if name.is_empty() {
            skip_entry(&mut decoder, size).map_err(|source| archive_io(archive, source))?;
            continue;
        }

        let relative = safe_join(destination, &name)?;
        record_top_level(&mut extraction, &name);

        match typeflag {
            b'5' => {
                fs::create_dir_all(&relative).map_err(|source| Error::io(&relative, source))?;
                extraction.directories += 1;
            }
            b'0' | 0 => {
                let mut data = vec![0u8; size as usize];
                decoder
                    .read_exact(&mut data)
                    .map_err(|source| archive_io(archive, source))?;
                skip_padding(&mut decoder, size).map_err(|source| archive_io(archive, source))?;

                if let Some(parent) = relative.parent() {
                    fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
                }
                fs::write(&relative, &data).map_err(|source| Error::io(&relative, source))?;
                set_executable_bit(&relative, &header);

                total = total.saturating_add(size);
                if total > MAX_TOTAL_BYTES {
                    return Err(archive_error(
                        archive,
                        "archive is larger than the accepted limit",
                    ));
                }
                extraction.files += 1;
                extraction.bytes = total;
            }
            // Symlinks, hard links, devices and FIFOs are skipped: a runtime
            // archive never needs them, and creating them would open a path
            // out of the destination directory.
            _ => skip_entry(&mut decoder, size).map_err(|source| archive_io(archive, source))?,
        }
    }

    extraction.top_level.sort();
    Ok(extraction)
}

/// Reads as much as possible, returning how many bytes were read.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(source),
        }
    }
    Ok(filled)
}

/// Skips the padding that follows a tar entry whose size is not a multiple of
/// the block size.
fn skip_padding<R: Read>(reader: &mut R, size: u64) -> std::io::Result<()> {
    let remainder = size % TAR_BLOCK as u64;
    if remainder == 0 {
        return Ok(());
    }
    let padding = TAR_BLOCK as u64 - remainder;
    let mut discard = vec![0u8; padding as usize];
    reader.read_exact(&mut discard)
}

/// Skips a tar entry that is not being extracted.
fn skip_entry<R: Read>(reader: &mut R, size: u64) -> std::io::Result<()> {
    let mut remaining = size;
    let mut buffer = [0u8; 8192];
    while remaining > 0 {
        let chunk = remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..chunk])?;
        remaining -= chunk as u64;
    }
    skip_padding(reader, size)
}

/// Parses an octal field of a tar header.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(field);
    let text = text.trim_matches(|c| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

/// Preserves the executable bit on Unix, where it matters.
///
/// PHP and MariaDB archives rely on it: a `php` binary that lost its
/// executable bit looks installed but cannot run.
#[cfg(unix)]
fn set_executable_bit(path: &Path, header: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = parse_octal(&header[100..108]) else {
        return;
    };
    if mode & 0o111 == 0 {
        return;
    }
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode((permissions.mode() & 0o7777) | (mode as u32 & 0o777));
        let _ = fs::set_permissions(path, permissions);
    }
}

/// Preserves the executable bit on Unix, where it matters.
#[cfg(not(unix))]
fn set_executable_bit(_path: &Path, _header: &[u8]) {}

/// Builds an [`Error::Archive`].
fn archive_error(path: &Path, reason: &str) -> Error {
    Error::Archive {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}

/// Wraps an I/O error with archive context.
fn archive_io(path: &Path, source: std::io::Error) -> Error {
    match source.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            archive_error(path, "the archive is truncated; download it again")
        }
        _ => Error::io(path, source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self, TempDir};

    #[test]
    fn kind_is_recognised_from_the_file_name() {
        assert_eq!(
            Kind::from_path(Path::new("php-8.4.2-Win32-vs17-x64.zip")),
            Some(Kind::Zip)
        );
        assert_eq!(
            Kind::from_path(Path::new("httpd-2.4.62.tar.gz")),
            Some(Kind::TarGz)
        );
        assert_eq!(Kind::from_path(Path::new("mariadb.tgz")), Some(Kind::TarGz));
        assert_eq!(Kind::from_path(Path::new("httpd-2.4.62.tar.bz2")), None);
        assert_eq!(Kind::from_path(Path::new("archive")), None);
    }

    #[test]
    fn zip_archives_are_extracted() {
        let temp = TempDir::new();
        let archive = temp.path().join("php.zip");
        testutil::write_zip(
            &archive,
            &[
                ("php-8.4.2/", None),
                ("php-8.4.2/php.exe", Some(b"MZ fake executable".as_slice())),
                ("php-8.4.2/ext/php_openssl.dll", Some(b"DLL".as_slice())),
            ],
        );

        let destination = temp.path().join("out");
        let extraction = extract(&archive, &destination).unwrap();

        assert_eq!(extraction.files, 2);
        assert_eq!(extraction.directories, 1);
        assert_eq!(extraction.top_level, vec!["php-8.4.2".to_owned()]);
        assert_eq!(extraction.single_top_level_dir(), Some("php-8.4.2"));
        assert_eq!(
            fs::read(destination.join("php-8.4.2/php.exe")).unwrap(),
            b"MZ fake executable"
        );
        assert!(destination.join("php-8.4.2/ext/php_openssl.dll").is_file());
    }

    #[test]
    fn zip_entries_outside_the_destination_are_refused() {
        let temp = TempDir::new();
        for entry in [
            "../evil.dll",
            "/etc/passwd",
            r"C:\Windows\System32\evil.dll",
            "..",
        ] {
            let archive = temp.path().join("evil.zip");
            testutil::write_zip(&archive, &[(entry, Some(b"payload".as_slice()))]);

            let destination = temp.path().join("out");
            let error = extract(&archive, &destination).unwrap_err();
            assert!(
                matches!(error, Error::UnsafeArchiveEntry { .. }),
                "`{entry}` must be refused, got {error:?}"
            );
            assert!(!temp.path().join("evil.dll").exists());
        }
    }

    #[test]
    fn zip_entries_with_a_bad_checksum_are_refused() {
        let temp = TempDir::new();
        let archive = temp.path().join("corrupt.zip");
        testutil::write_zip(&archive, &[("php.exe", Some(b"content".as_slice()))]);
        // Flip a byte inside the stored payload: the CRC no longer matches.
        let mut bytes = fs::read(&archive).unwrap();
        let position = bytes
            .windows(7)
            .position(|window| window == b"content")
            .expect("payload must be present in the archive");
        bytes[position] = b'X';
        fs::write(&archive, &bytes).unwrap();

        let error = extract(&archive, &temp.path().join("out")).unwrap_err();
        assert!(error.to_string().contains("CRC-32"), "{error}");
    }

    #[test]
    fn tar_gz_archives_are_extracted() {
        let temp = TempDir::new();
        let archive = temp.path().join("httpd-2.4.62.tar.gz");
        testutil::write_tar_gz(
            &archive,
            &[
                ("httpd-2.4.62/", None, 0o755),
                (
                    "httpd-2.4.62/bin/httpd",
                    Some(b"#!/bin/sh\n".as_slice()),
                    0o755,
                ),
                (
                    "httpd-2.4.62/conf/httpd.conf",
                    Some(b"ServerRoot .".as_slice()),
                    0o644,
                ),
            ],
        );

        let destination = temp.path().join("out");
        let extraction = extract(&archive, &destination).unwrap();

        assert_eq!(extraction.files, 2);
        assert_eq!(extraction.directories, 1);
        assert_eq!(extraction.top_level, vec!["httpd-2.4.62".to_owned()]);
        assert_eq!(
            fs::read_to_string(destination.join("httpd-2.4.62/conf/httpd.conf")).unwrap(),
            "ServerRoot ."
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(destination.join("httpd-2.4.62/bin/httpd"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(
                mode & 0o111,
                0,
                "the executable bit must survive extraction"
            );
        }
    }

    #[test]
    fn tar_entries_outside_the_destination_are_refused() {
        let temp = TempDir::new();
        let archive = temp.path().join("evil.tar.gz");
        testutil::write_tar_gz(&archive, &[("../evil", Some(b"payload".as_slice()), 0o644)]);

        let error = extract(&archive, &temp.path().join("out")).unwrap_err();
        assert!(
            matches!(error, Error::UnsafeArchiveEntry { .. }),
            "{error:?}"
        );
        assert!(!temp.path().join("evil").exists());
    }

    #[test]
    fn truncated_archives_are_reported_not_half_extracted() {
        let temp = TempDir::new();
        let archive = temp.path().join("truncated.zip");
        testutil::write_zip(&archive, &[("a.txt", Some(b"aaaa".as_slice()))]);
        let bytes = fs::read(&archive).unwrap();
        fs::write(&archive, &bytes[..bytes.len() / 2]).unwrap();

        let error = extract(&archive, &temp.path().join("out")).unwrap_err();
        assert!(matches!(error, Error::Archive { .. }), "{error:?}");
    }

    #[test]
    fn a_zip_symlink_entry_is_skipped_rather_than_created() {
        // A symlink whose target points outside the destination would defeat
        // `safe_join`: the entry name looks innocent and only the link target
        // is malicious. The extractor must not create the link at all.
        let temp = TempDir::new();
        let archive = temp.join("link.zip");
        let destination = temp.join("out");
        fs::create_dir_all(&destination).unwrap();

        testutil::write_zip_symlink(&archive, "innocent", "../../../etc/evil");
        let extraction = extract(&archive, &destination).unwrap();

        assert_eq!(
            extraction.files, 0,
            "a symlink is not a file: {extraction:?}"
        );
        assert!(
            !destination.join("innocent").exists(),
            "the link must not exist"
        );
        assert!(
            destination.join("innocent").symlink_metadata().is_err(),
            "not even as a dangling link"
        );
        assert!(
            !temp.join("etc").join("evil").exists(),
            "nothing written outside"
        );
    }

    #[test]
    fn a_tar_symlink_entry_is_skipped_rather_than_created() {
        let temp = TempDir::new();
        let archive = temp.join("link.tar.gz");
        let destination = temp.join("out");
        fs::create_dir_all(&destination).unwrap();

        // mode 0o120777 is tar's symlink type; the payload is the link target.
        testutil::write_tar_gz(
            &archive,
            &[
                ("php/", None, 0o755),
                (
                    "php/escape",
                    Some(b"../../../../tmp/pwned".as_slice()),
                    0o12_0777,
                ),
                ("php/real.txt", Some(b"fine".as_slice()), 0o644),
            ],
        );
        let extraction = extract(&archive, &destination).unwrap();

        assert_eq!(extraction.files, 1, "only the regular file: {extraction:?}");
        assert!(destination.join("php/real.txt").is_file());
        assert!(
            destination.join("php/escape").symlink_metadata().is_err(),
            "the symlink must not have been created"
        );
        assert!(!Path::new("/tmp/pwned").exists());
    }

    #[test]
    fn windows_path_shapes_are_refused() {
        let destination = Path::new(r"C:\Lambo\php\8.4.2");
        for bad in [
            r"C:\Windows\evil.dll",
            r"c:\windows\evil.dll",
            r"\\server\share\evil.dll",
            r"..\..\..\Windows\evil.dll",
            r"D:x.dll",
        ] {
            assert!(
                safe_join(destination, bad).is_err(),
                "`{bad}` must be refused"
            );
        }
    }

    #[test]
    fn an_entry_name_with_a_nul_byte_is_refused() {
        // A NUL truncates a path when it reaches the operating system, so what
        // is checked and what is opened would not be the same file.
        let destination = Path::new("/tmp/out");
        assert!(safe_join(destination, "php\0.exe").is_err());
        assert!(safe_join(destination, "php/\0/evil").is_err());
    }

    #[test]
    fn safe_join_accepts_normal_entries_only() {
        let destination = Path::new(r"C:\Lambo\php\8.4.2");
        assert_eq!(
            safe_join(destination, "php.exe").unwrap(),
            destination.join("php.exe")
        );
        assert_eq!(
            safe_join(destination, "php-8.4.2/ext/php_curl.dll").unwrap(),
            destination
                .join("php-8.4.2")
                .join("ext")
                .join("php_curl.dll")
        );
        // Backslashes are normalized rather than trusted.
        assert!(safe_join(destination, r"..\..\evil.dll").is_err());

        for bad in [
            "",
            "/",
            "//",
            "..",
            "../x",
            "/etc/passwd",
            "C:/Windows/x",
            "./../y",
        ] {
            assert!(
                safe_join(destination, bad).is_err(),
                "`{bad}` must be refused"
            );
        }
    }
}
