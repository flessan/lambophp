//! The installation's services, as one stack.
//!
//! `main.go` gives every configured service a `ManagedService` - its
//! configuration plus, when it has an executable, the [`Service`] that
//! supervises it - and everything an interface then does to the stack is a walk
//! over that list: a card's Start button, `Start Stack`, `Stop All`, the
//! `settings.auto_start` pass, and the startup sweep.
//!
//! The decisions live here rather than in either interface, which is the point
//! of the module: `lambo up` and a click on a card must take the same branch,
//! including the surprising ones. Starting a service the configuration marks
//! *disabled* is allowed - the original consulted `Enabled` only when it was
//! booting the whole stack - a component that is missing is installed before it
//! is started, and a component with no executable and no URL does nothing at
//! all.
//!
//! Two passes over the essentials are preserved separately, because they do not
//! behave the same way: [`Stack::start_essential`] is the tray's Start Stack,
//! which walks the fixed list and starts whatever it finds, and
//! [`Stack::ensure_essentials`] is the page's, which uses the active web server,
//! installs a missing component *before* it asks whether the service is enabled,
//! and pauses between starts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::catalog_panel;
use crate::error::{Error, Result};
use crate::logs::LogFn;
use crate::panel::{ESSENTIAL_SERVICES, PanelConfig, ServiceConf, expand_path};
use crate::service::{Service, ServiceConfig, ServiceHost, StateCallback};
use crate::zombies;

/// The page the original opened once the essential stack was up.
pub const LOCAL_URL: &str = "http://localhost/";

/// How long the original waited after starting each essential service.
pub const ESSENTIAL_PAUSE: Duration = Duration::from_millis(400);

/// How long it waited before opening the page.
pub const OPEN_PAUSE: Duration = Duration::from_millis(800);

/// How long the toolbar's Restart waits between stopping everything and starting
/// it again: long enough for the ports to come free.
pub const RESTART_PAUSE: Duration = Duration::from_secs(1);

/// How long a single service's Restart waits between stopping and starting:
/// the original's card handler slept 500 milliseconds.
pub const SERVICE_RESTART_PAUSE: Duration = Duration::from_millis(500);

/// What the stack needs from the installer when a component is missing.
///
/// The original called the catalogue's install functions directly. The seam
/// exists so the decisions below are testable without a network, and so the
/// install a start triggers reports progress exactly like any other install: the
/// app layer implements this over its [`crate::installer::Installer`].
pub trait ComponentInstall {
    /// Installs a component with the catalogue's own version.
    fn install(&mut self, name: &str) -> Result<()>;

    /// Installs one version of a component.
    fn install_version(&mut self, name: &str, version: &str) -> Result<()>;
}

/// What starting a service means, before anything is touched.
///
/// This is the original's branch table, in its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartPlan {
    /// The service has an executable and is installed: start it.
    Start,
    /// The component is missing: install this version, then start.
    InstallThenStart {
        /// The component's name.
        name: String,
        /// Its configured active version.
        version: String,
    },
    /// A service with no executable whose component is missing: install it, and
    /// leave it at that - there is nothing to start.
    Install(String),
    /// A service with no executable and a URL: open it.
    OpenUrl(String),
    /// A service with no executable, nothing to install and no URL.
    Nothing,
    /// The configured executable is not on disk.
    ExeMissing(PathBuf),
}

/// What a start did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// The service was started.
    Started,
    /// The component was installed, and then the service was started.
    InstalledThenStarted,
    /// The component was installed; the service is a tool with nothing to run.
    Installed,
    /// The service has no executable: this URL is what to open.
    Opened(String),
    /// Nothing was done, and nothing needed doing.
    Nothing,
    /// The executable named in the configuration is not on disk.
    ExeMissing(PathBuf),
    /// The install or the start failed, and the reason has been logged.
    Failed(String),
}

/// One step of the essential-stack passes, in the order it happened.
///
/// The original reported these through its log; returning them as well is what
/// lets an interface show the same order the log has, and lets the tests assert
/// it without reading the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EssentialStep {
    /// The component was installed and the service started.
    InstalledThenStarted(String),
    /// The component was installed before the service was started.
    Installed(String),
    /// The install failed, and the pass moved on.
    InstallFailed(String),
    /// The service was started.
    Started(String),
    /// The service is configured but disabled, or has no executable.
    Skipped(String),
    /// The service has no executable: its URL is what to open.
    Opened(String, String),
    /// Nothing needed doing.
    Nothing(String),
    /// The executable is not on disk.
    ExeMissing(String, PathBuf),
    /// The start failed, and the reason has been logged.
    Failed(String, String),
}

/// One step of the panel's `Start Stack`.
///
/// Both halves matter to the window: the kind decides the line the log shows,
/// and `Started` carries the engine's own [`StartOutcome`] so the URL a
/// component without an executable names can still be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartStackStep {
    /// The component was installed.
    Installed(String),
    /// The install failed, and the pass moved on.
    InstallFailed(String),
    /// The service is disabled, or has no executable.
    Skipped(String),
    /// The service was started, or resolved to something else to do.
    Started(String, StartOutcome),
}

/// What an essential-stack pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EssentialRun {
    /// What happened, in order.
    pub steps: Vec<EssentialStep>,
    /// The page the original opened when the pass was over.
    pub open_url: String,
}

/// One service of the installation: its configuration, and its engine when it
/// has one.
pub struct ManagedService {
    conf: ServiceConf,
    base_dir: PathBuf,
    service: Option<Arc<Service>>,
}

impl ManagedService {
    /// The configuration the dashboard's card is drawn from.
    pub fn conf(&self) -> &ServiceConf {
        &self.conf
    }

    /// The service's name, which is also its configuration key.
    pub fn name(&self) -> &str {
        &self.conf.name
    }

    /// The engine that supervises it, when it is a process at all.
    ///
    /// A tool like phpMyAdmin has no executable and therefore no engine: the
    /// original built a `Service` only when `ExePath` was non-empty, and the
    /// card's behaviour differs accordingly.
    pub fn service(&self) -> Option<&Arc<Service>> {
        self.service.as_ref()
    }

    /// Whether it is running right now.
    pub fn running(&self) -> bool {
        self.service
            .as_ref()
            .is_some_and(|service| service.running())
    }

    /// Its process id, when it has one.
    pub fn pid(&self) -> Option<u32> {
        self.service.as_ref().and_then(|service| service.pid())
    }

    /// The process id of the process this engine is holding, when it holds one.
    ///
    /// The narrow answer, for callers that must not act on a process they did
    /// not start - the start-up sweep, which keeps exactly the services this
    /// engine runs and kills everything else of this installation's under
    /// `bin/`. A leftover of the same program is what a sweep is for, and
    /// [`pid`](Self::pid) would report it as running.
    pub fn held_pid(&self) -> Option<u32> {
        self.service.as_ref().and_then(|service| service.held_pid())
    }

    /// The process id of this service's own process while it holds the port.
    ///
    /// The strict answer to "is this service serving right now": a port that
    /// something else occupies - on Windows the System process often holds 80
    /// through HTTP.sys - is not evidence of this service, and neither is a
    /// copy of the program that bound a different port.
    pub fn pid_holding_port(&self) -> Option<u32> {
        self.service
            .as_ref()
            .and_then(|service| service.pid_holding_port())
    }

    /// Whether the catalogue says its component is installed.
    ///
    /// A name the catalogue does not know reports `true`, which is
    /// [`catalog_panel::is_installed`]'s own rule: a component that cannot be
    /// installed must not be offered for installation.
    pub fn installed(&self) -> bool {
        catalog_panel::is_installed(&self.conf.name, &self.base_dir)
    }

    /// Whether it is a web server, which the dashboard groups separately.
    ///
    /// This is `groupForKind(kind) == groupWeb`, i.e. the kind `web` and nothing
    /// else; the other kinds only decide which group a card is drawn in.
    pub fn is_web_kind(&self) -> bool {
        self.conf.kind.eq_ignore_ascii_case("web")
    }

    /// The card's status line, exactly as the original composed it.
    pub fn status(&self, active_web_server: &str) -> String {
        let mut status = if self.conf.enabled {
            "Stopped"
        } else {
            "Disabled"
        }
        .to_owned();

        if self.service.is_none() {
            status = if self.installed() {
                "Installed (tool)"
            } else {
                "Not installed"
            }
            .to_owned();
        } else if self.running() {
            status = format!("Running  pid {}", self.pid().unwrap_or(0));
        }

        if self.conf.port > 0 {
            status.push_str(&format!("  :{}", self.conf.port));
        }

        // A web server that is not the active one is not merely stopped: the
        // card says which one to pick instead.
        if self.is_web_kind() && self.conf.name != active_web_server {
            status = format!("Inactive — pick {} up top", self.conf.name);
        }

        status
    }

    /// The card's status dot.
    pub fn dot(&self, active_web_server: &str) -> char {
        // Both "not this web server" and "not installed, with something that
        // should be" are the muted dot, so they are one condition.
        if (self.is_web_kind() && self.conf.name != active_web_server)
            || !(self.installed() || self.service.is_none())
        {
            '·'
        } else if self.running() {
            '●'
        } else {
            '○'
        }
    }

