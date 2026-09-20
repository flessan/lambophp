//! MariaDB and MySQL, managed end to end.
//!
//! "Managed" here means the whole life of the server belongs to Lambo: the
//! runtime is downloaded and verified, the data directory is created inside
//! `$LAMBO_HOME`, the configuration is generated, the server is started and
//! stopped on request, databases are created and dropped, and the credentials
//! are generated per installation. Nothing is installed machine-wide, no
//! service is registered, and no administrator rights are needed - which on
//! Windows is the difference between a tool a developer can use and one they
//! cannot.
//!
//! # Security
//!
//! - The server binds to `127.0.0.1` only. A development database is not a
//!   network service and must not become one by accident.
//! - The root password is generated per Lambo home ([`crate::secret`]) and
//!   stored in `$LAMBO_HOME/config/lambo.yml` with owner-only permissions. It
//!   is never a well-known string, and never appears in a process's command
//!   line: clients receive it through the `MYSQL_PWD` environment variable,
//!   because a password in `argv` is visible to every user of the machine in
//!   the process listing.
//! - Database names are validated before they are interpolated into SQL, so a
//!   project name cannot become a statement.
//!
//! # Windows
//!
//! - No `socket`/`pid-file` in `/var/run`: everything lives in the Lambo home.
//! - `mysqladmin shutdown` is tried before any process termination, so the
//!   storage engine flushes rather than being killed.
//! - Initialization uses `mariadb-install-db.exe -d … -p …` on Windows and
//!   `mariadb-install-db --datadir=…` on Unix; MySQL 8 uses
//!   `mysqld --initialize-insecure` on both.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, Family, Release};
use crate::config::{DatabaseConfig, DatabaseKind};
use crate::download::Downloader;
use crate::error::{Error, Result};
use crate::fsx;
use crate::naming;
use crate::paths::{self, Paths};
use crate::platform::Os;
use crate::process::{self, Output, ProcessSpec};
use crate::runtime::{self, InstalledRuntime, RuntimeKind};
use crate::version::VersionSpec;

/// How long a database server gets to initialize and start.
pub const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a graceful shutdown may take before Lambo escalates.
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// A database server Lambo can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    /// Which engine this is.
    pub kind: DatabaseKind,
    /// The installed runtime.
    pub runtime: InstalledRuntime,
    /// The server executable (`mariadbd`/`mysqld`).
    pub server: PathBuf,
    /// The command-line client, when present.
    pub client: Option<PathBuf>,
    /// The administrative client used for graceful shutdown.
    pub admin: Option<PathBuf>,
    /// The data-directory initializer, when present.
    pub initializer: Option<PathBuf>,
}

impl Database {
    /// Human-readable description, e.g. `MariaDB 11.4.4 (managed by Lambo)`.
    pub fn describe(&self) -> String {
        format!(
            "{} {} (managed by Lambo)",
            self.kind.display_name(),
            self.runtime.version
        )
    }
}

/// Maps a configured engine to the runtime family that provides it.
/// The catalogue family a database kind is published under.
///
/// `None` for [`DatabaseKind::None`]: there is nothing to look up.
pub fn family_for(kind: DatabaseKind) -> Family {
    match kind {
        DatabaseKind::Mariadb => Family::Mariadb,
        DatabaseKind::Mysql => Family::Mysql,
        DatabaseKind::None => Family::Mariadb,
    }
}

pub fn runtime_kind(kind: DatabaseKind) -> Option<RuntimeKind> {
    match kind {
        DatabaseKind::None => None,
        DatabaseKind::Mariadb => Some(RuntimeKind::Mariadb),
        DatabaseKind::Mysql => Some(RuntimeKind::Mysql),
    }
}

/// Maps a catalogue family back to the configured engine.
pub fn database_kind(family: Family) -> DatabaseKind {
    match family {
        Family::Mariadb => DatabaseKind::Mariadb,
        Family::Mysql => DatabaseKind::Mysql,
        Family::Php | Family::Apache | Family::DbUi => DatabaseKind::None,
    }
}

/// Finds an installed database server.
pub fn discover(paths: &Paths, kind: DatabaseKind, os: Os) -> Option<Database> {
    let family = runtime_kind(kind)?;
    let runtime = match runtime::active(paths, family) {
        Ok(Some(runtime)) => Some(runtime),
        _ => None,
    }
    .or_else(|| {
        runtime::installed(paths, family)
            .ok()?
            .into_iter()
            .find(|runtime| runtime.is_complete(os))
    })?;
    from_runtime(kind, runtime, os)
}

