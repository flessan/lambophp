//! Tests for the panel's pure half.
//!
//! Everything here runs on any platform, which is the point of `ui_state`: the
//! window's drawing is Win32 and cannot be checked here, but every decision it
//! draws from - geometry, labels, status, dots, buttons, rows, log lines - can.
//!
//! The services are real [`crate::stack::ManagedService`]s built by a real
//! [`Stack`] over a real temporary installation, so a card's status, dot and
//! button states are the engine's own answers and not a fixture's idea of them.

use std::sync::Arc;

use super::*;
use crate::download::Stage;
use crate::panel::ServiceConf;
use crate::service::HostService;
use crate::stack::Stack;
use crate::testutil::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One configured service.
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

/// A stack over a temporary installation, with the services as configured.
fn stack_of(base_dir: &Path, services: Vec<ServiceConf>) -> Stack {
    let mut config = PanelConfig::default_config();
    config.services = services;
    Stack::build(
        base_dir,
        &config,
        Arc::new(HostService::new()),
        crate::logs::nop_log(),
    )
}

/// One registered virtual host.
fn vhost_of(domain: &str) -> Vhost {
    Vhost {
        domain: domain.to_owned(),
        docroot: format!("{{base}}/www/{domain}"),
        port: 0,
        server_type: "apache".to_owned(),
        enabled: true,
        proxy_port: 0,
    }
}

fn project_of(name: &str, framework: &str, domain: &str) -> PanelProject {
    PanelProject {
        name: name.to_owned(),
        framework: framework.to_owned(),
        domain: domain.to_owned(),
        docroot: format!("C:\\Lambo\\www\\{name}"),
        port: 0,
    }
}

/// The settings page's value for a row, wherever it sits.
fn value_of(view: &SettingsView, label: &str) -> String {
    for block in &view.blocks {
        if let SettingsBlock::Section(section) = block {
            for row in &section.rows {
                if row.label == label {
                    return row.value.clone();
                }
            }
        }
    }
    panic!("no row named {label}: {view:?}");
}

// ---------------------------------------------------------------------------
// The window: layout, pages, tabs
// ---------------------------------------------------------------------------

#[test]
fn the_layout_grows_with_the_grid_and_the_window_follows_it() {
    // Four cards fit in the content area: (960 + 8) / (228 + 8).
    assert_eq!(columns(), 4);

    // A single service still gets two rows, so the window does not jump as the
    // rest are installed.
    let small = Layout::compute(1);
    assert_eq!(small.columns, 4);
    assert_eq!(small.rows, 2);
    assert_eq!(
        small.content_h,
        CARD_GRID_Y + 2 * (CARD_H + CARD_GAP) - CARD_GAP + 16
    );
    assert_eq!(small.progress_y, CONTENT_Y + small.content_h + 8);
    assert_eq!(small.log_y, small.progress_y + PROGRESS_H + 8);
    assert_eq!(small.window_h, small.log_y + LOG_H + 40);

    // Nine services need three rows.
    let big = Layout::compute(9);
    assert_eq!(big.rows, 3);
    assert_eq!(big.window_h, small.window_h + CARD_H + CARD_GAP);

    // Cards run across a row and then wrap.
    assert_eq!(small.card_position(0), (GRID_X, CARD_GRID_Y));
    assert_eq!(
        small.card_position(1),
        (GRID_X + CARD_W + CARD_GAP, CARD_GRID_Y)
    );
    assert_eq!(
        small.card_position(4),
        (GRID_X, CARD_GRID_Y + CARD_H + CARD_GAP)
    );

    // The log panel and the progress strip are what the heights were computed
    // for: the log is the last thing above the status bar.
    assert_eq!(small.log_y + LOG_H, small.window_h - 40);
    const { assert!(WINDOW_W > CONTENT_X + CONTENT_W, "the content area fits") };
    const { assert!(LOG_X + LOG_W <= WINDOW_W, "the log panel fits") };
}

#[test]
fn the_sidebar_and_the_tabs_sit_where_the_original_put_them() {
    assert_eq!(sidebar_position(0), (SIDE_X, SIDE_Y));
    assert_eq!(
        sidebar_position(2),
        (SIDE_X, SIDE_Y + 2 * (SIDE_H + SIDE_GAP))
    );
    // Five sidebar pages, and the last of them still above the content area
    // the smallest window has.
    let last = sidebar_position(Page::SIDEBAR.len() - 1);
    assert!(last.1 + SIDE_H <= CONTENT_Y + Layout::compute(1).content_h);

    assert_eq!(tab_position(0), (GRID_X, TAB_STRIP_Y));
    assert_eq!(tab_position(1), (GRID_X + TAB_BTN_W + 4, TAB_STRIP_Y));
    // The strip starts below the toolbar and above the grid.
    const { assert!(TAB_STRIP_Y + TAB_BTN_H <= CARD_GRID_Y) };

    // The toolbar's three stack buttons, in the original's order.
    assert_eq!(STACK_BUTTONS[0].2, 100, "Start Stack");
    assert_eq!(STACK_BUTTONS[1].2, 80, "Stop All");
    assert_eq!(STACK_BUTTONS[2].2, 80, "Restart");
    for (index, button) in STACK_BUTTONS.iter().enumerate() {
        assert!(
            button.0 + button.2 <= CONTENT_X + CONTENT_W,
            "button {index} fits"
        );
    }
    assert!(TOOLBAR_LABEL_RECT.0 + TOOLBAR_LABEL_RECT.2 <= WEB_PICKER_RECT.0);
    assert!(WEB_PICKER_RECT.0 + WEB_PICKER_RECT.2 <= STACK_BUTTONS[0].0);
}

#[test]
fn the_pages_are_the_landing_and_the_five_the_sidebar_lists() {
    let keys: Vec<&str> = Page::ALL.iter().map(|page| page.key()).collect();
    assert_eq!(
        keys,
        vec![
            "landing", "services", "projects", "editor", "vhosts", "settings"
        ]
    );
    let labels: Vec<&str> = Page::ALL.iter().map(|page| page.label()).collect();
    assert_eq!(
        labels,
        vec![
            "Home",
            "Services",
            "Projects",
            "Editor",
            "Virtual Hosts",
            "Settings"
        ]
    );

    for page in Page::ALL {
        assert_eq!(Page::from_key(page.key()), Some(page), "{page:?}");
        assert_eq!(page.status_text(), format!("Page: {}", page.key()));
    }
    assert_eq!(Page::from_key("nothing"), None);
    assert_eq!(
        Page::ALL[0],
        Page::Landing,
        "the window opens on the landing"
    );
    assert_eq!(
        Page::SIDEBAR[0],
        Page::Services,
        "services lead the sidebar"
    );
    assert_eq!(
        Page::SIDEBAR.as_slice(),
        &Page::ALL[1..],
        "the sidebar is everything but the landing page"
    );
}

#[test]
fn the_tabs_filter_the_groups_they_name() {
    let labels: Vec<&str> = TABS.iter().map(|tab| tab.label).collect();
    assert_eq!(labels, vec!["All", "Web", "Database", "Language", "Tools"]);

    for group in [
        ServiceGroup::Web,
        ServiceGroup::Language,
        ServiceGroup::Database,
        ServiceGroup::Tool,
    ] {
        assert!(tab_shows(0, group), "All shows everything");
    }
    assert!(tab_shows(1, ServiceGroup::Web));
    assert!(!tab_shows(1, ServiceGroup::Database));
    assert!(tab_shows(2, ServiceGroup::Database));
    assert!(tab_shows(3, ServiceGroup::Language));
    assert!(tab_shows(4, ServiceGroup::Tool));
    assert!(!tab_shows(4, ServiceGroup::Language));

    // A tab that is already selected does nothing - except the first one, which
    // is how the grid is reset.
    assert!(!tab_click_changes(1, 1));
    assert!(tab_click_changes(0, 0));
    assert!(tab_click_changes(2, 1));
    // An index that is not a tab shows everything rather than hiding the page.
    assert!(tab_shows(99, ServiceGroup::Tool));
}

#[test]
fn the_groups_are_the_kinds_the_original_grouped() {
    assert_eq!(ServiceGroup::of_kind("web"), ServiceGroup::Web);
    assert_eq!(ServiceGroup::of_kind("WEB"), ServiceGroup::Web);
    assert_eq!(ServiceGroup::of_kind("php"), ServiceGroup::Language);
    assert_eq!(ServiceGroup::of_kind("language"), ServiceGroup::Language);
    assert_eq!(ServiceGroup::of_kind("runtime"), ServiceGroup::Language);
    assert_eq!(ServiceGroup::of_kind("database"), ServiceGroup::Database);
    assert_eq!(ServiceGroup::of_kind("cache"), ServiceGroup::Database);
    assert_eq!(ServiceGroup::of_kind("tool"), ServiceGroup::Tool);
    assert_eq!(ServiceGroup::of_kind("queue"), ServiceGroup::Tool);
    assert_eq!(ServiceGroup::of_kind(""), ServiceGroup::Tool);

    assert_eq!(ServiceGroup::Web.label(), "Web Server");
    assert_eq!(ServiceGroup::Language.label(), "Language");
    assert_eq!(ServiceGroup::Database.label(), "Database / Cache");
    assert_eq!(ServiceGroup::Tool.label(), "Admin Tool");
}

// ---------------------------------------------------------------------------
// The service cards
// ---------------------------------------------------------------------------

#[test]
fn a_disabled_service_card_says_disabled_and_shows_its_port() {
    let temp = TempDir::new();
    let stack = stack_of(
        temp.path(),
        vec![conf(
            "Cache",
            "cache",
            false,
            6379,
            "{base}/bin/cache/bin/cache.exe",
        )],
    );
    let card = card_view(&stack.services()[0], "Apache");

    assert_eq!(card.status, "Disabled  :6379");
    assert_eq!(card.dot, '○', "installed, not running");
    assert_eq!(card.name, "Cache", "the catalogue does not know it");
    assert_eq!(card.group, ServiceGroup::Database);
    assert_eq!(card.icon, None, "no icon is drawn for a name with none");
    assert!(!card.enabled);
    assert!(card.active, "a non-web service is always picked");
}

