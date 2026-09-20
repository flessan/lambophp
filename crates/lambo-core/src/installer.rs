//! Installing a catalogue component: download, unpack, finish, activate.
//!
//! This is the workflow behind a service card's Install button, and behind
//! "switch this component to version X". It composes the pieces rather than
//! reimplementing any of them:
//!
//! ```text
//! catalogue (what to install)
//!    → resolver      (where to get it, when the URL is not fixed)
//!    → download cache (fetch it once, verify what is already there)
//!    → extraction     (unpack it, stripping the wrapper directory)
//!    → mirror         (copy a version into the canonical directory)
//!    → post-install   (patch its configuration, seed its data directory)
//!    → PATH           (make its binaries reachable)
//! ```
//!
//! Ported from the original implementation's `download.go`. The parts that
//! decide *what* happens are
//! pure and tested here; the parts that touch the machine are thin wrappers, so
//! the orchestration can be read as the sequence above.
//!
//! # Behaviour worth stating explicitly
//!
//! * **A component that is already installed is not re-downloaded.** The check
//!   file decides, and its post-install hook still runs - that is what heals a
//!   half-finished install from an earlier version.
//! * **A versioned install is mirrored into the canonical directory.** `bin/php`
//!   is what every other part of the product (and the user's `PATH`) points at;
//!   the version it currently *is* lives in `bin/php-8.3`, and activating a
//!   version copies it over the canonical directory.
//! * **The resolver falls back.** If Apache Lounge cannot be reached, the
//!   bundled URL is used and the reason is logged. An install must not depend on
//!   a page that answers differently every week.
//! * **Progress is reported for every stage**, including the ones with nothing
//!   to measure: the dashboard's bar moves from `starting` through `downloading`
//!   and `extracting` to `done`, then back to `idle` after the original's
//!   500 ms pause, which is what lets the UI show a finished state before it
//!   clears.
//!
//! # A deliberate deviation
//!
//! **Installs are serialised per process, not by a global mutex.** The original
//! held one process-wide mutex for the whole function, because two concurrent
//! installs into the same directory would interleave. Here the installer is
//! owned (`&mut self`), so a single installer cannot run two installs at once by
//! construction, and the front end that wants concurrency decides how to get it
//! instead of having a lock silently serialise its UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::archive;
use crate::catalog_panel::{self, Component, InstallPlan, Kind};
use crate::download::{self, ProgressFn, Stage};
use crate::download_cache::DownloadCache;
use crate::error::{Error, Result};
use crate::fsx;
use crate::logs::LogFn;
use crate::platform::Os;
use crate::postinstall::{self, HookContext};
use crate::process::{self, ProcessSpec, combined_output, describe_exit};
use crate::vendor::{self, Resolved};

/// How long a finished install rests before the progress bar is cleared.
///
/// The original slept 500 ms between `done` and `idle` so the dashboard showed
/// a completed bar before returning to idle; without it the bar flickered
/// straight back and the install looked like it had failed.
const DONE_PAUSE: Duration = Duration::from_millis(500);

/// Refreshes the user's `PATH` after an install, reporting how many
/// directories were added.
///
/// A boxed function rather than a call into [`crate::pathenv`] directly, so the
/// install workflow can be exercised end to end in a test that has no business
/// rewriting the `PATH` of the machine running it. The product attaches
/// [`Installer::with_system_path_refresh`]; an installer without one simply does
/// not update `PATH`, and says nothing about it rather than logging work it did
/// not do.
pub type PathRefresher = Arc<dyn Fn() -> Result<usize> + Send + Sync>;

/// The outcome of one install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The component's name.
    pub name: String,
    /// The version that was installed, as displayed.
    pub version: String,
    /// Where it was installed.
    pub install_dir: PathBuf,
    /// Whether it had already been installed, so nothing was downloaded.
    pub already_present: bool,
}

/// Runs installs against one installation root.
pub struct Installer {
    base_dir: PathBuf,
    log: LogFn,
    cache: DownloadCache,
    downloader: Box<dyn download::Downloader + Send + Sync>,
    path_refresher: Option<PathRefresher>,
    os: Os,
}

impl Installer {
    /// Builds an installer for an installation root.
    ///
    /// The transport is separate from the cache because the cache *holds* one:
    /// the downloader is also what the post-install hooks use for the files
    /// they fetch outside the cache (`get-pip.py`).
    pub fn new(
        base_dir: impl Into<PathBuf>,
        log: LogFn,
        cache: DownloadCache,
        downloader: Box<dyn download::Downloader + Send + Sync>,
    ) -> Self {
        Self {
            base_dir: base_dir.into(),
            log,
            cache,
            downloader,
            path_refresher: None,
            os: Os::host(),
        }
    }

    /// Sets the `PATH` refresher, run after a successful install.
    pub fn with_path_refresher(mut self, refresher: PathRefresher) -> Self {
        self.path_refresher = Some(refresher);
        self
    }

    /// Sets the refresher the product uses, where the platform has one.
    ///
    /// Windows keeps the user `PATH` in the registry and tells running shells
    /// about a change; every other platform has no equivalent step, so this
    /// leaves the installer as it was rather than pretending to update
    /// something that does not exist.
    // `mut` is only needed where there is a refresher to install, and only
    // Windows has one.
    #[cfg_attr(not(windows), allow(unused_mut))]
    pub fn with_system_path_refresh(mut self) -> Self {
        #[cfg(windows)]
        {
            self.path_refresher = Some(crate::pathenv::refresher(self.base_dir.clone()));
        }
        self
    }

