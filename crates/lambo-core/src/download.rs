//! Downloading and verifying runtime archives.
//!
//! Everything Lambo installs is *executed* afterwards, so this module is the
//! security boundary of the whole product. The rules are absolute:
//!
//! 1. **HTTPS only.** [`require_https`] rejects any other scheme, including
//!    redirects to one - `curl` is started with `--proto =https` so the
//!    transport enforces it too.
//! 2. **Verify before use.** A download is written to a `.part` file, hashed,
//!    and only then renamed into the cache. [`crate::archive::extract`] is
//!    never handed an unverified file.
//! 3. **Fail closed.** When no checksum is known - neither pinned in the
//!    catalogue nor published next to the archive - the download is rejected.
//!    A missing checksum is treated as a failed verification, not as a
//!    reason to skip it.
//! 4. **No shell.** The transport is invoked as an argument vector, so a URL
//!    can never be interpreted as shell syntax.
//!
//! The transport is pluggable ([`Downloader`]) because the production path
//! delegates TLS to the platform's `curl` - present on Windows 10 (1803+),
//! macOS and every mainstream Linux distribution, and verified by
//! `lambo doctor` - while tests use [`LocalDownloader`] to exercise the whole
//! verify-then-extract pipeline offline.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::platform::Os;
use crate::process::{self, ProcessSpec};
use crate::sha256;

/// Fetches a URL into a local file.
pub trait Downloader {
    /// Downloads `url` to `destination`, replacing any previous content.
    fn fetch(&self, url: &str, destination: &Path) -> Result<()>;
}

/// The production downloader: the platform's `curl`.
///
/// Delegation is deliberate. Implementing TLS in-process would mean shipping
/// a certificate store and a crypto stack; using the operating system's
/// `curl` reuses the trust store the user's other tools already rely on.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurlDownloader;

impl Downloader for CurlDownloader {
    fn fetch(&self, url: &str, destination: &Path) -> Result<()> {
        let spec = ProcessSpec::new(curl_program(), "download")
            .args([
                "--location", // follow redirects
                "--fail",     // non-2xx is an error
                "--silent",
                "--show-error",
                "--proto",
                "=https", // never downgrade to plain HTTP
                "--tlsv1.2",
                "--retry",
                "3",
                "--connect-timeout",
                "30",
                "--output",
            ])
            .arg(destination.display().to_string())
            .arg(url);

        let output = process::run(&spec, Os::host())?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Download {
                url: url.to_owned(),
                reason: format!(
                    "curl exited with {}: {}",
                    output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".to_owned()),
                    message.trim()
                ),
            });
        }
        Ok(())
    }
}

/// The production downloader: routes each URL to the transport that fits it.
///
/// `https://` goes to the platform's `curl`; `file://` is copied directly. Both
/// land in the same cache and are verified by the same code, so a local
/// artifact is held to exactly the standard a downloaded one is - there is no
/// local path that skips verification.
///
/// Composing the two transports rather than adding a third abstraction is
/// deliberate: there are only two, and each is already correct on its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDownloader;

impl Downloader for SystemDownloader {
    fn fetch(&self, url: &str, destination: &Path) -> Result<()> {
        if url.trim().starts_with("file://") {
            return LocalDownloader.fetch(url, destination);
        }
        CurlDownloader.fetch(url, destination)
    }
}

/// A downloader for tests: copies from a local file.
///
/// `file:///…` URLs and plain paths are both accepted so fixtures read
/// naturally.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalDownloader;

impl Downloader for LocalDownloader {
    fn fetch(&self, url: &str, destination: &Path) -> Result<()> {
        let source = url.strip_prefix("file://").unwrap_or(url);
        let source = source.trim_start_matches(|c| c == '/' && cfg!(windows));
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }
        fs::copy(source, destination).map_err(|error| Error::Download {
            url: url.to_owned(),
            reason: error.to_string(),
        })?;
        Ok(())
    }
}

/// The `curl` binary to use.
///
/// Windows 10 1803 and later ship `curl.exe` in `System32`; everywhere else
/// `curl` is resolved through `PATH`.
pub fn curl_program() -> &'static str {
    if cfg!(windows) { "curl.exe" } else { "curl" }
}