#[test]
fn an_inactive_web_server_card_is_told_to_pick_itself_up_top() {
    let temp = TempDir::new();
    let stack = stack_of(
        temp.path(),
        vec![
            conf("Apache", "web", true, 80, "{base}/bin/apache/bin/httpd.exe"),
            conf("Nginx", "web", false, 80, "{base}/bin/nginx/nginx.exe"),
        ],
    );
    let apache = card_view(&stack.services()[0], "Apache");
    assert!(apache.active);
    assert_eq!(apache.status, "Stopped  :80");
    assert_eq!(apache.toggle.label, "▶ Start");
    assert!(apache.toggle.enabled);
    assert_eq!(apache.toggle.scheme, Scheme::Success);

    // The other web server is not merely stopped: its card says what to do.
    let nginx = card_view(&stack.services()[1], "Apache");
    assert!(!nginx.active);
    assert_eq!(nginx.status, "Inactive — pick Nginx up top");
    assert_eq!(nginx.dot, '·');
    assert_eq!(nginx.toggle.label, "▶ Start");
    assert!(
        !nginx.toggle.enabled,
        "it is started from the picker, not from its card"
    );

    // And the answer flips with the setting.
    let nginx_active = card_view(&stack.services()[1], "Nginx");
    assert!(nginx_active.active);
    assert_eq!(nginx_active.status, "Disabled  :80");
    assert!(nginx_active.toggle.enabled);
    assert!(!card_view(&stack.services()[0], "Nginx").active);
}

#[test]
fn a_tools_card_has_no_restart_and_its_configure_button_is_the_editor() {
    let temp = TempDir::new();
    let mut tool = conf("Composer", "tool", true, 0, "");
    tool.config_file = "conf/composer.json".to_owned();
    let stack = stack_of(temp.path(), vec![tool]);
    let card = card_view(&stack.services()[0], "Apache");

    assert_eq!(card.status, "Not installed", "the catalogue knows it");
    assert!(
        !card.restart.enabled,
        "a tool with no process has nothing to restart"
    );
    assert_eq!(card.restart.action, CardAction::Restart);
    let configure = card.configure().expect("every card has one");
    assert_eq!(configure.action, CardAction::Configure);
    assert_eq!(configure.label, "Conf");
    assert_eq!(configure.scheme, Scheme::Primary);
    assert_eq!(card.buttons.len(), 1, "no engine, no version picker");
    assert!(card.buttons[0].enabled);
}

#[test]
fn a_runtime_card_opens_a_terminal_and_a_component_with_variants_gets_a_picker() {
    let temp = TempDir::new();
    let stack = stack_of(
        temp.path(),
        vec![
            conf("PHP-FPM", "php", true, 9000, "{base}/bin/php/php-cgi.exe"),
            conf("Ruby", "runtime", true, 0, ""),
        ],
    );

    let php = card_view(&stack.services()[0], "Apache");
    assert!(
        php.has_variants,
        "the catalogue offers PHP in several versions"
    );
    let version = php
        .buttons
        .iter()
        .find(|button| button.action == CardAction::Version)
        .expect("the version picker is there");
    assert_eq!(version.label, "Ver ▾");
    assert_eq!(version.scheme, Scheme::Warning);
    assert!(version.enabled);
    assert_eq!(php.group, ServiceGroup::Language);
    assert_eq!(php.icon, Some("php.ico"));

    // A language runtime's configure button is its terminal, and it has no
    // version picker of its own.
    let runtime = card_view(&stack.services()[1], "Apache");
    let terminal = runtime.configure().expect("every card has one");
    assert_eq!(terminal.label, "⌨ Term");
    assert_eq!(terminal.scheme, Scheme::Sidebar);
    assert!(!runtime.has_variants, "Ruby is not offered in versions");
    assert_eq!(
        runtime.buttons.len(),
        1,
        "no versions, so no picker: {:?}",
        runtime.buttons
    );
    assert_eq!(runtime.group, ServiceGroup::Language);
    assert_eq!(runtime.icon, Some("ruby.ico"));
    assert!(
        !runtime.restart.enabled,
        "the runtime is not installed, so it has no engine"
    );
    assert_eq!(runtime.toggle.label, "▶ Start");
}

#[test]
fn the_card_name_carries_the_version_the_catalogue_names() {
    // A component with variants is shortened to the variant its version starts
    // with: `8.4.22 NTS x64` becomes `8.4`.
    assert_eq!(card_name_line("PHP-FPM"), "PHP-FPM  8.4");
    // One without variants keeps the catalogue's whole version string.
    assert_eq!(card_name_line("Apache"), "Apache  2.4.68 (VS18, win64)");
    assert!(card_name_line("Nginx").starts_with("Nginx  "));
    // A name the catalogue does not know is just the name.
    assert_eq!(card_name_line("Cache"), "Cache");
    assert_eq!(card_name_line(""), "");
}

#[test]
fn the_version_menu_marks_the_build_the_catalogue_or_the_document_names() {
    let temp = TempDir::new();
    let component = catalog_panel::find("PHP-FPM").expect("PHP-FPM is catalogued");
    assert!(
        component.variants.len() >= 2,
        "the catalogue offers two builds"
    );

    // No active version recorded: the check sits on the variant the catalogue's
    // own version starts with, which is the build a fresh install unpacks.
    let stack = stack_of(
        temp.path(),
        vec![conf(
            "PHP-FPM",
            "php",
            true,
            9000,
            "{base}/bin/php/php-cgi.exe",
        )],
    );
    let items = version_menu(&stack.services()[0]);
    assert_eq!(items.len(), component.variants.len());
    assert_eq!(
        items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        component
            .variants
            .iter()
            .map(|variant| variant.version)
            .collect::<Vec<_>>(),
        "the lines are the catalogue's, in its order"
    );
    let fresh: Vec<bool> = items.iter().map(|item| item.checked).collect();
    assert_eq!(
        fresh.iter().filter(|checked| **checked).count(),
        1,
        "exactly one line is checked: {items:?}"
    );
    let expected = component
        .variants
        .iter()
        .find(|variant| component.version.starts_with(variant.version))
        .expect("the catalogue's version names a variant");
    let checked = items
        .iter()
        .find(|item| item.checked)
        .expect("a checked line");
    assert_eq!(checked.label, expected.version);

    // A recorded active version is what the check follows, even when it is not
    // the variant the catalogue's version starts with.
    let mut config = PanelConfig::default_config();
    let mut chosen = conf("PHP-FPM", "php", true, 9000, "{base}/bin/php/php-cgi.exe");
    chosen.active_version = component.variants[1].version.to_owned();
    config.services = vec![chosen];
    let stack = Stack::build(
        temp.path(),
        &config,
        Arc::new(HostService::new()),
        crate::logs::nop_log(),
    );
    let items = version_menu(&stack.services()[0]);
    assert_eq!(
        items
            .iter()
            .filter(|item| item.checked)
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        vec![component.variants[1].version]
    );

    // A service with no variants, and one the catalogue does not know, have no
    // menu at all - the card has no picker either.
    let stack = stack_of(
        temp.path(),
        vec![
            conf("Apache", "web", true, 80, "{base}/bin/apache/bin/httpd.exe"),
            conf("Cache", "tool", true, 0, ""),
        ],
    );
    assert!(version_menu(&stack.services()[0]).is_empty());
    assert!(version_menu(&stack.services()[1]).is_empty());

    assert_eq!(version_menu_title("PHP-FPM"), "Switch PHP-FPM version");
}

#[test]
fn the_card_button_row_is_the_originals_with_the_restart_square() {
    let plain = CardLayout::compute(false);
    assert_eq!(plain.icon, (8, 8, 32, 32));
    assert_eq!(plain.dot, (44, 10, 12, 14));
    assert_eq!(plain.name, (58, 10, CARD_W - 66, 16));
    assert_eq!(plain.status, (48, 28, CARD_W - 56, 14));

    // Toggle, restart, configure - the configure button fills to the edge.
    assert_eq!(
        plain.buttons,
        vec![(8, 44, 72, 22), (86, 44, 22, 22), (114, 44, 106, 22)]
    );
    assert_eq!(plain.buttons.last().expect("a button").0 + 106, CARD_W - 8);

    // With the version picker the row still ends at the same edge.
    let variant = CardLayout::compute(true);
    assert_eq!(
        variant.buttons,
        vec![
            (8, 44, 72, 22),
            (86, 44, 22, 22),
            (114, 44, 50, 22),
            (170, 44, 50, 22)
        ]
    );
    assert_eq!(variant.buttons.last().expect("a button").0 + 50, CARD_W - 8);

    // Every button sits inside the card, and the row is above its bottom edge.
    for layout in [plain, variant] {
        for rect in layout.buttons {
            assert!(rect.0 + rect.2 <= CARD_W, "{rect:?} fits the card");
            assert!(rect.1 + rect.3 <= CARD_H, "{rect:?} fits the card");
        }
    }

    // The card the view builds carries the same row.
    let temp = TempDir::new();
    let stack = stack_of(
        temp.path(),
        vec![conf(
            "Apache",
            "web",
            true,
            80,
            "{base}/bin/apache/bin/httpd.exe",
        )],
    );
    let card = card_view(&stack.services()[0], "Apache");
    let rects = CardLayout::compute(card.has_variants).buttons;
    assert_eq!(
        rects.len(),
        1 + 1 + card.buttons.len(),
        "toggle + restart + the rest"
    );
    assert_eq!(rects[1], (86, 44, 22, 22), "the restart square");
}

#[test]
fn the_icons_are_the_files_the_installation_carries() {
    assert_eq!(icon_file("Apache"), Some("apache.ico"));
    assert_eq!(icon_file("Nginx"), Some("nginx.ico"));
    assert_eq!(icon_file("PHP-FPM"), Some("php.ico"));
    assert_eq!(icon_file("MySQL"), Some("mysql.ico"));
    assert_eq!(icon_file("PostgreSQL"), Some("postgresql.ico"));
    assert_eq!(icon_file("Redis"), Some("redis.ico"));
    assert_eq!(icon_file("phpMyAdmin"), Some("phpmyadmin.ico"));
    assert_eq!(icon_file("Adminer"), Some("adminer.ico"));
    assert_eq!(icon_file("Composer"), Some("composer.ico"));
    assert_eq!(icon_file("Node.js"), Some("nodejs.ico"));
    assert_eq!(icon_file("Go"), Some("go.ico"));
    assert_eq!(icon_file("Mailpit"), Some("mailpit.ico"));
    assert_eq!(icon_file("RabbitMQ"), Some("rabbitmq.ico"));
    assert_eq!(icon_file("MinIO"), Some("minio.ico"));
    // Every language the original drew a card icon for, and none invented.
    for name in [
        "Python", "Java", "Julia", "Zig", "Dart", "Lua", "Ruby", "Rust", "Kotlin", "Haskell",
        "Elixir", "Crystal", "Scala", "Swift", "pgweb",
    ] {
        let file = icon_file(name).unwrap_or_else(|| panic!("{name} has an icon"));
        assert!(file.ends_with(".ico"), "{file}");
        assert_eq!(file, file.to_lowercase(), "{file} is a file name on disk");
    }
    assert_eq!(icon_file("Cache"), None);
    assert_eq!(icon_file(""), None);

    // The path is the original's: `{base}/assets/icons/<file>`, which the
    // installation's own assets fill.
    let base = Path::new("C:\\Lambo");
    assert_eq!(
        icon_path(base, "apache.ico"),
        base.join("assets").join("icons").join("apache.ico")
    );
}

