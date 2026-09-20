//! The panel's own state, and what each control means.
//!
//! `ui_state` builds the controls and `view.rs` says how they look; this is what
//! stands between them and the window: which page is open, what the forms hold,
//! which file the editor is on, where the progress bar has got to - and, for a
//! control the user pressed, which [`Action`] that is.
//!
//! It is deliberately not Win32. The window owns a [`Panel`], asks it for the
//! widgets of the current page, routes a `WM_COMMAND` through [`Panel::route`],
//! and then *performs* the action it gets back by calling the engine - so every
//! decision about what a click means is here, tested, and the unverifiable half
//! is one `match` over an enum with the engine's own calls in it.
//!
//! # No business logic
//!
//! This type decides what the user asked for, never what the answer is. Starting
//! a service, installing one, saving a virtual host, creating a project: all of
//! those are the engine's, and every branch that could disagree with `lambo` is
//! asked of the engine instead - [`configure_action`] for the configure button,
//! the stack for whether a card reads Start or Stop, the catalogue for what a
//! version menu entry installs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lambo_core::catalog_panel;
use lambo_core::logs::LogFn;
use lambo_core::panel::{PanelConfig, PanelProject, Vhost};
use lambo_core::service::HostService;
use lambo_core::stack::{ManagedService, Stack};
use lambo_core::ui_state::{
    CardPart, ConfigureAction, Editor, Layout, LogBuffer, Page, ProgressView, ProjectActionId,
    ProjectForm, SettingsAction, SettingsInput, SettingsView, WEB_SERVERS, Widget, WidgetId,
    configure_action, editor_files, editor_widgets, footer_widgets, landing_widgets,
    projects_widgets, services_widgets, settings_view, settings_widgets, sidebar_widgets,
    tab_click_changes, vhost_list, vhosts_widgets,
};
use lambo_core::vhost::VhostForm;

/// A log closure over a buffer and a clock.
///
/// The one place a `LogFn` is built, so the stack's log and the panel's log
/// cannot be wired up differently - which is the bug a second copy would
/// introduce.
fn log_sink(log: Arc<Mutex<LogBuffer>>, clock: Clock) -> LogFn {
    Arc::new(move |line: &str| {
        if let Ok(mut buffer) = log.lock() {
            buffer.push_at(&clock(), line);
        }
    })
}

/// The extension a picker's index names.
///
/// The items come from [`lambo_core::frameworks::DOMAIN_EXTENSIONS`], which is what
/// the picker was filled from, so an index past the end is the first entry
/// rather than nothing: a control can only report an index it has.
fn extension_at(index: usize) -> String {
    lambo_core::frameworks::DOMAIN_EXTENSIONS
        .get(index)
        .copied()
        .unwrap_or(lambo_core::frameworks::DOMAIN_EXTENSIONS[0])
        .to_owned()
}

/// The server type a picker's index names.
fn server_at(index: usize) -> String {
    lambo_core::ui_state::SERVER_OPTIONS
        .get(index)
        .copied()
        .unwrap_or(lambo_core::ui_state::SERVER_OPTIONS[0])
        .to_owned()
}

/// The framework a picker's index names.
fn framework_at(index: usize) -> String {
    lambo_core::ui_state::framework_names()
        .get(index)
        .copied()
        .unwrap_or_default()
        .to_owned()
}

/// Where the log's timestamps come from.
///
/// Every line the panel shows is stamped `HH:MM:SS`, and the clock that reads
/// them is the interface's: `std` has no local time, Windows does, and a test
/// has whichever one it wants. The panel formats what it is given -
/// [`lambo_core::ui_state::time_of_day`] - and asks for it at the moment a line
/// is written rather than holding a time that would go stale.
pub type Clock = Arc<dyn Fn() -> String + Send + Sync + 'static>;

/// This build's version, as the status bar and the settings page show it.
pub const VERSION: &str = match option_env!("CARGO_PKG_VERSION") {
    Some(version) => version,
    None => "0.0.0",
};

/// Where this product's source lives, as the settings page shows it.
pub const REPOSITORY: &str = match option_env!("CARGO_PKG_REPOSITORY") {
    Some(repository) => repository,
    None => "",
};