/// Whether the platform has a usable `curl`.
///
/// Reported by `lambo doctor`: without it, `lambo php install` cannot work and
/// the user needs to know that before they try.
pub fn transport_available() -> bool {
    let spec = ProcessSpec::new(curl_program(), "curl-version").arg("--version");
    process::run(&spec, Os::host())
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Verifies that a URL is safe to fetch.
///
/// Accepts `https://` for real downloads and `file://` for local fixtures
/// (tests and air-gapped mirrors); everything else is refused.
pub fn require_https(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed.starts_with("https://") {
        return Ok(());
    }
    if trimmed.starts_with("file://") {
        return Ok(());
    }
    Err(Error::NotHttps(url.to_owned()))
}

/// An archive to download, with everything needed to verify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Where to get it.
    pub url: String,
    /// Pinned SHA-256 digest, when the catalogue has one.
    pub sha256: Option<String>,
    /// URL of an upstream `.sha256` sidecar, consulted when [`Self::sha256`]
    /// is absent.
    pub checksum_url: Option<String>,
    /// Name to cache the artifact under, when the catalogue declares one.
    ///
    /// Derived from the URL otherwise, which is wrong for the two shapes real
    /// mirrors use: a redirect (`https://mirror/latest`) has no file name at
    /// all, and a query string (`…/download?file=php.zip`) yields `download`
    /// once the `?` is stripped.
    pub file_name_override: Option<String>,
    /// Size in bytes the artifact is supposed to be.
    ///
    /// Verification already rejects a truncated download, because a partial
    /// file hashes differently. This catches it earlier and with a better
    /// message: a transfer that stopped halfway is "expected 41 MB, got 3 MB",
    /// not a digest mismatch that reads like tampering.
    pub size: Option<u64>,
}

impl Artifact {
    /// Describes an artifact with a pinned checksum.
    pub fn pinned(url: impl Into<String>, sha256: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            sha256: Some(sha256.into()),
            checksum_url: None,
            file_name_override: None,
            size: None,
        }
    }

    /// Describes an artifact whose checksum is published next to it.
    pub fn with_sidecar(url: impl Into<String>) -> Self {
        let url = url.into();
        let checksum_url = format!("{url}.sha256");
        Self {
            url,
            sha256: None,
            checksum_url: Some(checksum_url),
            file_name_override: None,
            size: None,
        }
    }

    /// Records the size the artifact is supposed to be.
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// File name the artifact is cached under.
    pub fn file_name(&self) -> String {
        if let Some(declared) = &self.file_name_override {
            // Only the last component, and never a bare or relative name. This
            // becomes a path inside the cache directory, so a declared
            // `../../etc/passwd` must not be able to escape it.
            let leaf = declared.rsplit(['/', '\\']).next().unwrap_or("");
            if !leaf.is_empty() && leaf != "." && leaf != ".." {
                return leaf.to_owned();
            }
        }
        let name = self.url.rsplit('/').next().unwrap_or("download.bin");
        name.split('?').next().unwrap_or(name).to_owned()
    }
}

/// A verified artifact on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// Where the verified file lives in the cache.
    pub path: PathBuf,
    /// The digest the file was checked against.
    ///
    /// This is the digest that was *used*, which is not always the one the
    /// catalogue carried: when a release is unpinned, Lambo verifies against
    /// the `.sha256` published next to the artifact. An install manifest that
    /// recorded the catalogue field would write an empty string in that case
    /// and then claim, falsely, that nothing was verified.
    pub sha256: String,
}

/// Downloads an artifact into the cache and verifies it.
///
/// Returns the path of the verified file. The download lands in a `.part`
/// file first, so an interrupted or tampered download can never be mistaken
/// for a good one - and a previously verified cache entry is never
/// overwritten by a failed attempt.
///
/// The checksum is resolved **before** anything is downloaded: Lambo never
/// fetches a payload it could not verify, so an unpinned release fails without
/// touching the network for it.
pub fn download_verified(
    downloader: &dyn Downloader,
    artifact: &Artifact,
    paths: &Paths,
) -> Result<Verified> {
    require_https(&artifact.url)?;

    let expected =
        resolved_checksum(downloader, artifact)?.ok_or_else(|| Error::VerificationUnavailable {
            url: artifact.url.clone(),
            family: None,
            version: None,
            platform: None,
            hint: None,
        })?;

    let cache = paths.cache_dir();
    fs::create_dir_all(&cache).map_err(|source| Error::io(&cache, source))?;
    let file_name = artifact.file_name();
    let destination = cache.join(&file_name);

    // A cached file is only reused when it still matches its checksum.
    if destination.is_file() && sha256::verify_file(&destination, &expected).is_ok() {
        check_size(artifact, &destination)?;
        return Ok(Verified {
            path: destination,
            sha256: expected,
        });
    }

    // The download lands in a `.part` file so an interrupted transfer can never
    // be mistaken for a verified cache entry. Any failure on the way to a
    // verified file deletes it: a rejected artifact must leave nothing behind.
    let partial = cache.join(format!("{file_name}.part"));
    match download_and_verify(downloader, &artifact.url, &partial, &expected) {
        Ok(()) => {}
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
    }

    if destination.exists() {
        fs::remove_file(&destination).map_err(|source| Error::io(&destination, source))?;
    }
    fs::rename(&partial, &destination).map_err(|source| Error::io(&destination, source))?;
    check_size(artifact, &destination)?;
    Ok(Verified {
        path: destination,
        sha256: expected,
    })
}

