//! The application-level API: the boundary a GUI consumes.
//!
//! The CLI and a GUI are two front ends over one engine, and the spec for this
//! product is explicit that the GUI must not reach the engine by spawning
//! `lambo` and parsing what it prints. Parsed text is a contract nobody
//! maintains: a reworded message silently breaks a screen, and a screen that
//! guesses at state is worse than one that admits it does not know.
//!
//! So this module is the seam. It returns **structured models** - every one of
//! them `Serialize`, so a GUI can consume them in-process today and across a
//! boundary later without the models changing.
//!
//! # What this layer is not
//!
//! It holds no business logic. Every method composes [`crate::session`],
//! [`crate::doctor`], [`crate::php`], [`crate::dbui`], [`crate::workspace`] and
//! [`crate::logs`]. If a decision about ports, runtimes or services is being
//! made here, it is in the wrong place.
//!
//! # States, not booleans
//!
//! [`ServiceState`] and [`RuntimeState`] are enumerations rather than
//! on/off flags. "Not installed", "corrupt" and "failed to start" are different
//! situations needing different UI, and collapsing them loses exactly the
//! information the user came for.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::catalog::Catalog;
use crate::config::Config;
use crate::doctor;
use crate::download::SystemDownloader;
use crate::error::Result;
use crate::logs;
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::project::Project;
use crate::session::{self, Context};
use crate::state::names;
use crate::workspace::Workspaces;

/// The downloader used by an [`App`].
///
/// A `static` so a `Context` can borrow it for `'static`; `SystemDownloader` is
/// a unit struct with no state to initialise.
static DOWNLOADER: SystemDownloader = SystemDownloader;

/// The state of one managed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Running, and Lambo has the record.
    Running,
    /// Lambo has a record but the process is gone.
    Stopped,
    /// Lambo has never started it.
    NotStarted,
    /// Lambo tried to start it and that attempt failed.
    ///
    /// Distinct from [`Self::NotStarted`], and the distinction is the point: a
    /// service that crashed looks exactly like one that was never started in
    /// the state file, because a dead record is pruned. Without this state a
    /// GUI would offer `Start` on a service that has already failed and show
    /// the user nothing about why.
    Failed,
}

impl ServiceState {
    /// The word a UI shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::NotStarted => "not started",
            Self::Failed => "failed",
        }
    }

    /// Whether this state means something is wrong, as opposed to merely idle.
    pub fn is_problem(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

/// One managed service, as a UI needs to see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceInfo {
    /// Service name, e.g. `apache`, `mariadb`.
    pub name: String,
    /// Its state.
    pub state: ServiceState,
    /// Process id, when running.
    pub pid: Option<u32>,
    /// Port, when the service has one.
    pub port: Option<u16>,
    /// Uptime text, when running.
    pub uptime: Option<String>,
    /// Log file, when known.
    pub log: Option<PathBuf>,
    /// What is actually listening on the port, when it is not this service.
    pub occupant: Option<String>,
}

/// The state of a PHP runtime.
///
/// Distinguishes the cases a UI has to tell apart: a runtime that is missing,
/// one that is installed but broken, and one that runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// Installed and selected.
    Active,
    /// Installed but not selected.
    Installed,
    /// In the catalogue, not installed.
    Available,
    /// In the catalogue for other platforms only.
    Unavailable,
    /// Present but Lambo cannot vouch for its contents.
    Corrupt,
}

/// One PHP runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeInfo {
    /// Version string.
    pub version: String,
    /// Its state.
    pub state: RuntimeState,
    /// Install directory, when installed.
    pub path: Option<PathBuf>,
    /// Where the artifact would come from, for one that is not installed.
    pub source: Option<String>,
    /// The version the binary itself reported, when Lambo started it.
    ///
    /// `None` for a runtime that is not installed, and for one that would not
    /// run - which is the distinction that matters.
    pub reported_version: Option<String>,
    /// Whether the runtime is usable, i.e. it started and reported sanely.
    pub healthy: Option<bool>,
    /// Why it is not healthy, when it is not.
    pub problem: Option<String>,
}

