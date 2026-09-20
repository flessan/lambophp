//! What the window does about it.
//!
//! Every [`Action`] has exactly one engine call behind it, and that is all this
//! module is: the `match` the window would otherwise have to be. The decisions
//! live where they already did - `ui_state` says which control a click was,
//! [`crate::state::Panel`] says what it means, and `lambo-core` is what actually
//! starts a service, installs a runtime, writes a virtual host or creates a
//! project.
//!
//! The pieces that are more than a call are here because they are *sequences*
//! the original also had: the stack buttons boot the essentials in order and
//! then open the page the pass finished on, a version switch installs the build
//! and then re-reads the document, and a project is created and then published.
//! Nothing in them decides anything `lambo` would answer differently - each step
//! is a core call, in the order the original made them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lambo_core::browser;
use lambo_core::console::{self, PsqlConsole};
use lambo_core::download::PanelDownloader;
use lambo_core::download_cache::DownloadCache;
use lambo_core::installer::Installer;
use lambo_core::panel::{CONFIG_FILE, PanelConfig};
use lambo_core::pathenv::{self, SystemEnvironment};
use lambo_core::platform::Os;
use lambo_core::process;
use lambo_core::session::{self, CatalogInstaller};
use lambo_core::stack::{EssentialRun, EssentialStep, LOCAL_URL, StartOutcome, StartStackStep};
use lambo_core::tray::{AutoStart, SystemRunKey};
use lambo_core::ui_state::{
    Page, PathAction, ProgressView, ProjectForm, SettingsAction, path_lines,
};

use crate::state::{Action, Panel};

/// What the window still has to do once the engine has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// Repaint, and carry on.
    Nothing,
    /// Stop the message loop.
    Quit,
}

