//! The installation state of the control panel: `config.json`.
//!
//! This is the file that records which services exist, which are enabled,
//! which virtual hosts and projects the user has created, and the handful of
//! global settings that tie them together. It is the single source of truth
//! for the GUI's dashboard and for every service the engine can start, and it
//! is the one file a user is invited to edit by hand (the built-in editor has
//! a read-only view of it), so its shape is a compatibility surface.
//!
//! Ported from the original implementation's `config.go`. The JSON layout, the
//! field order, the
//! `omitempty` behaviour, the migrations and the `{base}` expansion are all
//! preserved exactly, because a configuration written by the previous
//! implementation must load here unchanged - and one written here must still
//! load there.
//!
//! # Differences that are deliberate
//!
//! * [`PanelConfig::save`] writes atomically through [`crate::fsx`]. Go used a
//!   plain truncating write, which meant a crash mid-save could leave a
//!   truncated `config.json` behind - a file that then fails to parse on the
//!   next launch, losing every project and virtual host. The resulting bytes
//!   are identical; only the failure mode changed.
//! * A nil slice is written as `[]` rather than `null`. Both re-parse to the
//!   same empty list.
//!
//! Nothing else about the format was touched.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// File name of the installation state, inside the install directory.
pub const CONFIG_FILE: &str = "config.json";

/// Schema version written into a freshly created configuration.
pub const CONFIG_VERSION: i64 = 1;

/// Web server the installation uses when nothing else is recorded.
pub const DEFAULT_WEB_SERVER: &str = "Apache";

/// Placeholder replaced by the install directory at use time.
pub const BASE_PLACEHOLDER: &str = "{base}";

/// Default port for a virtual host that does not name one.
pub const DEFAULT_VHOST_PORT: u16 = 80;

/// Name of the service that serves PHP through classic CGI.
pub const PHP_SERVICE: &str = "PHP-FPM";

/// Services booted by "Start Stack", in order.
///
/// The web server entry is replaced at run time by whichever server is active,
/// so switching to Nginx moves the whole stack with it rather than leaving a
/// stopped Apache in the list.
pub const ESSENTIAL_SERVICES: [&str; 4] = ["Apache", PHP_SERVICE, "MySQL", "phpMyAdmin"];

/// The complete installation state.
///
/// Field order is the serialization order and matches the Go struct, so the
/// file a user sees keeps the same shape it always had.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelConfig {
    /// Schema version. Written as [`CONFIG_VERSION`] but never migrated on its
    /// own, matching the original.
    #[serde(default)]
    pub version: i64,
    /// Every service the panel knows about, in dashboard order.
    #[serde(default, deserialize_with = "crate::serde_defaults::null_is_empty")]
    pub services: Vec<ServiceConf>,
    /// Registered virtual hosts.
    #[serde(default, deserialize_with = "crate::serde_defaults::null_is_empty")]
    pub vhosts: Vec<Vhost>,
    /// Scaffolded projects.
    #[serde(default, deserialize_with = "crate::serde_defaults::null_is_empty")]
    pub projects: Vec<PanelProject>,
    /// Global settings.
    #[serde(default)]
    pub settings: PanelSettings,
}

/// One entry of the service dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConf {
    /// Display name and the key the catalogue is looked up by.
    pub name: String,
    /// Coarse category: `web`, `php`, `database`, `cache`, `tool`, `queue`,
    /// `storage`, `mail`, `runtime`. Drives the dashboard tab a card appears in.
    pub kind: String,
    /// Executable, with `{base}` left unexpanded. Empty for tools that are not
    /// processes (phpMyAdmin, Adminer, Composer, the language runtimes).
    #[serde(default)]
    pub exe: String,
    /// Arguments, with `{base}` left unexpanded.
    #[serde(
        default,
        deserialize_with = "crate::serde_defaults::null_is_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub args: Vec<String>,
    /// Port the service listens on, when it has one.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub port: u16,
    /// Working directory, with `{base}` left unexpanded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workdir: String,
    /// Configuration file the card's Conf button opens.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub config_file: String,
    /// Whether the panel may start this service.
    #[serde(default)]
    pub enabled: bool,
    /// URL the card opens for tools that are not processes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub open_url: String,
    /// Selected variant for multi-version services (Node.js, Python). Empty
    /// means the catalogue's default version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_version: String,
    /// Extra environment variables, with `{base}` left unexpanded.
    #[serde(
        default,
        deserialize_with = "crate::serde_defaults::null_is_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub env: Vec<String>,
}

