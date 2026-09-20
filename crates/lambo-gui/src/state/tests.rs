//! Tests for the panel's state and its routing.
//!
//! The window cannot be tested, but the part of it that decides what a click
//! means can be: these tests press controls by identifier - the same numbers a
//! `WM_COMMAND` carries - and check the engine operation that comes back.

use std::fs;
use std::sync::Arc;

use super::*;
use lambo_core::download::Stage;
use lambo_core::logs;
use lambo_core::panel::{ServiceConf, config_path};
use lambo_core::service::HostService;
use lambo_core::testutil::TempDir;
use lambo_core::ui_state::WidgetKind;

/// One service's configuration, the way the document holds it.
fn conf(name: &str, kind: &str, enabled: bool, port: u16, exe: &str) -> ServiceConf {
    ServiceConf {
        name: name.to_owned(),
        kind: kind.to_owned(),
        exe: exe.to_owned(),
        args: Vec::new(),
        port,
        workdir: String::new(),
        config_file: String::new(),
        enabled,
        open_url: String::new(),
        active_version: String::new(),
        env: Vec::new(),
    }
}

/// A clock the log's timestamps come from, so a test can read them.
fn clock() -> Clock {
    Arc::new(|| "12:00:00".to_owned())
}

/// The page's own constructor: the stack's log is the panel's log.
#[test]
fn the_panels_log_is_the_stacks_log() {
    let temp = TempDir::new();
    let base = temp.path();
    // The document the panel is opened over is the one on disk, so the editor
    // has something to offer before anything has been installed.
    fs::write(config_path(base), "{}").expect("the document");
    let mut panel = Panel::open(base, PanelConfig::default_config(), clock());

    // The closure the engine is handed writes into the buffer the panel shows.
    let engine_log = panel.log_fn();
    engine_log("engine: starting Apache");
    assert!(
        panel.log_text().contains("engine: starting Apache"),
        "{}",
        panel.log_text()
    );

    panel.log("panel: hello");
    assert!(panel.log_text().contains("panel: hello"));

    // And the stack was built over the document, so the services the cards are
    // drawn from are the configured ones.
    assert_eq!(
        panel.stack().services().len(),
        panel.config().services.len()
    );
    panel.refresh_editor();
    assert!(
        panel
            .editor()
            .files
            .iter()
            .any(|file| file.label.starts_with("Lambo PHP config")),
        "the page offers the document: {:?}",
        panel.editor().files
    );
}

/// The tray's five lines, and the two passes they must not be confused for.
#[test]
fn each_tray_line_names_the_engine_operation_it_is() {
    use lambo_core::tray::TrayCommand;

    let temp = TempDir::new();
    let base = temp.path();
    fs::write(config_path(base), "{}").expect("the document");
    let panel = Panel::open(base, PanelConfig::default_config(), clock());

    assert_eq!(panel.tray_action(TrayCommand::Show), Action::Nothing);
    // The tray's Start is its *own* pass - the fixed essential list, no browser
    // - and the page's button is the other one.
    assert_eq!(
        panel.tray_action(TrayCommand::Start),
        Action::StartTrayStack
    );
    assert_eq!(panel.tray_action(TrayCommand::Stop), Action::StopAll);
    assert_eq!(
        panel.tray_action(TrayCommand::ToggleAutoStart),
        Action::Settings(SettingsAction::ToggleAutoStart)
    );
    assert_eq!(
        panel.tray_action(TrayCommand::Quit),
        Action::Settings(SettingsAction::Quit)
    );
}

