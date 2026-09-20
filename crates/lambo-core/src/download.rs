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
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::fsx;
use crate::paths::Paths;
use crate::platform::Os;
use crate::process::{self, ProcessSpec};
use crate::sha256;

/// What the installer is doing, as shown on the dashboard's progress bar.
///
/// Ported from the original implementation's progress callback. The variants
/// are the stage names it
/// reported, because the GUI's label and bar position are derived from them:
/// `starting` sits at the left edge, `extracting` with an unknown entry count
/// sits halfway, `post-install` and `done` sit at the right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Nothing in flight.
    Idle,
    /// Preparing - the bar is at zero.
    Starting,
    /// Bytes are arriving.
    Downloading,
    /// Entries are being unpacked.
    Extracting,
    /// A post-install hook is running - the bar is full.
    PostInstall,
    /// The component is installed - the bar is full.
    Done,
}

impl Stage {
    /// The stage's wire name, as the progress callback spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Idle => "idle",
            Stage::Starting => "starting",
            Stage::Downloading => "downloading",
            Stage::Extracting => "extracting",
            Stage::PostInstall => "post-install",
            Stage::Done => "done",
        }
    }
}

/// Progress reports: `(stage, component, done, total)`.
///
/// `total` is a byte count while downloading and an entry count while
/// extracting; a value of zero or less means "unknown", which the GUI renders
/// by leaving the bar where it is and omitting the fraction from the label.
pub type ProgressFn = Arc<dyn Fn(Stage, &str, i64, i64) + Send + Sync + 'static>;

/// A progress sink that reports nothing.
///
/// The CLI uses this by default: a command that shares its terminal with the
/// program it started has nowhere to draw a bar.
pub fn nop_progress() -> ProgressFn {
    Arc::new(|_, _, _, _| {})
}

/// Fetches a URL into a local file.
pub trait Downloader {
    /// Downloads `url` to `destination`, replacing any previous content.
    fn fetch(&self, url: &str, destination: &Path) -> Result<()>;