/// One registered virtual host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vhost {
    /// Domain name, e.g. `myapp.test`.
    #[serde(default)]
    pub domain: String,
    /// Document root, with `{base}` left unexpanded.
    #[serde(default)]
    pub docroot: String,
    /// Port the vhost is served on. Always written, even when zero.
    #[serde(default)]
    pub port: u16,
    /// `apache`, `nginx`, `both`, or empty for `apache`.
    #[serde(default)]
    pub server_type: String,
    /// Whether the vhost is written into the hosts file and server configs.
    #[serde(default)]
    pub enabled: bool,
    /// When set, the vhost is a reverse proxy to this loopback port instead of
    /// a document root. Used by the Node/Python/Go dev-server frameworks.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub proxy_port: u16,
}

/// One scaffolded project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelProject {
    /// Directory name under `www/`.
    #[serde(default)]
    pub name: String,
    /// Framework display name, matching a [`crate::framework`] entry.
    #[serde(default)]
    pub framework: String,
    /// Domain the project answers on.
    #[serde(default)]
    pub domain: String,
    /// Absolute document root.
    #[serde(default)]
    pub docroot: String,
    /// Reverse-proxy port for dev-server frameworks, zero otherwise.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub port: u16,
}

/// Global settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelSettings {
    /// Services started automatically when the window is created.
    #[serde(default, deserialize_with = "crate::serde_defaults::null_is_empty")]
    pub auto_start: Vec<String>,
    /// Override for the hosts file path. Empty means the platform default.
    #[serde(default)]
    pub hosts_file: String,
    /// File the managed Apache virtual hosts are written to.
    #[serde(default)]
    pub apache_vhosts_include: String,
    /// Directory the managed Nginx site files are written to.
    #[serde(default)]
    pub nginx_sites_dir: String,
    /// Which web server is active: `Apache` or `Nginx`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_web_server: String,
}

/// `true` for the ports that Go's `omitempty` would leave out.
fn is_zero_u16(value: &u16) -> bool {
    *value == 0
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self::default_config()
    }
}

