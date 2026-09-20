//! Starting, stopping and reporting on an installation's services.
//!
//! This module is the orchestration layer between the interfaces and the one
//! service engine: `lambo up` is the stack's essential pass, `lambo down` is its
//! Stop All, and `lambo status` reads the engines' live state. Nothing here
//! spawns a process, and nothing here remembers one: what is running is what the
//! engine says is running, which is why there is no state file left to go stale
//! (see [`crate::stack`]).
//!
//! The three rules the old session was built around still hold, and they are now
//! the stack's:
//!
//! 1. **Start in the original's order, stop in its order.** The order comes from
//!    the installation's own configuration, and both directions are the ones
//!    the original used.
//! 2. **A service that cannot be started is installed first.** A component the
//!    catalogue says is missing is installed before the service is started, so a
//!    fresh installation boots rather than failing on every card.
//! 3. **What was started is known.** The engine holds the process handle, so
//!    `down` stops what Lambo started and never guesses at a PID.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::apache::{self, Plan as ApachePlan};
use crate::catalog::Catalog;
use crate::config::{Config, DatabaseKind, ServerKind};
use crate::database::{self, Database, Plan as DatabasePlan};
use crate::dbui;
use crate::download::{Downloader, PanelDownloader, nop_progress};
use crate::download_cache::DownloadCache;
use crate::error::{Error, Result};
use crate::frameworks::{self, CreatedProject, ProjectSink};
use crate::installer::Installer;
use crate::logs::LogFn;
use crate::naming;
use crate::panel::{PanelConfig, PanelProject, ServiceConf, Vhost};
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::port;
use crate::project::Project;
use crate::runtime::{self, InstalledRuntime, RuntimeKind};
use crate::service::{HostService, Service, ServiceConfig};
use crate::stack::{ComponentInstall, EssentialStep, ManagedService, Stack, StartOutcome};
use crate::version::VersionSpec;
use crate::vhost::VhostForm;

/// How long the web server gets to answer its first request.
pub const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The services of one kind, by the names the configuration gives them.
///
/// The panels' names are the catalogue's - `MySQL`, `PostgreSQL`, `Apache` - and
/// the CLI's `db` and `server` commands ask for these rather than the old
/// internal labels, which no longer name anything that runs.
pub const DATABASE_SERVICES: [&str; 2] = ["MySQL", "PostgreSQL"];

/// The web servers, by the names the configuration gives them.
pub const WEB_SERVICES: [&str; 2] = ["Apache", "Nginx"];

/// The database managers, in the order one is preferred.
pub const MANAGER_SERVICES: [&str; 3] = ["phpMyAdmin", "Adminer", "pgweb"];

/// Where the database manager is served when the configuration does not say.
pub const DEFAULT_MANAGER_URL: &str = "http://localhost/phpmyadmin/";

/// The name a project's own server is reported under.
///
/// `server.kind: php` runs PHP's built-in development server - one process, no
/// Apache - and this is the name it carries in the log, in `lambo status` and in
/// a failure: the same name `php::serve_spec` gives it.
pub const PROJECT_SERVER: &str = "php-server";

/// The log file a project's own server writes to.
pub const PROJECT_SERVER_LOG: &str = "server.log";

/// How long the project's own server is given between liveness checks.
const PROJECT_SERVER_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// The installation's own configuration: the log path a project server writes
/// to, so a failed start has somewhere to point.
fn project_server_log(paths: &Paths) -> PathBuf {
    crate::logs::file(paths, crate::logs::Group::Php, PROJECT_SERVER_LOG)
}

/// The engine that supervises a project's PHP built-in server.
///
/// `server.kind: php` is PHP's own development server: `php -S 127.0.0.1:port -t
/// docroot`, with the project's generated `php.ini` passed through `PHPRC`. It is
/// the server for Linux and macOS, where Lambo ships no Apache build, and for any
/// project that wants the simplest thing that serves PHP.
///
/// It goes through [`crate::service::Service`] - the one engine - so its start,
/// its state callback, its log streaming and its tree kill are the same code the
/// panel's cards use. Nothing here spawns a process or remembers a PID.
pub fn project_server_engine(
    project: &Project,
    runtime: &InstalledRuntime,
    context: &Context<'_>,
) -> Result<Arc<Service>> {
    let port = project.http_port(&context.config);
    let spec = crate::php::serve_spec(
        runtime,
        port,
        &project.document_root(),
        &project_server_log(&context.paths),
        context.os,
    )?;

    let mut config = ServiceConfig::new(PROJECT_SERVER, spec.program.clone())
        .args(spec.args.clone())
        .port(port);
    if let Some(directory) = &spec.cwd {
        config = config.work_dir(directory.clone());
    }
    for (key, value) in &spec.env {
        config = config.env(key.clone(), value.clone());
    }

    Ok(Service::new(
        Arc::new(HostService::new()),
        config,
        Arc::clone(&context.log),
    ))
}

/// The project's own server, when the project runs one and its runtime is
/// installed.
///
/// `None` covers both "this project uses a web server instead" and "PHP is not
/// installed yet": neither is a service that could be running, and both are
/// answered by the caller, which knows whether it is reporting or installing.
pub fn project_server(project: &Project, context: &Context<'_>) -> Result<Option<Arc<Service>>> {
    if project.server_kind(&context.config) != ServerKind::Php {
        return Ok(None);
    }
    let spec = project.php_spec(&context.config);
    // Not installed is not an error here: the callers that install PHP
    // (`lambo up`, `lambo server start`) ask for the runtime themselves, and the
    // callers that report (`status`, `down`) have nothing to report or stop.
    let Some(runtime) = runtime::resolve(&context.paths, RuntimeKind::Php, &spec)? else {
        return Ok(None);
    };
    Ok(Some(project_server_engine(project, &runtime, context)?))
}