/// Does what the user asked for.
///
/// The window calls this for every routed control and for every tray command,
/// and then repaints whatever it finds afterwards.
pub fn perform(panel: &mut Panel, action: Action) -> FollowUp {
    match action {
        Action::Nothing => {}
        Action::ShowPage(page) => {
            panel.set_page(page);
        }
        Action::SwitchTab(tab) => {
            panel.set_tab(tab);
        }
        Action::StartService(name) => start_service(panel, &name),
        Action::StopService(name) => stop_service(panel, &name),
        Action::RestartService(name) => restart_service(panel, &name),
        Action::SwitchVersion { name, version } => switch_version(panel, &name, &version),
        Action::OpenUrl(url) => open_url(panel, &url),
        Action::OpenTerminal(name) => open_terminal(panel, &name),
        Action::OpenEditor(path) => open_editor(panel, &path),
        Action::StartStack => start_stack(panel),
        Action::StartTrayStack => start_tray_stack(panel),
        Action::StopAll => stop_all(panel),
        Action::RestartStack => restart_stack(panel),
        Action::SetWebServer(choice) => set_web_server(panel, &choice),
        Action::EditorSelect(index) => select_editor_file(panel, index),
        Action::EditorSave => {
            panel.save_editor();
        }
        Action::EditorReload => {
            panel.reload_editor();
        }
        Action::VhostSave => save_vhost(panel),
        Action::VhostDelete => delete_vhost(panel),
        Action::VhostApply => apply_vhosts(panel),
        Action::OpenProjectUrl(url) => open_url(panel, &url),
        Action::OpenProjectFolder(path) => open_folder(panel, Path::new(&path)),
        Action::DeleteProject(name) => delete_project(panel, &name),
        Action::CreateProject => create_project(panel),
        Action::BrowseProjectFolder => browse_project_folder(panel),
        Action::AdoptProject => adopt_project(panel),
        Action::Settings(action) => return settings(panel, action),
    }
    FollowUp::Nothing
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// The installer an install-in-the-background goes through.
///
/// The same downloader, cache and plan the CLI's own installs use, so the panel
/// cannot end up with a different PHP than `lambo php install` would.
fn installer(panel: &Panel) -> Installer {
    let log = panel.log_fn();
    let cache = DownloadCache::new(
        panel.base_dir(),
        Arc::clone(&log),
        Box::new(PanelDownloader),
    );
    Installer::new(panel.base_dir(), log, cache, Box::new(PanelDownloader))
}

/// Starts one service, installing its component first when it is missing.
///
/// What each outcome means - and every message it produces - belongs to
/// [`lambo_core::stack::Stack::start_with_install`]; this only opens the URL a
/// component without an executable asks for, which is the one outcome that is
/// the interface's to perform.
fn start_service(panel: &mut Panel, name: &str) {
    panel.set_busy(true);
    let mut installer = CatalogInstaller::new(panel.base_dir(), panel.log_fn());
    let outcome = panel.stack().start_with_install(name, &mut installer);
    panel.set_busy(false);

    match outcome {
        Ok(StartOutcome::Opened(url)) => open_url(panel, &url),
        Ok(_) => {}
        Err(error) => panel.log(&format!("[{name}] {error}")),
    }
}

/// Stops one service.
fn stop_service(panel: &mut Panel, name: &str) {
    if let Err(error) = panel.stack().stop(name) {
        panel.log(&format!("[{name}] {error}"));
    }
}

/// Stops a service, pauses, and starts it again.
fn restart_service(panel: &mut Panel, name: &str) {
    panel.set_busy(true);
    let outcome = panel.stack().restart(name, &pause);
    panel.set_busy(false);

    match outcome {
        Ok(StartOutcome::Opened(url)) => open_url(panel, &url),
        Ok(_) => {}
        Err(error) => panel.log(&format!("[{name}] {error}")),
    }
}

/// Installs another build of a component and makes it the active one.
///
/// The install is the engine's, with the same progress reporting the downloads
/// use; the document is re-read afterwards because the switch is recorded in it,
/// which is what makes the choice survive a restart.
fn switch_version(panel: &mut Panel, name: &str, version: &str) {
    // The original stopped a running service before it swapped the build under
    // it, and said so in the log; a service that is not running - or that has no
    // engine at all - is left alone. Stopping is the engine's, like every other
    // stop here.
    if panel
        .stack()
        .find(name)
        .is_some_and(|service| service.running())
    {
        panel.log(&format!("[{name}] stopping before version switch"));
        let _ = panel.stack().stop(name);
    }

    panel.set_busy(true);
    panel.set_progress(ProgressView::new(
        lambo_core::download::Stage::Starting,
        name,
        0,
        0,
    ));

    let progress = panel.progress_fn();
    let outcome = installer(panel).set_active_variant(name, version, &progress);

    panel.set_busy(false);
    panel.set_progress(ProgressView::idle());

    match outcome {
        Ok(_) => {
            panel.log(&format!("[{name}] switched to {version}"));
            // The original recorded the choice in the document and saved it
            // before it refreshed the list, which is what makes the picker's
            // check mark - and the reinstall of a half-installed component -
            // follow the build that is actually active.
            record_active_version(panel, name, version);
            reload_document(panel);
        }
        Err(error) => panel.log(&format!("[{name}] switch {version} failed: {error}")),
    }
}

/// Writes a version switch into the installation's document.
///
/// The install has already succeeded by the time this runs, so a document that
/// does not know the service - the original's index bounds check - is simply
/// nothing to record; a write that fails is reported, because the reload that
/// follows would otherwise show a version the user did not choose.
fn record_active_version(panel: &mut Panel, name: &str, version: &str) {
    let recorded = match session::PanelBook::load(panel.base_dir()) {
        Ok(mut book) => book.set_active_version(name, version).map(|_| ()),
        Err(error) => Err(error),
    };
    if let Err(error) = recorded {
        panel.log(&format!("[{name}] config: {error}"));
    }
}

/// The tray's `Start &Stack`: the fixed list, and nothing else.
///
/// The original's tray command called `startService` for each of the four fixed
/// essentials - the card's own decision table, so a missing component is
/// installed and a URL is opened - and then stopped. No pause, no wait, no
/// browser: the login-launch path must not open a window nobody asked for.
fn start_tray_stack(panel: &mut Panel) {
    panel.set_busy(true);
    let mut installer = CatalogInstaller::new(panel.base_dir(), panel.log_fn());
    let steps = panel.stack().start_essential(&mut installer);
    panel.set_busy(false);

    for step in &steps {
        match step {
            EssentialStep::Started(name) => panel.log(&format!("[{name}] started")),
            EssentialStep::InstalledThenStarted(name) => {
                panel.log(&format!("[{name}] installed and started"));
            }
            EssentialStep::Installed(name) => panel.log(&format!("[{name}] installed")),
            EssentialStep::InstallFailed(name) => panel.log(&format!("[{name}] install failed")),
            EssentialStep::Skipped(name) => panel.log(&format!("[{name}] skipped")),
            EssentialStep::Nothing(name) => panel.log(&format!("[{name}] nothing to do")),
            EssentialStep::ExeMissing(name, path) => {
                panel.log(&format!("[{name}] exe missing: {}", path.display()));
            }
            EssentialStep::Failed(name, reason) => {
                panel.log(&format!("[{name}] start failed: {reason}"));
            }
            // A tool with nothing to run: the tray's pass opens nothing, which
            // is the difference from the page's.
            EssentialStep::Opened(name, url) => panel.log(&format!("[{name}] {url}")),
        }
    }
}

/// The page's `Start Stack`: the essentials, in the original's own order.
///
/// [`Stack::ensure_stack_essentials`] is the page's pass rather than the tray's,
/// and the difference is a real one - see its own documentation.
fn start_stack(panel: &mut Panel) {
    panel.set_busy(true);
    let run = essentials(panel);
    panel.set_busy(false);
    report_start_stack(panel, &run);

    // The original waited here and then opened the site, whatever the pass did.
    open_url(panel, LOCAL_URL);
}

/// The tray's `Stop All`, and the page's own.
fn stop_all(panel: &mut Panel) {
    panel.stack().stop_all();
    panel.stack().sweep();
    panel.log("all services stopped");
}

/// Stops everything, pauses, and boots the essentials again.
///
/// The original's own sequence, which is `Stack::restart_essentials` - the three
/// narration lines, `Stop All`, the sweep, the one-second pause and the pass.
/// Its result is reported with the same two helpers the other buttons use, so
/// all three read the same in the log.
fn restart_stack(panel: &mut Panel) {
    panel.set_busy(true);
    let mut installer = CatalogInstaller::new(panel.base_dir(), panel.log_fn());
    let run = panel
        .stack()
        .restart_essentials(panel.config(), &mut installer, &pause);
    panel.set_busy(false);

    report(panel, &run);
    if !run.open_url.is_empty() {
        open_url(panel, &run.open_url);
    }
}

/// Runs the page's essential pass with the panel's installer.
fn essentials(panel: &Panel) -> Vec<StartStackStep> {
    let mut installer = CatalogInstaller::new(panel.base_dir(), panel.log_fn());
    panel
        .stack()
        .ensure_stack_essentials(panel.config(), &mut installer, &pause)
}

/// Logs what the page's pass did, in the original's words.
fn report_start_stack(panel: &mut Panel, steps: &[StartStackStep]) {
    for step in steps {
        match step {
            StartStackStep::Installed(name) => panel.log(&format!("[{name}] installed")),
            StartStackStep::InstallFailed(name) => panel.log(&format!("[{name}] install failed")),
            StartStackStep::Skipped(name) => panel.log(&format!("[{name}] skipped")),
            StartStackStep::Started(name, outcome) => report_outcome(panel, name, outcome),
        }
    }
}

/// Logs one start, and opens the URL a component without an executable names.
fn report_outcome(panel: &mut Panel, name: &str, outcome: &StartOutcome) {
    match outcome {
        StartOutcome::Started | StartOutcome::InstalledThenStarted => {
            panel.log(&format!("[{name}] started"))
        }
        StartOutcome::Installed => panel.log(&format!("[{name}] installed")),
        StartOutcome::Opened(url) => {
            panel.log(&format!("[{name}] {url}"));
            open_url(panel, url);
        }
        StartOutcome::Nothing => panel.log(&format!("[{name}] nothing to do")),
        StartOutcome::ExeMissing(path) => {
            panel.log(&format!("[{name}] exe missing: {}", path.display()));
        }
        StartOutcome::Failed(reason) => panel.log(&format!("[{name}] start failed: {reason}")),
    }
}

/// Makes another web server the active one.
///
/// Three things move together, as they did in the original: the document's
/// setting is written, the server that owned the port is stopped - it cannot
/// share it - and the stack is rebuilt so the cards' active server follows.
fn set_web_server(panel: &mut Panel, choice: &str) {
    match session::PanelBook::load(panel.base_dir())
        .and_then(|mut book| book.set_active_web_server(choice))
    {
        Ok(()) => {
            panel.stack().stop_other_web_servers(choice);
            reload_document(panel);
            panel.log(&format!("active web server: {choice}"));
        }
        Err(error) => panel.log(&format!("web server: {error}")),
    }
}

/// Opens a terminal with a language's own binaries on `PATH`.
fn open_terminal(panel: &mut Panel, name: &str) {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let spec = console::terminal(panel.base_dir(), name, &inherited);
    if let Err(error) = process::spawn(&spec, Os::host()) {
        panel.log(&format!("[{name}] terminal: {error}"));
    }
}

/// Opens PostgreSQL's console, or says that there is none to open.
pub fn psql_console(panel: &mut Panel) {
    match console::psql_console(panel.base_dir()) {
        PsqlConsole::Ready(spec) => {
            if let Err(error) = process::spawn(&spec, Os::host()) {
                panel.log(&format!("psql console: {error}"));
            }
        }
        PsqlConsole::NotInstalled => panel.log(console::PSQL_MISSING),
    }
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// Creates the project the form describes.
///
/// The form is read by the engine, so a name that is only punctuation or a
/// framework that does not exist is refused with the same message `lambo
/// frameworks create` prints.
fn create_project(panel: &mut Panel) {
    let form = panel.project_form().clone();
    panel.set_busy(true);
    let created = session::create_project(
        panel.base_dir(),
        &form.framework,
        &form.name,
        &project_domain(&form),
        panel.log_fn(),
    );
    panel.set_busy(false);

    match created {
        Ok(created) => {
            reload_document(panel);
            open_url(panel, &format!("http://{}", created.domain));
        }
        Err(error) => panel.log(&format!("projects: {error}")),
    }
}

/// Opens the folder picker over the projects page, and records the choice.
///
/// Cancelling keeps the location field as it is; choosing replaces it. The
/// load itself is a second press - `Open Project` - so a wrong pick costs
/// nothing.
fn browse_project_folder(panel: &mut Panel) {
    if let Some(folder) = crate::win32::pick_folder() {
        panel.set_project_location(folder.display().to_string());
    }
}

/// Loads the folder the location field names as a project.
///
/// Nothing is scaffolded: the folder is detected the way `lambo init` detects
/// it, registered with a domain of its own and published to the hosts file,
/// and the row the page lists is selected so its buttons act on it.
fn adopt_project(panel: &mut Panel) {
    let location = panel.project_location().trim().to_owned();
    if location.is_empty() {
        panel.log("projects: pick a folder first - Browse… opens the picker");
        return;
    }
    panel.set_busy(true);
    let adopted = session::adopt_project(panel.base_dir(), Path::new(&location), panel.log_fn());
    panel.set_busy(false);

    match adopted {
        Ok(adopted) => {
            reload_document(panel);
            let index = panel
                .projects()
                .iter()
                .position(|project| project.name == adopted.name);
            panel.select_project(index);
            open_url(panel, &format!("http://{}", adopted.domain));
        }
        Err(error) => panel.log(&format!("projects: {error}")),
    }
}

/// Deletes a project, its directory and its domains.
fn delete_project(panel: &mut Panel, name: &str) {
    panel.set_busy(true);
    let deleted = session::delete_project(panel.base_dir(), name, panel.log_fn());
    panel.set_busy(false);

    match deleted {
        Ok(true) => {
            panel.select_project(None);
            reload_document(panel);
        }
        Ok(false) => panel.log(&format!("projects: no project named {name}")),
        Err(error) => panel.log(&format!("projects: {error}")),
    }
}

// ---------------------------------------------------------------------------
// Virtual hosts and the editor
// ---------------------------------------------------------------------------

/// Saves the virtual-host form.
fn save_vhost(panel: &mut Panel) {
    let form = panel.vhost_form().clone();
    let current = panel.vhost_current().map(str::to_owned);
    // A refusal is logged by the engine itself, so there is nothing to add.
    if let Ok(vhost) =
        session::save_vhost(panel.base_dir(), current.as_deref(), &form, panel.log_fn())
    {
        panel.log(&format!("vhost saved: {}", vhost.domain));
        panel.edit_vhost(None);
        reload_document(panel);
    }
}

/// Deletes the virtual host the table has selected.
fn delete_vhost(panel: &mut Panel) {
    let Some(domain) = panel.vhost_to_delete() else {
        panel.log("vhost delete: select a row first");
        return;
    };
    match session::delete_vhost(panel.base_dir(), &domain, panel.log_fn()) {
        Ok(true) => {
            panel.edit_vhost(None);
            reload_document(panel);
        }
        Ok(false) => panel.log(&format!("vhost delete: {domain} is not in the document")),
        Err(_) => {}
    }
}

/// Publishes the document: the hosts file and the server configurations.
fn apply_vhosts(panel: &mut Panel) {
    if let Err(_error) = session::apply_vhosts(panel.base_dir(), panel.log_fn()) {
        // `apply_vhosts` logs the failure and what to do about it.
    }
}

/// Loads a file in the editor and shows the page.
fn open_editor(panel: &mut Panel, path: &Path) {
    let line = panel.load_editor_file(path);
    panel.log(&line);
    panel.set_page(Page::Editor);
}

/// Shows the file at this index of the editor's dropdown.
fn select_editor_file(panel: &mut Panel, index: usize) {
    if let Some(line) = panel.select_editor_file(index) {
        panel.log(&line);
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The settings page's buttons.
/// The log lines a `PATH` change leaves behind.
///
/// The original's five messages are [`lambo_core::ui_state::path_lines`]'s, so
/// what the settings page says lives with the rest of the panel's text rather
/// than here; this only turns an outcome into them.
fn path_report(action: PathAction, outcome: Result<usize, lambo_core::Error>) -> Vec<String> {
    match outcome {
        Ok(changed) => path_lines(action, changed, None),
        Err(error) => path_lines(action, 0, Some(&error.to_string())),
    }
}

fn settings(panel: &mut Panel, action: SettingsAction) -> FollowUp {
    match action {
        // The installation's configuration, in the editor the panel already
        // has: the original's `Edit config` opened the same file.
        SettingsAction::EditConfig => {
            let path = panel.base_dir().join(CONFIG_FILE);
            open_editor(panel, &path);
        }
        SettingsAction::ReloadConfig => {
            reload_document(panel);
            panel.log("configuration reloaded");
        }
        SettingsAction::ToggleAutoStart => {
            let setting = auto_start();
            setting.toggle(&panel.log_fn());
            panel.set_auto_start(setting.is_enabled());
        }
        SettingsAction::RestartAsAdmin => {
            // The original's handler, whole: a relaunch that fails is logged and
            // the running instance stays; one that succeeds says so, gives the
            // replacement a moment to appear and then exits, which is what
            // `FollowUp::Quit` does.
            if let Err(error) = pathenv::relaunch_elevated() {
                panel.log(&format!("elevate: {error}"));
                return FollowUp::Nothing;
            }
            panel.log("relaunching as administrator — this instance will exit");
            std::thread::sleep(Duration::from_millis(200));
            return FollowUp::Quit;
        }
        SettingsAction::AddToPath => {
            let appdata = std::env::var("APPDATA").ok();
            let outcome =
                pathenv::add_to_user_path(panel.base_dir(), appdata.as_deref(), &SystemEnvironment);
            for line in path_report(PathAction::Add, outcome) {
                panel.log(&line);
            }
        }
        SettingsAction::RemoveFromPath => {
            let outcome = pathenv::remove_from_user_path(panel.base_dir(), &SystemEnvironment);
            for line in path_report(PathAction::Remove, outcome) {
                panel.log(&line);
            }
        }
        SettingsAction::PsqlConsole => psql_console(panel),
        // The original's GitHub button opened the project page; ours opens
        // this product's. The address comes from the package metadata, so a
        // fork that changes `repository` gets a working button for free.
        SettingsAction::OpenRepository => {
            let url = crate::state::REPOSITORY;
            if url.is_empty() {
                panel.log("repository: no address is compiled into this build");
            } else if let Err(error) = browser::open(url, Os::host()) {
                panel.log(&format!("repository: {error}"));
            }
        }
        SettingsAction::Quit => return quit(panel),
    }
    FollowUp::Nothing
}

/// Stops everything and asks the window to close.
///
/// The original's `Quit`, in both places it appears: the services it started are
/// the services it stops, and then it exits.
fn quit(panel: &mut Panel) -> FollowUp {
    panel.log("stopping all services...");
    panel.stack().stop_all();
    panel.stack().sweep();
    FollowUp::Quit
}

/// The "start with Windows" setting of this build.
fn auto_start() -> AutoStart<SystemRunKey> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lambo-gui.exe"));
    AutoStart::new(SystemRunKey, exe)
}

/// Whether a login launch is set up.
///
/// The window asks this once, when it opens, so the tray's menu and the settings
/// page show the state the registry is actually in rather than the state this
/// process last wrote.
pub fn auto_start_enabled() -> bool {
    auto_start().is_enabled()
}

// ---------------------------------------------------------------------------
// Shared steps
// ---------------------------------------------------------------------------

/// The browser, with the engine deciding whether the URL is one it will open.
fn open_url(panel: &mut Panel, url: &str) {
    if let Err(error) = browser::open(url, Os::host()) {
        panel.log(&format!("{error}"));
    }
}

/// The file manager, on a directory that may not exist yet.
fn open_folder(panel: &mut Panel, path: &Path) {
    if let Err(error) = browser::open_folder(path, Os::host()) {
        panel.log(&format!("{}: {error}", path.display()));
    }
}

/// Re-reads the document, and rebuilds the services that run from it.
///
/// A failed read leaves the panels as they were rather than emptying them: the
/// document is on disk and the user's session should survive a transient read
/// error, which is what the engine's own message in the log is for.
fn reload_document(panel: &mut Panel) {
    match PanelConfig::load(panel.base_dir()) {
        Ok(config) => {
            panel.set_document(config);
            panel.rebuild_stack();
        }
        Err(error) => panel.log(&format!("configuration: {error}")),
    }
}

/// Logs what a stack pass did, in the original's words.
fn report(panel: &mut Panel, run: &EssentialRun) {
    for step in &run.steps {
        match step {
            EssentialStep::Started(name) => panel.log(&format!("[{name}] started")),
            EssentialStep::Skipped(name) => panel.log(&format!("[{name}] skipped")),
            EssentialStep::Installed(name) => panel.log(&format!("[{name}] installed")),
            EssentialStep::InstalledThenStarted(name) => {
                panel.log(&format!("[{name}] installed and started"));
            }
            EssentialStep::InstallFailed(name) => panel.log(&format!("[{name}] install failed")),
            EssentialStep::Opened(name, url) => panel.log(&format!("[{name}] {url}")),
            EssentialStep::Nothing(name) => panel.log(&format!("[{name}] nothing to do")),
            EssentialStep::ExeMissing(name, path) => {
                panel.log(&format!("[{name}] exe missing: {}", path.display()));
            }
            EssentialStep::Failed(name, reason) => {
                panel.log(&format!("[{name}] start failed: {reason}"));
            }
        }
    }
}

/// The pause the stack buttons take between services.
fn pause(duration: Duration) {
    std::thread::sleep(duration);
}

/// The domain the projects page asked for, as `lambo frameworks create` reads
/// it: a name with no extension becomes the project's own `.test` name.
fn project_domain(form: &ProjectForm) -> String {
    let name = form.domain_name.trim();
    if name.is_empty() {
        return String::new();
    }
    format!("{name}{}", form.extension)
}
