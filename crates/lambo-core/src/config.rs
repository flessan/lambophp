//! Global configuration: `$LAMBO_HOME/config/lambo.yml`.
//!
//! Users are never *required* to edit this file by hand - `lambo config
//! get/set` (and later the TUI and GUI) mutate it through this module - but
//! it stays human-readable and version-controllable.
//!
//! All validation of *values* happens in [`Config::set`], so every interface
//! gets identical behaviour for free. All *structure* is defaulted, so a file
//! written by an older Lambo keeps loading on a newer one.
//!
//! The file must stay portable: it never contains a platform-specific
//! absolute path except in `paths.projects`, which is explicitly a
//! per-machine override and is documented as such.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsx;
use crate::paths::Paths;
use crate::secret;
use crate::version::VersionSpec;
use crate::yaml;

/// Header written above the serialized YAML body.
const HEADER: &str = "\
# Lambo PHP global configuration
# Reference: https://github.com/flessan/lambophp/blob/main/docs/configuration.md
# Managed by `lambo config` - manual edits are welcome; invalid files are
# rejected with an explanation on the next run.
";

/// Web server implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ServerKind {
    /// Apache httpd - the XAMPP-alternative default.
    #[default]
    Apache,
    /// PHP's built-in development server (no Apache required).
    Php,
    /// nginx (planned; see docs/roadmap.md).
    Nginx,
}

impl ServerKind {
    /// Stable machine name, used in files and CLI output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Apache => "apache",
            Self::Php => "php",
            Self::Nginx => "nginx",
        }
    }

    /// Human-readable name for status output.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Apache => "Apache",
            Self::Php => "PHP server",
            Self::Nginx => "nginx",
        }
    }
}

impl fmt::Display for ServerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How to switch to PHP's built-in server, phrased so the advice works.
///
/// `lambo config set` writes the *global* configuration, and a project's
/// `lambo.yml` overrides it. Naming only that command therefore sends a user
/// whose project file pins `server.kind` - which is what `lambo init` writes -
/// to a fix that cannot possibly take effect. The project file has to be named
/// first.
pub const SWITCH_TO_PHP_SERVER: &str = "`server.kind: php` in lambo.yml switches to PHP's \
     built-in server; `lambo config set server.kind php` sets it globally, which a project's \
     lambo.yml overrides";

impl FromStr for ServerKind {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "apache" | "httpd" => Ok(Self::Apache),
            "php" | "php-server" => Ok(Self::Php),
            "nginx" => Ok(Self::Nginx),
            other => Err(Error::InvalidConfigValue {
                key: "server.kind".to_owned(),
                value: other.to_owned(),
                reason: "expected one of: apache, php, nginx".to_owned(),
            }),
        }
    }
}

/// Database engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DatabaseKind {
    /// No database; provisioning is skipped for the project.
    #[default]
    None,
    /// MariaDB (default choice; MySQL-compatible).
    Mariadb,
    /// Oracle MySQL.
    Mysql,
}

impl DatabaseKind {
    /// Stable machine name, used in files and CLI output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Mariadb => "mariadb",
            Self::Mysql => "mysql",
        }
    }

    /// Human-readable name for status output.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Mariadb => "MariaDB",
            Self::Mysql => "MySQL",
        }
    }

    /// The `DB_CONNECTION` value Laravel and friends expect.
    pub fn laravel_connection(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Mariadb => "mysql",
            Self::Mysql => "mysql",
        }
    }

    /// Whether provisioning should happen at all.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::None)
    }
}

impl fmt::Display for DatabaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DatabaseKind {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "disabled" => Ok(Self::None),
            "mariadb" => Ok(Self::Mariadb),
            "mysql" => Ok(Self::Mysql),
            other => Err(Error::InvalidConfigValue {
                key: "database.kind".to_owned(),
                value: other.to_owned(),
                reason: "expected one of: mariadb, mysql, none".to_owned(),
            }),
        }
    }
}

