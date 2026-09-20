//! The shared downloads cache: `<base>/downloads/`.
//!
//! It exists because the same two mistakes were written out by hand in two
//! places of the previous implementation - the installer and the framework
//! scaffolder - and both had the same bug: a bare existence check (`os.Stat`)
//! was treated as proof that a cached file was complete and valid. It is not.
//! Apache Lounge answers a removed build with HTTP 200 and a small HTML error
//! page, and that landed in `downloads/` under a `.zip` name. The existence
//! check saw a file, the caller skipped the download, and every later run
//! failed at extraction on the same poisoned copy - forever, with no way out
//! short of deleting the file by hand.
//!
//! So caching goes through here: **a file is adopted as valid at the moment it
//! is written, or never.** Anything already sitting in `downloads/` from an
//! earlier version of the application is treated as untrusted, because nothing
//! recorded where it came from. [`DownloadCache::purge_stale`] is how those are
//! re-checked, and the check is deliberately conservative: `downloads/` is not
//! uniform. It holds zips, `.exe` installers and legitimately textual payloads
//! (Adminer ships as a `.php` file, Composer as a `.phar`, pip's bootstrap as a
//! `.py`), so a file is only removed when there is positive evidence it is
//! wrong - see [`stale_reason`].
//!
//! Ported from the original implementation's `cache.go`. Behaviour is preserved
//! exactly, including
//! the log lines, the "treat a stale file as absent but do not delete it here"
//! split between `ensure` and `purge_stale`, and the deliberately narrow set of
//! files that are content-inspected.
//!
//! # Differences that are deliberate
//!
//! * **No process-wide cache.** Go held one in a package variable, set once
//!   `baseDir` was known, with `cacheFor` falling back to a fresh instance. In
//!   Rust the cache is constructed by the application (or by a CLI command)
//!   and passed to whatever needs it - the same "no global state" rule the
//!   rest of this crate follows, and the reason the whole crate's tests can
//!   run in parallel. The observable behaviour is unchanged: one cache per
//!   installation root, swept once at startup, reused by every install.
//! * **Exclusive access instead of interior mutability.** Go's pointer
//!   receivers mutated the adoption set through a shared pointer; here the
//!   mutating methods take `&mut self`, so no lock is needed.
//! * **The adoption sidecar is written atomically** ([`crate::fsx`]), and its
//!   names are stored sorted rather than in Go's map order. The file is a set
//!   of names either way; only the line order differs, and a crash can no
//!   longer leave a half-written sidecar behind.
//! * **The previous implementation's sidecar is not trusted, only cleared.**
//!   See [`LEGACY_SIDECAR_FILE`].

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::download::{Downloader, ProgressFn, Stage};
use crate::error::{Error, Result};
use crate::fsx;
use crate::logs::LogFn;

/// The directory the cache owns, relative to the installation root.
pub const DOWNLOADS_DIR: &str = "downloads";

/// The file that records which entries this application fetched itself.
pub const SIDECAR_FILE: &str = ".lambo-verified";

/// The sidecar the previous implementation wrote.
///
/// An installation upgraded in place still has one. Its names are *not*
/// adopted: they record the trust of a different program, and the whole point
/// of the sidecar is that only this process's own downloads are trustworthy -
/// a hand-edited or poisoned file recorded there would be trusted forever.
/// Instead the file is removed on first read and the files it mentioned are
/// re-validated by [`stale_reason`], which costs one read of each file's first
/// few hundred bytes. A clean cache is therefore re-adopted without any
/// download, and a poisoned one is finally collected.
pub const LEGACY_SIDECAR_FILE: &str = ".goampp-verified";

/// How much of a file's head is inspected when validating it.
const HEAD_BYTES: usize = 512;

/// How much of that head is searched for HTML markers.
const HTML_WINDOW: usize = 256;

/// The downloads cache for one installation root.
pub struct DownloadCache {
    dir: PathBuf,
    log: LogFn,
    downloader: Box<dyn Downloader + Send + Sync>,
    /// Names this process downloaded itself. Anything absent is untrusted.
    adopted: BTreeSet<String>,
    /// Whether [`DownloadCache::adopted`] has been read from disk yet.
    loaded: bool,
}