#[test]
fn the_configure_button_does_what_the_original_branch_table_did() {
    let temp = TempDir::new();
    let base = temp.path();

    let mut runtime = conf("Node.js", "runtime", true, 0, "");
    runtime.config_file = "conf/php/php.ini".to_owned();
    let mut tool = conf("phpMyAdmin", "tool", true, 0, "");
    tool.open_url = "http://localhost/phpmyadmin".to_owned();
    let mut editable = conf("PHP-FPM", "php", true, 9000, "{base}/bin/php/php-cgi.exe");
    editable.config_file = "conf/php/php.ini".to_owned();
    let bare = conf("Cache", "cache", true, 0, "");

    let stack = stack_of(base, vec![runtime, tool, editable, bare]);
    let services = stack.services();

    // A runtime gets a terminal, whatever else it has.
    assert_eq!(
        configure_action(&services[0], base),
        ConfigureAction::Terminal
    );
    // No configuration file, but a URL: open it.
    assert_eq!(
        configure_action(&services[1], base),
        ConfigureAction::Url("http://localhost/phpmyadmin".to_owned())
    );
    // A configuration file: the editor, at the path `{base}` expands to.
    let expanded = PathBuf::from(expand_path("conf/php/php.ini", base));
    assert_eq!(
        configure_action(&services[2], base),
        ConfigureAction::Editor(expanded)
    );
    // Neither: nothing, and the log says so.
    assert_eq!(
        configure_action(&services[3], base),
        ConfigureAction::Nothing
    );
    assert_eq!(no_config_file_line("Cache"), "[Cache] no config file");

    // The refusal a click on an inactive web server's card logs.
    assert_eq!(
        inactive_web_server_line("Nginx"),
        "[Nginx] not the active web server — set Active Web Server to Nginx first"
    );
    // And the picker's own line.
    assert_eq!(web_server_line("Nginx"), "active web server: Nginx");
}

#[test]
fn the_other_pages_have_a_place_for_every_control() {
    let window = Layout::compute(8);

    let editor = EditorLayout::compute(&window);
    assert_eq!(editor.dropdown, (50, 30, 360, 26));
    assert_eq!(editor.save, (420, 28, 84, 26));
    assert_eq!(editor.reload.0, editor.save.0 + 92, "save, then reload");
    assert_eq!(editor.content.2, CONTENT_W - 20);
    assert!(
        editor.content.1 + editor.content.3 <= window.content_h + CONTENT_Y + 40,
        "the text area must stay above the progress strip"
    );
    assert_eq!(editor.path_value.2, CONTENT_W - 60);

    let vhosts = VhostsLayout::compute(&window);
    assert_eq!(vhosts.list, (10, 28, CONTENT_W - 20, 160));
    assert_eq!(vhosts.domain_ext.0, vhosts.domain_name.0 + 144);
    assert_eq!(vhosts.server.0, 470);
    assert_eq!(
        (vhosts.save.2, vhosts.delete.2, vhosts.apply.2),
        (90, 90, 140)
    );
    assert_eq!(vhosts.apply.0, 202);
    assert_eq!(vhosts.docroot.2, CONTENT_W - 100);
    // The form sits under the list: every control below the table.
    assert!(vhosts.domain_label.1 > vhosts.list.1 + vhosts.list.3);
    assert!(vhosts.docroot.1 > vhosts.domain_name.1);
    assert!(vhosts.save.1 > vhosts.docroot.1);

    let projects = ProjectsLayout::compute(&window);
    assert_eq!(projects.create, (705, 28, 95, 26));
    assert_eq!(projects.actions.len(), 3, "one per ProjectActionId");
    assert_eq!(projects.actions[0], (10, 288, 140, 26));
    assert_eq!(projects.list.2, CONTENT_W - 20);
    assert!(
        projects.actions[2].0 + projects.actions[2].2 <= CONTENT_W,
        "the project buttons must fit the page"
    );
    assert_eq!(
        projects.runtime.1 + 24,
        projects.list_label.1,
        "the caption follows the status line"
    );
}

// ---------------------------------------------------------------------------
// The progress strip, the log and the editor
// ---------------------------------------------------------------------------

#[test]
fn the_progress_strip_reports_each_stage_of_an_install() {
    assert_eq!(PROGRESS_MAX, 1000);
    assert_eq!(
        ProgressView::idle(),
        ProgressView {
            label: "Idle".to_owned(),
            position: 0
        }
    );

    let starting = ProgressView::new(Stage::Starting, "Apache", 0, 0);
    assert_eq!(starting.label, "Starting Apache ...");
    assert_eq!(starting.position, 0);

    let downloading = ProgressView::new(Stage::Downloading, "Apache", 5_242_880, 10_485_760);
    assert_eq!(
        downloading.label,
        "Downloading Apache  50.0%  5.0 / 10.0 MB"
    );
    assert_eq!(downloading.position, 500);

    // No total: the bytes still come, the bar cannot move.
    let unknown = ProgressView::new(Stage::Downloading, "Apache", 1_048_576, 0);
    assert_eq!(unknown.label, "Downloading Apache  1.0 MB");
    assert_eq!(unknown.position, 0);

    let extracting = ProgressView::new(Stage::Extracting, "Apache", 3, 6);
    assert_eq!(extracting.label, "Extracting Apache  3 / 6 files");
    assert_eq!(extracting.position, 500);
    // An extraction with no entry count sits halfway, as the original's did.
    let counting = ProgressView::new(Stage::Extracting, "Apache", 0, 0);
    assert_eq!(counting.label, "Extracting Apache ...");
    assert_eq!(counting.position, PROGRESS_MAX / 2);

    let hook = ProgressView::new(Stage::PostInstall, "Apache", 0, 0);
    assert_eq!(hook.label, "Running post-install for Apache ...");
    assert_eq!(hook.position, PROGRESS_MAX);

    let done = ProgressView::new(Stage::Done, "Apache", 0, 0);
    assert_eq!(done.label, "Installed Apache");
    assert_eq!(done.position, PROGRESS_MAX);

    // The bar truncates, as the original's did.
    assert_eq!(
        ProgressView::new(Stage::Downloading, "x", 1, 3).position,
        333
    );
    assert_eq!(ProgressView::new(Stage::Downloading, "x", 0, 3).position, 0);
}

#[test]
fn the_log_ring_keeps_the_newest_text_and_drops_the_oldest_quarter() {
    let mut log = LogBuffer::new();
    assert!(log.is_empty());
    assert_eq!(log.len(), 0);
    log.push_at("15:04:05", "Lambo PHP started");
    assert_eq!(log.text(), "15:04:05 Lambo PHP started\r\n");
    assert!(!log.is_empty());

    // The limit is the original's 200 KiB. A line of 200 characters with a
    // nine-character timestamp is 209 bytes, so 1500 of them are 300 KiB: the
    // buffer has to have trimmed more than once by the end.
    let mut log = LogBuffer::new();
    log.push_at("00:00:00", "FIRST");
    for _ in 0..1500 {
        log.push_at("00:00:00", &"y".repeat(199));
    }
    log.push_at("00:00:00", "SENTINEL");

    let text = log.text().to_owned();
    assert!(log.len() <= LOG_LIMIT, "{} bytes kept", log.len());
    assert!(
        log.len() > LOG_LIMIT / 2,
        "the buffer is filled, not emptied: {} bytes",
        log.len()
    );
    assert!(text.contains("SENTINEL"), "the newest line survives");
    assert!(
        !text.contains("FIRST"),
        "the oldest text is the first to go"
    );
    assert!(text.starts_with("00:00:00 "), "the drop lands on a line");
    assert!(text.ends_with("\r\n"));

    log.clear();
    assert!(log.is_empty());

    // The clock, and the middle-truncation the dropdown uses.
    assert_eq!(time_of_day(9, 5, 7), "09:05:07");
    assert_eq!(time_of_day(23, 59, 59), "23:59:59");
    assert_eq!(truncate_mid("short", 60), "short");
    assert_eq!(truncate_mid("abcdef", 3), "abcdef");
    let cut = truncate_mid(&"a".repeat(200), 30);
    assert_eq!(
        cut.len(),
        29,
        "the original spent three chars on the ellipsis"
    );
    assert!(cut.starts_with("aaaaaaaaaaaaa..."), "{cut}");
    assert!(cut.ends_with("aaaaaaaaaaaaa"), "{cut}");
    assert_eq!(
        truncate_mid("C:\\Lambo\\conf\\apache\\vhosts.conf", 60),
        "C:\\Lambo\\conf\\apache\\vhosts.conf"
    );
}