/// This project's site, as the settings page shows it.
pub const HOMEPAGE: &str = match option_env!("CARGO_PKG_HOMEPAGE") {
    Some(homepage) => homepage,
    None => "",
};

/// What the user asked for when they pressed something.
///
/// Every variant names an engine operation, and nothing else: the window's job
/// is to call the one it is handed. `Nothing` is a real answer - a control that
/// does nothing at all, like a service with no configuration file, no URL and no
/// terminal - rather than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do, and nothing to say.
    Nothing,
    /// Open another page.
    ShowPage(Page),
    /// Show the services of another tab.
    SwitchTab(usize),
    /// Start the service, installing its component first when it is missing.
    StartService(String),
    /// Stop the service.
    StopService(String),
    /// Stop the service, pause, and start it again.
    RestartService(String),
    /// Install another version of a component, and make it the active one.
    SwitchVersion {
        /// The component.
        name: String,
        /// The version as the catalogue names it.
        version: String,
    },
    /// Open a URL in the browser.
    OpenUrl(String),
    /// Open a terminal with the service's binaries on `PATH`.
    OpenTerminal(String),
    /// Load a file in the editor, and show the editor.
    OpenEditor(PathBuf),
    /// Start the essential stack, from the services page.
    ///
    /// The page's pass, which is [`Stack::ensure_stack_essentials`]'s: the
    /// enabled web server in place of Apache, a missing component installed, a
    /// disabled service skipped, a pause between services, and the site opened
    /// when it is done.
    StartStack,
    /// Start the essential stack, from the tray.
    ///
    /// The tray's pass, which is [`Stack::start_essential`]'s: the *fixed*
    /// essential list, no pause, no wait, and nothing opened. The previous
    /// implementation's tray had
    /// its own command and its own function, and they behaved differently from
    /// the page's button - so this is a second action rather than the same one.
    StartTrayStack,
    /// Stop everything Lambo started.
    StopAll,
    /// Stop everything, pause, and start the essentials again.
    RestartStack,
    /// Make another web server the active one.
    SetWebServer(String),
    /// Show the file at this index of the editor's dropdown.
    EditorSelect(usize),
    /// Write the editor's buffer to its file.
    EditorSave,
    /// Re-read the editor's file from disk.
    EditorReload,
    /// Save the virtual-host form.
    VhostSave,
    /// Delete the selected virtual host.
    VhostDelete,
    /// Publish the document to the hosts file and the server configurations.
    VhostApply,
    /// Open a project's URL.
    OpenProjectUrl(String),
    /// Open a project's directory.
    OpenProjectFolder(String),
    /// Delete a project.
    DeleteProject(String),
    /// Create the project the form describes.
    CreateProject,
    /// Open the folder picker over the projects page's location field.
    BrowseProjectFolder,
    /// Load the project the location field names, detecting what it is.
    AdoptProject,
    /// One of the settings page's actions.
    Settings(SettingsAction),
}

/// A handle the window holds open: the stack, the document, and what is on
/// screen right now.
pub struct Panel {
    /// The installation.
    base_dir: PathBuf,
    /// The document: its settings, and the projects and virtual hosts as loaded.
    config: PanelConfig,
    /// The services, and the engine that supervises them.
    stack: Stack,
    /// The current page.
    page: Page,
    /// The tab the services page is on.
    tab: usize,
    /// The editor page's state.
    editor: Editor,
    /// The projects page's form.
    project_form: ProjectForm,
    /// The virtual-hosts page's form.
    vhost_form: VhostForm,
    /// The domain of the virtual host the form is editing, if it is editing one.
    vhost_current: Option<String>,
    /// The project a row button would act on, if one is selected.
    project_selected: Option<String>,
    /// The folder the projects page would load a project from.
    project_location: String,
    /// Whether this process is running as administrator.
    elevated: bool,
    /// Whether Windows starts Lambo at login.
    auto_start: bool,
    /// Whether an install or a creation is in flight.
    busy: bool,
    /// The services' states as the last [`Panel::poll`] saw them, one entry per
    /// service of the stack.
    ///
    /// Empty until the first poll, which is what keeps a freshly opened window
    /// from logging a state line for every service it already had.
    states: Vec<bool>,
    /// How long the log was when the window last looked.
    ///
    /// A timer that repainted unconditionally would redraw a still window twice
    /// a second; comparing this - and the strip's position - is how it knows it
    /// has nothing to do.
    seen_log: usize,
    seen_progress: (String, u16),
    /// The log panel's buffer, shared with the engine's own log callbacks.
    log: Arc<Mutex<LogBuffer>>,
    /// Where a line's timestamp comes from.
    clock: Clock,
    /// The progress strip, shared with the installer's progress callbacks.
    progress: Arc<Mutex<ProgressView>>,
}