impl fmt::Debug for DownloadCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadCache")
            .field("dir", &self.dir)
            .field("adopted", &self.adopted.len())
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl DownloadCache {
    /// Opens (but does not create) the cache rooted at `base_dir`.
    pub fn new(base_dir: &Path, log: LogFn, downloader: Box<dyn Downloader + Send + Sync>) -> Self {
        Self {
            dir: base_dir.join(DOWNLOADS_DIR),
            log,
            downloader,
            adopted: BTreeSet::new(),
            loaded: false,
        }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The cache location of a file name.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Whether a usable file already exists at `name`, creating the cache
    /// directory on the way.
    ///
    /// `false` is a download instruction, not an error: only a failure to
    /// prepare the directory is an error.
    pub fn ensure(&mut self, name: &str) -> Result<bool> {
        fsx::ensure_dir(&self.dir)?;
        self.load_adopted();

        let path = self.path(name);
        let usable = match fs::metadata(&path) {
            Ok(meta) => !meta.is_dir() && meta.len() > 0,
            Err(_) => false,
        };
        if !usable {
            // Missing, unreadable, a directory, or empty. An adoption record
            // for it is now a lie, so drop it.
            self.adopted.remove(name);
            return Ok(false);
        }

        if !self.adopted.contains(name) {
            // On disk but not recorded as fetched by us - a leftover from an
            // earlier run, or a file the user replaced by hand. Validate before
            // trusting it: a bare existence check is exactly what let a
            // poisoned file poison the cache in the first place.
            if stale_reason(&path, name).is_some() {
                // `purge_stale` will collect it; treat it as absent.
                return Ok(false);
            }
            self.mark_adopted(name, true);
        }
        Ok(true)
    }

    /// Downloads `url` into the cache and adopts the result as verified.
    ///
    /// A failed download leaves nothing behind to be mistaken for a cached
    /// file next run.
    pub fn save(&mut self, name: &str, url: &str, progress: &ProgressFn) -> Result<()> {
        self.ensure(name)?;
        let path = self.path(name);

        let transferred = {
            let report = |done: i64, total: i64| progress(Stage::Downloading, name, done, total);
            self.downloader.fetch_with_progress(url, &path, &report)
        };
        if let Err(error) = transferred {
            // The transfer renames its temporary file into place only on
            // success, but be explicit: a half-written file must not become
            // tomorrow's cache hit.
            self.discard(name, &path);
            return Err(error);
        }

        match fs::metadata(&path) {
            Ok(meta) if meta.len() > 0 => {}
            _ => {
                self.discard(name, &path);
                return Err(Error::Download {
                    url: url.to_owned(),
                    reason: format!("downloaded {name} is empty"),
                });
            }
        }

        self.mark_adopted(name, true);
        Ok(())
    }

    /// Returns the path to a verified cached copy of `url`, downloading it
    /// first if needed. The returned path is safe to extract or run.
    pub fn fetch(&mut self, name: &str, url: &str, progress: &ProgressFn) -> Result<PathBuf> {
        if self.ensure(name)? {
            (self.log)(&format!("  using cached {name}"));
        } else {
            self.save(name, url, progress)?;
        }
        Ok(self.path(name))
    }

    /// Removes cache entries that are provably not the file they claim to be,
    /// returning the names it removed.
    ///
    /// Deliberately conservative - see [`stale_reason`] for what counts as
    /// proof. Everything else is left alone: the worst case is that an odd but
    /// real file stays cached, which is strictly better than deleting a
    /// runtime somebody's project needs.
    pub fn purge_stale(&mut self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(&self.dir, error)),
        };

        // Sorted, so the log reads the same way on every run: the previous
        // implementation read a directory listing that was already sorted.
        let mut entries = entries
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| Error::io(&self.dir, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);

        let mut removed = Vec::new();
        for entry in entries {
            // `file_type` does not follow a symlink, so a link to a directory
            // is skipped here exactly as a directory entry is.
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }

            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(reason) = stale_reason(&path, &name) else {
                continue;
            };

            match fs::remove_file(&path) {
                Ok(()) => {
                    (self.log)(&format!("  removed stale cached {name} — {reason}"));
                    removed.push(name);
                }
                Err(error) => {
                    (self.log)(&format!("  could not remove stale {name}: {error}"));
                }
            }
        }

        // The disk changed underneath the adoption set.
        self.loaded = false;
        Ok(removed)
    }

    /// Clears provably-broken files left by an earlier run, so an installation
    /// that already hit the Apache Lounge bug heals on the next launch instead
    /// of failing at extraction once more. Runs once at startup.
    ///
    /// Returns the names it removed; a failure to read the cache directory is
    /// reported through the log rather than to the caller, because a cache
    /// that cannot be swept is not a reason to refuse to start.
    pub fn sweep(&mut self) -> Vec<String> {
        match self.purge_stale() {
            Ok(removed) => {
                if !removed.is_empty() {
                    (self.log)(&format!(
                        "downloads cache: cleared {} stale file(s) — they will be fetched again",
                        removed.len()
                    ));
                }
                removed
            }
            Err(error) => {
                (self.log)(&format!("downloads cache check: {error}"));
                Vec::new()
            }
        }
    }

    /// The names this process has adopted as verified.
    pub fn adopted(&self) -> impl Iterator<Item = &str> {
        self.adopted.iter().map(String::as_str)
    }

    /// Removes a file and drops its adoption record.
    fn discard(&mut self, name: &str, path: &Path) {
        let _ = fs::remove_file(path);
        self.adopted.remove(name);
    }

    /// Reads the adoption sidecar once per instance (and again after a sweep).
    fn load_adopted(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        self.adopted = read_names(&self.sidecar_path());

        // The previous implementation's record is cleared, never trusted: see
        // `LEGACY_SIDECAR_FILE`.
        let legacy = self.dir.join(LEGACY_SIDECAR_FILE);
        if legacy.is_file() {
            let _ = fs::remove_file(&legacy);
        }
    }

    /// Records (or forgets) a name and rewrites the sidecar.
    ///
    /// A sidecar that cannot be written is not an error: the file on disk is
    /// still there and still usable, it simply has to be validated again next
    /// time, which is the conservative direction.
    fn mark_adopted(&mut self, name: &str, adopted: bool) {
        self.load_adopted();
        if adopted {
            self.adopted.insert(name.to_owned());
        } else {
            self.adopted.remove(name);
        }

        let mut contents = String::new();
        for entry in &self.adopted {
            contents.push_str(entry);
            contents.push('\n');
        }
        let _ = fsx::write_atomic(&self.sidecar_path(), &contents);
    }

    /// The adoption sidecar.
    fn sidecar_path(&self) -> PathBuf {
        self.dir.join(SIDECAR_FILE)
    }
}