    /// The installation root.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// The download cache, for a caller that wants to sweep it.
    pub fn cache_mut(&mut self) -> &mut DownloadCache {
        &mut self.cache
    }

    /// Installs a component's default version.
    pub fn install(&mut self, name: &str, progress: &ProgressFn) -> Result<Installed> {
        self.install_version(name, "", progress)
    }

    /// Installs one version of a component.
    ///
    /// An empty version means "the component's default", which is the only
    /// choice available for the twenty components that have one build.
    pub fn install_version(
        &mut self,
        name: &str,
        version: &str,
        progress: &ProgressFn,
    ) -> Result<Installed> {
        let component = catalog_panel::find(name).ok_or_else(|| {
            Error::InvalidInput(format!("no download info registered for {name:?}"))
        })?;

        // A variant that is not in the catalogue is refused before anything is
        // downloaded, which is what the original did.
        if component.is_multi_version() && !version.is_empty() {
            component.variant(version)?;
        }

        let resolved = self.resolve(component);
        let plan = catalog_panel::plan(component, version, &self.base_dir, resolved.as_ref())?;

        // Already installed: run the hook again (that is the self-heal path)
        // and mirror the canonical directory, but download nothing.
        if plan.is_installed() {
            (self.log)(&format!(
                "[{}] {} already installed at {}",
                plan.name,
                plan.version,
                plan.install_dir.display()
            ));
            if plan.is_versioned() {
                self.mirror(&plan)?;
            }
            // The hook runs again on purpose: it is what heals a half-finished
            // install from an earlier version. Nothing else does.
            self.run_hook(&plan)?;
            return Ok(Installed {
                name: plan.name.clone(),
                version: plan.version.clone(),
                install_dir: plan.install_dir.clone(),
                already_present: true,
            });
        }

        progress(Stage::Starting, &plan.name, 0, 0);
        (self.log)(&format!(
            "[{}] {} — starting install",
            plan.name, plan.version
        ));
        if !plan.notes.is_empty() {
            (self.log)(&format!("  note: {}", plan.notes));
        }

        let downloaded = match self.cache.fetch(&plan.file_name, &plan.url, progress) {
            Ok(path) => path,
            Err(error) => {
                progress(Stage::Idle, &plan.name, 0, 0);
                return Err(Error::InvalidInput(format!("download: {error}")));
            }
        };

        if let Err(error) = ensure_install_dir(&plan.install_dir) {
            progress(Stage::Idle, &plan.name, 0, 0);
            return Err(Error::InvalidInput(format!("create install dir: {error}")));
        }

        if let Err(error) = self.unpack(&plan, &downloaded, progress) {
            progress(Stage::Idle, &plan.name, 0, 0);
            return Err(error);
        }

        // A versioned install is mirrored into the canonical directory *before*
        // the hook runs, because the hook works on the canonical directory -
        // `bin/php/php.ini` is where PHP is configured, whatever version the
        // files came from.
        if plan.is_versioned() {
            if let Err(error) = self.mirror(&plan) {
                progress(Stage::Idle, &plan.name, 0, 0);
                return Err(error);
            }
        }

        if let Err(error) = self.run_hook(&plan) {
            progress(Stage::Idle, &plan.name, 0, 0);
            return Err(error);
        }

        (self.log)(&format!("[{}] install complete", plan.name));
        self.refresh_path(&plan.name);
        progress(Stage::Done, &plan.name, 0, 0);
        std::thread::sleep(DONE_PAUSE);
        progress(Stage::Idle, &plan.name, 0, 0);

        Ok(Installed {
            name: plan.name.clone(),
            version: plan.version.clone(),
            install_dir: plan.install_dir.clone(),
            already_present: false,
        })
    }

    /// Switches a multi-version component to another build.
    ///
    /// Both refusals are the original's: an unknown component and a component
    /// with only one build are errors, not silent no-ops.
    pub fn set_active_variant(
        &mut self,
        name: &str,
        version: &str,
        progress: &ProgressFn,
    ) -> Result<Installed> {
        let component = catalog_panel::find(name)
            .ok_or_else(|| Error::InvalidInput(format!("no catalogue entry for {name:?}")))?;
        if !component.is_multi_version() {
            return Err(Error::InvalidInput(format!(
                "[{name}] is not multi-version"
            )));
        }
        self.install_version(name, version, progress)
    }

    /// Unpacks a downloaded artifact into the plan's install directory.
    fn unpack(
        &mut self,
        plan: &InstallPlan,
        downloaded: &Path,
        progress: &ProgressFn,
    ) -> Result<()> {
        match plan.kind {
            Kind::Zip => {
                (self.log)(&format!("  extracting into {}", plan.install_dir.display()));
                progress(Stage::Extracting, &plan.name, 0, 0);

                let name = plan.name.clone();
                let report = move |done: usize, total: usize| {
                    progress(Stage::Extracting, &name, done as i64, total as i64);
                };
                let report: Option<&dyn Fn(usize, usize)> = Some(&report);
                archive::extract_zip_with(
                    downloaded,
                    &plan.install_dir,
                    plan.strip_top.as_deref(),
                    report,
                )
                .map_err(|error| Error::InvalidInput(format!("extract: {error}")))?;
                Ok(())
            }
            Kind::File => {
                let target = plan.install_dir.join(&plan.target_file);
                (self.log)(&format!(
                    "  copying to {}/{}",
                    plan.install_dir.display(),
                    plan.target_file
                ));
                fsx::copy_file(downloaded, &target)
                    .map_err(|error| Error::InvalidInput(format!("copy: {error}")))
            }
            Kind::Exe => self.run_silent_installer(plan, downloaded, progress),
        }
    }