/// The window's creation pass: the banner, then the document's auto-start list.
#[test]
fn the_window_opens_with_the_banner_and_starts_what_the_document_asks_for() {
    let temp = TempDir::new();
    let base = temp.path();

    // Apache is "installed": its executable is on disk, which is what the
    // original's self-heal check looked for. It cannot actually run here, so the
    // auto-start pass has to report that - which is the line this test reads.
    let httpd = base
        .join("bin")
        .join("apache")
        .join("bin")
        .join("httpd.exe");
    fs::create_dir_all(httpd.parent().expect("the directory")).expect("the layout");
    fs::write(&httpd, "MZ").expect("the executable");

    let mut config = PanelConfig::default_config();
    config.settings.auto_start = vec!["Apache".to_owned()];
    config.services = vec![conf(
        "Apache",
        "web",
        true,
        80,
        "{base}/bin/apache/bin/httpd.exe",
    )];
    config.vhosts.clear();
    // `Panel::open` is the window's own constructor: it builds the stack over
    // this document with the *panel's* log, which is what makes the auto-start
    // pass's own line land in the buffer this test reads. (A stack built by
    // hand can only log into the buffer it was handed.)
    let mut panel = Panel::open(base, config, clock());

    panel.startup();
    let log = panel.log_text();

    // The three lines the log opens with, in the original's words.
    assert!(log.contains("Lambo PHP started at 12:00:00"), "{log}");
    assert!(
        log.contains(&format!("Base dir: {}", base.display())),
        "{log}"
    );
    assert!(
        log.contains("Loaded 1 services, 0 vhosts from config.json"),
        "{log}"
    );
    // And the auto-start pass, with its own failure line.
    assert!(log.contains("[auto-start] Apache:"), "{log}");
}

/// A panel over a stack of these services.
fn panel(base: &Path, mut services: Vec<ServiceConf>) -> Panel {
    let mut config = PanelConfig::default_config();
    for service in &mut services {
        if service.active_version.is_empty() {
            service.active_version = catalog_panel::find(&service.name)
                .map(|component| component.version.to_owned())
                .unwrap_or_default();
        }
    }
    config.services = services;
    let stack = Stack::build(base, &config, Arc::new(HostService::new()), logs::nop_log());
    let mut panel = Panel::new(base, config, stack, clock());
    // The panel opens on the landing page; these tests route clicks on the
    // workroom pages, so they start there.
    panel.set_page(Page::Services);
    panel
}

/// A panel whose services are the ones the tests route clicks for.
fn services_panel(base: &Path) -> Panel {
    let mut apache = conf("Apache", "web", true, 80, "{base}/bin/apache/bin/httpd.exe");
    apache.config_file = "{base}/conf/apache/httpd.conf".to_owned();
    let mut phpmyadmin = conf("phpMyAdmin", "tool", true, 0, "");
    phpmyadmin.open_url = "http://localhost/phpmyadmin".to_owned();
    let node = conf("Node.js", "runtime", true, 0, "{base}/bin/nodejs/node.exe");
    let composer = conf("Composer", "tool", true, 0, "");
    panel(base, vec![apache, phpmyadmin, node, composer])
}

/// The card of a service by name, out of the widgets the page built.
fn card_of(panel: &Panel, name: &str) -> (usize, WidgetId) {
    let index = panel
        .stack()
        .index_of(name)
        .unwrap_or_else(|| panic!("{name} is in the stack"));
    let widgets = panel.widgets();
    let frame = widgets
        .iter()
        .find(|widget| matches!(&widget.kind, WidgetKind::Card { view, .. } if view.name.starts_with(name)))
        .unwrap_or_else(|| panic!("{name} has a card"));
    (index, frame.id)
}

