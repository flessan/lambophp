//! Starting, stopping and reporting on a project's services.
//!
//! `lambo up` is the command the whole tool is judged on, so its contract is
//! written down here:
//!
//! 1. **Validate before starting.** A broken `lambo.yml`, a missing document
//!    root or a port that is already taken is reported before anything is
//!    started, so the user never ends up with a half-running stack and no idea
//!    which part failed.
//! 2. **Start in dependency order, stop in reverse.** Database → server →
//!    database manager, and back again. A page that queries a database must not
//!    be served before the database answers.
//! 3. **Never claim a service is up because a process was spawned.** Each
//!    service is confirmed by observation: a TCP port for the database, an HTTP
//!    response for the web server. A process that started and immediately died
//!    is reported as a failure with its log path.
//! 4. **Record what was started** in `$LAMBO_HOME/data/services.yml` so
//!    `lambo status` and `lambo down` act on facts rather than guesses, and so
//!    Lambo never stops a process it did not start.
//!
//! Everything here goes through the service modules ([`crate::apache`],
//! [`crate::database`], [`crate::dbui`], [`crate::php`]) and takes an explicit
//! [`Os`], so the same code path runs on Windows and Unix.

use std::path::PathBuf;

use crate::apache::{self, Plan as ApachePlan};
use crate::catalog::Catalog;
use crate::config::{Config, DatabaseKind, ServerKind};
use crate::database::{self, Database, Plan as DatabasePlan};
use crate::dbui::{self, Plan as DbUiPlan};
use crate::download::Downloader;
use crate::envfile;
use crate::error::{Error, Result};
use crate::logs;
use crate::naming;
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::port;
use crate::process;
use crate::project::Project;
use crate::runtime::{self, InstalledRuntime, RuntimeKind};
use crate::state::{ServiceRecord, State, names};
use crate::version::VersionSpec;

/// How long the web server gets to answer its first request.
pub const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
}