/// Builds a [`Database`] from an installed runtime.
pub fn from_runtime(kind: DatabaseKind, runtime: InstalledRuntime, os: Os) -> Option<Database> {
    let server = runtime.server_executable(os)?;
    Some(Database {
        client: runtime.client_executable(os),
        admin: runtime.admin_executable(os),
        initializer: runtime.init_executable(os),
        kind,
        runtime,
        server,
    })
}

/// Installs a database server from the catalogue.
pub fn install(
    paths: &Paths,
    catalog: &Catalog,
    kind: DatabaseKind,
    spec: &VersionSpec,
    platform: crate::platform::Platform,
    downloader: &dyn Downloader,
    sources: &crate::config::SourcesConfig,
) -> Result<Database> {
    if kind == DatabaseKind::None {
        return Err(Error::InvalidInput(
            "no database engine is configured; set `database.kind` to mariadb or mysql first"
                .to_owned(),
        ));
    }
    let family = family_for(kind);
    let release = catalog.find(family, spec, &platform.key()).ok_or_else(|| {
        Error::InvalidInput(format!(
            "no {} release for {} in the catalogue",
            kind.display_name(),
            platform
        ))
    })?;
    install_release(paths, kind, &release, downloader, platform.os, sources)
}

/// Installs one specific catalogue release.
pub fn install_release(
    paths: &Paths,
    kind: DatabaseKind,
    release: &Release,
    downloader: &dyn Downloader,
    os: Os,
    sources: &crate::config::SourcesConfig,
) -> Result<Database> {
    let family = runtime_kind(kind).expect("a database kind always maps to a runtime family");
    let version = semver::Version::parse(&release.version).map_err(|_| {
        Error::InvalidInput(format!("`{}` is not a valid version", release.version))
    })?;

    let resolved = crate::sources::resolve(
        family_for(kind),
        release,
        sources,
        paths,
        release.from_override,
    );
    let verified = crate::download::download_verified(downloader, &resolved.artifact, paths)
        .map_err(|error| {
            error.identify(family_for(kind), release).with_hint(format!(
                "Pin the digest in the catalogue override at `{}`",
                paths.catalogs_dir().display()
            ))
        })?;
    let final_dir = paths.runtime_version_dir(family, &release.version);
    let staging = final_dir.with_file_name(format!("{}.installing", release.version));

    let _ = fsx::remove_dir_all_if_exists(&staging)?;
    let extraction = match crate::archive::extract(&verified.path, &staging) {
        Ok(extraction) => extraction,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Some(wrapper) = extraction.single_top_level_dir() {
        fsx::move_children(&staging.join(wrapper), &staging)?;
    }

    crate::runtime::write_manifest(
        runtime_kind(kind).expect("a database kind always maps to a runtime family"),
        &release.version,
        &release.platform,
        &resolved,
        &verified.sha256,
        release.executable.as_deref(),
        &staging,
    )?;

    crate::runtime::promote(&staging, &final_dir)?;

    let runtime = InstalledRuntime {
        kind: family,
        name: release.version.clone(),
        version,
        path: final_dir,
    };
    if !runtime.is_complete(os) {
        let _ = fs::remove_dir_all(&runtime.path);
        return Err(Error::RuntimeNotInstalled {
            kind: kind.display_name(),
            name: release.version.clone(),
            path: runtime.path,
        });
    }
    runtime::set_active(paths, family, &release.version)?;

    from_runtime(kind, runtime, os).ok_or(Error::RuntimeMissing {
        kind: kind.display_name(),
        command: "lambo db install",
    })
}

/// Everything needed to configure, initialize and run one database server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// TCP port the server listens on.
    pub port: u16,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Temporary directory, kept inside the Lambo home.
    pub tmp_dir: PathBuf,
    /// Generated configuration file.
    pub config_file: PathBuf,
    /// Error log.
    pub log: PathBuf,
    /// Administrative user.
    pub username: String,
    /// Administrative password.
    pub password: String,
    /// Unix socket, when the platform uses one.
    pub socket: Option<PathBuf>,
}

impl Plan {
    /// Builds a plan from the global configuration.
    pub fn from_config(paths: &Paths, config: &DatabaseConfig) -> Self {
        Self {
            port: config.port,
            data_dir: paths.database_data_dir(),
            tmp_dir: paths.database_tmp_dir(),
            config_file: paths.database_config_file(),
            log: crate::logs::database(paths),
            username: config.username.clone(),
            password: config.password.clone(),
            socket: None,
        }
    }

