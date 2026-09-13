//! `lambo doctor`: what is wrong, and what to do about it.
//!
//! A diagnostic that says "something is broken" is worthless, so every check
//! here produces three things: a verdict, the evidence behind it, and the
//! command that fixes it. The checks are ordered from the most fundamental
//! (can Lambo write to its own home?) to the most specific (does this project's
//! document root exist?), because the first failure usually explains the rest.
//!
//! Nothing here changes anything. `lambo doctor` is read-only, which is what
//! makes it safe to run at any time - including in the middle of a broken
//! `lambo up`.

use std::path::PathBuf;

use crate::apache;
use crate::catalog::{self, Catalog, Family};
use crate::config::{Config, DatabaseKind};
use crate::dbui;
use crate::download;
use crate::logs;
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::port;
use crate::project::Project;
use crate::runtime::{InstalledRuntime, RuntimeKind};
use crate::session;
use crate::state::State;

/// How bad a finding is.
///
/// Ordered by urgency: [`Report::worst`] takes the maximum. `Unsupported` sits
/// below `Warn` because it is a fact about the platform with a stated
/// alternative, not something the user did wrong - it must never turn
/// `lambo doctor` red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Everything is as it should be.
    Ok,
    /// This platform cannot do it; here is the alternative.
    Unsupported,
    /// Something is missing but Lambo can work around it.
    Warn,
    /// This will stop a command from working.
    Fail,
}

impl Severity {
    /// The marker shown in front of a check.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unsupported => "--",
            Self::Warn => "!!",
            Self::Fail => "xx",
        }
    }
}

/// One diagnostic finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: &'static str,
    /// The verdict.
    pub severity: Severity,
    /// What was observed.
    pub detail: String,
    /// The command that fixes it, when there is one.
    pub fix: Option<String>,
}

impl Check {
    /// A passing check.
    pub fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            severity: Severity::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A warning with a fix.
    pub fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            severity: Severity::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// A failure with a fix.
    pub fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            severity: Severity::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// Something this platform does not support, with the way to work around it.
    ///
    /// Used where a capability genuinely cannot exist - no Apache build for
    /// macOS, no `mod_php` in a Unix PHP distribution - so the report says so
    /// plainly instead of reporting a failure the user cannot fix.
    pub fn unsupported(
        name: &'static str,
        detail: impl Into<String>,
        alternative: impl Into<String>,
    ) -> Self {
        Self {
            name,
            severity: Severity::Unsupported,
            detail: detail.into(),
            fix: Some(alternative.into()),
        }
    }

    /// Renders one line of the report.
    pub fn render(&self) -> String {
        let mut line = format!(
            " [{}] {:<12} {}",
            self.severity.marker(),
            self.name,
            self.detail
        );
        if let Some(fix) = &self.fix {
            line.push_str(&format!("\n       fix: {fix}"));
        }
        line
    }
}

/// The whole diagnostic run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every check, in the order it ran.
    pub checks: Vec<Check>,
}

impl Report {
    /// The worst severity found.
    pub fn worst(&self) -> Severity {
        self.checks
            .iter()
            .map(|check| check.severity)
            .max()
            .unwrap_or(Severity::Ok)
    }

    /// The exit code `lambo doctor` should return.
    pub fn exit_code(&self) -> i32 {
        match self.worst() {
            Severity::Ok | Severity::Unsupported | Severity::Warn => 0,
            Severity::Fail => 1,
        }
    }

    /// How many checks fell in each bucket: `(ok, unsupported, warn, fail)`.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut counts = (0, 0, 0, 0);
        for check in &self.checks {
            match check.severity {
                Severity::Ok => counts.0 += 1,
                Severity::Unsupported => counts.1 += 1,
                Severity::Warn => counts.2 += 1,
                Severity::Fail => counts.3 += 1,
            }
        }
        counts
    }

    /// Renders the report the way the CLI prints it.
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = self.checks.iter().map(Check::render).collect();
        let (ok, unsupported, warn, fail) = self.counts();
        lines.push(format!(
            "\n{ok} ok, {unsupported} unsupported, {warn} warning(s), {fail} failure(s)"
        ));
        lines.join("\n")
    }
}

/// Runs every check.
pub fn run(
    paths: &Paths,
    config: &Config,
    project: Option<&Project>,
    catalog: &Catalog,
    platform: Platform,
    os: Os,
) -> Report {
    let mut checks = vec![
        check_home(paths, os),
        check_config(paths, config),
        check_transport(os),
        check_catalog(paths, catalog, os),
        check_readiness(paths, catalog, config, platform),
        check_php(paths, config, os),
        check_server(paths, config, catalog, platform, os),
        check_database(paths, config, catalog, platform, os),
        check_database_ui(paths),
    ];
    checks.extend(check_ports(paths, config, os));
    if let Some(project) = project {
        checks.push(check_project(project, config));
        checks.extend(check_project_runtime(project, config, paths, os));
    }
    checks.push(check_services(paths, os));
    Report { checks }
}