/// A registered project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectInfo {
    /// Project name.
    pub name: String,
    /// Where it lives. Lambo never moves or copies a project.
    pub path: PathBuf,
    /// Detected framework or `plain php`.
    pub framework: String,
    /// Document root, resolved.
    pub document_root: PathBuf,
    /// PHP version the project asks for.
    pub php: String,
    /// Database engine, or `none`.
    pub database: String,
    /// The URL it is served at.
    pub url: String,
    /// Whether that URL currently answers.
    pub serving: bool,
}

/// One diagnostic finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// What was checked.
    pub name: String,
    /// `ok`, `warning`, `error` or `unsupported`.
    pub severity: String,
    /// What was observed.
    pub detail: String,
    /// The command that fixes it, when there is one.
    pub fix: Option<String>,
}

/// Everything the dashboard shows, in one call.
///
/// What the About screen shows.
///
/// The version comes from the build, and the paths from the resolved home - so
/// the GUI reports the same numbers `lambo status` reports instead of a
/// hard-coded string that drifts at the next release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct About {
    /// Product name.
    pub product: String,
    /// Lambo's own version, from the build that produced this binary.
    pub version: String,
    /// Where the application is installed.
    pub home: PathBuf,
    /// Where mutable user data lives - projects, runtimes, database files.
    pub data: PathBuf,
    /// Where configuration is read from.
    pub config: PathBuf,
    /// Where logs are written.
    pub logs: PathBuf,
    /// The licence the product ships under.
    pub license: &'static str,
    /// Where the source and issue tracker live.
    pub repository: &'static str,
}

/// A GUI's main screen should need exactly one round trip. Assembling this from
/// several calls would mean a screen that is half-updated while it loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dashboard {
    /// The open project, when one is open.
    pub project: Option<ProjectInfo>,
    /// Every managed service.
    pub services: Vec<ServiceInfo>,
    /// The active PHP runtime, when there is one.
    pub php: Option<RuntimeInfo>,
    /// The URL to open.
    pub url: Option<String>,
    /// The URL of the database manager, when it is installed.
    pub database_ui_url: Option<String>,
    /// Whether anything is currently failing.
    pub healthy: bool,
}

/// The outcome of an operation that changes something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationOutcome {
    /// Whether it succeeded.
    pub ok: bool,
    /// What happened, per step.
    pub steps: Vec<OperationStep>,
    /// The URL, for an operation that started serving.
    pub url: Option<String>,
    /// The failure, when it failed.
    pub error: Option<String>,
    /// Actionable causes of the failure.
    pub causes: Vec<String>,
    /// The command that fixes it, when known.
    pub hint: Option<String>,
    /// The service the failure was attributed to, when the error named one.
    pub failed_service: Option<String>,
}

impl OperationOutcome {
    /// A successful outcome from a session report.
    fn from_report(report: session::Report) -> Self {
        Self {
            ok: true,
            steps: report.steps.iter().map(OperationStep::from).collect(),
            url: report.url,
            error: None,
            causes: Vec::new(),
            hint: None,
            failed_service: None,
        }
    }

    /// A failed outcome from an error, keeping its causes and hint.
    fn from_error(error: &crate::error::Error) -> Self {
        Self {
            ok: false,
            steps: Vec::new(),
            url: None,
            error: Some(error.to_string()),
            causes: error.details(),
            hint: error.hint(),
            // Only an error that names a service is attributed to one. Guessing
            // from the operation's scope would mark services failed that the
            // error said nothing about.
            failed_service: match error {
                crate::error::Error::ServiceFailed { service, .. } => Some(service.clone()),
                _ => None,
            },
        }
    }
}

/// One step of an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationStep {
    /// Step name.
    pub name: String,
    /// What happened.
    pub detail: String,
    /// `done`, `skipped` or `failed`.
    pub outcome: String,
}