    /// Runs a downloaded installer silently, into the plan's directory.
    fn run_silent_installer(
        &mut self,
        plan: &InstallPlan,
        installer: &Path,
        progress: &ProgressFn,
    ) -> Result<()> {
        (self.log)(&format!(
            "  running silent installer → {} (this may take 30–60s)",
            plan.install_dir.display()
        ));
        progress(Stage::PostInstall, &plan.name, 0, 0);

        // `/S` is NSIS's silent flag and `/D=` its target. `/D` must be last
        // and unquoted, which is why it is not passed through the same path as
        // a normal argument.
        let absolute =
            std::path::absolute(&plan.install_dir).unwrap_or_else(|_| plan.install_dir.clone());
        let spec = ProcessSpec::new(installer, "installer")
            .arg("/S")
            .arg(format!("/D={}", absolute.display()));
        let output = process::run(&spec, self.os)?;
        if !output.status.success() {
            (self.log)(&format!("  installer output: {}", combined_output(&output)));
            return Err(Error::InvalidInput(format!(
                "silent installer: {}",
                describe_exit(&output)
            )));
        }
        (self.log)("  installer finished");
        Ok(())
    }

    /// Runs the post-install hook, on the canonical directory.
    fn run_hook(&mut self, plan: &InstallPlan) -> Result<()> {
        let Some(hook) = plan.hook else {
            return Ok(());
        };
        let mut context = HookContext {
            base_dir: &self.base_dir,
            log: &self.log,
            downloader: self.downloader.as_ref(),
            cache: &mut self.cache,
        };
        // The hook always works on the canonical directory - see
        // `install_version`.
        postinstall::run(hook, &plan.canonical_dir, &mut context)
            .map_err(|error| Error::InvalidInput(format!("post-install: {error}")))
    }

    /// Reflects the installed components into the user's `PATH`.
    ///
    /// A `PATH` that cannot be updated is not an install failure, and the
    /// original said so in the same words.
    fn refresh_path(&mut self, name: &str) {
        let Some(refresher) = &self.path_refresher else {
            return;
        };
        match refresher() {
            Ok(count) if count > 0 => (self.log)(&format!(
                "[{name}] added {count} bin dir(s) to user PATH — open a new terminal to use them"
            )),
            Ok(_) => {}
            Err(error) => (self.log)(&format!("[{name}] PATH update skipped: {error}")),
        }
    }

    /// Asks the resolver for a component whose URL is not fixed.
    ///
    /// A failure is logged and reported as `None`, which makes the caller fall
    /// back to the bundled URL - the behaviour the original had, and the reason
    /// a stale index page does not break an install.
    fn resolve(&self, component: &Component) -> Option<Resolved> {
        let resolver = component.resolver?;
        let outcome = match resolver {
            catalog_panel::Resolver::ApacheLatest => {
                vendor::resolve_apache_latest(self.downloader.as_ref(), &self.log)
            }
            catalog_panel::Resolver::ZigLatest => {
                vendor::resolve_zig_latest(self.downloader.as_ref(), &self.log)
            }
        };
        match outcome {
            Ok(resolved) => Some(resolved),
            Err(error) => {
                (self.log)(&format!(
                    "[{}] version resolve failed ({error}), falling back to bundled URL",
                    component.name
                ));
                None
            }
        }
    }

    /// Copies an installed version over the component's canonical directory.
    ///
    /// The error is passed through unwrapped, as the original did, so the
    /// caller reads the failing command rather than a description of it.
    fn mirror(&mut self, plan: &InstallPlan) -> Result<()> {
        point_junction(&plan.canonical_dir, &plan.install_dir, &self.log, self.os)
    }
}