    /// Downloads `url` to `destination`, reporting the bytes received so far.
    ///
    /// The default implementation transfers through [`Downloader::fetch`] and
    /// reports nothing: a transport that cannot observe a stream in flight (a
    /// local file copy, a test fixture) is still a correct transport, it just
    /// has nothing to say about progress. `total` is the size advertised by
    /// the server, or a non-positive value when it does not advertise one.
    fn fetch_with_progress(
        &self,
        url: &str,
        destination: &Path,
        progress: &dyn Fn(i64, i64),
    ) -> Result<()> {
        let _ = progress;
        self.fetch(url, destination)
    }
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
        // The slash before a Windows drive letter (`file:///C:/…`) is URL
        // furniture; the slash a POSIX path leads with is part of the path.
        let source = if cfg!(windows) && crate::sources::names_a_drive_path(source) {
            source.trim_start_matches('/')
        } else {
            source
        };
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

/// The transport the control-panel catalogue downloads through.
///
/// It differs from [`CurlDownloader`] in what it needs to report rather than in
/// what it fetches: an install shows a real progress bar, so the bytes have to
/// be observed as they arrive, and a mirror that answers with an error *page*
/// has to be refused before that page is mistaken for an archive.
///
/// The platform's `curl` still does the transfer (see [`CurlDownloader`] for
/// why), but with `--include` so the response head comes back on the same pipe
/// as the body. That head is read here, checked, and only then is the body
/// streamed to a `.part` file: a refusal therefore leaves nothing behind at all.
///
/// ```text
/// GET url →  curl --include --location --proto =https …  → head + body
///                                                          │
///                        status / Content-Type checked ─────┤
///                                                          ▼
///                                      128 KiB chunks → dest.part → dest
/// ```
///
/// The rules, all of them from the previous implementation:
///
/// * **HTTPS only**, enforced twice: [`require_https`] and curl's `--proto`.
/// * **A non-2xx status is an error**, reported as `HTTP 404`.
/// * **An HTML `Content-Type` is an error**, with the message that names the
///   extension it expected. Apache Lounge answers a removed build with `200`
///   and a small HTML page; without this check that page lands in the cache
///   under a `.zip` name and poisons every later install (issue #1).
/// * **A 128 KiB read buffer**, a progress report at most every 33 ms and a log
///   line at most every second, so a 600 MB download neither floods the log nor
///   starves the bar.
/// * **`.part` then rename**, with the part file removed on every failure path.
#[derive(Debug, Clone, Copy, Default)]
pub struct PanelDownloader;

/// How much is read from the socket at a time.
const CHUNK: usize = 128 * 1024;

/// How often progress is reported, at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(33);

/// How often a byte-count line is logged, at most.
const LOG_INTERVAL: Duration = Duration::from_secs(1);

/// The whole-request budget, matching the original client's timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// The `User-Agent` every download is sent with.
///
/// The product token carries no space: a `User-Agent` product token is a token,
/// and `Lambo PHP/0.14.0` would be read as two of them.
pub fn user_agent() -> String {
    format!(
        "{}/{} (+{})",
        crate::PRODUCT.replace(' ', ""),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

impl Downloader for PanelDownloader {
    fn fetch(&self, url: &str, destination: &Path) -> Result<()> {
        self.fetch_with_progress(url, destination, &|_, _| {})
    }

    fn fetch_with_progress(
        &self,
        url: &str,
        destination: &Path,
        progress: &dyn Fn(i64, i64),
    ) -> Result<()> {
        let log = crate::logs::nop_log();
        download_streaming(url, destination, &log, progress)
    }
}

/// Streams a URL to a file, the way the installer needs it.
///
/// A convenience over [`PanelDownloader`] for the one caller that fetches
/// outside the cache (`get-pip.py`), which also wants the log lines.
pub fn http_download(
    url: &str,
    destination: &Path,
    log: &crate::logs::LogFn,
    progress: &dyn Fn(i64, i64),
) -> Result<()> {
    download_streaming(url, destination, log, progress)
}

/// The working part of [`PanelDownloader::fetch_with_progress`].
fn download_streaming(
    url: &str,
    destination: &Path,
    log: &crate::logs::LogFn,
    progress: &dyn Fn(i64, i64),
) -> Result<()> {
    require_https(url)?;
    log(&format!("  GET {url}"));

    let spec = ProcessSpec::new(curl_program(), "download").args(curl_arguments(url));
    let mut child = process::spawn_piped(&spec, Os::host())?;

    let stdout = child.stdout.take().ok_or_else(|| Error::Http {
        url: url.to_owned(),
        reason: "the transfer produced no output stream".to_owned(),
    })?;
    // curl's diagnostics arrive on standard error. They are drained while the
    // body is read: a full pipe buffer would block the child, and the message
    // is what explains a failure.
    let stderr = child.stderr.take();
    let diagnostics = stderr.map(|stream| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::BufReader::new(stream).read_to_string(&mut text);
            text
        })
    });

    let part = part_path(destination);
    // The staging file is where the bytes go; `destination` is what a refusal
    // names, because that is the file the user asked for and the extension the
    // server got wrong.
    let transfer = stream_to_file(stdout, &part, destination, url, log, progress);

    // Whatever happened to the transfer, the child must not outlive it.
    let outcome = match transfer {
        Ok(()) => match child.wait() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(Error::Http {
                url: url.to_owned(),
                reason: describe_exit(status.code(), diagnostics),
            }),
            Err(source) => Err(Error::io(&part, source)),
        },
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    };

    if let Err(error) = outcome {
        let _ = fs::remove_file(&part);
        return Err(error);
    }

    // The rename is what makes the file exist as far as the cache is
    // concerned; until it happens there is only a part file to clean up.
    let _ = fs::remove_file(destination);
    fs::rename(&part, destination).map_err(|source| Error::io(&part, source))
}

/// The argument vector for a streaming transfer.
///
/// `--include` is the load-bearing flag: it puts the response head on standard
/// output ahead of the body, which is what lets the status and the content type
/// be checked before anything is written to disk.
fn curl_arguments(url: &str) -> Vec<String> {
    vec![
        "--include".to_owned(),
        "--location".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        "--proto".to_owned(),
        // Never downgrade to plain HTTP, on the initial request or a redirect.
        "=https".to_owned(),
        "--tlsv1.2".to_owned(),
        "--retry".to_owned(),
        "3".to_owned(),
        "--connect-timeout".to_owned(),
        "30".to_owned(),
        "--max-time".to_owned(),
        REQUEST_TIMEOUT.as_secs().to_string(),
        "--user-agent".to_owned(),
        user_agent(),
        url.to_owned(),
    ]
}