impl From<&session::Step> for OperationStep {
    fn from(step: &session::Step) -> Self {
        use session::StepOutcome;
        // Mapped to words rather than reusing `Step::marker`, which returns the
        // terminal glyphs (`+`, `-`, `x`). A GUI wants a label it can style,
        // not a character chosen for a fixed-width console.
        let outcome = match step.outcome {
            StepOutcome::Done => "done",
            StepOutcome::Skipped => "skipped",
            StepOutcome::Failed => "failed",
        };
        Self {
            name: step.name.clone(),
            detail: step.detail.clone(),
            outcome: outcome.to_owned(),
        }
    }
}

/// A tail of one log file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogView {
    /// Which log.
    pub name: String,
    /// Where it lives.
    pub path: PathBuf,
    /// The lines, oldest first.
    pub lines: Vec<String>,
}

/// The application: one handle a GUI holds open.
///
/// Cheap to construct and free of background threads. Every method does its
/// work when called, so a GUI controls when the engine is touched and can poll
/// on a timer rather than being pushed to.
#[derive(Debug)]
pub struct App {
    /// The Lambo home.
    pub paths: Paths,
    /// Global configuration.
    pub config: Config,
    /// Download catalogue including user overrides.
    pub catalog: Catalog,
    /// Target platform.
    pub platform: Platform,
    /// Operating system model.
    pub os: Os,
    /// Last failure per service, by name.
    ///
    /// The state file cannot carry this: a record for a process that died is
    /// pruned, so by the time a dashboard is built the evidence is gone. The
    /// app handle outlives one poll, which is what makes the evidence
    /// available at all.
    failures: std::collections::HashMap<String, String>,
}

impl App {
    /// Opens the application against the detected Lambo home.
    pub fn open() -> Result<Self> {
        Self::open_at(&Paths::detect()?)
    }

    /// Opens the application against a specific home.
    ///
    /// The split exists so a GUI can run against a home the user chose in
    /// onboarding, and so tests can run against a temporary one.
    pub fn open_at(paths: &Paths) -> Result<Self> {
        Ok(Self {
            config: Config::load(paths)?,
            catalog: Catalog::load(paths)?,
            paths: paths.clone(),
            platform: Platform::host(),
            os: Os::host(),
            failures: std::collections::HashMap::new(),
        })
    }