    /// Attaches the platform-appropriate socket path.
    pub fn with_socket(mut self, os: Os, paths: &Paths) -> Self {
        if !os.is_windows() {
            self.socket = Some(paths.database_state_dir().join("mysql.sock"));
        }
        self
    }

    /// The `127.0.0.1:port` address applications should connect to.
    pub fn host_and_port(&self) -> String {
        naming::database_host_and_port(self.port)
    }

    /// Renders a path the way the server's configuration parser expects it.
    ///
    /// Forward slashes on every platform: MySQL and MariaDB both treat
    /// backslashes as escapes inside string values, which makes a Windows
    /// `datadir` written naively point somewhere unexpected.
    fn config_value(&self, path: &Path, os: Os) -> String {
        let _ = os;
        paths::to_forward_slashes(path)
    }
}

/// Writes the generated `my.cnf`/`my.ini` and returns its path.
pub fn write_config(database: &Database, plan: &Plan, os: Os) -> Result<PathBuf> {
    fsx::ensure_dir(&plan.data_dir)?;
    fsx::ensure_dir(&plan.tmp_dir)?;
    if let Some(socket) = &plan.socket {
        if let Some(parent) = socket.parent() {
            fsx::ensure_dir(parent)?;
        }
    }

    let mut body = String::new();
    body.push_str("# Generated by Lambo PHP - do not edit.\n");
    body.push_str("# `lambo db start` rewrites this file from your configuration.\n\n");
    body.push_str("[mysqld]\n");
    body.push_str(&format!("port = {}\n", plan.port));
    // A development server is not a network service.
    body.push_str("bind-address = 127.0.0.1\n");
    body.push_str("skip-name-resolve\n");
    body.push_str(&format!(
        "basedir = {}\n",
        quote(&plan.config_value(&database.runtime.path, os))
    ));
    body.push_str(&format!(
        "datadir = {}\n",
        quote(&plan.config_value(&plan.data_dir, os))
    ));
    body.push_str(&format!(
        "tmpdir = {}\n",
        quote(&plan.config_value(&plan.tmp_dir, os))
    ));
    body.push_str(&format!(
        "log-error = {}\n",
        quote(&plan.config_value(&plan.log, os))
    ));
    body.push_str(&format!(
        "pid-file = {}\n",
        quote(&plan.config_value(&pid_file(plan), os))
    ));
    if let Some(socket) = &plan.socket {
        body.push_str(&format!(
            "socket = {}\n",
            quote(&plan.config_value(socket, os))
        ));
    }
    body.push_str("character-set-server = utf8mb4\n");
    body.push_str("collation-server = utf8mb4_unicode_ci\n");
    // Development defaults: generous limits, no slow-query noise.
    body.push_str("max_allowed_packet = 64M\n");
    body.push_str("\n[client]\n");
    body.push_str(&format!("port = {}\n", plan.port));
    body.push_str("host = 127.0.0.1\n");
    if let Some(socket) = &plan.socket {
        body.push_str(&format!(
            "socket = {}\n",
            quote(&plan.config_value(socket, os))
        ));
    }

    fsx::write_atomic(&plan.config_file, &body)?;
    Ok(plan.config_file.clone())
}

/// The PID file path.
fn pid_file(plan: &Plan) -> PathBuf {
    plan.data_dir.with_file_name("mysqld.pid")
}

/// Quotes a configuration value that needs it.
fn quote(value: &str) -> String {
    if value.contains(' ') || value.contains('#') {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    }
}

/// Whether the data directory has been initialized.
///
/// An initialized data directory always contains a `mysql` schema directory;
/// anything less means the server will refuse to start.
pub fn is_initialized(plan: &Plan) -> bool {
    plan.data_dir.join("mysql").is_dir()
}