/// Boots a project's server and waits until the project's URL answers.
///
/// Two failures are distinguished, because they are different problems: the
/// server that exited is reported at once - its log has the reason, and waiting
/// the full timeout would turn a broken configuration into a long pause - and a
/// server that stays alive without answering is reported when the bound runs out.
/// Returns the URL the project is served on and the line that describes what
/// happened: a start, or the engine's own `already running (pid N)`.
fn boot_project_server(
    project: &Project,
    runtime: &InstalledRuntime,
    context: &Context<'_>,
) -> Result<(String, String)> {
    let engine = project_server_engine(project, runtime, context)?;
    let url = naming::local_url(engine.config().port);

    // A server already serving this project's port is not started twice. The
    // line is the engine's own - `php-server already running (pid N)` - so the
    // log of a second `lambo up` reads exactly like the log of a second click on
    // a card.
    if let Some(pid) = engine.pid_holding_port() {
        let detail = format!("already running (pid {pid})");
        (context.log)(&format!("[{PROJECT_SERVER}] {detail}"));
        return Ok((url, detail));
    }

    if let Err(error) = engine.start() {
        return Err(Error::service_failed(
            PROJECT_SERVER.to_owned(),
            error.to_string(),
            ["the PHP runtime is installed but would not run"],
        ));
    }
    let started = format!("started on {url}");

    let deadline = std::time::Instant::now() + HTTP_TIMEOUT;
    loop {
        if crate::http::is_up(&url) {
            return Ok((url, started));
        }
        if !engine.running() {
            return Err(Error::service_failed(
                PROJECT_SERVER.to_owned(),
                format!("the server exited without serving {url}"),
                [format!(
                    "its output is in {}",
                    project_server_log(&context.paths).display()
                )],
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Timeout {
                service: PROJECT_SERVER.to_owned(),
                seconds: HTTP_TIMEOUT.as_secs(),
            });
        }
        std::thread::sleep(PROJECT_SERVER_POLL);
    }
}

/// Every service a panel can show: the web servers, the PHP worker, the
/// database engines and the managers.
///
/// The interfaces name services for attribution - "which one failed" - and the
/// names are the configuration's, which is the only place a name comes from now
/// that the engines are the state.
pub const ALL_SERVICES: [&str; 8] = [
    "Apache",
    "Nginx",
    "PHP-FPM",
    "MySQL",
    "PostgreSQL",
    "phpMyAdmin",
    "Adminer",
    "pgweb",
];

/// Everything a session needs, resolved once by the caller.
pub struct Context<'a> {
    /// The Lambo home.
    pub paths: Paths,
    /// Global configuration (mutable: `up` may generate credentials).
    pub config: Config,
    /// Download catalogue, including user overrides.
    pub catalog: Catalog,
    /// The platform being targeted.
    pub platform: Platform,
    /// How downloads happen.
    pub downloader: &'a dyn Downloader,
    /// The operating system model to use.
    pub os: Os,
    /// Where the engine narrates what it does.
    ///
    /// The interfaces set this to their own sink - the CLI leaves it silent and
    /// prints the report instead, the dashboard puts it in its log pane - so the
    /// service lines a user sees come from the engine and not from a copy of its
    /// decisions.
    pub log: LogFn,
}

impl<'a> Context<'a> {
    /// The operating system, spelled out for call sites that need it.
    pub fn os(&self) -> Os {
        self.os
    }

    /// The installation directory: where `config.json` and `bin/` live.
    ///
    /// The original called this `base_dir` and took it from the working directory.
    /// Lambo's installation is its home directory, which is what every path in
    /// the configuration is expanded from.
    pub fn install_dir(&self) -> PathBuf {
        self.paths.root().to_path_buf()
    }
}

/// An installation's services, and the configuration they came from.
///
/// The two belong together: the configuration says which web server is active
/// and which services are enabled, and the stack holds the engines.
pub struct Installation {
    /// The configuration the stack was built from.
    pub config: PanelConfig,
    /// The services themselves.
    pub stack: Stack,
}

impl Installation {
    /// The service with this name.
    pub fn service(&self, name: &str) -> Option<&crate::stack::ManagedService> {
        self.stack.find(name)
    }

    /// Whether the installation's catalogue says this component is installed.
    pub fn installed(&self, name: &str) -> bool {
        crate::catalog_panel::is_installed(name, self.stack.base_dir())
    }

    /// The services of one kind, in the configuration's order.
    pub fn names_of_kind(&self, kind: &str) -> Vec<String> {
        self.config
            .services
            .iter()
            .filter(|service| service.kind.eq_ignore_ascii_case(kind))
            .map(|service| service.name.clone())
            .collect()
    }
}

/// Reads the installation and builds its stack.
///
/// This is `main.go`'s first two steps: the configuration is loaded from the
/// installation directory, and every entry becomes a `ManagedService` - with an
/// engine only when it has an executable.
pub fn installation(context: &Context<'_>) -> Result<Installation> {
    let base_dir = context.install_dir();
    let config = PanelConfig::load(&base_dir)?;
    let stack = Stack::build(
        &base_dir,
        &config,
        Arc::new(HostService::new()),
        Arc::clone(&context.log),
    );
    Ok(Installation { config, stack })
}

/// The installer the stack reaches when a component is missing.
///
/// A start that finds a component absent installs it through the catalogue, the
/// same way the panel's own install button does: the same downloader, the same
/// cache, the same plan. Progress is not reported from here - `up` is a command,
/// not a bar - and the lines the install writes go to the context's log.
pub struct CatalogInstaller {
    installer: Installer,
}

impl CatalogInstaller {
    /// An installer for an installation directory.
    pub fn new(base_dir: &Path, log: LogFn) -> Self {
        let cache = DownloadCache::new(base_dir, Arc::clone(&log), Box::new(PanelDownloader));
        Self {
            installer: Installer::new(base_dir, log, cache, Box::new(PanelDownloader)),
        }
    }
}

impl ComponentInstall for CatalogInstaller {
    fn install(&mut self, name: &str) -> Result<()> {
        self.installer.install(name, &nop_progress())?;
        Ok(())
    }

    fn install_version(&mut self, name: &str, version: &str) -> Result<()> {
        self.installer
            .install_version(name, version, &nop_progress())?;
        Ok(())
    }
}

/// The installation a start runs, with the configurations Lambo generates for
/// it already written.
struct Prepared {
    /// The configuration the stack was built from, generated flags included.
    installation: Installation,
    /// The PHP runtime the project asked for, when a project was involved.
    php: Option<InstalledRuntime>,
    /// What generating the configurations did, for the report.
    steps: Vec<Step>,
}

/// Wraps a configuration in the stack that runs it.
fn installation_from(config: PanelConfig, context: &Context<'_>) -> Installation {
    let base_dir = context.install_dir();
    let stack = Stack::build(
        &base_dir,
        &config,
        Arc::new(HostService::new()),
        Arc::clone(&context.log),
    );
    Installation { config, stack }
}

/// Builds the installation a command runs, generating the configurations the
/// services read.
///
/// Two documents are written before anything starts, and the entries the stack
/// runs are pointed at them:
///
/// - Apache's configuration, generated for the project's document root and
///   validated with `httpd -t`; the entry gets `-f <generated>` when it has no
///   arguments of its own.
/// - The database's configuration, generated from the plan; the entry gets
///   `--defaults-file=<generated>` in front of its arguments.
///
/// Writing both is what the previous implementation did inside its own start
/// functions. What the cut-over changed is where the *result* lives: the flags
/// travel in the service configuration the engine reads, so the process a card
/// starts, the process `lambo status` reports and the process `lambo down`
/// stops are one object rather than two views of the same intent.
///
/// The PHP runtime is ensured first when a project is given: Apache's
/// configuration names the module to load, and PHP's built-in server *is* the
/// runtime.
///
/// Which database configuration is written - and whether one is written at all -
/// is the *project's* decision, resolved against the installation's setting. A
/// project that says `database.kind: none` gets no `my.cnf`, no credentials and
/// no database services, which is what `kind: none` means; a project that names
/// an engine gets that engine's configuration even when the installation's own
/// default is something else.
fn prepared_installation(project: Option<&Project>, context: &mut Context<'_>) -> Result<Prepared> {
    let mut steps = Vec::new();
    let base_dir = context.install_dir();
    let mut config = PanelConfig::load(&base_dir)?;
    let mut php = None;

    if let Some(project) = project {
        match project.server_kind(&context.config) {
            ServerKind::Apache => {
                let runtime = ensure_php(project, context)?;
                let listen = service_port(&config, "Apache").unwrap_or(80);
                let generated = prepare_apache(project, &runtime, context, listen)?;
                if point_apache_at(config.service_mut("Apache"), &generated) {
                    steps.push(Step::done(
                        "apache",
                        format!("generated {}", generated.display()),
                    ));
                }
                php = Some(runtime);
            }
            // PHP's own server needs the runtime and nothing else; the caller
            // starts it.
            ServerKind::Php => php = Some(ensure_php(project, context)?),
            // `validate` refuses nginx before anything gets here.
            ServerKind::Nginx => {}
        }
    }

    let database_kind = match project {
        Some(project) => project.database_kind(&context.config),
        None => context.config.database.kind,
    };
    if database_kind.is_enabled() {
        let port = service_port(&config, "MySQL").unwrap_or(context.config.database.port);
        let (database, plan) = database_target(context, database_kind, port)?;
        let generated = database::write_config(&database, &plan, context.os)?;
        if point_database_at(config.service_mut("MySQL"), &generated) {
            steps.push(Step::done(
                "database",
                format!("generated {}", generated.display()),
            ));
        }
    } else if project.is_some() {
        steps.push(Step::skipped("database", "this project does not use one"));
    }

    let installation = installation_from(config, context);
    Ok(Prepared {
        installation,
        php,
        steps,
    })
}