impl<'a> Context<'a> {
    /// The operating system, spelled out for call sites that need it.
    pub fn os(&self) -> Os {
        self.os
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

/// The services Lambo can run, in the order status lists them.
const STATUS_ORDER: [&str; 4] = [
    names::DATABASE,
    names::APACHE,
    names::PHP_SERVER,
    names::DBUI,
];

/// Brings a project's services up.
///
/// The steps are fixed and ordered; see the module documentation. On failure
/// nothing is left unrecorded: whatever did start is in the state file, so
/// `lambo down` can clean it up.
pub fn up(project: &Project, context: &mut Context<'_>, open_browser: bool) -> Result<Report> {
    let mut report = Report::default();
    let mut state = State::load(&context.paths)?;

    // 1. Validate. Nothing is started until the configuration makes sense.
    project.validate()?;
    report.push(Step::done(
        "validate",
        format!("{} ({})", project.name(), project.detection.summary()),
    ));

    // 2. Credentials, so the database is never provisioned without one.
    let database_kind = project.database_kind(&context.config);
    if database_kind.is_enabled() && context.config.ensure_credentials() {
        context.config.save(&context.paths)?;
        report.push(Step::done("credentials", "generated a database password"));
    }

    // 3. Ports. Resolved before anything binds, so a conflict is reported as a
    //    conflict rather than as a server that "failed to start". The web port
    //    may move to a free one; the database port may not.
    let ports = preflight(project, context, database_kind)?;
    match &ports.http_note {
        // The URL the user is about to be given is not the one they configured.
        // Saying so is not optional: a silent port change looks like Lambo
        // ignoring the configuration.
        Some(note) => report.push(Step::done("ports", note.clone())),
        None => report.push(Step::done("ports", "all required ports are free")),
    }

    // 4. PHP.
    let php = ensure_php(project, context)?;
    report.push(Step::done("php", format!("PHP {}", php.version)));

    // 5. Database.
    if database_kind.is_enabled() {
        let database = bring_up_database(project, context, database_kind)?;
        report.push(Step::done("database", database));
        let env = write_env(project, context)?;
        report.push(Step::done("env", env));
    } else {
        report.push(Step::skipped("database", "this project does not use one"));
    }

    // 6. Web server, on the port that was actually resolved.
    let serving = bring_up_server(project, context, &php, ports.http, &mut state)?;
    report.push(Step::done("server", serving));

    state.save(&context.paths)?;

    // 7. Browser, only once the URL actually answers. The URL must be built
    //    from the resolved port: reporting the configured one after a fallback
    //    would hand the user an address nothing is listening on.
    let url = naming::local_url(ports.http);
    if open_browser {
        match crate::browser::open(&url, context.os) {
            Ok(()) => {
                report.browser_opened = true;
                report.push(Step::done("browser", "opened in your default browser"));
            }
            Err(error) => report.push(Step::skipped("browser", error.to_string())),
        }
    }
    report.url = Some(url);

    Ok(report)
}

/// The ports a session will actually use, and why any differ from the config.
///
/// Only the **web** port is allowed to move. The database port is fixed by
/// contract: applications connect to it, `.env` records it, and a client the
/// user already has open would break if it quietly changed. So a database port
/// conflict is reported as a conflict, while a web port conflict resolves to
/// the next free port and the new URL is stated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPorts {
    /// The port the web server will bind.
    pub http: u16,
    /// The port the configuration asked for.
    pub http_requested: u16,
    /// Why the web port moved, ready to show the user. `None` when it did not.
    pub http_note: Option<String>,
}

/// Checks that every port the project needs is available.
///
/// The web port may move; the database and database-UI ports may not. See
/// [`ResolvedPorts`].
pub fn preflight(
    project: &Project,
    context: &Context<'_>,
    database_kind: DatabaseKind,
) -> Result<ResolvedPorts> {
    let requested = project.http_port(&context.config);
    let resolved = port::resolve_listen_port(requested, context.os);

    // A port Lambo could not bind *and* could not move away from is a hard
    // failure. Guessing a URL Lambo cannot serve would be worse than stopping.
    if resolved.reason.is_some() && !port::is_free(resolved.port) {
        return Err(port_conflict_error(
            resolved.port,
            resolved.occupant().map(str::to_owned),
            "server.port",
        ));
    }

    let mut fixed: Vec<(u16, &str)> = Vec::new();
    if database_kind.is_enabled() {
        fixed.push((project.file.database_port(&context.config), "database.port"));
    }
    if dbui::is_installed(&context.paths) {
        fixed.push((context.config.dbui.port, "dbui.port"));
    }

    for (number, key) in fixed {
        if let Err(error) = port::check(number, context.os) {
            let occupied_by = match &error {
                Error::PortInUse { occupied_by, .. } => occupied_by.clone(),
                _ => None,
            };
            return Err(port_conflict_error(number, occupied_by, key));
        }
    }

    Ok(ResolvedPorts {
        http: resolved.port,
        http_requested: requested,
        http_note: resolved.explanation(context.os),
    })
}

/// The failure reported when a port is taken and cannot be worked around.
fn port_conflict_error(number: u16, occupied_by: Option<String>, key: &str) -> Error {
    Error::ServiceFailed {
        service: format!("port {number}"),
        reason: port::conflict_advice(number, occupied_by.as_deref(), key),
        causes: vec![
            format!("`{key}` in lambo.yml or `lambo config set {key} <port>`"),
            "another application may be using it (Docker, IIS, Skype, …)".to_owned(),
            "find out with: lambo doctor".to_owned(),
        ],
        hint: Some("lambo doctor".to_owned()),
    }
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

/// Installs, initializes, secures and starts the database.
///
/// With a project, the project's own database is created too. Returns the
/// detail line the CLI prints.
pub fn start_database(
    context: &mut Context<'_>,
    project: Option<&Project>,
    kind: DatabaseKind,
    port: u16,
) -> Result<String> {
    if !kind.is_enabled() {
        return Err(Error::InvalidInput(
            "no database engine is configured for this project; set `database.kind` in lambo.yml \
             or `lambo config set database.kind mariadb`"
                .to_owned(),
        ));
    }
    if context.config.ensure_credentials() {
        context.config.save(&context.paths)?;
    }

    let (database, plan) = database_target(context, kind, port)?;
    let mut state = State::load(&context.paths)?;

    if let Some(record) = state
        .get(names::DATABASE)
        .filter(|record| record.is_alive(context.os))
    {
        return Ok(format!(
            "{} already running (pid {})",
            database.describe(),
            record.pid
        ));
    }

    let initialized = !database::is_initialized(&plan);
    database::initialize(&database, &plan, context.os)?;

    let record = database::start(&database, &plan, context.os)?;
    state.record(record.clone());
    // Saved immediately: if a later step fails, `lambo down` can still stop
    // what was started.
    state.save(&context.paths)?;
    database::wait_until_ready(&plan, database::STARTUP_TIMEOUT)
        .map_err(|error| database_failure(&database, &plan, error))?;

    if database::secure(&database, &plan, context.os)? {
        context.config.save(&context.paths)?;
    }

    if let Some(project) = project {
        database::create_database(&database, &plan, &project.database_name(), context.os)?;
    }

    Ok(if initialized {
        format!(
            "{} initialized and listening on {}",
            database.describe(),
            plan.host_and_port()
        )
    } else {
        format!(
            "{} listening on {}",
            database.describe(),
            plan.host_and_port()
        )
    })
}

/// Stops the database server, and only the database server.
pub fn stop_database(context: &mut Context<'_>) -> Result<String> {
    stop_one(context, names::DATABASE, "the database server")
}

/// Starts the project's web server on its own (`lambo server start`).
///
/// The port is resolved first and the URL is confirmed afterwards, so a
/// reported success always means something is answering on the URL given.
///
/// Returns a message and, separately, the port actually bound - the caller has
/// to show the real URL, and after a port fallback that is not the configured
/// one.
pub fn start_server(project: &Project, context: &mut Context<'_>) -> Result<ServerStart> {
    project.validate()?;

    let mut state = State::load(&context.paths)?;
    if let Some(record) = recorded_server(&state).filter(|record| record.is_alive(context.os)) {
        let port = record
            .port
            .unwrap_or_else(|| project.http_port(&context.config));
        return Ok(ServerStart {
            port,
            message: format!("already running (pid {})", record.pid),
            note: None,
        });
    }

    // The web port may move; a database port may not, and is not involved here.
    let requested = project.http_port(&context.config);
    let resolved = port::resolve_listen_port(requested, context.os);
    if resolved.reason.is_some() && !port::is_free(resolved.port) {
        return Err(port_conflict_error(
            resolved.port,
            resolved.occupant().map(str::to_owned),
            "server.port",
        ));
    }

    let php = ensure_php(project, context)?;
    let message = bring_up_server(project, context, &php, resolved.port, &mut state)?;
    Ok(ServerStart {
        port: resolved.port,
        message,
        note: resolved.explanation(context.os),
    })
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
    let state = match State::load(paths) {
        Ok(state) => state,
        Err(_) => return naming::local_url(configured),
    };
    let bound = recorded_server(&state)
        .filter(|record| record.is_alive(os))
        .and_then(|record| record.port);
    naming::local_url(bound.unwrap_or(configured))
}

/// The port a running web server actually bound, when one is running.
///
/// `None` when no server is up, so callers fall back to the configured port.
pub fn active_http_port(paths: &Paths, os: Os) -> Option<u16> {
    let state = State::load(paths).ok()?;
    recorded_server(&state)
        .filter(|record| record.is_alive(os))
        .and_then(|record| record.port)
}

/// Stops the web server, and only the web server.
pub fn stop_server(context: &mut Context<'_>) -> Result<String> {
    let state = State::load(&context.paths)?;
    match recorded_server(&state).map(|record| record.name.clone()) {
        Some(name) => stop_one(context, &name, "the web server"),
        None => Ok("the web server is not running".to_owned()),
    }
}

/// The recorded web server, whichever flavour the project uses.
fn recorded_server(state: &State) -> Option<&ServiceRecord> {
    [names::APACHE, names::PHP_SERVER]
        .iter()
        .find_map(|name| state.get(name))
}

/// Stops one named service and forgets its record.
fn stop_one(context: &mut Context<'_>, name: &str, label: &str) -> Result<String> {
    let mut state = State::load(&context.paths)?;
    let Some(record) = state.get(name).cloned() else {
        return Ok(format!("{label} is not running"));
    };
    if !record.is_alive(context.os) {
        state.remove(name);
        state.save(&context.paths)?;
        return Ok(format!("{label} was already stopped"));
    }
    let detail = match stop_record(context, &record)? {
        process::StopOutcome::Graceful => "stopped cleanly".to_owned(),
        process::StopOutcome::Forced => "terminated".to_owned(),
        process::StopOutcome::AlreadyGone => "was already gone".to_owned(),
        process::StopOutcome::NotOurs => {
            "not ours any more: that PID belongs to another process, so nothing was signalled"
                .to_owned()
        }
        process::StopOutcome::Failed(reason) => reason,
    };
    state.remove(name);
    state.save(&context.paths)?;
    Ok(detail)
}

/// Installs, initializes, secures and starts the database, then makes sure the
/// project's own database exists.
///
/// Returns the detail line for the report.
fn bring_up_database(
    project: &Project,
    context: &mut Context<'_>,
    kind: DatabaseKind,
) -> Result<String> {
    start_database(
        context,
        Some(project),
        kind,
        project.file.database_port(&context.config),
    )
}

/// Writes the project's `.env`, without touching values it already has.
fn write_env(project: &Project, context: &Context<'_>) -> Result<String> {
    let mut env = envfile::ensure_exists(&project.root)?;
    let changes = env.ensure_all(
        &project.env_keys(&context.config, &context.config.database.password),
        false,
    );
    if !changes.is_empty() {
        env.save()?;
    }
    Ok(if changes.written.is_empty() {
        format!(".env already had what it needs ({})", env.path.display())
    } else {
        format!(".env: wrote {}", changes.written.join(", "))
    })
}

/// Starts whichever server the project uses and confirms it serves.
fn bring_up_server(
    project: &Project,
    context: &mut Context<'_>,
    php: &InstalledRuntime,
    port: u16,
    state: &mut State,
) -> Result<String> {
    match project.server_kind(&context.config) {
        ServerKind::Apache => start_apache(project, context, php, port, state),
        ServerKind::Php => start_php_server(project, context, php, port, state),
        ServerKind::Nginx => Err(Error::Unsupported("nginx")),
    }
}

/// Starts Apache with a generated configuration.
fn start_apache(
    project: &Project,
    context: &mut Context<'_>,
    php: &InstalledRuntime,
    listen: u16,
    state: &mut State,
) -> Result<String> {
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

    apache::write_config(&context.paths, &plan, context.os)?;
    apache::validate(&context.paths, &plan.apache, context.os)?;

    let record = apache::start(&context.paths, &plan, context.os)?;
    state.record(record.clone());
    state.save(&context.paths)?;

    let url = naming::local_url(listen);
    // Apache is judged by the bounded timeout alone. `httpd -f` daemonises on
    // Unix, so `record.pid` may be a parent that exits normally; failing fast
    // on that would report a healthy Apache as dead. See `wait_for_service`.
    wait_for_http(&url, HTTP_TIMEOUT).map_err(|error| Error::ServiceFailed {
        service: "Apache".to_owned(),
        reason: "the server started but never answered an HTTP request".to_owned(),
        causes: vec![
            format!(
                "the error log may say why: {}",
                logs::apache(&context.paths).display()
            ),
            error.to_string(),
            "a PHP fatal error during start-up can also stop the first response".to_owned(),
        ],
        hint: Some("lambo logs apache".to_owned()),
    })?;

    Ok(format!("Apache listening on {url}"))
}

/// Starts PHP's built-in server, used when Apache is not available.
fn start_php_server(
    project: &Project,
    context: &mut Context<'_>,
    php: &InstalledRuntime,
    listen: u16,
    state: &mut State,
) -> Result<String> {
    let log = logs::file(&context.paths, logs::Group::Php, "server.log");
    let spec = crate::php::serve_spec(php, listen, &project.document_root(), &log, context.os)?;
    let mut child = process::spawn(&spec, context.os)?;
    let pid = child.id();
    // The handle is kept, not dropped: `php -S` *is* the server, so whether
    // this handle has exited is direct evidence that the server died. Dropping
    // it would cost the exit code and force a blind 30-second wait.
    let mut record = ServiceRecord::new(names::PHP_SERVER, pid, spec.render())
        .with_port(listen)
        .with_project(&project.root)
        .with_log(&log);
    if let Some(identity) = process::identity_settled(
        pid,
        context.os,
        &spec.program,
        std::time::Duration::from_millis(500),
    ) {
        record = record.with_identity(&identity);
    }
    state.record(record.clone());
    state.save(&context.paths)?;

    let url = naming::local_url(listen);
    wait_for_service(
        "the PHP development server",
        &record,
        &url,
        HTTP_TIMEOUT,
        context.os,
        Some(&mut child),
    )
    .map_err(|error| match error {
        // A dead process is already reported completely: the exit, the URL and
        // the log. Re-wrapping it would bury the one fact that matters.
        Error::ServiceFailed { .. } => error,
        // Alive but not answering within the bound. This keeps the wording it
        // has always had; only the dead-process case is new.
        error => Error::ServiceFailed {
            service: "the PHP development server".to_owned(),
            reason: "the server started but never answered an HTTP request".to_owned(),
            causes: vec![format!("log: {}", log.display()), error.to_string()],
            hint: Some("lambo logs php".to_owned()),
        },
    })?;

    Ok(format!(
        "PHP {} built-in server listening on {url}",
        php.version
    ))
}

/// How a process ended, as far as the OS will tell us.
fn exit_detail(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("the process exited with code {code}"),
        // No code means it was killed by a signal (Unix) rather than exiting.
        None => "the process was terminated by a signal".to_owned(),
    }
}

/// The `lambo logs` group that shows a service's own output.
fn log_hint(name: &str) -> String {
    match name {
        names::APACHE => "lambo logs apache",
        names::PHP_SERVER => "lambo logs php",
        names::DATABASE => "lambo logs database",
        _ => "lambo logs",
    }
    .to_owned()
}

/// Builds the failure for a server that died before it could answer.
///
/// This is the case that must not be reported as a timeout: nothing is coming,
/// and the reason is already in the log.
fn startup_exited(service: &str, record: &ServiceRecord, url: &str, detail: &str) -> Error {
    let mut causes = vec![detail.to_owned(), format!("it was expected to serve {url}")];
    if let Some(log) = &record.log {
        causes.push(format!("the log may say why: {}", log.display()));
    }
    if !record.command.is_empty() {
        causes.push(format!("it was started with: {}", record.command));
    }
    Error::ServiceFailed {
        service: service.to_owned(),
        reason: "the server exited before it answered an HTTP request".to_owned(),
        causes,
        hint: Some(log_hint(&record.name)),
    }
}

/// Waits until `url` answers, giving up as soon as the server exits.
///
/// A process that has already terminated will never answer, so polling it for
/// the whole [`HTTP_TIMEOUT`] leaves the user watching a frozen interface for
/// half a minute before being told something that was known in the first
/// second. Liveness is read from the recorded identity - the same PID-reuse-safe
/// test `lambo status` and the dashboard use - so the health wait and the status
/// report cannot disagree about whether the process is still Lambo's.
///
/// `child` is the handle when the caller still holds one; polling it yields an
/// exit code, which is unavailable once the handle is dropped.
///
/// A server that stays alive but never answers still waits the full `timeout`.
/// That is deliberate: slow start-ups are legitimate, and shortening the bound
/// to make failures fast would turn a working-but-slow server into a false
/// failure. Only a *dead* process fails early.
///
/// Callers must only pass a record for a process that is itself the server.
/// Apache is excluded: `httpd -f` daemonises on Unix, so the recorded PID is a
/// parent that exits normally, and treating that as a failure would be wrong.
pub fn wait_for_service(
    service: &str,
    record: &ServiceRecord,
    url: &str,
    timeout: std::time::Duration,
    os: Os,
    mut child: Option<&mut std::process::Child>,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // The handle answers first, because it knows the exit code. Without
        // one, fall back to the identity-checked liveness test.
        let exited = match child.as_mut() {
            Some(handle) => match handle.try_wait() {
                Ok(Some(status)) => Some(exit_detail(status)),
                _ => None,
            },
            None => None,
        }
        .or_else(|| (!record.is_alive(os)).then(|| "the process is no longer running".to_owned()));

        if let Some(detail) = exited {
            return Err(startup_exited(service, record, url, &detail));
        }

        // Bound per iteration, not held across them: the timeout message
        // should describe what the URL did most recently, and there is always
        // a probe result by the time the deadline is checked.
        let last = match crate::http::get(url, crate::http::DEFAULT_TIMEOUT.min(timeout)) {
            Ok(response) if response.is_healthy() => return Ok(()),
            Ok(response) => format!("HTTP {}", response.status),
            Err(error) => error.to_string(),
        };

        if std::time::Instant::now() >= deadline {
            return Err(Error::Timeout {
                service: format!("{url} (last error: {last})"),
                seconds: timeout.as_secs(),
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
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

/// Wraps a database start failure with the causes that actually matter.
fn database_failure(database: &Database, plan: &DatabasePlan, error: Error) -> Error {
    let _ = database;
    Error::ServiceFailed {
        service: "the database server".to_owned(),
        reason: error.to_string(),
        causes: vec![
            format!("the error log may say why: {}", plan.log.display()),
            "the data directory may be incomplete; `lambo db install --force` rebuilds it"
                .to_owned(),
            "another MySQL-compatible server may already own the port".to_owned(),
        ],
        hint: Some("lambo logs database".to_owned()),
    }
}

/// Stops every service Lambo started, in reverse start order.
///
/// Processes that are already gone are dropped from the state file, and nothing
/// else on the machine is touched.
pub fn down(context: &mut Context<'_>) -> Result<Report> {
    let mut report = Report::default();
    let mut state = State::load(&context.paths)?;

    for record in state
        .ordered_for_shutdown()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>()
    {
        if !record.is_alive(context.os) {
            state.remove(&record.name);
            report.push(Step::skipped(record.name.clone(), "already stopped"));
            continue;
        }

        let outcome = stop_record(context, &record)?;
        let detail = match outcome {
            process::StopOutcome::Graceful => "stopped cleanly".to_owned(),
            process::StopOutcome::Forced => "terminated".to_owned(),
            process::StopOutcome::AlreadyGone => "was already gone".to_owned(),
            process::StopOutcome::NotOurs => {
                "stale record: that PID belongs to another process, so nothing was signalled"
                    .to_owned()
            }
            process::StopOutcome::Failed(reason) => reason,
        };
        state.remove(&record.name);
        report.push(Step::done(record.name.clone(), detail));
    }

    state.prune_dead(context.os);
    state.save(&context.paths)?;
    Ok(report)
}

/// Stops one recorded service using the shutdown path that fits it.
fn stop_record(context: &mut Context<'_>, record: &ServiceRecord) -> Result<process::StopOutcome> {
    match record.name.as_str() {
        names::DATABASE => {
            let kind = context.config.database.kind;
            match database::discover(&context.paths, kind, context.os) {
                Some(database) => {
                    let plan = DatabasePlan::from_config(&context.paths, &context.config.database)
                        .with_socket(context.os, &context.paths);
                    database::stop(&database, &plan, record, context.os)
                }
                None => process::stop(record.pid, context.os, None, database::SHUTDOWN_TIMEOUT),
            }
        }
        names::APACHE => match apache::discover(&context.paths, context.os) {
            Some(apache) => apache::stop(&context.paths, &apache, record, context.os),
            None => process::stop(record.pid, context.os, None, apache::graceful_timeout()),
        },
        _ => process::stop(
            record.pid,
            context.os,
            None,
            std::time::Duration::from_secs(10),
        ),
    }
}

/// Reports what Lambo believes is running, corrected by what is actually there.
pub fn status(project: Option<&Project>, context: &mut Context<'_>) -> Result<Status> {
    let mut state = State::load(&context.paths)?;
    let pruned = state.prune_dead(context.os);
    if pruned {
        state.save(&context.paths)?;
    }

    let mut status = Status::default();
    for name in STATUS_ORDER {
        status.services.push(service_status(context, &state, name));
    }

    if let Some(project) = project {
        // The port actually bound, not the configured one - after a fallback
        // they differ, and `status` claiming `serving` on the configured URL
        // while the server listens elsewhere would be a false report.
        let url = effective_url(&context.paths, project, &context.config, context.os);
        status.serving = crate::http::is_up(&url);
        status.url = Some(url);
    }

    Ok(status)
}

/// Builds the observed status of one service.
fn service_status(context: &Context<'_>, state: &State, name: &str) -> ServiceStatus {
    let record = state.get(name);
    let running = record.is_some_and(|record| record.is_alive(context.os));
    let port = record.and_then(|record| record.port);
    ServiceStatus {
        occupant: port.and_then(|port| port::occupant(port, context.os)),
        uptime: record
            .filter(|_| running)
            .map(|record| record.uptime_text()),
        pid: record.map(|record| record.pid),
        log: record.and_then(|record| record.log.clone()),
        name: name.to_owned(),
        recorded: record.is_some(),
        running,
        port,
    }
}

/// Starts the database manager for `lambo db open`.
///
/// The manager is started only when it is not already serving, and the URL it
/// opens carries the connection details - never the password.
pub fn open_database_ui(
    context: &mut Context<'_>,
    database_name: Option<&str>,
    open: bool,
) -> Result<String> {
    let ui = dbui::discover(&context.paths, context.os).ok_or(Error::RuntimeMissing {
        kind: "database manager",
        command: "lambo db install-ui",
    })?;

    // When the site is up, Apache is already serving the manager at
    // `/phpmyadmin` through the alias, so there is nothing to start and no
    // second port for the user to learn. The standalone server below is the
    // fallback for when the project is not running.
    if let Some(http_port) = active_http_port(&context.paths, context.os) {
        let url = dbui::open_url_at(
            http_port,
            context.config.database.port,
            database_name,
            &context.config.database.username,
        );
        if open {
            crate::browser::open(&url, context.os)?;
        }
        return Ok(url);
    }

    let mut state = State::load(&context.paths)?;
    let already = state
        .get(names::DBUI)
        .filter(|record| record.is_alive(context.os))
        .and_then(|record| record.port);

    let port = match already {
        Some(port) => port,
        None => {
            let plan = DbUiPlan::new(
                &context.paths,
                &ui,
                context.config.dbui.port,
                context.config.database.port,
                context.config.database.username.clone(),
                context.os,
                context.config.dbui.kind.clone(),
            )?;
            let record = dbui::start(&plan, context.os)?;
            state.record(record.clone());
            state.save(&context.paths)?;
            crate::http::wait_until_up(
                &format!("http://127.0.0.1:{}/", plan.port),
                std::time::Duration::from_secs(15),
            )
            .map_err(|_| Error::Timeout {
                service: "the database manager".to_owned(),
                seconds: 15,
            })?;
            plan.port
        }
    };

    let url = dbui::open_url(
        port,
        context.config.database.port,
        database_name,
        &context.config.database.username,
    );
    if open {
        crate::browser::open(&url, context.os)?;
    }
    Ok(url)
}

/// The runtime a service would use, for `lambo doctor`.
pub fn active_runtime(paths: &Paths, kind: RuntimeKind) -> Option<InstalledRuntime> {
    runtime::active(paths, kind).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::LocalDownloader;
    use crate::testutil::TempDir;

    fn context(temp: &TempDir, config: Config) -> Context<'static> {
        Context {
            paths: temp.home(),
            config,
            catalog: Catalog::embedded().unwrap(),
            platform: Platform::host(),
            // `LocalDownloader` only handles file:// URLs, so nothing in a test
            // can reach the network by accident.
            downloader: &LocalDownloader,
            os: Os::host(),
        }
    }

    /// A project with a document root and an entry point.
    fn plain_project(temp: &TempDir, extra: &str) -> Project {
        let root = temp.join("shop");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.php"), "<?php echo 'hi';\n").unwrap();
        std::fs::write(
            root.join("lambo.yml"),
            format!("server:\n  document_root: .\n{extra}"),
        )
        .unwrap();
        Project::load(&root).unwrap()
    }

    #[test]
    fn the_reported_url_follows_the_port_a_server_actually_bound() {
        let temp = TempDir::new();
        let paths = temp.home();
        paths.ensure_layout().unwrap();
        let project = plain_project(&temp, "");
        let mut config = Config::default();
        config.server.port = 80;
        let os = Os::host();

        // Nothing running: the configured port is the best answer available.
        assert_eq!(
            effective_url(&paths, &project, &config, os),
            "http://localhost"
        );

        // Now record a live server on a fallback port. `effective_url` must
        // follow it, because reporting 80 here would point the user at an
        // address nothing is listening on. The record points at this test's
        // own process, which is genuinely alive, and carries no identity so
        // liveness alone decides.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bound = listener.local_addr().unwrap().port();
        let mut state = State::load(&paths).unwrap();
        let mut record = ServiceRecord::new(names::PHP_SERVER, std::process::id(), "php -S");
        record.port = Some(bound);
        state.record(record);
        state.save(&paths).unwrap();

        assert_eq!(active_http_port(&paths, os), Some(bound));
        assert_eq!(
            effective_url(&paths, &project, &config, os),
            format!("http://localhost:{bound}"),
            "the URL must follow the port that is really listening"
        );

        drop(listener);
    }

    #[test]
    fn a_taken_port_is_reported_before_anything_is_started() {
        let temp = TempDir::new();
        let project = plain_project(&temp, "");

        // Occupy the port the project wants with a real listener, so the check
        // observes a conflict rather than being told about one.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = listener.local_addr().unwrap().port();
        let mut config = Config::default();
        config.server.port = taken;
        let context = context(&temp, config);

        // The web port is allowed to move: `http://localhost:8081` is a better
        // outcome than refusing to start, as long as the change is stated.
        let ports = preflight(&project, &context, DatabaseKind::None).expect("resolves");
        assert_ne!(ports.http, taken, "must move off the occupied port");
        assert!(port::is_free(ports.http), "and land somewhere usable");
        assert_eq!(ports.http_requested, taken);

        let note = ports
            .http_note
            .as_deref()
            .expect("a port change must be explained, never absorbed silently");
        assert!(note.contains(&taken.to_string()), "{note}");
        assert!(
            note.contains(&ports.http.to_string()),
            "the note must name the URL the user will actually get: {note}"
        );
        drop(listener);
    }

    #[test]
    fn an_occupied_database_port_is_a_hard_failure_not_a_fallback() {
        let temp = TempDir::new();
        let project = plain_project(&temp, "");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = listener.local_addr().unwrap().port();
        let mut config = Config::default();
        config.server.port = port::first_free(45_100..45_200).unwrap();
        config.database.port = taken;
        let context = context(&temp, config);

        // Unlike the web port, the database port is a contract: `.env` records
        // it and applications connect to it. Silently moving it would break
        // every client the user already has configured.
        let error = preflight(&project, &context, DatabaseKind::Mariadb).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&taken.to_string()), "{message}");
        assert!(
            error
                .details()
                .iter()
                .any(|line| line.contains("database.port")),
            "{:?}",
            error.details()
        );
        drop(listener);
    }

    #[test]
    fn free_ports_pass_preflight() {
        let temp = TempDir::new();
        let project = plain_project(&temp, "");
        let mut config = Config::default();
        let free = port::first_free(45_000..45_100).unwrap();
        config.server.port = free;
        let context = context(&temp, config);
        let ports = preflight(&project, &context, DatabaseKind::None).expect("resolves");
        assert_eq!(ports.http, free, "a free port is used as configured");
        assert!(ports.http_note.is_none(), "and nothing needs explaining");
    }

    #[test]
    fn up_refuses_to_run_without_php_and_says_how_to_fix_it() {
        let temp = TempDir::new();
        let project = plain_project(&temp, "");
        let mut context = context(&temp, Config::default());
        let mut report = Report::default();

        // The Lambo home is a fresh temporary directory, so nothing is
        // installed; the catalogue ships no checksums, so the install cannot be
        // verified and must fail closed rather than run unverified code.
        let error = ensure_php(&project, &mut context).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("cannot verify"), "{message}");
        // The remedy must name a mechanism that exists. Earlier revisions of
        // this advice pointed at `lambo config set php.<version>.sha256`, a key
        // that has never been in `Config::KEYS` - the command fails with
        // "unknown configuration key", so the hint described a fix that could
        // not be applied. The catalogue override is the real mechanism.
        assert!(
            message.contains("config/catalogs/"),
            "the remedy must name the override directory: {message}"
        );
        assert!(
            message.contains("lambo config hash"),
            "the remedy must name how to compute the digest: {message}"
        );
        let details = error.details().join("\n");
        assert!(details.contains("lambo php install"), "{details}");
        assert!(details.contains("lambo php list-versions"), "{details}");
        report.push(Step::failed("php", message));
        assert_eq!(report.steps[0].marker(), "x");
        assert!(
            !State::load(&context.paths)
                .unwrap()
                .services
                .contains_key(names::APACHE)
        );
    }

    #[test]
    fn the_report_reads_as_a_list_of_what_happened() {
        let mut report = Report::default();
        report.push(Step::done("validate", "shop (plain PHP)"));
        report.push(Step::skipped("database", "this project does not use one"));
        report.push(Step::done(
            "server",
            "Apache listening on http://localhost:8080",
        ));
        report.url = Some("http://localhost:8080".to_owned());

        let rendered = report.render();
        assert!(
            rendered.contains("+ validate  shop (plain PHP)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("- database  this project does not use one"),
            "{rendered}"
        );
        assert!(rendered.ends_with("→ http://localhost:8080"), "{rendered}");
    }

    #[test]
    fn down_clears_records_of_processes_that_are_already_gone() {
        let temp = TempDir::new();
        let mut context = context(&temp, Config::default());
        let mut state = State::default();
        state.record(
            ServiceRecord::new(names::APACHE, u32::MAX, "httpd -f httpd.conf").with_port(8080),
        );
        state.record(ServiceRecord::new(names::DATABASE, u32::MAX - 1, "mariadbd").with_port(3306));
        state.save(&context.paths).unwrap();

        let report = down(&mut context).unwrap();
        assert_eq!(report.steps.len(), 2);
        assert!(
            report
                .steps
                .iter()
                .all(|step| step.outcome == StepOutcome::Skipped)
        );
        assert!(State::load(&context.paths).unwrap().is_empty());
    }

    #[test]
    fn status_reports_a_recorded_but_dead_service_as_stopped() {
        let temp = TempDir::new();
        let mut context = context(&temp, Config::default());
        let mut state = State::default();
        state.record(ServiceRecord::new(names::APACHE, u32::MAX, "httpd").with_port(8080));
        state.save(&context.paths).unwrap();

        let status = status(None, &mut context).unwrap();
        // The record is pruned because the process is gone, so the service is
        // reported as not started rather than as a corpse that is "running".
        let apache = status
            .services
            .iter()
            .find(|service| service.name == names::APACHE)
            .unwrap();
        assert_eq!(apache.state_word(), "not started");
        assert!(!status.serving);
        assert!(State::load(&context.paths).unwrap().is_empty());
    }

    #[test]
    fn status_lists_every_service_even_when_nothing_is_running() {
        let temp = TempDir::new();
        let mut context = context(&temp, Config::default());
        let status = status(None, &mut context).unwrap();

        let names: Vec<&str> = status
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                names::DATABASE,
                names::APACHE,
                names::PHP_SERVER,
                names::DBUI
            ]
        );
        assert!(
            status
                .services
                .iter()
                .all(|service| service.state_word() == "not started")
        );
    }