/// Creates the data directory and its system tables.
///
/// The server is created with a passwordless local root and secured
/// immediately afterwards ([`secure`]); both MariaDB's and MySQL's own
/// initialization tooling work this way, and the server is bound to
/// `127.0.0.1` for the whole window.
pub fn initialize(database: &Database, plan: &Plan, os: Os) -> Result<()> {
    if is_initialized(plan) {
        return Ok(());
    }
    fsx::ensure_dir(&plan.data_dir)?;
    fsx::ensure_dir(&plan.tmp_dir)?;

    let spec = initialize_spec(database, plan, os).ok_or_else(|| Error::ServiceFailed {
        service: database.kind.display_name().to_owned(),
        reason: "the data directory has not been initialized and this installation has no \
                 initializer"
            .to_owned(),
        causes: vec![
            "the runtime archive may be incomplete".to_owned(),
            format!("looked for one of: {}", database.kind.display_name()),
        ],
        hint: Some(format!(
            "lambo db install --force ({})",
            database.runtime.name
        )),
    })?;

    write_config(database, plan, os)?;
    let output = process::run(&spec, os)?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::ServiceFailed {
            service: database.kind.display_name().to_owned(),
            reason: "the data directory could not be initialized".to_owned(),
            causes: vec![
                format!(
                    "{} exited {}",
                    spec.render(),
                    describe_exit(output.status.code())
                ),
                format!("output: {}", details.trim()),
            ],
            hint: Some("lambo doctor".to_owned()),
        });
    }
    Ok(())
}

/// The initialization command for this engine and platform.
pub fn initialize_spec(database: &Database, plan: &Plan, os: Os) -> Option<ProcessSpec> {
    match database.kind {
        DatabaseKind::Mysql => {
            // MySQL 8 initializes through the server itself.
            Some(
                ProcessSpec::new(&database.server, "mysqld")
                    .arg("--initialize-insecure")
                    .arg(format!("--datadir={}", plan.data_dir.display()))
                    .arg(format!("--basedir={}", database.runtime.path.display()))
                    .stdout(Output::Null)
                    .stderr(Output::File(plan.log.clone())),
            )
        }
        DatabaseKind::Mariadb if os.is_windows() => {
            let initializer = database.initializer.clone()?;
            Some(
                ProcessSpec::new(&initializer, "mariadb-install-db")
                    .arg("-d")
                    .arg(plan.data_dir.display().to_string())
                    // `-n` keeps Windows from registering a service, which
                    // would need administrator rights.
                    .arg("-n")
                    .stdout(Output::Null)
                    .stderr(Output::File(plan.log.clone())),
            )
        }
        DatabaseKind::Mariadb => {
            let initializer = database.initializer.clone()?;
            Some(
                ProcessSpec::new(&initializer, "mariadb-install-db")
                    .arg(format!("--datadir={}", plan.data_dir.display()))
                    .arg(format!("--basedir={}", database.runtime.path.display()))
                    // Plain password authentication keeps `mysql`/Adminer
                    // working without a socket-auth plugin surprise.
                    .arg("--auth-root-authentication-method=normal")
                    .stdout(Output::Null)
                    .stderr(Output::File(plan.log.clone())),
            )
        }
        DatabaseKind::None => None,
    }
}

/// Sets the root password to the configured one, if it is not already set.
///
/// Returns `true` when a password was set by this call.
pub fn secure(database: &Database, plan: &Plan, os: Os) -> Result<bool> {
    if plan.password.is_empty() {
        return Err(Error::InvalidInput(
            "no database password is configured; run `lambo db install` to generate one".to_owned(),
        ));
    }
    // Already secured? Nothing to do.
    if connect(database, plan, os, &plan.password, "SELECT 1").is_ok() {
        return Ok(false);
    }

    let statement = set_password_sql(&plan.username, &plan.password);
    connect(database, plan, os, "", &statement)?;
    // Verify rather than assume: a server that silently ignored the statement
    // would leave Lambo believing it was secured.
    connect(database, plan, os, &plan.password, "SELECT 1").map_err(|_| Error::Database {
        reason: "the database password was set but cannot be used to connect".to_owned(),
        log: Some(plan.log.clone()),
    })?;
    Ok(true)
}

/// The SQL that sets the administrative password.
///
/// The password is quoted as a SQL literal; every character [`crate::secret`]
/// can generate is safe inside single quotes, and the alphabet deliberately
/// excludes `'` and `\`.
pub fn set_password_sql(username: &str, password: &str) -> String {
    format!("ALTER USER '{username}'@'localhost' IDENTIFIED BY '{password}'; FLUSH PRIVILEGES;")
}

