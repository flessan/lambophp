//! Registry of locally installed runtimes.
//!
//! Runtimes live under the Lambo home directory: `php/<version>/`,
//! `apache/<version>/`, `database/mariadb/<version>/` and so on (see
//! [`crate::paths`]). A runtime is *installed* when its directory exists and
//! contains the family's primary executable; a runtime is *active* when the
//! `.active` marker in its family directory names it.
//!
//! The active runtime is recorded in a small marker file rather than a
//! symlink: the registry then behaves identically on Unix and Windows and
//! never needs elevated privileges (`docs/adr/0003-single-home-directory.md`).
//!
//! Everything here is pure filesystem work with no assumptions about the
//! host beyond [`crate::platform`], so the same code discovers
//! `C:\Lambo\php\8.4.2\php.exe` and `~/.lambo/php/8.4.2/bin/php`.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::platform::Os;
use crate::version::VersionSpec;

/// Name of the marker file storing the active version directory name.
pub const ACTIVE_FILE: &str = ".active";

/// Name of the manifest written into a runtime directory on install.
///
/// Hidden by a leading dot so it never collides with an upstream file name,
/// and never mistaken for the runtime's own metadata.
pub const MANIFEST_FILE: &str = ".lambo-install.json";

/// What Lambo recorded when it installed a runtime.
///
/// A directory name is not proof of what is inside it. It can be renamed, a
/// directory can be left behind by an interrupted install, and two different
/// builds can be unpacked to the same name. The manifest is what Lambo
/// actually wrote, which is what an integrity check needs to be meaningful.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallManifest {
    /// Runtime family this directory holds.
    pub kind: String,
    /// Version Lambo unpacked, as recorded at install time.
    pub version: String,
    /// Platform the artifact was built for.
    pub platform: String,
    /// URL the artifact came from.
    pub source_url: String,
    /// How the artifact was located: `catalogue`, `override`, `mirror` or
    /// `local`.
    ///
    /// Absent in manifests written before source tracking existed, and read
    /// back as `unknown` rather than guessed. Claiming an install came from the
    /// official catalogue when the manifest does not say so would send somebody
    /// debugging a mirror problem to the wrong place entirely.
    #[serde(default = "unknown_source_kind")]
    pub source_kind: String,
    /// SHA-256 the artifact was verified against.
    pub sha256: String,
    /// Seconds since the Unix epoch when the install completed.
    pub installed_at: u64,
    /// Path of the primary executable, relative to the runtime root.
    ///
    /// Set when the catalogue entry declared one. Recording it means a later
    /// integrity check can confirm the thing Lambo is supposed to run is still
    /// the thing that was installed.
    pub executable: String,
    /// Paths, relative to the runtime root, that were present at install time.
    ///
    /// Recorded so an integrity check can tell "this was never finished" from
    /// "something was deleted afterwards".
    pub files: Vec<String>,
}

/// The `source_kind` recorded for a manifest that predates source tracking.
fn unknown_source_kind() -> String {
    crate::sources::SourceKind::UNKNOWN.to_owned()
}

/// What an integrity check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// A manifest is present and every recorded file is still there.
    Ok,
    /// No manifest: this directory was not written by Lambo, or predates the
    /// manifest. It may be perfectly usable, but Lambo cannot vouch for it.
    Unrecorded,
    /// A manifest exists but does not describe this directory.
    Mismatch {
        /// What is wrong, in one sentence.
        reason: String,
    },
    /// Files recorded at install time are gone.
    Incomplete {
        /// Paths, relative to the runtime root, that are missing.
        missing: Vec<String>,
    },
}

impl Integrity {
    /// Is the runtime safe to run?
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ok | Self::Unrecorded)
    }

    /// The word `lambo php list` prints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok | Self::Unrecorded => "installed",
            Self::Mismatch { .. } | Self::Incomplete { .. } => "corrupt",
        }
    }

    /// One line explaining the verdict, for diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Self::Ok => "matches what Lambo installed".to_owned(),
            Self::Unrecorded => {
                "no install manifest; Lambo cannot confirm what this directory contains".to_owned()
            }
            Self::Mismatch { reason } => reason.clone(),
            Self::Incomplete { missing } => format!(
                "{} recorded file(s) are missing, e.g. {}",
                missing.len(),
                missing.first().map(String::as_str).unwrap_or("")
            ),
        }
    }
}

/// How deep [`locate_executable`] descends into a runtime directory.
///
/// Upstream archives are inconsistent: PHP's Windows zip unpacks `php.exe`
/// next to the extension DLLs, MariaDB's zip nests everything under
/// `bin/`, and some tarballs add a single wrapper directory. Two extra
/// levels cover all of them while keeping the scan cheap and bounded.
const MAX_SEARCH_DEPTH: usize = 2;

/// Runtime families Lambo manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeKind {
    /// PHP itself.
    Php,
    /// Apache httpd.
    Apache,
    /// MariaDB (MySQL-compatible, the default engine).
    Mariadb,
    /// Oracle MySQL.
    Mysql,
    /// Composer (PHP package manager).
    ///
    /// Lambo detects Composer and gives it a directory, but does not install
    /// it: it is the user's own tool, so [`RuntimeKind::install_command`]
    /// points upstream instead of at a `lambo` subcommand that does not exist.
    Composer,
}