impl Panel {
    /// The panel for an installation.
    ///
    /// The services come from the stack that runs them rather than from the
    /// document, so a card is drawn from what is actually configured *and*
    /// supervised - the same list the engine works from.
    pub fn new(base_dir: &Path, config: PanelConfig, stack: Stack, clock: Clock) -> Self {
        let editor = Editor::open(base_dir, &config, stack.services());
        Self {
            base_dir: base_dir.to_path_buf(),
            config,
            stack,
            page: Page::Landing,
            tab: 0,
            editor,
            project_form: ProjectForm::default(),
            vhost_form: VhostForm::blank(),
            vhost_current: None,
            project_selected: None,
            project_location: String::new(),
            elevated: false,
            auto_start: false,
            busy: false,
            log: Arc::new(Mutex::new(LogBuffer::default())),
            clock,
            progress: Arc::new(Mutex::new(ProgressView::idle())),
            states: Vec::new(),
            seen_log: 0,
            seen_progress: (String::new(), 0),
        }
    }

    /// The panel for an installation, with the stack built here.
    ///
    /// [`Panel::new`] takes the stack because a caller may already have one; the
    /// window does not, and this is the wiring it needs: the stack's log and the
    /// panel's log are the *same* buffer, so a line the engine writes while it
    /// starts a service or unpacks a download is the line the log panel shows. A
    /// second buffer would be a copy, and a copy is where the two would first
    /// disagree.
    pub fn open(base_dir: &Path, config: PanelConfig, clock: Clock) -> Self {
        let log: Arc<Mutex<LogBuffer>> = Arc::new(Mutex::new(LogBuffer::default()));
        let stack = Stack::build(
            base_dir,
            &config,
            Arc::new(HostService::new()),
            log_sink(Arc::clone(&log), Arc::clone(&clock)),
        );
        let editor = Editor::open(base_dir, &config, stack.services());
        Self {
            base_dir: base_dir.to_path_buf(),
            config,
            stack,
            page: Page::Landing,
            tab: 0,
            editor,
            project_form: ProjectForm::default(),
            vhost_form: VhostForm::blank(),
            vhost_current: None,
            project_selected: None,
            project_location: String::new(),
            elevated: false,
            auto_start: false,
            busy: false,
            states: Vec::new(),
            seen_log: 0,
            seen_progress: (String::new(), 0),
            log,
            clock,
            progress: Arc::new(Mutex::new(ProgressView::idle())),
        }
    }

    /// The installation.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// The document, as the settings page and the card grid read it.
    pub fn config(&self) -> &PanelConfig {
        &self.config
    }

    /// The services, and the engine that supervises them.
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// The page on screen.
    pub fn page(&self) -> Page {
        self.page
    }

    /// The tab the services page is on.
    pub fn tab(&self) -> usize {
        self.tab
    }

    /// The editor's state.
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// The virtual-hosts page's form.
    pub fn vhost_form(&self) -> &VhostForm {
        &self.vhost_form
    }

    /// The projects page's form.
    /// The folder the projects page would load a project from.
    pub fn project_location(&self) -> &str {
        &self.project_location
    }

    /// Records the folder a project is loaded from.
    pub fn set_project_location(&mut self, location: String) {
        self.project_location = location;
    }

    pub fn project_form(&self) -> &ProjectForm {
        &self.project_form
    }

    /// The projects the page lists.
    pub fn projects(&self) -> &[PanelProject] {
        &self.config.projects
    }