    /// Reports this service's state to `callback` as it changes.
    pub fn set_state_callback(&self, callback: StateCallback) {
        if let Some(service) = &self.service {
            service.set_state_callback(callback);
        }
    }
}

/// Every service of an installation, in the configuration's order.
pub struct Stack {
    base_dir: PathBuf,
    host: Arc<dyn ServiceHost>,
    services: Vec<ManagedService>,
    log: LogFn,
}

impl Stack {
    /// Builds the stack a configuration describes.
    ///
    /// One [`ManagedService`] per configured service, always; an engine only
    /// when the entry has an executable. This is `main.go`'s construction loop,
    /// including the detail that a service with an empty `exe` is still a card.
    pub fn build(
        base_dir: &Path,
        config: &PanelConfig,
        host: Arc<dyn ServiceHost>,
        log: LogFn,
    ) -> Self {
        let mut stack = Self {
            base_dir: base_dir.to_path_buf(),
            host,
            services: Vec::new(),
            log,
        };
        stack.reload(config);
        stack
    }

    /// Rebuilds the stack from a configuration that has just been reloaded.
    ///
    /// A service that is still configured keeps the engine it already had - by
    /// handle, so a running one stays running and keeps its state callback -
    /// and anything else is built afresh. That is the original's `reloadConfig`,
    /// and it matters after an install has changed what exists on disk.
    pub fn reload(&mut self, config: &PanelConfig) {
        let previous = std::mem::take(&mut self.services);
        for conf in &config.services {
            let engine = previous
                .iter()
                .find(|old| old.conf.name == conf.name)
                .and_then(|old| old.service.clone());
            self.services.push(self.build_one(conf.clone(), engine));
        }
    }

    /// The installation directory every path in the configuration is relative
    /// to.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Every service, in the configuration's order.
    pub fn services(&self) -> &[ManagedService] {
        &self.services
    }

    /// The service with this name, if the configuration has one.
    pub fn find(&self, name: &str) -> Option<&ManagedService> {
        self.services.iter().find(|service| service.name() == name)
    }

    /// Where that service sits in the configuration's order.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.services
            .iter()
            .position(|service| service.name() == name)
    }

    /// What starting a service means.
    pub fn plan_start(&self, name: &str) -> Option<StartPlan> {
        let managed = self.find(name)?;
        let catalogued = catalog_panel::find(name).is_some();
        let installed = catalog_panel::is_installed(name, &self.base_dir);

        let Some(service) = managed.service() else {
            // No executable: either the component is missing and is installed in
            // the background, or the card is a link, or there is nothing to do.
            if catalogued && !installed {
                return Some(StartPlan::Install(name.to_owned()));
            }
            if !managed.conf.open_url.is_empty() {
                return Some(StartPlan::OpenUrl(managed.conf.open_url.clone()));
            }
            return Some(StartPlan::Nothing);
        };

        if catalogued && !installed {
            return Some(StartPlan::InstallThenStart {
                name: name.to_owned(),
                version: managed.conf.active_version.clone(),
            });
        }

        // Only an absolute path is checked: the original looked for the
        // executable on disk, and a bare name is left to the operating system's
        // search.
        let exe = &service.config().exe_path;
        if exe.is_absolute() && !exe.exists() {
            return Some(StartPlan::ExeMissing(exe.clone()));
        }

        Some(StartPlan::Start)
    }

    /// Starts a service the way the dashboard's Start button did, installing the
    /// component first when it is missing.
    ///
    /// An unknown name is an error rather than the original's silent return: a
    /// card can only name a configured service, and a caller asking for anything
    /// else has made a mistake worth reporting.
    pub fn start_with_install(
        &self,
        name: &str,
        installer: &mut dyn ComponentInstall,
    ) -> Result<StartOutcome> {
        let plan = self.plan_start(name).ok_or_else(|| self.unknown(name))?;

        match plan {
            StartPlan::Nothing => Ok(StartOutcome::Nothing),
            StartPlan::OpenUrl(url) => Ok(StartOutcome::Opened(url)),
            StartPlan::ExeMissing(path) => {
                self.note(name, &format!("exe missing: {}", path.display()));
                Ok(StartOutcome::ExeMissing(path))
            }
            StartPlan::Install(name) => match installer.install(&name) {
                Ok(()) => Ok(StartOutcome::Installed),
                Err(error) => {
                    self.note(&name, &format!("install failed: {error}"));
                    Ok(StartOutcome::Failed(error.to_string()))
                }
            },
            StartPlan::InstallThenStart { name, version } => {
                self.note(&name, "install incomplete — auto-reinstalling...");
                match installer.install_version(&name, &version) {
                    Ok(()) => Ok(match self.start_engine(&name) {
                        StartOutcome::Started => StartOutcome::InstalledThenStarted,
                        other => other,
                    }),
                    Err(error) => {
                        self.note(&name, &format!("install failed: {error}"));
                        Ok(StartOutcome::Failed(error.to_string()))
                    }
                }
            }
            StartPlan::Start => Ok(self.start_engine(name)),
        }
    }

    /// Stops a service, if it has an engine to stop.
    pub fn stop(&self, name: &str) -> Result<()> {
        let managed = self.find(name).ok_or_else(|| self.unknown(name))?;
        match managed.service() {
            Some(service) => service.stop(),
            None => Ok(()),
        }
    }

    /// Stops every service, in the configuration's order.
    ///
    /// Failures are ignored, as the original's `Stop All` ignored them: one
    /// service that will not stop must not leave the rest running.
    pub fn stop_all(&self) {
        for managed in &self.services {
            if let Some(service) = managed.service() {
                let _ = service.stop();
            }
        }
    }

    /// The tray's `Start Stack`: the fixed essential list, in order.
    ///
    /// It does not consult the configuration's `Enabled` flags and does not
    /// pause, which is exactly what the original's tray command did - the page's
    /// version, [`Stack::ensure_essentials`], is the one that does both.
    pub fn start_essential(&self, installer: &mut dyn ComponentInstall) -> Vec<EssentialStep> {
        let mut steps = Vec::new();
        for name in ESSENTIAL_SERVICES {
            if self.find(name).is_none() {
                continue;
            }
            match self.start_with_install(name, installer) {
                Ok(outcome) => steps.push(step_of(outcome, name)),
                Err(_) => continue,
            }
        }
        steps
    }

    /// The page's `Start Stack`, which boots the stack and returns the page to
    /// open.
    ///
    /// The order of the original's checks is the behaviour: a missing component
    /// is installed *before* the service's `Enabled` flag is consulted, so a
    /// disabled service whose component is missing is still installed. An
    /// install that fails ends that service's turn - after the failure is
    /// logged - and the pass carries on with the next one.
    pub fn ensure_essentials(
        &self,
        config: &PanelConfig,
        installer: &mut dyn ComponentInstall,
        sleep: &dyn Fn(Duration),
    ) -> EssentialRun {
        let names = config.essential_services();
        self.ensure_essentials_for(&names, installer, sleep)
    }

    /// The same pass, over an explicit list of services.
    ///
    /// `lambo up` uses it to leave the database out of a project that has none
    /// (see [`PanelConfig::essential_services_for`]); the order, the install
    /// first/skip-then/start sequence and the pauses are exactly the page's.
    pub fn ensure_essentials_for(
        &self,
        names: &[String],
        installer: &mut dyn ComponentInstall,
        sleep: &dyn Fn(Duration),
    ) -> EssentialRun {
        let mut steps = Vec::new();

        for name in names {
            let name = name.clone();
            let Some(managed) = self.find(&name) else {
                continue;
            };

            if catalog_panel::find(&name).is_some()
                && !catalog_panel::is_installed(&name, &self.base_dir)
            {
                match installer.install_version(&name, &managed.conf.active_version) {
                    Ok(()) => steps.push(EssentialStep::Installed(name.clone())),
                    Err(error) => {
                        self.note(&name, &format!("install failed: {error}"));
                        steps.push(EssentialStep::InstallFailed(name.clone()));
                        continue;
                    }
                }
            }

            if managed.service().is_none() || !managed.conf.enabled {
                steps.push(EssentialStep::Skipped(name));
                continue;
            }

            match self.start_with_install(&name, installer) {
                Ok(outcome) => steps.push(step_of(outcome, &name)),
                Err(_) => continue,
            }
            sleep(ESSENTIAL_PAUSE);
        }

        sleep(OPEN_PAUSE);
        EssentialRun {
            steps,
            open_url: LOCAL_URL.to_owned(),
        }
    }

    /// The panel's `Start Stack`, in the original's own phases.
    ///
    /// [`Stack::ensure_essentials`] runs the same pass, but through the card's
    /// decision table: it consults `plan_start`, which resolves a service that
    /// has no executable to a URL to open, a component to install, or nothing at
    /// all. The page did not. It installed a missing component, *then* skipped a
    /// service that was disabled or had no executable, then started the rest -
    /// and a component whose install failed ended that service's turn without a
    /// start being attempted.
    ///
    /// The difference is observable: for a component with nothing to run, the
    /// page installs it and moves on, where the other pass reports the URL it
    /// would have opened. Both are ported, because both existed.
    pub fn ensure_stack_essentials(
        &self,
        config: &PanelConfig,
        installer: &mut dyn ComponentInstall,
        sleep: &dyn Fn(Duration),
    ) -> Vec<StartStackStep> {
        let mut steps = Vec::new();

        for name in config.essential_services() {
            let Some(managed) = self.find(&name) else {
                continue;
            };

            if catalog_panel::find(&name).is_some()
                && !catalog_panel::is_installed(&name, &self.base_dir)
            {
                match installer.install_version(&name, &managed.conf.active_version) {
                    Ok(()) => steps.push(StartStackStep::Installed(name.clone())),
                    Err(error) => {
                        self.note(&name, &format!("install failed: {error}"));
                        steps.push(StartStackStep::InstallFailed(name.clone()));
                        continue;
                    }
                }
            }

            if managed.service().is_none() || !managed.conf.enabled {
                steps.push(StartStackStep::Skipped(name));
                continue;
            }

            match self.start_with_install(&name, installer) {
                Ok(outcome) => steps.push(StartStackStep::Started(name.clone(), outcome)),
                Err(_) => continue,
            }
            sleep(ESSENTIAL_PAUSE);
        }

        // The page waits again and opens the site; the waiting is passed in so a
        // test does not have to.
        sleep(OPEN_PAUSE);
        steps
    }

    /// Starts the services `settings.auto_start` names, in the order it names
    /// them.
    ///
    /// This is the window-creation pass, so it is deliberately *not* the card's
    /// decision table: the original called `Service.Start` directly, which means
    /// a tool with no executable is skipped, a disabled service is started all
    /// the same, and nothing is installed. A failure is logged as
    /// `[auto-start] {name}: {err}` - the one line this pass adds - and the rest
    /// still start.
    pub fn auto_start(&self, names: &[String]) -> Vec<String> {
        let mut lines = Vec::new();
        for name in names {
            let Some(service) = self.find(name).and_then(|managed| managed.service()) else {
                continue;
            };
            if let Err(error) = service.start() {
                let line = format!("[auto-start] {name}: {error}");
                (self.log)(&line);
                lines.push(line);
            }
        }
        lines
    }

    /// Reports every service's state to one callback.
    pub fn set_state_callback(&self, callback: StateCallback) {
        for managed in &self.services {
            managed.set_state_callback(Arc::clone(&callback));
        }
    }

    /// The startup sweep: kills what a previous run left under the installation
    /// and logs what it killed.
    ///
    /// The services this stack is running right now are kept, whatever they are:
    /// the original swept the whole `bin/` tree with no exceptions, which would
    /// have killed a running stack the moment a second copy of the window was
    /// opened.
    pub fn sweep(&self) -> Vec<String> {
        // What this engine is running, not what a service looks like from the
        // outside: a process of the same program that nobody holds is a leftover,
        // and the sweep exists to kill it.
        let keep: Vec<u32> = self
            .services
            .iter()
            .filter_map(|service| service.held_pid())
            .collect();
        let killed = zombies::sweep(&*self.host, &self.base_dir, &keep);
        for line in &killed {
            (self.log)(line);
        }
        killed
    }

    /// The services toolbar's `Restart`: everything down, the leftovers swept,
    /// and the essentials up again.
    ///
    /// The timeline is the original's, message for message: the three lines of
    /// narration, `Stop All`, the sweep of whatever a previous run left behind,
    /// a one-second pause, and then the page's own essential pass - with the
    /// active web server in place of Apache, and its pauses between starts. The
    /// sleep is the caller's, as in [`Stack::ensure_essentials`], so a test does
    /// not have to wait for it.
    pub fn restart_essentials(
        &self,
        config: &PanelConfig,
        installer: &mut dyn ComponentInstall,
        sleep: &dyn Fn(Duration),
    ) -> EssentialRun {
        (self.log)("restart stack: stopping all services...");
        self.stop_all();
        self.sweep();
        sleep(RESTART_PAUSE);
        (self.log)("restart stack: starting essentials...");
        let run = self.ensure_essentials(config, installer, sleep);
        (self.log)("restart stack: done");
        run
    }

    /// The web-server picker's switch: stops every *other* running web server.
    ///
    /// Returns the lines it logged - one per server that had to be stopped - so
    /// a caller can show them without reading the log. The setting itself is the
    /// document's (`settings.active_web_server`); what a running server has to
    /// do about a change to it is this.
    pub fn stop_other_web_servers(&self, choice: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for managed in &self.services {
            if !managed.is_web_kind() || managed.name() == choice || !managed.running() {
                continue;
            }
            let Some(service) = managed.service() else {
                continue;
            };
            let line = format!(
                "[{}] stopped — switching active web server to {choice}",
                managed.name()
            );
            (self.log)(&line);
            lines.push(line);
            let _ = service.stop();
        }
        lines
    }

    /// One service's `Restart`: stop it, let the port come free, start it again.
    ///
    /// The original's card handler, which the window built and then never
    /// placed - the original kept its per-service Restart button off the card, at
    /// `(cardW+100, cardH+100)` and one pixel square, so this timeline ran only
    /// if something else put it on screen. The port keeps the timeline, for the
    /// card the Lambo panel draws; the engine is what stops and starts, so the
    /// service reports itself running again exactly as it did after any other
    /// start.
    ///
    /// A stop that fails is not reported, as in the original: the start that
    /// follows is what has to succeed, and it logs its own failure. A service
    /// with no engine returns [`StartOutcome::Nothing`] without sleeping.
    pub fn restart(&self, name: &str, sleep: &dyn Fn(Duration)) -> Result<StartOutcome> {
        let managed = self.find(name).ok_or_else(|| self.unknown(name))?;
        let Some(service) = managed.service() else {
            return Ok(StartOutcome::Nothing);
        };
        let _ = service.stop();
        sleep(SERVICE_RESTART_PAUSE);
        Ok(self.start_engine(name))
    }

    /// Builds one service, keeping `engine` when there is one to keep.
    fn build_one(&self, conf: ServiceConf, engine: Option<Arc<Service>>) -> ManagedService {
        let service = match engine {
            Some(service) => Some(service),
            None if conf.exe.is_empty() => None,
            None => Some(Service::new(
                Arc::clone(&self.host),
                engine_config(&self.base_dir, &conf),
                Arc::clone(&self.log),
            )),
        };
        ManagedService {
            conf,
            base_dir: self.base_dir.clone(),
            service,
        }
    }

    /// Starts the engine, logging the failure the way the original did.
    fn start_engine(&self, name: &str) -> StartOutcome {
        let Some(service) = self.find(name).and_then(|managed| managed.service()) else {
            return StartOutcome::Nothing;
        };
        match service.start() {
            Ok(()) => StartOutcome::Started,
            Err(error) => {
                self.note(name, &format!("start: {error}"));
                StartOutcome::Failed(error.to_string())
            }
        }
    }

    /// A line of the stack's own log, prefixed with the service's name.
    fn note(&self, name: &str, line: &str) {
        (self.log)(&format!("[{name}] {line}"));
    }

    /// The error for a name the configuration does not have.
    fn unknown(&self, name: &str) -> Error {
        Error::InvalidInput(format!("no service named {name}"))
    }
}