/// PHP-related configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PhpConfig {
    /// Default PHP version when a project does not pin one.
    pub default: VersionSpec,
}

impl Default for PhpConfig {
    fn default() -> Self {
        Self {
            default: VersionSpec::Stable,
        }
    }
}

/// Web server configuration.
///
/// Ports default to **80/443** so the product reads the way a user expects:
/// `http://localhost`, not `http://localhost:8080`. See
/// docs/adr/0007-localhost-first-ports.md, which supersedes ADR-0005.
///
/// Windows has no privileged-port rule, so a standard user can bind 80 and the
/// default workflow needs no elevation. Linux and macOS do restrict ports below
/// 1024, and there Lambo falls back to 8080 automatically rather than demanding
/// root - see [`crate::port::resolve_listen_port`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Which server implementation to run.
    pub kind: ServerKind,
    /// Plain HTTP port.
    ///
    /// `http_port` is accepted as an alias so configuration files written by
    /// king-PHP keep working after the rename.
    #[serde(alias = "http_port")]
    pub port: u16,
    /// HTTPS port for TLS-enabled projects.
    pub https_port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            kind: ServerKind::Apache,
            port: 80,
            https_port: 443,
        }
    }
}

/// Database configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Default engine for newly initialized projects.
    pub kind: DatabaseKind,
    /// TCP port the database listens on.
    pub port: u16,
    /// Administrative user of the local server.
    pub username: String,
    /// Password of [`DatabaseConfig::username`].
    ///
    /// Generated per installation on first use (see [`crate::secret`]) and
    /// never hardcoded. An empty value means "not generated yet"; run
    /// [`Config::ensure_credentials`] (which `lambo db install` and
    /// `lambo up` do) to fill it in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            kind: DatabaseKind::Mariadb,
            port: 3306,
            username: "root".to_owned(),
            password: String::new(),
        }
    }
}

/// Configuration of the bundled database manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DbUiConfig {
    /// Which manager to serve.
    ///
    /// **phpMyAdmin** is the default: it is the manager users arriving from
    /// XAMPP expect, and it is what `http://localhost/phpmyadmin` promises.
    /// Adminer remains selectable for anyone who prefers a single-file manager.
    pub kind: String,
    /// Port the manager listens on when it is served on its own.
    ///
    /// Under Apache the manager is aliased into the project's site at
    /// `/phpmyadmin`, so this port is not used and the user never sees it.
    pub port: u16,
}

impl Default for DbUiConfig {
    fn default() -> Self {
        Self {
            kind: "phpmyadmin".to_owned(),
            port: 8081,
        }
    }
}

/// Per-machine path overrides.
///
/// Everything else about the layout is derived from `LAMBO_HOME`; these are
/// the two knobs users actually want.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Where new projects are created. Empty means `$LAMBO_HOME/projects`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub projects: String,
}

/// How to pin an artifact's digest, phrased so the advice works.
///
/// There is no `lambo config set <family>.<version>.sha256` key, and adding one
/// would duplicate the catalogue override mechanism that already exists. The
/// override file is the real place a digest goes, so that is what the advice
/// names. `lambo config hash` computes the value; it deliberately does not
/// record it, because a digest is a claim a human has to make.
pub const PIN_A_DIGEST: &str = "`config/catalogs/<family>.json` is where a pinned \
     `sha256` goes, and `lambo config hash <file>` computes the value";

/// Where runtime artifacts are fetched from.
///
/// Both fields are optional and empty by default, which means "the official
/// catalogue". Nothing here can weaken verification: a mirror changes where the
/// bytes come from, never what digest they must match. See
/// [`crate::sources`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    /// Base URL of a mirror holding the same artifacts as the official hosts.
    ///
    /// Artifacts are addressed by name (`<mirror>/<artifact>`), so a mirror is a
    /// flat directory of files - which is what an internal artifact store or a
    /// directory share actually is. Must be `https://`, or `file://` for a
    /// mounted share.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mirror: String,
    /// Directory of pre-downloaded artifacts. Empty means
    /// `$LAMBO_HOME/cache/artifacts`.
    ///
    /// A file is only used when its name is the one Lambo derives from the
    /// artifact's identity, and it is still verified against the catalogue's
    /// digest before anything is unpacked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifacts: String,
}