    /// The virtual hosts the page lists.
    pub fn vhosts(&self) -> &[Vhost] {
        &self.config.vhosts
    }

    /// The domain of the virtual host the form is editing.
    pub fn vhost_current(&self) -> Option<&str> {
        self.vhost_current.as_deref()
    }

    /// The project a row button acts on.
    pub fn project_selected(&self) -> Option<&str> {
        self.project_selected.as_deref()
    }

    /// The current time, as the log's lines are stamped with it.
    ///
    /// The panel asks the clock it was given rather than reading one itself:
    /// `std` has no local time, so the interface supplies what the platform has,
    /// and a test supplies something it can assert on.
    pub fn timestamp(&self) -> String {
        (self.clock)()
    }

    /// The window's creation pass: the banner, then the original's start-up.
    ///
    /// The previous implementation did this inside its `WmCreate`: the three
    /// lines the log opens
    /// with, Apache's own runtime files when Apache is installed, and the
    /// services `settings.auto_start` names - each of them a core call, in the
    /// original's order. Everything the pass *decides* is the engine's: which
    /// lines the log opens with is [`lambo_core::ui_state::startup_lines`], whether
    /// Apache is there is its own executable, what the self-heal rewrites is
    /// [`lambo_core::postinstall::ensure_apache_runtime_files`], and the auto-start
    /// pass is [`Stack::auto_start`], which logs its own failures.
    pub fn startup(&mut self) {
        let started_at = self.timestamp();
        let lines = lambo_core::ui_state::startup_lines(
            &self.base_dir,
            self.config.services.len(),
            self.config.vhosts.len(),
            &started_at,
        );
        for line in lines {
            self.log(&line);
        }

        // The original checked for the executable before seeding anything, so a
        // user who has not installed Apache sees nothing at all.
        let httpd = self
            .base_dir
            .join("bin")
            .join("apache")
            .join("bin")
            .join("httpd.exe");
        if httpd.is_file() {
            let log = self.log_fn();
            if let Err(error) =
                lambo_core::postinstall::ensure_apache_runtime_files(&self.base_dir, &log)
            {
                self.log(&format!("  self-heal: {error}"));
            }
        }

        // The document's own list, in its order. A service the stack has no
        // engine for is skipped by `auto_start` itself.
        let names = self.config.settings.auto_start.clone();
        if !names.is_empty() {
            self.stack().auto_start(&names);
        }
    }

    /// Whether this process is running as administrator, as the settings page
    /// and the `Restart as Admin` slot read it.
    pub fn elevated(&self) -> bool {
        self.elevated
    }

    /// Whether Windows starts Lambo at login, as the tray's check mark reads it.
    pub fn auto_start(&self) -> bool {
        self.auto_start
    }

    /// Whether an install or a creation is in flight.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// The log panel's text.
    pub fn log_text(&self) -> String {
        self.log
            .lock()
            .map(|log| log.text().to_owned())
            .unwrap_or_default()
    }

    /// How much log text there is, without copying it.
    ///
    /// The timer asks this twice a second, and the buffer can hold a quarter of
    /// a megabyte; comparing a length is what keeps the refresh rule from
    /// cloning it to find out nothing changed.
    pub fn log_len(&self) -> usize {
        self.log.lock().map(|log| log.len()).unwrap_or_default()
    }

    /// The progress strip.
    pub fn progress(&self) -> ProgressView {
        self.progress
            .lock()
            .map(|progress| progress.clone())
            .unwrap_or_else(|_| ProgressView::idle())
    }

    /// A log callback the engine can write through.
    ///
    /// The engine's calls take a `LogFn`, so the buffer is shared rather than
    /// copied: a line the installer writes and a line the page writes land in
    /// the same panel, in the order they happened.
    pub fn log_fn(&self) -> LogFn {
        log_sink(Arc::clone(&self.log), Arc::clone(&self.clock))
    }

    /// The strip's callback, for an install's progress.
    pub fn progress_fn(&self) -> lambo_core::download::ProgressFn {
        let progress = Arc::clone(&self.progress);
        Arc::new(move |stage, name: &str, done: i64, total: i64| {
            if let Ok(mut view) = progress.lock() {
                *view = ProgressView::new(stage, name, done, total);
            }
        })
    }