    #[test]
    fn status_reports_the_project_url_only_when_it_answers() {
        let temp = TempDir::new();
        let project = plain_project(&temp, "");
        let mut config = Config::default();
        config.server.port = port::first_free(45_100..45_200).unwrap();
        let mut context = context(&temp, config.clone());

        let status = status(Some(&project), &mut context).unwrap();
        assert_eq!(
            status.url.as_deref(),
            Some(naming::local_url(config.server.port).as_str())
        );
        assert!(
            !status.serving,
            "nothing is listening, so nothing may be reported as serving"
        );
    }

    #[test]
    fn env_writing_fills_in_only_what_is_missing() {
        let temp = TempDir::new();
        let mut config = Config::default();
        config.database.password = "s3cret".to_owned();
        let context = context(&temp, config);

        let root = temp.join("shop");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.php"), "<?php\n").unwrap();
        std::fs::write(
            root.join("lambo.yml"),
            "server:\n  document_root: .\ndatabase:\n  kind: mariadb\n",
        )
        .unwrap();
        std::fs::write(root.join(".env"), "DB_HOST=db.internal\nAPP_NAME=Shop\n").unwrap();

        let project = Project::load(&root).unwrap();
        let detail = write_env(&project, &context).unwrap();

        let written = std::fs::read_to_string(root.join(".env")).unwrap();
        assert!(
            written.contains("DB_HOST=db.internal"),
            "the project's own value must win: {written}"
        );
        assert!(written.contains("DB_NAME=shop"), "{written}");
        assert!(written.contains("DB_PASSWORD=s3cret"), "{written}");
        assert!(detail.contains("wrote"), "{detail}");

        // A second run changes nothing.
        let detail = write_env(&project, &context).unwrap();
        assert!(detail.contains("already had"), "{detail}");
    }