#[test]
fn the_editor_offers_the_files_that_exist_and_loads_the_first() {
    let temp = TempDir::new();
    let base = temp.path();
    let mut config = PanelConfig::default_config();
    // Written the way the configuration writes it, with the placeholder the
    // installation expands.
    config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();

    // A service whose configuration file exists, and one whose does not.
    let mut php = conf("PHP-FPM", "php", true, 9000, "{base}/bin/php/php-cgi.exe");
    php.config_file = "{base}/conf/php/php.ini".to_owned();
    let mut ghost = conf("Ghost", "tool", true, 0, "");
    ghost.config_file = "{base}/conf/ghost.ini".to_owned();
    let stack = stack_of(base, vec![php, ghost]);
    let services = stack.services();

    // The installation's own files.
    std::fs::create_dir_all(base.join("conf/php")).expect("the php config directory");
    std::fs::create_dir_all(base.join("conf/apache")).expect("the apache directory");
    std::fs::write(base.join("conf/php/php.ini"), "; php\r\n").expect("php.ini");
    std::fs::write(base.join("conf/apache/vhosts.conf"), "# vhosts\r\n").expect("the include");
    let config_file = crate::panel::config_path(base);
    std::fs::create_dir_all(config_file.parent().expect("it has a directory"))
        .expect("the config directory");
    std::fs::write(&config_file, "{}\r\n").expect("config.json");

    let files = editor_files(base, &config, services);
    let labels: Vec<&str> = files.iter().map(|file| file.label.as_str()).collect();
    assert_eq!(
        labels[0], "PHP-FPM",
        "the service's configuration comes first"
    );
    assert_eq!(labels[1], "Lambo PHP config");
    assert_eq!(labels[2], "Apache vhosts");
    assert!(
        labels[3..].iter().all(|label| *label == "Windows hosts"),
        "the hosts file is the original's last entry, when it exists: {labels:?}"
    );
    assert!(
        !labels.contains(&"Ghost"),
        "a file that is not on disk is not offered: {labels:?}"
    );
    assert_eq!(
        files[0].path,
        PathBuf::from(expand_path("{base}/conf/php/php.ini", base))
    );
    assert_eq!(
        files[0].label, "PHP-FPM",
        "the entry carries the service's name"
    );

    // The label names the file and its path, shortened the original's way.
    let label = editor_label(&files[1]);
    assert!(label.starts_with("Lambo PHP config  —  "), "{label}");
    assert!(label.contains("config.json"), "{label}");

    // Opening the page loads the first entry.
    let mut editor = Editor::open(base, &config, services);
    assert_eq!(editor.selected, Some(0));
    assert_eq!(editor.loaded, Some(files[0].path.clone()));
    assert_eq!(editor.status_text(), files[0].path.display().to_string());
    assert!(editor.failure.is_none());
    assert!(!editor.has_unsaved_changes());

    // Selecting another entry loads it, and the line says which.
    let line = editor.select(1).expect("there is a second file");
    assert!(line.starts_with("editor: loaded "), "{line}");
    assert_eq!(editor.selected, Some(1));
    assert_eq!(editor.select(99), None, "there is no such entry");

    // An installation with nothing of its own to offer still opens an editor.
    // The system hosts file exists on the machine this runs on, so "nothing of
    // its own" means at most that one entry - never anything else.
    let empty = TempDir::new();
    let mut bare = Editor::open(empty.path(), &config, &[]);
    match bare.files.as_slice() {
        [] => {
            assert_eq!(bare.status_text(), "(no file loaded)");
            assert!(bare.save().ends_with("no file loaded"));
            assert_eq!(bare.select(0), None);
        }
        [hosts] => {
            assert_eq!(hosts.label, "Windows hosts", "{hosts:?}");
            assert_eq!(bare.selected, Some(0));
            assert_eq!(bare.loaded.as_ref(), Some(&hosts.path));
            assert_eq!(bare.select(99), None, "there is still only one entry");
        }
        other => panic!("an empty installation offers nothing else: {other:?}"),
    }
}

#[test]
fn the_editor_normalises_to_crlf_and_saves_atomically() {
    let temp = TempDir::new();
    let base = temp.path();
    let path = base.join("config.json");
    std::fs::create_dir_all(base).expect("the directory");
    let lf = "{\n  \"version\": 1\n}\n";
    std::fs::write(&path, lf).expect("the file");

    let mut editor = Editor::default();
    let line = editor.load(&path);
    assert!(line.starts_with("editor: loaded "), "{line}");
    assert_eq!(editor.text.matches("\r\n").count(), 3, "LF became CRLF");
    assert!(!editor.text.contains("\n\n"), "no line endings are doubled");
    assert!(!editor.has_unsaved_changes());

    // An edit is visible as unsaved, and saving writes exactly what is in the
    // buffer - as bytes, atomically.
    let edited = "{\r\n  \"version\": 2\r\n}\r\n";
    editor.set_text(edited.to_owned());
    assert!(editor.has_unsaved_changes());
    let saved = editor.save();
    assert!(
        saved.ends_with("(22 bytes)"),
        "the line reports the size: {saved}"
    );
    assert!(!editor.has_unsaved_changes());
    assert_eq!(
        std::fs::read_to_string(&path).expect("it was written"),
        edited
    );
    // The same text is not an edit.
    editor.set_text(edited.to_owned());
    assert!(!editor.has_unsaved_changes());
}

#[test]
fn the_editor_reports_a_file_it_cannot_read_and_saves_nothing() {
    let temp = TempDir::new();
    let missing = temp.path().join("gone.conf");

    let mut editor = Editor::default();
    let line = editor.load(&missing);
    assert!(line.starts_with("editor: "), "{line}");
    assert!(line.contains("gone.conf"), "{line}");
    assert!(editor.failure.is_some());
    assert!(
        editor.status_text().starts_with("(failed to read: "),
        "{}",
        editor.status_text()
    );
    assert!(editor.text.is_empty());
    assert_eq!(editor.save(), "editor: no file loaded");
    assert_eq!(
        editor.select(0),
        None,
        "nothing is selected when nothing is offered"
    );

    // An editor that is not holding a file refuses to save, with the original's
    // line.
    let mut fresh = Editor::default();
    assert_eq!(fresh.save(), "editor: no file loaded");
    assert_eq!(fresh.status_text(), "(no file loaded)");
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[test]
fn the_settings_page_lists_the_paths_the_state_and_the_about_rows() {
    let temp = TempDir::new();
    let base = temp.path();
    let mut config = PanelConfig::default_config();
    config.services[0].enabled = true;
    config.vhosts = vec![vhost_of("app.test")];
    config.projects = vec![project_of("app", "Laravel", "app.test")];

    let view = settings_view(SettingsInput {
        base_dir: base,
        config: &config,
        elevated: false,
        auto_start: false,
        version: "0.14.0-rc.1",
        repository: "https://example.invalid/lambo",
        homepage: "https://example.invalid",
    });

    // The three sections, in the original's order.
    let titles: Vec<&str> = view
        .blocks
        .iter()
        .filter_map(|block| match block {
            SettingsBlock::Section(section) => Some(section.title),
            _ => None,
        })
        .collect();
    assert_eq!(titles, vec!["PATHS", "STATE", "ABOUT"]);
    assert_eq!(view.blocks[1], SettingsBlock::Gap(8));
    assert!(matches!(view.blocks[4], SettingsBlock::Actions(_)));

    // Every path row a user needs to find the installation's files.
    assert_eq!(value_of(&view, "Install dir"), base.display().to_string());
    assert_eq!(
        value_of(&view, "Config"),
        crate::panel::config_path(base).display().to_string()
    );
    assert_eq!(
        value_of(&view, "Apache vhosts"),
        expand_path(&config.settings.apache_vhosts_include, base)
    );
    assert_eq!(
        value_of(&view, "Nginx sites"),
        expand_path(&config.settings.nginx_sites_dir, base)
    );
    assert_eq!(
        value_of(&view, "Hosts file"),
        crate::vhost::default_hosts_file().display().to_string()
    );
    assert_eq!(
        value_of(&view, "Downloads cache"),
        base.join(DOWNLOADS_DIR).display().to_string()
    );
    // The configured hosts file is named only when it is set.
    assert!(
        view.blocks
            .iter()
            .all(|block| !format!("{block:?}").contains("Hosts file (configured)"))
    );

    // The state: services, vhosts, projects, the active server, auto-start and
    // elevation.
    assert_eq!(
        value_of(&view, "Services"),
        format!(
            "{} total · {} enabled",
            config.services.len(),
            config.enabled_service_count()
        )
    );
    assert_eq!(value_of(&view, "Vhosts"), "1");
    assert_eq!(value_of(&view, "Projects"), "1");
    assert_eq!(value_of(&view, "Active web server"), "Apache");
    assert_eq!(value_of(&view, "Auto-start on boot"), AUTO_START_OFF);
    assert!(value_of(&view, "Running elevated").contains("no"));
    assert!(value_of(&view, "Config version").starts_with(&config.version.to_string()));

    // About: this product, its version and where it comes from - and nothing
    // about anyone else's.
    assert_eq!(value_of(&view, "Product"), crate::PRODUCT);
    assert_eq!(value_of(&view, "Version"), "0.14.0-rc.1");
    assert_eq!(
        value_of(&view, "Repository"),
        "https://example.invalid/lambo"
    );
    assert_eq!(value_of(&view, "Homepage"), "https://example.invalid");

    // A configured hosts file is named as well as the platform's, because that
    // is which one an apply will use.
    config.settings.hosts_file = "conf/hosts.custom".to_owned();
    let with_hosts = settings_view(SettingsInput {
        base_dir: base,
        config: &config,
        elevated: true,
        auto_start: true,
        version: "0.14.0-rc.1",
        repository: "",
        homepage: "",
    });
    assert_eq!(
        value_of(&with_hosts, "Hosts file (configured)"),
        expand_path("conf/hosts.custom", base)
    );
    assert_eq!(value_of(&with_hosts, "Auto-start on boot"), AUTO_START_ON);
    assert!(value_of(&with_hosts, "Running elevated").contains("administrator"));
}

#[test]
fn the_settings_actions_are_the_originals_and_admin_depends_on_elevation() {
    let normal = settings_actions(false);
    let labels: Vec<&str> = normal.iter().map(|button| button.label).collect();
    assert_eq!(
        labels,
        vec![
            "Edit config",
            "Reload config",
            "Toggle Auto-start",
            "Restart as Admin",
            "Add tools to PATH",
            "Remove from PATH",
            "Open psql Console",
            "Quit Lambo PHP",
            "\u{2605} Star on GitHub",
        ]
    );

    // Already elevated: the button that relaunches as administrator is not
    // offered, because it would do nothing.
    let elevated = settings_actions(true);
    let labels: Vec<&str> = elevated.iter().map(|button| button.label).collect();
    assert_eq!(labels.len(), 8);
    assert!(!labels.contains(&"Restart as Admin"));
    assert!(labels.contains(&"Open psql Console"));
    assert!(labels.contains(&"\u{2605} Star on GitHub"));

    // Every action is reachable, and each is painted as its role.
    for action in [
        SettingsAction::EditConfig,
        SettingsAction::ReloadConfig,
        SettingsAction::ToggleAutoStart,
        SettingsAction::RestartAsAdmin,
        SettingsAction::AddToPath,
        SettingsAction::RemoveFromPath,
        SettingsAction::PsqlConsole,
        SettingsAction::Quit,
        SettingsAction::OpenRepository,
    ] {
        assert!(
            normal.iter().any(|button| button.action == action),
            "{action:?} is offered"
        );
    }
    let find = |action| {
        normal
            .iter()
            .find(|button| button.action == action)
            .expect("it is offered")
    };
    assert_eq!(find(SettingsAction::EditConfig).scheme, Scheme::Primary);
    assert_eq!(find(SettingsAction::AddToPath).scheme, Scheme::Success);
    assert_eq!(find(SettingsAction::RemoveFromPath).scheme, Scheme::Warning);
    assert_eq!(find(SettingsAction::Quit).scheme, Scheme::Danger);
    assert!(normal.iter().all(|button| button.enabled));
}

#[test]
fn the_settings_layout_is_the_originals_section_and_button_grid() {
    let temp = TempDir::new();
    let base = temp.path();
    let config = PanelConfig::default_config();
    let view = settings_view(SettingsInput {
        base_dir: base,
        config: &config,
        elevated: false,
        auto_start: false,
        version: "0.14.0-rc.1",
        repository: "",
        homepage: "",
    });
    let items = settings_layout(&view);

    // Three sections, each with a title and its rule.
    let sections: Vec<&SettingsItem> = items
        .iter()
        .filter(|item| item.kind == SettingsItemKind::Section)
        .collect();
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[0].text, "PATHS");
    assert!(
        items
            .iter()
            .any(|item| item.kind == SettingsItemKind::Divider)
    );

    // Every row is a label and a value on the same line, and the rows of a
    // section are 18 pixels apart.
    let labels: Vec<&SettingsItem> = items
        .iter()
        .filter(|item| item.kind == SettingsItemKind::Label)
        .collect();
    assert!(labels.len() >= 16, "{} rows", labels.len());
    for label in &labels {
        let value = items
            .iter()
            .find(|item| {
                item.kind == SettingsItemKind::Value
                    && item.rect.1 == label.rect.1
                    && item.text == value_of(&view, label.text.as_str())
            })
            .unwrap_or_else(|| panic!("no value for {}", label.text));
        assert!(
            value.rect.0 > label.rect.0,
            "{} comes after its label",
            label.text
        );
    }
    assert_eq!(labels[1].rect.1 - labels[0].rect.1, 18);

    // The section's own cost: 26 pixels before its first row.
    assert_eq!(sections[0].rect.0, 10);
    assert_eq!(labels[0].rect.1, sections[0].rect.1 + 26);

    // The actions are a four-column grid of 170x28 with a 10-pixel gutter and
    // an 8-pixel row gap, in the order the buttons are in.
    let actions: Vec<&SettingsItem> = items
        .iter()
        .filter(|item| matches!(item.kind, SettingsItemKind::Action(_)))
        .collect();
    assert_eq!(actions.len(), 9);
    for (index, action) in actions.iter().enumerate() {
        assert_eq!(
            action.kind,
            SettingsItemKind::Action(index),
            "the grid keeps the button order"
        );
        assert_eq!((action.rect.2, action.rect.3), (170, 28));
        assert_eq!(action.rect.0, 20 + (index as i32 % 4) * 180);
    }
    assert_eq!(actions[0].rect.1, actions[3].rect.1, "the first row");
    assert_eq!(actions[4].rect.1, actions[0].rect.1 + 36, "the second row");
    assert_eq!(
        actions[0].text, "Edit config",
        "each grid cell carries its label"
    );

    // Nothing overlaps the content area and the page grows downward.
    assert!(
        items
            .iter()
            .all(|item| item.rect.0 + item.rect.2 <= CONTENT_W)
    );
    assert!(items.windows(2).all(|pair| pair[1].rect.1 >= 0));
}