#[test]
fn a_new_panel_opens_on_the_services_page_over_the_stacks_services() {
    let temp = TempDir::new();
    let panel = services_panel(temp.path());

    assert_eq!(panel.page(), Page::Services);
    assert_eq!(panel.tab(), 0);
    assert!(!panel.busy());
    assert_eq!(panel.progress(), ProgressView::idle());
    assert_eq!(panel.log_text(), "");
    // The lists are the document's, and the document is the engine's.
    assert_eq!(panel.vhosts().len(), panel.config().vhosts.len());
    assert_eq!(panel.projects().len(), panel.config().projects.len());

    // The page is the sidebar, the toolbar, the tabs, one frame per service, and
    // the footer.
    // Five sidebar buttons, five toolbar entries, five tabs, one frame per
    // service, and the footer: three status parts, the strip's label and bar,
    // and the log with its caption.
    let widgets = panel.widgets();
    assert_eq!(widgets.len(), Page::SIDEBAR.len() + 5 + 5 + 4 + 7);
    for page in Page::SIDEBAR {
        assert!(
            widgets
                .iter()
                .any(|widget| widget.id == WidgetId::Page(page))
        );
    }
    assert!(
        widgets
            .iter()
            .any(|widget| widget.id == WidgetId::StartStack)
    );
    assert!(widgets.iter().any(|widget| widget.id == WidgetId::Log));
    assert!(widgets.iter().any(|widget| widget.id == WidgetId::Progress));
    let cards = widgets
        .iter()
        .filter(|widget| matches!(widget.kind, WidgetKind::Card { .. }))
        .count();
    assert_eq!(cards, 4, "one frame per service, in the stack's order");

    // The frames carry the index the routing uses, which is the stack's order.
    assert_eq!(card_of(&panel, "Apache").0, 0);
    assert_eq!(card_of(&panel, "Node.js").0, 2);
}

#[test]
fn the_window_opens_on_the_landing_page_that_leads_into_the_stack() {
    let temp = TempDir::new();
    let config = PanelConfig::default_config();
    let stack = Stack::build(
        temp.path(),
        &config,
        Arc::new(HostService::new()),
        logs::nop_log(),
    );
    let panel = Panel::new(temp.path(), config, stack, clock());
    assert_eq!(panel.page(), Page::Landing);

    // The landing page names its three ways in, and each goes where it says.
    let widgets = panel.widgets();
    for id in [
        WidgetId::LandingStart,
        WidgetId::LandingWelcome,
        WidgetId::LandingDashboard,
    ] {
        assert!(widgets.iter().any(|widget| widget.id == id), "{id:?}");
    }
    assert_eq!(
        panel.route(WidgetId::LandingStart),
        Some(Action::StartStack)
    );
    assert_eq!(
        panel.route(WidgetId::LandingWelcome),
        Some(Action::OpenUrl("http://localhost".to_owned()))
    );
    assert_eq!(
        panel.route(WidgetId::LandingDashboard),
        Some(Action::ShowPage(Page::Projects))
    );

    // The landing page has no sidebar button of its own.
    assert!(
        !widgets
            .iter()
            .any(|widget| widget.id == WidgetId::Page(Page::Landing))
    );
}

#[test]
fn the_projects_page_carries_the_location_of_the_project_to_load() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());
    assert!(panel.set_page(Page::Projects));
    assert_eq!(panel.project_location(), "");

    // Typing into the field records the folder; the window feeds it back in.
    panel.set_edit(
        WidgetId::ProjectLocation,
        r"C:\Projects\my project".to_owned(),
    );
    assert_eq!(panel.project_location(), r"C:\Projects\my project");
    let widgets = panel.widgets();
    let field = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::ProjectLocation)
        .expect("the location field is there");
    assert!(matches!(&field.kind, WidgetKind::Edit(text) if text == r"C:\Projects\my project"));
    assert!(
        widgets
            .iter()
            .any(|widget| widget.id == WidgetId::ProjectBrowse)
    );
    assert!(
        widgets
            .iter()
            .any(|widget| widget.id == WidgetId::ProjectAdopt)
    );

    // Browse opens the picker; Open loads what the field names.
    assert_eq!(
        panel.route(WidgetId::ProjectBrowse),
        Some(Action::BrowseProjectFolder)
    );
    assert_eq!(
        panel.route(WidgetId::ProjectAdopt),
        Some(Action::AdoptProject)
    );
}