/// Can Lambo read and write its own home?
pub fn check_home(paths: &Paths, os: Os) -> Check {
    let missing: Vec<PathBuf> = paths
        .essential_dirs()
        .into_iter()
        .filter(|dir| !dir.is_dir())
        .collect();
    if !missing.is_empty() {
        let shown = missing
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Check::fail(
            "lambo home",
            format!("missing directories: {shown}"),
            "lambo up creates them on first run",
        );
    }
    if !crate::fsx::is_writable(&paths.data_dir()) {
        return Check::fail(
            "lambo home",
            format!("`{}` is not writable", paths.data_dir().display()),
            "check the folder permissions of your Lambo home",
        );
    }
    let _ = os;
    Check::ok("lambo home", paths.root().display().to_string())
}

/// Is the global configuration readable, and does it have credentials?
pub fn check_config(paths: &Paths, config: &Config) -> Check {
    if config.database.kind.is_enabled() && config.database.password.is_empty() {
        return Check::warn(
            "config",
            "no database password has been generated yet",
            "lambo db install",
        );
    }
    Check::ok("config", paths.config_file().display().to_string())
}

/// Is there a way to download runtimes?
pub fn check_transport(os: Os) -> Check {
    match download::transport_available() {
        true => Check::ok("downloads", "curl is available"),
        false => {
            let program = if os.is_windows() { "curl.exe" } else { "curl" };
            Check::fail(
                "downloads",
                format!("`{program}` was not found, so Lambo cannot download runtimes"),
                if os.is_windows() {
                    "Windows 10 1803 and later ship curl; install it or add it to PATH"
                } else {
                    "install curl with your package manager"
                },
            )
        }
    }
}

/// Says where a broken catalogue entry came from.
///
/// The fix for a broken entry the user wrote is to edit it. The fix for one
/// Lambo shipped is to update Lambo, because no file on disk will make it go
/// away. Naming the wrong one wastes the user's time in a directory that has
/// nothing to do with the problem.
fn catalog_remedy(catalog: &Catalog, paths: &Paths, blocking: &[&catalog::CatalogIssue]) -> String {
    let overridden = blocking
        .iter()
        .any(|issue| catalog.is_overridden(issue.family));
    match overridden {
        true => format!(
            "edit the entry in `{}`; it comes from one of your catalogue overrides",
            paths.catalogs_dir().display()
        ),
        false => "it is listed in the catalogue that ships with this Lambo, so update Lambo - \
             or override the entry in `config/catalogs/` if you have a verified mirror"
            .to_owned(),
    }
}

/// Can the artifacts this machine needs actually be installed?
///
/// Distinct from [`check_catalog`], which counts what the catalogue *lists*.
/// An entry can be listed for this platform and still be uninstallable, because
/// nothing can confirm its bytes - and "2 PHP releases available" is a misleading
/// thing to print above a command that then fails closed.
///
/// This separates the two facts: what is listed, and what is verifiable. It is
/// reported as unsupported rather than failed, because there is nothing broken
/// on the user's machine to fix; the missing digests are Lambo's to add.
pub fn check_readiness(
    paths: &Paths,
    catalog: &Catalog,
    config: &Config,
    platform: Platform,
) -> Check {
    let key = platform.key();
    let mirror = config.sources.mirror.trim();

    let mut listed = 0usize;
    let mut verifiable = 0usize;
    for family in [
        catalog::Family::Php,
        catalog::Family::Apache,
        catalog::Family::Mariadb,
        catalog::Family::Mysql,
    ] {
        for release in catalog.available(family, &key) {
            listed += 1;
            // A pinned digest, or a sidecar published beside the official URL.
            // Neither means the download fails closed.
            if release.sha256.is_some() || release.checksum_url.is_some() {
                verifiable += 1;
            }
        }
    }

    let origin = if mirror.is_empty() {
        "the official catalogue"
    } else {
        "a configured mirror"
    };

    if listed == 0 {
        return Check::unsupported(
            "runtime readiness",
            format!("the catalogue lists nothing for {key}"),
            "add an entry in `config/catalogs/`, or see docs/configuration.md",
        );
    }
    if verifiable == 0 {
        return Check::unsupported(
            "runtime readiness",
            format!(
                "{listed} artifact(s) are listed for {key} but none has a digest Lambo can \
                 verify, so none can be installed"
            ),
            format!(
                "{}. Or place an already-verified artifact in `{}` - see docs/configuration.md",
                crate::config::PIN_A_DIGEST,
                crate::sources::artifacts_dir(&config.sources, paths).display()
            ),
        );
    }
    if verifiable < listed {
        return Check::warn(
            "runtime readiness",
            format!(
                "{verifiable} of {listed} artifacts listed for {key} can be verified; the rest \
                 will be refused"
            ),
            "pin a digest for the remainder, or they cannot be installed",
        );
    }
    Check::ok(
        "runtime readiness",
        format!("all {listed} artifacts for {key} can be verified from {origin}"),
    )
}

