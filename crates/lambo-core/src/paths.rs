//! Filesystem layout of a Lambo PHP installation.
//!
//! Lambo follows the *single home directory* model (like rustup and Bun):
//! everything it owns lives under one root directory. Setting `LAMBO_HOME`
//! relocates the whole installation, which is what makes portable and
//! no-install modes trivial; see `docs/adr/0003-single-home-directory.md`.
//!
//! Defaults are platform-idiomatic, never hardcoded to a drive letter:
//!
//! | Platform | Default root                    |
//! |----------|---------------------------------|
//! | Windows  | `%USERPROFILE%\Lambo`           |
//! | Linux    | `$HOME/.lambo`                  |
//! | macOS    | `$HOME/.lambo`                  |
//!
//! Layout (the shape the Windows installer creates as `C:\Lambo`):
//!
//! ```text
//! $LAMBO_HOME/
//! ├── apache/     # managed Apache httpd installations
//! ├── bin/        # shims added to PATH (optional)
//! ├── cache/      # downloaded runtime archives
//! ├── config/     # lambo.yml, workspaces.yml, apache/, catalogs/
//! ├── data/       # service state, database data directory, database manager
//! ├── database/   # managed MariaDB/MySQL installations
//! ├── logs/       # apache/, database/, lambo/
//! ├── php/        # one directory per installed PHP version
//! └── projects/   # default location for new projects
//! ```

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::platform::Os;
use crate::runtime::RuntimeKind;

/// Environment variable that relocates the Lambo home directory.
pub const HOME_ENV: &str = "LAMBO_HOME";

/// Environment variable honoured for backwards compatibility with king-PHP.
///
/// Used *only* when `LAMBO_HOME` is unset, and only after
/// [`crate::migration`] has offered to move the installation.
pub const LEGACY_HOME_ENV: &str = "KING_HOME";

/// Name of the default home directory inside the user's home on Windows.
pub const DEFAULT_HOME_DIR_WINDOWS: &str = "Lambo";

/// Name of the default home directory inside the user's home on Unix.
pub const DEFAULT_HOME_DIR_UNIX: &str = ".lambo";

/// Name of the per-project configuration file.
pub const PROJECT_FILE: &str = "lambo.yml";

/// Name of the global configuration file.
pub const CONFIG_FILE: &str = "lambo.yml";