/// Where a download is written while it is in flight.
fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".part");
    destination.with_file_name(name)
}

/// Reads the head off a transfer, then streams the body into `part`.
///
/// `destination` is the file the download is *for*; the refusal below names it,
/// because a `.part` extension is our own and never what the server got wrong.
fn stream_to_file(
    stream: impl Read,
    part: &Path,
    destination: &Path,
    url: &str,
    log: &crate::logs::LogFn,
    progress: &dyn Fn(i64, i64),
) -> Result<()> {
    let mut buffered = std::io::BufReader::new(stream);
    let head = read_head(&mut buffered, url)?.ok_or_else(|| Error::Http {
        url: url.to_owned(),
        reason: "the server sent no response".to_owned(),
    })?;

    if !(200..300).contains(&head.status) {
        return Err(Error::Http {
            url: url.to_owned(),
            reason: format!("HTTP {}", head.status),
        });
    }
    if let Some(content_type) = head.header("content-type") {
        reject_html(url, destination, content_type)?;
    }

    let total = head.content_length();
    if let Some(declared) = total.filter(|declared| *declared > 0) {
        log(&format!(
            "  size: {:.1} MB",
            declared as f64 / (1024.0 * 1024.0)
        ));
    }

    if let Some(parent) = part.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
    }
    let mut file = fs::File::create(part).map_err(|source| Error::io(part, source))?;

    let declared = total.map_or(-1, |declared| declared as i64);
    let mut buffer = vec![0u8; CHUNK];
    let mut written: i64 = 0;
    let mut last_log = Instant::now();
    let mut last_progress = Instant::now();

    loop {
        let read = match buffered.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(source) => return Err(Error::io(part, source)),
        };
        file.write_all(&buffer[..read])
            .map_err(|source| Error::io(part, source))?;
        written += read as i64;

        if last_log.elapsed() >= LOG_INTERVAL {
            log(&describe_progress(written, declared));
            last_log = Instant::now();
        }
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            progress(written, declared);
            last_progress = Instant::now();
        }
    }

    // The bar must reach its end even for a transfer that finished inside the
    // last interval.
    progress(written, declared);
    file.flush().map_err(|source| Error::io(part, source))?;
    Ok(())
}

/// The line logged while a download is in flight.
fn describe_progress(written: i64, declared: i64) -> String {
    let mebibytes = |bytes: i64| bytes as f64 / (1024.0 * 1024.0);
    if declared > 0 {
        let percent = written as f64 * 100.0 / declared as f64;
        format!(
            "  {percent:>5.1}%  {:>6.1} / {:.1} MB",
            mebibytes(written),
            mebibytes(declared)
        )
    } else {
        format!("  downloaded {:.1} MB", mebibytes(written))
    }
}

/// Refuses a response whose type says the body is a web page.
///
/// This is the guard against the failure that started all of this: a mirror
/// answering a removed build with `200 OK` and an HTML error page.
fn reject_html(url: &str, destination: &Path, content_type: &str) -> Result<()> {
    if !content_type.to_lowercase().contains("html") {
        return Ok(());
    }
    let extension = destination
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    Err(Error::Download {
        url: url.to_owned(),
        reason: format!(
            "expected {extension} but got Content-Type {content_type:?} — the server returned \
             a page, not the file (URL likely stale)"
        ),
    })
}

/// The response head: status and headers, of the final response in a redirect
/// chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ResponseHead {
    status: u16,
    headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// A header value by name, case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The declared body length, when the server declares one it can honour.
    fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse().ok()
    }
}