#[test]
fn the_sidebar_and_the_tabs_route_to_their_pages() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());

    assert_eq!(
        panel.route(WidgetId::Page(Page::Vhosts)),
        Some(Action::ShowPage(Page::Vhosts))
    );
    assert!(panel.set_page(Page::Vhosts));
    assert_eq!(panel.page(), Page::Vhosts);
    // Pressing the page that is already open changes nothing.
    assert!(!panel.set_page(Page::Vhosts));
    assert_eq!(
        panel.route(WidgetId::Page(Page::Services)),
        Some(Action::ShowPage(Page::Services))
    );

    assert_eq!(panel.route(WidgetId::Tab(3)), Some(Action::SwitchTab(3)));
    // Choosing the tab that is already open is not a change - except for the
    // first one, which re-packs the grid, which is the original's own rule.
    assert!(panel.set_tab(3));
    assert!(!panel.set_tab(3), "the open tab is not a change");
    assert!(panel.set_tab(0), "All re-packs the grid even so");
    assert_eq!(panel.tab(), 0);
    assert_eq!(panel.route(WidgetId::StartStack), Some(Action::StartStack));
    assert_eq!(panel.route(WidgetId::StopAll), Some(Action::StopAll));
    assert_eq!(
        panel.route(WidgetId::RestartStack),
        Some(Action::RestartStack)
    );

    // Every page builds its own controls, and the footer always comes with them.
    for page in Page::ALL {
        panel.set_page(page);
        let widgets = panel.widgets();
        assert!(
            widgets.iter().any(|widget| widget.id == WidgetId::Progress),
            "{page:?}"
        );
        assert!(
            widgets
                .iter()
                .any(|widget| widget.id == WidgetId::Status(1)),
            "{page:?}"
        );
    }
}

#[test]
fn a_card_says_what_each_of_its_buttons_means() {
    let temp = TempDir::new();
    let panel = services_panel(temp.path());
    let (apache, frame) = card_of(&panel, "Apache");

    // Pressing the frame itself, or one of its labels, is nothing.
    assert_eq!(panel.route(frame), None);
    for part in [
        CardPart::Icon,
        CardPart::Dot,
        CardPart::Name,
        CardPart::Status,
    ] {
        assert_eq!(
            panel.route(WidgetId::Card {
                index: apache,
                part
            }),
            None,
            "{part:?}"
        );
    }

    // A stopped service's toggle starts it: which of the two the button is comes
    // from the engine's own reading of the service.
    assert_eq!(
        panel.route(WidgetId::Card {
            index: apache,
            part: CardPart::Toggle
        }),
        Some(Action::StartService("Apache".to_owned()))
    );
    assert_eq!(
        panel.route(WidgetId::Card {
            index: apache,
            part: CardPart::Restart
        }),
        Some(Action::RestartService("Apache".to_owned()))
    );
    // Configure is the engine's branch table: a configuration file means the
    // editor, and the page is pointed at that file.
    assert_eq!(
        panel.route(WidgetId::Card {
            index: apache,
            part: CardPart::Configure
        }),
        Some(Action::OpenEditor(
            temp.path().join("conf/apache/httpd.conf")
        ))
    );

    // Apache has no variants, so its version menu has nothing to report.
    assert_eq!(
        panel.route(WidgetId::Version {
            index: apache,
            variant: 0
        }),
        None
    );
    assert_eq!(
        panel.route(WidgetId::Card {
            index: apache,
            part: CardPart::Version
        }),
        None
    );

    // A card index the stack does not have routes nowhere.
    assert_eq!(
        panel.route(WidgetId::Card {
            index: 99,
            part: CardPart::Toggle
        }),
        None
    );
}

#[test]
fn a_runtime_card_opens_a_terminal_and_a_url_card_opens_the_url() {
    let temp = TempDir::new();
    let panel = services_panel(temp.path());

    let node = panel.stack().index_of("Node.js").expect("Node.js");
    assert_eq!(
        panel.route(WidgetId::Card {
            index: node,
            part: CardPart::Configure
        }),
        Some(Action::OpenTerminal("Node.js".to_owned()))
    );

    let phpmyadmin = panel.stack().index_of("phpMyAdmin").expect("phpMyAdmin");
    assert_eq!(
        panel.route(WidgetId::Card {
            index: phpmyadmin,
            part: CardPart::Configure
        }),
        Some(Action::OpenUrl("http://localhost/phpmyadmin".to_owned()))
    );

    // A tool with no configuration file, no URL and no terminal does nothing at
    // all, which is an answer rather than a refusal.
    let composer = panel.stack().index_of("Composer").expect("Composer");
    assert_eq!(
        panel.route(WidgetId::Card {
            index: composer,
            part: CardPart::Configure
        }),
        Some(Action::Nothing)
    );
}