/// The names recorded in a sidecar file. A missing or unreadable file reads as
/// an empty set, never as an error: nothing recorded means nothing trusted.
fn read_names(path: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(contents) = fs::read_to_string(path) else {
        return names;
    };
    for line in contents.split('\n') {
        let name = line.trim();
        if !name.is_empty() {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Why a cached file cannot be trusted, or `None` when it is fine.
///
/// A file is only rejected on positive evidence:
///
/// * zero size - it cannot be a usable download, whatever it is;
/// * a `.zip` that is not a zip by magic bytes - the poisoned case;
/// * content that is HTML in a file that claims to be a `.zip` or an `.exe` -
///   the exact shape of an error page served under a `200`.
///
/// Read errors and directories return `None`: an unreadable file is not proof
/// of corruption, and deleting something that cannot be read is not safe.
pub fn stale_reason(path: &Path, name: &str) -> Option<&'static str> {
    let mut file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if meta.is_dir() {
        return None;
    }
    if meta.len() == 0 {
        return Some("file is empty");
    }

    // Only archives and installers are content-inspected. A textual payload
    // under any other name is legitimate and is left alone.
    let kind = Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if kind != "zip" && kind != "exe" {
        return None;
    }

    let mut head = [0u8; HEAD_BYTES];
    let read = file.read(&mut head).unwrap_or(0);
    if read == 0 {
        return None;
    }
    let head = &head[..read];

    if kind == "zip" && !has_zip_magic(head) {
        return Some("not a zip (truncated or error page saved under a zip name)");
    }
    if is_html(head) {
        return Some("looks like an HTML error page");
    }
    None
}

/// Whether the bytes start with a ZIP local file, end-of-central-directory or
/// data-descriptor signature.
pub fn has_zip_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes[0] == b'P'
        && bytes[1] == b'K'
        && matches!(bytes[2], 3 | 5 | 7)
        && matches!(bytes[3], 4 | 6 | 8)
}

/// Whether the bytes look like the start of an HTML or XML document.
pub fn is_html(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(HTML_WINDOW)];
    let text = String::from_utf8_lossy(window).trim().to_lowercase();
    ["<!doctype html", "<html", "<?xml", "<head>", "<body"]
        .iter()
        .any(|marker| text.starts_with(marker))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::download::nop_progress;
    use crate::logs::nop_log;
    use crate::testutil::TempDir;

    /// The bytes of a real (if tiny) ZIP archive.
    const ZIP: &[u8] = b"PK\x03\x04\x14\x00\x00\x00\x08\x00payload";

    /// A downloader that writes canned bytes and counts its calls, so a test
    /// can tell a cache hit from a second download.
    struct FixtureDownloader {
        calls: Arc<AtomicUsize>,
        payload: Vec<u8>,
        report: Option<(i64, i64)>,
    }

    impl FixtureDownloader {
        fn new(payload: Vec<u8>) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                payload,
                report: None,
            }
        }

        fn reporting(payload: Vec<u8>, done: i64, total: i64) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                payload,
                report: Some((done, total)),
            }
        }

        fn calls(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.calls)
        }
    }

    impl Downloader for FixtureDownloader {
        fn fetch(&self, _url: &str, destination: &Path) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
            }
            fs::write(destination, &self.payload).map_err(|error| Error::io(destination, error))
        }

        fn fetch_with_progress(
            &self,
            url: &str,
            destination: &Path,
            progress: &dyn Fn(i64, i64),
        ) -> Result<()> {
            if let Some((done, total)) = self.report {
                progress(done, total);
            }
            self.fetch(url, destination)
        }
    }

    /// A downloader that always fails, without touching the file system.
    struct FailingDownloader;

    impl Downloader for FailingDownloader {
        fn fetch(&self, url: &str, _destination: &Path) -> Result<()> {
            Err(Error::Download {
                url: url.to_owned(),
                reason: "connection refused".to_owned(),
            })
        }
    }

    /// A log sink that records everything it is given.
    fn recorder() -> (LogFn, Arc<std::sync::Mutex<Vec<String>>>) {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_owned());
        });
        (log, lines)
    }

    fn cache(temp: &TempDir, downloader: impl Downloader + Send + Sync + 'static) -> DownloadCache {
        DownloadCache::new(temp.path(), nop_log(), Box::new(downloader))
    }

    fn write(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create fixture directory");
        }
        fs::write(path, contents).expect("failed to write fixture");
    }

    fn exists(path: &Path) -> bool {
        path.exists()
    }

    #[test]
    fn purge_stale_removes_only_provably_broken() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(Vec::new()));
        let dir = cache.dir().to_path_buf();
        fs::create_dir_all(&dir).expect("failed to create the cache directory");

        // A real zip, an error page saved as a zip, an empty zip, and an error
        // page saved as an installer: the first survives, the rest do not.
        let good = dir.join("apache.zip");
        write(&good, ZIP);
        let poisoned = dir.join("php.zip");
        write(
            &poisoned,
            b"<!DOCTYPE html><html><body>404 Not Found</body></html>",
        );
        let empty = dir.join("node.zip");
        write(&empty, b"");
        let bad_exe = dir.join("mysql.exe");
        write(&bad_exe, b"<html><head><title>Error</title></head></html>");

        // Textual payloads are legitimate and must be left alone, whatever
        // they look like: Adminer is a .php file, Composer a .phar, pip's
        // bootstrap a .py, and a stray note a .txt.
        let php = dir.join("adminer.php");
        write(&php, b"<?php echo 1;");
        let phar = dir.join("composer.phar");
        write(&phar, b"#!/usr/bin/env php\n<?php");
        let py = dir.join("get-pip.py");
        write(&py, b"<html>even html here is fine</html>");
        let txt = dir.join("notes.txt");
        write(&txt, b"<html>");
        // A subdirectory is not a cache entry.
        fs::create_dir_all(dir.join("partial")).expect("failed to create subdirectory");

        let removed = cache.purge_stale().expect("purge must succeed");
        assert_eq!(
            removed,
            vec![
                "mysql.exe".to_owned(),
                "node.zip".to_owned(),
                "php.zip".to_owned(),
            ],
            "removal is reported in directory order"
        );

        assert!(good.exists(), "a real zip must survive");
        assert!(php.exists(), "adminer.php must survive");
        assert!(phar.exists(), "composer.phar must survive");
        assert!(py.exists(), "get-pip.py must survive");
        assert!(txt.exists(), "notes.txt must survive");
        assert!(dir.join("partial").is_dir(), "a directory must survive");
        assert!(!poisoned.exists(), "an HTML page named .zip must go");
        assert!(!empty.exists(), "an empty file must go");
        assert!(!bad_exe.exists(), "an HTML page named .exe must go");
    }

    #[test]
    fn ensure_misses_when_absent() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(ZIP.to_vec()));
        assert!(
            !cache.ensure("php.zip").expect("ensure must succeed"),
            "an absent file is a download instruction"
        );
        assert!(
            cache.dir().is_dir(),
            "ensure creates the cache directory on the way"
        );
    }

    #[test]
    fn ensure_validates_legacy_files_instead_of_trusting_them() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(ZIP.to_vec()));
        fs::create_dir_all(cache.dir()).expect("failed to create the cache directory");

        // Nothing recorded this file, so it is checked by content: a poisoned
        // zip is a miss, and does not become adopted on the way.
        let poisoned = cache.path("php.zip");
        write(&poisoned, b"<!DOCTYPE html><html>gone</html>");
        assert!(!cache.ensure("php.zip").expect("ensure must succeed"));
        assert!(
            !cache.adopted().any(|name| name == "php.zip"),
            "a poisoned file must not be adopted"
        );

        // A clean leftover is validated cheaply and adopted, so it is not
        // downloaded again.
        let clean = cache.path("apache.zip");
        write(&clean, ZIP);
        assert!(cache.ensure("apache.zip").expect("ensure must succeed"));
        assert!(cache.adopted().any(|name| name == "apache.zip"));

        // An empty file is not usable even though it exists.
        let empty = cache.path("node.zip");
        write(&empty, b"");
        assert!(!cache.ensure("node.zip").expect("ensure must succeed"));

        // A directory in the cache's place is not usable either.
        fs::create_dir_all(cache.path("redis.zip")).expect("failed to create directory");
        assert!(!cache.ensure("redis.zip").expect("ensure must succeed"));
    }

    #[test]
    fn adoption_round_trip() {
        let temp = TempDir::new();
        let log = nop_log();

        {
            let downloader = FixtureDownloader::new(ZIP.to_vec());
            let mut cache = DownloadCache::new(temp.path(), Arc::clone(&log), Box::new(downloader));
            cache
                .save("php.zip", "https://example.com/php.zip", &nop_progress())
                .expect("save must succeed");
        }

        // A second cache over the same directory must find the adoption record
        // and answer "hit" without inspecting the file's contents.
        let downloader = FixtureDownloader::new(b"not a zip at all".to_vec());
        let calls = downloader.calls();
        let mut cache = DownloadCache::new(temp.path(), log, Box::new(downloader));
        assert!(cache.ensure("php.zip").expect("ensure must succeed"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "a hit must not download");

        let sidecar = cache.dir().join(SIDECAR_FILE);
        assert!(sidecar.is_file(), "the sidecar must persist the adoption");
        let recorded = fs::read_to_string(&sidecar).expect("failed to read the sidecar");
        assert_eq!(recorded, "php.zip\n");
    }

    #[test]
    fn purge_stale_on_a_missing_directory_is_ok() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(Vec::new()));
        assert_eq!(
            cache
                .purge_stale()
                .expect("a missing cache is not an error"),
            Vec::<String>::new()
        );
        assert!(cache.sweep().is_empty());
    }

    #[test]
    fn fetch_uses_the_cache_after_the_first_download() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let downloader = FixtureDownloader::new(ZIP.to_vec());
        let calls = downloader.calls();
        let mut cache = DownloadCache::new(temp.path(), log, Box::new(downloader));

        let first = cache
            .fetch("php.zip", "https://example.com/php.zip", &nop_progress())
            .expect("fetch must succeed");
        assert_eq!(first, cache.path("php.zip"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(exists(&first));

        let second = cache
            .fetch("php.zip", "https://example.com/php.zip", &nop_progress())
            .expect("fetch must succeed");
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second fetch is a hit");

        let lines = lines.lock().expect("log lock").clone();
        assert_eq!(lines, vec!["  using cached php.zip".to_owned()]);
    }

    #[test]
    fn save_removes_a_failed_download() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FailingDownloader);

        let error = cache
            .save("php.zip", "https://example.com/php.zip", &nop_progress())
            .expect_err("a failed download must be an error");
        assert!(matches!(error, Error::Download { .. }));
        assert!(
            !exists(&cache.path("php.zip")),
            "nothing may be left behind"
        );
        assert!(!cache.adopted().any(|name| name == "php.zip"));
    }

    #[test]
    fn save_rejects_an_empty_download() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(Vec::new()));

        let error = cache
            .save("php.zip", "https://example.com/php.zip", &nop_progress())
            .expect_err("an empty download must be an error");
        assert!(error.to_string().contains("downloaded php.zip is empty"));
        assert!(!exists(&cache.path("php.zip")));
    }

    #[test]
    fn save_reports_bytes_through_the_progress_callback() {
        let temp = TempDir::new();
        let downloader = FixtureDownloader::reporting(ZIP.to_vec(), 5, 10);
        let mut cache = cache(&temp, downloader);

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let progress: ProgressFn = Arc::new(move |stage, name, done, total| {
            sink.lock()
                .expect("progress lock")
                .push((stage, name.to_owned(), done, total));
        });

        cache
            .save("php.zip", "https://example.com/php.zip", &progress)
            .expect("save must succeed");

        let seen = seen.lock().expect("progress lock").clone();
        assert_eq!(
            seen,
            vec![(Stage::Downloading, "php.zip".to_owned(), 5, 10)]
        );
    }

    #[test]
    fn the_legacy_sidecar_is_cleared_and_its_names_are_not_trusted() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(ZIP.to_vec()));
        fs::create_dir_all(cache.dir()).expect("failed to create the cache directory");

        // An installation upgraded in place: the previous implementation
        // recorded a poisoned file as verified.
        let poisoned = cache.path("php.zip");
        write(&poisoned, b"<html><body>404</body></html>");
        write(
            &cache.dir().join(LEGACY_SIDECAR_FILE),
            b"php.zip\nnode.zip\n",
        );

        assert!(
            !cache.ensure("php.zip").expect("ensure must succeed"),
            "another program's adoption record is not proof"
        );
        assert!(
            cache.path("php.zip").exists(),
            "reporting a file as absent is not deleting it - the sweep does that"
        );
        assert_eq!(
            cache.purge_stale().expect("the sweep must succeed"),
            vec!["php.zip".to_owned()],
            "and the next sweep collects it"
        );
        assert!(
            !cache.dir().join(LEGACY_SIDECAR_FILE).exists(),
            "the legacy sidecar is cleared"
        );
    }

    #[test]
    fn sweep_heals_a_poisoned_cache_and_says_so() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let mut cache = DownloadCache::new(temp.path(), log, Box::new(FailingDownloader));
        fs::create_dir_all(cache.dir()).expect("failed to create the cache directory");
        // Saved under a `.zip` name but the bytes are a page: the extension's
        // own rule is the one that names the reason, exactly as the original
        // ordered its checks.
        write(&cache.path("php.zip"), b"<!DOCTYPE html><html>nope</html>");

        let removed = cache.sweep();
        assert_eq!(removed, vec!["php.zip".to_owned()]);

        let lines = lines.lock().expect("log lock").clone();
        assert_eq!(
            lines,
            vec![
                "  removed stale cached php.zip — not a zip (truncated or error page saved under a zip name)"
                    .to_owned(),
                "downloads cache: cleared 1 stale file(s) — they will be fetched again".to_owned(),
            ]
        );
    }

    #[test]
    fn sweep_says_nothing_when_there_is_nothing_to_clear() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let mut cache = DownloadCache::new(temp.path(), log, Box::new(FailingDownloader));
        fs::create_dir_all(cache.dir()).expect("failed to create the cache directory");
        write(&cache.path("php.zip"), ZIP);

        assert!(cache.sweep().is_empty());
        assert!(lines.lock().expect("log lock").is_empty());
    }

    #[test]
    fn a_sweep_collects_a_file_replaced_behind_the_cache() {
        let temp = TempDir::new();
        let mut cache = cache(&temp, FixtureDownloader::new(ZIP.to_vec()));
        cache
            .save("php.zip", "https://example.com/php.zip", &nop_progress())
            .expect("save must succeed");
        assert!(cache.ensure("php.zip").expect("ensure must succeed"));

        // The file is replaced behind the cache's back, the way a partial
        // download or a user edit would. The sweep inspects what is on disk -
        // it knows nothing about the in-memory adoption set - so the poisoned
        // file is collected even though this process adopted the name a moment
        // ago, and the next install downloads it again instead of extracting
        // an error page.
        write(&cache.path("php.zip"), b"<html>gone</html>");
        cache.sweep();
        assert!(!cache.path("php.zip").exists());
    }

    #[test]
    fn stale_reason_reports_the_documented_cases() {
        let temp = TempDir::new();
        let dir = temp.path();

        // Missing and unreadable files are not "provably broken".
        assert_eq!(stale_reason(&dir.join("absent.zip"), "absent.zip"), None);

        let empty = dir.join("empty.zip");
        write(&empty, b"");
        assert_eq!(stale_reason(&empty, "empty.zip"), Some("file is empty"));

        // Zero size is rejected whatever the extension, because an empty file
        // can never be a usable download.
        let empty_php = dir.join("empty.php");
        write(&empty_php, b"");
        assert_eq!(stale_reason(&empty_php, "empty.php"), Some("file is empty"));

        let truncated = dir.join("truncated.zip");
        write(&truncated, b"PK");
        assert_eq!(
            stale_reason(&truncated, "truncated.zip"),
            Some("not a zip (truncated or error page saved under a zip name)")
        );

        let upper = dir.join("SHOUTY.ZIP");
        write(&upper, b"PK\x03\x04data");
        assert_eq!(stale_reason(&upper, "SHOUTY.ZIP"), None, "case-insensitive");

        // An `.exe` is not checked for zip magic, only for HTML.
        let installer = dir.join("mysql.exe");
        write(&installer, b"MZ\x90\x00binary");
        assert_eq!(stale_reason(&installer, "mysql.exe"), None);

        // Directories are never stale.
        let sub = dir.join("partial.zip");
        fs::create_dir_all(&sub).expect("failed to create directory");
        assert_eq!(stale_reason(&sub, "partial.zip"), None);
    }

    #[test]
    fn zip_magic_accepts_only_real_zip_signatures() {
        assert!(has_zip_magic(b"PK\x03\x04"));
        assert!(has_zip_magic(b"PK\x05\x06"));
        assert!(has_zip_magic(b"PK\x07\x08"));
        assert!(has_zip_magic(b"PK\x03\x04rest of the archive"));

        assert!(!has_zip_magic(b"PK"));
        assert!(!has_zip_magic(b"PK\x03\x05"));
        assert!(!has_zip_magic(b"PK\x01\x04"));
        assert!(!has_zip_magic(b"pk\x03\x04"), "the signature is upper case");
        assert!(!has_zip_magic(b"<html>"));
        assert!(!has_zip_magic(b""));
    }

    #[test]
    fn html_detection_uses_a_case_insensitive_prefix() {
        for marker in [
            "<!DOCTYPE html>",
            "<html lang=\"en\">",
            "  \r\n<html>",
            "<?xml version=\"1.0\"?>",
            "<head><title>x</title></head>",
            "<body>oops</body>",
            "<HTML><BODY>oops</BODY></HTML>",
        ] {
            assert!(is_html(marker.as_bytes()), "`{marker}` is HTML");
        }

        // An executable's `MZ` header. It is a byte string rather than a
        // `&str` because `\x90` is not a byte any `&str` can hold, and the
        // check is byte-oriented for exactly that reason.
        assert!(!is_html(b"MZ\x90\x00"));

        for not_html in [
            "PK\x03\x04",
            "<?phpecho 1;",
            "<!--- a comment is not html",
            "<htm",
            "",
        ] {
            assert!(!is_html(not_html.as_bytes()), "`{not_html}` is not HTML");
        }

        // Only the first 256 bytes are examined, so a marker further in does
        // not make an archive look like an error page.
        let mut late = vec![b'x'; HTML_WINDOW];
        late.extend_from_slice(b"<html>");
        assert!(!is_html(&late));
    }
}