impl PanelConfig {
    /// The configuration a fresh installation starts from.
    ///
    /// Go's `DefaultConfig(baseDir)` took a base directory it never used - every
    /// path is stored with a literal `{base}` placeholder - so the parameter is
    /// not carried over.
    pub fn default_config() -> Self {
        use ServiceConf as S;

        let service = |name: &str, kind: &str| S {
            name: name.to_owned(),
            kind: kind.to_owned(),
            exe: String::new(),
            args: Vec::new(),
            port: 0,
            workdir: String::new(),
            config_file: String::new(),
            enabled: false,
            open_url: String::new(),
            active_version: String::new(),
            env: Vec::new(),
        };
        let runtime = |name: &str| service(name, "runtime");

        Self {
            version: CONFIG_VERSION,
            services: vec![
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/apache/bin/httpd.exe"),
                    workdir: format!("{BASE_PLACEHOLDER}/bin/apache"),
                    config_file: format!("{BASE_PLACEHOLDER}/bin/apache/conf/httpd.conf"),
                    port: 80,
                    enabled: true,
                    ..service("Apache", "web")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/nginx/nginx.exe"),
                    workdir: format!("{BASE_PLACEHOLDER}/bin/nginx"),
                    config_file: format!("{BASE_PLACEHOLDER}/bin/nginx/conf/nginx.conf"),
                    port: 8080,
                    ..service("Nginx", "web")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/php/php-cgi.exe"),
                    args: vec!["-b".to_owned(), "127.0.0.1:9000".to_owned()],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/php"),
                    config_file: format!("{BASE_PLACEHOLDER}/bin/php/php.ini"),
                    port: 9000,
                    ..service("PHP-FPM", "php")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/mysql/bin/mysqld.exe"),
                    args: vec![
                        "--console".to_owned(),
                        format!("--basedir={BASE_PLACEHOLDER}/bin/mysql"),
                        format!("--datadir={BASE_PLACEHOLDER}/bin/mysql/data"),
                    ],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/mysql"),
                    config_file: format!("{BASE_PLACEHOLDER}/bin/mysql/my.ini"),
                    port: 3306,
                    enabled: true,
                    ..service("MySQL", "database")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/pgsql/bin/postgres.exe"),
                    args: vec![
                        "-D".to_owned(),
                        format!("{BASE_PLACEHOLDER}/bin/pgsql/data"),
                    ],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/pgsql/bin"),
                    config_file: format!("{BASE_PLACEHOLDER}/bin/pgsql/data/postgresql.conf"),
                    port: 5432,
                    ..service("PostgreSQL", "database")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/redis/redis-server.exe"),
                    args: vec![format!("{BASE_PLACEHOLDER}/bin/redis/redis.windows.conf")],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/redis"),
                    config_file: format!("{BASE_PLACEHOLDER}/bin/redis/redis.windows.conf"),
                    port: 6379,
                    ..service("Redis", "cache")
                },
                S {
                    enabled: true,
                    open_url: "http://localhost/phpmyadmin/".to_owned(),
                    ..service("phpMyAdmin", "tool")
                },
                S {
                    enabled: true,
                    open_url: "http://localhost/adminer/".to_owned(),
                    ..service("Adminer", "tool")
                },
                S {
                    enabled: true,
                    ..service("Composer", "tool")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/pgweb/pgweb.exe"),
                    args: vec![
                        "--bind=127.0.0.1".to_owned(),
                        "--listen=8081".to_owned(),
                        "--url=postgres://postgres:postgres@localhost:5432/postgres?sslmode=disable"
                            .to_owned(),
                        "--skip-open".to_owned(),
                    ],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/pgweb"),
                    port: 8081,
                    open_url: "http://localhost:8081/".to_owned(),
                    ..service("pgweb", "web")
                },
                runtime("Erlang"),
                S {
                    exe: "cmd.exe".to_owned(),
                    args: vec![
                        "/c".to_owned(),
                        format!("{BASE_PLACEHOLDER}/bin/rabbitmq/sbin/rabbitmq-server.bat"),
                    ],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/rabbitmq"),
                    port: 5672,
                    open_url: "http://localhost:15672/".to_owned(),
                    env: vec![
                        format!("ERLANG_HOME={BASE_PLACEHOLDER}/bin/erlang"),
                        format!("RABBITMQ_BASE={BASE_PLACEHOLDER}/data/rabbitmq"),
                        "RABBITMQ_NODENAME=rabbit@localhost".to_owned(),
                    ],
                    ..service("RabbitMQ", "queue")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/minio/minio.exe"),
                    args: vec![
                        "server".to_owned(),
                        format!("{BASE_PLACEHOLDER}/data/minio"),
                        "--address".to_owned(),
                        ":9010".to_owned(),
                        "--console-address".to_owned(),
                        ":9011".to_owned(),
                    ],
                    workdir: format!("{BASE_PLACEHOLDER}/bin/minio"),
                    port: 9010,
                    open_url: "http://localhost:9011/".to_owned(),
                    ..service("MinIO", "storage")
                },
                S {
                    exe: format!("{BASE_PLACEHOLDER}/bin/mailpit/mailpit.exe"),
                    workdir: format!("{BASE_PLACEHOLDER}/bin/mailpit"),
                    port: 8025,
                    open_url: "http://localhost:8025/".to_owned(),
                    ..service("Mailpit", "mail")
                },
                runtime("Node.js"),
                runtime("Python"),
                runtime("Go"),
                runtime("Java"),
                runtime("Julia"),
                runtime("Zig"),
                runtime("Dart"),
                runtime("Lua"),
                runtime("Ruby"),
                runtime("Rust"),
                runtime("Kotlin"),
                runtime("Haskell"),
                runtime("Elixir"),
                runtime("Crystal"),
                runtime("Scala"),
                runtime("Swift"),
            ],
            vhosts: vec![Vhost {
                domain: "myapp.test".to_owned(),
                docroot: format!("{BASE_PLACEHOLDER}/www/myapp"),
                port: DEFAULT_VHOST_PORT,
                server_type: "apache".to_owned(),
                enabled: false,
                proxy_port: 0,
            }],
            projects: Vec::new(),
            settings: PanelSettings {
                auto_start: Vec::new(),
                hosts_file: String::new(),
                apache_vhosts_include: format!("{BASE_PLACEHOLDER}/conf/apache/vhosts.conf"),
                nginx_sites_dir: format!("{BASE_PLACEHOLDER}/conf/nginx/sites"),
                active_web_server: DEFAULT_WEB_SERVER.to_owned(),
            },
        }
    }

    /// Reads the configuration, creating a default one when it is absent.
    ///
    /// A configuration that needs migrating is migrated and written back; a
    /// configuration that does not is never written to, so a hand-edited file
    /// keeps its comments and formatting until something actually changes.
    pub fn load(base_dir: &Path) -> Result<Self> {
        let path = config_path(base_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == ErrorKind::NotFound => {
                let config = Self::default_config();
                config.save(base_dir)?;
                return Ok(config);
            }
            Err(source) => return Err(Error::io(&path, source)),
        };

        let mut config: Self =
            serde_json::from_str(&text).map_err(|source| Error::Json { path, source })?;

        if config.migrate() {
            // The original ignored a failed migration write and carried on with
            // the migrated value in memory. Same here: a read-only install
            // directory must not stop the panel from working.
            let _ = config.save(base_dir);
        }
        Ok(config)
    }

    /// Writes the configuration.
    pub fn save(&self, base_dir: &Path) -> Result<()> {
        let path = config_path(base_dir);
        let text = serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        crate::fsx::write_atomic(&path, &text)
    }

    /// Brings an older or hand-edited configuration up to date in place.
    ///
    /// Returns whether anything changed, which is what decides whether the
    /// file is written back. The rules, in order:
    ///
    /// 1. An unset active web server becomes `Apache`.
    /// 2. `Apache` is always enabled: it is the default server, and a
    ///    configuration that had it disabled would have no way to serve PHP.
    /// 3. `Nginx` is disabled whenever `Apache` is the active server, so the
    ///    two cannot both try to own port 80.
    /// 4. Every service introduced by a later release is appended by name.
    ///
    /// Rule 3 runs before rule 4, which is what stops an existing `Nginx`
    /// entry being duplicated by step 4.
    pub fn migrate(&mut self) -> bool {
        let mut migrated = false;

        if self.settings.active_web_server.is_empty() {
            self.settings.active_web_server = DEFAULT_WEB_SERVER.to_owned();
            migrated = true;
        }
        let active = self.settings.active_web_server.clone();

        for service in &mut self.services {
            match service.name.as_str() {
                "Apache" if !service.enabled => {
                    service.enabled = true;
                    migrated = true;
                }
                "Nginx" if active == DEFAULT_WEB_SERVER && service.enabled => {
                    service.enabled = false;
                    migrated = true;
                }
                _ => {}
            }
        }

        for default in Self::default_config().services {
            if !self.services.iter().any(|s| s.name == default.name) {
                self.services.push(default);
                migrated = true;
            }
        }

        migrated
    }

    /// Looks a service up by name.
    pub fn service(&self, name: &str) -> Option<&ServiceConf> {
        self.services.iter().find(|service| service.name == name)
    }

    /// Looks a service up by name, mutably.
    pub fn service_mut(&mut self, name: &str) -> Option<&mut ServiceConf> {
        self.services
            .iter_mut()
            .find(|service| service.name == name)
    }

    /// Which web server is active, falling back to Apache.
    pub fn active_web_server(&self) -> &str {
        if self.settings.active_web_server.is_empty() {
            DEFAULT_WEB_SERVER
        } else {
            &self.settings.active_web_server
        }
    }

    /// How many services are enabled.
    pub fn enabled_service_count(&self) -> usize {
        self.services
            .iter()
            .filter(|service| service.enabled)
            .count()
    }

    /// The services "Start Stack" boots, with the active web server in place
    /// of the hard-coded Apache entry.
    pub fn essential_services(&self) -> Vec<String> {
        self.essential_services_for(true)
    }

    /// The same list, for a start that knows whether a database is wanted.
    ///
    /// The installation's Start Stack always includes the database and its
    /// manager: it boots the machine, and the panel has no project to consult.
    /// `lambo up` does: a project whose `lambo.yml` says `database.kind: none`
    /// must not have MariaDB installed, configured or started on its account -
    /// that is what `none` means - so the database engine and the database
    /// manager are left out of *its* pass. Everything else keeps the original's
    /// order.
    pub fn essential_services_for(&self, database: bool) -> Vec<String> {
        let active = self.active_web_server();
        ESSENTIAL_SERVICES
            .iter()
            .filter(|name| database || (**name != "MySQL" && **name != "phpMyAdmin"))
            .map(|name| {
                if *name == "Apache" || *name == "Nginx" {
                    active.to_owned()
                } else {
                    (*name).to_owned()
                }
            })
            .collect()
    }
}