/// Mirrors `target` into `canonical`, replacing whatever was there.
///
/// The original called this "pointJunction" and used `robocopy /MIR`, which is
/// a copy rather than a junction; the name and the log line are its own, and
/// the behaviour - the canonical directory ends up holding a copy of the
/// active version - is what the rest of the product depends on.
///
/// A reparse point left behind by an older version is removed first, because
/// writing through it would modify the tree it points at instead of the
/// canonical directory. (`robocopy` itself would refuse, or worse, follow it.)
///
/// `robocopy`'s exit codes 0–7 are all success: they distinguish "copied",
/// "nothing to do" and "extra files removed" from a genuine failure, and the
/// original treated them that way.
pub fn point_junction(canonical: &Path, target: &Path, log: &LogFn, os: Os) -> Result<()> {
    // A junction, a symlink and an unknown reparse point are all "replace it";
    // a plain directory is left alone and a plain file is an error, which is
    // what `ensure_install_dir` below reports.
    if let Ok(metadata) = std::fs::symlink_metadata(canonical) {
        if metadata.file_type().is_symlink() {
            std::fs::remove_file(canonical)
                .map_err(|source| Error::InvalidInput(format!("remove old junction: {source}")))?;
            log(&format!(
                "  removed legacy junction at {}",
                canonical.display()
            ));
        }
    }

    ensure_install_dir(canonical)
        .map_err(|error| Error::InvalidInput(format!("create canonical dir: {error}")))?;

    let spec = ProcessSpec::new("robocopy", "robocopy")
        .arg(target.display().to_string())
        .arg(canonical.display().to_string())
        .args([
            "/MIR", "/NJH", "/NJS", "/NFL", "/NDL", "/NP", "/R:1", "/W:1",
        ]);
    let output = process::run(&spec, os)?;
    let code = output.status.code().unwrap_or(-1);
    if !output.status.success() && !(0..8).contains(&code) {
        return Err(Error::InvalidInput(format!(
            "robocopy mirror {} → {}: {}: {}",
            target.display(),
            canonical.display(),
            describe_exit(&output),
            combined_output(&output)
        )));
    }
    log(&format!(
        "  active version → {}",
        target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    Ok(())
}

/// Whether two paths name the same location, as the original decided it.
///
/// The comparison is textual, not filesystem-based: `filepath.Clean` on both
/// sides, then a case-insensitive compare. It deliberately does *not* resolve
/// `..` against the disk, so `bin/php/extra/../php.ini` and `bin/php/php.ini`
/// are the same path even when `extra/` does not exist - which is what lets the
/// check run before anything has been created.
pub fn same_path(a: &Path, b: &Path) -> bool {
    clean_path(a) == clean_path(b)
}

/// A path in the canonical form [`same_path`] compares: forward slashes, no
/// empty, `.` or resolved `..` components, no trailing separator, lower case.
fn clean_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for component in text.split('/') {
        match component {
            "" | "." => continue,
            // A leading `..` has nothing to pop and is kept, as `Clean` does;
            // otherwise dropping it would make `..` and `.` compare equal.
            ".." if parts.is_empty() || parts.last() == Some(&"..") => parts.push(".."),
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/").to_lowercase()
}

/// Creates an install directory, following a reparse point if one is there.
///
/// An install directory that is a junction (or symlink) from an earlier version
/// must be created *through*: creating it in place fails, and the files would
/// land outside the intended tree.
pub fn ensure_install_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            let resolved = std::fs::canonicalize(path).map_err(|source| {
                Error::InvalidInput(format!("resolve junction {}: {source}", path.display()))
            })?;
            return fsx::ensure_dir(&resolved);
        }
        if metadata.is_dir() {
            return Ok(());
        }
        return Err(Error::InvalidInput(format!(
            "{} exists but is not a directory",
            path.display()
        )));
    }
    fsx::ensure_dir(path)
}