/// Is the catalogue fit to ship in a release?
///
/// Stricter than [`check_catalog`], and deliberately so. A missing digest is
/// legitimate for a running Lambo - verification metadata is unavailable and
/// the download fails closed - but it is not legitimate in a published
/// catalogue, where it means no user can install that entry at all.
///
/// This validates metadata only; it never downloads an artifact, so it is cheap
/// enough to run on every commit and in the release pipeline alike.
pub fn check_release_catalog(catalog: &Catalog) -> Check {
    let blocking = catalog.release_blocking_issues();
    if blocking.is_empty() {
        return Check::ok(
            "release catalogue",
            "every entry carries a pinned digest and valid metadata",
        );
    }
    let first = &blocking[0];
    let detail = if blocking.len() == 1 {
        first.to_string()
    } else {
        format!(
            "{} entries are not releasable, the first being {first}",
            blocking.len()
        )
    };
    Check::fail(
        "release catalogue",
        detail,
        "calculate each artifact's SHA-256 and record it in `catalogs/default.json`; \
         see docs/release.md",
    )
}

/// Does the download catalogue offer anything for this platform?
///
/// A malformed user override in `$LAMBO_HOME/config/catalogs/` is a quiet way
/// to break every install command, so it is surfaced here with the reason.
///
/// Validation runs before the count. A catalogue that lists three PHP releases
/// for this platform and has a malformed digest on one of them is not "3 PHP
/// releases" - one of them is a dead entry that will fail after the download
/// completes, which is the most expensive possible moment to find out.
pub fn check_catalog(paths: &Paths, catalog: &Catalog, os: Os) -> Check {
    let platform = crate::platform::Platform::host();
    let _ = os;

    let issues = catalog.validate();
    let blocking: Vec<&catalog::CatalogIssue> = issues
        .iter()
        .filter(|issue| issue.severity == catalog::IssueSeverity::Error)
        .collect();
    let warnings: Vec<&catalog::CatalogIssue> = issues
        .iter()
        .filter(|issue| issue.severity == catalog::IssueSeverity::Warning)
        .collect();

    if !blocking.is_empty() {
        let first = blocking[0];
        let detail = if blocking.len() == 1 {
            first.to_string()
        } else {
            format!("{} broken entries, the first being {first}", blocking.len())
        };
        return Check::fail(
            "catalogue",
            detail,
            catalog_remedy(catalog, paths, &blocking),
        );
    }

    let php = catalog
        .available(catalog::Family::Php, &platform.key())
        .len();
    let apache = catalog
        .available(catalog::Family::Apache, &platform.key())
        .len();
    let database = catalog
        .available(catalog::Family::Mariadb, &platform.key())
        .len();
    if php == 0 {
        return Check::fail(
            "catalogue",
            format!("no PHP release is listed for {}", platform.key()),
            "check $LAMBO_HOME/config/catalogs/php.json, or update Lambo",
        );
    }

    let detail = format!(
        "{php} PHP, {apache} Apache and {database} MariaDB releases for {}",
        platform.key()
    );
    match warnings.first() {
        Some(first) if warnings.len() == 1 => {
            Check::warn("catalogue", detail, first.message.clone())
        }
        Some(first) => Check::warn(
            "catalogue",
            detail,
            format!(
                "{} catalogue entries need attention, e.g. {}",
                warnings.len(),
                first.message
            ),
        ),
        None => Check::ok("catalogue", detail),
    }
}