// ---------------------------------------------------------------------------
// Projects and virtual hosts
// ---------------------------------------------------------------------------

#[test]
fn the_projects_form_composes_the_domain_the_original_composed() {
    let form = ProjectForm {
        framework: "Laravel".to_owned(),
        name: "My Shop".to_owned(),
        domain_name: "shop".to_owned(),
        extension: ".test".to_owned(),
    };
    assert_eq!(form.domain(), "shop.test");

    // No extension: `.test`, the original's default.
    let mut no_ext = form.clone();
    no_ext.extension = String::new();
    assert_eq!(no_ext.domain(), "shop.test");

    // No name part: the project's own slug, which is the same rule the CLI
    // applies.
    let mut no_name = form.clone();
    no_name.domain_name = String::new();
    no_name.name = "My Shop!".to_owned();
    assert_eq!(no_name.domain(), "my-shop.test");

    // Trimmed on both sides, and an extension other than the default is kept.
    let mut other = form.clone();
    other.domain_name = "  shop  ".to_owned();
    other.extension = ".local".to_owned();
    assert_eq!(other.domain(), "shop.local");

    // A form the user has not filled in asks for a name that does not exist:
    // the engine refuses it, with the message the form shows.
    let blank = ProjectForm::default();
    assert_eq!(blank.domain(), ".test");
}

#[test]
fn the_projects_table_and_its_buttons_are_the_originals() {
    let projects = vec![
        project_of("shop", "Laravel", "shop.test"),
        project_of("api", "Symfony", "api.local"),
    ];
    let rows = project_rows(&projects);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "shop");
    assert_eq!(rows[0].framework, "Laravel");
    assert_eq!(rows[0].domain, "shop.test");
    assert_eq!(rows[0].docroot, "C:\\Lambo\\www\\shop");
    assert_eq!(rows[1].domain, "api.local");

    // The three buttons, in the original's order: open, open the folder, delete.
    let actions = projects_actions();
    assert_eq!(
        actions
            .iter()
            .map(|action| action.id)
            .collect::<Vec<ProjectActionId>>(),
        vec![
            ProjectActionId::OpenInBrowser,
            ProjectActionId::OpenFolder,
            ProjectActionId::Delete
        ]
    );
    assert_eq!(actions[0].label, "Open in Browser");
    assert_eq!(actions[1].label, "Open Folder");
    assert_eq!(actions[2].label, "Delete");
    assert_eq!(actions[0].scheme, Scheme::Primary);
    assert_eq!(actions[1].scheme, Scheme::Neutral);
    assert_eq!(actions[2].scheme, Scheme::Danger);

    // The layout gives each button the original's width, left to right.
    let layout = ProjectsLayout::compute(&Layout::compute(1));
    assert_eq!(layout.actions[0].2, 140);
    assert_eq!(layout.actions[1].2, 110);
    assert_eq!(layout.actions[2].2, 90);
    assert_eq!(layout.actions[1].0, layout.actions[0].0 + 146);
    assert_eq!(layout.actions[2].0, layout.actions[1].0 + 116);
    assert_eq!(layout.list.2, CONTENT_W - 20);
    assert!(layout.list.1 > layout.list_label.1);
    assert!(layout.runtime.1 > layout.framework.1);

    // The URL a project answers on, and the two lines its creation and deletion
    // log.
    assert_eq!(project_url(&projects[0]), "http://shop.test");
    assert_eq!(
        project_creation_log_line("shop", "Laravel", "shop.test"),
        "projects: creating 'shop' (Laravel) at shop.test ..."
    );
    assert_eq!(
        project_delete_log_line("shop", "shop.test"),
        "projects: deleting 'shop' (shop.test)..."
    );
}

#[test]
fn the_projects_page_offers_every_framework_and_the_runtime_status() {
    // Every framework the engine can scaffold, in the catalogue's order, with
    // nothing added or dropped.
    let names = framework_names();
    assert_eq!(names.len(), crate::frameworks::frameworks().len());
    assert_eq!(names.len(), 17, "the original's framework count");
    assert!(names.contains(&"Laravel"));
    assert!(names.contains(&"WordPress"));
    assert!(names.contains(&"Spring Boot"));
    assert_eq!(names[0], crate::frameworks::frameworks()[0].name);

    // The runtime line is the engine's, not a second copy of it.
    let temp = TempDir::new();
    let status = crate::frameworks::runtime_status_text(temp.path());
    for tool in ["PHP", "Composer", "Node.js", "Python", "Java", "Go"] {
        assert!(status.contains(tool), "{status} names {tool}");
    }
    assert_eq!(
        status.matches('✓').count() + status.matches('○').count(),
        6,
        "{status}"
    );
    assert!(
        status.starts_with('✓') || status.starts_with('○'),
        "{status}"
    );
}

#[test]
fn the_vhost_form_selects_the_extension_and_the_server_and_the_table_is_the_engines() {
    // The server combo's contents and its index, including the original's
    // fallback for anything it does not know.
    assert_eq!(SERVER_OPTIONS, ["apache", "nginx", "both"]);
    assert_eq!(server_index("apache"), 0);
    assert_eq!(server_index("nginx"), 1);
    assert_eq!(server_index("both"), 2);
    assert_eq!(server_index(""), 0);
    assert_eq!(server_index("Apache"), 0, "the original matched exactly");
    assert_eq!(server_index("caddy"), 0, "and defaulted to apache");
    assert_eq!(server_at(0), "apache");
    assert_eq!(server_at(1), "nginx");
    assert_eq!(server_at(2), "both");
    assert_eq!(server_at(99), "apache", "an index past the end is Apache");

    // The table's rows are the engine's rows: the page only says how many.
    let temp = TempDir::new();
    let base = temp.path();
    let vhosts = vec![
        vhost_of("app.test"),
        Vhost {
            domain: "api.local".to_owned(),
            docroot: "{base}/www/api/public".to_owned(),
            port: 8080,
            server_type: "nginx".to_owned(),
            enabled: false,
            proxy_port: 0,
        },
    ];
    let rows = vhost_list(&vhosts, base);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows,
        vhosts
            .iter()
            .map(|vhost| crate::vhost::vhost_row(vhost, base))
            .collect::<Vec<_>>()
    );
    // The five cells, with the two defaults the page used to show: no port is
    // port 80, and no server type is apache.
    assert_eq!(rows[0].marker, "\u{2713}");
    assert_eq!(rows[0].domain, "app.test");
    assert_eq!(rows[0].port, 80);
    assert_eq!(rows[0].server, "apache");
    assert_eq!(rows[1].marker, " ", "a disabled host is not marked");
    assert_eq!(rows[1].domain, "api.local");
    assert_eq!(
        rows[0].docroot,
        expand_path("{base}/www/app.test", base),
        "a document root is shown expanded, as the original showed it"
    );
    assert_eq!(rows[1].port, 8080);
    assert_eq!(rows[1].server, "nginx");
    assert!(
        !vhost_list(&[], base).iter().any(|_| true),
        "no hosts, no rows"
    );
}

// ---------------------------------------------------------------------------
// The window itself
// ---------------------------------------------------------------------------