/// Connects as the configured user and runs one statement.
pub fn connect(
    database: &Database,
    plan: &Plan,
    os: Os,
    password: &str,
    statement: &str,
) -> Result<String> {
    let spec = client_spec(database, plan, password)
        .ok_or_else(|| Error::Database {
            reason: "this installation has no command-line client".to_owned(),
            log: Some(plan.log.clone()),
        })?
        .args(["--batch", "--skip-column-names", "--execute", statement]);
    let output = process::run(&spec, os)?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(Error::Database {
            reason: if details.is_empty() {
                format!("the client exited {}", describe_exit(output.status.code()))
            } else {
                details
            },
            log: Some(plan.log.clone()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The client command line, with the password passed through the environment.
///
/// `MYSQL_PWD` rather than `-p<password>`: an argument is visible in the
/// process listing to every user on the machine.
pub fn client_spec(database: &Database, plan: &Plan, password: &str) -> Option<ProcessSpec> {
    let client = database.client.clone()?;
    Some(
        ProcessSpec::new(&client, "database")
            .arg("--host=127.0.0.1")
            .arg(format!("--port={}", plan.port))
            .arg(format!("--user={}", plan.username))
            .env("MYSQL_PWD", password),
    )
}

/// Creates a database.
pub fn create_database(database: &Database, plan: &Plan, name: &str, os: Os) -> Result<()> {
    if !naming::is_valid_database_name(name) {
        return Err(Error::InvalidInput(format!(
            "`{name}` is not a valid database name: use lowercase letters, digits and underscores, \
             starting with a letter"
        )));
    }
    connect(
        database,
        plan,
        os,
        &plan.password,
        &format!("CREATE DATABASE IF NOT EXISTS `{name}`;"),
    )
    .map(|_| ())
}

/// Drops a database.
pub fn drop_database(database: &Database, plan: &Plan, name: &str, os: Os) -> Result<()> {
    if !naming::is_valid_database_name(name) {
        return Err(Error::InvalidInput(format!(
            "`{name}` is not a valid database name"
        )));
    }
    connect(
        database,
        plan,
        os,
        &plan.password,
        &format!("DROP DATABASE IF EXISTS `{name}`;"),
    )
    .map(|_| ())
}

/// Lists the user databases (the system schemas are noise here).
pub fn list_databases(database: &Database, plan: &Plan, os: Os) -> Result<Vec<String>> {
    let output = connect(
        database,
        plan,
        os,
        &plan.password,
        "SELECT schema_name FROM information_schema.schemata \
         WHERE schema_name NOT IN ('information_schema','mysql','performance_schema','sys') \
         ORDER BY schema_name;",
    )?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The command line for an interactive shell (`lambo db shell`).
pub fn shell_spec(database: &Database, plan: &Plan, name: Option<&str>) -> Option<ProcessSpec> {
    let mut spec = client_spec(database, plan, &plan.password)?;
    spec = spec.interactive();
    if let Some(name) = name {
        spec = spec.arg(name);
    }
    Some(spec)
}

/// Waits until the server accepts TCP connections.
///
/// A TCP connect is the check rather than `mysqladmin ping` because it is the
/// thing applications actually need, and it needs no client, no credentials
/// and no shell on any platform.
pub fn wait_until_ready(plan: &Plan, timeout: std::time::Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if crate::port::is_listening(plan.port) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Timeout {
                service: format!("{} on port {}", "the database server", plan.port),
                seconds: timeout.as_secs(),
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Whether the server is accepting connections.
pub fn is_ready(plan: &Plan) -> bool {
    crate::port::is_listening(plan.port)
}

/// The credentials an application needs, for `.env` files and `lambo db credentials`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// Engine name as applications spell it.
    pub kind: DatabaseKind,
    /// Host to connect to.
    pub host: String,
    /// Port to connect to.
    pub port: u16,
    /// User name.
    pub username: String,
    /// Password.
    pub password: String,
    /// Database name, when the query was about one project.
    pub database: Option<String>,
}

impl Credentials {
    /// Builds credentials for a plan, optionally for one database.
    pub fn for_plan(plan: &Plan, kind: DatabaseKind, database: Option<&str>) -> Self {
        Self {
            kind,
            host: "127.0.0.1".to_owned(),
            port: plan.port,
            username: plan.username.clone(),
            password: plan.password.clone(),
            database: database.map(str::to_owned),
        }
    }

    /// Renders credentials for `lambo db credentials`.
    ///
    /// The password is shown only when `reveal` is set; otherwise it is
    /// replaced with a note about where it lives, because terminal scrollback
    /// and screen sharing are where secrets leak.
    pub fn render(&self, reveal: bool) -> String {
        let password = if reveal {
            self.password.clone()
        } else {
            "(hidden - pass --show-password)".to_owned()
        };
        let mut lines = vec![
            format!("engine:   {}", self.kind.display_name()),
            format!("host:     {}", self.host),
            format!("port:     {}", self.port),
            format!("user:     {}", self.username),
            format!("password: {password}"),
        ];
        if let Some(database) = &self.database {
            lines.push(format!("database: {database}"));
        }
        lines.join("\n")
    }
}

/// Describes an exit code in words.
fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(0) => "successfully".to_owned(),
        Some(code) => format!("with status {code}"),
        None => "without reporting a status".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::LocalDownloader;
    use crate::testutil::{self, TempDir};

    fn config(port: u16) -> DatabaseConfig {
        DatabaseConfig {
            kind: DatabaseKind::Mariadb,
            port,
            username: "root".to_owned(),
            password: "generated-password".to_owned(),
        }
    }

    /// A fake MariaDB installation with the binaries of a real one.
    fn fake_mariadb(paths: &Paths, version: &str, os: Os) -> Database {
        testutil::install_fake_runtime(paths, RuntimeKind::Mariadb, version, os);
        let runtime = runtime::installed(paths, RuntimeKind::Mariadb)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        from_runtime(DatabaseKind::Mariadb, runtime, os).unwrap()
    }

    /// A port nothing is listening on.
    fn unused_port() -> u16 {
        crate::port::first_free(34_567..34_600).unwrap_or(34_567)
    }

    #[test]
    fn the_generated_configuration_binds_to_the_loopback_interface() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::Windows);
        let plan = Plan::from_config(&paths, &config(3306));

        let path = write_config(&database, &plan, Os::Windows).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        assert!(text.starts_with("# Generated by Lambo PHP"));
        assert!(text.contains("bind-address = 127.0.0.1"), "{text}");
        assert!(text.contains("port = 3306"));
        assert!(text.contains("[mysqld]"));
        assert!(text.contains("[client]"));
        assert!(text.contains("character-set-server = utf8mb4"));
        // No password anywhere in the configuration file.
        assert!(
            !text.contains("generated-password"),
            "secrets do not belong in my.cnf:\n{text}"
        );
        assert!(
            !text.contains('\\'),
            "backslashes break Windows paths:\n{text}"
        );
    }

    #[test]
    fn a_unix_configuration_gets_a_socket_and_windows_does_not() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::Linux);

        let unix_plan = Plan::from_config(&paths, &config(3306)).with_socket(Os::Linux, &paths);
        assert!(unix_plan.socket.is_some());
        write_config(&database, &unix_plan, Os::Linux).unwrap();
        let text = std::fs::read_to_string(&unix_plan.config_file).unwrap();
        assert!(text.contains("socket = "), "{text}");

        let windows_plan =
            Plan::from_config(&paths, &config(3306)).with_socket(Os::Windows, &paths);
        assert!(windows_plan.socket.is_none());
        write_config(&database, &windows_plan, Os::Windows).unwrap();
        let text = std::fs::read_to_string(&windows_plan.config_file).unwrap();
        assert!(
            !text.contains("socket ="),
            "Windows clients connect over TCP:\n{text}"
        );
    }

    #[test]
    fn a_path_with_a_space_is_quoted_in_the_configuration() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::Windows);
        let mut plan = Plan::from_config(&paths, &config(3306));
        plan.data_dir = PathBuf::from(r"C:\Lambo\My Data\database");
        plan.tmp_dir = PathBuf::from(r"C:\Lambo\My Data\tmp");

        write_config(&database, &plan, Os::Windows).unwrap();
        let text = std::fs::read_to_string(&plan.config_file).unwrap();
        assert!(
            text.contains("datadir = \"C:/Lambo/My Data/database\""),
            "{text}"
        );
    }

    #[test]
    fn initialization_uses_the_right_tool_for_each_engine_and_platform() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::Windows);
        let plan = Plan::from_config(&paths, &config(3306));

        let windows = initialize_spec(&database, &plan, Os::Windows).unwrap();
        let rendered = windows.render();
        assert!(rendered.contains("-d"), "{rendered}");
        assert!(
            rendered.contains("-n"),
            "Lambo must not register a Windows service: {rendered}"
        );

        let unix = initialize_spec(&database, &plan, Os::Linux).unwrap();
        let rendered = unix.render();
        assert!(rendered.contains("--datadir="), "{rendered}");
        assert!(
            rendered.contains("--auth-root-authentication-method=normal"),
            "{rendered}"
        );

        // MySQL initializes through the server itself.
        testutil::install_fake_runtime(&paths, RuntimeKind::Mysql, "8.0.40", Os::Windows);
        let mysql_runtime = runtime::installed(&paths, RuntimeKind::Mysql)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let mysql = from_runtime(DatabaseKind::Mysql, mysql_runtime, Os::Windows).unwrap();
        let rendered = initialize_spec(&mysql, &plan, Os::Windows)
            .unwrap()
            .render();
        assert!(rendered.contains("--initialize-insecure"), "{rendered}");
    }

    #[test]
    fn an_already_initialized_data_directory_is_left_alone() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::host());
        let plan = Plan::from_config(&paths, &config(3306)).with_socket(Os::host(), &paths);
        assert!(!is_initialized(&plan));

        std::fs::create_dir_all(plan.data_dir.join("mysql")).unwrap();
        assert!(is_initialized(&plan));
        // initialize() returns early and never runs the initializer.
        assert!(initialize(&database, &plan, Os::host()).is_ok());
    }

    #[test]
    fn the_generated_configuration_never_touches_the_data_directory() {
        // The server the engine starts is the one that decides what happens to
        // its data; the only thing Lambo writes is the configuration it reads
        // at start-up. A regeneration must therefore leave every file in the
        // data directory exactly as it was - the previous implementation
        // asserted this around its own stop, which the engine now owns.
        let temp = TempDir::new();
        let paths = temp.home();
        let os = Os::host();
        let database = fake_mariadb(&paths, "11.4.4", os);
        let plan = Plan::from_config(&paths, &config(3306)).with_socket(os, &paths);

        // Initialize, then put something recognisable in the data directory.
        std::fs::create_dir_all(plan.data_dir.join("mysql")).unwrap();
        std::fs::create_dir_all(plan.data_dir.join("shop")).unwrap();
        std::fs::write(plan.data_dir.join("shop").join("users.ibd"), b"table data").unwrap();
        assert!(is_initialized(&plan));

        let marker = plan.data_dir.join("shop").join("users.ibd");
        let before = std::fs::read(&marker).unwrap();

        let written = write_config(&database, &plan, os).unwrap();
        assert!(written.exists(), "the configuration must be written");

        assert!(
            is_initialized(&plan),
            "the data directory must still look initialized"
        );
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            before,
            "the data must be untouched"
        );
        assert!(
            plan.data_dir.join("mysql").is_dir(),
            "the system schema must not have been recreated or removed"
        );
    }

    #[test]
    fn the_password_is_passed_through_the_environment_not_the_command_line() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::Windows);
        let plan = Plan::from_config(&paths, &config(3306));

        let spec = client_spec(&database, &plan, "top-secret").unwrap();
        let rendered = spec.render();
        assert!(
            !rendered.contains("top-secret"),
            "a password in argv is world-readable: {rendered}"
        );
        assert_eq!(
            spec.env.get("MYSQL_PWD").map(String::as_str),
            Some("top-secret")
        );
        assert!(rendered.contains("--host=127.0.0.1"), "{rendered}");
        assert!(rendered.contains("--port=3306"), "{rendered}");
    }

    #[test]
    fn database_names_are_validated_before_they_reach_sql() {
        let temp = TempDir::new();
        let paths = temp.home();
        let database = fake_mariadb(&paths, "11.4.4", Os::host());
        let plan = Plan::from_config(&paths, &config(3306));

        for hostile in [
            "shop; DROP TABLE users",
            "back`tick",
            "UPPER",
            "",
            "a".repeat(65).as_str(),
        ] {
            let error = create_database(&database, &plan, hostile, Os::host()).unwrap_err();
            assert!(
                matches!(error, Error::InvalidInput(_)),
                "{hostile}: {error:?}"
            );
            let error = drop_database(&database, &plan, hostile, Os::host()).unwrap_err();
            assert!(
                matches!(error, Error::InvalidInput(_)),
                "{hostile}: {error:?}"
            );
        }
    }

    #[test]
    fn the_password_statement_is_a_single_quoted_literal() {
        let sql = set_password_sql("root", "Ab3_kLm9");
        assert_eq!(
            sql,
            "ALTER USER 'root'@'localhost' IDENTIFIED BY 'Ab3_kLm9'; FLUSH PRIVILEGES;"
        );
    }

    #[test]
    fn credentials_are_hidden_by_default() {
        let temp = TempDir::new();
        let paths = temp.home();
        let plan = Plan::from_config(&paths, &config(3306));
        let credentials = Credentials::for_plan(&plan, DatabaseKind::Mariadb, Some("shop"));

        let hidden = credentials.render(false);
        assert!(!hidden.contains("generated-password"), "{hidden}");
        assert!(hidden.contains("hidden"), "{hidden}");

        let revealed = credentials.render(true);
        assert!(revealed.contains("generated-password"), "{revealed}");
        assert!(revealed.contains("database: shop"), "{revealed}");
        assert!(revealed.contains("host:     127.0.0.1"), "{revealed}");
    }

    #[test]
    fn readiness_is_measured_by_the_port_not_by_hope() {
        let temp = TempDir::new();
        let paths = temp.home();
        let port = unused_port();
        let plan = Plan::from_config(&paths, &config(port));

        assert!(!is_ready(&plan));
        let error = wait_until_ready(&plan, std::time::Duration::from_millis(400)).unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
        assert!(error.to_string().contains(&port.to_string()), "{error}");
    }

    #[test]
    fn installing_a_verified_archive_yields_a_usable_server() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = temp.join("mariadb-11.4.4.zip");
        testutil::write_zip(
            &archive,
            &[
                ("mariadb-11.4.4-winx64/", None),
                ("mariadb-11.4.4-winx64/bin/", None),
                (
                    "mariadb-11.4.4-winx64/bin/mysqld.exe",
                    Some(b"MZ".as_slice()),
                ),
                (
                    "mariadb-11.4.4-winx64/bin/mysql.exe",
                    Some(b"MZ".as_slice()),
                ),
                (
                    "mariadb-11.4.4-winx64/bin/mysqladmin.exe",
                    Some(b"MZ".as_slice()),
                ),
                (
                    "mariadb-11.4.4-winx64/bin/mariadb-install-db.exe",
                    Some(b"MZ".as_slice()),
                ),
            ],
        );

        let release = Release {
            version: "11.4.4".to_owned(),
            platform: "windows-x64".to_owned(),
            url: format!("file://{}", archive.display()),
            sha256: Some(crate::sha256::sha256_file(&archive).unwrap()),
            checksum_url: None,
            ..Default::default()
        };
        let database = install_release(
            &paths,
            DatabaseKind::Mariadb,
            &release,
            &LocalDownloader,
            Os::Windows,
            &Default::default(),
        )
        .unwrap();

        assert!(database.server.ends_with("mysqld.exe"));
        assert!(database.client.is_some());
        assert!(database.admin.is_some());
        assert!(database.initializer.is_some());
        assert_eq!(database.describe(), "MariaDB 11.4.4 (managed by Lambo)");
        assert_eq!(
            runtime::active(&paths, RuntimeKind::Mariadb)
                .unwrap()
                .unwrap()
                .name,
            "11.4.4"
        );
    }

    #[test]
    fn a_catalogue_without_the_engine_is_reported_clearly() {
        let temp = TempDir::new();
        let paths = temp.home();
        let catalog = Catalog::embedded().unwrap();
        let platform = crate::platform::Platform::new(Os::MacOs, crate::platform::Arch::Aarch64);

        let error = install(
            &paths,
            &catalog,
            DatabaseKind::Mariadb,
            &"11.4".parse::<VersionSpec>().unwrap(),
            platform,
            &LocalDownloader,
            &Default::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no MariaDB release"), "{error}");

        let error = install(
            &paths,
            &catalog,
            DatabaseKind::None,
            &VersionSpec::Stable,
            platform,
            &LocalDownloader,
            &Default::default(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no database engine is configured"),
            "{error}"
        );
    }

    #[test]
    fn kind_mapping_is_total() {
        assert_eq!(
            runtime_kind(DatabaseKind::Mariadb),
            Some(RuntimeKind::Mariadb)
        );
        assert_eq!(runtime_kind(DatabaseKind::Mysql), Some(RuntimeKind::Mysql));
        assert_eq!(runtime_kind(DatabaseKind::None), None);
        assert_eq!(database_kind(Family::Mariadb), DatabaseKind::Mariadb);
        assert_eq!(database_kind(Family::Mysql), DatabaseKind::Mysql);
        assert_eq!(database_kind(Family::Php), DatabaseKind::None);
    }
}
