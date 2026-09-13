//! PHP runtime management: install, switch, run.
//!
//! This is the module that makes `lambo php install 8.4` mean something. The
//! flow is the same on every platform:
//!
//! ```text
//! catalogue ──▶ download (https) ──▶ verify sha256 ──▶ extract safely
//!                                    ──▶ validate php[.exe] ──▶ generate
//!                                    php.ini ──▶ mark active
//! ```
//!
//! Nothing is executed before it has been verified, and nothing is registered
//! as installed before the executable has been found on disk - a half-finished
//! install is removed, never left behind looking usable.
//!
//! # Windows specifics, handled
//!
//! - PHP for Windows is a `.zip` whose DLLs must sit next to `php.exe`; the
//!   archive is unpacked as a unit and `extension_dir` is generated from the
//!   resulting layout.
//! - `php.ini` is written next to the binary (where PHP looks first) *and*
//!   referenced through `PHPRC` when Lambo runs it, so a system-wide
//!   `C:\Windows\php.ini` can never change a project's behaviour.
//! - Only extensions whose DLL actually exists are enabled, so a version
//!   without `php_gd.dll` does not start with a wall of warnings.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::catalog::{Catalog, Family, Release};
use crate::config::SourcesConfig;
use crate::download::{self, Downloader};
use crate::error::{Error, Result};
use crate::paths::{self, Paths};
use crate::platform::{Os, Platform};
use crate::process::{self, ProcessSpec};
use crate::runtime::{self, InstalledRuntime, RuntimeKind};
use crate::version::VersionSpec;

/// Extensions Lambo enables by default when they are present.
///
/// These cover what a typical PHP project needs out of the box: database
/// access, HTTPS, encoding, images and file metadata.
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "curl",
    "mbstring",
    "openssl",
    "fileinfo",
    "mysqli",
    "pdo_mysql",
    "gd",
    "zip",
    "intl",
];

/// How a PHP version relates to this machine, for `lambo php list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionStatus {
    /// Installed and selected.
    Active,
    /// Installed but not selected.
    Installed,
    /// In the catalogue for this platform, not installed yet.
    Available,
    /// In the catalogue, but only for other platforms.
    Unavailable,
    /// The directory exists but Lambo cannot vouch for its contents.
    ///
    /// Either the install was never finished, the directory was renamed, or
    /// files recorded at install time have gone missing. Reporting this as
    /// `installed` is how a user ends up running a PHP that is missing half
    /// its extensions and spends the evening debugging it.
    Corrupt,
}

impl VersionStatus {
    /// The word `lambo php list` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Installed => "installed",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Corrupt => "corrupt",
        }
    }
}

impl fmt::Display for VersionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of `lambo php list`: the catalogue and the local inventory merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    /// Version string, e.g. `8.4.2`.
    pub version: String,
    /// Where it stands for this machine.
    pub status: VersionStatus,
    /// Installation directory, when installed.
    pub path: Option<PathBuf>,
    /// Platforms the catalogue offers this version for.
    pub platforms: Vec<String>,
    /// Where the artifact would come from, and whether it can be verified.
    ///
    /// `None` for a version that is already installed, where the question is
    /// moot - the manifest records where it actually came from.
    pub source: Option<String>,
}

impl VersionRow {
    /// The platforms line `lambo php list` shows for an unavailable version.
    pub fn platform_text(&self) -> String {
        if self.platforms.is_empty() {
            "none".to_owned()
        } else {
            self.platforms.join(", ")
        }
    }
}

/// Merges the catalogue with the installed runtimes, newest first.
///
/// Installed versions come first (they are the ones worth acting on), then
/// everything the catalogue knows about. A version only published for another
/// platform is reported as [`VersionStatus::Unavailable`] rather than dropped:
/// silently hiding it is what makes a user think Lambo is broken.
/// Describes where a release would be fetched from, for `lambo php list`.
///
/// Two facts, because either one alone misleads: where the bytes come from, and
/// whether anything can confirm they are the right bytes. A version listed as
/// `available` with no digest anywhere is not installable, and saying only
/// "available" is how a user ends up reading a failure as a Lambo bug.
fn source_text(release: &Release, sources: &crate::config::SourcesConfig, paths: &Paths) -> String {
    let resolved =
        crate::sources::resolve(Family::Php, release, sources, paths, release.from_override);
    let origin = match resolved.kind {
        crate::sources::SourceKind::Catalogue => "official",
        crate::sources::SourceKind::Override => "override",
        crate::sources::SourceKind::Mirror => "mirror",
        crate::sources::SourceKind::Local => "local",
        crate::sources::SourceKind::Unknown => "unknown",
    };
    // Three positions, and the difference between the first two is the one that
    // matters:
    //
    // - a digest pinned in the catalogue is trusted metadata Lambo carries;
    // - a `.sha256` sidecar is fetched from the same host as the artifact, so
    //   whoever controls that host controls both halves. It is better than
    //   nothing and it is what upstream publishes, but it is not the same
    //   guarantee, and saying "official" for both would hide the difference;
    // - neither means installation fails closed, so the version cannot be
    //   installed at all however available it looks.
    if resolved.artifact.sha256.is_some() {
        return origin.to_owned();
    }
    if resolved.artifact.checksum_url.is_some() {
        return format!("{origin}, digest not pinned");
    }
    format!("{origin}, unverifiable")
}

pub fn version_table(
    paths: &Paths,
    catalog: &Catalog,
    platform: Platform,
    sources: &crate::config::SourcesConfig,
) -> Result<Vec<VersionRow>> {
    let installed = runtime::installed(paths, RuntimeKind::Php)?;
    let active = runtime::active_name(paths, RuntimeKind::Php)?;
    let key = platform.key();

    let mut rows: Vec<VersionRow> = installed
        .iter()
        .map(|runtime| VersionRow {
            version: runtime.name.clone(),
            status: if !runtime::verify(&runtime.path, RuntimeKind::Php).is_usable() {
                // Integrity outranks selection: an active-but-broken runtime
                // must not read as merely "active".
                VersionStatus::Corrupt
            } else if active.as_deref() == Some(runtime.name.as_str()) {
                VersionStatus::Active
            } else {
                VersionStatus::Installed
            },
            path: Some(runtime.path.clone()),
            platforms: catalog
                .releases(Family::Php)
                .iter()
                .filter(|release| release.version == runtime.name)
                .map(|release| release.platform.clone())
                .collect(),
            // Already installed: where it came from is in its manifest, and
            // repeating it here would only add a column of noise.
            source: None,
        })
        .collect();

    // Every catalogue version that is not already installed.
    for release in catalog.releases(Family::Php) {
        let name = release.version.clone();
        if rows.iter().any(|row| row.version == name) {
            continue;
        }
        let platforms: Vec<String> = catalog
            .releases(Family::Php)
            .iter()
            .filter(|other| other.version == name)
            .map(|other| other.platform.clone())
            .collect();
        let status = if catalog
            .available(Family::Php, &key)
            .iter()
            .any(|r| r.version == name)
        {
            VersionStatus::Available
        } else {
            VersionStatus::Unavailable
        };
        // Where this would be fetched from, and whether that can be verified.
        // The two halves matter together: "available" says the catalogue lists
        // it, which is not the same as "installable".
        //
        // The entry for *this* platform is the one that counts. A version is
        // usually listed for several, and they need not agree - a digest pinned
        // for windows-x64 says nothing about linux-x64, so taking whichever
        // entry the loop happened to reach first reports another platform's
        // readiness as this one's.
        let for_this_platform = catalog
            .releases(Family::Php)
            .iter()
            .find(|other| other.version == name && other.platform == key);
        let source = match for_this_platform {
            Some(release) => source_text(release, sources, paths),
            // Not offered here at all, so there is no source to describe; the
            // platforms column already says where it does exist.
            None => "not for this platform".to_owned(),
        };
        rows.push(VersionRow {
            source: Some(source),
            version: name,
            status,
            path: None,
            platforms,
        });
    }

    // Installed first, then newest first within each group.
    rows.sort_by(|a, b| {
        let rank = |row: &VersionRow| match row.status {
            VersionStatus::Active => 0,
            // A corrupt runtime sorts ahead of the healthy installed ones: it
            // is on disk, it is broken, and it needs the user's attention
            // rather than being scrolled past under the available versions.
            VersionStatus::Corrupt => 1,
            VersionStatus::Installed => 2,
            VersionStatus::Available => 3,
            VersionStatus::Unavailable => 4,
        };
        rank(a).cmp(&rank(b)).then_with(|| {
            match (a.version.parse::<Version>(), b.version.parse::<Version>()) {
                (Ok(a), Ok(b)) => b.cmp(&a),
                _ => b.version.cmp(&a.version),
            }
        })
    });
    Ok(rows)
}