/// Reads the response head from an HTTP stream.
///
/// Handles a redirect chain by keeping the last head seen, and leaves the
/// reader positioned at the first body byte. Returns `None` when the stream
/// ended before a complete head arrived.
///
/// The body-vs-another-head decision is made by looking at the first bytes after
/// a blank line: a replay means a second response follows. A file whose content
/// begins with the literal text `HTTP/` would be misread - archives never do,
/// and the alternative (trusting curl's own idea of where the head ends) is a
/// separate `--dump-header` file this would then have to keep in step with the
/// body it streamed.
fn read_head(reader: &mut impl BufRead, url: &str) -> Result<Option<ResponseHead>> {
    let mut head: Option<ResponseHead> = None;
    loop {
        let peeked = reader.fill_buf().map_err(|source| Error::Http {
            url: url.to_owned(),
            reason: source.to_string(),
        })?;
        if peeked.is_empty() {
            return Ok(head);
        }
        if !peeked.starts_with(b"HTTP/") {
            // The body starts here, with these bytes still buffered.
            return Ok(head);
        }
        head = Some(read_head_block(reader, url)?);
    }
}

/// Reads one `status line + headers + blank line` block.
fn read_head_block(reader: &mut impl BufRead, url: &str) -> Result<ResponseHead> {
    let mut line = String::new();
    let read_line = |reader: &mut dyn BufRead, line: &mut String| -> Result<usize> {
        line.clear();
        reader.read_line(line).map_err(|source| Error::Http {
            url: url.to_owned(),
            reason: source.to_string(),
        })
    };

    if read_line(&mut *reader, &mut line)? == 0 {
        return Err(Error::Http {
            url: url.to_owned(),
            reason: "the response ended before its status line".to_owned(),
        });
    }
    let status = parse_status_line(&line).ok_or_else(|| Error::Http {
        url: url.to_owned(),
        reason: format!("unreadable status line `{}`", line.trim()),
    })?;

    let mut headers = Vec::new();
    loop {
        if read_line(&mut *reader, &mut line)? == 0 {
            // A head that is never terminated by a blank line would otherwise
            // swallow the whole response.
            return Err(Error::Http {
                url: url.to_owned(),
                reason: "the response ended inside its headers".to_owned(),
            });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }

    Ok(ResponseHead { status, headers })
}

/// Reads the status code out of a status line.
///
/// `HTTP/1.1 404 Not Found`, `HTTP/2 200` and `HTTP/1.0 200 OK` all yield their
/// code; anything else yields `None`.
fn parse_status_line(line: &str) -> Option<u16> {
    let rest = line.trim().strip_prefix("HTTP/")?;
    let mut parts = rest.split_whitespace();
    let _version = parts.next()?;
    parts.next()?.parse().ok()
}

/// The message reported when the transport itself failed.
fn describe_exit(
    code: Option<i32>,
    diagnostics: Option<std::thread::JoinHandle<String>>,
) -> String {
    let text = diagnostics
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let text = text.trim();
    match (code, text.is_empty()) {
        (Some(code), true) => format!("curl exited with {code}"),
        (Some(code), false) => format!("curl exited with {code}: {text}"),
        (None, true) => "the transfer was terminated".to_owned(),
        (None, false) => format!("the transfer was terminated: {text}"),
    }
}

/// Fetches a small document into memory over a transport.
///
/// Used by the resolvers that have no fixed URL to point at: Apache Lounge's
/// download index and Zig's release index are both HTML/JSON documents whose
/// content decides what to fetch next.
///
/// The body is spooled through a temporary file because [`Downloader`] writes
/// to a file by design - that is what makes it safe to hand it an archive - and
/// `limit` bounds what is read into memory, so a hostile or misconfigured server
/// cannot turn a page fetch into an out-of-memory condition. The scratch file
/// lives in the system temporary directory and is removed before returning,
/// whatever happened.
pub fn fetch_document(downloader: &dyn Downloader, url: &str, limit: u64) -> Result<String> {
    let (path, file) = fsx::create_temp(&std::env::temp_dir(), ".lambo-fetch-")?;
    drop(file);

    let result = downloader.fetch(url, &path).and_then(|()| {
        let file = fs::File::open(&path).map_err(|error| Error::io(&path, error))?;
        let mut buffer = Vec::new();
        file.take(limit)
            .read_to_end(&mut buffer)
            .map_err(|error| Error::io(&path, error))?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    });

    let _ = fs::remove_file(&path);
    result
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

        let name = self
            .url
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("download.bin");

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

    // -----------------------------------------------------------------
    // The streaming transport (the control-panel downloads)
    // -----------------------------------------------------------------

    /// Builds a response exactly as `curl --include` writes it.
    fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (name, value) in headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(body);
        out
    }

    fn log_sink() -> (crate::logs::LogFn, Arc<std::sync::Mutex<Vec<String>>>) {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: crate::logs::LogFn = Arc::new(move |line: &str| {
            sink.lock().unwrap().push(line.to_owned());
        });
        (log, lines)
    }

    #[test]
    fn the_response_head_is_read_before_the_body() {
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(response(
            "200 OK",
            &[
                ("Content-Type", "application/zip"),
                ("Content-Length", "11"),
            ],
            b"PK\x03\x04bytes",
        )));

        let head = read_head(&mut reader, "https://x/y.zip")
            .unwrap()
            .expect("a head must be read");

        assert_eq!(head.status, 200);
        assert_eq!(head.header("content-type"), Some("application/zip"));
        assert_eq!(head.header("Content-Type"), Some("application/zip"));
        assert_eq!(head.content_length(), Some(11));

        // The reader is left exactly at the first body byte: nothing of the
        // body was consumed with the head.
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"PK\x03\x04bytes");
    }

    #[test]
    fn a_redirect_chain_yields_the_last_head() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&response(
            "302 Found",
            &[("Location", "https://mirror/y.zip")],
            b"",
        ));
        stream.extend_from_slice(&response("200 OK", &[("Content-Length", "4")], b"data"));

        let head = read_head(
            &mut std::io::BufReader::new(std::io::Cursor::new(stream)),
            "u",
        )
        .unwrap()
        .expect("a head");
        assert_eq!(head.status, 200, "the final response decides");
        assert_eq!(head.header("location"), None);
        assert_eq!(head.content_length(), Some(4));
    }

    #[test]
    fn a_missing_content_length_is_not_a_length() {
        let head = ResponseHead {
            status: 200,
            headers: vec![("Content-Length".to_owned(), "not a number".to_owned())],
        };
        assert_eq!(head.content_length(), None);

        let head = ResponseHead {
            status: 200,
            headers: vec![("Content-Length".to_owned(), "-1".to_owned())],
        };
        assert_eq!(head.content_length(), None);
    }

    #[test]
    fn status_lines_of_every_shape_are_parsed() {
        assert_eq!(parse_status_line("HTTP/1.1 404 Not Found\r\n"), Some(404));
        assert_eq!(parse_status_line("HTTP/1.0 200 OK"), Some(200));
        assert_eq!(parse_status_line("HTTP/2 200"), Some(200));
        assert_eq!(parse_status_line("  HTTP/1.1 301 Moved\n"), Some(301));
        assert_eq!(parse_status_line("HTTP/1.1 999\n"), Some(999));
        assert_eq!(parse_status_line("ICY 200 OK"), None);
        assert_eq!(parse_status_line("HTTP/1.1"), None);
        assert_eq!(parse_status_line("HTTP/1.1 abc"), None);
        assert_eq!(parse_status_line(""), None);
    }

    #[test]
    fn a_head_that_never_ends_is_an_error() {
        let stream = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n".to_vec();
        let error = read_head(
            &mut std::io::BufReader::new(std::io::Cursor::new(stream)),
            "u",
        )
        .expect_err("an unterminated head must be refused");
        assert!(error.to_string().contains("inside its headers"), "{error}");
    }

    #[test]
    fn an_html_content_type_is_refused_before_anything_is_written() {
        // The exact failure this guards: a stale URL answered with a page.
        let error = reject_html(
            "https://www.apachelounge.com/download/VS18/binaries/httpd.zip",
            Path::new("/cache/httpd-2.4.68.zip"),
            "text/HTML; charset=utf-8",
        )
        .expect_err("html must be refused");
        let text = error.to_string();
        assert!(
            text.contains("expected .zip but got Content-Type"),
            "{text}"
        );
        assert!(
            text.contains("the server returned a page, not the file (URL likely stale)"),
            "{text}"
        );

        // Anything else is passed through, including no content type at all.
        assert!(reject_html("u", Path::new("a.zip"), "application/octet-stream").is_ok());
        assert!(reject_html("u", Path::new("a.exe"), "application/x-msdownload").is_ok());
        assert!(reject_html("u", Path::new("a.phar"), "").is_ok());
        // A stored file whose *payload* is html but whose type is not is not
        // this guard's business - Adminer ships as a .php page.
        assert!(reject_html("u", Path::new("adminer.php"), "text/plain").is_ok());
    }

    #[test]
    fn a_non_2xx_status_is_reported_as_http() {
        let stream = response("404 Not Found", &[], b"<html>gone</html>");
        let temp = TempDir::new();
        let part = temp.join("thing.zip.part");
        let part_destination = temp.join("thing.zip");
        let (log, lines) = log_sink();

        let error = stream_to_file(
            std::io::Cursor::new(stream),
            &part,
            &part_destination,
            "https://mirror/thing.zip",
            &log,
            &|_, _| {},
        )
        .expect_err("a 404 must fail");

        assert!(error.to_string().contains("HTTP 404"), "{error}");
        assert!(!part.exists(), "nothing may be written for a failed status");
        assert!(
            lines.lock().unwrap().is_empty(),
            "no size line without a body"
        );
    }

    #[test]
    fn a_downloaded_page_is_refused_with_its_extension_named() {
        let stream = response(
            "200 OK",
            &[("Content-Type", "text/html; charset=UTF-8")],
            b"<!DOCTYPE html><html>nope</html>",
        );
        let temp = TempDir::new();
        let part = temp.join("nginx-1.28.3.zip.part");
        let part_destination = temp.join("nginx-1.28.3.zip");

        let error = stream_to_file(
            std::io::Cursor::new(stream),
            &part,
            &part_destination,
            "https://nginx.org/download/nginx-1.28.3.zip",
            &crate::logs::nop_log(),
            &|_, _| {},
        )
        .expect_err("an error page must not be written");

        assert!(error.to_string().contains("expected .zip"), "{error}");
        assert!(!part.exists());
    }

    #[test]
    fn the_body_is_streamed_to_the_part_file_with_progress() {
        let body = vec![b'z'; 300 * 1024];
        let stream = response(
            "200 OK",
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", &body.len().to_string()),
            ],
            &body,
        );
        let temp = TempDir::new();
        let part = temp.join("php.zip.part");
        let part_destination = temp.join("php.zip");
        let (log, lines) = log_sink();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);

        stream_to_file(
            std::io::Cursor::new(stream),
            &part,
            &part_destination,
            "https://windows.php.net/php.zip",
            &log,
            &move |done, total| sink.lock().unwrap().push((done, total)),
        )
        .unwrap();

        assert_eq!(fs::read(&part).unwrap().len(), body.len());
        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "one size line: {lines:?}");
        assert_eq!(lines[0], "  size: 0.3 MB");

        // The final report always arrives, even inside the reporting interval.
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.last(), Some(&(body.len() as i64, body.len() as i64)));

        // The log format of a transfer in flight is the original's.
        assert_eq!(
            describe_progress(1024 * 1024, 4 * 1024 * 1024),
            "   25.0%     1.0 / 4.0 MB"
        );
        assert_eq!(
            describe_progress(3 * 1024 * 1024, -1),
            "  downloaded 3.0 MB"
        );
    }

    #[test]
    fn progress_is_reported_repeatedly_over_a_slow_transfer() {
        /// A stream that dribbles bytes out, so the reporting interval passes.
        struct Slow {
            remaining: usize,
        }

        impl Read for Slow {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(20));
                let chunk = buffer.len().min(8 * 1024).min(self.remaining);
                for byte in buffer.iter_mut().take(chunk) {
                    *byte = b'x';
                }
                self.remaining -= chunk;
                Ok(chunk)
            }
        }

        let temp = TempDir::new();
        let part = temp.join("big.zip.part");
        let part_destination = temp.join("big.zip");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        // The head arrives from a normal reader, then the body dribbles.
        let head = std::io::Cursor::new(response(
            "200 OK",
            &[("Content-Length", &(64 * 1024).to_string())],
            b"",
        ));

        stream_to_file(
            head.chain(Slow {
                remaining: 64 * 1024,
            }),
            &part,
            &part_destination,
            "https://example.com/big.zip",
            &crate::logs::nop_log(),
            &move |_, _| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .unwrap();

        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a transfer that lasts longer than the interval must be reported \
             more than once, got {}",
            calls.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[test]
    fn a_reading_error_leaves_the_part_file_to_the_caller() {
        /// A stream that fails part-way through the body.
        struct Broken {
            sent: usize,
        }

        impl Read for Broken {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.sent == 0 {
                    buffer[..5].copy_from_slice(b"bytes");
                    self.sent = 5;
                    return Ok(5);
                }
                Err(std::io::Error::other("connection reset"))
            }
        }

        let temp = TempDir::new();
        let part = temp.join("node.zip.part");
        let part_destination = temp.join("node.zip");
        let stream = std::io::Cursor::new(response("200 OK", &[], b"")).chain(Broken { sent: 0 });

        let error = stream_to_file(
            stream,
            &part,
            &part_destination,
            "https://nodejs.org/node.zip",
            &crate::logs::nop_log(),
            &|_, _| {},
        )
        .expect_err("a broken stream must fail");
        assert!(error.to_string().contains("connection reset"), "{error}");

        // Removing the part file is `download_streaming`'s job, which is why
        // this function reports the failure and leaves what it was writing:
        // the caller knows whether the destination was a fresh download.
        assert!(part.exists(), "the partial file is left for the caller");
        assert_eq!(fs::read(&part).unwrap(), b"bytes");
        let _ = fs::remove_file(&part);
    }

    #[test]
    fn the_transport_argument_vector_never_permits_plain_http() {
        let args = curl_arguments("https://example.com/php.zip");

        assert!(args.contains(&"--include".to_owned()), "the head is needed");
        assert!(args.contains(&"--location".to_owned()));
        assert!(args.contains(&"--max-time".to_owned()));
        // `--proto =https` is the one that matters: it stops a redirect from
        // downgrading the transfer to plain HTTP.
        let proto = args.iter().position(|arg| arg == "--proto").unwrap();
        assert_eq!(args[proto + 1], "=https");
        assert!(!args.iter().any(|arg| arg.contains("=http,")));
        assert_eq!(args.last().unwrap(), "https://example.com/php.zip");
    }

    #[test]
    fn the_user_agent_names_this_product_and_builds_from_the_crate_metadata() {
        let agent = user_agent();
        // The previous implementation sent its own product name and version in
        // this shape; this keeps the shape: the product and its version, then
        // the project in parentheses.
        assert!(agent.starts_with("LamboPHP/"), "{agent}");
        assert!(agent.contains(env!("CARGO_PKG_VERSION")), "{agent}");
        assert!(agent.contains(env!("CARGO_PKG_REPOSITORY")), "{agent}");
        assert_eq!(
            agent,
            format!(
                "LamboPHP/{} (+{})",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_REPOSITORY")
            )
        );
    }

    #[test]
    fn a_part_file_is_a_sibling_of_its_destination() {
        assert_eq!(
            part_path(Path::new("/cache/php.zip")),
            PathBuf::from("/cache/php.zip.part")
        );
        assert_eq!(
            part_path(Path::new("php.zip")),
            PathBuf::from("php.zip.part")
        );
    }

    #[test]
    fn only_https_and_local_files_are_accepted_by_the_streaming_transport() {
        // The transport refuses a plain-http URL before starting curl, which is
        // what keeps a mirror from being reached over an unencrypted
        // connection even if a catalogue is edited by hand.
        let temp = TempDir::new();
        let destination = temp.join("thing.zip");
        let error = http_download(
            "http://mirror.example.com/thing.zip",
            &destination,
            &crate::logs::nop_log(),
            &|_, _| {},
        )
        .expect_err("plain http must be refused");
        assert!(matches!(error, Error::NotHttps(_)), "{error}");
        assert!(!destination.exists());
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