/// Is a usable PHP installed?
///
/// "Usable" means it runs. This check used to read the filesystem and the
/// module list, so a runtime whose binary would not start reported itself as
/// `PHP 8.4.2 (/path)` - a green tick over something that cannot serve a
/// request. It now starts PHP and reports what PHP actually said.
pub fn check_php(paths: &Paths, config: &Config, os: Os) -> Check {
    let runtime = match crate::php::current(paths) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => {
            return Check::fail(
                "php",
                "no PHP version is installed",
                format!("lambo php install {}", config.php.default),
            );
        }
        Err(error) => return Check::fail("php", error.to_string(), "lambo php install"),
    };

    let health = crate::php::RuntimeHealth::check(paths, &runtime, os);

    // A runtime that will not start is a failure whatever else is true of it.
    // Reporting its extension list would be reporting a guess.
    if !health.ran {
        return Check::fail(
            "php",
            health.describe(),
            format!(
                "lambo php remove {} && lambo php install {}",
                runtime.name, runtime.name
            ),
        );
    }

    let detail = format!(
        "PHP {} ({}) - {}",
        runtime.version,
        runtime.path.display(),
        health.describe()
    );

    // The generated configuration is the contract Lambo makes with the user:
    // they never edit php.ini because Lambo's is the one in effect. If PHP
    // loaded something else, that contract is broken and every setting Lambo
    // "wrote" is inert.
    let expected_ini = crate::php::php_ini_path(&runtime, os);
    if expected_ini.is_file() && health.loaded_ini.as_deref() != Some(expected_ini.as_path()) {
        let actual = health
            .loaded_ini
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "no configuration file".to_owned());
        return Check::warn(
            "php",
            format!("{detail}, but PHP loaded {actual}"),
            format!(
                "lambo config show php - check whether `PHPRC` or a system php.ini is \
                 overriding {}",
                expected_ini.display()
            ),
        );
    }

    let wanted = ["mysqli", "pdo_mysql"];
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|name| !health.modules.iter().any(|module| module.as_str() == *name))
        .collect();
    if !missing.is_empty() && config.database.kind.is_enabled() {
        return Check::warn(
            "php",
            format!(
                "PHP {} is missing {} - database access will fail",
                runtime.version,
                missing.join(", ")
            ),
            "lambo php install <version> - Lambo's builds ship both extensions",
        );
    }

    Check::ok("php", detail)
}

/// Is there a web server to run?
pub fn check_server(
    paths: &Paths,
    config: &Config,
    catalog: &Catalog,
    platform: Platform,
    os: Os,
) -> Check {
    match apache::discover(paths, os) {
        Some(apache) => {
            let config_file = paths.apache_config_file();
            if config_file.is_file() {
                match apache::validate(paths, &apache, os) {
                    Ok(_) => Check::ok("apache", apache.describe()),
                    Err(error) => Check::fail(
                        "apache",
                        format!("the generated configuration is invalid: {error}"),
                        "lambo server restart",
                    ),
                }
            } else {
                Check::ok(
                    "apache",
                    format!("{} (no configuration generated yet)", apache.describe()),
                )
            }
        }
        None => match config.server.kind {
            crate::config::ServerKind::Php => Check::warn(
                "apache",
                "no Apache installed; PHP's built-in server will be used",
                "`lambo config set server.kind apache` switches to Apache",
            ),
            // Nothing installed. Whether Lambo can fix that depends on the
            // catalogue, not on the platform: Apache is only published for
            // Windows, so on macOS the honest answer is "unsupported".
            _ if catalog
                .available(Family::Apache, &platform.key())
                .is_empty() =>
            {
                Check::unsupported(
                    "apache",
                    format!(
                        "no Apache build is published for {} in the catalogue",
                        platform.key()
                    ),
                    format!(
                        "{}. Alternatively install Apache yourself and `lambo up` will find it",
                        crate::config::SWITCH_TO_PHP_SERVER
                    ),
                )
            }
            _ => Check::fail(
                "apache",
                "no Apache installation was found",
                if os.is_windows() {
                    "`lambo up` downloads and installs Apache automatically".to_owned()
                } else {
                    format!(
                        "`lambo up` installs Apache automatically, or switch to PHP's built-in \
                         server: {}",
                        crate::config::SWITCH_TO_PHP_SERVER
                    )
                },
            ),
        },
    }
}

/// Is the database engine installed and initialized?
pub fn check_database(
    paths: &Paths,
    config: &Config,
    catalog: &Catalog,
    platform: Platform,
    os: Os,
) -> Check {
    if !config.database.kind.is_enabled() {
        return Check::ok("database", "disabled in the configuration");
    }
    let kind = config.database.kind;
    let Some(database) = crate::database::discover(paths, kind, os) else {
        // A configured engine with no catalogue entry for this platform cannot
        // be installed by Lambo. Say so, and name the switch that avoids it.
        let family = crate::database::family_for(kind);
        if catalog.available(family, &platform.key()).is_empty() {
            return Check::unsupported(
                "database",
                format!(
                    "no {} build is published for {} in the catalogue",
                    kind.display_name(),
                    platform.key()
                ),
                "install the server yourself and `lambo db` will find it, or \
                 `lambo config set database.kind none` to run without a database",
            );
        }
        return Check::fail(
            "database",
            format!("no {} installation was found", kind.display_name()),
            "lambo db install",
        );
    };
    let plan = crate::database::Plan::from_config(paths, &config.database).with_socket(os, paths);
    if !crate::database::is_initialized(&plan) {
        return Check::warn(
            "database",
            format!(
                "{} is installed but its data directory is empty",
                database.describe()
            ),
            "lambo db install",
        );
    }
    if crate::database::is_ready(&plan) {
        Check::ok(
            "database",
            format!(
                "{} listening on {}",
                database.describe(),
                plan.host_and_port()
            ),
        )
    } else {
        Check::ok(
            "database",
            format!("{} installed, not running", database.describe()),
        )
    }
}