/// Browser behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Open the project URL automatically after `lambo up`.
    pub open: bool,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self { open: true }
    }
}

/// The global configuration document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// PHP defaults.
    pub php: PhpConfig,
    /// Web server settings.
    pub server: ServerConfig,
    /// Database settings.
    pub database: DatabaseConfig,
    /// Database manager settings.
    pub dbui: DbUiConfig,
    /// Path overrides.
    pub paths: PathsConfig,
    /// Browser behaviour.
    pub browser: BrowserConfig,
    /// Where runtime artifacts come from.
    pub sources: SourcesConfig,
}

impl Config {
    /// Keys understood by [`Config::get`] and [`Config::set`], in help order.
    pub const KEYS: &'static [&'static str] = &[
        "php.default",
        "server.kind",
        "server.port",
        "server.https_port",
        "database.kind",
        "database.port",
        "database.username",
        "database.password",
        "dbui.kind",
        "dbui.port",
        "paths.projects",
        "browser.open",
        "sources.mirror",
        "sources.artifacts",
    ];

    /// Loads the configuration, falling back to defaults when no file
    /// exists yet. A malformed file is an error, never silently ignored.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        match fs::read_to_string(&path) {
            Ok(text) => yaml::from_str(&text).map_err(|source| Error::Yaml { path, source }),
            Err(source) if source.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::io(path, source)),
        }
    }

    /// Loads the configuration and persists it when it did not exist.
    ///
    /// This is what `lambo init` and `lambo up` use, so a fresh installation
    /// gets a file (with generated credentials) instead of a directory full
    /// of implicit defaults.
    pub fn load_or_init(paths: &Paths) -> Result<Self> {
        let mut config = Self::load(paths)?;
        if config.ensure_credentials() {
            config.save(paths)?;
        }
        Ok(config)
    }

    /// Persists the configuration atomically, with the standard header.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let body = self.to_yaml()?;
        fsx::write_atomic(&paths.config_file(), &format!("{HEADER}{body}"))
    }

    /// Serializes the effective configuration to YAML (without the header).
    pub fn to_yaml(&self) -> Result<String> {
        yaml::to_string(self).map_err(Error::Serialize)
    }

    /// Fills in the database password when it is still empty.
    ///
    /// Returns `true` when something changed, so callers know whether the
    /// configuration has to be written back.
    pub fn ensure_credentials(&mut self) -> bool {
        if self.database.username.is_empty() {
            self.database.username = "root".to_owned();
        }
        if self.database.password.is_empty() {
            self.database.password = secret::database_password();
            return true;
        }
        false
    }

    /// The effective projects directory.
    pub fn projects_dir(&self, paths: &Paths) -> PathBuf {
        if self.paths.projects.is_empty() {
            paths.projects_dir()
        } else {
            PathBuf::from(&self.paths.projects)
        }
    }

    /// Reads one value as a string; see [`Config::KEYS`].
    pub fn get(&self, key: &str) -> Result<String> {
        match key {
            "php.default" => Ok(self.php.default.to_string()),
            "server.kind" => Ok(self.server.kind.as_str().to_owned()),
            "server.port" => Ok(self.server.port.to_string()),
            "server.https_port" => Ok(self.server.https_port.to_string()),
            "database.kind" => Ok(self.database.kind.as_str().to_owned()),
            "database.port" => Ok(self.database.port.to_string()),
            "database.username" => Ok(self.database.username.clone()),
            "database.password" => Ok(self.database.password.clone()),
            "dbui.kind" => Ok(self.dbui.kind.clone()),
            "dbui.port" => Ok(self.dbui.port.to_string()),
            "paths.projects" => Ok(self.paths.projects.clone()),
            "browser.open" => Ok(self.browser.open.to_string()),
            "sources.mirror" => Ok(self.sources.mirror.clone()),
            "sources.artifacts" => Ok(self.sources.artifacts.clone()),
            _ => Err(Error::UnknownConfigKey(key.to_owned())),
        }
    }

    /// Validates and writes one value; see [`Config::KEYS`].
    ///
    /// This is the single mutator every interface uses, which is what
    /// guarantees the TUI and GUI can never accept a value the CLI rejects.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "php.default" => self.php.default = value.parse()?,
            "server.kind" => self.server.kind = value.parse()?,
            "server.port" => self.server.port = parse_port(key, value)?,
            "server.https_port" => self.server.https_port = parse_port(key, value)?,
            "database.kind" => self.database.kind = value.parse()?,
            "database.port" => self.database.port = parse_port(key, value)?,
            "database.username" => self.database.username = parse_username(key, value)?,
            "database.password" => self.database.password = parse_password(key, value)?,
            "dbui.kind" => self.dbui.kind = parse_dbui_kind(key, value)?,
            "dbui.port" => self.dbui.port = parse_port(key, value)?,
            "paths.projects" => self.paths.projects = parse_projects_dir(key, value)?,
            "browser.open" => self.browser.open = parse_bool(key, value)?,
            "sources.mirror" => self.sources.mirror = parse_mirror(key, value)?,
            "sources.artifacts" => self.sources.artifacts = parse_artifacts_dir(key, value)?,
            _ => return Err(Error::UnknownConfigKey(key.to_owned())),
        }
        Ok(())
    }
}