    /// Writes a line to the log panel.
    pub fn log(&self, line: &str) {
        if let Ok(mut buffer) = self.log.lock() {
            buffer.push_at(&(self.clock)(), line);
        }
    }

    /// Sets the progress strip, which is how a pass that is not a download
    /// reports itself.
    pub fn set_progress(&self, view: ProgressView) {
        if let Ok(mut progress) = self.progress.lock() {
            *progress = view;
        }
    }

    /// Marks whether an install or a creation is in flight.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Records whether this process is elevated, which the settings page shows.
    pub fn set_elevated(&mut self, elevated: bool) {
        self.elevated = elevated;
    }

    /// Records whether Windows starts Lambo at login.
    pub fn set_auto_start(&mut self, enabled: bool) {
        self.auto_start = enabled;
    }

    /// Replaces the document, after the engine has changed it.
    pub fn set_document(&mut self, config: PanelConfig) {
        self.config = config;
        self.refresh_editor();
    }

    /// Opens a page, answering whether it changed.
    ///
    /// The editor's file list is rebuilt on the way in: services are installed
    /// and uninstalled while the panel is open, and a configuration file that
    /// appeared since the last visit has to be offered.
    pub fn set_page(&mut self, page: Page) -> bool {
        if page == self.page {
            return false;
        }
        self.page = page;
        if page == Page::Editor {
            self.refresh_editor();
        }
        true
    }

    /// Shows a tab, answering whether it changed, which is the original's test
    /// before it moved any cards.
    pub fn set_tab(&mut self, tab: usize) -> bool {
        if !tab_click_changes(tab, self.tab) {
            return false;
        }
        self.tab = tab;
        true
    }

    /// Rebuilds the editor's dropdown from the installation as it is now.
    ///
    /// The buffer is untouched: a half-written configuration file is not worth
    /// losing because a service was installed on another page.
    pub fn refresh_editor(&mut self) {
        self.editor.files = editor_files(&self.base_dir, &self.config, self.stack.services());
        if let Some(index) = self.editor.selected {
            if index >= self.editor.files.len() {
                self.editor.selected = None;
            }
        }
    }

    /// Records that the editor's buffer changed.
    pub fn set_editor_text(&mut self, text: String) {
        self.editor.set_text(text);
    }

    /// Writes the editor's buffer to the file it came from.
    ///
    /// The engine does the writing - atomically, and refusing when nothing is
    /// loaded - and the line it returns is the one the log panel shows.
    pub fn save_editor(&mut self) -> String {
        let line = self.editor.save();
        self.log(&line);
        line
    }

    /// Re-reads the file the editor has open.
    ///
    /// A reload is a load of the file that is already there, which is what the
    /// original's `Reload` did: it read whatever is on disk *now* and discarded
    /// the buffer, which is the point of the button beside `Save`.
    pub fn reload_editor(&mut self) -> String {
        let Some(path) = self.editor.loaded.clone() else {
            return "editor: no file loaded".to_owned();
        };
        let line = self.editor.load(&path);
        self.log(&line);
        line
    }

    /// Records what a text field now holds.
    ///
    /// The window creates the controls and the user types into them; the *form*
    /// is the model, and it is what the engine reads when the page's button is
    /// pressed. Nothing is validated here - a field holds what was typed, and
    /// the engine's own reader is what refuses it.
    pub fn set_edit(&mut self, id: WidgetId, text: String) {
        match id {
            WidgetId::VhostDomainName => self.vhost_form.name = text,
            WidgetId::VhostPort => self.vhost_form.port = text,
            WidgetId::VhostDocroot => self.vhost_form.docroot = text,
            WidgetId::ProjectName => self.project_form.name = text,
            WidgetId::ProjectDomainName => self.project_form.domain_name = text,
            WidgetId::ProjectLocation => self.project_location = text,
            WidgetId::EditorText => self.editor.set_text(text),
            _ => {}
        }
    }