/// Is the database manager available?
pub fn check_database_ui(paths: &Paths) -> Check {
    match dbui::is_installed(paths) {
        true => Check::ok("db ui", dbui::entry_path(paths).display().to_string()),
        false => Check::warn(
            "db ui",
            "no database manager installed; `lambo db open` will not work",
            "lambo db install-ui",
        ),
    }
}

/// Are the configured ports usable?
///
/// A port held by a Lambo service is fine; a port held by anything else is a
/// problem the user has to resolve, because Lambo never kills other
/// applications' processes.
pub fn check_ports(paths: &Paths, config: &Config, os: Os) -> Vec<Check> {
    let state = State::load(paths).unwrap_or_default();
    let ours: Vec<u16> = state
        .ordered()
        .into_iter()
        .filter(|record| record.is_alive(os))
        .filter_map(|record| record.port)
        .collect();

    let mut checks = Vec::new();
    for (number, key) in [
        (config.server.port, "server.port"),
        (config.database.port, "database.port"),
        (config.dbui.port, "dbui.port"),
    ] {
        if port::is_free(number) {
            checks.push(Check::ok("ports", format!("{key}: {number} is free")));
        } else if ours.contains(&number) {
            checks.push(Check::ok(
                "ports",
                format!("{key}: {number} is in use by Lambo"),
            ));
        } else {
            let occupant = port::occupant(number, os);
            // `detail` carries the explanation and `fix` carries something
            // runnable. Putting the prose in `fix` reads as a command that does
            // not exist, which is worse than giving no fix at all.
            let occupant_note = occupant
                .as_deref()
                .map(|name| format!(" by {name}"))
                .unwrap_or_default();
            let detail = format!(
                "{key}: {number} is already in use{occupant_note}; \
                 `lambo up` will serve on the next free port instead"
            );
            let first_free = port::first_free(port::alternatives(number));
            let fix = match first_free {
                Some(candidate) => format!("lambo config set {key} {candidate}"),
                None => format!("lambo config set {key} <free port>"),
            };
            checks.push(Check::fail("ports", detail, fix));
        }
    }
    checks
}

/// Is the project's configuration usable?
pub fn check_project(project: &Project, config: &Config) -> Check {
    if let Err(error) = project.validate() {
        return Check::fail("project", error.to_string(), "edit lambo.yml");
    }
    let detail = format!(
        "{}: {} served from {}",
        project.name(),
        project.detection.framework.display_name(),
        project.document_root().display()
    );
    let _ = config;
    Check::ok("project", detail)
}

/// Does the project have what its own configuration asks for?
pub fn check_project_runtime(
    project: &Project,
    config: &Config,
    paths: &Paths,
    os: Os,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // The PHP the project asks for must actually be installed - and run.
    //
    // "Satisfies ~8.3" is a claim about serving requests, not about a directory
    // existing. This matters most when the project pins a version other than
    // the active one: `check_php` health-checks the active runtime, so a
    // broken *pinned* version would otherwise get a green tick here while
    // `lambo up` went on to fail on it.
    let spec = project.php_spec(config);
    match crate::php::resolve(paths, &spec) {
        Ok(runtime) => {
            let health = crate::php::RuntimeHealth::check(paths, &runtime, os);
            if health.ran {
                checks.push(Check::ok(
                    "project php",
                    format!("PHP {} satisfies {spec}", runtime.version),
                ));
            } else {
                checks.push(Check::fail(
                    "project php",
                    format!(
                        "PHP {} satisfies {spec} but {}",
                        runtime.version,
                        health.describe()
                    ),
                    format!(
                        "lambo php remove {} && lambo php install {}",
                        runtime.name, runtime.name
                    ),
                ));
            }
        }
        Err(_) => checks.push(Check::fail(
            "project php",
            format!("no installed PHP satisfies `{spec}`"),
            format!("lambo php install {spec}"),
        )),
    }

    // A project that wants a database needs its engine.
    let kind = project.database_kind(config);
    if kind.is_enabled() && crate::database::discover(paths, kind, os).is_none() {
        checks.push(Check::fail(
            "project db",
            format!(
                "this project needs {} and none is installed",
                kind.display_name()
            ),
            "lambo db install",
        ));
    }

    // Apache needs mod_php for the project's PHP; without it a `.php` request
    // is served as a download, which is the single most confusing failure in
    // this whole stack.
    if matches!(
        project.server_kind(config),
        crate::config::ServerKind::Apache
    ) {
        if let (Ok(runtime), Some(apache)) = (
            crate::php::resolve(paths, &spec),
            apache::discover(paths, os),
        ) {
            let _ = apache;
            if apache::php_module(&runtime, os).is_none() && os.is_windows() {
                checks.push(Check::fail(
                    "project apache",
                    "this PHP build has no Apache module (`php8apache2_4.dll`), so Apache cannot \
                     run PHP",
                    format!(
                        "lambo php install <version> with a thread-safe build, or switch to PHP's \
                         built-in server: {}",
                        crate::config::SWITCH_TO_PHP_SERVER
                    ),
                ));
            }
        }
    }

    checks
}