/// A resolved view of Lambo's filesystem layout.
///
/// Constructing a `Paths` performs no I/O; directories are created only by
/// [`Paths::ensure_layout`]. This keeps every consumer testable: tests point
/// `Paths` at a temporary directory and nothing escapes into the real system.
/// All paths are built with [`Path::join`], so the same code produces
/// `C:\Lambo\php\8.4` on Windows and `/home/u/.lambo/php/8.4` on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Resolves the effective home directory for this machine.
    ///
    /// Precedence: `LAMBO_HOME` (when set and non-empty), then the platform
    /// default.
    ///
    /// The king-PHP `KING_HOME` variable is deliberately *not* consulted.
    /// Inheriting it would make a fresh Lambo installation write its
    /// configuration, runtimes and state into a directory named after the old
    /// tool, which is the opposite of what an upgrade should produce. The
    /// legacy home is reported by [`Paths::legacy_root`] so `lambo migrate` can
    /// offer a one-way upgrade instead.
    pub fn detect() -> Result<Self> {
        Self::detect_with(env::var_os(HOME_ENV).as_deref(), user_home(), Os::host())
    }

    /// Environment-free resolution, so the precedence rules are testable on
    /// any host (notably the Windows ones, which cannot run on Linux CI).
    pub fn detect_with(
        lambo_home: Option<&OsStr>,
        user_home: Option<PathBuf>,
        os: Os,
    ) -> Result<Self> {
        if let Some(root) = lambo_home.filter(|value| !value.is_empty()) {
            return Ok(Self::from_root(root));
        }
        let home = user_home
            .filter(|value| !value.as_os_str().is_empty())
            .ok_or(Error::NoHomeDir)?;
        Ok(Self::from_root(crate::platform::default_data_dir(
            os, home,
        )?))
    }

    /// Uses an explicit root directory (tests, portable installations).
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory of this installation.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for user-facing binaries and shims (`bin/`).
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Cache of downloaded runtime archives (`cache/`).
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Configuration directory (`config/`).
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// Machine-managed data that is neither configuration nor logs (`data/`).
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Unified log directory (`logs/`).
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Default directory for new projects (`projects/`).
    ///
    /// Only a default: `paths.projects` in the global configuration wins,
    /// and `lambo init` works in any directory regardless.
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// Managed Apache httpd installations (`apache/`).
    pub fn apache_dir(&self) -> PathBuf {
        self.root.join("apache")
    }

    /// Installed PHP runtimes (`php/`), one subdirectory per version.
    pub fn php_dir(&self) -> PathBuf {
        self.root.join("php")
    }

    /// Root of database engine installations (`database/`).
    pub fn database_dir(&self) -> PathBuf {
        self.root.join("database")
    }

    /// Installed runtimes of one family.
    ///
    /// PHP, Apache and Composer each get a top-level directory; database
    /// engines share `database/` and are separated by engine name so a
    /// MariaDB and a MySQL installation can coexist.
    pub fn runtime_dir(&self, kind: RuntimeKind) -> PathBuf {
        match kind {
            RuntimeKind::Php => self.php_dir(),
            RuntimeKind::Apache => self.apache_dir(),
            RuntimeKind::Composer => self.root.join("composer"),
            RuntimeKind::Mariadb => self.database_dir().join("mariadb"),
            RuntimeKind::Mysql => self.database_dir().join("mysql"),
        }
    }

    /// Directory of one installed runtime version.
    pub fn runtime_version_dir(&self, kind: RuntimeKind, version: &str) -> PathBuf {
        self.runtime_dir(kind).join(version)
    }

    /// Generated Apache configuration (`config/apache/`).
    ///
    /// Users never edit this: `lambo server start` regenerates it from
    /// `lambo.yml` every time.
    pub fn apache_config_dir(&self) -> PathBuf {
        self.config_dir().join("apache")
    }

    /// The generated main Apache configuration file.
    pub fn apache_config_file(&self) -> PathBuf {
        self.apache_config_dir().join("httpd.conf")
    }

    /// Directory holding one generated virtual host per project.
    pub fn apache_vhosts_dir(&self) -> PathBuf {
        self.apache_config_dir().join("vhosts")
    }

    /// Runtime files Apache needs to own: PID file, scoreboard, mutexes.
    ///
    /// Keeping these out of the Apache installation directory is what lets a
    /// system-wide Apache and a Lambo Apache run side by side.
    pub fn apache_run_dir(&self) -> PathBuf {
        self.data_dir().join("apache")
    }

    /// Instance state of the local database server (`data/database/`).
    pub fn database_state_dir(&self) -> PathBuf {
        self.data_dir().join("database")
    }

    /// Data directory (`datadir`) of the local database server.
    pub fn database_data_dir(&self) -> PathBuf {
        self.database_state_dir().join("data")
    }

    /// Temporary directory handed to the database server.
    ///
    /// Windows database servers refuse the system temp directory when it
    /// lives on another volume or carries unusual ACLs, so Lambo always
    /// provides its own.
    pub fn database_tmp_dir(&self) -> PathBuf {
        self.database_state_dir().join("tmp")
    }

    /// Generated database configuration file.
    pub fn database_config_file(&self) -> PathBuf {
        self.config_dir().join("database.cnf")
    }

    /// Where the bundled database manager (Adminer) lives.
    /// Where the database manager (phpMyAdmin by default) is installed.
    ///
    /// Named for the `dbui` family rather than for a manager, because
    /// discovery runs without the config in hand and must find whichever
    /// manager is installed.
    pub fn dbui_dir(&self) -> PathBuf {
        self.data_dir().join("dbui")
    }

    /// Download catalogues shipped with Lambo and user overrides.
    pub fn catalogs_dir(&self) -> PathBuf {
        self.config_dir().join("catalogs")
    }

    /// State of the services Lambo currently owns.
    pub fn state_file(&self) -> PathBuf {
        self.data_dir().join("services.yml")
    }

    /// Log directory of one service group (`apache`, `database`, `lambo`).
    pub fn service_logs_dir(&self, service: &str) -> PathBuf {
        self.logs_dir().join(service)
    }

    /// Path of the global configuration file.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join(CONFIG_FILE)
    }

    /// Path of the workspaces registry.
    pub fn workspaces_file(&self) -> PathBuf {
        self.config_dir().join("workspaces.yml")
    }

    /// Every directory the layout guarantees to exist, in display order.
    pub fn layout_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.bin_dir(),
            self.cache_dir(),
            self.config_dir(),
            self.data_dir(),
            self.logs_dir(),
            self.projects_dir(),
            self.php_dir(),
            self.apache_dir(),
            self.database_dir(),
            self.apache_config_dir(),
            self.apache_vhosts_dir(),
            self.apache_run_dir(),
            self.database_state_dir(),
            self.dbui_dir(),
            self.catalogs_dir(),
            self.service_logs_dir("apache"),
            self.service_logs_dir("database"),
            self.service_logs_dir("lambo"),
        ]
    }

    /// Creates the full directory layout. Idempotent.
    pub fn ensure_layout(&self) -> Result<()> {
        for dir in self.layout_dirs() {
            fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
        }
        Ok(())
    }

    /// Returns `true` when every primary directory exists.
    pub fn layout_exists(&self) -> bool {
        self.layout_dirs().iter().all(|dir| dir.is_dir())
    }

    /// Directories whose absence means the installation is incomplete.
    ///
    /// [`Paths::layout_exists`] is strict (it includes log and cache
    /// directories); this is the subset a broken installation is diagnosed
    /// against.
    pub fn essential_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.config_dir(),
            self.data_dir(),
            self.logs_dir(),
            self.php_dir(),
            self.apache_dir(),
            self.database_dir(),
        ]
    }

    /// Root of a legacy king-PHP installation, when one exists.
    ///
    /// Used by `lambo doctor` and [`crate::migration`] to offer an upgrade;
    /// new installations never look here.
    pub fn legacy_root() -> Option<PathBuf> {
        env::var_os(LEGACY_HOME_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| user_home().map(|home| home.join(".king")))
            .filter(|path| path.is_dir())
    }
}