/// Path of the installation state inside an install directory.
pub fn config_path(base_dir: &Path) -> PathBuf {
    base_dir.join(CONFIG_FILE)
}

/// Expands environment variables and the `{base}` placeholder.
///
/// Two steps, in this order, matching the original exactly:
///
/// 1. `$NAME`, `${NAME}` and the shell specials (`$$`, `$1`, `$?`, …) are
///    expanded; an unknown name expands to nothing, and a `$` that is not
///    followed by a name is left alone as a literal dollar sign.
/// 2. Every literal `{base}` is replaced by the install directory, wherever it
///    appears - the scan is not word-aware, so `{base}extra` becomes
///    `/install/extra`.
///
/// An empty input returns an empty string without consulting the environment.
pub fn expand_path(input: &str, base_dir: &Path) -> String {
    if input.is_empty() {
        return String::new();
    }
    let expanded = expand_env(input);
    replace_base(&expanded, &base_dir.to_string_lossy())
}

/// Replaces every `{base}` occurrence, byte-wise, exactly like the original.
fn replace_base(input: &str, base_dir: &str) -> String {
    if !input.contains(BASE_PLACEHOLDER) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len() + base_dir.len());
    let mut rest = input;
    while let Some(index) = rest.find(BASE_PLACEHOLDER) {
        out.push_str(&rest[..index]);
        out.push_str(base_dir);
        rest = &rest[index + BASE_PLACEHOLDER.len()..];
    }
    out.push_str(rest);
    out
}