/// The engine configuration one panel entry describes.
///
/// Every path is expanded from the installation directory, including the
/// arguments and the environment, and the working directory is optional: the
/// original passed an empty one on and its engine ignored it.
fn engine_config(base_dir: &Path, conf: &ServiceConf) -> ServiceConfig {
    let work_dir = expand_path(&conf.workdir, base_dir);
    let mut config = ServiceConfig::new(&conf.name, expand_path(&conf.exe, base_dir))
        .args(
            conf.args
                .iter()
                .map(|arg| expand_path(arg, base_dir))
                .collect::<Vec<String>>(),
        )
        .port(conf.port);

    if !work_dir.is_empty() {
        config = config.work_dir(work_dir);
    }

    for entry in &conf.env {
        // `KEY=VALUE`, as the configuration file spells it, expanded whole: the
        // original passed the list of strings to the same expansion it used for
        // the arguments.
        let entry = expand_path(entry, base_dir);
        let (key, value) = match entry.split_once('=') {
            Some((key, value)) => (key, value),
            None => (entry.as_str(), ""),
        };
        config = config.env(key, value);
    }

    config
}

/// Turns a start's outcome into one step of an essential pass.
fn step_of(outcome: StartOutcome, name: &str) -> EssentialStep {
    match outcome {
        StartOutcome::Started => EssentialStep::Started(name.to_owned()),
        StartOutcome::InstalledThenStarted => EssentialStep::InstalledThenStarted(name.to_owned()),
        StartOutcome::Installed => EssentialStep::Installed(name.to_owned()),
        StartOutcome::Opened(url) => EssentialStep::Opened(name.to_owned(), url),
        StartOutcome::Nothing => EssentialStep::Nothing(name.to_owned()),
        StartOutcome::ExeMissing(path) => EssentialStep::ExeMissing(name.to_owned(), path),
        StartOutcome::Failed(reason) => EssentialStep::Failed(name.to_owned(), reason),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::logs::nop_log;
    use crate::process::ProcessSpec;
    use crate::service::{CapturedRun, HostedProcess, WaitOutcome};
    use crate::testutil::TempDir;

    /// Keeps a scripted process alive until the test releases it, so "running"
    /// means the same thing here as it does on a real machine.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        signal: std::sync::Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap_or_else(poisoned);
            while !*open {
                open = self.signal.wait(open).unwrap_or_else(poisoned);
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap_or_else(poisoned) = true;
            self.signal.notify_all();
        }
    }

    /// A process the host hands out.
    struct LiveProcess {
        pid: u32,
        gate: Arc<Gate>,
    }

    impl HostedProcess for LiveProcess {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn wait(&self) -> WaitOutcome {
            self.gate.wait();
            WaitOutcome::Clean
        }

        fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
            None
        }

        fn take_stderr(&self) -> Option<Box<dyn Read + Send>> {
            None
        }
    }

    /// A host that records what the stack asked of it and touches nothing.
    struct RecordingHost {
        started: Mutex<Vec<ProcessSpec>>,
        killed: Mutex<Vec<u32>>,
        gates: Mutex<Vec<Arc<Gate>>>,
        processes: Mutex<Vec<zombies::RunningProcess>>,
        refuse_starts: AtomicBool,
        refuse_kills: AtomicBool,
    }

    impl Default for RecordingHost {
        fn default() -> Self {
            Self {
                started: Mutex::new(Vec::new()),
                killed: Mutex::new(Vec::new()),
                gates: Mutex::new(Vec::new()),
                processes: Mutex::new(Vec::new()),
                refuse_starts: AtomicBool::new(false),
                refuse_kills: AtomicBool::new(false),
            }
        }
    }

    impl RecordingHost {
        fn recording() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// The programs that were started, in order.
        fn started(&self) -> Vec<String> {
            self.started
                .lock()
                .unwrap_or_else(poisoned)
                .iter()
                .map(|spec| spec.program.display().to_string())
                .collect()
        }

        /// The PIDs that were terminated, in order.
        fn killed(&self) -> Vec<u32> {
            self.killed.lock().unwrap_or_else(poisoned).clone()
        }

        fn refusing_starts(&self) {
            self.refuse_starts.store(true, Ordering::SeqCst);
        }

        fn refusing_kills(&self) {
            self.refuse_kills.store(true, Ordering::SeqCst);
        }

        fn reporting(&self, processes: Vec<zombies::RunningProcess>) {
            *self.processes.lock().unwrap_or_else(poisoned) = processes;
        }

        /// Ends every scripted process, which is what a kill does on a real
        /// machine: the engine's waiter returns and the service stops reporting
        /// itself as running.
        fn release_all(&self) {
            for gate in self.gates.lock().unwrap_or_else(poisoned).iter() {
                gate.release();
            }
        }
    }

    impl ServiceHost for RecordingHost {
        fn port_busy(&self, _port: u16) -> bool {
            false
        }

        fn start(&self, spec: &ProcessSpec, _piped: bool) -> Result<Arc<dyn HostedProcess>> {
            if self.refuse_starts.load(Ordering::SeqCst) {
                return Err(Error::InvalidInput("the scripted host refuses".to_owned()));
            }
            let mut started = self.started.lock().unwrap_or_else(poisoned);
            started.push(spec.clone());
            let pid = 4000 + started.len() as u32;
            drop(started);

            let gate = Arc::new(Gate::default());
            self.gates
                .lock()
                .unwrap_or_else(poisoned)
                .push(Arc::clone(&gate));
            Ok(Arc::new(LiveProcess { pid, gate }))
        }

        fn process_handle(&self, pid: u32) -> Arc<dyn HostedProcess> {
            Arc::new(LiveProcess {
                pid,
                gate: Arc::new(Gate::default()),
            })
        }

        fn kill_tree(&self, pid: u32) -> Result<()> {
            if self.refuse_kills.load(Ordering::SeqCst) {
                return Err(Error::InvalidInput("the scripted host refuses".to_owned()));
            }
            self.killed.lock().unwrap_or_else(poisoned).push(pid);
            Ok(())
        }

        fn kill_process(&self, _pid: u32) -> Result<()> {
            if self.refuse_kills.load(Ordering::SeqCst) {
                return Err(Error::InvalidInput("the scripted host refuses".to_owned()));
            }
            Ok(())
        }

        fn run_captured(&self, _spec: &ProcessSpec) -> Result<CapturedRun> {
            Ok(CapturedRun::default())
        }

        fn self_exe(&self) -> Result<PathBuf> {
            Err(Error::InvalidInput("no executable path".to_owned()))
        }

        fn sleep(&self, _duration: Duration) {}

        fn list_processes(&self) -> Result<Vec<zombies::RunningProcess>> {
            Ok(self.processes.lock().unwrap_or_else(poisoned).clone())
        }
    }

    /// An installer that records what it was asked to install.
    #[derive(Default)]
    struct ScriptedInstall {
        installed: Vec<String>,
        failed: Option<String>,
    }

    impl ScriptedInstall {
        fn refusing(reason: &str) -> Self {
            Self {
                installed: Vec::new(),
                failed: Some(reason.to_owned()),
            }
        }
    }

    impl ComponentInstall for ScriptedInstall {
        fn install(&mut self, name: &str) -> Result<()> {
            match &self.failed {
                Some(reason) => Err(Error::InvalidInput(reason.clone())),
                None => {
                    self.installed.push(name.to_owned());
                    Ok(())
                }
            }
        }

        fn install_version(&mut self, name: &str, version: &str) -> Result<()> {
            match &self.failed {
                Some(reason) => Err(Error::InvalidInput(reason.clone())),
                None => {
                    self.installed.push(format!("{name}:{version}"));
                    Ok(())
                }
            }
        }
    }

    fn poisoned<T>(error: std::sync::PoisonError<T>) -> T {
        error.into_inner()
    }

    fn recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().unwrap_or_else(poisoned).push(line.to_owned());
        });
        (log, lines)
    }

    fn log_lines(lines: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        lines.lock().unwrap_or_else(poisoned).clone()
    }

    /// Waits for something to become true, because a start reports itself from
    /// another thread.
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A card's configuration, as the file would spell it.
    /// The kind the shipped configuration gives a service. The web group is what
    /// the "pick X up top" rule keys off, so a fixture has to be honest about it.
    fn kind_of(name: &str) -> &'static str {
        match name {
            "Apache" | "Nginx" => "web",
            "PHP-FPM" => "php",
            "MySQL" | "PostgreSQL" | "Redis" => "database",
            _ => "tool",
        }
    }

    fn conf(name: &str, exe: &str) -> ServiceConf {
        ServiceConf {
            name: name.to_owned(),
            kind: kind_of(name).to_owned(),
            exe: exe.to_owned(),
            args: Vec::new(),
            port: 0,
            workdir: String::new(),
            config_file: String::new(),
            enabled: true,
            open_url: String::new(),
            active_version: String::new(),
            env: Vec::new(),
        }
    }

    fn tool(name: &str) -> ServiceConf {
        ServiceConf {
            kind: "tool".to_owned(),
            ..conf(name, "")
        }
    }

    /// A configuration of exactly these services.
    fn config_with(services: Vec<ServiceConf>) -> PanelConfig {
        PanelConfig {
            services,
            ..PanelConfig::default_config()
        }
    }

    /// The essential four, each with an executable under the installation.
    fn essential_config() -> PanelConfig {
        config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("PHP-FPM", "{base}/bin/php/php-cgi.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
            tool("phpMyAdmin"),
        ])
    }

    /// Writes a file, creating the directories it needs.
    fn write_file(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("the file has a directory"))
            .expect("failed to create the fixture directory");
        std::fs::write(path, "").expect("failed to write the fixture");
    }

    /// Marks a component installed, at the file its own catalogue entry checks
    /// for.
    fn mark_installed(base_dir: &Path, name: &str) {
        let component = catalog_panel::find(name).unwrap_or_else(|| panic!("{name} is catalogued"));
        write_file(&component.canonical_dir(base_dir).join(component.check_file));
    }

    /// The executable the fixtures' configurations name for a service.
    fn exe_of(name: &str) -> Option<&'static str> {
        match name {
            "Apache" => Some("{base}/bin/apache/bin/httpd.exe"),
            "Nginx" => Some("{base}/bin/nginx/nginx.exe"),
            "PHP-FPM" => Some("{base}/bin/php/php-cgi.exe"),
            "MySQL" => Some("{base}/bin/mysql/bin/mysqld.exe"),
            _ => None,
        }
    }

    /// Puts a configuration's own executable on disk.
    fn make_exe(base_dir: &Path, exe: &str) {
        write_file(Path::new(&expand_path(exe, base_dir)));
    }

    /// An installation the panel would call complete: the component's check file
    /// and the executable the configuration names are both there. Without the
    /// executable the card's own rule refuses the start - `exe missing` - however
    /// installed the component is.
    fn install_fixture(base_dir: &Path, name: &str) {
        mark_installed(base_dir, name);
        if let Some(exe) = exe_of(name) {
            make_exe(base_dir, exe);
        }
    }

    fn stack_of(base_dir: &Path, config: &PanelConfig, host: Arc<RecordingHost>) -> Stack {
        Stack::build(base_dir, config, host, nop_log())
    }

    /// The same stack, with the test's log sink.
    fn stack_logging(
        base_dir: &Path,
        config: &PanelConfig,
        host: &Arc<RecordingHost>,
        log: LogFn,
    ) -> Stack {
        Stack::build(base_dir, config, Arc::<RecordingHost>::clone(host), log)
    }

    #[test]
    fn every_configured_service_becomes_a_card() {
        let temp = TempDir::new();
        let config = essential_config();
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        assert_eq!(stack.services().len(), config.services.len());
        let names: Vec<&str> = stack.services().iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["Apache", "PHP-FPM", "MySQL", "phpMyAdmin"]);
        assert_eq!(stack.index_of("MySQL"), Some(2));
        assert_eq!(stack.index_of("Nothing"), None);
    }

    #[test]
    fn a_service_without_an_executable_has_no_engine() {
        let temp = TempDir::new();
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            tool("phpMyAdmin"),
        ]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        assert!(
            stack
                .find("Apache")
                .expect("configured")
                .service()
                .is_some()
        );
        assert!(
            stack
                .find("phpMyAdmin")
                .expect("configured")
                .service()
                .is_none(),
            "a tool is a card without an engine"
        );
    }

    #[test]
    fn the_configuration_paths_are_expanded_from_the_installation() {
        let temp = TempDir::new();
        let mut apache = conf("Apache", "{base}/bin/apache/bin/httpd.exe");
        apache.args = vec!["-d".to_owned(), "{base}/bin/apache".to_owned()];
        apache.port = 8080;
        apache.workdir = "{base}/bin/apache".to_owned();
        apache.env = vec!["APACHE_LOG_DIR={base}/log".to_owned()];
        let config = config_with(vec![apache]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        let engine = stack
            .find("Apache")
            .and_then(|service| service.service())
            .expect("it has an engine")
            .config()
            .clone();
        let base = temp.path().display().to_string();
        assert_eq!(engine.name, "Apache");
        assert_eq!(
            engine.exe_path,
            PathBuf::from(format!("{base}/bin/apache/bin/httpd.exe"))
        );
        assert_eq!(
            engine.args,
            vec!["-d".to_owned(), format!("{base}/bin/apache")]
        );
        assert_eq!(engine.port, 8080);
        assert_eq!(
            engine.work_dir,
            Some(PathBuf::from(format!("{base}/bin/apache")))
        );
        assert_eq!(
            engine.env,
            vec![("APACHE_LOG_DIR".to_owned(), format!("{base}/log"))]
        );
    }

    #[test]
    fn an_empty_working_directory_stays_unset() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        assert_eq!(
            stack
                .find("Apache")
                .and_then(|service| service.service())
                .expect("it has an engine")
                .config()
                .work_dir,
            None
        );
    }

    #[test]
    fn an_environment_entry_without_a_value_is_still_set() {
        let temp = TempDir::new();
        let mut apache = conf("Apache", "{base}/bin/apache/bin/httpd.exe");
        apache.env = vec!["LAMBO".to_owned()];
        let config = config_with(vec![apache]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        assert_eq!(
            stack
                .find("Apache")
                .and_then(|service| service.service())
                .expect("it has an engine")
                .config()
                .env,
            vec![("LAMBO".to_owned(), String::new())]
        );
    }

    #[test]
    fn reloading_keeps_the_engine_of_a_service_that_is_still_configured() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let mut stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let before = Arc::clone(
            stack
                .find("Apache")
                .and_then(|service| service.service())
                .expect("it has an engine"),
        );

        stack.reload(&config);

        let after = stack
            .find("Apache")
            .and_then(|service| service.service())
            .expect("it still has one");
        assert!(
            Arc::ptr_eq(&before, after),
            "a running service keeps the engine it was started with"
        );
    }

    #[test]
    fn reloading_drops_a_service_the_configuration_no_longer_has() {
        let temp = TempDir::new();
        let mut stack = stack_of(
            temp.path(),
            &config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]),
            RecordingHost::recording(),
        );

        stack.reload(&config_with(vec![]));

        assert!(stack.find("Apache").is_none());
        assert!(stack.services().is_empty());
    }

    #[test]
    fn discovering_an_executable_after_a_reload_gives_the_service_an_engine() {
        let temp = TempDir::new();
        let mut stack = stack_of(
            temp.path(),
            &config_with(vec![tool("phpMyAdmin")]),
            RecordingHost::recording(),
        );
        assert!(
            stack
                .find("phpMyAdmin")
                .expect("configured")
                .service()
                .is_none()
        );

        stack.reload(&config_with(vec![tool("phpMyAdmin")]));

        // Still none: the configuration is what decides. A new engine appears
        // when the entry gains an executable, which is what an install does by
        // rewriting the configuration.
        assert!(
            stack
                .find("phpMyAdmin")
                .expect("configured")
                .service()
                .is_none()
        );

        stack.reload(&config_with(vec![conf(
            "phpMyAdmin",
            "{base}/bin/phpmyadmin/php.exe",
        )]));
        assert!(
            stack
                .find("phpMyAdmin")
                .expect("configured")
                .service()
                .is_some(),
            "an entry with an executable has an engine from then on"
        );
    }

    #[test]
    fn a_disabled_service_says_so_and_starts_anyway() {
        let temp = TempDir::new();
        let mut mysql = conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe");
        mysql.enabled = false;
        let config = config_with(vec![mysql]);
        install_fixture(temp.path(), "MySQL");
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();

        assert_eq!(
            stack.find("MySQL").expect("configured").status("Apache"),
            "Disabled"
        );

        // The card's Start button does not consult the flag; only the essential
        // pass does.
        let outcome = stack
            .start_with_install("MySQL", &mut installer)
            .expect("it is configured");
        assert_eq!(outcome, StartOutcome::Started);
        assert!(host.started()[0].ends_with("mysqld.exe"));
    }

    #[test]
    fn a_running_service_shows_its_pid_and_its_port() {
        let temp = TempDir::new();
        let mut apache = conf("Apache", "{base}/bin/apache/bin/httpd.exe");
        apache.port = 80;
        let config = config_with(vec![apache]);
        install_fixture(temp.path(), "Apache");
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let mut installer = ScriptedInstall::default();

        stack
            .start_with_install("Apache", &mut installer)
            .expect("it is configured");

        let managed = stack.find("Apache").expect("configured");
        assert!(eventually(|| managed.running()));
        assert_eq!(
            managed.status("Apache"),
            format!("Running  pid {}  :80", managed.pid().expect("a pid"))
        );
        assert_eq!(managed.dot("Apache"), '●');
    }

    #[test]
    fn a_web_service_that_is_not_the_active_one_says_which_to_pick() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Nginx", "{base}/bin/nginx/nginx.exe")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        let nginx = stack.find("Nginx").expect("configured");
        assert_eq!(nginx.status("Apache"), "Inactive — pick Nginx up top");
        assert_eq!(nginx.dot("Apache"), '·');
        assert_eq!(nginx.status("Nginx"), "Stopped");
    }

    #[test]
    fn a_stopped_service_has_an_empty_dot() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        install_fixture(temp.path(), "Apache");
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        assert_eq!(stack.find("Apache").expect("configured").dot("Apache"), '○');
    }

    #[test]
    fn a_tool_that_is_not_installed_says_so_and_has_an_empty_dot() {
        let temp = TempDir::new();
        let config = config_with(vec![tool("phpMyAdmin")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        let phpmyadmin = stack.find("phpMyAdmin").expect("configured");
        assert_eq!(phpmyadmin.status("Apache"), "Not installed");
        // A tool has no engine, so the original's `IsInstalled(..) || Service ==
        // nil` counted it as installed: the dot is the empty circle, not the
        // in-between dot that an uninstalled *process* has.
        assert_eq!(phpmyadmin.dot("Apache"), '○');
    }

    #[test]
    fn a_tool_that_is_installed_says_installed() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "phpMyAdmin");
        let config = config_with(vec![tool("phpMyAdmin")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        let phpmyadmin = stack.find("phpMyAdmin").expect("configured");
        assert_eq!(phpmyadmin.status("Apache"), "Installed (tool)");
        assert_eq!(phpmyadmin.dot("Apache"), '○');
    }

    #[test]
    fn an_unknown_name_is_an_error_for_a_start_and_nothing_for_a_lookup() {
        let temp = TempDir::new();
        let stack = stack_of(temp.path(), &essential_config(), RecordingHost::recording());
        let mut installer = ScriptedInstall::default();

        assert!(stack.find("Nope").is_none());
        assert!(stack.plan_start("Nope").is_none());
        assert!(stack.start_with_install("Nope", &mut installer).is_err());
        assert!(stack.stop("Nope").is_err());
    }

    #[test]
    fn an_executable_that_is_not_on_disk_is_reported_before_the_start() {
        let temp = TempDir::new();
        mark_installed(temp.path(), "Apache");
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::default();

        let outcome = stack
            .start_with_install("Apache", &mut installer)
            .expect("it is configured");

        let path = PathBuf::from(format!(
            "{}/bin/apache/bin/httpd.exe",
            temp.path().display()
        ));
        assert_eq!(outcome, StartOutcome::ExeMissing(path.clone()));
        assert!(host.started().is_empty(), "nothing was started");
        assert_eq!(
            log_lines(&lines),
            vec![format!("[Apache] exe missing: {}", path.display())]
        );
    }

    #[test]
    fn a_relative_executable_is_left_to_the_operating_system() {
        let temp = TempDir::new();
        mark_installed(temp.path(), "Apache");
        let config = config_with(vec![conf("Apache", "httpd.exe")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());

        assert_eq!(stack.plan_start("Apache"), Some(StartPlan::Start));
    }

    #[test]
    fn a_missing_component_is_installed_and_then_started() {
        let temp = TempDir::new();
        let mut apache = conf("Apache", "{base}/bin/apache/bin/httpd.exe");
        apache.active_version = "2.4.63".to_owned();
        let config = config_with(vec![apache]);
        make_exe(temp.path(), "{base}/bin/apache/bin/httpd.exe");
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::default();

        assert_eq!(
            stack.plan_start("Apache"),
            Some(StartPlan::InstallThenStart {
                name: "Apache".to_owned(),
                version: "2.4.63".to_owned(),
            })
        );

        let outcome = stack
            .start_with_install("Apache", &mut installer)
            .expect("it is configured");

        assert_eq!(outcome, StartOutcome::InstalledThenStarted);
        assert_eq!(installer.installed, vec!["Apache:2.4.63".to_owned()]);
        assert_eq!(
            log_lines(&lines).first(),
            Some(&"[Apache] install incomplete — auto-reinstalling...".to_owned()),
            "the re-install is announced before it happens: {:?}",
            log_lines(&lines)
        );
        assert!(host.started()[0].ends_with("httpd.exe"));
    }

    #[test]
    fn an_install_that_fails_stops_that_start() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::refusing("no network");

        let outcome = stack
            .start_with_install("Apache", &mut installer)
            .expect("it is configured");

        assert_eq!(outcome, StartOutcome::Failed("no network".to_owned()));
        assert!(host.started().is_empty());
        assert_eq!(
            log_lines(&lines),
            vec![
                "[Apache] install incomplete — auto-reinstalling...".to_owned(),
                "[Apache] install failed: no network".to_owned(),
            ]
        );
    }

    #[test]
    fn a_tool_that_is_missing_is_installed_without_a_version() {
        let temp = TempDir::new();
        let config = config_with(vec![tool("phpMyAdmin")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let mut installer = ScriptedInstall::default();

        assert_eq!(
            stack.plan_start("phpMyAdmin"),
            Some(StartPlan::Install("phpMyAdmin".to_owned()))
        );

        let outcome = stack
            .start_with_install("phpMyAdmin", &mut installer)
            .expect("it is configured");

        assert_eq!(outcome, StartOutcome::Installed);
        assert_eq!(installer.installed, vec!["phpMyAdmin".to_owned()]);
    }

    #[test]
    fn a_tool_with_a_url_is_opened_and_one_without_is_left_alone() {
        let temp = TempDir::new();
        // Neither name is in the catalogue, so there is nothing to install and
        // the cards are links, which is what a URL-only service is.
        let mut link = tool("Custom Tool");
        link.open_url = "http://localhost/custom/".to_owned();
        let config = config_with(vec![link, tool("Another Tool")]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let mut installer = ScriptedInstall::default();

        assert_eq!(
            stack
                .start_with_install("Custom Tool", &mut installer)
                .expect("it is configured"),
            StartOutcome::Opened("http://localhost/custom/".to_owned())
        );
        assert_eq!(
            stack
                .start_with_install("Another Tool", &mut installer)
                .expect("it is configured"),
            StartOutcome::Nothing
        );
        assert!(installer.installed.is_empty(), "a URL needs no install");
    }

    #[test]
    fn a_catalogued_tool_that_is_installed_is_not_installed_again() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "phpMyAdmin");
        let mut adminer = tool("phpMyAdmin");
        adminer.open_url = "http://localhost/phpmyadmin/".to_owned();
        let config = config_with(vec![adminer]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let mut installer = ScriptedInstall::default();

        let outcome = stack
            .start_with_install("phpMyAdmin", &mut installer)
            .expect("it is configured");

        assert_eq!(
            outcome,
            StartOutcome::Opened("http://localhost/phpmyadmin/".to_owned())
        );
        assert!(installer.installed.is_empty());
    }

    #[test]
    fn start_stack_walks_the_essential_list_in_order() {
        let temp = TempDir::new();
        for name in ["Apache", "PHP-FPM", "MySQL", "phpMyAdmin"] {
            install_fixture(temp.path(), name);
        }
        let mut phpmyadmin = tool("phpMyAdmin");
        phpmyadmin.open_url = "http://localhost/phpmyadmin/".to_owned();
        let config = config_with(vec![
            conf("PHP-FPM", "{base}/bin/php/php-cgi.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            phpmyadmin,
        ]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();

        let steps = stack.start_essential(&mut installer);

        assert_eq!(
            host.started(),
            vec![
                format!("{}/bin/apache/bin/httpd.exe", temp.path().display()),
                format!("{}/bin/php/php-cgi.exe", temp.path().display()),
                format!("{}/bin/mysql/bin/mysqld.exe", temp.path().display()),
            ],
            "the tray's Start Stack uses the fixed list, not the config's order"
        );
        assert_eq!(
            steps,
            vec![
                EssentialStep::Started("Apache".to_owned()),
                EssentialStep::Started("PHP-FPM".to_owned()),
                EssentialStep::Started("MySQL".to_owned()),
                EssentialStep::Opened(
                    "phpMyAdmin".to_owned(),
                    "http://localhost/phpmyadmin/".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn start_stack_ignores_a_service_the_configuration_does_not_have() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::refusing("nothing to install");

        let steps = stack.start_essential(&mut installer);

        // Apache is not installed, its install is refused, and the three
        // services that are not configured at all are simply not there.
        assert_eq!(host.started(), Vec::<String>::new());
        assert_eq!(
            steps,
            vec![EssentialStep::Failed(
                "Apache".to_owned(),
                "nothing to install".to_owned()
            )]
        );
    }

    #[test]
    fn start_stack_leaves_a_web_server_the_configuration_does_not_have_alone() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "MySQL");
        let config = config_with(vec![conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe")]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();

        let steps = stack.start_essential(&mut installer);

        assert_eq!(steps, vec![EssentialStep::Started("MySQL".to_owned())]);
        assert_eq!(host.started().len(), 1);
    }

    #[test]
    fn the_page_stack_uses_the_active_web_server_and_pauses_between_starts() {
        let temp = TempDir::new();
        for name in ["Nginx", "PHP-FPM", "MySQL", "phpMyAdmin"] {
            install_fixture(temp.path(), name);
        }
        let mut nginx = conf("Nginx", "{base}/bin/nginx/nginx.exe");
        nginx.enabled = true;
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            nginx,
            conf("PHP-FPM", "{base}/bin/php/php-cgi.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
            tool("phpMyAdmin"),
        ]);
        let mut config = config;
        config.settings.active_web_server = "Nginx".to_owned();
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();
        let pauses = Mutex::new(Vec::new());

        let run = stack.ensure_essentials(&config, &mut installer, &|duration| {
            pauses.lock().unwrap_or_else(poisoned).push(duration);
        });

        assert_eq!(
            host.started(),
            vec![
                format!("{}/bin/nginx/nginx.exe", temp.path().display()),
                format!("{}/bin/php/php-cgi.exe", temp.path().display()),
                format!("{}/bin/mysql/bin/mysqld.exe", temp.path().display()),
            ],
            "switching the active web server moves the whole stack with it"
        );
        assert_eq!(
            *pauses.lock().unwrap_or_else(poisoned),
            vec![
                ESSENTIAL_PAUSE,
                ESSENTIAL_PAUSE,
                ESSENTIAL_PAUSE,
                OPEN_PAUSE
            ]
        );
        assert_eq!(run.open_url, LOCAL_URL);
        assert_eq!(
            run.steps.last(),
            Some(&EssentialStep::Skipped("phpMyAdmin".to_owned())),
            "an installed tool has nothing to start"
        );
    }

    #[test]
    fn the_panels_start_stack_is_the_pages_own_phases() {
        // The page's pass differs from `ensure_essentials` in what it reports
        // and in one thing it does: a component with nothing to run is
        // *installed* and then skipped, rather than resolved to the URL it would
        // have opened. Both passes exist because both did.
        let temp = TempDir::new();
        install_fixture(temp.path(), "Apache");
        install_fixture(temp.path(), "PHP-FPM");
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("PHP-FPM", "{base}/bin/php/php-cgi.exe"),
            tool("phpMyAdmin"),
        ]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();
        let pauses = Mutex::new(Vec::new());

        let steps = stack.ensure_stack_essentials(&config, &mut installer, &|duration| {
            pauses.lock().unwrap_or_else(poisoned).push(duration);
        });

        assert_eq!(
            host.started(),
            vec![
                format!("{}/bin/apache/bin/httpd.exe", temp.path().display()),
                format!("{}/bin/php/php-cgi.exe", temp.path().display()),
            ]
        );
        assert_eq!(
            steps,
            vec![
                StartStackStep::Started("Apache".to_owned(), StartOutcome::Started),
                StartStackStep::Started("PHP-FPM".to_owned(), StartOutcome::Started),
                // phpMyAdmin is missing, so its component is fetched first - and
                // only then does the pass notice there is nothing to start.
                StartStackStep::Installed("phpMyAdmin".to_owned()),
                StartStackStep::Skipped("phpMyAdmin".to_owned()),
            ]
        );
        // One pause per service that was *started* - not per service walked -
        // and then the wait before the site is opened, which the caller does.
        assert_eq!(
            *pauses.lock().unwrap_or_else(poisoned),
            vec![ESSENTIAL_PAUSE, ESSENTIAL_PAUSE, OPEN_PAUSE]
        );
    }

    #[test]
    fn the_panels_start_stack_reports_an_install_it_had_to_do_first() {
        let temp = TempDir::new();
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();

        let steps = stack.ensure_stack_essentials(&config, &mut installer, &|_| {});

        // Apache was not installed, so the component is fetched and then
        // started; the installer is the one that remembers the name.
        assert_eq!(
            steps,
            vec![
                StartStackStep::Installed("Apache".to_owned()),
                // The plan was made before the install, so the start that
                // follows it is the engine's own `InstalledThenStarted`.
                StartStackStep::Started("Apache".to_owned(), StartOutcome::InstalledThenStarted),
            ]
        );
        // Twice, because both halves install: the pass's own pre-check, and the
        // start's plan - which was formed before the component arrived.
        assert_eq!(
            installer.installed,
            vec!["Apache:".to_owned(), "Apache:".to_owned()]
        );
    }

    #[test]
    fn the_page_stack_skips_a_disabled_service_but_installs_its_component_first() {
        let temp = TempDir::new();
        for name in ["Apache", "PHP-FPM"] {
            install_fixture(temp.path(), name);
        }
        let mut mysql = conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe");
        mysql.enabled = false;
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("PHP-FPM", "{base}/bin/php/php-cgi.exe"),
            mysql,
            tool("phpMyAdmin"),
        ]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();

        let run = stack.ensure_essentials(&config, &mut installer, &|_| {});

        assert_eq!(
            installer.installed,
            vec!["MySQL:".to_owned(), "phpMyAdmin:".to_owned()],
            "the component is installed before the Enabled flag is consulted"
        );
        assert_eq!(
            run.steps,
            vec![
                EssentialStep::Started("Apache".to_owned()),
                EssentialStep::Started("PHP-FPM".to_owned()),
                EssentialStep::Installed("MySQL".to_owned()),
                EssentialStep::Skipped("MySQL".to_owned()),
                EssentialStep::Installed("phpMyAdmin".to_owned()),
                EssentialStep::Skipped("phpMyAdmin".to_owned()),
            ]
        );
        assert_eq!(
            host.started().len(),
            2,
            "the disabled service is not started"
        );
    }

    #[test]
    fn the_page_stack_logs_a_failed_install_and_moves_on() {
        let temp = TempDir::new();
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            tool("phpMyAdmin"),
        ]);
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::refusing("no network");

        let run = stack.ensure_essentials(&config, &mut installer, &|_| {});

        assert_eq!(
            log_lines(&lines),
            vec![
                "[Apache] install failed: no network".to_owned(),
                "[phpMyAdmin] install failed: no network".to_owned(),
            ],
            "the pass reports each failure and carries on with the next service"
        );
        assert_eq!(
            run.steps,
            vec![
                EssentialStep::InstallFailed("Apache".to_owned()),
                EssentialStep::InstallFailed("phpMyAdmin".to_owned()),
            ]
        );
        assert!(host.started().is_empty());
    }

    #[test]
    fn the_page_stack_needs_no_service_the_configuration_lacks() {
        let temp = TempDir::new();
        let config = config_with(vec![]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let mut installer = ScriptedInstall::default();

        let run = stack.ensure_essentials(&config, &mut installer, &|_| {});

        assert!(run.steps.is_empty());
        assert_eq!(run.open_url, LOCAL_URL);
    }

    #[test]
    fn auto_start_starts_each_name_in_order() {
        let temp = TempDir::new();
        for name in ["Apache", "MySQL"] {
            install_fixture(temp.path(), name);
        }
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
            tool("phpMyAdmin"),
        ]);
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);

        let reported = stack.auto_start(&[
            "MySQL".to_owned(),
            "phpMyAdmin".to_owned(),
            "Nothing".to_owned(),
            "Apache".to_owned(),
        ]);

        assert_eq!(
            host.started(),
            vec![
                format!("{}/bin/mysql/bin/mysqld.exe", temp.path().display()),
                format!("{}/bin/apache/bin/httpd.exe", temp.path().display()),
            ],
            "the order is the setting's, and a tool with no engine is skipped"
        );
        assert!(reported.is_empty());
        assert!(
            !log_lines(&lines)
                .iter()
                .any(|line| line.starts_with("[auto-start]")),
            "{:?}",
            log_lines(&lines)
        );
    }

    #[test]
    fn auto_start_logs_a_failure_and_carries_on() {
        let temp = TempDir::new();
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
        ]);
        let host = RecordingHost::recording();
        host.refusing_starts();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);

        let reported = stack.auto_start(&["Apache".to_owned(), "MySQL".to_owned()]);

        assert_eq!(
            reported,
            vec![
                "[auto-start] Apache: the scripted host refuses".to_owned(),
                "[auto-start] MySQL: the scripted host refuses".to_owned(),
            ]
        );
        assert_eq!(log_lines(&lines), reported);
    }

    #[test]
    fn stop_all_stops_in_the_configurations_order() {
        let temp = TempDir::new();
        for name in ["Apache", "PHP-FPM", "MySQL"] {
            install_fixture(temp.path(), name);
        }
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("PHP-FPM", "{base}/bin/php/php-cgi.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
            tool("phpMyAdmin"),
        ]);
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();
        for name in ["Apache", "PHP-FPM", "MySQL"] {
            stack
                .start_with_install(name, &mut installer)
                .expect("it is configured");
        }
        assert!(eventually(|| stack
            .services()
            .iter()
            .filter(|service| service.service().is_some())
            .all(|service| service.running())));

        stack.stop_all();

        assert_eq!(
            host.killed(),
            vec![4001, 4002, 4003],
            "the order is the configuration's, not the reverse"
        );
    }

    #[test]
    fn stop_all_ignores_a_service_that_will_not_stop() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "Apache");
        install_fixture(temp.path(), "MySQL");
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
        ]);
        let host = RecordingHost::recording();
        host.refusing_kills();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();
        for name in ["Apache", "MySQL"] {
            stack
                .start_with_install(name, &mut installer)
                .expect("it is configured");
        }

        // Both ways of stopping fail here, which the engine reports; Stop All
        // must survive it and leave nothing behind.
        stack.stop_all();

        assert!(host.killed().is_empty());
    }

    #[test]
    fn one_callback_reports_every_service() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "Apache");
        install_fixture(temp.path(), "MySQL");
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("MySQL", "{base}/bin/mysql/bin/mysqld.exe"),
        ]);
        let stack = stack_of(temp.path(), &config, RecordingHost::recording());
        let seen: Arc<Mutex<Vec<(bool, u32)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        stack.set_state_callback(Arc::new(move |running: bool, pid: u32| {
            sink.lock().unwrap_or_else(poisoned).push((running, pid));
        }));
        let mut installer = ScriptedInstall::default();

        stack
            .start_with_install("MySQL", &mut installer)
            .expect("it is configured");

        assert!(eventually(|| !seen
            .lock()
            .unwrap_or_else(poisoned)
            .is_empty()));
        let seen = seen.lock().unwrap_or_else(poisoned).clone();
        let first = seen.first().copied();
        assert_eq!(first, Some((true, 4001)));
    }

    #[test]
    fn the_sweep_keeps_what_this_stack_is_running() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "Apache");
        let config = config_with(vec![conf("Apache", "{base}/bin/apache/bin/httpd.exe")]);
        let stale = temp.path().join("bin").join("mysql").join("mysqld.exe");
        let running = temp
            .path()
            .join("bin")
            .join("apache")
            .join("bin")
            .join("httpd.exe");
        let host = RecordingHost::recording();
        // Only the leftover is visible when the service starts. A process of
        // Apache's own program that is already running would make the start
        // refuse - it is the instance the failed run left behind - and this test
        // is about the sweep, so the service is started first and both processes
        // are visible to the enumeration afterwards.
        host.reporting(vec![zombies::RunningProcess {
            pid: 900,
            path: stale.clone(),
        }]);
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::default();
        stack
            .start_with_install("Apache", &mut installer)
            .expect("it is configured");
        assert!(eventually(|| stack
            .find("Apache")
            .is_some_and(|s| s.pid() == Some(4001))));

        // What the sweep enumerates: the leftover, and the process this stack
        // is running.
        host.reporting(vec![
            zombies::RunningProcess {
                pid: 900,
                path: stale.clone(),
            },
            zombies::RunningProcess {
                pid: 4001,
                path: running,
            },
        ]);

        let killed = stack.sweep();

        assert_eq!(
            killed,
            vec![format!(
                "startup sweep: killed stale {} (pid 900)",
                stale.display()
            )]
        );
        assert_eq!(host.killed(), vec![900], "the running service is kept");
        assert_eq!(
            log_lines(&lines).last(),
            Some(&killed[0]),
            "the sweep is logged where the start was: {:?}",
            log_lines(&lines)
        );
    }

    #[test]
    fn a_stack_with_nothing_running_sweeps_everything_under_bin() {
        let temp = TempDir::new();
        let stale = temp.path().join("bin").join("php").join("php-cgi.exe");
        let host = RecordingHost::recording();
        host.reporting(vec![zombies::RunningProcess {
            pid: 42,
            path: stale.clone(),
        }]);
        let stack = stack_of(temp.path(), &config_with(vec![]), Arc::clone(&host));

        let killed = stack.sweep();

        assert_eq!(killed.len(), 1);
        assert_eq!(host.killed(), vec![42]);
    }

    #[test]
    fn the_lifecycle_the_cli_runs_goes_through_the_stack() {
        let temp = TempDir::new();
        for name in ["Apache", "PHP-FPM", "MySQL"] {
            install_fixture(temp.path(), name);
        }
        let config = essential_config();
        let host = RecordingHost::recording();
        let stack = stack_of(temp.path(), &config, Arc::clone(&host));
        let mut installer = ScriptedInstall::default();

        // `lambo up`: boot the stack and report the page.
        let run = stack.ensure_essentials(&config, &mut installer, &|_| {});
        assert_eq!(run.open_url, LOCAL_URL);
        assert_eq!(host.started().len(), 3);

        // `lambo status`: the cards, from the engine's live state.
        assert!(eventually(|| stack
            .services()
            .iter()
            .filter(|service| service.name() != "phpMyAdmin")
            .all(|service| service.running())));
        let statuses: Vec<String> = stack
            .services()
            .iter()
            .map(|service| service.status("Apache"))
            .collect();
        assert!(statuses[0].starts_with("Running  pid "), "{statuses:?}");
        assert_eq!(statuses[3], "Not installed");

        // `lambo down`: stop everything, in configuration order.
        stack.stop_all();
        assert_eq!(host.killed(), vec![4001, 4002, 4003]);
    }

    #[test]
    fn switching_the_active_web_server_stops_the_other_one() {
        let temp = TempDir::new();
        for name in ["Apache", "Nginx"] {
            install_fixture(temp.path(), name);
        }
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            conf("Nginx", "{base}/bin/nginx/nginx.exe"),
        ]);
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::default();
        for name in ["Apache", "Nginx"] {
            stack
                .start_with_install(name, &mut installer)
                .expect("it is configured");
        }
        assert!(eventually(|| stack
            .services()
            .iter()
            .all(|service| service.running())));

        // Switching to Nginx stops Apache. The kill is the evidence: a service
        // keeps reporting itself running until its process's exit has been seen,
        // which is the original's own behaviour - `Stop` never cleared its
        // handle either, the waiter did.
        let reported = stack.stop_other_web_servers("Nginx");
        assert_eq!(
            reported,
            vec!["[Apache] stopped \u{2014} switching active web server to Nginx".to_owned()]
        );
        assert_eq!(host.killed(), vec![4001], "the other web server is killed");
        assert!(
            log_lines(&lines).contains(&"[Apache] stopping (pid 4001)...".to_owned()),
            "{:?}",
            log_lines(&lines)
        );
        assert!(
            !log_lines(&lines)
                .iter()
                .any(|line| line.contains("[Nginx] stopping")),
            "the server that was picked is left alone"
        );

        // And the other way round: choosing Apache stops Nginx, once.
        let reported = stack.stop_other_web_servers("Apache");
        assert_eq!(
            reported,
            vec!["[Nginx] stopped \u{2014} switching active web server to Apache".to_owned()]
        );
        assert_eq!(host.killed(), vec![4001, 4002]);
    }

    #[test]
    fn a_restart_stops_everything_pauses_and_starts_the_essentials_again() {
        let temp = TempDir::new();
        for name in ["Apache", "PHP-FPM", "MySQL", "phpMyAdmin"] {
            install_fixture(temp.path(), name);
        }
        let config = essential_config();
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);
        let mut installer = ScriptedInstall::default();

        // Something is running before the restart.
        stack
            .start_with_install("Apache", &mut installer)
            .expect("it is configured");
        assert!(eventually(|| stack
            .find("Apache")
            .expect("configured")
            .running()));

        // The pause after the stop is where a real kill takes effect, so that is
        // where the scripted host is told its processes are gone.
        let released = AtomicBool::new(false);
        let host_for_sleep = Arc::clone(&host);
        let run = stack.restart_essentials(&config, &mut installer, &|duration| {
            if !released.swap(true, Ordering::SeqCst) {
                assert_eq!(duration, RESTART_PAUSE, "the restart pauses after stopping");
                host_for_sleep.release_all();
                // And the engine has to see the exit before the service can be
                // started again - on a real machine that is what the pause is
                // for, and here it is what the test waits for.
                assert!(
                    eventually(|| !stack.find("Apache").expect("configured").running()),
                    "the stopped process must be observed as gone"
                );
            }
        });

        // The engine's own lines are in the same log, so the restart's three are
        // found by name and checked for order rather than by position.
        let said = log_lines(&lines);
        let at = |line: &str| {
            said.iter()
                .position(|entry| entry == line)
                .unwrap_or_else(|| panic!("{line} was not logged: {said:?}"))
        };
        let stopping = at("restart stack: stopping all services...");
        let starting = at("restart stack: starting essentials...");
        let done = at("restart stack: done");
        assert!(stopping < starting && starting < done, "{said:?}");
        assert_eq!(
            done,
            said.len() - 1,
            "the last line is the last one: {said:?}"
        );
        assert_eq!(
            run.open_url, LOCAL_URL,
            "the essentials pass opens the page"
        );
        assert!(
            !said.iter().any(|line| line.contains("install failed")),
            "{said:?}"
        );

        // Everything came back up: the manual start, then the three essentials
        // that are processes. phpMyAdmin is a tool, so it is not started at all.
        assert_eq!(host.started().len(), 4, "{:?}", host.started());
        assert_eq!(host.killed(), vec![4001], "the restart stopped what was up");
        assert!(stack.find("Apache").expect("configured").running());
        assert!(stack.find("PHP-FPM").expect("configured").running());
        assert!(stack.find("MySQL").expect("configured").running());
    }

    #[test]
    fn restarting_one_service_stops_it_pauses_and_starts_it_again() {
        let temp = TempDir::new();
        install_fixture(temp.path(), "Apache");
        let config = config_with(vec![
            conf("Apache", "{base}/bin/apache/bin/httpd.exe"),
            // A card with no engine: a tool, or a runtime that is not a process.
            conf("phpMyAdmin", ""),
        ]);
        let host = RecordingHost::recording();
        let (log, lines) = recorder();
        let stack = stack_logging(temp.path(), &config, &host, log);

        // The pause between the stop and the start is where a real kill takes
        // effect, so that is where the scripted host is told its process is
        // gone - and the engine has to have *seen* it before the start, or the
        // start is refused as `already running`.
        let pauses: Mutex<Vec<Duration>> = Mutex::new(Vec::new());
        let sleep = |duration: Duration| {
            pauses.lock().unwrap_or_else(poisoned).push(duration);
            host.release_all();
            assert!(eventually(|| !stack
                .find("Apache")
                .expect("configured")
                .running()));
        };

        // A service that is not running is still restarted the same way: the
        // stop does nothing, the pause happens because the original slept
        // unconditionally, and the start is the one the card would have done.
        let outcome = stack
            .restart("Apache", &sleep)
            .expect("it is a configured service");
        assert_eq!(outcome, StartOutcome::Started);
        assert_eq!(host.started().len(), 1, "{:?}", host.started());
        assert_eq!(
            *pauses.lock().unwrap_or_else(poisoned),
            vec![SERVICE_RESTART_PAUSE],
            "the original's 500 milliseconds"
        );

        // Running: the restart stops it first, and the wait for its process to
        // be gone is what lets the start happen at all.
        assert!(eventually(|| stack
            .find("Apache")
            .expect("configured")
            .running()));
        let outcome = stack
            .restart("Apache", &sleep)
            .expect("it is a configured service");
        assert_eq!(outcome, StartOutcome::Started);
        assert_eq!(host.killed(), vec![4001], "the running service is stopped");
        assert_eq!(host.started().len(), 2, "{:?}", host.started());
        assert_eq!(
            *pauses.lock().unwrap_or_else(poisoned),
            vec![SERVICE_RESTART_PAUSE, SERVICE_RESTART_PAUSE]
        );
        let said = log_lines(&lines);
        assert!(
            said.contains(&"[Apache] stopping (pid 4001)...".to_owned()),
            "{said:?}"
        );
        assert!(
            said.contains(&"[Apache] started (pid 4001)".to_owned())
                && said.contains(&"[Apache] started (pid 4002)".to_owned()),
            "both starts are logged: {said:?}"
        );

        // A service with no engine has nothing to restart, and does not pause
        // for the privilege.
        let outcome = stack
            .restart("phpMyAdmin", &sleep)
            .expect("it is a configured service");
        assert_eq!(outcome, StartOutcome::Nothing);
        assert_eq!(
            pauses.lock().unwrap_or_else(poisoned).len(),
            2,
            "no pause without an engine"
        );

        // A name the configuration does not have is an error, not a silent
        // success.
        assert!(stack.restart("Nothing", &sleep).is_err());
    }
}