/// The port a configured service listens on, when it has one.
fn service_port(config: &PanelConfig, name: &str) -> Option<u16> {
    config
        .service(name)
        .map(|service| service.port)
        .filter(|port| *port > 0)
}

/// Generates and validates the Apache configuration for a project.
///
/// The executable is discovered, and installed when it is not there yet: the
/// configuration names the module directory, so it cannot be written before
/// Apache exists. Validation is Apache's own parser - `httpd -t` - because a
/// configuration Lambo believes in and Apache does not is worse than no server
/// at all: it fails on the first request instead of at start.
fn prepare_apache(
    project: &Project,
    php: &InstalledRuntime,
    context: &Context<'_>,
    listen: u16,
) -> Result<PathBuf> {
    let apache = match apache::discover(&context.paths, context.os) {
        Some(apache) => apache,
        None => apache::install(
            &context.paths,
            &context.catalog,
            &VersionSpec::Stable,
            context.platform,
            context.downloader,
            &context.config.sources,
        )?,
    };

    let plan = ApachePlan {
        php: apache::php_module(php, context.os),
        apache,
        port: listen,
        document_root: project.document_root(),
        project_name: project.name(),
        allow_override: true,
        directory_index: vec!["index.php".to_owned(), "index.html".to_owned()],
        // Mount the database manager into the same site when it is installed,
        // so `http://localhost/phpmyadmin` works without a second port.
        aliases: dbui::aliases(&context.paths),
    };

    let generated = apache::write_config(&context.paths, &plan, context.os)?;
    apache::validate(&context.paths, &plan.apache, context.os)?;
    Ok(generated)
}

/// Points a web server at the configuration Lambo generated for it.
///
/// Only an entry with no arguments of its own is pointed: arguments a user put
/// in `config.json` are a deliberate choice, and the generated configuration is
/// Lambo's default rather than an override of it. Returns whether the flag was
/// added.
fn point_apache_at(service: Option<&mut ServiceConf>, generated: &Path) -> bool {
    let Some(service) = service else {
        return false;
    };
    if !service.args.is_empty() {
        return false;
    }
    service.args = vec!["-f".to_owned(), generated.display().to_string()];
    true
}

/// Puts the generated database configuration first on a database entry.
///
/// `--defaults-file` is only honoured as the first argument, so it goes in
/// front; every argument after it wins over the file, which is how the entry's
/// own arguments keep their meaning while the settings Lambo computed - the
/// bind address, the socket, the character set - apply. Returns whether the
/// flag was added.
fn point_database_at(service: Option<&mut ServiceConf>, generated: &Path) -> bool {
    let Some(service) = service else {
        return false;
    };
    if service
        .args
        .iter()
        .any(|argument| argument.starts_with("--defaults-file"))
    {
        return false;
    }
    service
        .args
        .insert(0, format!("--defaults-file={}", generated.display()));
    true
}

/// Boots the installation's stack for a project.
///
/// The steps are the ones the previous implementation ran, and they keep their
/// order:
///
/// 1. **Validate.** Nothing is generated or started for a project Lambo cannot
///    make sense of.
/// 2. **Generate.** The PHP runtime is ensured, then Apache's configuration for
///    the project's document root and the database's own configuration.
/// 3. **Start.** The dashboard's Start Stack: the active web server, PHP-FPM,
///    the database and the database manager, in the configuration's order,
///    installing whatever the catalogue says is missing.
/// 4. **Open.** The page the pass ends on.
pub fn up(project: &Project, context: &mut Context<'_>, open_browser: bool) -> Result<Report> {
    let mut report = Report::default();

    project.validate()?;
    report.push(Step::done(
        "validate",
        format!("{} ({})", project.name(), project.detection.summary()),
    ));

    // The project's own choices, resolved once: its server decides what is
    // started, and its database decides whether a database is provisioned at
    // all. Both fall back to the installation's configuration when the project
    // does not say, so a project that says nothing behaves as it always did.
    let server_kind = project.server_kind(&context.config);
    let database_kind = project.database_kind(&context.config);

    let prepared = prepared_installation(Some(project), context)?;
    let php = prepared.php.clone();
    if let Some(php) = &php {
        report.push(Step::done("php", format!("PHP {}", php.version)));
    }
    for step in prepared.steps {
        report.push(step);
    }

    let preparation = prepared.installation;
    let url = match server_kind {
        // PHP's built-in server: one process, started and watched by the same
        // engine the panel's cards use, on the port the project asks for.
        ServerKind::Php => {
            let runtime = php.ok_or(Error::RuntimeMissing {
                kind: "PHP",
                command: "lambo php install",
            })?;
            let (url, detail) = boot_project_server(project, &runtime, context)?;
            report.push(Step::done(PROJECT_SERVER, detail));
            url
        }
        // Apache (or the active web server): the installation's essential pass,
        // with the database only when this project uses one.
        _ => {
            let names = preparation
                .config
                .essential_services_for(database_kind.is_enabled());
            let mut installer =
                CatalogInstaller::new(&context.install_dir(), Arc::clone(&context.log));
            let run = preparation
                .stack
                .ensure_essentials_for(&names, &mut installer, &|pause| std::thread::sleep(pause));

            for step in &run.steps {
                report.push(step_report(step));
            }

            // Nothing is reported as up until it answers. The essential pass
            // starts processes; whether the project is *served* is a question
            // for the project's own URL.
            let url = run.open_url.clone();
            if !crate::http::is_up(&url) {
                crate::http::wait_until_up(&url, HTTP_TIMEOUT).map_err(|error| {
                    Error::service_failed(
                        preparation.config.active_web_server().to_owned(),
                        format!("the server started but never answered {url}"),
                        [error.to_string()],
                    )
                })?;
            }
            url
        }
    };
    report.url = Some(url.clone());

    if open_browser {
        match crate::browser::open(&url, context.os) {
            Ok(()) => {
                report.browser_opened = true;
                report.push(Step::done("browser", "opened in your default browser"));
            }
            Err(error) => report.push(Step::skipped("browser", error.to_string())),
        }
    }

    Ok(report)
}