/// Parses a TCP port, rejecting `0` (and anything above 65535 via `u16`).
fn parse_port(key: &str, value: &str) -> Result<u16> {
    let invalid = |reason: &str| invalid_value(key, value, reason);
    let port: u16 = value
        .trim()
        .parse()
        .map_err(|_| invalid("expected a number 1-65535"))?;
    if port == 0 {
        return Err(invalid("port 0 is not a listenable port"));
    }
    Ok(port)
}

/// Parses strict `true`/`false` so typos like `ture` fail loudly.
fn parse_bool(key: &str, value: &str) -> Result<bool> {
    value
        .trim()
        .parse()
        .map_err(|_| invalid_value(key, value, "expected `true` or `false`"))
}

/// Parses a database user name.
///
/// The name ends up in `.env` files and on MySQL command lines, so quotes
/// and whitespace are rejected instead of escaped.
fn parse_username(key: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid_value(key, value, "must not be empty"));
    }
    if trimmed
        .chars()
        .any(|c| matches!(c, ' ' | '"' | '\'' | '`' | '\\'))
    {
        return Err(invalid_value(
            key,
            value,
            "must not contain spaces or quotes",
        ));
    }
    Ok(trimmed.to_owned())
}

/// Parses a database password.
///
/// Empty clears the stored password (it is regenerated on the next
/// `lambo db install`); characters that would break a `.env` file or a
/// command line are rejected with an explanation.
fn parse_password(key: &str, value: &str) -> Result<String> {
    if value
        .chars()
        .any(|c| matches!(c, '\n' | '\r' | '"' | '\'' | '#' | '\\'))
    {
        return Err(invalid_value(
            key,
            "<redacted>",
            "must not contain quotes, comments or line breaks",
        ));
    }
    Ok(value.to_owned())
}

/// Validates the database manager name.
fn parse_dbui_kind(key: &str, value: &str) -> Result<String> {
    let trimmed = value.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "adminer" | "phpmyadmin" => Ok(trimmed),
        _ => Err(invalid_value(
            key,
            value,
            "expected one of: adminer, phpmyadmin",
        )),
    }
}

/// Validates the projects directory override.
///
/// An empty value resets to the default. The directory is not required to
/// exist yet - `lambo init` creates it - but it must be usable as a path.
fn parse_projects_dir(key: &str, value: &str) -> Result<String> {
    let trimmed = value.trim().trim_matches('"').to_owned();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.chars().any(|c| c == '\0' || c == '\n') {
        return Err(invalid_value(
            key,
            value,
            "contains characters that are not valid in a path",
        ));
    }
    Ok(trimmed)
}