#[test]
fn the_window_is_titled_and_the_status_bar_names_the_installation() {
    assert_eq!(WINDOW_TITLE, "Lambo PHP — Local Web Stack Control Panel");
    assert!(WINDOW_TITLE.starts_with(crate::PRODUCT));

    // The system's preference decides the palette, and nothing else.
    assert_eq!(theme(true), Theme::Dark);
    assert_eq!(theme(false), Theme::Light);

    let temp = TempDir::new();
    let base = temp.path();
    let parts = status_parts(Page::Services, base, "0.14.0-rc.1");
    assert_eq!(parts[0], "Page: services");
    assert_eq!(parts[1], truncate_mid(&base.display().to_string(), 70));
    assert_eq!(parts[2], "Lambo PHP 0.14.0-rc.1");
    assert!(STATUS_PART_WIDTHS[0] + STATUS_PART_WIDTHS[2] < WINDOW_W);
    assert_eq!(
        STATUS_PART_WIDTHS[1], 0,
        "the middle part takes what is left"
    );

    // A long installation path in the middle does not push the version out.
    let long = PathBuf::from(format!("C:\\{}", "x".repeat(120)));
    let parts = status_parts(Page::Settings, &long, "0.14.0-rc.1");
    assert!(parts[1].len() <= 70, "{} chars", parts[1].len());
    assert!(parts[1].contains("..."), "{}", parts[1]);
    assert_eq!(parts[2], "Lambo PHP 0.14.0-rc.1");
}

#[test]
fn the_path_and_startup_lines_are_the_originals_rebranded() {
    let temp = TempDir::new();
    let base = temp.path();
    let lines = startup_lines(base, 12, 3, "15:04:05");
    assert_eq!(lines[0], "Lambo PHP started at 15:04:05");
    assert_eq!(lines[1], format!("Base dir: {}", base.display()));
    assert_eq!(lines[2], "Loaded 12 services, 3 vhosts from config.json");

    // The five PATH messages, with the counts the engine returned.
    assert_eq!(
        path_lines(PathAction::Add, 3, None),
        vec![
            "path: added 3 Lambo bin dirs to user PATH".to_owned(),
            "path: open a NEW terminal to use them".to_owned(),
        ]
    );
    assert_eq!(
        path_lines(PathAction::Add, 0, None),
        vec!["path: already on PATH (no changes)".to_owned()]
    );
    assert_eq!(
        path_lines(PathAction::Remove, 2, None),
        vec!["path: removed 2 Lambo entries from user PATH".to_owned()]
    );
    assert_eq!(
        path_lines(PathAction::Remove, 0, None),
        vec!["path: nothing to remove".to_owned()]
    );
    assert_eq!(
        path_lines(PathAction::Add, 0, Some("access denied")),
        vec!["path: access denied".to_owned()],
        "an error replaces the outcome"
    );
    assert_eq!(tray_failure_line("no icon"), "tray: no icon");
}

#[test]
fn nothing_the_panel_says_names_the_other_product() {
    let temp = TempDir::new();
    let base = temp.path();
    let config = PanelConfig::default_config();
    let stack = stack_of(
        base,
        vec![conf(
            "Apache",
            "web",
            true,
            80,
            "{base}/bin/apache/bin/httpd.exe",
        )],
    );

    let said = format!(
        "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
        settings_view(SettingsInput {
            base_dir: base,
            config: &config,
            elevated: false,
            auto_start: true,
            version: "0.14.0-rc.1",
            repository: "https://example.invalid/lambo-php",
            homepage: "https://example.invalid",
        }),
        card_view(&stack.services()[0], "Apache"),
        CardLayout::compute(true),
        ProgressView::new(Stage::Done, "Apache", 0, 0),
        projects_actions(),
        startup_lines(base, 1, 0, "00:00:00"),
        status_parts(Page::Vhosts, base, "0.14.0-rc.1"),
        path_lines(PathAction::Add, 3, None),
        crate::console::terminal_title("Go"),
        crate::vhost::vhost_row(&vhost_of("app.test"), base),
        Page::ALL.map(|page| page.label()),
        ProjectRow {
            name: "shop".to_owned(),
            framework: "Laravel".to_owned(),
            domain: "shop.test".to_owned(),
            docroot: "C:\\Lambo\\www\\shop".to_owned(),
        },
    )
    .to_lowercase();

    for other in ["goampp", "saweria", "imtaqin", "donate", "fdciabdul"] {
        assert!(
            !said.contains(other),
            "the panel still says {other}: {said}"
        );
    }
    // The rebrand is a rename, not a removal: the product's own name is there.
    assert!(said.contains("lambo php"));
}

// ---------------------------------------------------------------------------
// The controls the window creates
// ---------------------------------------------------------------------------

#[test]
fn every_widget_number_maps_back_to_the_widget_it_came_from() {
    let ids = [
        WidgetId::Page(Page::Services),
        WidgetId::Page(Page::Settings),
        WidgetId::Tab(0),
        WidgetId::Tab(4),
        WidgetId::WebPicker,
        WidgetId::StartStack,
        WidgetId::StopAll,
        WidgetId::RestartStack,
        WidgetId::Status(0),
        WidgetId::Status(2),
        WidgetId::Progress,
        WidgetId::ProgressLabel,
        WidgetId::Log,
        WidgetId::Card {
            index: 0,
            part: CardPart::Toggle,
        },
        WidgetId::Card {
            index: 7,
            part: CardPart::Version,
        },
        WidgetId::Version {
            index: 0,
            variant: 0,
        },
        WidgetId::Version {
            index: 3,
            variant: 5,
        },
        WidgetId::EditorFile,
        WidgetId::EditorSave,
        WidgetId::EditorReload,
        WidgetId::EditorText,
        WidgetId::EditorPath,
        WidgetId::VhostList,
        WidgetId::VhostDomainName,
        WidgetId::VhostDomainExt,
        WidgetId::VhostPort,
        WidgetId::VhostServer,
        WidgetId::VhostDocroot,
        WidgetId::VhostSave,
        WidgetId::VhostDelete,
        WidgetId::VhostApply,
        WidgetId::LandingStart,
        WidgetId::LandingWelcome,
        WidgetId::LandingDashboard,
        WidgetId::ProjectFramework,
        WidgetId::ProjectName,
        WidgetId::ProjectDomainName,
        WidgetId::ProjectDomainExt,
        WidgetId::ProjectCreate,
        WidgetId::ProjectList,
        WidgetId::ProjectLocation,
        WidgetId::ProjectBrowse,
        WidgetId::ProjectAdopt,
        WidgetId::ProjectAction(ProjectActionId::OpenInBrowser),
        WidgetId::ProjectAction(ProjectActionId::OpenFolder),
        WidgetId::ProjectAction(ProjectActionId::Delete),
        WidgetId::Settings(SettingsAction::EditConfig),
        WidgetId::Settings(SettingsAction::ReloadConfig),
        WidgetId::Settings(SettingsAction::ToggleAutoStart),
        WidgetId::Settings(SettingsAction::AddToPath),
        WidgetId::Settings(SettingsAction::RemoveFromPath),
        WidgetId::Settings(SettingsAction::PsqlConsole),
        WidgetId::Settings(SettingsAction::Quit),
        WidgetId::Settings(SettingsAction::RestartAsAdmin),
        WidgetId::Settings(SettingsAction::OpenRepository),
    ];

    for id in ids {
        assert_eq!(
            WidgetId::from_value(id.value()),
            Some(id),
            "{id:?} reports {}",
            id.value()
        );
    }

    // Every number handed out is distinct, or two controls would answer to the
    // same click.
    let mut numbers: Vec<u16> = ids.iter().map(|id| id.value()).collect();
    numbers.sort_unstable();
    let count = numbers.len();
    numbers.dedup();
    assert_eq!(numbers.len(), count, "the numbering is unique");

    // A caption is never routed, and neither is a number no widget owns.
    assert_eq!(WidgetId::Decoration.value(), 0);
    assert_eq!(WidgetId::from_value(0), None);
    assert_eq!(WidgetId::from_value(1000), None);
    assert_eq!(WidgetId::from_value(1204), None);
    assert_eq!(WidgetId::from_value(1403), None);
    assert_eq!(WidgetId::from_value(3209), None);
    // Six pages, numbered from 1001: the landing page first.
    assert_eq!(
        WidgetId::from_value(1001),
        Some(WidgetId::Page(Page::Landing))
    );
    assert_eq!(
        WidgetId::from_value(1006),
        Some(WidgetId::Page(Page::Settings))
    );
    assert_eq!(WidgetId::from_value(1007), None);
    assert_eq!(
        WidgetId::from_value(2008),
        None,
        "the eighth card slot is free"
    );

    // The tray's identifiers share `WM_COMMAND` with the controls' and are not
    // widgets: the numbering stops where they start.
    assert_eq!(TRAY_BASE, 40000);
    assert_eq!(WidgetId::from_value(TRAY_BASE), None);
    assert_eq!(WidgetId::from_value(40001), None, "the tray's Show");
    assert_eq!(WidgetId::from_value(u16::MAX), None);
    assert_eq!(
        WidgetId::Version {
            index: 339,
            variant: 99
        }
        .value(),
        TRAY_BASE - 1,
        "the last version entry is the last widget number"
    );
    assert_eq!(
        WidgetId::from_value(5999),
        None,
        "and so is the gap before 6000"
    );

    // The ranges themselves: the version menu keeps the original's base, and a
    // card owns sixteen numbers with eight of them used.
    assert_eq!(
        WidgetId::Version {
            index: 0,
            variant: 1
        }
        .value(),
        6001
    );
    assert_eq!(
        WidgetId::from_value(6001),
        Some(WidgetId::Version {
            index: 0,
            variant: 1
        })
    );
    assert_eq!(
        WidgetId::from_value(2099),
        Some(WidgetId::Card {
            index: 6,
            part: CardPart::Status
        })
    );
    assert_eq!(
        WidgetId::Card {
            index: 4,
            part: CardPart::Toggle
        }
        .value(),
        2000 + 4 * 16 + 4
    );
}

#[test]
fn the_sidebar_lists_the_five_pages_and_marks_the_current_one() {
    let widgets = sidebar_widgets(Page::Editor);
    assert_eq!(widgets.len(), Page::SIDEBAR.len());

    for (index, widget) in widgets.iter().enumerate() {
        let page = Page::SIDEBAR[index];
        assert_eq!(widget.id, WidgetId::Page(page));
        assert_eq!(widget.rect, {
            let (x, y) = sidebar_position(index);
            (x, y, SIDE_W, SIDE_H)
        });
        match &widget.kind {
            WidgetKind::Button {
                label,
                scheme,
                enabled,
            } => {
                assert_eq!(label, page.label());
                assert!(*enabled);
                let expected = if page == Page::Editor {
                    Scheme::Primary
                } else {
                    Scheme::Sidebar
                };
                assert_eq!(*scheme, expected, "{page:?}");
            }
            other => panic!("a sidebar entry is a button: {other:?}"),
        }
    }
}