#[test]
fn a_version_menu_entry_names_the_catalogue_version_it_installs() {
    let temp = TempDir::new();
    let mut php = conf("PHP-FPM", "php", true, 9000, "{base}/bin/php/php-cgi.exe");
    php.active_version = "8.3".to_owned();
    let panel = panel(temp.path(), vec![php]);
    let (index, _) = card_of(&panel, "PHP-FPM");

    let component = catalog_panel::find("PHP-FPM").expect("PHP-FPM is catalogued");
    assert!(
        component.variants.len() >= 2,
        "the catalogue offers two builds"
    );
    assert_eq!(
        panel.route(WidgetId::Version { index, variant: 1 }),
        Some(Action::SwitchVersion {
            name: "PHP-FPM".to_owned(),
            version: component.variants[1].version.to_owned(),
        })
    );

    // An entry the catalogue does not have is not an action.
    assert_eq!(panel.route(WidgetId::Version { index, variant: 99 }), None);
}

#[test]
fn the_settings_page_routes_its_actions_and_shows_them() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());
    panel.set_page(Page::Settings);
    panel.set_auto_start(true);

    assert_eq!(
        panel.route(WidgetId::Settings(SettingsAction::Quit)),
        Some(Action::Settings(SettingsAction::Quit))
    );
    assert_eq!(
        panel.route(WidgetId::Settings(SettingsAction::AddToPath)),
        Some(Action::Settings(SettingsAction::AddToPath))
    );

    assert_eq!(
        panel.route(WidgetId::Settings(SettingsAction::OpenRepository)),
        Some(Action::Settings(SettingsAction::OpenRepository)),
        "the repository button routes like every other settings action"
    );

    let widgets = panel.widgets();
    let buttons = widgets
        .iter()
        .filter(|widget| matches!(widget.id, WidgetId::Settings(_)))
        .count();
    assert_eq!(buttons, 9);
    // The page's own value rows are there too, and are not routed.
    assert!(widgets.iter().any(
        |widget| matches!(&widget.kind, WidgetKind::Label(text) if text.starts_with("Install dir"))
    ));
    assert_eq!(panel.route(WidgetId::Decoration), None);

    // A settings number the grid does not have routes nowhere.
    assert_eq!(
        panel
            .route(WidgetId::Settings(SettingsAction::RestartAsAdmin))
            .is_some(),
        !panel.elevated,
        "the admin action is offered until the process is elevated"
    );
}

#[test]
fn the_editor_page_loads_saves_and_keeps_unsaved_text() {
    let temp = TempDir::new();
    let base = temp.path();
    std::fs::create_dir_all(base.join("conf/apache")).expect("the apache directory");
    std::fs::write(base.join("conf/apache/httpd.conf"), "# apache\r\n").expect("the include");
    let config_file = lambo_core::panel::config_path(base);
    std::fs::create_dir_all(config_file.parent().expect("a directory")).expect("the directory");
    std::fs::write(&config_file, "{}\r\n").expect("config.json");

    let mut panel = services_panel(base);
    panel.set_page(Page::Editor);
    panel.refresh_editor();

    assert_eq!(panel.route(WidgetId::EditorSave), Some(Action::EditorSave));
    assert_eq!(
        panel.route(WidgetId::EditorReload),
        Some(Action::EditorReload)
    );
    // The dropdown's choice arrives with its selection, not as a command.
    assert_eq!(panel.route(WidgetId::EditorFile), None);
    assert_eq!(panel.route(WidgetId::EditorText), None);

    panel.refresh_editor();
    assert!(
        panel
            .editor()
            .files
            .iter()
            .any(|file| file.label.starts_with("Lambo PHP config")),
        "the page offers the document: {:?}",
        panel.editor().files
    );
    let line = panel.select_editor_file(0).expect("the first file loads");
    assert!(line.starts_with("editor: loaded "), "{line}");
    assert_eq!(
        panel.editor().status_text(),
        panel.editor().files[0].path.display().to_string(),
        "the first entry is what the page loaded"
    );

    // Unsaved text stays put when another page is visited and the file list is
    // rebuilt.
    let before = panel.editor().text.clone();
    panel.set_editor_text(format!("{before}// a change\r\n"));
    assert!(panel.editor().has_unsaved_changes());
    assert!(panel.set_page(Page::Services));
    assert!(panel.set_page(Page::Editor));
    assert_eq!(panel.editor().text, format!("{before}// a change\r\n"));

    // A file the card's Conf button names is loaded, and selected in the
    // dropdown when it is one of the files offered.
    let apache = base.join("conf/apache/httpd.conf");
    let line = panel.load_editor_file(&apache);
    assert!(line.contains("httpd.conf"));
    assert_eq!(panel.editor().status_text(), apache.display().to_string());
}