/// `os.ExpandEnv`'s algorithm: `$name`, `${name}` and the shell specials.
fn expand_env(input: &str) -> String {
    if !input.contains('$') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'$' && index + 1 < bytes.len() {
            let (name, width) = shell_name(&bytes[index + 1..]);
            if name.is_empty() && width > 0 {
                // Valid but empty syntax such as `${}`: consume it silently.
            } else if name.is_empty() {
                // A `$` that does not introduce a name stays a literal dollar.
                out.push('$');
            } else if let Ok(value) = std::env::var(&name) {
                out.push_str(&value);
            }
            index += 1 + width;
            continue;
        }

        // Copy one whole UTF-8 scalar so multi-byte text survives untouched.
        let text = &input[index..];
        let ch = text
            .chars()
            .next()
            .expect("index always sits on a character boundary");
        out.push(ch);
        index += ch.len_utf8();
    }

    out
}

/// Reads the variable name starting after a `$`.
///
/// Returns the name and how many bytes it consumed, so the caller can decide
/// between "expands to nothing", "literal dollar" and "unknown variable" -
/// three cases the original distinguishes and users can observe. The grammar is
/// Go's `os.getShellName`, including its two bad-syntax rules: `${name}` and the
/// single-character specials, then a run of letters, digits and underscores -
/// `$DB_NAME` is one name, not `$DB` followed by text.
fn shell_name(bytes: &[u8]) -> (String, usize) {
    if bytes.is_empty() {
        return (String::new(), 0);
    }

    if bytes[0] == b'{' {
        // `${name}`. `len(bytes) > 2` counts the characters Go's index
        // arithmetic does: `{`, the name and `}`.
        if bytes.len() > 2 && is_shell_special_var(bytes[1]) && bytes[2] == b'}' {
            return ((bytes[1] as char).to_string(), 3);
        }
        return match bytes.iter().position(|byte| *byte == b'}') {
            Some(1) => (String::new(), 2),
            Some(end) => (
                String::from_utf8_lossy(&bytes[1..end]).into_owned(),
                end + 1,
            ),
            // `${` with no closing brace is bad syntax, and the original eats
            // the two characters it read rather than printing them.
            None => (String::new(), 1),
        };
    }

    if is_shell_special_var(bytes[0]) {
        return ((bytes[0] as char).to_string(), 1);
    }

    let mut end = 0;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    (String::from_utf8_lossy(&bytes[..end]).into_owned(), end)
}