/// One step of `up`, from what the stack did.
fn step_report(step: &EssentialStep) -> Step {
    match step {
        EssentialStep::Started(name) => Step::done(name.clone(), "started"),
        EssentialStep::InstalledThenStarted(name) => {
            Step::done(name.clone(), "installed, then started")
        }
        EssentialStep::Installed(name) => Step::done(name.clone(), "installed"),
        EssentialStep::InstallFailed(name) => {
            Step::failed(name.clone(), "install failed; see the log")
        }
        EssentialStep::Skipped(name) => Step::skipped(name.clone(), "not started"),
        EssentialStep::Opened(name, url) => Step::done(name.clone(), format!("opened {url}")),
        EssentialStep::Nothing(name) => Step::skipped(name.clone(), "nothing to start"),
        EssentialStep::ExeMissing(name, path) => Step::failed(
            name.clone(),
            format!("executable missing: {}", path.display()),
        ),
        EssentialStep::Failed(name, reason) => Step::failed(name.clone(), reason.clone()),
    }
}

/// Stops every service of the installation, and the project's own server.
///
/// The original's Stop All: configuration order, and one service that will not
/// stop does not leave the rest running. Two things are added, both because a
/// command line is not the process that started what it is stopping:
///
/// - **The project's own server.** `lambo up` starts `php -S` for a
///   `server.kind: php` project; that server is not one of the installation's
///   configured services, so it is stopped explicitly when the project is
///   known - `lambo down` run inside the project. Its port is released either
///   way, because the sweep below covers what this installation runs.
/// - **The sweep.** A run that was killed leaves Lambo's own programs behind
///   under the installation directory; they are this installation's, they hold
///   the ports the next start needs, and they are what the original's launch
///   sweep existed to clear.
pub fn down(project: Option<&Project>, context: &mut Context<'_>) -> Result<Report> {
    let installation = installation(context)?;
    let mut report = Report::default();

    // What is running *before* the stop: `stop_all` says nothing, as the
    // original's did not, and a service that was already stopped is not a step -
    // a home where nothing runs must report exactly that.
    let running: Vec<(String, u32)> = installation
        .stack
        .services()
        .iter()
        .filter(|service| service.running())
        .map(|service| (service.name().to_owned(), service.pid().unwrap_or(0)))
        .collect();

    installation.stack.stop_all();

    for (name, pid) in &running {
        report.push(Step::done(name.clone(), format!("stopped (pid {pid})")));
    }

    // The project server, before the sweep: it is named, so the report can say
    // what it was.
    if let Some(project) = project {
        if let Ok(Some(engine)) = project_server(project, context) {
            if let Some(pid) = engine.pid_holding_port() {
                engine.stop()?;
                report.push(Step::done(
                    PROJECT_SERVER.to_owned(),
                    format!("stopped (pid {pid})"),
                ));
            }
        }
    }

    let swept = installation.stack.sweep();
    if !swept.is_empty() {
        report.push(Step::done(
            "sweep",
            format!("stopped {} leftover process(es)", swept.len()),
        ));
    }

    Ok(report)
}

/// What `lambo status` reports: one card per configured service.
pub fn status(project: Option<&Project>, context: &mut Context<'_>) -> Result<Status> {
    let installation = installation(context)?;
    let mut status = Status::default();

    for service in installation.stack.services() {
        status.services.push(service_status(service, context.os));
    }

    if let Some(project) = project {
        let url = effective_url(&context.paths, project, &context.config, context.os);
        status.serving = crate::http::is_up(&url);
        status.url = Some(url);

        // The project's own server, when it has one. It is not a service of the
        // installation's configuration - there is no card for it, and inventing
        // one would put a `php -S` process on a page about the machine - but it
        // is the server this project is served by, and `lambo status` has to
        // report what is running for the project you asked about.
        if let Ok(Some(engine)) = project_server(project, context) {
            // The strict answer: one PHP runtime serves every project, so the
            // port is what says whether *this* project is being served.
            let pid = engine.pid_holding_port();
            status.services.push(ServiceStatus {
                name: PROJECT_SERVER.to_owned(),
                recorded: true,
                running: pid.is_some(),
                pid,
                port: Some(engine.config().port),
                uptime: None,
                log: Some(project_server_log(&context.paths)),
                occupant: if pid.is_some() {
                    None
                } else {
                    port::occupant(engine.config().port, context.os)
                },
            });
        }
    }

    Ok(status)
}

/// One service, as the status command sees it.
///
/// The engine's own answer is the only source: `recorded` means "has an engine",
/// and `running` is whether that engine holds a live process.
fn service_status(service: &ManagedService, os: Os) -> ServiceStatus {
    let port = (service.conf().port > 0).then_some(service.conf().port);
    let running = service.running();

    ServiceStatus {
        name: service.name().to_owned(),
        recorded: service.service().is_some(),
        running,
        pid: service.pid(),
        port,
        // The engine has no start time: the original's card carried the PID and
        // the port, and `Running  pid N` is what it said.
        uptime: None,
        // The engine keeps its output in the service's own log, which the card
        // opens rather than names.
        log: None,
        occupant: match (running, port) {
            (false, Some(port)) => port::occupant(port, os),
            _ => None,
        },
    }
}

/// The port the installation's active web server is on, when something is
/// listening there.
pub fn active_http_port(paths: &Paths, _os: Os) -> Option<u16> {
    let config = PanelConfig::load(paths.root()).ok()?;
    let port = config.service(config.active_web_server())?.port;
    if port == 0 || !port::is_listening(port) {
        return None;
    }
    Some(port)
}

/// Installs, initializes, secures and starts the installation's database.
///
/// Returns the detail line the CLI prints. The database is a service of the
/// installation like any other, so it is started by the engine; the port it
/// takes is the one the configuration gives it, because that is the port its
/// client tools are configured with.
pub fn start_database(
    context: &mut Context<'_>,
    project: Option<&Project>,
    kind: DatabaseKind,
    port: u16,
) -> Result<String> {
    let _ = project;
    let name = database_service(kind)?;
    let base_dir = context.install_dir();

    // The server reads the configuration Lambo generates for it, as it did
    // before: the credentials the plan carries, the socket on Unix and the
    // character set are all in that file.
    let (database, plan) = database_target(context, kind, port)?;
    let generated = database::write_config(&database, &plan, context.os)?;
    let mut config = PanelConfig::load(&base_dir)?;
    point_database_at(config.service_mut(&name), &generated);
    let installation = installation_from(config, context);

    let mut installer = CatalogInstaller::new(&base_dir, Arc::clone(&context.log));
    let configured = installation
        .service(&name)
        .map(|service| service.conf().port)
        .unwrap_or(0);

    match installation
        .stack
        .start_with_install(&name, &mut installer)?
    {
        StartOutcome::Started | StartOutcome::InstalledThenStarted | StartOutcome::Installed => {}
        StartOutcome::ExeMissing(path) => {
            return Err(Error::RuntimeNotInstalled {
                kind: "database",
                name: name.clone(),
                path,
            });
        }
        StartOutcome::Failed(reason) => {
            return Err(Error::service_failed(
                name.clone(),
                reason,
                ["the engine could not start it, or could not install it"],
            ));
        }
        StartOutcome::Opened(_) | StartOutcome::Nothing => {
            return Err(Error::service_failed(
                name.clone(),
                "the configuration has no database executable",
                ["the service entry has no `exe`"],
            ));
        }
    }

    match configured {
        0 => Ok(format!("{name} is started")),
        port => Ok(format!("{name} is listening on port {port}")),
    }
}