/// Compares a downloaded file against the size the catalogue declared.
///
/// A mismatch is reported in the units a person reads, because "expected
/// 43117568 bytes, got 3145728" is what tells someone their transfer stopped
/// rather than that the archive was tampered with.
fn check_size(artifact: &Artifact, file: &Path) -> Result<()> {
    let Some(expected) = artifact.size else {
        return Ok(());
    };
    let actual = fs::metadata(file)
        .map_err(|source| Error::io(file, source))?
        .len();
    if actual == expected {
        return Ok(());
    }
    let _ = fs::remove_file(file);
    Err(Error::Download {
        url: artifact.url.clone(),
        reason: format!(
            "the download is the wrong size: expected {expected} bytes, got {actual} - \
             the transfer was probably interrupted, so the file was deleted"
        ),
    })
}

/// Fetches `url` into `destination` and checks it against `expected`.
///
/// Split out from [`download_verified`] so the caller can delete the partial
/// file whichever half fails.
fn download_and_verify(
    downloader: &dyn Downloader,
    url: &str,
    destination: &Path,
    expected: &str,
) -> Result<()> {
    downloader.fetch(url, destination)?;
    sha256::verify_file(destination, expected)
}

/// The checksum to verify against: the pinned one, else the published sidecar.
fn resolved_checksum(downloader: &dyn Downloader, artifact: &Artifact) -> Result<Option<String>> {
    if let Some(pinned) = &artifact.sha256 {
        return Ok(Some(pinned.clone()));
    }
    let Some(url) = &artifact.checksum_url else {
        return Ok(None);
    };
    require_https(url)?;

    let temporary = std::env::temp_dir().join(format!(
        "lambo-checksum-{}-{}",
        std::process::id(),
        artifact.file_name()
    ));
    let result = downloader.fetch(url, &temporary);
    let digest = match result {
        Ok(()) => sha256::parse_digest_file(&temporary)?,
        // A missing sidecar is not fatal here: the caller decides whether an
        // absent checksum is acceptable (it usually is not).
        Err(_) => None,
    };
    let _ = fs::remove_file(&temporary);
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Writes a payload and returns its digest.
    fn payload(temp: &TempDir, name: &str, contents: &[u8]) -> (PathBuf, String) {
        let path = temp.join(name);
        fs::write(&path, contents).unwrap();
        let digest = sha256::sha256_hex(contents);
        (path, digest)
    }

    /// A `file://` URL for a local payload.
    ///
    /// Lambo only accepts `https://` and `file://` URLs, so a bare path is not
    /// a valid artifact location - see `only_https_and_local_files_are_accepted`.
    fn url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    #[test]
    fn only_https_and_local_files_are_accepted() {
        assert!(require_https("https://windows.php.net/downloads/php.zip").is_ok());
        assert!(require_https("file:///tmp/php.zip").is_ok());
        for bad in [
            "http://windows.php.net/downloads/php.zip",
            "ftp://example.com/php.zip",
            "C:\\Lambo\\php.zip",
            "/tmp/php.zip",
            "",
        ] {
            assert!(require_https(bad).is_err(), "`{bad}` must be refused");
        }
    }

    #[test]
    fn a_verified_download_is_cached() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, digest) = payload(&temp, "php-8.4.2.zip", b"fake archive contents");

        let artifact = Artifact::pinned(url(&source), digest.clone());
        let verified = download_verified(&LocalDownloader, &artifact, &paths).unwrap();
        let cached = &verified.path;

        assert_eq!(*cached, paths.cache_dir().join("php-8.4.2.zip"));
        assert_eq!(fs::read(cached).unwrap(), b"fake archive contents");
        // The caller learns which digest was actually used, not just that one
        // was. Here it is the pinned one.
        assert_eq!(verified.sha256, digest);
        assert!(!paths.cache_dir().join("php-8.4.2.zip.part").exists());

        // A second call reuses the cache without touching the source.
        let again = download_verified(&LocalDownloader, &artifact, &paths).unwrap();
        let again = &again.path;
        assert_eq!(again, cached);
    }

    #[test]
    fn a_wrong_checksum_is_rejected_and_nothing_is_cached() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, _digest) = payload(&temp, "tampered.zip", b"what actually arrived");

        let artifact = Artifact::pinned(url(&source), "0".repeat(64));
        let error = download_verified(&LocalDownloader, &artifact, &paths).unwrap_err();

        assert!(matches!(error, Error::ChecksumMismatch { .. }), "{error:?}");
        assert!(
            !paths.cache_dir().join("tampered.zip").exists(),
            "nothing may be cached"
        );
        assert!(
            error.to_string().contains("was not extracted or executed"),
            "{error}"
        );
    }

    #[test]
    fn a_rejected_download_leaves_no_partial_file_behind() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, _digest) = payload(&temp, "tampered.zip", b"what actually arrived");

        // The digest is right for a different file, so verification must fail.
        let artifact = Artifact::pinned(url(&source), "0".repeat(64));
        let error = download_verified(&LocalDownloader, &artifact, &paths).unwrap_err();
        assert!(matches!(error, Error::ChecksumMismatch { .. }), "{error:?}");

        let leftovers: Vec<_> = fs::read_dir(paths.cache_dir())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a download that failed verification must be deleted, found: {leftovers:?}"
        );
    }

    #[test]
    fn a_download_of_the_wrong_size_is_rejected_and_deleted() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, digest) = payload(&temp, "php.zip", b"1234567890");

        // The digest is correct, so verification alone would pass; the size is
        // what a catalogue entry uses to say "this is a truncated transfer".
        let artifact = Artifact {
            url: url(&source),
            sha256: Some(digest),
            checksum_url: None,
            file_name_override: None,
            size: Some(1024),
        };
        let error = download_verified(&LocalDownloader, &artifact, &paths).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("expected 1024 bytes"), "{message}");
        assert!(message.contains("got 10 -"), "{message}");
        assert!(
            !message.contains("   "),
            "the message must not carry flattened line-continuation whitespace: {message}"
        );
        assert!(
            !paths.cache_dir().join("php.zip").exists(),
            "a rejected download must not stay in the cache"
        );

        // With the size matching, the same artifact verifies and is kept.
        let artifact = Artifact {
            size: Some(10),
            ..artifact
        };
        let verified = download_verified(&LocalDownloader, &artifact, &paths).unwrap();
        assert!(verified.path.is_file());
    }

    #[test]
    fn a_declared_file_name_decides_the_cache_name() {
        // The two URL shapes a real mirror uses, where deriving the name from
        // the URL gives the wrong answer.
        let redirect = Artifact {
            url: "https://mirror.example.com/latest".to_owned(),
            file_name_override: Some("php-8.4.2-linux-x64.tar.gz".to_owned()),
            ..Artifact::pinned("https://mirror.example.com/latest", "a".repeat(64))
        };
        assert_eq!(redirect.file_name(), "php-8.4.2-linux-x64.tar.gz");

        let query = Artifact {
            url: "https://mirror.example.com/download?file=php.zip".to_owned(),
            file_name_override: Some("php-8.4.2.zip".to_owned()),
            ..Artifact::pinned("https://mirror.example.com/download", "a".repeat(64))
        };
        assert_eq!(query.file_name(), "php-8.4.2.zip");
        // Without the declaration this one would have been cached as
        // "download", and every artifact behind that endpoint would collide.
        let undeclared = Artifact::with_sidecar("https://mirror.example.com/download?file=php.zip");
        assert_eq!(undeclared.file_name(), "download");
    }

    #[test]
    fn a_declared_file_name_cannot_escape_the_cache() {
        let artifact_for = |declared: &str| Artifact {
            file_name_override: Some(declared.to_owned()),
            ..Artifact::pinned("https://mirror.example.com/a.zip", "a".repeat(64))
        };

        // The declared name becomes a path inside the cache directory, so the
        // property that matters is that the result is a single plain component:
        // no separators, never `.` or `..`, never empty.
        for declared in [
            "../../etc/passwd",
            "..\\..\\windows\\system32\\drivers\\etc\\hosts",
            "/etc/passwd",
            "..",
            ".",
            "",
        ] {
            let name = artifact_for(declared).file_name();
            assert!(
                !name.contains('/') && !name.contains('\\'),
                "`{declared}` produced `{name}`, which has a separator and could escape the cache"
            );
            assert!(
                name != ".." && name != "." && !name.is_empty(),
                "`{declared}` produced `{name}`, which is not a usable file name"
            );
        }

        // A traversal attempt keeps only the last component, which is a plain
        // file inside the cache - harmless, and still what the mirror meant.
        assert_eq!(artifact_for("../../etc/passwd").file_name(), "passwd");
        // A name that is *only* a traversal component has nothing usable in it,
        // so those fall back to the URL-derived name.
        for declared in ["..", ".", ""] {
            assert_eq!(
                artifact_for(declared).file_name(),
                "a.zip",
                "`{declared}` should fall back to the URL name"
            );
        }
    }

    #[test]
    fn a_missing_checksum_fails_closed() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, _digest) = payload(&temp, "unknown.zip", b"contents");

        let artifact = Artifact {
            url: url(&source),
            sha256: None,
            checksum_url: None,
            file_name_override: None,
            size: None,
        };
        let error = download_verified(&LocalDownloader, &artifact, &paths).unwrap_err();

        // The whole point of a dedicated variant: a caller can branch on the
        // situation instead of grepping prose out of a formatted message.
        match &error {
            Error::VerificationUnavailable {
                url,
                family,
                version,
                platform,
                hint,
            } => {
                assert_eq!(url, &artifact.url);
                assert_eq!((family, version, platform), (&None, &None, &None));
                assert_eq!(hint, &None);
            }
            other => panic!("expected VerificationUnavailable, got {other}"),
        }
        // Nothing was fetched: an unverifiable artifact never reaches the cache.
        assert!(!paths.cache_dir().join("unknown.zip").exists());
    }

    #[test]
    fn an_upstream_sidecar_supplies_the_checksum() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, digest) = payload(&temp, "mariadb.zip", b"mariadb archive");
        let sidecar = temp.join("mariadb.zip.sha256");
        fs::write(&sidecar, format!("{digest}  mariadb.zip\n")).unwrap();

        let artifact = Artifact::with_sidecar(url(&source));
        let verified = download_verified(&LocalDownloader, &artifact, &paths).unwrap();
        assert_eq!(fs::read(&verified.path).unwrap(), b"mariadb archive");
        // The artifact itself is unpinned, so the digest that was verified
        // against came from the sidecar. An install manifest built from the
        // catalogue field alone would have recorded nothing.
        assert_eq!(verified.sha256, digest);
    }

    #[test]
    fn a_corrupted_cache_entry_is_refreshed() {
        let temp = TempDir::new();
        let paths = temp.home();
        let (source, digest) = payload(&temp, "php.zip", b"good contents");
        let artifact = Artifact::pinned(url(&source), digest);

        // Pretend a previous run left a damaged file behind.
        let cached = paths.cache_dir().join("php.zip");
        fs::create_dir_all(paths.cache_dir()).unwrap();
        fs::write(&cached, b"half-written garbage").unwrap();

        let refreshed = download_verified(&LocalDownloader, &artifact, &paths).unwrap();
        let refreshed = &refreshed.path;
        assert_eq!(fs::read(refreshed).unwrap(), b"good contents");
    }

    #[test]
    fn artifact_file_names_come_from_the_url() {
        let artifact = Artifact::with_sidecar(
            "https://windows.php.net/downloads/releases/archives/php-8.4.2-Win32-vs17-x64.zip",
        );
        assert_eq!(artifact.file_name(), "php-8.4.2-Win32-vs17-x64.zip");
        assert_eq!(
            artifact.checksum_url.as_deref(),
            Some(
                "https://windows.php.net/downloads/releases/archives/php-8.4.2-Win32-vs17-x64.zip.sha256"
            )
        );

        let query = Artifact::with_sidecar("https://example.com/adminer.php?v=4.8.1");
        assert_eq!(query.file_name(), "adminer.php");
    }
}