#[test]
fn the_landing_page_offers_the_three_ways_in() {
    let widgets = landing_widgets();

    // The title, the tagline, the three buttons and the hint under them.
    assert_eq!(widgets.len(), 6);
    assert_eq!(widgets[0].id, WidgetId::Decoration, "the title");
    assert_eq!(widgets[1].id, WidgetId::Decoration, "the tagline");
    assert_eq!(widgets[2].id, WidgetId::LandingStart);
    assert_eq!(widgets[3].id, WidgetId::LandingWelcome);
    assert_eq!(widgets[4].id, WidgetId::LandingDashboard);
    assert_eq!(widgets[5].id, WidgetId::Decoration, "the hint");

    // The start button carries the stack's own action, so the landing page
    // and the services page start the stack the same way.
    match &widgets[2].kind {
        WidgetKind::Button {
            label,
            scheme,
            enabled,
        } => {
            assert_eq!(label, "Start Stack & Open Welcome Page");
            assert_eq!(*scheme, Scheme::Primary);
            assert!(*enabled);
        }
        other => panic!("the start button is a button: {other:?}"),
    }
    match &widgets[3].kind {
        WidgetKind::Button { label, .. } => assert_eq!(label, "Open Welcome Page"),
        other => panic!("the welcome button is a button: {other:?}"),
    }
    match &widgets[4].kind {
        WidgetKind::Button { label, .. } => assert_eq!(label, "Open Dashboard"),
        other => panic!("the dashboard button is a button: {other:?}"),
    }
}

#[test]
fn the_toolbar_has_the_picker_and_the_three_stack_buttons() {
    let widgets = toolbar_widgets("Nginx");
    assert_eq!(widgets.len(), 5);

    // The caption first, and it is not routed.
    assert_eq!(widgets[0].id, WidgetId::Decoration);
    assert_eq!(widgets[0].id.value(), 0);

    let picker = &widgets[1];
    assert_eq!(picker.id, WidgetId::WebPicker);
    assert_eq!(picker.rect, WEB_PICKER_RECT);
    match &picker.kind {
        WidgetKind::Combo { items, selected } => {
            assert_eq!(items, &["Apache".to_owned(), "Nginx".to_owned()]);
            assert_eq!(*selected, 1, "the active server is the selected one");
        }
        other => panic!("the picker is a combo: {other:?}"),
    }

    // A server the picker does not list selects the first entry, which is what
    // the original's `Select(0)` did.
    match &toolbar_widgets("Caddy")[1].kind {
        WidgetKind::Combo { selected, .. } => assert_eq!(*selected, 0),
        other => panic!("the picker is a combo: {other:?}"),
    }

    let labels: Vec<String> = widgets[2..]
        .iter()
        .map(|widget| match &widget.kind {
            WidgetKind::Button { label, .. } => label.clone(),
            other => panic!("a stack control is a button: {other:?}"),
        })
        .collect();
    assert_eq!(labels, vec!["Start Stack", "Stop All", "Restart"]);
    assert_eq!(widgets[2].id, WidgetId::StartStack);
    assert_eq!(widgets[2].rect, STACK_BUTTONS[0]);
    assert_eq!(widgets[3].id, WidgetId::StopAll);
    assert_eq!(widgets[4].id, WidgetId::RestartStack);
}

#[test]
fn the_services_page_packs_the_cards_the_tab_shows() {
    let temp = TempDir::new();
    let stack = stack_of(
        temp.path(),
        vec![
            conf("Apache", "web", true, 80, "{base}/bin/apache/bin/httpd.exe"),
            conf(
                "MySQL",
                "database",
                true,
                3306,
                "{base}/bin/mysql/bin/mysqld.exe",
            ),
            conf("Composer", "tool", true, 0, ""),
        ],
    );
    let services = stack.services();
    let layout = Layout::compute(services.len());

    let widgets = services_widgets(services, "Apache", &layout, 0);
    // Five toolbar entries, five tabs, one frame per card.
    assert_eq!(widgets.len(), 5 + TABS.len() + 3);
    assert_eq!(widgets[5].id, WidgetId::Tab(0));
    assert_eq!(
        widgets[5 + TABS.len()].id,
        WidgetId::Decoration,
        "a card frame"
    );
    assert_eq!(
        widgets[5 + TABS.len()].rect,
        (GRID_X, CARD_GRID_Y, CARD_W, CARD_H)
    );
    match &widgets[5 + TABS.len()].kind {
        WidgetKind::Card {
            index: _,
            view,
            layout,
        } => {
            assert_eq!(view.name, "Apache  2.4.68 (VS18, win64)");
            assert_eq!(view.status, "Stopped  :80");
            assert_eq!(layout.buttons.len(), 3, "toggle, restart, configure");
        }
        other => panic!("a card frame carries its view: {other:?}"),
    }

    // The tab filter hides and re-packs: only the web server survives `Web`.
    assert_eq!(card_visibility(services, 0), vec![true, true, true]);
    assert_eq!(card_visibility(services, 1), vec![true, false, false]);
    assert_eq!(card_visibility(services, 3), vec![false, false, false]);
    assert_eq!(card_visibility(services, 4), vec![false, false, true]);

    let positions = card_positions(services, 1, &layout);
    assert_eq!(positions[0], Some((GRID_X, CARD_GRID_Y)));
    assert_eq!(positions[1], None);
    assert_eq!(positions[2], None);

    // Squeezing the database tab puts MySQL in the first slot, which is what a
    // re-packed grid means.
    let positions = card_positions(services, 2, &layout);
    assert_eq!(positions[0], None);
    assert_eq!(positions[1], Some((GRID_X, CARD_GRID_Y)));
    assert_eq!(positions[2], None);
}

#[test]
fn a_cards_children_are_its_labels_and_its_buttons() {
    let temp = TempDir::new();
    let stack = stack_of(
        temp.path(),
        vec![conf(
            "PHP-FPM",
            "php",
            true,
            9000,
            "{base}/bin/php/php-cgi.exe",
        )],
    );
    let view = card_view(&stack.services()[0], "Apache");
    let layout = CardLayout::compute(view.has_variants);
    let children = card_children(0, &view, &layout);

    let parts: Vec<CardPart> = children
        .iter()
        .map(|(id, _, _)| match id {
            WidgetId::Card { part, .. } => *part,
            other => panic!("a card's child is numbered by part: {other:?}"),
        })
        .collect();
    assert_eq!(
        parts,
        vec![
            CardPart::Icon,
            CardPart::Dot,
            CardPart::Name,
            CardPart::Status,
            CardPart::Toggle,
            CardPart::Restart,
            CardPart::Configure,
            CardPart::Version,
        ],
        "four labels, then the cards' buttons in the row's order"
    );
    assert_eq!(children[0].1, layout.icon);
    assert_eq!(children[3].1, layout.status);
    assert_eq!(children[5].1, layout.buttons[1], "the restart square");
    assert_eq!(children[7].1, layout.buttons[3], "the version picker");

    // The labels say what the view says, and the buttons report their states.
    match &children[2].2 {
        WidgetKind::Label(text) => assert_eq!(*text, view.name),
        other => panic!("the name is a label: {other:?}"),
    }
    match &children[6].2 {
        WidgetKind::Button {
            label,
            scheme,
            enabled,
        } => {
            assert_eq!(label, "Conf");
            assert_eq!(*scheme, Scheme::Primary);
            assert!(*enabled);
        }
        other => panic!("configure is a button: {other:?}"),
    }
    match &children[5].2 {
        WidgetKind::Button { label, enabled, .. } => {
            assert_eq!(label, "↻");
            assert!(*enabled, "the service has an engine");
        }
        other => panic!("restart is a button: {other:?}"),
    }

    // A card with no variants has no version button, and its index is the one
    // the caller gave.
    let bare = stack_of(temp.path(), vec![conf("Composer", "tool", true, 0, "")]);
    let view = card_view(&bare.services()[0], "Apache");
    let layout = CardLayout::compute(view.has_variants);
    let children = card_children(3, &view, &layout);
    assert_eq!(children.len(), 7);
    assert!(children.iter().all(|(id, _, _)| match id {
        WidgetId::Card { index, .. } => *index == 3,
        _ => false,
    }));
    match &children[5].2 {
        WidgetKind::Button { enabled, .. } => assert!(!enabled, "a tool has no engine"),
        other => panic!("restart is a button: {other:?}"),
    }
}

#[test]
fn the_footer_is_the_status_bar_the_strip_and_the_log() {
    let temp = TempDir::new();
    let base = temp.path();
    let layout = Layout::compute(8);
    let progress = ProgressView::new(Stage::Downloading, "Apache", 5_242_880, 10_485_760);

    let widgets = footer_widgets(
        &progress,
        "15:04:05 Lambo PHP started\r\n",
        Page::Services,
        base,
        "0.14.0-rc.1",
        &layout,
    );

    // Three status parts that fill the window, left to right.
    let status: Vec<&Widget> = widgets
        .iter()
        .filter(|widget| matches!(widget.id, WidgetId::Status(_)))
        .collect();
    assert_eq!(status.len(), 3);
    assert_eq!(status[0].rect.0, 0);
    assert_eq!(status[1].rect.0, STATUS_PART_WIDTHS[0]);
    assert_eq!(
        status[1].rect.0 + status[1].rect.2,
        WINDOW_W - STATUS_PART_WIDTHS[2],
        "the middle part takes what the other two leave"
    );
    assert_eq!(status[2].rect.0 + status[2].rect.2, WINDOW_W);
    assert!(status.iter().all(|widget| widget.rect.1 < layout.window_h));

    // The strip: the label on the left of the row, the bar beside it - with the
    // bar inside the content area.
    let label = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::ProgressLabel)
        .expect("the label is there");
    assert_eq!(label.rect, (GRID_X, layout.progress_y + 3, 380, 16));
    let bar = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::Progress)
        .expect("the bar is there");
    assert_eq!(bar.rect, (400, layout.progress_y, 550, PROGRESS_H));
    assert!(bar.rect.0 + bar.rect.2 <= CONTENT_X + CONTENT_W);
    match &bar.kind {
        WidgetKind::Progress { label, position } => {
            assert_eq!(label, "Downloading Apache  50.0%  5.0 / 10.0 MB");
            assert_eq!(*position, 500);
        }
        other => panic!("the bar is a progress control: {other:?}"),
    }
    match &label.kind {
        WidgetKind::Label(text) => assert_eq!(text, &progress.label),
        other => panic!("the strip's label is a label: {other:?}"),
    }

    // The log, captioned the way the original captioned it.
    let caption = widgets
        .iter()
        .find(|widget| matches!(&widget.kind, WidgetKind::Label(text) if text == "Logs"))
        .expect("the caption is there");
    assert_eq!(
        caption.rect,
        (
            LOG_X,
            layout.log_y - LOG_LABEL_ABOVE,
            LOG_W,
            LOG_LABEL_ABOVE
        )
    );
    let log = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::Log)
        .expect("the log is there");
    assert_eq!(log.rect, (LOG_X, layout.log_y, LOG_W, LOG_H));
    match &log.kind {
        WidgetKind::Text(text) => assert!(text.contains("Lambo PHP started")),
        other => panic!("the log is a text control: {other:?}"),
    }
}