    #[test]
    fn a_database_manager_is_only_installed_when_it_can_be_verified() {
        let temp = TempDir::new();
        let mut context = context(&temp, Config::default());
        let error = open_database_ui(&mut context, Some("shop"), false).unwrap_err();
        assert!(matches!(error, Error::RuntimeMissing { .. }), "{error:?}");
        assert!(
            error.details().iter().any(|line| line.contains("lambo db")),
            "{:?}",
            error.details()
        );
    }

    #[test]
    fn the_php_server_spec_binds_the_loopback_interface_and_the_document_root() {
        let temp = TempDir::new();
        let paths = temp.home();
        crate::testutil::install_fake_runtime(&paths, RuntimeKind::Php, "8.4.2", Os::host());
        let php = runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .remove(0);
        let docroot = paths.projects_dir().join("shop");
        let log = logs::php(&paths);

        let spec = crate::php::serve_spec(&php, 8080, &docroot, &log, Os::host()).unwrap();
        let rendered = spec.render();
        assert!(rendered.contains("127.0.0.1:8080"), "{rendered}");
        assert!(rendered.contains("-S"), "{rendered}");
        assert!(
            rendered.contains(&docroot.display().to_string()),
            "{rendered}"
        );
        assert!(spec.detached, "the server must outlive the CLI");
    }
}