impl RuntimeKind {
    /// Every runtime family, in display order.
    pub const ALL: [Self; 5] = [
        Self::Php,
        Self::Apache,
        Self::Mariadb,
        Self::Mysql,
        Self::Composer,
    ];

    /// Directory name of the family, used in messages and catalogue keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Php => "php",
            Self::Apache => "apache",
            Self::Mariadb => "mariadb",
            Self::Mysql => "mysql",
            Self::Composer => "composer",
        }
    }

    /// Human-readable name.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Php => "PHP",
            Self::Apache => "Apache",
            Self::Mariadb => "MariaDB",
            Self::Mysql => "MySQL",
            Self::Composer => "Composer",
        }
    }

    /// The command that installs a missing runtime of this family.
    pub fn install_command(&self) -> &'static str {
        match self {
            Self::Php => "lambo php install <version>",
            // Apache has no install command of its own: `lambo up` (and
            // `lambo server start`) installs it when the project needs it.
            Self::Apache => "lambo up",
            Self::Mariadb | Self::Mysql => "lambo db install",
            // Composer is the user's own tool; Lambo detects it but does not
            // ship or install it, so the pointer is upstream.
            Self::Composer => "install Composer from https://getcomposer.org/download/",
        }
    }

    /// Executables that must exist for an installation to be usable.
    ///
    /// Names are given without extension; [`Os::executable_name`] adds
    /// `.exe` on Windows. The first candidate that exists wins, which is how
    /// MariaDB (which still ships `mysqld` compatibility binaries) and MySQL
    /// share one code path.
    pub fn server_executables(&self) -> &'static [&'static str] {
        match self {
            Self::Php => &["php"],
            Self::Apache => &["httpd", "apache2", "bin/httpd", "bin/apache2"],
            Self::Mariadb => &["mariadbd", "mysqld", "bin/mariadbd", "bin/mysqld"],
            Self::Mysql => &["mysqld", "bin/mysqld"],
            Self::Composer => &["composer", "composer.phar"],
        }
    }

    /// Command-line clients used for interactive and scripted work.
    pub fn client_executables(&self) -> &'static [&'static str] {
        match self {
            Self::Php => &["php"],
            Self::Apache => &["httpd", "apache2", "bin/httpd", "bin/apache2"],
            Self::Mariadb => &["mariadb", "mysql", "bin/mariadb", "bin/mysql"],
            Self::Mysql => &["mysql", "bin/mysql"],
            Self::Composer => &["composer", "composer.phar"],
        }
    }

    /// The administrative client used to initialize a data directory.
    pub fn init_executables(&self) -> &'static [&'static str] {
        match self {
            Self::Mariadb => &[
                "mariadb-install-db",
                "mysql_install_db",
                "bin/mariadb-install-db.exe",
                "bin/mysql_install_db.exe",
                "scripts/mariadb-install-db.exe",
                "scripts/mysql_install_db.exe",
            ],
            Self::Mysql => &["mysqld", "bin/mysqld"],
            _ => &[],
        }
    }

    /// The client used to shut a running server down gracefully.
    pub fn admin_executables(&self) -> &'static [&'static str] {
        match self {
            Self::Mariadb => &[
                "mariadb-admin",
                "mysqladmin",
                "bin/mariadb-admin",
                "bin/mysqladmin",
            ],
            Self::Mysql => &["mysqladmin", "bin/mysqladmin"],
            _ => &[],
        }
    }

    /// `true` for the two database families.
    pub fn is_database(&self) -> bool {
        matches!(self, Self::Mariadb | Self::Mysql)
    }
}

impl fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A runtime discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRuntime {
    /// Runtime family.
    pub kind: RuntimeKind,
    /// Directory name (the raw version string).
    pub name: String,
    /// Parsed version.
    pub version: Version,
    /// Full path of the runtime directory.
    pub path: PathBuf,
}

impl InstalledRuntime {
    /// Locates the server executable inside this runtime, if present.
    pub fn server_executable(&self, os: Os) -> Option<PathBuf> {
        locate_executable(&self.path, os, self.kind.server_executables())
    }

    /// Locates a command-line client inside this runtime, if present.
    pub fn client_executable(&self, os: Os) -> Option<PathBuf> {
        locate_executable(&self.path, os, self.kind.client_executables())
    }

    /// Locates the data-directory initializer of a database runtime.
    pub fn init_executable(&self, os: Os) -> Option<PathBuf> {
        locate_executable(&self.path, os, self.kind.init_executables())
    }

    /// Locates the administrative client of a database runtime.
    pub fn admin_executable(&self, os: Os) -> Option<PathBuf> {
        locate_executable(&self.path, os, self.kind.admin_executables())
    }

    /// Whether the runtime is usable: directory present and executable found.
    ///
    /// A directory left behind by an interrupted download must be reported as
    /// broken, never treated as installed - `lambo doctor` relies on this.
    pub fn is_complete(&self, os: Os) -> bool {
        self.path.is_dir() && self.server_executable(os).is_some()
    }
}