/// Stops the installation's database services.
pub fn stop_database(context: &mut Context<'_>) -> Result<String> {
    let installation = installation(context)?;
    let mut stopped = Vec::new();

    for name in DATABASE_SERVICES {
        let Some(service) = installation.service(name) else {
            continue;
        };
        if service.service().is_none() {
            continue;
        }
        if service.running() {
            installation.stack.stop(name)?;
            stopped.push(name.to_owned());
        }
    }

    if stopped.is_empty() {
        Ok("the database is not running".to_owned())
    } else {
        Ok(format!("stopped {}", stopped.join(", ")))
    }
}

/// Starts the installation's web server for a project.
///
/// "Just the web server" is the active one, run against the configuration
/// generated for this project's document root. The engines are asked first: a
/// server that is already running is reported as running rather than started
/// twice, which is the original's `already running (pid N)`.
pub fn start_server(project: &Project, context: &mut Context<'_>) -> Result<ServerStart> {
    project.validate()?;

    // A project that runs PHP's own server has no installation service to ask:
    // the server is the runtime, started for this project's document root.
    if project.server_kind(&context.config) == ServerKind::Php {
        let runtime = ensure_php(project, context)?;
        let port = project.http_port(&context.config);
        if let Some(engine) = project_server(project, context)? {
            if let Some(pid) = engine.pid_holding_port() {
                return Ok(ServerStart {
                    port,
                    message: format!("already running (pid {pid})"),
                    note: None,
                });
            }
        }
        let (_, detail) = boot_project_server(project, &runtime, context)?;
        return Ok(ServerStart {
            port,
            message: if detail.starts_with("already running") {
                detail
            } else {
                format!("{PROJECT_SERVER} is serving on port {port}")
            },
            note: None,
        });
    }

    let preparation = prepared_installation(Some(project), context)?.installation;
    let active = preparation.config.active_web_server().to_owned();
    let port = preparation
        .service(&active)
        .map(|service| service.conf().port)
        .filter(|port| *port > 0)
        .unwrap_or(80);

    if let Some(pid) = preparation
        .service(&active)
        .and_then(|service| service.pid())
    {
        return Ok(ServerStart {
            port,
            message: format!("already running (pid {pid})"),
            note: None,
        });
    }

    let mut installer = CatalogInstaller::new(&context.install_dir(), Arc::clone(&context.log));
    match preparation
        .stack
        .start_with_install(&active, &mut installer)?
    {
        StartOutcome::Started | StartOutcome::InstalledThenStarted => {}
        StartOutcome::Failed(reason) => {
            return Err(Error::service_failed(
                active.clone(),
                reason,
                ["the engine could not start it"],
            ));
        }
        StartOutcome::ExeMissing(path) => {
            return Err(Error::RuntimeNotInstalled {
                kind: "web server",
                name: active.clone(),
                path,
            });
        }
        other => {
            return Err(Error::service_failed(
                active.clone(),
                format!("{active} could not be started ({other:?})"),
                ["the service entry has no executable"],
            ));
        }
    }

    Ok(ServerStart {
        port,
        message: format!("{active} is serving on port {port}"),
        note: None,
    })
}

/// Stops the installation's web servers, and the project's own server.
///
/// `server.kind: php` starts a server that belongs to the project rather than to
/// the machine, so `lambo server stop` stops that one when it knows the project;
/// the installation's servers are stopped either way, which is what makes the
/// command outside a project still do what it says.
pub fn stop_server(project: Option<&Project>, context: &mut Context<'_>) -> Result<String> {
    let installation = installation(context)?;
    let mut stopped = Vec::new();

    if let Some(project) = project {
        if let Ok(Some(engine)) = project_server(project, context) {
            if engine.pid_holding_port().is_some() {
                engine.stop()?;
                stopped.push(PROJECT_SERVER.to_owned());
            }
        }
    }

    for name in WEB_SERVICES {
        let Some(service) = installation.service(name) else {
            continue;
        };
        if service.service().is_some() && service.running() {
            installation.stack.stop(name)?;
            stopped.push(name.to_owned());
        }
    }

    if stopped.is_empty() {
        Ok("the web server is not running".to_owned())
    } else {
        Ok(format!("stopped {}", stopped.join(", ")))
    }
}

/// Opens the installation's database manager.
///
/// A manager is a card with no executable, so this is the card's own behaviour:
/// a component the catalogue says is missing is installed first, and the URL the
/// configuration gives it is what opens. The name of a project's database is
/// kept in the signature for the callers that have one; the manager is served at
/// one address for the whole installation.
pub fn open_database_ui(
    context: &mut Context<'_>,
    name: Option<&str>,
    open: bool,
) -> Result<String> {
    let _ = name;
    let installation = installation(context)?;
    let manager = MANAGER_SERVICES
        .iter()
        .find(|candidate| installation.service(candidate).is_some())
        .map(|candidate| (*candidate).to_owned())
        .ok_or_else(|| {
            Error::InvalidInput("this configuration has no database manager".to_owned())
        })?;

    let base_dir = context.install_dir();
    if crate::catalog_panel::find(&manager).is_some()
        && !crate::catalog_panel::is_installed(&manager, &base_dir)
    {
        // The card's Start button installs a missing manager rather than opening
        // a page that is not there yet.
        let mut installer = CatalogInstaller::new(&base_dir, Arc::clone(&context.log));
        if let Some(service) = installation.service(&manager) {
            let version = service.conf().active_version.clone();
            installer.install_version(&manager, &version)?;
        }
    }

    let url = installation
        .service(&manager)
        .map(|service| service.conf().open_url.clone())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_MANAGER_URL.to_owned());

    if open {
        crate::browser::open(&url, context.os)?;
    }
    Ok(url)
}

/// The configured service a database kind runs as.
///
/// Public because attribution needs it: an operation that targets exactly one
/// service may blame that row when it fails without naming anything, and the
/// name is the configuration's, not a label this module invented.
pub fn database_service(kind: DatabaseKind) -> Result<String> {
    match kind {
        DatabaseKind::Mariadb | DatabaseKind::Mysql => Ok("MySQL".to_owned()),
        DatabaseKind::None => Err(Error::InvalidInput(
            "no database engine is configured; set `database.kind` in lambo.yml".to_owned(),
        )),
    }
}

/// The result of one step of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// It happened.
    Done,
    /// It was not needed.
    Skipped,
    /// It went wrong.
    Failed,
}

/// One step of a session, as reported to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What was attempted.
    pub name: String,
    /// How it went.
    pub outcome: StepOutcome,
    /// The detail line shown under the step.
    pub detail: String,
}

impl Step {
    /// A step that happened.
    pub fn done(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: StepOutcome::Done,
            detail: detail.into(),
        }
    }

    /// A step that was not needed.
    pub fn skipped(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: StepOutcome::Skipped,
            detail: detail.into(),
        }
    }

    /// A step that failed.
    pub fn failed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: StepOutcome::Failed,
            detail: detail.into(),
        }
    }

    /// The marker shown in front of a step.
    pub fn marker(&self) -> &'static str {
        match self.outcome {
            StepOutcome::Done => "+",
            StepOutcome::Skipped => "-",
            StepOutcome::Failed => "x",
        }
    }
}