#[test]
fn a_vhost_row_fills_the_form_and_the_delete_acts_on_the_row() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());
    let mut config = panel.config().clone();
    config.vhosts = vec![Vhost {
        domain: "shop.test".to_owned(),
        docroot: "{base}/www/shop".to_owned(),
        port: 0,
        server_type: String::new(),
        enabled: true,
        proxy_port: 0,
    }];
    panel.set_document(config);
    panel.set_page(Page::Vhosts);

    // Nothing selected: the table is read-only until a row is picked.
    assert_eq!(panel.vhost_current(), None);
    assert_eq!(panel.vhost_to_delete(), None);

    panel.edit_vhost(Some(0));
    assert_eq!(panel.vhost_current(), Some("shop.test"));
    assert_eq!(panel.vhost_form().name, "shop");
    assert_eq!(panel.vhost_form().extension, ".test");
    assert_eq!(panel.vhost_form().port, "80", "a zero port shows as 80");
    assert_eq!(
        panel.vhost_form().server,
        "apache",
        "an empty type is apache"
    );
    assert_eq!(panel.vhost_to_delete(), Some("shop.test".to_owned()));

    // The page's buttons and its table.
    assert_eq!(panel.route(WidgetId::VhostSave), Some(Action::VhostSave));
    assert_eq!(
        panel.route(WidgetId::VhostDelete),
        Some(Action::VhostDelete)
    );
    assert_eq!(panel.route(WidgetId::VhostApply), Some(Action::VhostApply));
    assert_eq!(panel.route(WidgetId::VhostList), None);
    assert_eq!(panel.route(WidgetId::VhostDomainName), None);

    let widgets = panel.widgets();
    let table = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::VhostList)
        .expect("the table is there");
    match &table.kind {
        WidgetKind::List { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][1], "shop.test");
            assert_eq!(rows[0][3], "80");
        }
        other => panic!("a table is a list: {other:?}"),
    }

    // Selecting nothing empties the form again, which is how a new host is added.
    panel.edit_vhost(None);
    assert_eq!(panel.vhost_current(), None);
    assert_eq!(panel.vhost_form(), &VhostForm::blank());
}

#[test]
fn a_project_row_names_the_project_its_buttons_act_on() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());
    let mut config = panel.config().clone();
    config.projects = vec![PanelProject {
        name: "shop".to_owned(),
        framework: "Laravel".to_owned(),
        domain: "shop.test".to_owned(),
        docroot: "C:\\Lambo\\www\\shop".to_owned(),
        port: 0,
    }];
    panel.set_document(config);
    panel.set_page(Page::Projects);

    // The buttons act on the selected row, and there is none yet.
    assert_eq!(panel.project_selected(), None);
    for action in [
        ProjectActionId::OpenInBrowser,
        ProjectActionId::OpenFolder,
        ProjectActionId::Delete,
    ] {
        assert_eq!(
            panel.route(WidgetId::ProjectAction(action)),
            None,
            "{action:?}"
        );
    }

    panel.select_project(Some(0));
    assert_eq!(panel.project_selected(), Some("shop"));
    assert_eq!(
        panel.route(WidgetId::ProjectAction(ProjectActionId::OpenInBrowser)),
        Some(Action::OpenProjectUrl("http://shop.test".to_owned()))
    );
    assert_eq!(
        panel.route(WidgetId::ProjectAction(ProjectActionId::OpenFolder)),
        Some(Action::OpenProjectFolder(
            temp.path().join("www").join("shop").display().to_string()
        ))
    );
    assert_eq!(
        panel.route(WidgetId::ProjectAction(ProjectActionId::Delete)),
        Some(Action::DeleteProject("shop".to_owned()))
    );
    assert_eq!(
        panel.route(WidgetId::ProjectCreate),
        Some(Action::CreateProject)
    );
    assert_eq!(panel.route(WidgetId::ProjectList), None);
    assert_eq!(panel.route(WidgetId::ProjectName), None);

    // A row that is no longer there is not an action.
    panel.select_project(Some(7));
    assert_eq!(panel.project_selected(), None);
}