/// Finds the first existing executable among `candidates` inside `dir`.
///
/// Candidates may carry a relative sub-path (`bin/mysqld`), which is how
/// Unix and Windows layouts of the same runtime are both matched. The
/// platform executable extension is appended automatically.
pub fn locate_executable(dir: &Path, os: Os, candidates: &[&str]) -> Option<PathBuf> {
    for candidate in candidates {
        // A candidate may itself name the relative location inside the
        // runtime; normalise separators so Windows accepts `bin/mysqld`.
        let relative = PathBuf::from(candidate.replace('/', std::path::MAIN_SEPARATOR_STR));
        let named = os.executable_name(candidate.rsplit('/').next().unwrap_or(candidate));
        let with_extension = relative.with_file_name(&named);

        for candidate_path in [relative.clone(), with_extension] {
            let full = dir.join(&candidate_path);
            if is_executable_file(&full, os) {
                return Some(full);
            }
        }
    }

    // Fall back to a bounded scan for archives with an unexpected layout.
    let names: Vec<String> = candidates
        .iter()
        .filter_map(|candidate| candidate.rsplit('/').next())
        .map(|name| os.executable_name(name))
        .collect();
    scan_for_executable(dir, os, &names, 0)
}

/// Recursively searches for one of `names`, up to [`MAX_SEARCH_DEPTH`].
fn scan_for_executable(dir: &Path, os: Os, names: &[String], depth: usize) -> Option<PathBuf> {
    if depth > MAX_SEARCH_DEPTH {
        return None;
    }
    let entries = fs::read_dir(dir).ok()?;
    let mut subdirectories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            subdirectories.push(path);
            continue;
        }
        if names.iter().any(|wanted| name.eq_ignore_ascii_case(wanted))
            && is_executable_file(&path, os)
        {
            return Some(path);
        }
    }
    for subdirectory in subdirectories {
        if let Some(found) = scan_for_executable(&subdirectory, os, names, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Whether `path` exists and can be executed on this platform.
///
/// Windows decides by extension; Unix also requires the executable bit, so a
/// half-extracted archive is not mistaken for a working runtime.
pub fn is_executable_file(path: &Path, os: Os) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    if os.is_windows() {
        return true;
    }
    executable_bit(&metadata)
}

/// Whether the file's permission bits allow execution.
///
/// Only Unix has such a bit. The check is behind `cfg` rather than behind the
/// `os` argument so this file compiles for Windows, where a caller may still
/// ask about a Unix-shaped path in a test.
#[cfg(unix)]
fn executable_bit(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

/// On a platform with no executable bit, existence is the best answer.
#[cfg(not(unix))]
fn executable_bit(_metadata: &fs::Metadata) -> bool {
    true
}

/// Lists installed runtimes of one family, sorted newest first.
///
/// A missing family directory is an empty list, not an error - on a fresh
/// machine nothing is installed yet. Directory names that are not semver are
/// ignored (they were not put there by Lambo).
/// Records what Lambo installed, inside the runtime directory.
///
/// Written last: a runtime with no manifest was never finished, which is
/// exactly what an integrity check needs to be able to tell.
///
/// `executable`, when given, is the path the catalogue entry declared for the
/// primary executable. It is checked against what was actually unpacked,
/// because a declared path that is not there means the entry describes an
/// archive Lambo does not have: better to fail here, before the directory is
/// promoted, than to install a runtime whose binary cannot be found and let
/// `lambo up` report it as broken later.
pub fn write_manifest(
    kind: RuntimeKind,
    version: &str,
    platform: &str,
    source: &crate::sources::Resolved,
    sha256: &str,
    executable: Option<&str>,
    runtime_dir: &Path,
) -> Result<()> {
    let declared = executable.unwrap_or("").to_owned();
    if !declared.is_empty() {
        let relative = PathBuf::from(declared.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !runtime_dir.join(&relative).exists() {
            return Err(Error::InvalidInput(format!(
                "the catalogue says {kind} {version} provides `{declared}`, but the unpacked                  archive has no such file; the entry describes a different artifact"
            )));
        }
    }
    let files = list_relative(runtime_dir, runtime_dir);
    let manifest = InstallManifest {
        kind: kind.to_string(),
        version: version.to_owned(),
        platform: platform.to_owned(),
        source_url: source.artifact.url.clone(),
        source_kind: source.kind.as_str().to_owned(),
        sha256: sha256.to_owned(),
        installed_at: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        executable: declared,
        files,
    };
    let json = serde_json::to_string_pretty(&manifest).map_err(|source| Error::Json {
        path: runtime_dir.join(MANIFEST_FILE),
        source,
    })?;
    crate::fsx::write_atomic(&runtime_dir.join(MANIFEST_FILE), &json)
}

/// Reads the manifest in a runtime directory, if there is one.
///
/// A manifest that cannot be parsed is treated as absent rather than an
/// error: the runtime itself may be fine, and reporting a broken sidecar as a
/// broken PHP would send the user reinstalling for no reason.
pub fn read_manifest(runtime_dir: &Path) -> Option<InstallManifest> {
    let text = fs::read_to_string(runtime_dir.join(MANIFEST_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Checks a runtime directory against what Lambo recorded.
pub fn verify(runtime_dir: &Path, kind: RuntimeKind) -> Integrity {
    let Some(manifest) = read_manifest(runtime_dir) else {
        return Integrity::Unrecorded;
    };
    if manifest.kind != kind.to_string() {
        return Integrity::Mismatch {
            reason: format!(
                "the install manifest says this directory holds {}, not {kind}",
                manifest.kind
            ),
        };
    }
    if !manifest.version.is_empty() {
        let Ok(recorded) = Version::parse(&manifest.version) else {
            return Integrity::Mismatch {
                reason: format!(
                    "the install manifest records `{}` as the version, which is not a version",
                    manifest.version
                ),
            };
        };
        let directory = runtime_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        if let Some(directory) = directory
            && directory != manifest.version
        {
            // The directory name is what `lambo php list` shows, so a rename
            // makes Lambo report a version it does not have.
            return Integrity::Mismatch {
                reason: format!(
                    "the directory is named `{directory}` but the install manifest records {recorded}"
                ),
            };
        }
    }
    let missing = manifest
        .files
        .iter()
        .filter(|relative| !runtime_dir.join(relative).exists())
        .cloned()
        .collect::<Vec<_>>();
    match missing.is_empty() {
        true => Integrity::Ok,
        false => Integrity::Incomplete { missing },
    }
}

/// Every path under `root`, relative to `root`, as `/`-separated strings.
///
/// The manifest is excluded from itself, and the order is sorted so two
/// installs of the same archive produce identical manifests.
fn list_relative(dir: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("").file_name().and_then(|n| n.to_str()) == Some(MANIFEST_FILE) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            out.extend(list_relative(&path, root));
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root) {
            out.push(
                relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
    out.sort();
    out
}

/// Moves a finished staging directory into place as the installed runtime.
///
/// The three install paths all used the same two lines: delete the directory
/// that is there, then rename staging over it. Between those two lines the
/// user owns nothing - the working runtime is already gone and the new one is
/// not yet in place. A rename can fail after the delete for reasons that have
/// nothing to do with the download: a file held open by a running service, a
/// permission problem, a full disk. The outcome was a reinstall that left the
/// user with no runtime at all, which is worse than the failed upgrade.
///
/// So the existing directory is moved aside rather than deleted, and only
/// removed once the new one is in place. If the rename fails, the old runtime
/// is put back.
pub fn promote(staging: &Path, final_dir: &Path) -> Result<()> {
    let backup = final_dir.with_file_name(format!(
        "{}.replacing",
        final_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = fs::remove_dir_all(&backup);

    let mut moved_aside = false;
    if final_dir.exists() {
        fs::rename(final_dir, &backup).map_err(|source| Error::io(&backup, source))?;
        moved_aside = true;
    }

    if let Err(error) = fs::rename(staging, final_dir) {
        // Put the previous runtime back before reporting anything: a user who
        // reads this error should still have the version they started with.
        if moved_aside {
            let _ = fs::rename(&backup, final_dir);
        }
        return Err(Error::io(final_dir, error));
    }

    // The new runtime is live. The old one is now safe to discard, and a
    // failure to discard it is not an installation failure - it leaves a
    // `.replacing` directory behind, which the next install clears.
    if moved_aside {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

pub fn installed(paths: &Paths, kind: RuntimeKind) -> Result<Vec<InstalledRuntime>> {
    let dir = paths.runtime_dir(kind);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io(&dir, source)),
    };

    let mut runtimes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::io(&dir, source))?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(version) = Version::parse(&name) else {
            continue;
        };
        runtimes.push(InstalledRuntime {
            kind,
            name,
            version,
            path: entry.path(),
        });
    }
    runtimes.sort_by(|a, b| b.version.cmp(&a.version).then_with(|| a.name.cmp(&b.name)));
    Ok(runtimes)
}

/// Raw content of the active marker file, without validating the target.
///
/// Kept separate from [`active`] so listings can gracefully highlight a
/// stale marker instead of failing.
pub fn active_name(paths: &Paths, kind: RuntimeKind) -> Result<Option<String>> {
    let marker = paths.runtime_dir(kind).join(ACTIVE_FILE);
    match fs::read_to_string(&marker) {
        Ok(name) => {
            let name = name.trim().to_owned();
            Ok(if name.is_empty() { None } else { Some(name) })
        }
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io(&marker, source)),
    }
}

/// Resolves the active runtime, validating that it still exists on disk.
///
/// A marker pointing to a deleted runtime is a real error here
/// ([`Error::RuntimeNotInstalled`]); diagnostics surface it, and
/// `lambo php use` repairs it by writing a fresh marker.
pub fn active(paths: &Paths, kind: RuntimeKind) -> Result<Option<InstalledRuntime>> {
    let Some(name) = active_name(paths, kind)? else {
        return Ok(None);
    };
    let path = paths.runtime_dir(kind).join(&name);
    if !path.is_dir() {
        return Err(Error::RuntimeNotInstalled {
            kind: kind.display_name(),
            name,
            path,
        });
    }
    match Version::parse(&name) {
        Ok(version) => Ok(Some(InstalledRuntime {
            kind,
            name,
            version,
            path,
        })),
        Err(_) => Err(Error::RuntimeNotInstalled {
            kind: kind.display_name(),
            name,
            path,
        }),
    }
}

/// Resolves the runtime to use for a project.
///
/// A pinned version spec wins; otherwise the active runtime is used; failing
/// that, the newest installed runtime becomes active implicitly (it is not
/// persisted, so `lambo php use` stays the only writer of the marker).
pub fn resolve(
    paths: &Paths,
    kind: RuntimeKind,
    spec: &VersionSpec,
) -> Result<Option<InstalledRuntime>> {
    let candidates = installed(paths, kind)?;
    if spec.is_req() {
        if let Some(selected) = select(spec, &candidates) {
            return Ok(Some(selected.clone()));
        }
        return Ok(None);
    }
    if let Some(current) = active(paths, kind)? {
        return Ok(Some(current));
    }
    Ok(select(spec, &candidates).cloned())
}

/// Records `name` as the active runtime of the family.
///
/// Validates that the runtime exists before writing the marker; it is
/// impossible to activate something that is not installed.
pub fn set_active(paths: &Paths, kind: RuntimeKind, name: &str) -> Result<()> {
    let path = paths.runtime_dir(kind).join(name);
    if !path.is_dir() {
        return Err(Error::RuntimeNotInstalled {
            kind: kind.display_name(),
            name: name.to_owned(),
            path,
        });
    }
    let marker = paths.runtime_dir(kind).join(ACTIVE_FILE);
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
    }
    fs::write(&marker, format!("{name}\n")).map_err(|source| Error::io(&marker, source))
}

/// Clears the active marker (used by `lambo php remove`).
pub fn clear_active(paths: &Paths, kind: RuntimeKind) -> Result<()> {
    let marker = paths.runtime_dir(kind).join(ACTIVE_FILE);
    match fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io(&marker, source)),
    }
}

/// Deletes an installed runtime directory.
///
/// Refuses to delete anything that is not a version directory inside the
/// family root, so a typo cannot remove a user directory.
pub fn remove(paths: &Paths, kind: RuntimeKind, name: &str) -> Result<()> {
    // Only a plain directory name may be removed: `..`, absolute paths and
    // the marker file itself are refused so a typo cannot delete anything
    // outside the family directory.
    if !crate::platform::is_valid_path_component(name) || name == ACTIVE_FILE {
        return Err(Error::InvalidInput(format!(
            "`{name}` is not a runtime version"
        )));
    }
    let target = paths.runtime_dir(kind).join(name);
    if !target.is_dir() {
        return Err(Error::RuntimeNotInstalled {
            kind: kind.display_name(),
            name: name.to_owned(),
            path: target,
        });
    }

    // A running service holding this runtime must be stopped first. Deleting
    // the files under a live httpd or mysqld does not stop it: the process
    // keeps its open handles, keeps serving from inodes that no longer have a
    // name, and then cannot be restarted. The user is left with an orphan and
    // no way to tell it is one, because Lambo's own record still says it is
    // running.
    if let Some(service) = services_using(paths, &target)? {
        return Err(Error::InvalidInput(format!(
            "{} {} is in use by the running `{service}` service; run `lambo down` first",
            kind.display_name(),
            name
        )));
    }

    // The active marker is cleared before the files go. If the deletion fails
    // halfway - a locked file on Windows, a permissions problem anywhere -
    // this leaves a broken directory that is no longer selected, which is
    // recoverable. The reverse leaves the active version pointing at a
    // directory that is half gone, and every later command fails on it.
    if active_name(paths, kind)?.as_deref() == Some(name) {
        clear_active(paths, kind)?;
    }
    fs::remove_dir_all(&target).map_err(|source| Error::io(&target, source))?;
    Ok(())
}

/// The recorded services whose command line runs out of `runtime_dir`.
///
/// Only services Lambo started and can still account for are considered. A
/// stale record for a process that is gone must not block a removal, so
/// liveness is re-checked rather than trusted.
fn services_using(paths: &Paths, runtime_dir: &Path) -> Result<Option<String>> {
    let state = crate::state::State::load(paths)?;
    let os = Os::host();
    for record in state.services.values() {
        if !record.is_alive(os) {
            continue;
        }
        // The command line is recorded verbatim at start, so a runtime in use
        // appears in it as a path prefix. Both separators are checked because
        // a Windows command line may be recorded either way.
        let wanted = [
            runtime_dir.to_string_lossy().into_owned(),
            runtime_dir.to_string_lossy().replace('\\', "/"),
            runtime_dir.to_string_lossy().replace('/', "\\"),
        ];
        if wanted.iter().any(|needle| record.command.contains(needle)) {
            return Ok(Some(record.name.clone()));
        }
    }
    Ok(None)
}

/// Picks the best installed runtime satisfying `spec`.
///
/// Channel resolution against the local inventory:
/// - `stable` - newest without a prerelease tag
/// - `latest` - newest of any kind
/// - `nightly` - newest whose prerelease tag contains `nightly`
pub fn select<'a>(
    spec: &VersionSpec,
    candidates: &'a [InstalledRuntime],
) -> Option<&'a InstalledRuntime> {
    let mut best: Option<&InstalledRuntime> = None;
    for candidate in candidates {
        let eligible = match spec {
            VersionSpec::Req(req) => req.matches(&candidate.version),
            VersionSpec::Stable => candidate.version.pre.is_empty(),
            VersionSpec::Latest => true,
            VersionSpec::Nightly => candidate.version.pre.as_str().contains("nightly"),
        };
        if !eligible {
            continue;
        }
        match best {
            Some(current) if current.version >= candidate.version => {}
            _ => best = Some(candidate),
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Creates fake runtime directories `kind/<name>` inside a fresh home,
    /// each containing a usable executable for the given platform.
    fn home_with(kind: RuntimeKind, versions: &[&str], os: Os) -> (TempDir, Paths) {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        for version in versions {
            let dir = paths.runtime_version_dir(kind, version);
            fs::create_dir_all(&dir).unwrap();
            let executable = dir.join(os.executable_name(kind.as_str()));
            fs::write(&executable, b"#!/bin/sh\n").unwrap();
            mark_executable(&executable);
        }
        (temp, paths)
    }

    /// Sets the executable bit on Unix; a no-op on Windows.
    #[cfg(unix)]
    fn mark_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn mark_executable(_path: &Path) {}

    /// The primary executable name `home_with` creates, for this platform.
    fn os_executable_name() -> &'static str {
        if cfg!(windows) { "php.exe" } else { "php" }
    }

    #[test]
    fn an_install_records_a_manifest_that_verify_accepts() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");

        // home_with builds the directory by hand, so there is no manifest yet.
        assert_eq!(verify(&dir, RuntimeKind::Php), Integrity::Unrecorded);

        write_manifest(
            RuntimeKind::Php,
            "8.4.2",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/php.tar.gz"),
            "a".repeat(64).as_str(),
            None,
            &dir,
        )
        .unwrap();

        let manifest = read_manifest(&dir).expect("a manifest was just written");
        assert_eq!(manifest.kind, "php");
        assert_eq!(manifest.version, "8.4.2");
        assert_eq!(manifest.platform, "linux-x64");
        assert!(manifest.installed_at > 0, "the timestamp must be recorded");
        // The manifest lists what was there, and never itself.
        assert!(!manifest.files.is_empty());
        assert!(
            !manifest.files.iter().any(|f| f == MANIFEST_FILE),
            "{:?}",
            manifest.files
        );
        assert_eq!(verify(&dir, RuntimeKind::Php), Integrity::Ok);
    }

    #[test]
    fn a_deleted_file_makes_the_runtime_corrupt() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        let victim = dir.join("ext");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("openssl.so"), b"x").unwrap();

        write_manifest(
            RuntimeKind::Php,
            "8.4.2",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/php.tar.gz"),
            "a".repeat(64).as_str(),
            None,
            &dir,
        )
        .unwrap();
        assert_eq!(verify(&dir, RuntimeKind::Php), Integrity::Ok);

        // Losing an extension is exactly the failure that used to be
        // invisible: the directory still exists, so Lambo called it installed.
        fs::remove_file(victim.join("openssl.so")).unwrap();

        match verify(&dir, RuntimeKind::Php) {
            Integrity::Incomplete { missing } => {
                assert_eq!(missing, vec!["ext/openssl.so".to_owned()]);
                assert!(!Integrity::Incomplete { missing }.is_usable());
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn a_renamed_directory_does_not_pass_as_the_version_it_is_named() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        write_manifest(
            RuntimeKind::Php,
            "8.4.2",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/php.tar.gz"),
            "a".repeat(64).as_str(),
            None,
            &dir,
        )
        .unwrap();

        // Rename the directory to a version Lambo does not have. The name is
        // what `lambo php list` shows and what `lambo php use` matches, so
        // accepting it would report a version that was never installed.
        let renamed = paths.runtime_version_dir(RuntimeKind::Php, "9.9.9");
        fs::rename(&dir, &renamed).unwrap();

        match verify(&renamed, RuntimeKind::Php) {
            Integrity::Mismatch { reason } => {
                assert!(reason.contains("9.9.9"), "{reason}");
                assert!(reason.contains("8.4.2"), "{reason}");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_describing_another_family_is_rejected() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        write_manifest(
            RuntimeKind::Apache,
            "8.4.2",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/httpd.tar.gz"),
            "a".repeat(64).as_str(),
            None,
            &dir,
        )
        .unwrap();

        match verify(&dir, RuntimeKind::Php) {
            Integrity::Mismatch { reason } => assert!(reason.contains("apache"), "{reason}"),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn removing_a_runtime_a_live_service_is_using_is_refused() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");

        // Record a service that is genuinely running - this process - and
        // whose command line points into the runtime, which is what a real
        // httpd started from that directory looks like.
        let mut state = crate::state::State::default();
        state.record(crate::state::ServiceRecord::new(
            crate::state::names::APACHE,
            std::process::id(),
            format!("{} -f httpd.conf", dir.display()),
        ));
        state.save(&paths).unwrap();

        let error = remove(&paths, RuntimeKind::Php, "8.4.2").unwrap_err();
        assert!(error.to_string().contains("lambo down"), "{error}");
        assert!(dir.exists(), "a refused removal must not delete anything");

        // Once the service is gone the removal goes through.
        crate::state::State::default().save(&paths).unwrap();
        remove(&paths, RuntimeKind::Php, "8.4.2").unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_stale_service_record_does_not_block_a_removal() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");

        // A PID nothing is listening on: the record is stale, and treating it
        // as live would make the runtime permanently unremovable.
        let mut state = crate::state::State::default();
        state.record(crate::state::ServiceRecord::new(
            crate::state::names::APACHE,
            u32::MAX,
            format!("{} -f httpd.conf", dir.display()),
        ));
        state.save(&paths).unwrap();

        remove(&paths, RuntimeKind::Php, "8.4.2").unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_declared_executable_that_is_not_there_fails_the_install() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");

        // A catalogue entry claiming a path the archive does not contain
        // describes a different artifact. Refusing here beats installing a
        // runtime whose binary cannot be found.
        let error = write_manifest(
            RuntimeKind::Php,
            "8.4.2",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/php.tar.gz"),
            "a".repeat(64).as_str(),
            Some("bin/php-not-here"),
            &dir,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("bin/php-not-here"), "{message}");
        assert!(!dir.join(MANIFEST_FILE).exists(), "nothing was recorded");

        // The same call with the real path succeeds and records it.
        write_manifest(
            RuntimeKind::Php,
            "8.4.2",
            "linux-x64",
            &crate::sources::Resolved::catalogue("https://example.com/php.tar.gz"),
            "a".repeat(64).as_str(),
            Some(os_executable_name()),
            &dir,
        )
        .unwrap();
        assert_eq!(
            read_manifest(&dir).unwrap().executable,
            os_executable_name()
        );
    }

    #[test]
    fn promote_replaces_an_existing_runtime_and_leaves_no_backup() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let final_dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        let staging = final_dir.with_file_name("8.4.2.install-ok");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("php"), b"new").unwrap();

        promote(&staging, &final_dir).unwrap();

        assert_eq!(fs::read(final_dir.join("php")).unwrap(), b"new");
        assert!(!staging.exists(), "staging was moved, not copied");
        assert!(
            !final_dir.with_file_name("8.4.2.replacing").exists(),
            "a successful upgrade discards the previous runtime"
        );
    }

    #[test]
    fn a_failed_promote_puts_the_previous_runtime_back() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);
        let final_dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        fs::write(final_dir.join("php"), b"working").unwrap();

        // A staging directory that does not exist makes the rename fail for a
        // reason that has nothing to do with the download - the same class of
        // failure as a locked file or a full disk - after the existing
        // runtime has already been moved aside.
        let staging = final_dir.with_file_name("8.4.2.never-extracted");
        assert!(promote(&staging, &final_dir).is_err());

        // The whole point: the user still has the runtime they started with.
        assert_eq!(
            fs::read(final_dir.join("php")).unwrap(),
            b"working",
            "a failed upgrade must not destroy the installed runtime"
        );
        assert!(
            !final_dir.with_file_name("8.4.2.replacing").exists(),
            "the backup was restored, not left behind"
        );
    }

    #[test]
    fn installed_sorts_newest_first_and_ignores_junk() {
        let (_temp, paths) = home_with(
            RuntimeKind::Php,
            &["8.3.14", "8.4.2", "8.5.0-nightly.1"],
            Os::Linux,
        );
        let php_dir = paths.runtime_dir(RuntimeKind::Php);
        // A directory name that is not semver must be skipped, not fail.
        fs::create_dir_all(php_dir.join("not-a-version")).unwrap();
        // A stray file must be skipped too.
        fs::write(php_dir.join(ACTIVE_FILE), "8.4.2\n").unwrap();

        let list = installed(&paths, RuntimeKind::Php).unwrap();
        let names: Vec<&str> = list.iter().map(|runtime| runtime.name.as_str()).collect();
        assert_eq!(names, ["8.5.0-nightly.1", "8.4.2", "8.3.14"]);
        assert!(list.iter().all(|runtime| runtime.kind == RuntimeKind::Php));
    }

    #[test]
    fn missing_directory_is_an_empty_list() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        assert!(installed(&paths, RuntimeKind::Php).unwrap().is_empty());
        assert!(installed(&paths, RuntimeKind::Mariadb).unwrap().is_empty());
    }

    #[test]
    fn database_engines_live_side_by_side() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        fs::create_dir_all(paths.runtime_version_dir(RuntimeKind::Mariadb, "11.4.2")).unwrap();
        fs::create_dir_all(paths.runtime_version_dir(RuntimeKind::Mysql, "8.0.40")).unwrap();

        assert_eq!(installed(&paths, RuntimeKind::Mariadb).unwrap().len(), 1);
        assert_eq!(installed(&paths, RuntimeKind::Mysql).unwrap().len(), 1);
    }

    #[test]
    fn select_honors_specs_and_channels() {
        let (_temp, paths) = home_with(
            RuntimeKind::Php,
            &["8.3.14", "8.4.0", "8.4.2", "8.5.0-nightly.1"],
            Os::Linux,
        );
        let list = installed(&paths, RuntimeKind::Php).unwrap();

        let spec: VersionSpec = "8.4".parse().unwrap();
        assert_eq!(select(&spec, &list).unwrap().name, "8.4.2");
        assert_eq!(select(&VersionSpec::Stable, &list).unwrap().name, "8.4.2");
        assert_eq!(
            select(&VersionSpec::Latest, &list).unwrap().name,
            "8.5.0-nightly.1"
        );
        assert_eq!(
            select(&VersionSpec::Nightly, &list).unwrap().name,
            "8.5.0-nightly.1"
        );

        let unsatisfiable: VersionSpec = "9.0".parse().unwrap();
        assert!(select(&unsatisfiable, &list).is_none());
        assert!(select(&VersionSpec::Stable, &[]).is_none());
    }

    #[test]
    fn activation_roundtrips_and_validates() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.4.2"], Os::Linux);

        assert!(active_name(&paths, RuntimeKind::Php).unwrap().is_none());
        set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();
        assert_eq!(
            active_name(&paths, RuntimeKind::Php).unwrap().as_deref(),
            Some("8.4.2")
        );
        assert_eq!(
            active(&paths, RuntimeKind::Php).unwrap().unwrap().name,
            "8.4.2"
        );

        // Cannot activate something that is not installed.
        assert!(set_active(&paths, RuntimeKind::Php, "7.4.33").is_err());

        // A stale marker resolves to an error, not a panic.
        fs::remove_dir_all(paths.runtime_version_dir(RuntimeKind::Php, "8.4.2")).unwrap();
        assert!(matches!(
            active(&paths, RuntimeKind::Php).unwrap_err(),
            Error::RuntimeNotInstalled { .. }
        ));
    }

    #[test]
    fn resolve_prefers_a_pinned_version_over_the_active_marker() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.3.14", "8.4.2"], Os::Linux);
        set_active(&paths, RuntimeKind::Php, "8.3.14").unwrap();

        let pinned: VersionSpec = "8.4".parse().unwrap();
        assert_eq!(
            resolve(&paths, RuntimeKind::Php, &pinned)
                .unwrap()
                .unwrap()
                .name,
            "8.4.2"
        );
        assert_eq!(
            resolve(&paths, RuntimeKind::Php, &VersionSpec::Stable)
                .unwrap()
                .unwrap()
                .name,
            "8.3.14",
            "the active marker wins for channel specs"
        );
        assert!(
            resolve(&paths, RuntimeKind::Php, &"9.0".parse().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn remove_deletes_only_version_directories() {
        let (_temp, paths) = home_with(RuntimeKind::Php, &["8.3.14", "8.4.2"], Os::Linux);
        set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();

        remove(&paths, RuntimeKind::Php, "8.4.2").unwrap();
        assert!(
            !paths
                .runtime_version_dir(RuntimeKind::Php, "8.4.2")
                .exists()
        );
        // Removing the active runtime also clears the marker.
        assert!(active_name(&paths, RuntimeKind::Php).unwrap().is_none());

        for bad in ["", "..", "8.4/../../etc", ACTIVE_FILE, "9.9.9"] {
            assert!(
                remove(&paths, RuntimeKind::Php, bad).is_err(),
                "`{bad}` must be refused"
            );
        }
    }

    #[test]
    fn incomplete_installations_are_detected() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
        fs::create_dir_all(&dir).unwrap();

        let runtime = InstalledRuntime {
            kind: RuntimeKind::Php,
            name: "8.4.2".to_owned(),
            version: Version::parse("8.4.2").unwrap(),
            path: dir.clone(),
        };
        assert!(!runtime.is_complete(Os::Windows));

        fs::write(dir.join("php.exe"), b"MZ").unwrap();
        assert!(runtime.is_complete(Os::Windows));
        assert_eq!(
            runtime.server_executable(Os::Windows),
            Some(dir.join("php.exe"))
        );
    }

    #[test]
    fn executables_are_found_in_nested_layouts() {
        let temp = TempDir::new();
        let dir = temp.path().join("mariadb");
        fs::create_dir_all(dir.join("bin")).unwrap();
        let server = dir.join("bin").join(Os::Linux.executable_name("mariadbd"));
        fs::write(&server, b"#!/bin/sh\n").unwrap();
        mark_executable(&server);

        assert_eq!(
            locate_executable(&dir, Os::Linux, RuntimeKind::Mariadb.server_executables()),
            Some(dir.join("bin").join("mariadbd")),
            "the `bin/mariadbd` candidate must match"
        );

        // A layout no candidate names is still found by the bounded scan.
        let odd = temp.path().join("odd");
        fs::create_dir_all(odd.join("wrapper")).unwrap();
        let php = odd.join("wrapper").join("php");
        fs::write(&php, b"#!/bin/sh\n").unwrap();
        mark_executable(&php);
        assert_eq!(
            locate_executable(&odd, Os::Linux, RuntimeKind::Php.server_executables()),
            Some(php)
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_files_are_not_runtimes() {
        let temp = TempDir::new();
        let path = temp.path().join("php");
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();

        assert!(!is_executable_file(&path, Os::Linux));
        // On Windows the extension is what counts, so the same file is
        // accepted there once it carries one.
        let windows_path = temp.path().join("php.exe");
        fs::write(&windows_path, b"MZ").unwrap();
        assert!(is_executable_file(&windows_path, Os::Windows));
    }

    #[test]
    fn kind_metadata_is_complete_for_every_family() {
        for kind in RuntimeKind::ALL {
            assert!(!kind.as_str().is_empty());
            assert!(!kind.display_name().is_empty());
            // Every family Lambo installs is installed by a `lambo` command;
            // Composer is the one it only detects, so it points upstream.
            if kind == RuntimeKind::Composer {
                assert!(
                    kind.install_command()
                        .starts_with("install Composer from https://")
                );
            } else {
                assert!(
                    kind.install_command().starts_with("lambo "),
                    "`{}` has no lambo install command",
                    kind.as_str()
                );
            }
            assert!(!kind.server_executables().is_empty());
            assert!(!kind.client_executables().is_empty());
            if kind.is_database() {
                assert!(!kind.init_executables().is_empty());
                assert!(!kind.admin_executables().is_empty());
            }
        }
        assert!(RuntimeKind::Mariadb.is_database());
        assert!(!RuntimeKind::Php.is_database());
    }
}