    /// Records what a drop-down now holds, and says what that asks for.
    ///
    /// Most of these are choices a form remembers until its button is pressed -
    /// an extension, a server type, a framework. One of them *is* the action:
    /// picking a web server publishes the choice through the engine, and picking
    /// an editor file loads it.
    pub fn select_combo(&mut self, id: WidgetId, index: usize) -> Option<Action> {
        match id {
            WidgetId::WebPicker => WEB_SERVERS
                .get(index)
                .map(|name| Action::SetWebServer((*name).to_owned())),
            WidgetId::VhostDomainExt => {
                self.vhost_form.extension = extension_at(index);
                None
            }
            WidgetId::VhostServer => {
                self.vhost_form.server = server_at(index);
                None
            }
            WidgetId::ProjectFramework => {
                self.project_form.framework = framework_at(index);
                None
            }
            WidgetId::ProjectDomainExt => {
                self.project_form.extension = extension_at(index);
                None
            }
            WidgetId::EditorFile => Some(Action::EditorSelect(index)),
            _ => None,
        }
    }

    /// Rebuilds the stack from the document the panel holds.
    ///
    /// A component that was installed, a version that was switched, a setting
    /// that changed: the services the pages run come from the document, so a
    /// change to it is followed by this. Engines that are already running are
    /// carried over by [`Stack::reload`], so a rebuild never restarts anything.
    pub fn rebuild_stack(&mut self) {
        let log = self.log_fn();
        let stack = Stack::build(
            &self.base_dir,
            &self.config,
            Arc::new(HostService::new()),
            log,
        );
        self.stack = stack;
        self.states.clear();
        self.refresh_editor();
    }

    /// Re-reads the services' states, and reports whether anything moved.
    ///
    /// This is the window's refresh rule: the engine has no background threads,
    /// so the timer asks. A service that started, stopped or died between two
    /// ticks is logged once - including the one that died on its own, which is
    /// the line a user needs and no button produces - and the log growing or the
    /// strip moving is a reason to repaint as well.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;

        let states: Vec<bool> = self
            .stack
            .services()
            .iter()
            .map(|service| service.running())
            .collect();

        if !self.states.is_empty() {
            for (index, running) in states.iter().enumerate() {
                if self.states.get(index) == Some(running) {
                    continue;
                }
                if let Some(service) = self.stack.services().get(index) {
                    let name = service.name().to_owned();
                    let word = if *running { "running" } else { "stopped" };
                    self.log(&format!("[{name}] {word}"));
                }
                changed = true;
            }
        }
        self.states = states;

        let length = self.log_len();
        if length != self.seen_log {
            self.seen_log = length;
            changed = true;
        }

        let progress = self.progress();
        let seen = (progress.label.clone(), progress.position);
        if seen != self.seen_progress {
            self.seen_progress = seen;
            changed = true;
        }