/// What a session did, for the CLI to print.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The steps that ran, in order.
    pub steps: Vec<Step>,
    /// The URL of the project, once it is serving.
    pub url: Option<String>,
    /// Whether the browser was opened.
    pub browser_opened: bool,
}

impl Report {
    /// Records a step.
    pub fn push(&mut self, step: Step) {
        self.steps.push(step);
    }

    /// Renders the report the way the CLI prints it.
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = self
            .steps
            .iter()
            .map(|step| format!(" {} {}  {}", step.marker(), step.name, step.detail))
            .collect();
        if let Some(url) = &self.url {
            lines.push(format!(" → {url}"));
        }
        lines.join("\n")
    }
}

/// The observed state of one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// Service name.
    pub name: String,
    /// Whether Lambo has a record of it.
    pub recorded: bool,
    /// Whether the recorded process is actually alive.
    pub running: bool,
    /// Process identifier, when recorded.
    pub pid: Option<u32>,
    /// Port, when the service has one.
    pub port: Option<u16>,
    /// Uptime text, when running.
    pub uptime: Option<String>,
    /// Log file, when known.
    pub log: Option<PathBuf>,
    /// What is actually listening on the port, when something is.
    pub occupant: Option<String>,
}

impl ServiceStatus {
    /// The word shown for this service.
    pub fn state_word(&self) -> &'static str {
        match (self.recorded, self.running) {
            (true, true) => "running",
            (true, false) => "stopped",
            (false, _) => "not started",
        }
    }
}

/// Everything `lambo status` reports.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// One entry per known service.
    pub services: Vec<ServiceStatus>,
    /// The project URL, when a project is involved.
    pub url: Option<String>,
    /// Whether the project URL answers.
    pub serving: bool,
}

/// Resolves the project's PHP, installing it when it is missing.
pub fn ensure_php(project: &Project, context: &mut Context<'_>) -> Result<InstalledRuntime> {
    let spec = project.php_spec(&context.config);
    match crate::php::resolve(&context.paths, &spec) {
        Ok(runtime) => {
            // Found on disk - but "found" is not "runs". A runtime installed
            // before the health gate existed, or one whose binary broke
            // afterwards, resolves fine and would then be handed to Apache.
            // `lambo up` would report the server as up while PHP could not
            // serve a single request, which is the one thing this project is
            // not allowed to do.
            //
            // Only for the host platform, for the same reason as the install
            // gate: a runtime for another platform is not expected to execute
            // here.
            if context.platform.os == crate::platform::Os::host() {
                let health = crate::php::RuntimeHealth::check(&context.paths, &runtime, context.os);
                if !health.ran {
                    return Err(Error::ServiceFailed {
                        service: format!("PHP {}", runtime.name),
                        reason: health.describe(),
                        causes: vec![
                            "the runtime is installed and its files are all present".to_owned(),
                            "the binary could not be executed".to_owned(),
                        ],
                        hint: Some(format!(
                            "lambo php remove {} && lambo php install {}",
                            runtime.name, runtime.name
                        )),
                    });
                }
            }
            Ok(runtime)
        }
        // Missing, or the active marker points at a version that is gone:
        // install what the project asked for.
        Err(_) => crate::php::install(
            &context.paths,
            &context.catalog,
            &spec,
            context.platform,
            context.downloader,
            &context.config.sources,
        )
        .map_err(|error| Error::ServiceFailed {
            service: format!("PHP {spec}"),
            reason: error.to_string(),
            causes: vec![
                "the project asks for a version Lambo cannot install on this machine".to_owned(),
                format!(
                    "`lambo php list-versions` shows what is available for {}",
                    context.platform.key()
                ),
                "a release without a pinned checksum is never run".to_owned(),
            ],
            hint: Some(format!(
                "lambo php install {spec}, or `lambo init --php <version>`"
            )),
        }),
    }
}

/// Resolves the database server and the plan it runs with.
///
/// Installs the runtime when it is missing, because every database command -
/// `lambo db start`, `create`, `shell` - needs a server to talk to.
pub fn database_target(
    context: &mut Context<'_>,
    kind: DatabaseKind,
    port: u16,
) -> Result<(Database, DatabasePlan)> {
    let database = match database::discover(&context.paths, kind, context.os) {
        Some(database) => database,
        None => database::install(
            &context.paths,
            &context.catalog,
            kind,
            &VersionSpec::Stable,
            context.platform,
            context.downloader,
            &context.config.sources,
        )?,
    };
    let mut plan = DatabasePlan::from_config(&context.paths, &context.config.database)
        .with_socket(context.os, &context.paths);
    plan.port = port;
    Ok((database, plan))
}

/// What `lambo server start` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStart {
    /// The port actually bound.
    pub port: u16,
    /// The human-readable outcome.
    pub message: String,
    /// Why the port differs from the configuration, when it does.
    pub note: Option<String>,
}

impl ServerStart {
    /// The URL to show the user, built from the port that was really bound.
    pub fn url(&self) -> String {
        crate::naming::local_url(self.port)
    }
}

/// The URL to show the user for a project.
///
/// Prefers the port a running server actually bound. `lambo up` may have
/// fallen back from the configured port - port 80 taken, or refused on Unix -
/// and reporting the configured one afterwards would point the user at an
/// address nothing is listening on.
///
/// Every command that displays or opens the project URL must go through this
/// rather than calling `project.url` directly, or the two will disagree.
pub fn effective_url(paths: &Paths, project: &Project, config: &Config, os: Os) -> String {
    let configured = project.http_port(config);
    naming::local_url(active_http_port(paths, os).unwrap_or(configured))
}

/// Waits until a URL answers, polling.
///
/// Used where the process behind the URL is not one Lambo owns for its whole
/// lifetime - notably Apache, which daemonises and whose recorded PID may be a
/// parent that exits normally. Such a service can only be judged by the bounded
/// timeout, never by process liveness.
pub fn wait_for_http(url: &str, timeout: std::time::Duration) -> Result<()> {
    if crate::http::wait_until_up(url, timeout).is_ok() {
        return Ok(());
    }
    Err(Error::Timeout {
        service: url.to_owned(),
        seconds: timeout.as_secs(),
    })
}

/// The runtime a service would use, for `lambo doctor`.
pub fn active_runtime(paths: &Paths, kind: RuntimeKind) -> Option<InstalledRuntime> {
    runtime::active(paths, kind).ok().flatten()
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// The installation's document, loaded for editing.
///
/// This is the analogue of the original's `app.cfg`, and it holds everything the two
/// pages that write to it need: the projects, their virtual hosts and the
/// settings that say where the managed files go. A change is recorded here and
/// persisted by the method that made it; the hosts file and the server
/// configurations are rewritten by [`PanelBook::apply`], which is the original's
/// "Apply to System" split into the two halves the pages actually use - the
/// projects page applies as part of creating or deleting a project, the
/// virtual-hosts page saves first and applies when the user asks.
pub struct PanelBook<'a> {
    base_dir: &'a Path,
    config: PanelConfig,
}

impl<'a> PanelBook<'a> {
    /// Loads the document of an installation.
    ///
    /// A missing document is written with the installation's defaults, which is
    /// what the panel does when it starts.
    pub fn load(base_dir: &'a Path) -> Result<Self> {
        Ok(Self {
            base_dir,
            config: PanelConfig::load(base_dir)?,
        })
    }