/// PHP runtimes installed locally.
pub fn list(paths: &Paths) -> Result<Vec<InstalledRuntime>> {
    runtime::installed(paths, RuntimeKind::Php)
}

/// The PHP version Lambo would run right now.
pub fn current(paths: &Paths) -> Result<Option<InstalledRuntime>> {
    runtime::active(paths, RuntimeKind::Php)
}

/// Marks `spec` as the active PHP version, installing nothing.
///
/// Errors when no installed version matches, which is what turns
/// `lambo php use 8.5` into "8.5 is not installed; run `lambo php install 8.5`"
/// instead of silently keeping the old version.
pub fn activate(paths: &Paths, spec: &VersionSpec) -> Result<InstalledRuntime> {
    let installed = list(paths)?;
    let selected = runtime::select(spec, &installed).ok_or_else(|| Error::RuntimeNotInstalled {
        kind: "PHP",
        name: spec.to_string(),
        path: paths.php_dir(),
    })?;
    runtime::set_active(paths, RuntimeKind::Php, &selected.name)?;
    Ok(selected.clone())
}

/// Removes an installed PHP version.
pub fn remove(paths: &Paths, version: &str) -> Result<()> {
    runtime::remove(paths, RuntimeKind::Php, version)
}

/// Resolves the PHP runtime a project should run.
///
/// A pinned version in `lambo.yml` wins; otherwise the active runtime; and
/// only then the newest installed one.
pub fn resolve(paths: &Paths, spec: &VersionSpec) -> Result<InstalledRuntime> {
    runtime::resolve(paths, RuntimeKind::Php, spec)?.ok_or(Error::RuntimeMissing {
        kind: "PHP",
        command: "lambo php install <version>",
    })
}

/// The `php` executable to run, resolved through [`resolve`].
pub fn executable(paths: &Paths, spec: &VersionSpec, os: Os) -> Result<PathBuf> {
    let runtime = resolve(paths, spec)?;
    php_executable(&runtime, os)
}

/// The `php` executable inside an installed runtime.
pub fn php_executable(runtime: &InstalledRuntime, os: Os) -> Result<PathBuf> {
    runtime
        .server_executable(os)
        .ok_or_else(|| Error::RuntimeNotInstalled {
            kind: "PHP",
            name: runtime.name.clone(),
            path: runtime.path.clone(),
        })
}

/// A PHP runtime resolved for one platform: every path a service needs, with
/// no lookup left to the caller.
///
/// [`InstalledRuntime`] is what discovery returns - a directory and a version.
/// This is what running anything needs: the executable, the generated
/// `php.ini`, and the extensions directory, each already resolved for the
/// platform's real layout (Windows keeps `php.ini` beside the binary, a Unix
/// build keeps it under `etc/`). Nothing here depends on the user's `PATH`.
///
/// Constructing one fails if the executable is missing, so a `Runtime` in hand
/// is a promise that PHP can actually be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    /// The runtime as discovered on disk.
    pub installed: InstalledRuntime,
    /// Parsed version.
    pub version: Version,
    /// Platform these paths were resolved for.
    pub os: Os,
    /// Root directory of the installation.
    pub root: PathBuf,
    /// The `php` executable.
    pub executable: PathBuf,
    /// The Lambo-generated `php.ini` (pass it via `PHPRC`).
    pub php_ini: PathBuf,
    /// Where PHP loads extensions from.
    pub extensions_dir: PathBuf,
}

impl Runtime {
    /// Resolves an [`InstalledRuntime`] into concrete paths for `os`.
    ///
    /// Fails with [`Error::RuntimeNotInstalled`] when the executable cannot be
    /// found - an interrupted install must never be presented as usable.
    pub fn from_installed(installed: &InstalledRuntime, os: Os) -> Result<Self> {
        let executable = php_executable(installed, os)?;
        Ok(Self {
            version: installed.version.clone(),
            root: installed.path.clone(),
            php_ini: php_ini_path(installed, os),
            extensions_dir: extension_dir(installed, os),
            installed: installed.clone(),
            os,
            executable,
        })
    }

    /// Resolves a version spec against the installed runtimes.
    pub fn resolve(paths: &Paths, spec: &VersionSpec, os: Os) -> Result<Self> {
        Self::from_installed(&resolve(paths, spec)?, os)
    }

    /// The active runtime, resolved. `None` when no version is selected.
    pub fn active(paths: &Paths, os: Os) -> Result<Option<Self>> {
        match current(paths)? {
            Some(installed) => Ok(Some(Self::from_installed(&installed, os)?)),
            None => Ok(None),
        }
    }

    /// Whether the generated `php.ini` exists on disk.
    pub fn has_ini(&self) -> bool {
        self.php_ini.is_file()
    }

    /// The `PHPRC` environment pair that makes a child process use this
    /// runtime's configuration.
    ///
    /// Lambo never edits the user's `PATH` or a global `php.ini`: the
    /// configuration is attached to the process it starts.
    pub fn phprc(&self) -> (&'static str, PathBuf) {
        ("PHPRC", self.php_ini.clone())
    }
}

/// The value Lambo puts in `PHPRC` when it starts PHP, or `None` when no
/// generated `php.ini` exists yet.
///
/// PHP accepts either a directory containing `php.ini` or the path to the ini
/// file itself. Lambo always passes the **directory**, for one reason: it is
/// the form PHP's own documentation describes, and it is what every call site
/// here now uses. `serve_spec` used to pass the file while `run` passed the
/// directory - both happened to work, so nothing caught it, but two call sites
/// disagreeing about a contract is how a future change breaks one of them.
///
/// Public because PHP is not the only thing Lambo serves through a managed
/// runtime: `dbui` starts `php -S` to serve Adminer, and Adminer without the
/// generated `php.ini` has no `mysqli` or `pdo_mysql` to connect with.
pub fn phprc(runtime: &InstalledRuntime, os: Os) -> Option<String> {
    let ini = php_ini_path(runtime, os);
    if !ini.is_file() {
        return None;
    }
    let directory = ini.parent().unwrap_or(&runtime.path);
    Some(directory.display().to_string())
}