/// Whether `KING_HOME` is set in the environment.
///
/// Lambo never reads it for its own paths, but a user who still exports it is
/// worth telling: their shell profile mentions a tool that no longer exists.
pub fn legacy_home_env_is_set() -> bool {
    env::var_os(LEGACY_HOME_ENV).is_some_and(|value| !value.is_empty())
}

/// The user's home directory: `$HOME`, falling back to `%USERPROFILE%`.
///
/// Windows sets `USERPROFILE` (and usually `HOME` only for shells that
/// emulate Unix), so both are consulted in that order-independent way.
pub fn user_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Renders a path with forward slashes.
///
/// Apache, MySQL/MariaDB and PHP all accept forward slashes on Windows, and
/// their configuration parsers treat a backslash as an escape character.
/// Every generated configuration file goes through this helper so a Windows
/// path never has to be double-escaped by hand.
pub fn to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Quotes a path for a configuration file when it contains spaces.
///
/// `C:\Lambo\My Projects` must be written as `"C:/Lambo/My Projects"` in
/// Apache and MariaDB configuration files.
pub fn quote_for_config(path: &Path) -> String {
    let rendered = to_forward_slashes(path);
    if rendered.contains(' ') {
        format!("\"{rendered}\"")
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn from_root_resolves_all_subpaths() {
        let paths = Paths::from_root(r"C:\Lambo");
        assert_eq!(paths.root(), Path::new(r"C:\Lambo"));
        assert_eq!(
            paths.config_file(),
            Path::new(r"C:\Lambo").join("config/lambo.yml")
        );
        assert_eq!(
            paths.workspaces_file(),
            Path::new(r"C:\Lambo").join("config/workspaces.yml")
        );
        assert_eq!(paths.php_dir(), Path::new(r"C:\Lambo").join("php"));
        assert_eq!(
            paths.runtime_version_dir(RuntimeKind::Php, "8.4.2"),
            Path::new(r"C:\Lambo").join("php").join("8.4.2")
        );
        assert_eq!(
            paths.runtime_dir(RuntimeKind::Mariadb),
            Path::new(r"C:\Lambo").join("database").join("mariadb")
        );
        assert_eq!(
            paths.runtime_dir(RuntimeKind::Mysql),
            Path::new(r"C:\Lambo").join("database").join("mysql")
        );
        assert_eq!(
            paths.service_logs_dir("apache"),
            Path::new(r"C:\Lambo").join("logs/apache")
        );
        assert_eq!(
            paths.state_file(),
            Path::new(r"C:\Lambo").join("data/services.yml")
        );
    }

    #[test]
    fn unix_layout_uses_the_same_names() {
        let paths = Paths::from_root("/home/thio/.lambo");
        assert_eq!(
            paths.projects_dir(),
            Path::new("/home/thio/.lambo/projects")
        );
        assert_eq!(
            paths.database_data_dir(),
            Path::new("/home/thio/.lambo/data/database/data")
        );
        assert_eq!(
            paths.apache_config_file(),
            Path::new("/home/thio/.lambo/config/apache/httpd.conf")
        );
    }

    #[test]
    fn lambo_home_wins_over_everything() {
        let custom = PathBuf::from(r"D:\Development\Lambo");
        let paths = Paths::detect_with(
            Some(OsStr::new(&custom)),
            Some(PathBuf::from(r"C:\Users\Thio")),
            Os::Windows,
        )
        .unwrap();
        assert_eq!(paths.root(), custom);
    }

    #[test]
    fn a_king_home_is_never_used_as_the_lambo_home() {
        // A fresh Lambo installation must be Lambo-only: inheriting KING_HOME
        // would put config/lambo.yml and every runtime inside ~/.king.
        let paths =
            Paths::detect_with(None, Some(PathBuf::from(r"C:\Users\Thio")), Os::Windows).unwrap();
        assert_eq!(paths.root(), PathBuf::from(r"C:\Users\Thio").join("Lambo"));
        assert!(
            !paths.root().to_string_lossy().contains("king"),
            "{}",
            paths.root().display()
        );
    }

    #[test]
    fn empty_environment_values_are_ignored() {
        let home = PathBuf::from(r"C:\Users\Thio");
        let paths =
            Paths::detect_with(Some(OsStr::new("")), Some(home.clone()), Os::Windows).unwrap();
        // Expectations are built with `join` too: on a Unix host the
        // separator inside the Windows-style literal stays a literal.
        assert_eq!(paths.root(), home.join("Lambo"));
        assert!(paths.root().to_string_lossy().ends_with("Lambo"));
    }

    #[test]
    fn platform_default_is_used_without_environment() {
        let windows_home = PathBuf::from(r"C:\Users\Thio");
        let windows = Paths::detect_with(None, Some(windows_home.clone()), Os::Windows).unwrap();
        assert_eq!(windows.root(), windows_home.join("Lambo"));

        let linux = Paths::detect_with(None, Some(PathBuf::from("/home/thio")), Os::Linux).unwrap();
        assert_eq!(linux.root(), Path::new("/home/thio/.lambo"));
    }

    #[test]
    fn missing_home_directory_is_an_error_not_a_guess() {
        let err = Paths::detect_with(None, None, Os::Windows).unwrap_err();
        assert!(matches!(err, Error::NoHomeDir));
        assert!(err.to_string().contains("LAMBO_HOME"));
    }

    #[test]
    fn ensure_layout_is_idempotent() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path().join("home"));

        assert!(!paths.layout_exists());
        paths.ensure_layout().unwrap();
        assert!(paths.layout_exists());
        paths.ensure_layout().unwrap();
        assert!(paths.layout_exists());

        for dir in paths.essential_dirs() {
            assert!(dir.is_dir(), "{} must exist", dir.display());
        }
    }

    #[test]
    fn forward_slash_rendering_is_safe_for_generated_configs() {
        assert_eq!(
            to_forward_slashes(Path::new(r"C:\Lambo\php\8.4.2")),
            "C:/Lambo/php/8.4.2"
        );
        assert_eq!(
            to_forward_slashes(Path::new("/home/thio/.lambo")),
            "/home/thio/.lambo"
        );
        assert_eq!(
            quote_for_config(Path::new(r"C:\Lambo\My Projects")),
            "\"C:/Lambo/My Projects\""
        );
        assert_eq!(quote_for_config(Path::new(r"C:\Lambo")), "C:/Lambo");
    }

    #[test]
    fn constants_describe_the_new_naming() {
        assert_eq!(HOME_ENV, "LAMBO_HOME");
        assert_eq!(PROJECT_FILE, "lambo.yml");
        assert_eq!(CONFIG_FILE, "lambo.yml");
    }
}