/// Validates a mirror base URL.
///
/// The scheme is checked here rather than at download time so a typo is
/// reported by the command that made it. `https://` is required; `file://` is
/// allowed for a mounted share, which is how an air-gapped lab usually serves
/// artifacts. Plain `http://` is rejected: a mirror that can be tampered with
/// in transit is not a mirror, it is a way to install something else.
fn parse_mirror(key: &str, value: &str) -> Result<String> {
    let trimmed = value.trim().trim_matches('"').to_owned();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let scheme_ok = trimmed.starts_with("https://") || trimmed.starts_with("file://");
    if !scheme_ok {
        return Err(invalid_value(
            key,
            value,
            "must start with https:// (or file:// for a mounted share)",
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(invalid_value(
            key,
            value,
            "contains characters that are not valid in a URL",
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

/// Validates the local artifacts directory.
fn parse_artifacts_dir(key: &str, value: &str) -> Result<String> {
    let trimmed = value.trim().trim_matches('"').to_owned();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.chars().any(|c| c == '\0' || c == '\n') {
        return Err(invalid_value(
            key,
            value,
            "contains characters that are not valid in a path",
        ));
    }
    Ok(trimmed)
}

/// Builds an [`Error::InvalidConfigValue`].
fn invalid_value(key: &str, value: &str, reason: &str) -> Error {
    Error::InvalidConfigValue {
        key: key.to_owned(),
        value: value.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn defaults_are_localhost_first() {
        let config = Config::default();
        assert_eq!(config.php.default, VersionSpec::Stable);
        assert_eq!(config.server.kind, ServerKind::Apache);
        // Port 80 so the URL is `http://localhost`, which is what the product
        // promises. Windows binds 80 without elevation; on Unix the fallback in
        // `port::resolve_listen_port` keeps the first run working without root,
        // so this default does not reintroduce a privilege requirement.
        assert_eq!(
            config.server.port, 80,
            "the default must give http://localhost, not a port the user has to remember"
        );
        assert_eq!(config.server.https_port, 443);
        assert_eq!(config.database.kind, DatabaseKind::Mariadb);
        assert_eq!(config.database.port, 3306);
        assert_eq!(config.database.username, "root");
        assert!(
            config.database.password.is_empty(),
            "no password may be baked in"
        );
        assert_eq!(config.dbui.port, 8081);
        assert!(config.browser.open);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        paths.ensure_layout().unwrap();

        let mut config = Config::default();
        config.php.default = "8.4".parse().unwrap();
        config.database.kind = DatabaseKind::Mysql;
        config.ensure_credentials();
        config.save(&paths).unwrap();

        let raw = fs::read_to_string(paths.config_file()).unwrap();
        assert!(raw.starts_with("# Lambo PHP global configuration"));
        assert!(raw.contains("database:"));
        assert!(
            !raw.contains("king"),
            "no legacy naming may leak into new files"
        );

        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded, config);
        assert!(!loaded.database.password.is_empty());
    }

    #[test]
    fn missing_file_loads_as_defaults() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        assert_eq!(Config::load(&paths).unwrap(), Config::default());
        assert!(
            !paths.config_file().exists(),
            "load must not create the file"
        );
    }

    #[test]
    fn load_or_init_writes_generated_credentials_once() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());

        let first = Config::load_or_init(&paths).unwrap();
        assert!(!first.database.password.is_empty());
        assert!(paths.config_file().exists());

        let second = Config::load_or_init(&paths).unwrap();
        assert_eq!(second.database.password, first.database.password);
    }

    #[test]
    fn get_and_set_agree_on_every_key() {
        let mut config = Config::default();
        for key in Config::KEYS {
            let before = config.get(key).unwrap();
            // Setting the current value must be accepted and change nothing.
            config.set(key, &before).unwrap();
            assert_eq!(
                config.get(key).unwrap(),
                before,
                "key {key} did not round-trip"
            );
        }
    }

    #[test]
    fn set_validates_every_kind_of_value() {
        let mut config = Config::default();

        config.set("php.default", "8.4").unwrap();
        assert_eq!(config.get("php.default").unwrap(), "~8.4");

        config.set("server.kind", "Apache").unwrap();
        assert_eq!(config.server.kind, ServerKind::Apache);
        assert!(config.set("server.kind", "iis").is_err());

        config.set("server.port", "8081").unwrap();
        assert_eq!(config.server.port, 8081);
        for bad in ["0", "70000", "eighty", ""] {
            assert!(
                config.set("server.port", bad).is_err(),
                "`{bad}` must be rejected"
            );
        }

        config.set("database.kind", "mysql").unwrap();
        assert_eq!(config.database.kind, DatabaseKind::Mysql);
        assert!(config.set("database.kind", "postgres").is_err());

        config.set("database.username", " lambo ").unwrap();
        assert_eq!(config.database.username, "lambo");
        assert!(config.set("database.username", "root user").is_err());

        assert!(config.set("database.password", "with\"quote").is_err());
        config.set("database.password", "s3cret").unwrap();
        assert_eq!(config.database.password, "s3cret");

        config.set("dbui.kind", "Adminer").unwrap();
        assert_eq!(config.dbui.kind, "adminer", "the kind is lowercased");
        assert!(config.set("dbui.kind", "dbeaver").is_err());

        config.set("browser.open", "false").unwrap();
        assert!(!config.browser.open);
        assert!(config.set("browser.open", "ture").is_err());

        assert!(config.set("does.not.exist", "1").is_err());
    }

    #[test]
    fn legacy_http_port_key_still_loads() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::write(
            paths.config_file(),
            "server:\n  kind: apache\n  http_port: 9090\ndatabase:\n  kind: mariadb\n",
        )
        .unwrap();

        let config = Config::load(&paths).unwrap();
        assert_eq!(config.server.port, 9090, "king-PHP used `http_port`");
    }

    #[test]
    fn unknown_legacy_sections_are_ignored_not_fatal() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::write(
            paths.config_file(),
            "ssl:\n  auto: true\nworkspaces:\n  default: []\n",
        )
        .unwrap();
        let config = Config::load(&paths).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn projects_dir_falls_back_to_the_home_layout() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        let config = Config::default();
        assert_eq!(config.projects_dir(&paths), paths.projects_dir());

        let mut overridden = Config::default();
        overridden
            .set("paths.projects", r"D:\Development\Lambo")
            .unwrap();
        assert_eq!(
            overridden.projects_dir(&paths),
            PathBuf::from(r"D:\Development\Lambo")
        );

        overridden.set("paths.projects", "").unwrap();
        assert_eq!(overridden.projects_dir(&paths), paths.projects_dir());
    }

    #[test]
    fn database_kinds_map_to_laravel_connection_names() {
        assert_eq!(DatabaseKind::Mariadb.laravel_connection(), "mysql");
        assert_eq!(DatabaseKind::Mysql.laravel_connection(), "mysql");
        assert!(!DatabaseKind::None.is_enabled());
        assert!(DatabaseKind::Mariadb.is_enabled());
        assert_eq!("none".parse::<DatabaseKind>().unwrap(), DatabaseKind::None);
        assert_eq!("off".parse::<DatabaseKind>().unwrap(), DatabaseKind::None);
    }

    #[test]
    fn server_kinds_have_display_names() {
        assert_eq!(ServerKind::Apache.display_name(), "Apache");
        assert_eq!(ServerKind::Php.display_name(), "PHP server");
        assert_eq!("php".parse::<ServerKind>().unwrap(), ServerKind::Php);
        assert_eq!("httpd".parse::<ServerKind>().unwrap(), ServerKind::Apache);
    }
}