#[test]
fn the_log_and_the_strip_are_shared_with_the_engines_callbacks() {
    let temp = TempDir::new();
    let panel = services_panel(temp.path());

    panel.log("first");
    let engine_log = panel.log_fn();
    engine_log("second");
    panel.log("third");
    let text = panel.log_text();
    assert!(
        text.contains("12:00:00 first"),
        "each line is stamped: {text}"
    );
    let first = text.find("first").expect("the first line");
    let second = text.find("second").expect("the engine's line");
    let third = text.find("third").expect("the last line");
    assert!(first < second && second < third, "the log keeps the order");
    assert!(
        text.ends_with("\r\n"),
        "each line ends the way the panel expects"
    );

    // The installer's progress lands in the strip.
    let progress = panel.progress_fn();
    progress(Stage::Downloading, "Apache", 1024 * 1024, 2 * 1024 * 1024);
    let view = panel.progress();
    assert_eq!(view.position, 500);
    assert!(
        view.label.starts_with("Downloading Apache  50.0%"),
        "{}",
        view.label
    );

    panel.set_progress(ProgressView::idle());
    assert_eq!(panel.progress(), ProgressView::idle());
}

#[test]
fn the_status_bar_and_the_strip_come_from_the_pages_own_state() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());
    let widgets = panel.widgets();

    let status: Vec<String> = widgets
        .iter()
        .filter(|widget| matches!(widget.id, WidgetId::Status(_)))
        .filter_map(|widget| match &widget.kind {
            WidgetKind::Label(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(status.len(), 3);
    assert_eq!(status[0], Page::Services.status_text());
    assert_eq!(
        status[1],
        lambo_core::ui_state::truncate_mid(&temp.path().display().to_string(), 70)
    );
    assert_eq!(status[2], format!("Lambo PHP {VERSION}"));

    // A page that is opened shows its own name, from the page's own table.
    panel.set_page(Page::Settings);
    let status = panel.widgets();
    assert!(status
        .iter()
        .any(|widget| matches!(&widget.kind, WidgetKind::Label(text) if *text == Page::Settings.status_text())));
}

#[test]
fn the_panel_keeps_its_own_state_out_of_the_engine() {
    let temp = TempDir::new();
    let mut panel = services_panel(temp.path());

    // The version the panel reports is this build's, and the settings page is
    // built from the engine's description of the installation.
    const { assert!(!VERSION.is_empty()) };
    let settings = panel.settings();
    assert!(settings.blocks.iter().any(|block| matches!(
        block,
        lambo_core::ui_state::SettingsBlock::Section(section)
            if section.rows.iter().any(|row| row.label == "Install dir"
                && row.value == temp.path().display().to_string())
    )));

    panel.set_elevated(true);
    assert!(panel.settings().blocks.iter().any(|block| matches!(
        block,
        lambo_core::ui_state::SettingsBlock::Section(section)
            if section.rows.iter().any(|row| row.label == "Running elevated" && row.value == "yes (administrator)")
    )));

    // The runtime line is the engine's, not a copy of it.
    assert_eq!(
        panel.runtime_status(),
        lambo_core::frameworks::runtime_status_text(temp.path())
    );
}