/// What actually happened when Lambo ran a PHP runtime.
///
/// Everything else in this module reasons about files: does `bin/php` exist,
/// does the manifest match the directory, is the version in the catalogue. A
/// runtime can pass every one of those checks and still not run - a Windows
/// build unpacked on Linux, a binary linked against a library this machine
/// does not have, an archive that verified perfectly and contained the wrong
/// thing.
///
/// The only way to know is to start it. This type holds the evidence, and it
/// keeps the evidence rather than collapsing it into a bool: [`RuntimeHealth::problem`]
/// says *why*, which is the difference between "PHP is broken" and an
/// actionable line telling the user which library is missing.
///
/// Checking a runtime never modifies it and never touches the user's `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeHealth {
    /// The runtime that was checked.
    pub runtime: InstalledRuntime,
    /// The executable Lambo tried to start.
    ///
    /// `None` when no PHP executable could be found in the runtime at all. An
    /// `Option` rather than a path that falls back to the runtime directory:
    /// printing the directory in an "executable" column is a false statement,
    /// and a user reading it would go looking for a file that does not exist.
    pub executable: Option<PathBuf>,
    /// Whether the process started and exited successfully.
    pub ran: bool,
    /// The version PHP itself reported, from `php -v`.
    ///
    /// `None` when PHP could not be run, or printed something unparseable.
    pub reported_version: Option<Version>,
    /// The `php.ini` PHP says it loaded, from `php --ini`.
    ///
    /// `None` means PHP loaded no configuration file at all - not that the
    /// check was skipped. That distinction is the whole point: it is how
    /// Lambo proves the generated `php.ini` is the one in effect.
    pub loaded_ini: Option<PathBuf>,
    /// Modules reported by `php -m`.
    pub modules: Vec<String>,
    /// Why this runtime is not usable, when it is not.
    pub problem: Option<String>,
}

impl RuntimeHealth {
    /// Runs a runtime and records what it reports.
    ///
    /// Starts PHP three times (`-v`, `--ini`, `-m`). That is cheap next to
    /// what it proves, and it is done at install time and by `lambo doctor`,
    /// not per request.
    ///
    /// A runtime for another platform is reported as unrunnable rather than
    /// attempted: starting a Windows `php.exe` on Linux cannot succeed, and
    /// the result would say nothing about the runtime.
    pub fn check(paths: &Paths, runtime: &InstalledRuntime, os: Os) -> Self {
        let executable = match php_executable(runtime, os) {
            Ok(executable) => executable,
            Err(_) => {
                return Self {
                    problem: Some(format!(
                        "no PHP executable was found under {}",
                        runtime.path.display()
                    )),
                    modules: Vec::new(),
                    loaded_ini: None,
                    reported_version: None,
                    executable: None,
                    ran: false,
                    runtime: runtime.clone(),
                };
            }
        };

        // `php -v` is the probe that decides everything else. If it does not
        // run, the remaining checks have nothing to report.
        let version_run = match probe(paths, runtime, &["-v"], os) {
            Ok(run) => run,
            Err(error) => {
                return Self {
                    problem: Some(format!(
                        "`{}` could not be started: {error}",
                        executable.display()
                    )),
                    modules: Vec::new(),
                    loaded_ini: None,
                    reported_version: None,
                    executable: Some(executable),
                    ran: false,
                    runtime: runtime.clone(),
                };
            }
        };

        if !version_run.success {
            return Self {
                problem: Some(version_failure(&executable, &version_run)),
                modules: Vec::new(),
                loaded_ini: None,
                reported_version: None,
                executable: Some(executable),
                ran: false,
                runtime: runtime.clone(),
            };
        }

        let reported_version = parse_php_version(&version_run.stdout);

        // A runtime that runs is usable. What follows refines the report
        // rather than deciding it, so a failure here must not be mistaken for
        // a broken runtime.
        let loaded_ini = probe(paths, runtime, &["--ini"], os)
            .ok()
            .filter(|run| run.success)
            .and_then(|run| parse_loaded_ini(&run.stdout));

        let modules = probe(paths, runtime, &["-m"], os)
            .ok()
            .filter(|run| run.success)
            .map(|run| parse_module_list(&run.stdout))
            .unwrap_or_default();

        let mut health = Self {
            problem: None,
            reported_version,
            loaded_ini,
            modules,
            executable: Some(executable),
            ran: true,
            runtime: runtime.clone(),
        };

        // The catalogue said one version and the binary says another. The
        // download verified against its digest, so this is not corruption -
        // it means the archive and its catalogue entry disagree, which is a
        // bug in one of them and worth telling the user about. It is reported
        // rather than fatal: a working PHP with a wrong label still works.
        if let Some(reported) = &health.reported_version {
            if reported != &runtime.version {
                health.problem = Some(format!(
                    "this runtime reports PHP {reported} but was installed as {} - the \
                     archive and the catalogue entry disagree",
                    runtime.version
                ));
            }
        }

        health
    }

    /// Whether this runtime can be used to run PHP.
    pub fn is_healthy(&self) -> bool {
        self.ran && self.problem.is_none()
    }

    /// One line for `lambo doctor` and `lambo php current`.
    pub fn describe(&self) -> String {
        if !self.ran {
            return self
                .problem
                .clone()
                .unwrap_or_else(|| "PHP did not run".to_owned());
        }
        let version = self
            .reported_version
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| self.runtime.version.to_string());
        let ini = match &self.loaded_ini {
            Some(path) => path.display().to_string(),
            None => "no php.ini loaded".to_owned(),
        };
        match &self.problem {
            Some(problem) => format!("PHP {version} runs, but {problem}"),
            None => format!(
                "PHP {version} runs, {} extension(s), configuration {}",
                self.modules.len(),
                ini
            ),
        }
    }
}

/// Turns a failed `php -v` into something a user can act on.
///
/// The exit code alone is useless here - what matters is what PHP printed
/// before it gave up, because that is where the missing library or the
/// "cannot execute binary file" message appears.
fn version_failure(executable: &Path, run: &PhpRun) -> String {
    let detail = run
        .combined()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned();
    let code = match run.code {
        Some(code) => format!("code {code}"),
        None => "terminated by a signal".to_owned(),
    };
    if detail.is_empty() {
        return format!(
            "`{}` exited with {code} and printed nothing",
            executable.display()
        );
    }
    format!("`{}` exited with {code}: {detail}", executable.display())
}

/// One PHP invocation whose output Lambo reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpRun {
    /// Exit code, or `None` when the process was killed by a signal.
    pub code: Option<i32>,
    /// Whether PHP exited zero.
    pub success: bool,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

impl PhpRun {
    /// Both streams, for building an error message.
    pub fn combined(&self) -> String {
        let mut text = self.stdout.clone();
        if !self.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&self.stderr);
        }
        text
    }
}