    /// The service name a project's web server is recorded under.
    fn web_service(&self, project: &Project) -> &'static str {
        match project.server_kind(&self.config) {
            crate::config::ServerKind::Php => names::PHP_SERVER,
            // Apache and nginx both run as the Apache-recorded service; nginx
            // is still planned, so it has no name of its own yet.
            _ => names::APACHE,
        }
    }

    /// The names a managed service can have.
    fn is_service_name(name: &str) -> bool {
        [
            names::APACHE,
            names::PHP_SERVER,
            names::DATABASE,
            names::DBUI,
        ]
        .contains(&name)
    }

    /// Records a failed attempt, and clears a previous one on success.
    ///
    /// Attribution comes from evidence that names a service - the error
    /// itself, or the steps that reported failure - and failing that from an
    /// operation that only ever targeted one service, which is not a guess.
    /// An operation spanning several services that names none of them is left
    /// unattributed rather than blaming all of them.
    fn note_failure(&mut self, outcome: &OperationOutcome, targets: &[&str]) {
        if outcome.ok {
            // A service that came up is no longer failed, whatever happened
            // last time.
            self.failures.clear();
            return;
        }
        let message = outcome
            .error
            .clone()
            .unwrap_or_else(|| "the operation failed".to_owned());
        // Attribution, in order of how directly the evidence names a service:
        // the error itself, then the steps that reported failure. An operation
        // that fails without naming anything is left unattributed - marking
        // every service it touched as failed would be a guess dressed up as a
        // diagnosis, and the notice line already carries the message.
        let named: Vec<String> = outcome
            .steps
            .iter()
            .filter(|step| step.outcome == "failed")
            .map(|step| step.name.clone())
            .collect();
        let blamed: Vec<String> = match &outcome.failed_service {
            // Only if it actually names a managed service. Errors also carry
            // things like "PHP stable" - a runtime description, not a service
            // - and recording a failure under a name no service row can match
            // would silently swallow it: nothing would show as Failed even
            // though something genuinely went wrong.
            Some(service) if Self::is_service_name(service) => vec![service.clone()],
            None if !named.is_empty() => named,
            // One target, so "this operation failed" is also "this service
            // failed" - no inference required.
            None if targets.len() == 1 => vec![targets[0].to_owned()],
            // Either the error named nothing, or it named something that is not
            // a service. Neither is a basis for blaming a service, and the
            // notice already carries the message.
            _ => Vec::new(),
        };
        for name in blamed {
            self.failures.insert(name, message.clone());
        }
    }

    /// Why a service last failed, when it did.
    pub fn failure(&self, service: &str) -> Option<&str> {
        self.failures.get(service).map(String::as_str)
    }

    /// A context for the session functions.
    fn context(&self) -> Context<'static> {
        Context {
            paths: self.paths.clone(),
            config: self.config.clone(),
            catalog: self.catalog.clone(),
            platform: self.platform,
            downloader: &DOWNLOADER,
            os: self.os,
        }
    }

    // -----------------------------------------------------------------------
    // Dashboard
    // -----------------------------------------------------------------------

    /// Everything the dashboard shows, in one call.
    pub fn dashboard(&self, project: Option<&Project>) -> Result<Dashboard> {
        let mut context = self.context();
        let status = session::status(project, &mut context)?;

        let services = status
            .services
            .iter()
            .map(|service| ServiceInfo {
                name: service.name.clone(),
                state: match (service.recorded, service.running) {
                    (true, true) => ServiceState::Running,
                    (true, false) => ServiceState::Stopped,
                    // A service that is not recorded reads as "not started" -
                    // unless this handle saw an attempt on it fail. The state
                    // file cannot carry that distinction, so without this the
                    // GUI would offer Start on a service that has just failed
                    // and show the user nothing about why.
                    (false, _) if self.failures.contains_key(&service.name) => ServiceState::Failed,
                    (false, _) => ServiceState::NotStarted,
                },
                pid: service.pid.filter(|_| service.running),
                port: service.port,
                uptime: service.uptime.clone(),
                log: service.log.clone(),
                occupant: service.occupant.clone(),
            })
            .collect::<Vec<_>>();

        let info = match project {
            Some(project) => {
                Some(self.project_info(project, status.url.as_deref(), status.serving))
            }
            None => None,
        };

        let php = self.active_runtime()?;

        // Healthy means nothing is failing. A service that was never started is
        // not a failure - a fresh install has none of them running - so only a
        // state that is actually a problem counts against it.
        let healthy = !services.iter().any(|service| service.state.is_problem());

        let database_ui_url = crate::dbui::is_installed(&self.paths)
            .then(|| crate::dbui::url(status.url.as_deref().and_then(port_of).unwrap_or(80)));

        Ok(Dashboard {
            url: status.url,
            project: info,
            services,
            php,
            database_ui_url,
            healthy,
        })
    }

    // -----------------------------------------------------------------------
    // Projects
    // -----------------------------------------------------------------------

    /// Every registered project.
    pub fn projects(&self) -> Result<Vec<ProjectInfo>> {
        let registry = Workspaces::load(&self.paths)?;
        let mut projects = Vec::new();
        for (_workspace, paths) in registry.iter() {
            for path in paths {
                // A project that no longer parses is skipped rather than
                // failing the whole list: one moved folder must not blank the
                // GUI's project picker.
                if let Ok(project) = Project::load(path) {
                    let url = session::effective_url(&self.paths, &project, &self.config, self.os);
                    let serving = crate::http::is_up(&url);
                    projects.push(self.project_info(&project, Some(&url), serving));
                }
            }
        }
        Ok(projects)
    }

    /// Adds a project directory to the default workspace.
    ///
    /// Returns whether it was newly added; registering the same directory twice
    /// is not an error.
    pub fn add_project(&self, path: &Path) -> Result<bool> {
        let mut registry = Workspaces::load(&self.paths)?;
        let added = registry.add("default", path);
        registry.save(&self.paths)?;
        Ok(added)
    }

    /// Removes a project from the default workspace.
    ///
    /// Never touches the directory: a project is a reference to a folder the
    /// user owns, not something Lambo holds a copy of.
    pub fn remove_project(&self, path: &Path) -> Result<bool> {
        let mut registry = Workspaces::load(&self.paths)?;
        let removed = registry.remove("default", path)?;
        registry.save(&self.paths)?;
        Ok(removed)
    }

    /// Describes one project.
    fn project_info(&self, project: &Project, url: Option<&str>, serving: bool) -> ProjectInfo {
        ProjectInfo {
            name: project.name(),
            path: project.document_root().clone(),
            framework: project.detection.framework.display_name().to_owned(),
            document_root: project.document_root(),
            php: project.php_spec(&self.config).to_string(),
            database: project
                .database_kind(&self.config)
                .display_name()
                .to_owned(),
            url: url.unwrap_or("").to_owned(),
            serving,
        }
    }

    // -----------------------------------------------------------------------
    // Runtimes
    // -----------------------------------------------------------------------

    /// Every PHP version, installed and available.
    pub fn runtimes(&self) -> Result<Vec<RuntimeInfo>> {
        let rows = crate::php::version_table(
            &self.paths,
            &self.catalog,
            self.platform,
            &self.config.sources,
        )?;
        let active = self.active_runtime()?;

        Ok(rows
            .into_iter()
            .map(|row| {
                // Only the active runtime is started to check it. Probing every
                // installed version would spawn a process per row on every
                // dashboard refresh.
                let is_active = active
                    .as_ref()
                    .is_some_and(|runtime| runtime.version == row.version);
                let checked = is_active.then(|| active.as_ref().expect("checked above"));
                RuntimeInfo {
                    reported_version: checked.and_then(|runtime| {
                        runtime.reported_version.as_ref().map(ToString::to_string)
                    }),
                    healthy: checked.map(|runtime| runtime.healthy.unwrap_or(false)),
                    problem: checked.and_then(|runtime| runtime.problem.clone()),
                    state: match row.status {
                        crate::php::VersionStatus::Active => RuntimeState::Active,
                        crate::php::VersionStatus::Installed => RuntimeState::Installed,
                        crate::php::VersionStatus::Available => RuntimeState::Available,
                        crate::php::VersionStatus::Unavailable => RuntimeState::Unavailable,
                        crate::php::VersionStatus::Corrupt => RuntimeState::Corrupt,
                    },
                    version: row.version,
                    path: row.path,
                    source: row.source,
                }
            })
            .collect())
    }

    /// The active runtime, with the evidence that it runs.
    ///
    /// `None` when no version is selected - which is not an error, it is the
    /// state of a fresh install.
    pub fn active_runtime(&self) -> Result<Option<RuntimeInfo>> {
        let Some(runtime) = crate::php::current(&self.paths)? else {
            return Ok(None);
        };
        let health = crate::php::RuntimeHealth::check(&self.paths, &runtime, self.os);
        Ok(Some(RuntimeInfo {
            reported_version: health.reported_version.as_ref().map(ToString::to_string),
            healthy: Some(health.is_healthy()),
            problem: health.problem.clone(),
            version: runtime.name.clone(),
            state: RuntimeState::Active,
            path: Some(runtime.path.clone()),
            source: None,
        }))
    }

    // -----------------------------------------------------------------------
    // Services
    // -----------------------------------------------------------------------

    /// Starts everything a project needs.
    pub fn start_all(&mut self, project: &Project, open_browser: bool) -> OperationOutcome {
        let mut context = self.context();
        let outcome = match session::up(project, &mut context, open_browser) {
            Ok(report) => {
                // `up` may have generated database credentials; keep them so a
                // second call in the same session does not regenerate.
                self.config = context.config;
                OperationOutcome::from_report(report)
            }
            Err(error) => OperationOutcome::from_error(&error),
        };
        self.note_failure(
            &outcome,
            &[names::APACHE, names::PHP_SERVER, names::DATABASE],
        );
        outcome
    }

    /// Stops every service Lambo started, and nothing else.
    pub fn stop_all(&mut self) -> OperationOutcome {
        let mut context = self.context();
        let outcome = match session::down(&mut context) {
            Ok(report) => OperationOutcome::from_report(report),
            Err(error) => OperationOutcome::from_error(&error),
        };
        self.note_failure(
            &outcome,
            &[
                names::APACHE,
                names::PHP_SERVER,
                names::DATABASE,
                names::DBUI,
            ],
        );
        outcome
    }

    /// Starts just the web server.
    pub fn server_start(&mut self, project: &Project) -> OperationOutcome {
        let target = self.web_service(project);
        let mut context = self.context();
        let outcome = match session::start_server(project, &mut context) {
            Ok(started) => OperationOutcome {
                ok: true,
                // The port change is a step of its own: a UI that hides it
                // would show a URL that disagrees with the configuration.
                steps: started
                    .note
                    .iter()
                    .map(|note| OperationStep {
                        name: "ports".to_owned(),
                        detail: note.clone(),
                        outcome: "done".to_owned(),
                    })
                    .chain([OperationStep {
                        name: "server".to_owned(),
                        detail: started.message.clone(),
                        outcome: "done".to_owned(),
                    }])
                    .collect(),
                url: Some(started.url()),
                error: None,
                causes: Vec::new(),
                hint: None,
                failed_service: None,
            },
            Err(error) => OperationOutcome::from_error(&error),
        };
        self.note_failure(&outcome, &[target]);
        outcome
    }

    /// Stops just the web server.
    pub fn server_stop(&mut self) -> OperationOutcome {
        let mut context = self.context();
        let outcome = match session::stop_server(&mut context) {
            Ok(message) => OperationOutcome {
                ok: true,
                steps: vec![OperationStep {
                    name: "server".to_owned(),
                    detail: message,
                    outcome: "done".to_owned(),
                }],
                url: None,
                error: None,
                causes: Vec::new(),
                hint: None,
                failed_service: None,
            },
            Err(error) => OperationOutcome::from_error(&error),
        };
        // Whichever flavour is recorded, and there is no project here to say
        // which. Both are passed so nothing is attributed on a guess; an error
        // that names the service still attributes precisely.
        self.note_failure(&outcome, &[names::APACHE, names::PHP_SERVER]);
        outcome
    }

    /// Starts just the database server.
    pub fn database_start(&mut self, project: &Project) -> OperationOutcome {
        let kind = project.database_kind(&self.config);
        let port = project.file.database_port(&self.config);
        let mut context = self.context();
        let outcome = match session::start_database(&mut context, Some(project), kind, port) {
            Ok(message) => OperationOutcome {
                ok: true,
                steps: vec![OperationStep {
                    name: "database".to_owned(),
                    detail: message,
                    outcome: "done".to_owned(),
                }],
                url: None,
                error: None,
                causes: Vec::new(),
                hint: None,
                failed_service: None,
            },
            Err(error) => OperationOutcome::from_error(&error),
        };
        self.note_failure(&outcome, &[names::DATABASE]);
        outcome
    }

    /// Stops just the database server.
    pub fn database_stop(&mut self) -> OperationOutcome {
        let mut context = self.context();
        let outcome = match session::stop_database(&mut context) {
            Ok(message) => OperationOutcome {
                ok: true,
                steps: vec![OperationStep {
                    name: "database".to_owned(),
                    detail: message,
                    outcome: "done".to_owned(),
                }],
                url: None,
                error: None,
                causes: Vec::new(),
                hint: None,
                failed_service: None,
            },
            Err(error) => OperationOutcome::from_error(&error),
        };
        self.note_failure(&outcome, &[names::DATABASE]);
        outcome
    }

    /// Opens the database manager in a browser and returns its URL.
    pub fn open_database_ui(&mut self, project: Option<&Project>) -> Result<String> {
        let name = project.map(Project::database_name);
        let mut context = self.context();
        session::open_database_ui(&mut context, name.as_deref(), true)
    }

    // -----------------------------------------------------------------------
    // Diagnostics and logs
    // -----------------------------------------------------------------------

    /// Every diagnostic finding.
    pub fn doctor(&self, project: Option<&Project>) -> Vec<Diagnostic> {
        let report = doctor::run(
            &self.paths,
            &self.config,
            project,
            &self.catalog,
            self.platform,
            self.os,
        );
        report
            .checks
            .iter()
            .map(|check| Diagnostic {
                name: check.name.to_owned(),
                // Words, not `Severity::marker`'s terminal glyphs (`ok`, `--`,
                // `!!`, `xx`). A GUI styles by name; it should not have to know
                // what character a fixed-width console prints.
                severity: match check.severity {
                    doctor::Severity::Ok => "ok",
                    doctor::Severity::Unsupported => "unsupported",
                    doctor::Severity::Warn => "warning",
                    doctor::Severity::Fail => "error",
                }
                .to_owned(),
                detail: check.detail.clone(),
                fix: check.fix.clone(),
            })
            .collect()
    }

    /// The logs a UI can offer, with a tail of each.
    pub fn logs(&self, max_lines: usize) -> Result<Vec<LogView>> {
        let mut views = Vec::new();
        for (name, path) in [
            ("apache", logs::apache(&self.paths)),
            ("database", logs::database(&self.paths)),
            ("php", logs::php(&self.paths)),
            ("lambo", logs::lambo(&self.paths)),
        ] {
            // A log that does not exist yet is not an error; a fresh install
            // has none. Reporting an empty view beats failing the panel.
            let lines = logs::tail(&path, max_lines).unwrap_or_default();
            views.push(LogView {
                name: name.to_owned(),
                path,
                lines,
            });
        }
        Ok(views)
    }

    // -----------------------------------------------------------------------
    // Configuration
    // -----------------------------------------------------------------------

    /// Saves the current configuration.
    pub fn save_config(&mut self) -> Result<()> {
        self.config.save(&self.paths)
    }

    /// The URL of the project as it would be served.
    /// Everything the About screen needs, in one call.
    pub fn about(&self) -> About {
        About {
            product: "Lambo PHP".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            home: self.paths.root().to_path_buf(),
            data: self.paths.data_dir(),
            config: self.paths.config_dir(),
            logs: self.paths.logs_dir(),
            // Dual-licensed, as Cargo.toml declares and as the two licence
            // files shipped in every package say. Stated here rather than
            // typed into the GUI, so it cannot drift from the manifest.
            license: "Apache-2.0 OR MIT",
            repository: env!("CARGO_PKG_REPOSITORY"),
        }
    }

    pub fn project_url(&self, project: &Project) -> String {
        session::effective_url(&self.paths, project, &self.config, self.os)
    }
}

/// Extracts the port from a local URL.
///
/// `http://localhost` has no port component, which is the normal case now that
/// 80 is the default.
fn port_of(url: &str) -> Option<u16> {
    let authority = url.split("://").nth(1)?;
    let port = authority.rsplit(':').next()?;
    // A bare host has no colon, so `rsplit` returns the host itself; parsing it
    // as a number fails and yields `None`, which is the right answer.
    port.parse().ok()
}