/// Whether a byte can appear in a variable name: letters, digits, underscore.
///
/// Go's `os.isAlphaNum`. The underscore is the one that matters in practice -
/// `$LAMBO_HOME` is a name, and treating it as `$LAMBO` followed by `_HOME`
/// would expand the wrong variable.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The characters Go's `os.Expand` treats as one-character variable names.
fn is_shell_special_var(byte: u8) -> bool {
    matches!(
        byte,
        b'*' | b'#' | b'$' | b'@' | b'!' | b'?' | b'-' | b'0'..=b'9'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn default_configuration_has_thirty_services_in_dashboard_order() {
        let config = PanelConfig::default_config();
        let names: Vec<&str> = config
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect();
        assert_eq!(names.len(), 30);
        assert_eq!(
            names,
            [
                "Apache",
                "Nginx",
                "PHP-FPM",
                "MySQL",
                "PostgreSQL",
                "Redis",
                "phpMyAdmin",
                "Adminer",
                "Composer",
                "pgweb",
                "Erlang",
                "RabbitMQ",
                "MinIO",
                "Mailpit",
                "Node.js",
                "Python",
                "Go",
                "Java",
                "Julia",
                "Zig",
                "Dart",
                "Lua",
                "Ruby",
                "Rust",
                "Kotlin",
                "Haskell",
                "Elixir",
                "Crystal",
                "Scala",
                "Swift",
            ]
        );
        assert_eq!(config.enabled_service_count(), 5);
        assert!(config.service("Apache").unwrap().enabled);
        assert!(config.service("MySQL").unwrap().enabled);
        assert!(config.service("phpMyAdmin").unwrap().enabled);
        assert!(!config.service("Nginx").unwrap().enabled);
        assert!(!config.service("Swift").unwrap().enabled);
    }

    #[test]
    fn default_configuration_matches_the_original_layout() {
        let config = PanelConfig::default_config();
        assert_eq!(config.version, 1);
        assert_eq!(config.projects, Vec::new());

        let apache = config.service("Apache").unwrap();
        assert_eq!(apache.kind, "web");
        assert_eq!(apache.exe, "{base}/bin/apache/bin/httpd.exe");
        assert_eq!(apache.workdir, "{base}/bin/apache");
        assert_eq!(apache.config_file, "{base}/bin/apache/conf/httpd.conf");
        assert_eq!(apache.port, 80);
        assert!(apache.args.is_empty());
        assert!(apache.env.is_empty());

        let mysql = config.service("MySQL").unwrap();
        assert_eq!(
            mysql.args,
            [
                "--console",
                "--basedir={base}/bin/mysql",
                "--datadir={base}/bin/mysql/data",
            ]
        );

        let rabbitmq = config.service("RabbitMQ").unwrap();
        assert_eq!(rabbitmq.exe, "cmd.exe");
        assert_eq!(
            rabbitmq.env,
            [
                "ERLANG_HOME={base}/bin/erlang",
                "RABBITMQ_BASE={base}/data/rabbitmq",
                "RABBITMQ_NODENAME=rabbit@localhost",
            ]
        );

        assert_eq!(config.vhosts.len(), 1);
        let vhost = &config.vhosts[0];
        assert_eq!(vhost.domain, "myapp.test");
        assert_eq!(vhost.docroot, "{base}/www/myapp");
        assert_eq!(vhost.port, 80);
        assert_eq!(vhost.server_type, "apache");
        assert!(!vhost.enabled);

        assert_eq!(config.settings.active_web_server, "Apache");
        assert!(config.settings.auto_start.is_empty());
        assert!(config.settings.hosts_file.is_empty());
        assert_eq!(
            config.settings.apache_vhosts_include,
            "{base}/conf/apache/vhosts.conf"
        );
        assert_eq!(config.settings.nginx_sites_dir, "{base}/conf/nginx/sites");
    }

    /// The JSON a hand-edited file is expected to contain: field names, field
    /// order and which fields are left out entirely.
    #[test]
    fn serialized_json_uses_the_original_key_names_and_omits_empty_fields() {
        let config = PanelConfig {
            version: 1,
            services: vec![ServiceConf {
                name: "Apache".to_owned(),
                kind: "web".to_owned(),
                exe: "{base}/bin/apache/bin/httpd.exe".to_owned(),
                args: Vec::new(),
                port: 80,
                workdir: "{base}/bin/apache".to_owned(),
                config_file: "{base}/bin/apache/conf/httpd.conf".to_owned(),
                enabled: true,
                open_url: String::new(),
                active_version: String::new(),
                env: Vec::new(),
            }],
            vhosts: vec![Vhost {
                domain: "myapp.test".to_owned(),
                docroot: "{base}/www/myapp".to_owned(),
                port: 80,
                server_type: "apache".to_owned(),
                enabled: false,
                proxy_port: 0,
            }],
            projects: Vec::new(),
            settings: PanelSettings {
                auto_start: Vec::new(),
                hosts_file: String::new(),
                apache_vhosts_include: "{base}/conf/apache/vhosts.conf".to_owned(),
                nginx_sites_dir: "{base}/conf/nginx/sites".to_owned(),
                active_web_server: "Apache".to_owned(),
            },
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let expected = r#"{
  "version": 1,
  "services": [
    {
      "name": "Apache",
      "kind": "web",
      "exe": "{base}/bin/apache/bin/httpd.exe",
      "port": 80,
      "workdir": "{base}/bin/apache",
      "config_file": "{base}/bin/apache/conf/httpd.conf",
      "enabled": true
    }
  ],
  "vhosts": [
    {
      "domain": "myapp.test",
      "docroot": "{base}/www/myapp",
      "port": 80,
      "server_type": "apache",
      "enabled": false
    }
  ],
  "projects": [],
  "settings": {
    "auto_start": [],
    "hosts_file": "",
    "apache_vhosts_include": "{base}/conf/apache/vhosts.conf",
    "nginx_sites_dir": "{base}/conf/nginx/sites",
    "active_web_server": "Apache"
  }
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn a_missing_configuration_is_created_with_defaults() {
        let temp = TempDir::new();
        let config = PanelConfig::load(temp.path()).unwrap();

        assert_eq!(config.services.len(), 30);
        assert!(config_path(temp.path()).is_file());

        // And it round-trips: the written file parses back to the same value.
        let reloaded = PanelConfig::load(temp.path()).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn unknown_keys_are_ignored_and_missing_keys_default() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            r#"{"version": 1, "future_field": {"nested": true}, "settings": {"active_web_server": "Nginx"}}"#,
        )
        .unwrap();

        // `migrate` then appends every service the file is missing, which is
        // the point of the fourth migration rule.
        let config = PanelConfig::load(temp.path()).unwrap();
        assert_eq!(config.services.len(), 30);
        assert!(config.projects.is_empty());
        assert_eq!(config.settings.active_web_server, "Nginx");
    }

    /// The file the previous implementation writes the first time it starts
    /// must load here.
    ///
    /// The fixture is byte-for-byte what `DefaultConfig` + `json.MarshalIndent`
    /// produce, taken from the original `config.go`: the same field order, the
    /// same `omitempty` gaps, and `"projects": null` for the empty list Go's
    /// marshaller writes for a nil slice. It is here because "a configuration
    /// written by the previous implementation loads unchanged" is a claim that
    /// only a verbatim artifact can support.
    #[test]
    fn a_configuration_written_by_the_previous_implementation_loads_unchanged() {
        const LEGACY_DEFAULT: &str = include_str!("../tests/fixtures/legacy-default-config.json");

        let temp = TempDir::new();
        fs::write(config_path(temp.path()), LEGACY_DEFAULT).unwrap();
        let config = PanelConfig::load(temp.path())
            .expect("the configuration the previous implementation writes must load");

        // The previous implementation's dashboard order and its own values, untouched.
        assert_eq!(config.version, 1);
        assert_eq!(config.services.len(), 30);
        let apache = &config.services[0];
        assert_eq!(apache.name, "Apache");
        assert_eq!(apache.kind, "web");
        assert_eq!(apache.exe, "{base}/bin/apache/bin/httpd.exe");
        assert_eq!(apache.port, 80);
        assert!(apache.enabled);
        assert_eq!(config.services[1].name, "Nginx");
        assert!(!config.services[1].enabled, "Apache is the active server");
        let rabbit = config
            .services
            .iter()
            .find(|service| service.name == "RabbitMQ")
            .expect("RabbitMQ is one of its services");
        assert_eq!(
            rabbit.env,
            [
                "ERLANG_HOME={base}/bin/erlang",
                "RABBITMQ_BASE={base}/data/rabbitmq",
                "RABBITMQ_NODENAME=rabbit@localhost"
            ]
        );

        // `{base}` is the installation directory, expanded byte-wise the way
        // the previous implementation resolved it: the template's own
        // separators survive, so on Windows the result mixes the installation
        // directory's backslashes with the shipped template's forward slashes.
        assert_eq!(
            expand_path(&config.services[0].exe, temp.path()),
            format!("{}/bin/apache/bin/httpd.exe", temp.path().display())
        );

        // The shipped virtual host, disabled, exactly as it ships.
        assert_eq!(config.vhosts.len(), 1);
        assert_eq!(config.vhosts[0].domain, "myapp.test");
        assert_eq!(config.vhosts[0].docroot, "{base}/www/myapp");
        assert_eq!(config.vhosts[0].server_type, "apache");
        assert!(!config.vhosts[0].enabled);

        // Settings, and the null project list read as the empty list it means.
        assert_eq!(
            config.settings.apache_vhosts_include,
            "{base}/conf/apache/vhosts.conf"
        );
        assert_eq!(config.settings.nginx_sites_dir, "{base}/conf/nginx/sites");
        assert_eq!(config.settings.active_web_server, "Apache");
        assert!(config.settings.auto_start.is_empty());
        assert!(config.projects.is_empty());

        // Field for field, the state the previous implementation writes on its
        // first launch is the state this one starts from.
        assert_eq!(
            config,
            PanelConfig::default_config(),
            "the shipped configuration must match, service for service"
        );

        // What is written back stays readable, and an empty list is written as
        // `[]` rather than the `null` that was read.
        config.save(temp.path()).unwrap();
        let saved = fs::read_to_string(config_path(temp.path())).unwrap();
        assert!(saved.contains("\"projects\": []"), "{saved}");
        assert!(!saved.contains("null"), "{saved}");
        let reloaded = PanelConfig::load(temp.path()).unwrap();
        assert_eq!(reloaded, config, "the round trip is lossless");
    }

    #[test]
    fn malformed_json_is_reported_with_the_file_and_the_reason() {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), "{ this is not json").unwrap();

        let error = PanelConfig::load(temp.path()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("config.json") && message.contains("invalid JSON"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn migration_fills_in_an_unset_active_web_server() {
        let mut config = PanelConfig {
            settings: PanelSettings {
                active_web_server: String::new(),
                ..PanelConfig::default_config().settings
            },
            ..PanelConfig::default_config()
        };
        assert!(config.migrate());
        assert_eq!(config.settings.active_web_server, "Apache");
    }

    #[test]
    fn migration_enables_apache_and_disables_nginx() {
        let mut config = PanelConfig::default_config();
        config.service_mut("Apache").unwrap().enabled = false;
        config.service_mut("Nginx").unwrap().enabled = true;

        assert!(config.migrate());
        assert!(config.service("Apache").unwrap().enabled);
        assert!(!config.service("Nginx").unwrap().enabled);
    }

    #[test]
    fn migration_keeps_nginx_when_nginx_is_the_active_server() {
        let mut config = PanelConfig::default_config();
        config.settings.active_web_server = "Nginx".to_owned();
        config.service_mut("Nginx").unwrap().enabled = true;
        // The default document ships Apache already enabled, so the rule below
        // only fires when something switched it off - which is the state being
        // migrated.
        config.service_mut("Apache").unwrap().enabled = false;

        // Apache is still forced on - that rule is unconditional - but Nginx
        // is left alone because it is the server in charge.
        assert!(config.migrate());
        assert!(config.service("Apache").unwrap().enabled);
        assert!(config.service("Nginx").unwrap().enabled);
    }

    #[test]
    fn migration_appends_only_the_services_that_are_missing() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        config.services.retain(|service| service.name != "Mailpit");
        let template = config.services[0].clone();
        config.services.push(ServiceConf {
            name: "SonarQube".to_owned(),
            kind: "tool".to_owned(),
            enabled: false,
            ..template
        });
        config.save(temp.path()).unwrap();

        let loaded = PanelConfig::load(temp.path()).unwrap();
        assert_eq!(
            loaded
                .services
                .iter()
                .filter(|service| service.name == "Mailpit")
                .count(),
            1
        );
        assert_eq!(loaded.services.len(), 31);
        assert!(loaded.service("SonarQube").is_some());
    }

    #[test]
    fn migration_leaves_a_complete_configuration_alone() {
        let temp = TempDir::new();
        let config = PanelConfig::default_config();
        config.save(temp.path()).unwrap();
        let before = fs::read(config_path(temp.path())).unwrap();

        let loaded = PanelConfig::load(temp.path()).unwrap();
        assert_eq!(loaded, config);

        // Nothing migrated, so the file on disk is byte-for-byte untouched.
        let after = fs::read(config_path(temp.path())).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn expand_path_substitutes_the_install_directory() {
        let base = Path::new("C:/lambo");
        assert_eq!(expand_path("{base}/bin/php", base), "C:/lambo/bin/php");
        assert_eq!(
            expand_path("{base}/conf/apache/vhosts.conf", base),
            "C:/lambo/conf/apache/vhosts.conf"
        );
        // The scan is not word-aware, so a placeholder glued to a suffix still
        // expands - matching the original byte-wise replacement.
        assert_eq!(expand_path("{base}extra", base), "C:/lamboextra");
        // Several occurrences, and text with no placeholder at all.
        assert_eq!(expand_path("{base}{base}", base), "C:/lamboC:/lambo");
        assert_eq!(expand_path("/usr/local", base), "/usr/local");
    }

    #[test]
    fn expand_path_leaves_an_empty_string_empty() {
        let temp = TempDir::new();
        assert_eq!(expand_path("", temp.path()), "");
        // The environment is not consulted for an empty input: even a variable
        // that is definitely set cannot change the result.
        assert_eq!(expand_path("", temp.path()), "");
    }

    #[test]
    fn expand_path_expands_environment_variables() {
        // `PATH` is the one variable every platform and CI runner is certain to
        // define, which keeps this test free of environment setup (the crate
        // forbids `unsafe`, so tests cannot set variables themselves).
        let path = std::env::var("PATH").expect("PATH is set on every supported platform");
        let base = Path::new("/install");

        assert_eq!(expand_path("$PATH/bin", base), format!("{path}/bin"));
        assert_eq!(expand_path("${PATH}/bin", base), format!("{path}/bin"));
        assert_eq!(
            expand_path("$PATH/{base}", base),
            format!("{path}//install")
        );
        // An unset name expands to nothing, and the text around it survives.
        assert_eq!(expand_path("a$LAMBO_EXPAND_CERTAINLY_UNSET b", base), "a b");
    }

    #[test]
    fn expand_path_keeps_a_lone_dollar_sign() {
        let base = Path::new("/install");
        // A `$` that introduces no name is a literal dollar, as in the original.
        assert_eq!(expand_path("price$", base), "price$");
        assert_eq!(expand_path("$ 5", base), "$ 5");
        // `${}` is valid syntax with an empty name: it disappears.
        assert_eq!(expand_path("a${}b", base), "ab");
        // `${` with no closing brace is *bad* syntax, and Go answers that by
        // eating the two characters it read - the dollar does not survive, and
        // neither does the brace.
        assert_eq!(expand_path("a${b", base), "ab");
        // The one-character specials are names as well, and `${$}` is how one is
        // spelled in braces.
        assert_eq!(shell_name(b"{$}rest"), ("$".to_owned(), 3));
        // A name runs over underscores: `$LAMBO_HOME` is one variable.
        assert_eq!(shell_name(b"LAMBO_HOME/x"), ("LAMBO_HOME".to_owned(), 10));
    }

    #[test]
    fn expand_path_keeps_multibyte_text_intact() {
        let temp = TempDir::new();
        let base = temp.path();
        let expanded = expand_path("Café/{base}/日本", base);
        assert!(expanded.starts_with("Café/"));
        assert!(expanded.ends_with("/日本"));
    }

    #[test]
    fn essential_services_follow_the_active_web_server() {
        let mut config = PanelConfig::default_config();
        assert_eq!(
            config.essential_services(),
            ["Apache", "PHP-FPM", "MySQL", "phpMyAdmin"]
        );

        config.settings.active_web_server = "Nginx".to_owned();
        assert_eq!(
            config.essential_services(),
            ["Nginx", "PHP-FPM", "MySQL", "phpMyAdmin"]
        );
    }

    #[test]
    fn active_web_server_falls_back_to_apache() {
        let mut config = PanelConfig::default_config();
        config.settings.active_web_server = String::new();
        assert_eq!(config.active_web_server(), "Apache");
    }
}