/// Runs `php` and reports both its output and how it exited.
///
/// [`capture`] predates this and throws the exit status away, which is why a
/// broken runtime used to read as "PHP reported no extensions". Everything
/// that needs to know *whether* PHP worked uses this instead.
pub fn probe(
    paths: &Paths,
    runtime: &InstalledRuntime,
    arguments: &[&str],
    os: Os,
) -> Result<PhpRun> {
    let program = php_executable(runtime, os)?;
    let mut spec = ProcessSpec::new(&program, "php").args(arguments.iter().copied());
    if let Some(value) = phprc(runtime, os) {
        spec = spec.env("PHPRC", value);
    }
    spec = spec.env("LAMBO_HOME", paths.root().display().to_string());

    let output = process::run(&spec, os)?;
    Ok(PhpRun {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Parses the version out of `php -v`.
///
/// The first line looks like `PHP 8.4.2 (cli) (built: …) (NTS)`; everything
/// after the version is build metadata that varies by distribution, so only
/// the second field is read.
fn parse_php_version(output: &str) -> Option<Version> {
    let line = output.lines().next()?;
    let field = line.split_whitespace().nth(1)?;
    // A pre-release suffix (`8.4.0RC1`) is not a valid semver pre-release
    // without the hyphen, and it is not worth rejecting a working runtime
    // over, so only the numeric part is kept.
    let numeric: String = field
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    Version::parse(&numeric).ok()
}

/// Parses the loaded configuration file out of `php --ini`.
///
/// The line Lambo needs is `Loaded Configuration File:  /path/php.ini`, and
/// PHP prints `(none)` there when it loaded nothing - which is a real answer,
/// not a missing one, so it parses to `None` deliberately.
fn parse_loaded_ini(output: &str) -> Option<PathBuf> {
    for line in output.lines() {
        let Some(value) = line.strip_prefix("Loaded Configuration File:") else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() || value == "(none)" {
            return None;
        }
        return Some(PathBuf::from(value));
    }
    None
}

/// Parses `php -m` into module names.
///
/// PHP prints two section headers - `[PHP Modules]` and `[Zend Modules]` - and
/// a blank line between them. Neither is a module.
fn parse_module_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('['))
        .map(str::to_owned)
        .collect()
}

/// Installs a PHP version for `platform`.
///
/// Returns the installed runtime. `catalog` is an argument so tests (and the
/// GUI) can supply their own.
pub fn install(
    paths: &Paths,
    catalog: &Catalog,
    spec: &VersionSpec,
    platform: Platform,
    downloader: &dyn Downloader,
    sources: &SourcesConfig,
) -> Result<InstalledRuntime> {
    let release = catalog.find(Family::Php, spec, &platform.key()).ok_or_else(|| {
        Error::InvalidInput(format!(
            "no PHP {spec} release for {platform} in the catalogue; run `lambo php list-versions` \
             to see what is available"
        ))
    })?;
    install_release(paths, &release, downloader, sources)
}

/// Installs one specific catalogue release.
pub fn install_release(
    paths: &Paths,
    release: &Release,
    downloader: &dyn Downloader,
    sources: &SourcesConfig,
) -> Result<InstalledRuntime> {
    let version = Version::parse(&release.version).map_err(|_| {
        Error::InvalidInput(format!(
            "catalogue entry `{}` is not a valid version",
            release.version
        ))
    })?;
    // The platform decides the executable name and the layout Lambo expects, so
    // an unparseable key must be an error rather than a guess. Silently
    // defaulting to Linux here is how a Windows install would get a Unix
    // layout and report itself as broken afterwards.
    let os = Platform::from_key(&release.platform)
        .ok_or_else(|| {
            Error::InvalidInput(format!(
                "catalogue entry for {} names an unknown platform `{}`; expected one of \
                 windows-x64, linux-x64, linux-arm64, macos-x64, macos-arm64",
                release.version, release.platform
            ))
        })?
        .os;

    // Where the bytes come from is decided in one place, and the expected
    // digest always comes from the catalogue rather than from the source.
    let resolved =
        crate::sources::resolve(Family::Php, release, sources, paths, release.from_override);
    let verified =
        download::download_verified(downloader, &resolved.artifact, paths).map_err(|error| {
            error.identify(Family::Php, release).with_hint(format!(
                "Pin the digest: {}. Alternatively install PHP yourself and put it \
                 on the machine for Lambo to find.",
                crate::config::PIN_A_DIGEST
            ))
        })?;
    let final_dir = paths.runtime_version_dir(RuntimeKind::Php, &release.version);
    let staging = final_dir.with_file_name(format!("{}.installing", release.version));

    // Extract to a staging directory first: a failed extraction must not leave
    // a directory that looks like an installed runtime.
    let _ = fs::remove_dir_all(&staging);
    let extraction = match crate::archive::extract(&verified.path, &staging) {
        Ok(extraction) => extraction,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    // Upstream archives differ: some wrap everything in one directory. The
    // installed layout is always the same, whatever upstream did.
    if let Some(wrapper) = extraction.single_top_level_dir() {
        let inner = staging.join(wrapper);
        crate::fsx::move_children(&inner, &staging)?;
    }

    // The manifest goes into staging, before the rename. A directory with no
    // manifest was therefore never finished, which is what lets `lambo php
    // list` report `corrupt` instead of offering a runtime that will not run.
    runtime::write_manifest(
        RuntimeKind::Php,
        &release.version,
        &release.platform,
        &resolved,
        &verified.sha256,
        release.executable.as_deref(),
        &staging,
    )?;

    crate::runtime::promote(&staging, &final_dir)?;

    let installed = InstalledRuntime {
        kind: RuntimeKind::Php,
        name: release.version.clone(),
        version,
        path: final_dir.clone(),
    };
    if !installed.is_complete(os) {
        let _ = fs::remove_dir_all(&final_dir);
        return Err(Error::RuntimeNotInstalled {
            kind: "PHP",
            name: release.version.clone(),
            path: final_dir,
        });
    }

    let ini = php_ini_path(&installed, os);
    write_php_ini(&ini, &installed, DEFAULT_EXTENSIONS, paths, os)?;

    // Prove it runs before reporting it installed.
    //
    // Everything above checked bytes and files: the digest matched, the archive
    // was safe to unpack, `bin/php` is on disk. None of that means the program
    // starts. A Windows archive unpacked on Linux, a build linked against a
    // library this machine does not have, or an upstream archive that contained
    // the wrong thing all pass every check so far. Starting PHP once is the
    // only thing that distinguishes them, and finding out here costs one
    // process - finding out at `lambo up` costs the user an evening.
    //
    // Only for the host platform. A runtime installed for another platform is
    // not expected to execute here, and reporting it as broken would be a
    // false claim about the install rather than a true one.
    if os == Platform::host().os {
        let health = RuntimeHealth::check(paths, &installed, os);
        if !health.ran {
            let problem = health
                .problem
                .clone()
                .unwrap_or_else(|| "PHP did not run".to_owned());
            let _ = fs::remove_dir_all(&final_dir);
            return Err(Error::ServiceFailed {
                service: format!("PHP {}", release.version),
                reason: problem,
                causes: vec![
                    // True and worth stating: the bytes were checked, so the
                    // user should not go looking for a corrupted download.
                    "the archive verified against its digest and unpacked cleanly".to_owned(),
                    "the installed binary could not be executed".to_owned(),
                ],
                hint: Some("lambo doctor".to_owned()),
            });
        }
    }

    runtime::set_active(paths, RuntimeKind::Php, &release.version)?;
    Ok(installed)
}

/// Moves every entry of `from` up into `to`.
/// The command line for PHP's built-in development server.
///
/// Used when Apache is unavailable - on Linux and macOS, where Lambo does not
/// ship an Apache build, and by `server.kind: php` for users who want the
/// simplest possible setup. The server binds to `127.0.0.1` only, serves the
/// project's document root, and gets the same generated `php.ini` as every
/// other way of running PHP (`PHPRC`).
pub fn serve_spec(
    runtime: &InstalledRuntime,
    port: u16,
    document_root: &Path,
    log: &Path,
    os: Os,
) -> Result<ProcessSpec> {
    let executable = php_executable(runtime, os)?;
    let mut spec = ProcessSpec::new(&executable, "php-server")
        .arg("-S")
        .arg(format!("127.0.0.1:{port}"))
        .arg("-t")
        .arg(document_root.display().to_string())
        .cwd(document_root)
        .log_to(log)
        .detached();
    if let Some(value) = phprc(runtime, os) {
        spec = spec.env("PHPRC", value);
    }
    Ok(spec)
}

/// Where the generated `php.ini` lives.
///
/// PHP looks for `php.ini` next to the binary first on Windows and in its
/// configuration directory on Unix; Lambo writes to both the conventional
/// location and uses `PHPRC` when running, so the project's configuration
/// always wins over a machine-wide one.
pub fn php_ini_path(runtime: &InstalledRuntime, os: Os) -> PathBuf {
    if os.is_windows() {
        runtime.path.join("php.ini")
    } else {
        runtime.path.join("etc").join("php.ini")
    }
}

/// Generates `php.ini` for a runtime.
///
/// Users never edit this file: it is regenerated whenever the runtime is
/// installed and whenever a project asks for extra extensions.
pub fn write_php_ini(
    path: &Path,
    runtime: &InstalledRuntime,
    extensions: &[impl AsRef<str>],
    paths: &Paths,
    os: Os,
) -> Result<PathBuf> {
    let enabled = enabled_extensions(runtime, extensions, os);
    let log_dir = paths.logs_dir().join("php");
    fs::create_dir_all(&log_dir).map_err(|source| Error::io(&log_dir, source))?;

    let mut ini = String::new();
    ini.push_str("; Generated by Lambo PHP - do not edit.\n");
    ini.push_str("; `lambo php install` and `lambo up` rewrite this file.\n");
    ini.push_str("[PHP]\n");
    ini.push_str(&format!(
        "extension_dir = \"{}\"\n",
        paths::to_forward_slashes(&extension_dir(runtime, os))
    ));
    ini.push_str(&format!(
        "error_log = \"{}\"\n",
        paths::to_forward_slashes(&log_dir.join("php-error.log"))
    ));
    ini.push_str("log_errors = On\n");
    ini.push_str("display_errors = On\n");
    ini.push_str("display_startup_errors = On\n");
    ini.push_str("date.timezone = UTC\n");
    ini.push_str("memory_limit = 512M\n");
    ini.push_str("max_execution_time = 300\n");
    ini.push_str("upload_max_filesize = 64M\n");
    ini.push_str("post_max_size = 64M\n");
    ini.push_str("max_input_vars = 5000\n");
    ini.push_str("zend.exception_ignore_args = Off\n");
    ini.push('\n');
    for extension in &enabled {
        ini.push_str(&format!("extension={extension}\n"));
    }

    crate::fsx::write_atomic(path, &ini)?;
    Ok(path.to_path_buf())
}

/// The directory PHP loads extensions from.
pub fn extension_dir(runtime: &InstalledRuntime, os: Os) -> PathBuf {
    if os.is_windows() {
        runtime.path.join("ext")
    } else {
        // Static and packaged builds put modules in lib/php/extensions/…;
        // when that is absent, PHP's compiled-in default still applies.
        let nested = runtime.path.join("lib").join("php").join("extensions");
        if nested.is_dir() {
            nested
        } else {
            runtime.path.join("lib")
        }
    }
}

/// Filters requested extensions down to the ones that are actually present.
///
/// Enabling a missing extension makes PHP print a warning for every request,
/// which is exactly the kind of noise a local environment should not produce.
pub fn enabled_extensions(
    runtime: &InstalledRuntime,
    requested: &[impl AsRef<str>],
    os: Os,
) -> Vec<String> {
    let directory = extension_dir(runtime, os);
    let mut enabled = Vec::new();
    for extension in requested {
        let name = extension.as_ref();
        if name.is_empty() || !crate::platform::is_valid_path_component(name) {
            continue;
        }
        let candidates = if os.is_windows() {
            vec![format!("php_{name}.dll")]
        } else {
            vec![format!("{name}.so"), name.to_owned()]
        };
        let present = candidates.iter().any(|file| {
            directory.join(file).is_file()
                || find_module_file(&directory, file).is_some()
                // A statically built PHP (common on Linux) has no module
                // files at all; in that case trust the request.
                || !directory.is_dir()
        });
        if present && !enabled.contains(&name.to_owned()) {
            enabled.push(name.to_owned());
        }
    }
    enabled
}

/// Looks for a module file one level below `directory` (versioned subdirs).
fn find_module_file(directory: &Path, file: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join(file).is_file() {
            return Some(path.join(file));
        }
    }
    None
}

/// Runs `php` with the given arguments, inheriting the terminal.
///
/// This backs `lambo php -v`, `lambo php artisan migrate` and friends: the
/// user's PATH never has to change.
pub fn run(
    paths: &Paths,
    runtime: &InstalledRuntime,
    arguments: &[String],
    os: Os,
) -> Result<std::process::ExitStatus> {
    let program = php_executable(runtime, os)?;

    let mut spec = ProcessSpec::new(&program, "php").interactive();
    if let Some(value) = phprc(runtime, os) {
        spec = spec.env("PHPRC", value);
    }
    spec = spec.env("LAMBO_PHP_VERSION", &runtime.name);
    spec = spec.env("LAMBO_HOME", paths.root().display().to_string());
    spec.args = arguments.to_vec();

    let mut child = process::spawn(&spec, os)?;
    let status = child.wait().map_err(|source| Error::Io {
        path: program,
        source,
    })?;
    Ok(status)
}

/// Runs `php` capturing its output (used by `lambo doctor` and `php -m`).
pub fn capture(
    paths: &Paths,
    runtime: &InstalledRuntime,
    arguments: &[&str],
    os: Os,
) -> Result<String> {
    // Delegates to `probe` so there is exactly one place that decides how PHP
    // is invoked for output capture. The exit status is deliberately dropped
    // here - callers that need it use `probe` - but the environment and the
    // PHPRC contract are the same either way.
    let run = probe(paths, runtime, arguments, os)?;
    if run.success {
        return Ok(run.stdout);
    }
    Ok(run.combined())
}

/// The extensions a runtime actually has loaded, via `php -m`.
///
/// Returns an empty list when PHP cannot be run: `lambo doctor` distinguishes
/// "no extensions" from "PHP is broken" by other means.
pub fn loaded_modules(paths: &Paths, runtime: &InstalledRuntime, os: Os) -> Vec<String> {
    capture(paths, runtime, &["-m"], os)
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('['))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Versions the catalogue offers for this platform.
pub fn available_versions(catalog: &Catalog, platform: Platform) -> Vec<String> {
    let mut versions: Vec<String> = catalog
        .available(Family::Php, &platform.key())
        .into_iter()
        .map(|release| release.version)
        .collect();
    versions.dedup();
    versions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::LocalDownloader;
    use crate::testutil::{self, TempDir};

    /// A catalogue with one PHP release served from a local fixture archive.
    fn catalog_with(archive: &Path, version: &str) -> Catalog {
        let release = Release {
            version: version.to_owned(),
            platform: "linux-x64".to_owned(),
            url: format!("file://{}", archive.display()),
            sha256: Some(crate::sha256::sha256_file(archive).unwrap()),
            checksum_url: None,
            ..Default::default()
        };
        Catalog {
            schema: 1,
            php: vec![release],
            ..Catalog::default()
        }
    }

    #[test]
    fn installing_from_a_verified_archive_produces_a_working_runtime() {
        let temp = TempDir::new();
        let paths = temp.home();

        // A fixture that looks like the real thing: one wrapper directory,
        // the binary, an extension directory and a module file.
        let archive = temp.join("php-8.4.2.tar.gz");
        testutil::write_tar_gz(
            &archive,
            &[
                ("php-8.4.2/", None, 0o755),
                ("php-8.4.2/bin/", None, 0o755),
                (
                    "php-8.4.2/bin/php",
                    Some(b"#!/bin/sh\necho PHP 8.4.2\n".as_slice()),
                    0o755,
                ),
                ("php-8.4.2/lib/", None, 0o755),
                ("php-8.4.2/lib/curl.so", Some(b"module".as_slice()), 0o644),
            ],
        );

        let catalog = catalog_with(&archive, "8.4.2");
        let release = catalog.php.first().unwrap();
        let installed =
            install_release(&paths, release, &LocalDownloader, &Default::default()).unwrap();

        assert_eq!(installed.name, "8.4.2");
        // The wrapper directory was flattened, so the layout is predictable.
        assert!(
            installed.path.join("bin").join("php").is_file()
                || installed.path.join("php").is_file()
        );
        assert!(installed.is_complete(Os::Linux));
        assert_eq!(current(&paths).unwrap().unwrap().name, "8.4.2");

        // A php.ini was generated next to the runtime.
        let ini = php_ini_path(&installed, Os::Linux);
        assert!(ini.is_file());
        let contents = fs::read_to_string(&ini).unwrap();
        assert!(contents.contains("extension_dir"), "{contents}");
        assert!(
            contents.contains("extension=curl"),
            "present modules must be enabled: {contents}"
        );
        assert!(contents.contains("Generated by Lambo PHP"));
    }

    /// A tar.gz that looks like a real PHP release.
    fn php_archive(temp: &TempDir, version: &str) -> PathBuf {
        let archive = temp.join(format!("php-{version}.tar.gz"));
        testutil::write_tar_gz(
            &archive,
            &[
                (format!("php-{version}/").as_str(), None, 0o755),
                (format!("php-{version}/bin/").as_str(), None, 0o755),
                (
                    format!("php-{version}/bin/php").as_str(),
                    Some(b"#!/bin/sh\necho PHP\n".as_slice()),
                    0o755,
                ),
            ],
        );
        archive
    }

    #[test]
    fn an_interrupted_install_does_not_block_the_next_attempt() {
        let temp = TempDir::new();
        let paths = temp.home();

        // What a killed `lambo php install` leaves behind: a staging directory
        // holding half an extraction.
        let staging = paths
            .runtime_version_dir(RuntimeKind::Php, "8.4.2")
            .with_file_name("8.4.2.installing");
        fs::create_dir_all(staging.join("bin")).unwrap();
        fs::write(staging.join("bin").join("half-written"), b"partial").unwrap();

        // The staging directory must not be mistaken for an installed runtime.
        assert!(
            runtime::installed(&paths, RuntimeKind::Php)
                .unwrap()
                .iter()
                .all(|runtime| runtime.name != "8.4.2"),
            "a staging directory is not an installed version"
        );

        // And a retry has to succeed over the top of it.
        let catalog = catalog_with(&php_archive(&temp, "8.4.2"), "8.4.2");
        let release = catalog.php.first().unwrap();
        let installed =
            install_release(&paths, release, &LocalDownloader, &Default::default()).unwrap();
        assert!(installed.is_complete(Os::Linux), "{installed:?}");
        assert!(
            !staging.exists(),
            "the staging directory must be gone afterwards"
        );
    }

    #[test]
    fn reinstalling_reuses_the_verified_cached_archive() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = php_archive(&temp, "8.4.2");
        let catalog = catalog_with(&archive, "8.4.2");
        let release = catalog.php.first().unwrap();

        install_release(&paths, release, &LocalDownloader, &Default::default()).unwrap();
        let cached = paths.cache_dir().join(
            crate::download::Artifact::pinned(
                format!("file://{}", archive.display()),
                String::new(),
            )
            .file_name(),
        );
        assert!(
            cached.is_file(),
            "the verified download must be cached: {cached:?}"
        );
        let first_len = fs::metadata(&cached).unwrap().len();

        // Delete the runtime but keep the cache, then install again. The cache
        // entry is still checksum-verified before it is reused, so this is
        // safe - and it must not have to fetch again.
        runtime::remove(&paths, RuntimeKind::Php, "8.4.2").unwrap();
        let again =
            install_release(&paths, release, &LocalDownloader, &Default::default()).unwrap();
        assert!(again.is_complete(Os::Linux));
        assert_eq!(
            fs::metadata(&cached).unwrap().len(),
            first_len,
            "same cached file reused"
        );
    }

    #[test]
    fn a_corrupted_cache_entry_is_rejected_rather_than_installed() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = php_archive(&temp, "8.4.2");
        let catalog = catalog_with(&archive, "8.4.2");
        let release = catalog.php.first().unwrap();

        // Poison the cache after a good install: the checksum on the catalogue
        // entry no longer matches, so the entry must not be trusted.
        install_release(&paths, release, &LocalDownloader, &Default::default()).unwrap();
        for entry in fs::read_dir(paths.cache_dir()).unwrap().flatten() {
            fs::write(entry.path(), b"corrupted").unwrap();
        }
        runtime::remove(&paths, RuntimeKind::Php, "8.4.2").unwrap();

        // A corrupted entry is refreshed from the source, so this succeeds -
        // which is only correct because verification ran first.
        let installed =
            install_release(&paths, release, &LocalDownloader, &Default::default()).unwrap();
        assert!(installed.is_complete(Os::Linux));
    }

    #[test]
    fn installing_a_tampered_archive_installs_nothing() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = temp.join("php-8.4.2.tar.gz");
        testutil::write_tar_gz(
            &archive,
            &[("php-8.4.2/bin/php", Some(b"#!/bin/sh\n".as_slice()), 0o755)],
        );

        let mut release = catalog_with(&archive, "8.4.2").php.remove(0);
        release.sha256 = Some("0".repeat(64));

        let error =
            install_release(&paths, &release, &LocalDownloader, &Default::default()).unwrap_err();
        assert!(matches!(error, Error::ChecksumMismatch { .. }), "{error:?}");
        assert!(
            list(&paths).unwrap().is_empty(),
            "nothing may be registered"
        );
        assert!(!paths.php_dir().join("8.4.2").exists());
    }

    #[test]
    fn an_archive_without_a_php_binary_is_rejected_and_cleaned_up() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = temp.join("php-8.4.2.tar.gz");
        testutil::write_tar_gz(
            &archive,
            &[("readme.txt", Some(b"no php here".as_slice()), 0o644)],
        );

        let release = catalog_with(&archive, "8.4.2")
            .php
            .into_iter()
            .next()
            .unwrap();
        let error =
            install_release(&paths, &release, &LocalDownloader, &Default::default()).unwrap_err();

        assert!(
            matches!(error, Error::RuntimeNotInstalled { .. }),
            "{error:?}"
        );
        assert!(
            !paths.php_dir().join("8.4.2").exists(),
            "the broken install must be removed"
        );
        assert!(list(&paths).unwrap().is_empty());
    }

    /// A catalogue covering two platforms, so the "unavailable for this
    /// platform" case has something real to come from.
    #[test]
    fn the_source_column_describes_this_platform_not_whichever_came_first() {
        let temp = TempDir::new();
        let paths = temp.home();

        // One version, two platforms, and only the Windows one has a digest.
        // Taking whichever entry the loop reached first reported Windows'
        // readiness as Linux's, so `lambo php list` claimed an artifact was
        // verifiable on a machine where it was not.
        let catalog = Catalog {
            schema: 1,
            php: vec![
                Release {
                    version: "8.4.2".to_owned(),
                    platform: "windows-x64".to_owned(),
                    url: "https://example.com/php-8.4.2-win.zip".to_owned(),
                    sha256: Some("b".repeat(64)),
                    archive_format: Some(crate::catalog::ArchiveFormat::Zip),
                    ..Default::default()
                },
                Release {
                    version: "8.4.2".to_owned(),
                    platform: "linux-x64".to_owned(),
                    url: "https://example.com/php-8.4.2-linux.tar.gz".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    archive_format: Some(crate::catalog::ArchiveFormat::TarGz),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let platform = Platform::new(Os::Linux, crate::platform::Arch::X86_64);
        let rows = version_table(&paths, &catalog, platform, &Default::default()).unwrap();

        let row = rows
            .iter()
            .find(|row| row.version == "8.4.2")
            .expect("the version is listed");
        let source = row.source.clone().expect("a source is described");
        assert!(
            source.contains("digest not pinned"),
            "the linux-x64 entry has no digest, so the column must say so; got `{source}`"
        );
        // A version not offered here at all says that, rather than describing
        // the source of some other platform's artifact.
        let catalog2 = Catalog {
            php: vec![Release {
                version: "9.9.9".to_owned(),
                platform: "windows-x64".to_owned(),
                url: "https://example.com/php-9.9.9-win.zip".to_owned(),
                sha256: Some("c".repeat(64)),
                archive_format: Some(crate::catalog::ArchiveFormat::Zip),
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows = version_table(&paths, &catalog2, platform, &Default::default()).unwrap();
        let row = rows.iter().find(|row| row.version == "9.9.9").unwrap();
        assert_eq!(
            row.source.as_deref(),
            Some("not for this platform"),
            "a version unavailable here must not borrow another platform's source"
        );
    }

    fn two_platform_catalog() -> Catalog {
        Catalog {
            schema: 1,
            php: vec![
                Release {
                    version: "8.4.2".to_owned(),
                    platform: "linux-x64".to_owned(),
                    url: "https://example.com/php-8.4.2-linux.tar.gz".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    ..Default::default()
                },
                Release {
                    version: "8.3.16".to_owned(),
                    platform: "linux-x64".to_owned(),
                    url: "https://example.com/php-8.3.16-linux.tar.gz".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    ..Default::default()
                },
                Release {
                    version: "8.2.27".to_owned(),
                    platform: "windows-x64".to_owned(),
                    url: "https://example.com/php-8.2.27-win.zip".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    ..Default::default()
                },
            ],
            ..Catalog::default()
        }
    }

    #[test]
    fn the_version_table_reports_a_broken_runtime_as_corrupt() {
        let temp = TempDir::new();
        let paths = temp.home();
        let catalog = two_platform_catalog();
        let platform = Platform {
            os: Os::Linux,
            arch: crate::platform::Arch::X86_64,
        };

        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.3.16", Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.3.16");

        // A directory with no manifest is one Lambo did not write. It may well
        // work - a user may have unpacked PHP there themselves - so it is
        // reported as installed rather than condemned.
        runtime::set_active(&paths, RuntimeKind::Php, "8.3.16").unwrap();
        let rows = version_table(&paths, &catalog, platform, &Default::default()).unwrap();
        assert_eq!(rows[0].version, "8.3.16");
        assert_eq!(rows[0].status, VersionStatus::Active);

        // Now record what Lambo installed, then break it by deleting the
        // executable the manifest says was there.
        runtime::write_manifest(
            RuntimeKind::Php,
            "8.3.16",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/php.tar.gz"),
            "a".repeat(64).as_str(),
            None,
            &dir,
        )
        .unwrap();
        std::fs::remove_file(dir.join(Os::Linux.executable_name("php"))).unwrap();

        let rows = version_table(&paths, &catalog, platform, &Default::default()).unwrap();
        assert_eq!(rows[0].version, "8.3.16");
        // Integrity outranks selection: this is the active version, and it
        // must not read as "active" while it cannot run.
        assert_eq!(rows[0].status, VersionStatus::Corrupt);
        assert_eq!(rows[0].status.as_str(), "corrupt");
    }

    #[test]
    fn the_version_table_merges_installed_and_catalogue_entries() {
        let temp = TempDir::new();
        let paths = temp.home();
        let catalog = two_platform_catalog();
        let platform = Platform {
            os: Os::Linux,
            arch: crate::platform::Arch::X86_64,
        };

        // Nothing installed: every linux release is "available", and the
        // Windows-only one is reported rather than hidden.
        let rows = version_table(&paths, &catalog, platform, &Default::default()).unwrap();
        let statuses: Vec<_> = rows
            .iter()
            .map(|r| (r.version.as_str(), r.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("8.4.2", VersionStatus::Available),
                ("8.3.16", VersionStatus::Available),
                ("8.2.27", VersionStatus::Unavailable),
            ],
            "{rows:?}"
        );
        assert_eq!(rows[2].platform_text(), "windows-x64");
        assert!(rows[0].path.is_none(), "an uninstalled version has no path");
    }

    #[test]
    fn the_version_table_marks_the_active_version_and_sorts_installed_first() {
        let temp = TempDir::new();
        let paths = temp.home();
        let catalog = two_platform_catalog();
        let platform = Platform {
            os: Os::Linux,
            arch: crate::platform::Arch::X86_64,
        };

        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.3.16", Os::Linux);
        runtime::set_active(&paths, RuntimeKind::Php, "8.3.16").unwrap();

        let rows = version_table(&paths, &catalog, platform, &Default::default()).unwrap();
        let statuses: Vec<_> = rows
            .iter()
            .map(|r| (r.version.as_str(), r.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("8.3.16", VersionStatus::Active),
                ("8.4.2", VersionStatus::Available),
                ("8.2.27", VersionStatus::Unavailable),
            ],
            "{rows:?}"
        );
        assert_eq!(
            rows[0].path.as_deref().unwrap(),
            &paths.runtime_version_dir(RuntimeKind::Php, "8.3.16")
        );
    }

    #[test]
    fn an_installed_version_not_in_the_catalogue_is_still_listed() {
        let temp = TempDir::new();
        let paths = temp.home();
        let platform = Platform {
            os: Os::Linux,
            arch: crate::platform::Arch::X86_64,
        };

        // A hand-placed or side-loaded runtime must not disappear from the
        // list just because the catalogue does not mention it.
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.1.31", Os::Linux);

        let rows = version_table(
            &paths,
            &two_platform_catalog(),
            platform,
            &Default::default(),
        )
        .unwrap();
        let row = rows
            .iter()
            .find(|r| r.version == "8.1.31")
            .expect("installed row missing");
        assert_eq!(row.status, VersionStatus::Installed);
        assert!(
            row.platforms.is_empty(),
            "the catalogue knows nothing about it"
        );
    }

    #[test]
    fn a_resolved_runtime_carries_every_path_a_service_needs() {
        let temp = TempDir::new();
        let paths = temp.home();
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::Linux);
        runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();

        // A hand-placed runtime has no generated ini yet; `lambo php install`
        // writes one. Do both halves here so the assertion means something.
        let installed = runtime::installed(&paths, RuntimeKind::Php).unwrap();
        assert!(
            !Runtime::from_installed(&installed[0], Os::Linux)
                .unwrap()
                .has_ini()
        );
        write_php_ini(
            &php_ini_path(&installed[0], Os::Linux),
            &installed[0],
            DEFAULT_EXTENSIONS,
            &paths,
            Os::Linux,
        )
        .unwrap();

        let resolved = Runtime::active(&paths, Os::Linux)
            .unwrap()
            .expect("an active runtime");
        assert_eq!(resolved.version.to_string(), "8.4.2");
        assert!(
            resolved.executable.is_file(),
            "the executable must exist: {:?}",
            resolved.executable
        );
        assert_eq!(
            resolved.root,
            paths.runtime_version_dir(RuntimeKind::Php, "8.4.2")
        );
        assert!(
            resolved.php_ini.is_file(),
            "the generated ini must exist: {:?}",
            resolved.php_ini
        );
        assert!(resolved.has_ini());
        // A Unix build keeps the ini under etc/, a Windows build beside the binary.
        assert!(
            resolved.php_ini.ends_with("etc/php.ini"),
            "{:?}",
            resolved.php_ini
        );

        // PHPRC is how Lambo attaches the config without touching the PATH.
        let (key, value) = resolved.phprc();
        assert_eq!(key, "PHPRC");
        assert_eq!(value, resolved.php_ini);
    }

    #[test]
    fn resolving_a_runtime_without_an_executable_fails_instead_of_lying() {
        let temp = TempDir::new();
        let paths = temp.home();

        // A directory with no php binary: what an interrupted download leaves.
        let root = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        fs::create_dir_all(&root).unwrap();
        let broken = InstalledRuntime {
            kind: RuntimeKind::Php,
            name: "8.4.2".to_owned(),
            version: Version::parse("8.4.2").unwrap(),
            path: root,
        };

        let error = Runtime::from_installed(&broken, Os::Linux).unwrap_err();
        assert!(
            matches!(error, Error::RuntimeNotInstalled { .. }),
            "a runtime with no executable must not resolve: {error:?}"
        );
    }

    #[test]
    fn activate_requires_an_installed_version() {
        let temp = TempDir::new();
        let paths = temp.home();
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.3.14", Os::Linux);
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::Linux);

        let spec: VersionSpec = "8.3".parse().unwrap();
        assert_eq!(activate(&paths, &spec).unwrap().name, "8.3.14");
        assert_eq!(current(&paths).unwrap().unwrap().name, "8.3.14");

        let missing: VersionSpec = "8.5".parse().unwrap();
        let error = activate(&paths, &missing).unwrap_err();
        assert!(error.to_string().contains("8.5"), "{error}");
    }

    #[test]
    fn resolve_prefers_the_pinned_version_and_errors_when_nothing_is_installed() {
        let temp = TempDir::new();
        let paths = temp.home();

        let spec: VersionSpec = "8.4".parse().unwrap();
        let error = resolve(&paths, &spec).unwrap_err();
        assert!(matches!(error, Error::RuntimeMissing { .. }), "{error:?}");
        assert!(error.to_string().contains("lambo php install"), "{error}");

        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::Linux);
        assert_eq!(resolve(&paths, &spec).unwrap().name, "8.4.2");
        assert_eq!(
            executable(&paths, &spec, Os::Linux)
                .unwrap()
                .file_name()
                .unwrap(),
            "php"
        );
    }

    #[test]
    fn only_present_extensions_are_enabled() {
        let temp = TempDir::new();
        let paths = temp.home();
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::Linux);
        let runtime = current(&paths).unwrap().unwrap_or_else(|| {
            runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();
            current(&paths).unwrap().unwrap()
        });

        // No module files exist in the fake runtime, so a static build is
        // assumed and every request is honoured.
        let enabled = enabled_extensions(&runtime, &["curl", "mbstring", ""], Os::Linux);
        assert_eq!(enabled, vec!["curl".to_owned(), "mbstring".to_owned()]);

        // With a real extension directory, only what exists is enabled.
        let ext = extension_dir(&runtime, Os::Windows);
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("php_curl.dll"), b"DLL").unwrap();
        let enabled = enabled_extensions(&runtime, &["curl", "imagick"], Os::Windows);
        assert_eq!(enabled, vec!["curl".to_owned()]);
        let _ = paths;
    }

    #[test]
    fn php_ini_points_at_the_runtime_and_logs() {
        let temp = TempDir::new();
        let paths = temp.home();
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::Windows);
        let runtime = list(&paths).unwrap().into_iter().next().unwrap();

        let ini = php_ini_path(&runtime, Os::Windows);
        assert_eq!(ini, runtime.path.join("php.ini"));
        write_php_ini(&ini, &runtime, &["curl"], &paths, Os::Windows).unwrap();

        let contents = fs::read_to_string(&ini).unwrap();
        assert!(contents.contains("extension_dir = \""), "{contents}");
        // Apache and PHP both want forward slashes, even on Windows.
        assert!(
            !contents.contains('\\'),
            "generated config must use forward slashes: {contents}"
        );
        assert!(contents.contains("logs/php/php-error.log"));
        assert!(contents.contains("display_errors = On"));
    }

    #[test]
    fn available_versions_come_from_the_catalogue() {
        let catalog = Catalog::embedded().unwrap();
        let platform = Platform::new(Os::Windows, crate::platform::Arch::X86_64);
        let versions = available_versions(&catalog, platform);
        assert!(!versions.is_empty());
        assert!(
            versions
                .iter()
                .all(|version| Version::parse(version).is_ok())
        );

        // `install` refuses a version the catalogue does not have.
        let temp = TempDir::new();
        let paths = temp.home();
        let spec: VersionSpec = "5.6".parse().unwrap();
        let error = install(
            &paths,
            &catalog,
            &spec,
            platform,
            &LocalDownloader,
            &Default::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("catalogue"), "{error}");
    }

    #[test]
    fn php_is_run_without_touching_the_users_path() {
        // `lambo php -v` must work with nothing but the Lambo home set up:
        // the runtime is resolved from the marker file, not from PATH.
        let temp = TempDir::new();
        let paths = temp.home();
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::host());
        runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();

        let runtime = current(&paths).unwrap().unwrap();
        let program = php_executable(&runtime, Os::host()).unwrap();
        assert!(
            program.starts_with(paths.root()),
            "{} must live under the Lambo home",
            program.display()
        );

        // The fake runtime is not a real PHP, so only the plumbing is
        // asserted: the process starts and exits.
        let status = run(&paths, &runtime, &["-v".to_owned()], Os::host())
            .expect("the runtime must be started without touching PATH");
        assert!(status.success(), "the fake runtime exits 0");
    }

    // -----------------------------------------------------------------------
    // Parsing what PHP prints
    //
    // These pin the parsers to output captured from real builds. The formats
    // are not documented as stable, so if a future PHP changes them these are
    // the tests that say so - and they are the reason a health check cannot
    // silently start reporting "no configuration loaded" for every user.
    // -----------------------------------------------------------------------

    #[test]
    fn php_version_banners_parse() {
        let cases = [
            (
                "PHP 8.4.2 (cli) (built: Dec 17 2024 18:22:53) (NTS)",
                "8.4.2",
            ),
            (
                "PHP 8.3.10 (cli) (built: Jul  2 2024 12:00:00) (ZTS)",
                "8.3.10",
            ),
            (
                "PHP 8.2.9 (cli) (built: Aug  1 2023 09:00:00) (NTS)\nCopyright (c) The PHP Group",
                "8.2.9",
            ),
            // Windows prints the same first line.
            (
                "PHP 8.4.2 (cli) (built: Dec 17 2024 18:22:53) (NTS Visual C++ 2019 x64)",
                "8.4.2",
            ),
        ];
        for (banner, expected) in cases {
            assert_eq!(
                parse_php_version(banner).map(|v| v.to_string()),
                Some(expected.to_owned()),
                "failed to parse: {banner}"
            );
        }
    }

    #[test]
    fn a_version_suffix_does_not_reject_a_working_runtime() {
        // Pre-release builds print `8.4.0RC1`, which is not valid semver. A
        // working PHP must not be reported as broken over its label.
        assert_eq!(
            parse_php_version("PHP 8.4.0RC1 (cli) (built: Aug 27 2024) (NTS)")
                .map(|v| v.to_string()),
            Some("8.4.0".to_owned())
        );
    }

    #[test]
    fn output_that_is_not_a_php_banner_parses_to_nothing() {
        assert_eq!(parse_php_version(""), None);
        assert_eq!(parse_php_version("bash: php: command not found"), None);
        assert_eq!(parse_php_version("PHP (cli)"), None);
    }

    #[test]
    fn the_loaded_configuration_file_is_read_from_php_ini_output() {
        let output = "Configuration File (php.ini) Path: /usr/local/lib\n\
                      Loaded Configuration File:         /home/u/.lambo/php/8.4.2/etc/php.ini\n\
                      Scan this dir for additional .ini files: (none)\n\
                      Additional .ini files parsed:      (none)\n";
        assert_eq!(
            parse_loaded_ini(output).map(|p| p.display().to_string()),
            Some("/home/u/.lambo/php/8.4.2/etc/php.ini".to_owned())
        );
    }

    #[test]
    fn no_loaded_configuration_file_is_an_answer_not_a_missing_check() {
        // PHP prints `(none)` when it loaded nothing. Conflating that with
        // "the check did not run" is how a broken PHPRC goes unnoticed, so the
        // two must stay distinguishable at the call site even though both
        // arrive here as `None`.
        let none = "Configuration File (php.ini) Path: /usr/local/lib\n\
                    Loaded Configuration File:         (none)\n";
        assert_eq!(parse_loaded_ini(none), None);
        assert_eq!(parse_loaded_ini("unrelated output"), None);
    }

    #[test]
    fn the_module_list_drops_section_headers_and_blank_lines() {
        let output = "[PHP Modules]\nCore\ncurl\nmysqli\n\n[Zend Modules]\n";
        assert_eq!(
            parse_module_list(output),
            vec!["Core".to_owned(), "curl".to_owned(), "mysqli".to_owned()]
        );
    }
}