/// Whether a component is installed, by catalogue name.
///
/// Re-exported from the catalogue so an installer caller has one place to ask.
pub fn is_installed(name: &str, base_dir: &Path) -> bool {
    catalog_panel::is_installed(name, base_dir)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::download::Downloader;
    use crate::testutil::{TempDir, fixture, write_zip};

    /// Marks a downloaded fixture as a program a test can run.
    ///
    /// Windows runs a `.exe` whatever its permission bits say; a host that
    /// keeps them would refuse the fixture transport's shell script, so the
    /// executable bit is set there - which is what a real download of an
    /// installer ends up with anyway.
    #[cfg(unix)]
    fn make_runnable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        if path.extension().is_some_and(|extension| extension == "exe") {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        }
    }

    /// Nothing to do: an `.exe` is a program by its name alone.
    #[cfg(not(unix))]
    fn make_runnable(_path: &Path) {}

    /// A transport that writes prepared bytes and counts its calls, so a test
    /// can tell an install that downloaded from one that used the cache.
    struct FixtureTransport {
        payload: Vec<u8>,
        calls: Arc<AtomicUsize>,
        reported: bool,
    }

    impl FixtureTransport {
        fn new(payload: Vec<u8>) -> Self {
            Self {
                payload,
                calls: Arc::new(AtomicUsize::new(0)),
                reported: true,
            }
        }

        /// A transport that reported no progress at all - a transport is
        /// allowed to have nothing to say about the bytes in flight.
        fn silent(payload: Vec<u8>) -> Self {
            Self {
                payload,
                calls: Arc::new(AtomicUsize::new(0)),
                reported: false,
            }
        }
    }

    impl Downloader for FixtureTransport {
        fn fetch(&self, _url: &str, destination: &Path) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(parent) = destination.parent() {
                fsx::ensure_dir(parent)?;
            }
            fs::write(destination, &self.payload).map_err(|error| Error::io(destination, error))?;
            make_runnable(destination);
            Ok(())
        }

        fn fetch_with_progress(
            &self,
            url: &str,
            destination: &Path,
            progress: &dyn Fn(i64, i64),
        ) -> Result<()> {
            if self.reported {
                let total = self.payload.len() as i64;
                progress(0, total);
                progress(total, total);
            }
            self.fetch(url, destination)
        }
    }

    /// One progress report: the stage, the file, the bytes done and the total.
    type Report = (Stage, String, i64, i64);

    /// Every report a run made, as the test's sink collected them.
    type Reports = Arc<Mutex<Vec<Report>>>;

    /// A progress sink that records every report it is given.
    fn progress_recorder() -> (ProgressFn, Reports) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&reports);
        let progress: ProgressFn = Arc::new(move |stage, name, done, total| {
            sink.lock()
                .expect("progress lock")
                .push((stage, name.to_owned(), done, total));
        });
        (progress, reports)
    }

    /// A log sink that records everything it is given.
    fn log_recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_owned());
        });
        (log, lines)
    }

    /// The stages reported, in order.
    /// The stage names a run reported, in order, with consecutive repeats
    /// collapsed: a transport and an extractor both report as they work, so
    /// "downloading" twice is one stage, not two.
    fn stage_sequence(reports: &Reports) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for stage in stages(reports) {
            if out.last() != Some(&stage) {
                out.push(stage);
            }
        }
        out
    }

    fn stages(reports: &Reports) -> Vec<&'static str> {
        reports
            .lock()
            .expect("progress lock")
            .iter()
            .map(|(stage, _, _, _)| stage.as_str())
            .collect()
    }

    /// An installer over a temporary root, with a prepared transport.
    fn installer(
        root: &Path,
        transport: impl Downloader + Send + Sync + 'static,
        log: LogFn,
    ) -> (Installer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = CountingTransport {
            inner: Box::new(transport),
            calls: Arc::clone(&calls),
        };
        let cache = DownloadCache::new(root, Arc::clone(&log), Box::new(counted));
        (
            Installer::new(root, log, cache, Box::new(NullTransport)),
            calls,
        )
    }

    /// Counts calls through to another transport.
    struct CountingTransport {
        inner: Box<dyn Downloader + Send + Sync>,
        calls: Arc<AtomicUsize>,
    }

    impl Downloader for CountingTransport {
        fn fetch(&self, url: &str, destination: &Path) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.fetch(url, destination)
        }

        fn fetch_with_progress(
            &self,
            url: &str,
            destination: &Path,
            progress: &dyn Fn(i64, i64),
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.fetch_with_progress(url, destination, progress)
        }
    }

    /// The transport the installer itself uses for post-install downloads.
    /// Every fixture component here has a hook that needs no network.
    struct NullTransport;

    impl Downloader for NullTransport {
        fn fetch(&self, url: &str, _destination: &Path) -> Result<()> {
            Err(Error::Download {
                url: url.to_owned(),
                reason: "no network in tests".to_owned(),
            })
        }
    }

    /// A real ZIP holding one file inside a wrapper directory, as nginx ships.
    fn nginx_archive(temp: &TempDir) -> Vec<u8> {
        let path = temp.join("nginx-1.28.3.zip");
        write_zip(
            &path,
            &[
                ("nginx-1.28.3/", None),
                ("nginx-1.28.3/nginx.exe", Some(b"nginx".as_slice())),
                ("nginx-1.28.3/html/index.html", Some(b"<html>".as_slice())),
            ],
        );
        fs::read(&path).expect("failed to read the fixture archive")
    }

    /// A real ZIP holding an Apache tree: the check file, and nothing else the
    /// httpd.conf patch needs.
    fn apache_archive(temp: &TempDir) -> Vec<u8> {
        let path = temp.join("httpd.zip");
        write_zip(
            &path,
            &[
                ("Apache24/", None),
                ("Apache24/bin/httpd.exe", Some(b"MZ".as_slice())),
                (
                    "Apache24/conf/httpd.conf",
                    Some(b"ServerRoot \"bin/apache\"\n".as_slice()),
                ),
            ],
        );
        fs::read(&path).expect("failed to read the fixture archive")
    }

    #[test]
    fn same_path_ignores_separators_and_a_trailing_slash() {
        assert!(same_path(Path::new("bin/php"), Path::new("bin/php/")));
        assert!(same_path(Path::new("bin\\php"), Path::new("bin/php/")));
        assert!(!same_path(Path::new("bin/php"), Path::new("bin/php83")));
        // Two spellings of the same directory that differ in case are the same
        // directory on Windows and are not on a case-sensitive filesystem; the
        // comparison is textual in both cases.
        #[cfg(windows)]
        assert!(same_path(
            Path::new("C:/TMP/LaMbO/BIN"),
            Path::new("c:/tmp/lambo/bin/")
        ));
    }

    #[test]
    fn same_path_cleans_dots_without_resolving_parents_on_disk() {
        assert!(same_path(
            Path::new("bin/php/./composer.phar"),
            Path::new("bin/php/composer.phar")
        ));
        assert!(same_path(
            Path::new("bin/php/extra/../php.ini"),
            Path::new("bin/php/php.ini")
        ));
        // A leading `..` has nothing to pop and must not vanish: if it did,
        // `..` and `.` would compare equal.
        assert!(!same_path(Path::new(".."), Path::new(".")));
        assert!(same_path(Path::new(".."), Path::new("../")));
    }

    #[test]
    fn ensure_install_dir_creates_missing_directories_and_accepts_existing_ones() {
        let temp = TempDir::new();
        let dir = temp.join("bin/nginx");
        assert!(ensure_install_dir(&dir).is_ok());
        assert!(dir.is_dir());
        // Idempotent: an install over an existing directory is normal.
        assert!(ensure_install_dir(&dir).is_ok());
    }

    #[test]
    fn ensure_install_dir_refuses_a_file() {
        let temp = TempDir::new();
        let path = temp.join("bin/nginx");
        fixture(temp.path(), "bin/nginx", "not a directory");
        let error = ensure_install_dir(&path).expect_err("a file is not an install dir");
        assert!(
            error.to_string().contains("exists but is not a directory"),
            "unexpected message: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_install_dir_follows_a_junction_instead_of_replacing_it() {
        let temp = TempDir::new();
        let real = temp.join("versions/nginx-1.28.3");
        fs::create_dir_all(&real).expect("failed to create the real directory");
        let link = temp.join("bin/nginx");
        fs::create_dir_all(link.parent().expect("link has a parent"))
            .expect("failed to create bin");
        std::os::unix::fs::symlink(&real, &link).expect("failed to create the symlink");

        ensure_install_dir(&link).expect("a junction is created through");

        // The junction is still a junction, and it still points at the tree the
        // files belong in - creating the directory in place would have thrown
        // the version away.
        let metadata = fs::symlink_metadata(&link).expect("the junction must survive");
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            fs::canonicalize(&link).expect("resolvable"),
            fs::canonicalize(&real).expect("resolvable")
        );
    }

    #[test]
    fn point_junction_refuses_a_canonical_path_that_is_a_file() {
        let temp = TempDir::new();
        let canonical = temp.join("bin/nginx");
        fixture(temp.path(), "bin/nginx", "in the way");
        let (log, _) = log_recorder();

        // Errors before the copy is attempted, so this test never needs
        // robocopy - which is the point: a broken canonical path must not be
        // half-mirrored over.
        let error = point_junction(&canonical, &temp.join("versions/nginx"), &log, Os::host())
            .expect_err("a file cannot be mirrored into");
        assert!(
            error.to_string().contains("exists but is not a directory"),
            "unexpected message: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn point_junction_removes_a_legacy_junction_first() {
        let temp = TempDir::new();
        let canonical = temp.join("bin/php");
        let old = temp.join("versions/php-8.2");
        let new = temp.join("versions/php-8.3");
        fs::create_dir_all(&old).expect("failed to create the old version");
        fs::create_dir_all(&new).expect("failed to create the new version");
        fs::create_dir_all(canonical.parent().expect("parent")).expect("failed to create bin");
        std::os::unix::fs::symlink(&old, &canonical).expect("failed to create the symlink");
        let (log, lines) = log_recorder();

        // `robocopy` does not exist here, so the copy itself fails on Unix; the
        // point of the test is what happened before it.
        let _ = point_junction(&canonical, &new, &log, Os::host());

        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded
                .iter()
                .any(|line| line.contains("removed legacy junction at")),
            "the removal must be reported: {recorded:?}"
        );
        // The old target is untouched: a junction is removed, never followed
        // into and never deleted.
        assert!(old.is_dir());
        // And the canonical path is a real directory now, not a link to the
        // version that used to be active.
        let metadata = fs::symlink_metadata(&canonical).expect("the canonical path exists");
        assert!(!metadata.file_type().is_symlink());
    }

    #[test]
    fn is_installed_answers_for_known_and_unknown_components() {
        let temp = TempDir::new();
        // Unknown components report `true` so no Install button is offered for
        // something the catalogue cannot install.
        assert!(is_installed("Swift", temp.path()));
        assert!(!is_installed("Nginx", temp.path()));
        fixture(temp.path(), "bin/nginx/nginx.exe", "MZ");
        assert!(is_installed("Nginx", temp.path()));
    }

    #[test]
    fn install_refuses_an_unknown_component_and_an_unknown_variant_before_downloading() {
        let temp = TempDir::new();
        let (log, _) = log_recorder();
        let (mut installer, calls) = installer(temp.path(), FixtureTransport::new(Vec::new()), log);
        let (progress, reports) = progress_recorder();

        let error = installer
            .install("Swift", &progress)
            .expect_err("Swift is a stub with no download info");
        assert!(
            error.to_string().contains("no download info registered"),
            "unexpected: {error}"
        );

        let error = installer
            .install_version("PHP-FPM", "9.9", &progress)
            .expect_err("9.9 is not in the catalogue");
        assert!(
            error.to_string().contains("not in catalogue"),
            "unexpected: {error}"
        );

        let error = installer
            .set_active_variant("Nginx", "1.28.3", &progress)
            .expect_err("nginx ships one build");
        assert!(
            error.to_string().contains("is not multi-version"),
            "unexpected: {error}"
        );

        assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing may be downloaded");
        assert!(stages(&reports).is_empty(), "refusals report no progress");
    }

    #[test]
    fn install_unpacks_a_zip_reporting_every_stage() {
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let archive = nginx_archive(&temp);
        let bytes = archive.len() as i64;
        let (mut installer, calls) = installer(temp.path(), FixtureTransport::new(archive), log);
        let (progress, reports) = progress_recorder();

        let installed = installer
            .install("Nginx", &progress)
            .expect("nginx installs");

        assert_eq!(installed.name, "Nginx");
        assert_eq!(installed.version, "1.28.3 stable");
        assert_eq!(installed.install_dir, temp.join("bin/nginx"));
        assert!(!installed.already_present);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The wrapper directory is stripped, so nginx.exe is where every other
        // part of the product looks for it.
        assert!(temp.join("bin/nginx/nginx.exe").is_file());
        assert!(temp.join("bin/nginx/html/index.html").is_file());
        assert!(!temp.join("bin/nginx/nginx-1.28.3").exists());

        assert_eq!(
            stage_sequence(&reports),
            vec!["starting", "downloading", "extracting", "done", "idle"]
        );
        // The byte counts come from the transport and the entry counts from the
        // extractor - not from a fixed placeholder.
        let reports = reports.lock().expect("progress lock").clone();
        let counts = |stage: Stage| -> Vec<(i64, i64)> {
            reports
                .iter()
                .filter(|(reported, _, _, _)| *reported == stage)
                .map(|(_, _, done, total)| (*done, *total))
                .collect()
        };
        assert_eq!(counts(Stage::Downloading), vec![(0, bytes), (bytes, bytes)]);
        // One report per entry, before the entry is examined, after the
        // "extracting, nothing measured yet" report the installer opens with.
        assert_eq!(
            counts(Stage::Extracting),
            vec![(0, 0), (1, 3), (2, 3), (3, 3)]
        );

        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded
                .iter()
                .any(|line| line == "[Nginx] 1.28.3 stable — starting install")
        );
        assert!(
            recorded
                .iter()
                .any(|line| line == "[Nginx] install complete")
        );
        assert!(!recorded.iter().any(|line| line.contains("note:")));
    }

    #[test]
    fn a_second_install_uses_the_cache_and_reports_nothing_to_do() {
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let (mut installer, calls) = installer(
            temp.path(),
            FixtureTransport::new(nginx_archive(&temp)),
            log,
        );
        let (progress, _) = progress_recorder();

        installer
            .install("Nginx", &progress)
            .expect("nginx installs");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The check file is what decides, and it is untouched by the second
        // call: the component is already installed.
        let installed = installer
            .install("Nginx", &progress)
            .expect("second call succeeds");
        assert!(installed.already_present);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "nothing may be downloaded again"
        );

        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded
                .iter()
                .any(|line| line.starts_with("[Nginx] 1.28.3 stable already installed at ")),
            "the already-installed line must be reported: {recorded:?}"
        );
    }

    #[test]
    fn a_single_file_component_is_copied_under_its_catalogue_name_and_hooked() {
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let payload = b"<?php // composer".to_vec();
        let (mut installer, calls) = installer(temp.path(), FixtureTransport::new(payload), log);
        let (progress, _) = progress_recorder();

        let installed = installer
            .install("Composer", &progress)
            .expect("composer installs");

        assert_eq!(installed.install_dir, temp.join("bin/php"));
        assert!(!installed.already_present);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The download is `composer-stable.phar`; what PHP needs is
        // `composer.phar`, which is what the catalogue's target file says.
        let phar = temp.join("bin/php/composer.phar");
        assert_eq!(
            fs::read_to_string(&phar).expect("the phar is text"),
            "<?php // composer"
        );

        // The wrapper is the hook's work, and it is written CRLF with `%~dp0`
        // so it resolves the phar beside itself wherever the root is moved to.
        let wrapper = temp.join("bin/php/composer.bat");
        assert_eq!(
            fs::read_to_string(&wrapper).expect("the wrapper is text"),
            "@echo off\r\nphp \"%~dp0composer.phar\" %*\r\n"
        );

        // A wrapper that already exists is the user's, and is never replaced.
        fs::write(&wrapper, "@echo off\r\nrem mine\r\n").expect("failed to replace the wrapper");
        fs::remove_file(&phar).expect("failed to remove the phar");
        installer.install("Composer", &progress).expect("reinstall");
        assert_eq!(
            fs::read_to_string(&wrapper).expect("the wrapper is text"),
            "@echo off\r\nrem mine\r\n"
        );

        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded
                .iter()
                .any(|line| line == "  created composer.bat wrapper")
        );
    }

    #[test]
    fn an_already_installed_component_still_runs_its_hook() {
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let (mut installer, calls) = installer(
            temp.path(),
            FixtureTransport::new(apache_archive(&temp)),
            log,
        );
        let (progress, _) = progress_recorder();

        // Apache's hook is the one that seeds the site Apache serves, so the
        // install is observable outside its own directory.
        let installed = installer
            .install("Apache", &progress)
            .expect("apache installs");
        assert_eq!(installed.install_dir, temp.join("bin/apache"));
        assert!(temp.join("bin/apache/conf/httpd.conf").is_file());
        let welcome = temp.join("www/index.php");
        assert!(welcome.is_file(), "the hook must seed the welcome page");
        let phpinfo = temp.join("www/phpinfo.php");
        assert!(phpinfo.is_file());
        let vhosts = temp.join("conf/apache/vhosts.conf");
        assert!(vhosts.is_file(), "the hook must seed the vhost include");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Now lose the page, as a half-finished install would, and ask again.
        // The check file is still there, so this is the already-installed path:
        // nothing is downloaded, and the hook is what heals the install.
        fs::remove_file(&welcome).expect("failed to remove the welcome page");
        fs::remove_file(&phpinfo).expect("failed to remove the phpinfo shortcut");
        let installed = installer
            .install("Apache", &progress)
            .expect("second call succeeds");

        assert!(installed.already_present);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "nothing may be downloaded again"
        );
        assert!(
            welcome.is_file(),
            "the hook must have recreated the welcome page"
        );
        assert!(phpinfo.is_file());

        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded
                .iter()
                .any(|line| line.starts_with("[Apache] ") && line.contains("already installed at ")),
            "unexpected: {recorded:?}"
        );
        // The resolver cannot be reached here, and the bundled URL is used
        // rather than failing the install.
        assert!(
            recorded
                .iter()
                .any(|line| line.contains("version resolve failed")
                    && line.contains("falling back to bundled URL")),
            "unexpected: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .filter(|line| line.starts_with("  self-heal: wrote welcome page"))
                .count()
                == 2,
            "the hook ran twice: {recorded:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_installer_is_run_with_a_silent_flag_and_an_unquoted_directory() {
        let temp = TempDir::new();
        let (log, _) = log_recorder();
        // The download stands in for RubyInstaller's NSIS executable: it
        // records the arguments it was given, which is the only way to see
        // them, and it proves the process was executed rather than shelled.
        let arguments = temp.join("arguments.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            arguments.display()
        );
        let (mut installer, _) =
            installer(temp.path(), FixtureTransport::new(script.into_bytes()), log);
        let (progress, reports) = progress_recorder();

        let installed = installer.install("Ruby", &progress).expect("ruby installs");

        assert!(!installed.already_present);
        let recorded = fs::read_to_string(&arguments).expect("the installer must have run");
        let recorded: Vec<&str> = recorded.lines().collect();
        assert_eq!(recorded.len(), 2, "exactly two arguments: {recorded:?}");
        assert_eq!(recorded[0], "/S");
        // `/D=` must be last and unquoted, or NSIS reads the quotes as part of
        // the path and installs somewhere unexpected.
        assert!(recorded[1].starts_with("/D="), "unexpected: {recorded:?}");
        assert!(!recorded[1].contains('"'));
        assert!(recorded[1].ends_with("bin/ruby") || recorded[1].ends_with(r"bin\ruby"));
        assert_eq!(
            stage_sequence(&reports),
            vec!["starting", "downloading", "post-install", "done", "idle"]
        );
    }

    #[test]
    fn a_failed_download_leaves_nothing_installed() {
        let temp = TempDir::new();
        let (log, _) = log_recorder();
        struct Refusing;
        impl Downloader for Refusing {
            fn fetch(&self, url: &str, _destination: &Path) -> Result<()> {
                Err(Error::Download {
                    url: url.to_owned(),
                    reason: "connection refused".to_owned(),
                })
            }
        }

        let cache = DownloadCache::new(temp.path(), Arc::clone(&log), Box::new(Refusing));
        let mut installer = Installer::new(temp.path(), log, cache, Box::new(NullTransport));
        let (progress, reports) = progress_recorder();

        let error = installer
            .install("Nginx", &progress)
            .expect_err("the download failed");
        assert!(
            error.to_string().contains("download: "),
            "unexpected: {error}"
        );
        assert!(!temp.join("bin/nginx").exists());
        // A failure returns the bar to idle instead of leaving it mid-install.
        assert_eq!(stages(&reports), vec!["starting", "idle"]);
    }

    #[test]
    fn a_transport_without_progress_still_installs() {
        let temp = TempDir::new();
        let (log, _) = log_recorder();
        let (mut installer, calls) = installer(
            temp.path(),
            FixtureTransport::silent(nginx_archive(&temp)),
            log,
        );
        let (progress, reports) = progress_recorder();

        installer
            .install("Nginx", &progress)
            .expect("a silent transport is a correct transport");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(temp.join("bin/nginx/nginx.exe").is_file());
        // Only the installer's own stage reports are left, and the bar still
        // ends where it should.
        assert_eq!(
            stage_sequence(&reports),
            vec!["starting", "extracting", "done", "idle"]
        );
    }

    #[test]
    fn an_install_reports_paths_that_cannot_be_reached() {
        // The PATH step is a policy the front end supplies. Without one the
        // install is complete and silent about it, which is what the original
        // did when the registry could not be opened: a note, not a failure.
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let payload = b"<?php // composer".to_vec();
        let (mut plain, _) = installer(temp.path(), FixtureTransport::new(payload), log);
        let (progress, _) = progress_recorder();

        plain
            .install("Composer", &progress)
            .expect("composer installs");
        let recorded = lines.lock().expect("log lock").clone();
        assert!(!recorded.iter().any(|line| line.contains("PATH")));

        // With a refresher, the count is reported in the original's words.
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let payload = b"<?php // composer".to_vec();
        let (reachable, _) = installer(temp.path(), FixtureTransport::new(payload), log);
        let count = Arc::new(AtomicUsize::new(2));
        let reported = Arc::clone(&count);
        let mut reachable =
            reachable.with_path_refresher(Arc::new(move || Ok(reported.load(Ordering::SeqCst))));

        reachable
            .install("Composer", &progress)
            .expect("composer installs");
        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded.iter().any(|line| line
                == "[Composer] added 2 bin dir(s) to user PATH — open a new terminal to use them"),
            "unexpected: {recorded:?}"
        );

        // A refresher that fails is logged, not fatal - and says so in the
        // original's words.
        let temp = TempDir::new();
        let (log, lines) = log_recorder();
        let payload = b"<?php // composer".to_vec();
        let (refused, _) = installer(temp.path(), FixtureTransport::new(payload), log);
        let mut refused = refused.with_path_refresher(Arc::new(|| {
            Err(Error::InvalidInput(
                "open HKCU\\Environment: access denied".to_owned(),
            ))
        }));

        refused
            .install("Composer", &progress)
            .expect("the install still succeeded");
        let recorded = lines.lock().expect("log lock").clone();
        assert!(
            recorded.iter().any(|line| line
                == "[Composer] PATH update skipped: open HKCU\\Environment: access denied"),
            "unexpected: {recorded:?}"
        );
    }

    #[test]
    fn base_dir_is_the_root_the_installer_was_built_with() {
        let temp = TempDir::new();
        let (log, _) = log_recorder();
        let (installer, _) = installer(temp.path(), FixtureTransport::new(Vec::new()), log);
        assert_eq!(installer.base_dir(), temp.path());
    }

    #[test]
    fn the_cache_is_reachable_through_the_installer() {
        let temp = TempDir::new();
        let (log, _) = log_recorder();
        let (mut installer, _) = installer(temp.path(), FixtureTransport::new(Vec::new()), log);
        assert_eq!(installer.cache_mut().dir(), temp.join("downloads"));
    }
}