    /// The projects the document holds, in the order they were created.
    pub fn projects(&self) -> &[PanelProject] {
        &self.config.projects
    }

    /// The virtual hosts the document holds, in the order they were added.
    pub fn vhosts(&self) -> &[Vhost] {
        &self.config.vhosts
    }

    /// The configuration a caller needs to read (the settings, mostly).
    pub fn config(&self) -> &PanelConfig {
        &self.config
    }

    /// Persists the document.
    ///
    /// The original discarded `SaveConfig`'s result in every handler that was
    /// not creating a project. That is a deviation this port makes on purpose:
    /// an edit the user made, and that the interface believed it saved, must not
    /// disappear silently - so the error travels back to the caller, which logs
    /// it, instead of being thrown away.
    pub fn save(&self) -> Result<()> {
        self.config.save(self.base_dir)
    }

    /// Moves a project and its domains to a new domain.
    ///
    /// Both halves move together: the project row the projects page lists, and
    /// every virtual host on the old domain. Leaving either behind is how a
    /// project ends up answering on a domain the list does not show.
    /// Returns the updated project, or `None` when no project has that name.
    pub fn set_project_domain(&mut self, name: &str, domain: &str) -> Result<Option<PanelProject>> {
        // The rule - which rows move, and what happens to the hosts - is
        // `vhost::move_project_domain`'s, so the page, the CLI and any future
        // caller cannot disagree about it. This only persists the result.
        let moved = crate::vhost::move_project_domain(
            &mut self.config.projects,
            &mut self.config.vhosts,
            name,
            domain,
        );
        if moved.is_some() {
            self.save()?;
        }
        Ok(moved)
    }

    /// Saves a virtual host: the row being edited, or a new one.
    ///
    /// The original's Save handler, exactly: editing keeps the enabled flag of
    /// the row it replaces, so a disabled host is not enabled by editing it, and
    /// a new host is enabled. Nothing is applied here - the page's own button
    /// does that. Returns the row as it was stored, flag included.
    pub fn save_vhost(&mut self, current: Option<&str>, vhost: Vhost) -> Result<Vhost> {
        let stored = crate::vhost::store_vhost(&mut self.config.vhosts, current, vhost);
        self.save()?;
        Ok(stored)
    }

    /// Makes a web server the installation's active one.
    ///
    /// The original's picker, which stopped at writing the setting and saving
    /// the document; the server that has to go is the stack's business
    /// ([`crate::stack::Stack::stop_other_web_servers`]), and so is the line
    /// that says which one was picked.
    pub fn set_active_web_server(&mut self, choice: &str) -> Result<()> {
        self.config.settings.active_web_server = choice.to_owned();
        self.save()
    }

    /// Records which build of a component the installation runs.
    ///
    /// Returns whether a service of that name is in the document. The install
    /// itself is [`crate::installer::Installer::set_active_variant`]'s; this is
    /// what the original's version picker wrote *after* a successful install, and
    /// saving it is what makes the choice outlive the window - the version menu's
    /// check mark and a half-finished component's reinstall both read it. The
    /// original guarded the write with an index bounds check and dropped
    /// `SaveConfig`'s error; the first is the `Ok(false)` here, and the second is
    /// [`PanelBook::save`](Self::save)'s documented deviation.
    pub fn set_active_version(&mut self, name: &str, version: &str) -> Result<bool> {
        let Some(service) = self
            .config
            .services
            .iter_mut()
            .find(|service| service.name == name)
        else {
            return Ok(false);
        };
        service.active_version = version.to_owned();
        self.save()?;
        Ok(true)
    }

    /// Removes a virtual host by domain, returning whether it was there.
    pub fn remove_vhost(&mut self, domain: &str) -> Result<bool> {
        if !crate::vhost::remove_vhost(&mut self.config.vhosts, domain) {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }
}

impl ProjectSink for PanelBook<'_> {
    fn projects(&self) -> &[PanelProject] {
        &self.config.projects
    }

    fn record(&mut self, project: &PanelProject, vhost: &Vhost) -> Result<()> {
        self.config.projects.push(project.clone());
        self.config.vhosts.push(vhost.clone());
        self.save()
    }

    fn remove(&mut self, name: &str) -> Result<bool> {
        let Some(index) = self
            .config
            .projects
            .iter()
            .position(|project| project.name == name)
        else {
            return Ok(false);
        };
        let domain = self.config.projects.remove(index).domain;
        // Every vhost on the domain, not only the one the project registered: a
        // hand-added vhost is the same site, and leaving it behind would serve a
        // directory that no longer exists. The rule is the page's own.
        crate::vhost::remove_vhost(&mut self.config.vhosts, &domain);
        self.save()?;
        Ok(true)
    }

    fn apply(&mut self) -> Result<()> {
        crate::vhost::apply(self.base_dir, &self.config)
    }
}

/// Deletes a project: its directory, its record, and its domains.
///
/// Returns whether there was a project to delete. A deletion does not fail
/// visibly - the directory, the document and the hosts file are each reported
/// through the log, as the original reported them - because the user asked for
/// the project to be gone, and a locked file must not make that look like a
/// refusal.
pub fn delete_project(base_dir: &Path, name: &str, log: LogFn) -> Result<bool> {
    let mut book = PanelBook::load(base_dir)?;
    Ok(frameworks::delete_project(&mut book, base_dir, name, &log))
}

/// The virtual hosts the installation publishes.
pub fn vhosts(base_dir: &Path) -> Result<Vec<Vhost>> {
    Ok(PanelBook::load(base_dir)?.vhosts().to_vec())
}

/// Saves the virtual-host form the page holds.
///
/// The form is validated by the page's own rules ([`crate::vhost::read_vhost_form`]),
/// so `lambo vhosts` and the virtual-hosts page accept and refuse exactly the
/// same input. `current` names the row being edited, if there is one: editing
/// keeps that row's enabled flag, and a new host is enabled.
pub fn save_vhost(
    base_dir: &Path,
    current: Option<&str>,
    form: &VhostForm,
    log: LogFn,
) -> Result<Vhost> {
    let vhost = match crate::vhost::read_vhost_form(form) {
        Ok(vhost) => vhost,
        Err(error) => {
            // The page's own prefix for a rejection, so the log reads the same
            // whichever interface asked.
            log(&format!("vhost save: {error}"));
            return Err(error);
        }
    };

    let mut book = PanelBook::load(base_dir)?;
    match book.save_vhost(current, vhost) {
        // The row as stored, with the enabled flag the save decided.
        Ok(stored) => Ok(stored),
        Err(error) => {
            log(&format!("vhost save: {error}"));
            Err(error)
        }
    }
}

/// Removes a virtual host by domain, returning whether it was there.
///
/// The document is saved; nothing is applied, which is the page's behaviour -
/// its own button publishes a change.
pub fn delete_vhost(base_dir: &Path, domain: &str, log: LogFn) -> Result<bool> {
    let mut book = PanelBook::load(base_dir)?;
    book.remove_vhost(domain).map_err(|error| {
        log(&format!("vhost delete: {error}"));
        error
    })
}