/// Are the recorded services actually alive?
pub fn check_services(paths: &Paths, os: Os) -> Check {
    let state = match State::load(paths) {
        Ok(state) => state,
        Err(error) => return Check::fail("services", error.to_string(), "lambo down"),
    };
    if state.is_empty() {
        return Check::ok("services", "nothing is recorded as running");
    }

    let alive = state.alive(os);
    let dead: Vec<&str> = state
        .ordered()
        .into_iter()
        .filter(|record| !record.is_alive(os))
        .map(|record| record.name.as_str())
        .collect();
    if !dead.is_empty() {
        return Check::warn(
            "services",
            format!(
                "{} recorded but no longer running (see the logs)",
                dead.join(", ")
            ),
            "lambo down",
        );
    }
    let names: Vec<&str> = alive.iter().map(|record| record.name.as_str()).collect();
    Check::ok("services", format!("running: {}", names.join(", ")))
}

/// The log directory to point a user at.
pub fn log_directory(paths: &Paths) -> PathBuf {
    logs::dir(paths, logs::Group::Lambo)
}

/// The runtime a doctor report should mention for a family.
pub fn active(paths: &Paths, kind: RuntimeKind) -> Option<InstalledRuntime> {
    session::active_runtime(paths, kind)
}

/// Whether a project needs the database checks at all.
pub fn needs_database(kind: DatabaseKind) -> bool {
    kind.is_enabled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;
    use crate::state::{ServiceRecord, names};
    use crate::testutil::{self, TempDir};

    fn setup() -> (TempDir, Paths, Config) {
        let temp = TempDir::new();
        let paths = temp.home();
        paths.ensure_layout().unwrap();
        let config = Config::default();
        (temp, paths, config)
    }

    #[test]
    fn a_fresh_install_reports_the_three_things_that_are_missing() {
        let (_temp, paths, config) = setup();
        let report = run(
            &paths,
            &config,
            None,
            &Catalog::embedded().unwrap(),
            Platform::host(),
            Os::host(),
        );

        let by_name = |name: &str| {
            report
                .checks
                .iter()
                .find(|check| check.name == name)
                .unwrap()
        };
        assert_eq!(by_name("lambo home").severity, Severity::Ok);
        assert_eq!(
            by_name("php").severity,
            Severity::Fail,
            "no PHP is installed yet"
        );
        assert_eq!(by_name("database").severity, Severity::Fail);
        assert_eq!(by_name("db ui").severity, Severity::Warn);
        assert_eq!(report.exit_code(), 1);

        let rendered = report.render();
        assert!(rendered.contains("lambo php install"), "{rendered}");
        assert!(rendered.contains("lambo db install"), "{rendered}");
    }

    #[test]
    fn every_failure_names_the_command_that_fixes_it() {
        let (_temp, paths, config) = setup();
        let report = run(
            &paths,
            &config,
            None,
            &Catalog::embedded().unwrap(),
            Platform::host(),
            Os::host(),
        );
        for check in report
            .checks
            .iter()
            .filter(|check| check.severity == Severity::Fail)
        {
            assert!(
                check.fix.is_some(),
                "`{}` fails without a fix: {:?}",
                check.name,
                check
            );
        }
    }

    #[test]
    fn installing_php_turns_its_check_green() {
        let (_temp, paths, mut config) = setup();
        config.database.kind = DatabaseKind::None;
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::host());
        runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();

        let check = check_php(&paths, &config, Os::host());
        assert_eq!(check.severity, Severity::Ok, "{check:?}");
        assert!(check.detail.contains("8.4.2"), "{check:?}");
    }

    #[test]
    fn php_without_database_extensions_is_a_warning_when_a_database_is_configured() {
        let (_temp, paths, config) = setup();
        testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::host());
        runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();

        // The fake runtime reports no loaded modules, and MariaDB is the
        // default engine, so database access would fail.
        let check = check_php(&paths, &config, Os::host());
        assert_eq!(check.severity, Severity::Warn, "{check:?}");
        assert!(check.detail.contains("mysqli"), "{check:?}");
    }

    #[test]
    fn a_managed_apache_is_found_and_described() {
        let (_temp, paths, config) = setup();
        testutil::install_fake_runtime(&paths, RuntimeKind::Apache, "2.4.62", Os::host());
        runtime::set_active(&paths, RuntimeKind::Apache, "2.4.62").unwrap();

        let check = check_server(
            &paths,
            &config,
            &Catalog::embedded().unwrap(),
            Platform::host(),
            Os::host(),
        );
        assert_eq!(check.severity, Severity::Ok, "{check:?}");
        assert!(check.detail.contains("2.4.62"), "{check:?}");
    }

    #[test]
    fn a_port_held_by_something_else_is_a_failure_and_by_lambo_is_fine() {
        let (_temp, paths, mut config) = setup();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = listener.local_addr().unwrap().port();
        config.server.port = taken;
        config.database.port = port::first_free(46_000..46_100).unwrap();
        config.dbui.port = port::first_free(46_100..46_200).unwrap();

        let checks = check_ports(&paths, &config, Os::host());
        let server = checks
            .iter()
            .find(|check| check.detail.contains("server.port"))
            .unwrap();
        assert_eq!(server.severity, Severity::Fail, "{server:?}");
        assert!(server.detail.contains(&taken.to_string()), "{server:?}");
        assert!(
            server.fix.as_deref().unwrap().contains("lambo config set"),
            "{server:?}"
        );

        // The same port is fine when a live Lambo service owns it.
        let mut state = State::default();
        state.record(
            ServiceRecord::new(names::APACHE, std::process::id(), "httpd").with_port(taken),
        );
        state.save(&paths).unwrap();
        let checks = check_ports(&paths, &config, Os::host());
        let server = checks
            .iter()
            .find(|check| check.detail.contains("server.port"))
            .unwrap();
        assert_eq!(server.severity, Severity::Ok, "{server:?}");
        assert!(server.detail.contains("by Lambo"), "{server:?}");
        drop(listener);
    }

    #[test]
    fn a_recorded_but_dead_service_is_reported_as_such() {
        let (_temp, paths, _config) = setup();
        let mut state = State::default();
        state.record(ServiceRecord::new(names::APACHE, u32::MAX, "httpd"));
        state.save(&paths).unwrap();

        let check = check_services(&paths, Os::host());
        assert_eq!(check.severity, Severity::Warn, "{check:?}");
        assert!(check.detail.contains("apache"), "{check:?}");
        assert_eq!(check.fix.as_deref(), Some("lambo down"));
    }

    #[test]
    fn a_live_service_is_reported_as_running() {
        let (_temp, paths, _config) = setup();
        let mut state = State::default();
        // This test process is certainly alive.
        state.record(
            ServiceRecord::new(names::DATABASE, std::process::id(), "mariadbd").with_port(3306),
        );
        state.save(&paths).unwrap();

        let check = check_services(&paths, Os::host());
        assert_eq!(check.severity, Severity::Ok, "{check:?}");
        assert!(check.detail.contains("database"), "{check:?}");
    }

    #[test]
    fn an_initialized_database_directory_changes_the_verdict() {
        let (_temp, paths, config) = setup();
        testutil::install_fake_runtime(&paths, RuntimeKind::Mariadb, "11.4.4", Os::host());
        runtime::set_active(&paths, RuntimeKind::Mariadb, "11.4.4").unwrap();

        let before = check_database(
            &paths,
            &config,
            &Catalog::embedded().unwrap(),
            Platform::host(),
            Os::host(),
        );
        assert_eq!(before.severity, Severity::Warn, "{before:?}");
        assert!(
            before.detail.contains("data directory is empty"),
            "{before:?}"
        );

        std::fs::create_dir_all(paths.database_data_dir().join("mysql")).unwrap();
        let after = check_database(
            &paths,
            &config,
            &Catalog::embedded().unwrap(),
            Platform::host(),
            Os::host(),
        );
        assert_eq!(after.severity, Severity::Ok, "{after:?}");
    }

    #[test]
    fn a_missing_home_directory_is_a_failure() {
        let temp = TempDir::new();
        // Built directly rather than through `home()`, which creates the layout.
        let paths = Paths::from_root(temp.join("fresh-home"));
        let check = check_home(&paths, Os::host());
        assert_eq!(check.severity, Severity::Fail, "{check:?}");
        assert!(check.detail.contains("missing directories"), "{check:?}");
        assert!(
            check.fix.as_deref().unwrap().starts_with("lambo up"),
            "{check:?}"
        );

        paths.ensure_layout().unwrap();
        assert_eq!(check_home(&paths, Os::host()).severity, Severity::Ok);
    }

    #[test]
    fn a_project_without_the_php_it_asks_for_is_reported() {
        let (_temp, paths, config) = setup();
        let root = paths.projects_dir().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.php"), "<?php\n").unwrap();
        std::fs::write(
            root.join("lambo.yml"),
            "php: '8.1'\nserver:\n  document_root: .\n",
        )
        .unwrap();
        let project = Project::load(&root).unwrap();

        assert_eq!(check_project(&project, &config).severity, Severity::Ok);

        let checks = check_project_runtime(&project, &config, &paths, Os::host());
        let php = checks
            .iter()
            .find(|check| check.name == "project php")
            .unwrap();
        assert_eq!(php.severity, Severity::Fail, "{php:?}");
        assert!(
            php.detail.contains("8.1") || php.detail.contains("~8.1"),
            "{php:?}"
        );
        assert!(
            php.fix.as_deref().unwrap().contains("lambo php install"),
            "{php:?}"
        );
    }

    /// A platform the catalogue has no Apache for.
    fn macos_arm() -> Platform {
        Platform {
            os: Os::MacOs,
            arch: crate::platform::Arch::Aarch64,
        }
    }

    #[test]
    fn a_platform_with_no_apache_build_is_reported_as_unsupported() {
        let (_temp, paths, config) = setup();
        let catalog = Catalog::embedded().unwrap();
        assert!(
            catalog
                .available(Family::Apache, &macos_arm().key())
                .is_empty(),
            "the embedded catalogue really must have no Apache for macos-arm64"
        );

        let check = check_server(&paths, &config, &catalog, macos_arm(), Os::MacOs);
        assert_eq!(check.severity, Severity::Unsupported, "{check:?}");
        assert!(check.detail.contains("macos-arm64"), "{check:?}");
        // The alternative has to be actionable, not a shrug.
        let fix = check.fix.as_deref().unwrap();
        assert!(
            fix.contains("server.kind php"),
            "must name the workaround: {fix}"
        );
    }

    #[test]
    fn a_platform_with_an_apache_build_fails_normally_instead() {
        let (_temp, paths, config) = setup();
        let catalog = Catalog::embedded().unwrap();
        let windows = Platform {
            os: Os::Windows,
            arch: crate::platform::Arch::X86_64,
        };
        assert!(!catalog.available(Family::Apache, &windows.key()).is_empty());

        // Nothing installed, but Lambo can install it: that is a fixable
        // failure, not a platform limitation.
        let check = check_server(&paths, &config, &catalog, windows, Os::Windows);
        assert_eq!(check.severity, Severity::Fail, "{check:?}");
        assert!(
            check.fix.as_deref().unwrap().contains("lambo up"),
            "{check:?}"
        );
    }

    #[test]
    fn a_database_with_no_build_for_the_platform_is_unsupported() {
        let (_temp, paths, config) = setup();
        let catalog = Catalog::embedded().unwrap();
        assert!(
            catalog
                .available(Family::Mariadb, &macos_arm().key())
                .is_empty(),
            "the embedded catalogue really must have no MariaDB for macos-arm64"
        );

        let check = check_database(&paths, &config, &catalog, macos_arm(), Os::MacOs);
        assert_eq!(check.severity, Severity::Unsupported, "{check:?}");
        assert!(check.detail.contains("MariaDB"), "{check:?}");
        assert!(
            check.fix.as_deref().unwrap().contains("database.kind none"),
            "{check:?}"
        );
    }

    #[test]
    fn an_unsupported_finding_alone_does_not_fail_the_run() {
        let report = Report {
            checks: vec![
                Check::ok("a", "fine"),
                Check::unsupported(
                    "apache",
                    "no Apache build for macos-arm64",
                    "server.kind: php",
                ),
            ],
        };
        assert_eq!(report.worst(), Severity::Unsupported);
        assert_eq!(
            report.exit_code(),
            0,
            "a platform limitation is not the user's fault and must not turn doctor red"
        );
        assert!(
            report.render().contains("1 unsupported"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn the_report_summary_counts_every_bucket() {
        let report = Report {
            checks: vec![
                Check::ok("a", "fine"),
                Check::unsupported("b", "not on this platform", "use the alternative"),
                Check::warn("c", "hmm", "do a thing"),
                Check::fail("d", "broken", "do another thing"),
            ],
        };
        assert_eq!(report.counts(), (1, 1, 1, 1));
        assert_eq!(report.worst(), Severity::Fail);
        assert_eq!(report.exit_code(), 1);

        let rendered = report.render();
        assert!(rendered.contains("[ok]"), "{rendered}");
        assert!(rendered.contains("[--]"), "{rendered}");
        assert!(rendered.contains("[!!]"), "{rendered}");
        assert!(rendered.contains("[xx]"), "{rendered}");
        assert!(
            rendered.contains("1 ok, 1 unsupported, 1 warning(s), 1 failure(s)"),
            "{rendered}"
        );
        assert!(rendered.contains("fix: do a thing"), "{rendered}");
    }

    #[test]
    fn warnings_do_not_fail_the_command() {
        let report = Report {
            checks: vec![Check::warn("a", "hmm", "fix it")],
        };
        assert_eq!(report.exit_code(), 0);
        assert_eq!(Report::default().exit_code(), 0);
    }
}