#[test]
fn the_settings_page_turns_its_layout_into_controls() {
    let temp = TempDir::new();
    let config = PanelConfig::default_config();
    let view = settings_view(SettingsInput {
        base_dir: temp.path(),
        config: &config,
        elevated: false,
        auto_start: false,
        version: "0.14.0-rc.1",
        repository: "",
        homepage: "",
    });

    let widgets = settings_widgets(&view);
    let buttons: Vec<&Widget> = widgets
        .iter()
        .filter(|widget| matches!(widget.id, WidgetId::Settings(_)))
        .collect();
    assert_eq!(buttons.len(), 9, "every action of the grid is a control");
    let labels: Vec<&str> = buttons
        .iter()
        .map(|widget| match &widget.kind {
            WidgetKind::Button { label, .. } => label.as_str(),
            other => panic!("a grid cell is a button: {other:?}"),
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "Edit config",
            "Reload config",
            "Toggle Auto-start",
            "Restart as Admin",
            "Add tools to PATH",
            "Remove from PATH",
            "Open psql Console",
            "Quit Lambo PHP",
            "★ Star on GitHub",
        ],
        "the original's order, and the admin button keeps its place"
    );

    // The grid's own order and geometry survive the conversion.
    assert_eq!(buttons[0].rect, (20, buttons[0].rect.1, 170, 28));
    assert_eq!(buttons[1].rect.0, 200);
    assert_eq!(buttons[4].rect.1, buttons[0].rect.1 + 36, "the second row");
    for (index, button) in buttons.iter().enumerate() {
        assert_eq!(
            button.id,
            WidgetId::Settings(view_actions(&view)[index].action),
            "the grid keeps the buttons' order"
        );
    }

    // Everything else is a caption, and captions are not routed.
    let captions = widgets.len() - buttons.len();
    assert!(captions >= 16, "{captions} captions");
    assert!(
        widgets
            .iter()
            .filter(|widget| matches!(widget.id, WidgetId::Decoration))
            .count()
            == captions
    );

    // Already elevated: the grid loses the admin button, and nothing else moves.
    let elevated = settings_view(SettingsInput {
        base_dir: temp.path(),
        config: &config,
        elevated: true,
        auto_start: true,
        version: "0.14.0-rc.1",
        repository: "",
        homepage: "",
    });
    let widgets = settings_widgets(&elevated);
    let labels: Vec<String> = widgets
        .iter()
        .filter_map(|widget| match &widget.kind {
            WidgetKind::Button { label, .. } => Some(label.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(labels.len(), 8);
    assert!(!labels.iter().any(|label| label == "Restart as Admin"));
    assert!(
        labels.iter().any(|label| label == "★ Star on GitHub"),
        "the repository button is offered whether or not the process is elevated"
    );
}

#[test]
fn the_editor_the_vhosts_and_the_projects_pages_have_their_controls() {
    let temp = TempDir::new();
    let base = temp.path();
    let mut config = PanelConfig::default_config();
    config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();
    std::fs::create_dir_all(base.join("conf/apache")).expect("the apache directory");
    std::fs::write(base.join("conf/apache/vhosts.conf"), "# vhosts\r\n").expect("the include");
    let config_file = crate::panel::config_path(base);
    std::fs::create_dir_all(config_file.parent().expect("a directory")).expect("the directory");
    std::fs::write(&config_file, "{}\r\n").expect("config.json");

    let stack = stack_of(
        base,
        vec![conf(
            "Apache",
            "web",
            true,
            80,
            "{base}/bin/apache/bin/httpd.exe",
        )],
    );
    let layout = Layout::compute(1);

    // The editor: a combo of the files it offers, two buttons, its path and its
    // text.
    let editor = Editor::open(base, &config, stack.services());
    let widgets = editor_widgets(&editor, &layout);
    let ids: Vec<WidgetId> = widgets.iter().map(|widget| widget.id).collect();
    assert!(ids.contains(&WidgetId::EditorFile));
    assert!(ids.contains(&WidgetId::EditorSave));
    assert!(ids.contains(&WidgetId::EditorReload));
    assert!(ids.contains(&WidgetId::EditorText));
    assert!(ids.contains(&WidgetId::EditorPath));
    match &widgets[1].kind {
        WidgetKind::Combo { items, selected } => {
            assert_eq!(items.len(), editor.files.len());
            assert_eq!(*selected, 0, "the original loaded the first entry");
            assert!(items[0].starts_with("Lambo PHP config  —  "));
        }
        other => panic!("the file picker is a combo: {other:?}"),
    }

    // The virtual-hosts page: the table's five columns, the form's fields and
    // its three buttons.
    let mut form = crate::vhost::VhostForm::blank();
    form.name = "shop".to_owned();
    form.docroot = "{base}/www/shop".to_owned();
    form.port = "8080".to_owned();
    form.server = "nginx".to_owned();
    let rows = vhost_list(&[vhost_of("shop.test")], base);
    let widgets = vhosts_widgets(&form, &rows);
    let table = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::VhostList)
        .expect("the table is there");
    match &table.kind {
        WidgetKind::List { columns, rows } => {
            assert_eq!(columns.len(), 5);
            assert_eq!(columns[1], ("Domain", 180));
            assert_eq!(columns[2], ("Document Root", 360));
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 5);
            assert_eq!(rows[0][1], "shop.test");
            assert_eq!(rows[0][3], "80", "a zero port shows as 80");
            assert_eq!(rows[0][4], "apache");
        }
        other => panic!("the list is a list: {other:?}"),
    }
    let port = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::VhostPort)
        .expect("the port field is there");
    match &port.kind {
        WidgetKind::Edit(text) => assert_eq!(text, "8080"),
        other => panic!("the port is an edit: {other:?}"),
    }
    let server = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::VhostServer)
        .expect("the server picker is there");
    match &server.kind {
        WidgetKind::Combo { items, selected } => {
            assert_eq!(items, &SERVER_OPTIONS.map(str::to_owned));
            assert_eq!(*selected, 1, "nginx is the second entry");
        }
        other => panic!("the server picker is a combo: {other:?}"),
    }
    let docroot = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::VhostDocroot)
        .expect("the document root is there");
    match &docroot.kind {
        WidgetKind::Edit(text) => assert_eq!(text, "{base}/www/shop"),
        other => panic!("the document root is an edit: {other:?}"),
    }
    for id in [
        WidgetId::VhostSave,
        WidgetId::VhostDelete,
        WidgetId::VhostApply,
    ] {
        assert!(
            widgets.iter().any(|widget| widget.id == id),
            "{id:?} is there"
        );
    }

    // The projects page: the framework selector, the form, the table and the
    // three buttons.
    let form = ProjectForm {
        framework: "Laravel".to_owned(),
        name: "shop".to_owned(),
        domain_name: String::new(),
        extension: ".local".to_owned(),
    };
    let widgets = projects_widgets(
        &[project_of("shop", "Laravel", "shop.test")],
        &form,
        "✓ PHP   ○ Composer",
        r"C:\Projects\my project",
    );

    // The location the page loads a project from: the folder the field names,
    // and the two buttons that pick it and open it.
    let location = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::ProjectLocation)
        .expect("the location field is there");
    match &location.kind {
        WidgetKind::Edit(text) => assert_eq!(text, r"C:\Projects\my project"),
        other => panic!("the location is an edit: {other:?}"),
    }
    for id in [WidgetId::ProjectBrowse, WidgetId::ProjectAdopt] {
        assert!(
            widgets.iter().any(|widget| widget.id == id),
            "{id:?} is there"
        );
    }

    let framework = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::ProjectFramework)
        .expect("the selector is there");
    match &framework.kind {
        WidgetKind::Combo { items, selected } => {
            assert_eq!(items.len(), 17);
            assert_eq!(*selected, 0, "Laravel is the first framework");
        }
        other => panic!("the framework selector is a combo: {other:?}"),
    }
    let extension = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::ProjectDomainExt)
        .expect("the extension picker is there");
    match &extension.kind {
        WidgetKind::Combo { selected, items } => {
            assert_eq!(items.len(), 6);
            assert_eq!(items[0], ".test");
            assert_eq!(*selected, 1, ".local is the second entry");
        }
        other => panic!("the extension picker is a combo: {other:?}"),
    }
    let table = widgets
        .iter()
        .find(|widget| widget.id == WidgetId::ProjectList)
        .expect("the table is there");
    match &table.kind {
        WidgetKind::List { columns, rows } => {
            assert_eq!(columns.len(), 4);
            assert_eq!(columns[3], ("Document Root", 340));
            assert_eq!(rows[0][0], "shop");
            assert_eq!(rows[0][3], "C:\\Lambo\\www\\shop");
        }
        other => panic!("the table is a list: {other:?}"),
    }
    for action in projects_actions() {
        assert!(
            widgets
                .iter()
                .any(|widget| widget.id == WidgetId::ProjectAction(action.id)),
            "{:?} is there",
            action.id
        );
    }
    let status = widgets
        .iter()
        .find(|widget| matches!(&widget.kind, WidgetKind::Label(text) if text.starts_with("Runtime status: ")))
        .expect("the runtime line is there");
    match &status.kind {
        WidgetKind::Label(text) => assert!(text.ends_with("✓ PHP   ○ Composer")),
        other => panic!("the runtime line is a label: {other:?}"),
    }
}