/// Publishes the document: the hosts file, Apache's include, nginx's sites.
///
/// The virtual-hosts page's "Apply to System". The original logged a failure and
/// stopped there; the error is returned here as well, so `lambo` can exit
/// non-zero for a script while the log still reads exactly as it did.
pub fn apply_vhosts(base_dir: &Path, log: LogFn) -> Result<()> {
    let mut book = PanelBook::load(base_dir)?;
    match book.apply() {
        Ok(()) => {
            log("vhosts applied \u{2014} hosts file + Apache/Nginx configs updated");
            Ok(())
        }
        Err(error) => {
            log(&format!("apply vhosts: {error}"));
            log("  \u{2192} if 'access denied', relaunch Lambo PHP as administrator");
            Err(error)
        }
    }
}

/// Moves a project to a new domain and publishes it.
///
/// The change touches three things at once - the project row, its virtual host,
/// and the files those are written from - so it goes through the one
/// implementation that owns all three. Returns whether a project of that name
/// was registered.
pub fn set_project_domain(base_dir: &Path, name: &str, domain: &str, log: LogFn) -> Result<bool> {
    let mut book = PanelBook::load(base_dir)?;
    let Some(project) = book.set_project_domain(name, domain)? else {
        return Ok(false);
    };

    if let Err(error) = book.apply() {
        log(&format!("apply vhosts: {error}"));
        log("  \u{2192} run Lambo PHP as administrator for hosts-file writes to work");
        return Err(error);
    }

    log(&format!(
        "project '{}' now answers on http://{}",
        project.name, project.domain
    ));
    log("NOTE: Apache needs a restart to pick up the changed vhost \u{2014} click 'Restart Stack'");
    Ok(true)
}

/// Creates a project from what the user asked for.
///
/// The steps the projects page takes before the scaffold, in its own order: the
/// framework is looked up by name, the project name is required and is slugified
/// (`My Shop!` becomes `my-shop`), and an empty domain becomes the project's own
/// `.test` name. Both interfaces call this, so `lambo frameworks create` and the
/// projects page behave identically and report the same errors.
pub fn create_project(
    base_dir: &Path,
    framework_name: &str,
    name: &str,
    domain: &str,
    log: LogFn,
) -> Result<CreatedProject> {
    if framework_name.is_empty() {
        return Err(Error::InvalidInput(
            "projects: pick a framework first".to_owned(),
        ));
    }
    let framework = frameworks::framework_by_name(framework_name).ok_or_else(|| {
        Error::InvalidInput(format!("projects: unknown framework {framework_name}"))
    })?;

    // The original's `slugify`, which leaves nothing behind for a name made entirely
    // of punctuation - and that is what rejects it below.
    let name = frameworks::project_slug(name);
    if name.is_empty() {
        return Err(Error::InvalidInput(
            "projects: project name required".to_owned(),
        ));
    }

    let mut book = PanelBook::load(base_dir)?;
    frameworks::create_project(&mut book, base_dir, framework, &name, domain, &log)
}

/// Loads an existing folder as a project, detecting what it is.
///
/// The panel's `Open Project` goes through here, so what the dashboard loads
/// and what `lambo init` would conclude about the same folder cannot disagree.
pub fn adopt_project(
    base_dir: &Path,
    folder: &Path,
    log: LogFn,
) -> Result<frameworks::AdoptedProject> {
    let mut book = PanelBook::load(base_dir)?;
    frameworks::adopt_project(&mut book, base_dir, folder, &log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::testutil::TempDir;

    fn context(temp: &TempDir, config: Config) -> Context<'static> {
        static DOWNLOADER: crate::download::SystemDownloader = crate::download::SystemDownloader;
        let paths = temp.home();
        paths.ensure_layout().expect("the layout");
        Context {
            paths,
            config,
            catalog: crate::catalog::Catalog::embedded().expect("the embedded catalogue"),
            platform: Platform::host(),
            downloader: &DOWNLOADER,
            os: Os::host(),
            log: crate::logs::nop_log(),
        }
    }

    #[test]
    fn the_installation_directory_is_the_home() {
        let temp = TempDir::new();
        let context = context(&temp, Config::default());
        assert_eq!(context.install_dir(), context.paths.root());
    }

    #[test]
    fn making_a_web_server_active_writes_it_to_the_document() {
        let temp = TempDir::new();
        let base_dir = temp.path();

        let mut book = PanelBook::load(base_dir).expect("the document loads");
        assert_eq!(book.config().active_web_server(), "Apache", "the default");

        book.set_active_web_server("Nginx").expect("it saves");

        // The document on disk says so, so the next load does too - which is
        // what the picker is for.
        let reloaded = PanelBook::load(base_dir).expect("it reloads");
        assert_eq!(reloaded.config().settings.active_web_server, "Nginx");
        assert_eq!(reloaded.config().active_web_server(), "Nginx");
        assert!(
            reloaded
                .config()
                .essential_services()
                .contains(&"Nginx".to_owned()),
            "and the essential pass follows the setting"
        );
        assert!(
            !reloaded
                .config()
                .essential_services()
                .contains(&"Apache".to_owned())
        );
    }

    #[test]
    fn a_database_kind_names_the_service_it_runs_as() {
        assert_eq!(database_service(DatabaseKind::Mariadb).unwrap(), "MySQL");
        assert_eq!(database_service(DatabaseKind::Mysql).unwrap(), "MySQL");
        assert!(database_service(DatabaseKind::None).is_err());
    }

    #[test]
    fn an_installation_has_one_card_per_configured_service() {
        let temp = TempDir::new();
        let context = context(&temp, Config::default());

        // Loading creates the default document, so the cards are the shipped
        // ones; the point here is that the stack is built from that document and
        // not from anything in `lambo.yml`.
        let installation = installation(&context).expect("the installation loads");
        assert_eq!(
            installation.stack.services().len(),
            installation.config.services.len()
        );
        assert!(installation.service("Apache").is_some());
        assert!(
            installation
                .names_of_kind("web")
                .contains(&"Apache".to_owned())
        );
    }

    #[test]
    fn the_web_port_is_reported_only_when_something_is_listening() {
        let temp = TempDir::new();
        let context = context(&temp, Config::default());

        // Nothing is running, so there is no port to report, whatever the
        // configuration says.
        assert_eq!(active_http_port(&context.paths, Os::host()), None);
    }

    #[test]
    fn the_report_reads_as_a_list_of_what_happened() {
        let mut report = Report::default();
        report.push(Step::done("Apache", "started"));
        report.push(Step::skipped("phpMyAdmin", "nothing to start"));
        report.url = Some("http://localhost/".to_owned());

        let rendered = report.render();
        assert!(rendered.contains("Apache"));
        assert!(rendered.contains("http://localhost/"));
    }

    #[test]
    fn a_status_word_follows_the_engine() {
        let running = ServiceStatus {
            name: "Apache".to_owned(),
            recorded: true,
            running: true,
            pid: Some(4242),
            port: Some(80),
            uptime: None,
            log: None,
            occupant: None,
        };
        assert_eq!(running.state_word(), "running");

        let idle = ServiceStatus {
            running: false,
            ..running.clone()
        };
        assert_eq!(idle.state_word(), "stopped");

        let tool = ServiceStatus {
            recorded: false,
            ..idle
        };
        assert_eq!(tool.state_word(), "not started");
    }
}