        changed
    }

    /// Shows the file at this index of the dropdown, loading it.
    pub fn select_editor_file(&mut self, index: usize) -> Option<String> {
        self.editor.select(index)
    }

    /// Loads a file in the editor, selecting it in the dropdown when it is one
    /// of the files offered.
    pub fn load_editor_file(&mut self, path: &Path) -> String {
        if let Some(index) = self.editor.files.iter().position(|file| file.path == path) {
            self.editor.selected = Some(index);
        }
        self.editor.load(path)
    }

    /// Points the virtual-hosts form at a row, or at nothing for a new host.
    pub fn edit_vhost(&mut self, index: Option<usize>) {
        match index.and_then(|index| self.config.vhosts.get(index)) {
            Some(vhost) => {
                self.vhost_form = VhostForm::from_vhost(vhost);
                self.vhost_current = Some(vhost.domain.clone());
            }
            None => {
                self.vhost_form = VhostForm::blank();
                self.vhost_current = None;
            }
        }
    }

    /// The row the delete button acts on.
    ///
    /// Deleting is a row's action, not the form's: the table's selection is what
    /// the button beside it works on, which is what the original did - and the
    /// form is free to hold a half-typed domain while it does.
    pub fn vhost_to_delete(&self) -> Option<String> {
        self.vhost_current.clone()
    }

    /// Selects a project row, for the buttons beside the table.
    pub fn select_project(&mut self, index: Option<usize>) {
        self.project_selected = index
            .and_then(|index| self.config.projects.get(index))
            .map(|project| project.name.clone());
    }

    /// The settings page, as the engine describes this installation.
    pub fn settings(&self) -> SettingsView {
        settings_view(SettingsInput {
            base_dir: &self.base_dir,
            config: &self.config,
            elevated: self.elevated,
            auto_start: self.auto_start,
            version: VERSION,
            repository: REPOSITORY,
            homepage: HOMEPAGE,
        })
    }

    /// The projects page's runtime line.
    pub fn runtime_status(&self) -> String {
        lambo_core::frameworks::runtime_status_text(&self.base_dir)
    }

    /// The virtual hosts as the table shows them.
    pub fn vhost_rows(&self) -> Vec<lambo_core::vhost::VhostRow> {
        vhost_list(&self.config.vhosts, &self.base_dir)
    }

    /// Every control of the current page, plus the sidebar and the footer.
    ///
    /// The cards of the services page are built for *every* service, in the
    /// stack's order: which of them the tab shows is the window's business,
    /// because hiding a card is moving a control and not drawing a new one - see
    /// [`lambo_core::ui_state::card_visibility`] and
    /// [`lambo_core::ui_state::card_positions`].
    pub fn widgets(&self) -> Vec<Widget> {
        let layout = Layout::compute(self.stack.services().len());
        let mut widgets = sidebar_widgets(self.page);

        match self.page {
            Page::Landing => widgets.extend(landing_widgets()),
            Page::Services => widgets.extend(services_widgets(
                self.stack.services(),
                &self.config.settings.active_web_server,
                &layout,
                self.tab,
            )),
            Page::Editor => widgets.extend(editor_widgets(&self.editor, &layout)),
            Page::Vhosts => widgets.extend(vhosts_widgets(&self.vhost_form, &self.vhost_rows())),
            Page::Projects => widgets.extend(projects_widgets(
                &self.config.projects,
                &self.project_form,
                &self.runtime_status(),
                &self.project_location,
            )),
            Page::Settings => widgets.extend(settings_widgets(&self.settings())),
        }

        widgets.extend(footer_widgets(
            &self.progress(),
            &self.log_text(),
            self.page,
            &self.base_dir,
            VERSION,
            &layout,
        ));
        widgets
    }

    /// What a control means.
    ///
    /// `None` is "the window handles this itself": a caption, a read-only panel,
    /// or one of the text fields and pickers whose value arrives in the
    /// notification rather than in the command.
    pub fn route(&self, id: WidgetId) -> Option<Action> {
        Some(match id {
            WidgetId::Decoration => return None,
            WidgetId::Page(page) => Action::ShowPage(page),
            WidgetId::Tab(index) => Action::SwitchTab(index),
            WidgetId::StartStack => Action::StartStack,
            WidgetId::StopAll => Action::StopAll,
            WidgetId::RestartStack => Action::RestartStack,
            WidgetId::Card { index, part } => self.card_action(index, part)?,
            WidgetId::Version { index, variant } => self.version_action(index, variant)?,
            WidgetId::EditorSave => Action::EditorSave,
            WidgetId::EditorReload => Action::EditorReload,
            WidgetId::VhostSave => Action::VhostSave,
            WidgetId::VhostDelete => Action::VhostDelete,
            WidgetId::VhostApply => Action::VhostApply,
            WidgetId::ProjectCreate => Action::CreateProject,
            WidgetId::ProjectBrowse => Action::BrowseProjectFolder,
            WidgetId::ProjectAdopt => Action::AdoptProject,
            WidgetId::ProjectAction(action) => self.project_action(action)?,
            WidgetId::LandingStart => Action::StartStack,
            WidgetId::LandingWelcome => Action::OpenUrl("http://localhost".to_owned()),
            WidgetId::LandingDashboard => Action::ShowPage(Page::Projects),
            WidgetId::Settings(action) => Action::Settings(action),
            // The picker's choice arrives with its selection, and the rest are
            // panels and fields the window reads directly.
            WidgetId::WebPicker
            | WidgetId::Status(_)
            | WidgetId::Progress
            | WidgetId::ProgressLabel
            | WidgetId::Log
            | WidgetId::EditorFile
            | WidgetId::EditorText
            | WidgetId::EditorPath
            | WidgetId::VhostList
            | WidgetId::VhostDomainName
            | WidgetId::VhostDomainExt
            | WidgetId::VhostPort
            | WidgetId::VhostServer
            | WidgetId::VhostDocroot
            | WidgetId::ProjectFramework
            | WidgetId::ProjectName
            | WidgetId::ProjectDomainName
            | WidgetId::ProjectDomainExt
            | WidgetId::ProjectLocation
            | WidgetId::ProjectList => return None,
        })
    }

    /// What a card's control means, from the service that card was built for.
    fn card_action(&self, index: usize, part: CardPart) -> Option<Action> {
        let service = self.service_at(index)?;
        let name = service.name().to_owned();
        Some(match part {
            // Which of the two the button is comes from the engine, so a card
            // that says Stop stops and one that says Start starts.
            CardPart::Toggle => {
                if service.running() {
                    Action::StopService(name)
                } else {
                    Action::StartService(name)
                }
            }
            CardPart::Restart => Action::RestartService(name),
            CardPart::Configure => match configure_action(service, &self.base_dir) {
                ConfigureAction::Terminal => Action::OpenTerminal(name),
                ConfigureAction::Url(url) => Action::OpenUrl(url),
                ConfigureAction::Editor(path) => Action::OpenEditor(path),
                ConfigureAction::Nothing => Action::Nothing,
            },
            // The labels, the icon and the dot are not controls; the version
            // button opens its menu rather than reporting an action.
            CardPart::Icon
            | CardPart::Dot
            | CardPart::Name
            | CardPart::Status
            | CardPart::Version => return None,
        })
    }

    /// What a line of the tray's menu asks for.
    ///
    /// The tray's identifiers arrive through the same `WM_COMMAND` a control's
    /// do, and `view::tray_command` turns one back into the original's command -
    /// so the mapping onto an action belongs here, where the harness can press
    /// all five and check what each becomes.
    pub fn tray_action(&self, command: lambo_core::tray::TrayCommand) -> Action {
        use lambo_core::tray::TrayCommand;
        match command {
            // Showing the window is the window's own business, not an engine
            // operation: `Action::Nothing` is what it means to the engine.
            TrayCommand::Show => Action::Nothing,
            TrayCommand::Start => Action::StartTrayStack,
            TrayCommand::Stop => Action::StopAll,
            TrayCommand::ToggleAutoStart => Action::Settings(SettingsAction::ToggleAutoStart),
            TrayCommand::Quit => Action::Settings(SettingsAction::Quit),
        }
    }

    /// What a version menu entry means: install that build and make it active.
    fn version_action(&self, index: usize, variant: usize) -> Option<Action> {
        let service = self.service_at(index)?;
        let component = catalog_panel::find(service.name())?;
        let version = component.variants.get(variant)?;
        Some(Action::SwitchVersion {
            name: service.name().to_owned(),
            version: version.version.to_owned(),
        })
    }

    /// What a project row's button means, for the selected project.
    fn project_action(&self, action: ProjectActionId) -> Option<Action> {
        let name = self.project_selected.clone()?;
        let project = self
            .config
            .projects
            .iter()
            .find(|project| project.name == name)?;
        Some(match action {
            ProjectActionId::OpenInBrowser => {
                Action::OpenProjectUrl(lambo_core::ui_state::project_url(project))
            }
            ProjectActionId::OpenFolder => Action::OpenProjectFolder(
                self.base_dir
                    .join("www")
                    .join(&project.name)
                    .display()
                    .to_string(),
            ),
            ProjectActionId::Delete => Action::DeleteProject(project.name.clone()),
        })
    }

    /// The service a card index was built from.
    fn service_at(&self, index: usize) -> Option<&ManagedService> {
        self.stack.services().get(index)
    }
}

#[cfg(test)]
#[path = "state/tests.rs"]
mod tests;
