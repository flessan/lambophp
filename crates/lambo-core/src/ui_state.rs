//! Everything the control panel decides before it draws anything.
//!
//! The window is Win32: its drawing and its event wiring cannot be checked on
//! anything but Windows, and Windows is not where a mistake is most likely.
//! What a card says, which rows a page lists, what the progress strip shows,
//! when a log line is dropped, where every control sits - those are decisions,
//! and they live here, where every local check runs them.
//!
//! This module is the presentation half of the original's `ui_tabs.go` and
//! `main.go`:
//! the layout constants and the geometry computed from them, the pages and their
//! labels, the service tabs, the cards, the progress strip, the log panel, the
//! editor, the settings and projects pages, the status bar and the theme. Not
//! one function here reads a control or talks to a window: the interface hands
//! over the state the engine already knows and draws what comes back.
//!
//! Five things are deliberately **not** here, because the engine already owns
//! them and a second copy would be a second answer:
//!
//! * A card's status and dot are [`crate::stack::ManagedService::status`] and
//!   [`crate::stack::ManagedService::dot`] - the ported matrix, tested with the
//!   stack.
//! * What a form accepts and what saving it does are [`crate::vhost`]'s rules
//!   (`VhostForm::blank`, `VhostForm::from_vhost`, `read_vhost_form`,
//!   `store_vhost`, `remove_vhost`) and [`crate::frameworks`]'.
//! * Creating a project is [`crate::session::create_project`]: the framework
//!   lookup, the name rule, the domain fallback and the errors it reports are
//!   the same ones `lambo frameworks create` gets.
//! * Auto-start is [`crate::tray::AutoStart`], which writes the setting *and*
//!   logs what it did; the stack's start passes are [`crate::stack::Stack`].
//! * The progress strip takes [`crate::download::Stage`], the engine's own
//!   stage type, rather than a string it would have to interpret.
//!
//! Nothing here starts a process, writes a service file or installs anything.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog_panel;
use crate::download::Stage;
use crate::download_cache::DOWNLOADS_DIR;
use crate::fsx;
use crate::panel::{CONFIG_VERSION, PanelConfig, PanelProject, Vhost, config_path, expand_path};
use crate::stack::ManagedService;
use crate::tray::{AUTO_START_OFF, AUTO_START_ON};

/// A rectangle in page-relative pixels: `(x, y, width, height)`.
pub type Rect = (i32, i32, i32, i32);

// ---------------------------------------------------------------------------
// The window and the services page
// ---------------------------------------------------------------------------

/// The window's design width, in logical pixels.
pub const WINDOW_W: i32 = 1100;

/// The sidebar: five buttons stacked from the top of the content area.
pub const SIDE_X: i32 = 10;
/// Vertical start of the sidebar.
pub const SIDE_Y: i32 = 48;
/// Sidebar button width.
pub const SIDE_W: i32 = 110;
/// Sidebar button height.
pub const SIDE_H: i32 = 38;
/// Gap between two sidebar buttons.
pub const SIDE_GAP: i32 = 4;

/// Where the pages start, and how wide they are.
pub const CONTENT_X: i32 = 130;
/// Vertical start of a page.
pub const CONTENT_Y: i32 = 48;
/// Width of a page's content area.
pub const CONTENT_W: i32 = 960;

/// Height of the progress strip.
pub const PROGRESS_H: i32 = 18;

/// The log panel along the bottom.
pub const LOG_X: i32 = 10;
/// Log panel width.
pub const LOG_W: i32 = 1080;
/// Log panel height.
pub const LOG_H: i32 = 90;
/// How far above the log panel its "Logs" caption sits.
pub const LOG_LABEL_ABOVE: i32 = 18;

/// The service tab strip.
pub const TAB_BTN_W: i32 = 84;
/// Service tab height.
pub const TAB_BTN_H: i32 = 24;
/// Vertical position of the tab strip.
pub const TAB_STRIP_Y: i32 = 36;

/// Where the card grid starts, below the tab strip.
pub const CARD_GRID_Y: i32 = 64;
/// Card width.
pub const CARD_W: i32 = 228;
/// Card height.
pub const CARD_H: i32 = 72;
/// Gap between cards, horizontally and vertically.
pub const CARD_GAP: i32 = 8;
/// Gap between two buttons of a card.
pub const CARD_BUTTON_GAP: i32 = 6;

/// Left edge of the card grid and the tab strip.
pub const GRID_X: i32 = 10;

/// The services toolbar, above the tab strip: the web-server picker and the
/// three stack buttons.
pub const TOOLBAR_LABEL_RECT: Rect = (10, 12, 30, 16);
/// The Apache/Nginx picker. Its dropdown height is the combo's business.
pub const WEB_PICKER_RECT: Rect = (40, 4, 110, 26);
/// Start Stack, Stop All and Restart, in that order.
pub const STACK_BUTTONS: [Rect; 3] = [(160, 4, 100, 26), (266, 4, 80, 26), (352, 4, 80, 26)];

/// The status bar's three parts: the page, the installation, the version. A
/// width of zero means the part takes the space that is left.
pub const STATUS_PART_WIDTHS: [i32; 3] = [220, 0, 90];

/// The progress bar's range, as the original had it.
pub const PROGRESS_MAX: u16 = 1000;

/// How much log text is kept before the oldest quarter is dropped.
///
/// The original's `maxLogBytes`.
pub const LOG_LIMIT: usize = 200 * 1024;

/// The layout the window is built with, computed from the service count.
///
/// The original's `computeLayout`: the grid is as tall as it needs to be, the
/// progress strip follows it, the log panel follows that, and the window is the
/// sum plus a status bar. A two-row minimum keeps the window from jumping
/// around as services are installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// How many cards fit on one row.
    pub columns: i32,
    /// How many rows the grid has.
    pub rows: i32,
    /// Height of the page content area.
    pub content_h: i32,
    /// Vertical position of the progress strip.
    pub progress_y: i32,
    /// Vertical position of the log panel.
    pub log_y: i32,
    /// The window's height.
    pub window_h: i32,
}

impl Layout {
    /// Computes the layout for an installation with `service_count` services.
    pub fn compute(service_count: usize) -> Self {
        let columns = columns();
        let count = service_count as i32;
        let rows = ((count + columns - 1) / columns).max(2);
        let content_h = CARD_GRID_Y + rows * (CARD_H + CARD_GAP) - CARD_GAP + 16;
        let progress_y = CONTENT_Y + content_h + 8;
        let log_y = progress_y + PROGRESS_H + 8;
        Self {
            columns,
            rows,
            content_h,
            progress_y,
            log_y,
            window_h: log_y + LOG_H + 40,
        }
    }

    /// Where the card at `index` sits inside the grid, in logical pixels.
    ///
    /// Cards are laid out in reading order: the original placed them column by
    /// column across the available width and wrapped, which is what this does
    /// with a single index.
    pub fn card_position(&self, index: usize) -> (i32, i32) {
        let index = index as i32;
        let column = index % self.columns;
        let row = index / self.columns;
        (
            GRID_X + column * (CARD_W + CARD_GAP),
            CARD_GRID_Y + row * (CARD_H + CARD_GAP),
        )
    }
}

/// How many cards fit on one row. Never fewer than one.
pub fn columns() -> i32 {
    ((CONTENT_W + CARD_GAP) / (CARD_W + CARD_GAP)).max(1)
}

// ---------------------------------------------------------------------------
// The pages that are mostly native controls
// ---------------------------------------------------------------------------

/// Where the editor page's controls sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorLayout {
    /// The "File:" caption.
    pub file_label: Rect,
    /// The list of the installation's files.
    pub dropdown: Rect,
    /// The Save button.
    pub save: Rect,
    /// The Reload button.
    pub reload: Rect,
    /// The "Path:" caption.
    pub path_label: Rect,
    /// The loaded file's path, or why it could not be read.
    pub path_value: Rect,
    /// The text itself.
    pub content: Rect,
}

impl EditorLayout {
    /// Computes the page's layout.
    pub fn compute(layout: &Layout) -> Self {
        Self {
            file_label: (10, 38, 36, 16),
            dropdown: (50, 30, 360, 26),
            save: (420, 28, 84, 26),
            reload: (512, 28, 84, 26),
            path_label: (10, 66, 36, 16),
            path_value: (50, 62, CONTENT_W - 60, 20),
            content: (10, 86, CONTENT_W - 20, (layout.content_h - 100).max(60)),
        }
    }
}

/// Where the virtual-hosts page's controls sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VhostsLayout {
    /// The list of registered hosts: enabled mark, domain, document root, port,
    /// server.
    pub list: Rect,
    /// The "Domain" caption.
    pub domain_label: Rect,
    /// The domain's name, without its extension.
    pub domain_name: Rect,
    /// The extension picker.
    pub domain_ext: Rect,
    /// The "Port" caption.
    pub port_label: Rect,
    /// The port, as text.
    pub port: Rect,
    /// The "Server" caption.
    pub server_label: Rect,
    /// The apache/nginx/both picker.
    pub server: Rect,
    /// The "DocRoot" caption.
    pub docroot_label: Rect,
    /// The document root.
    pub docroot: Rect,
    /// The Save button.
    pub save: Rect,
    /// The Delete button.
    pub delete: Rect,
    /// The Apply to System button.
    pub apply: Rect,
}

impl VhostsLayout {
    /// Computes the page's layout.
    pub fn compute(_layout: &Layout) -> Self {
        Self {
            list: (10, 28, CONTENT_W - 20, 160),
            domain_label: (10, 204, 60, 16),
            domain_name: (72, 200, 140, 24),
            domain_ext: (216, 200, 80, 24),
            port_label: (310, 204, 34, 16),
            port: (345, 200, 60, 24),
            server_label: (420, 204, 48, 16),
            server: (470, 200, 100, 24),
            docroot_label: (10, 234, 60, 16),
            docroot: (72, 230, CONTENT_W - 100, 24),
            save: (10, 264, 90, 26),
            delete: (106, 264, 90, 26),
            apply: (202, 264, 140, 26),
        }
    }
}

/// Where the projects page's controls sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectsLayout {
    /// The "Framework:" caption.
    pub framework_label: Rect,
    /// The framework selector.
    pub framework: Rect,
    /// The "Name:" caption.
    pub name_label: Rect,
    /// The project's name.
    pub name: Rect,
    /// The "Domain:" caption.
    pub domain_label: Rect,
    /// The domain's name, without its extension.
    pub domain_name: Rect,
    /// The extension picker.
    pub domain_ext: Rect,
    /// The Create Project button.
    pub create: Rect,
    /// The runtime status line.
    pub runtime: Rect,
    /// The caption above the list.
    pub list_label: Rect,
    /// The registered projects: name, framework, domain, document root.
    pub list: Rect,
    /// The three buttons under the list, in [`ProjectActionId`]'s order.
    pub actions: [Rect; 3],
    /// The caption of the folder a project is loaded from.
    pub location_label: Rect,
    /// The folder a project is loaded from.
    pub location: Rect,
    /// `Browse…`: open the folder picker on the location field.
    pub browse: Rect,
    /// `Open Project`: load the project the location field names.
    pub open: Rect,
}

impl ProjectsLayout {
    /// Computes the page's layout.
    pub fn compute(_layout: &Layout) -> Self {
        Self {
            framework_label: (10, 34, 72, 16),
            framework: (90, 30, 200, 24),
            name_label: (305, 34, 38, 16),
            name: (345, 30, 110, 24),
            domain_label: (465, 34, 48, 16),
            domain_name: (515, 30, 100, 24),
            domain_ext: (617, 30, 80, 24),
            create: (705, 28, 95, 26),
            runtime: (10, 66, CONTENT_W - 20, 16),
            list_label: (10, 90, CONTENT_W - 20, 16),
            list: (10, 110, CONTENT_W - 20, 170),
            actions: [(10, 288, 140, 26), (156, 288, 110, 26), (272, 288, 90, 26)],
            location_label: (10, 326, 64, 16),
            location: (80, 322, 600, 24),
            browse: (688, 321, 92, 26),
            open: (788, 321, 120, 26),
        }
    }
}

/// The landing page's layout: the title, the tagline, the three ways in and
/// the hint under them.
pub struct LandingLayout {
    /// The product's name.
    pub title: Rect,
    /// The line under the name.
    pub tagline: Rect,
    /// `Start Stack & Open Welcome Page`.
    pub start: Rect,
    /// `Open Welcome Page`.
    pub welcome: Rect,
    /// `Open Dashboard`.
    pub dashboard: Rect,
    /// The hint under the buttons.
    pub hint: Rect,
}

impl LandingLayout {
    /// Computes the page's layout.
    pub fn compute(_layout: &Layout) -> Self {
        Self {
            title: (10, 36, CONTENT_W - 20, 34),
            tagline: (10, 76, CONTENT_W - 20, 18),
            start: (10, 116, 260, 32),
            welcome: (280, 116, 190, 32),
            dashboard: (480, 116, 170, 32),
            hint: (10, 162, CONTENT_W - 20, 16),
        }
    }
}

// ---------------------------------------------------------------------------
// Pages and navigation
// ---------------------------------------------------------------------------

/// The six pages: the landing page the window opens on, and the five the
/// sidebar lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    /// The landing page: start the stack, open the welcome page, go to the
    /// dashboard.
    Landing,
    /// Service cards, the web-server picker and the stack controls.
    Services,
    /// The framework selector, the project list and its actions.
    Projects,
    /// The configuration editor.
    Editor,
    /// The virtual-hosts table and its form.
    Vhosts,
    /// Paths, state, actions and about.
    Settings,
}

impl Page {
    /// Every page, in the order the window numbers them.
    pub const ALL: [Page; 6] = [
        Page::Landing,
        Page::Services,
        Page::Projects,
        Page::Editor,
        Page::Vhosts,
        Page::Settings,
    ];

    /// The pages the sidebar lists, in its order. The landing page is the
    /// way in, not a workroom, so it has no sidebar button of its own.
    pub const SIDEBAR: [Page; 5] = [
        Page::Services,
        Page::Projects,
        Page::Editor,
        Page::Vhosts,
        Page::Settings,
    ];

    /// The page's key, which is what the status bar names and the window's
    /// state records.
    pub fn key(self) -> &'static str {
        match self {
            Page::Landing => "landing",
            Page::Services => "services",
            Page::Projects => "projects",
            Page::Editor => "editor",
            Page::Vhosts => "vhosts",
            Page::Settings => "settings",
        }
    }

    /// The sidebar button's label. The landing page has no sidebar button;
    /// this is the name its own navigation uses.
    pub fn label(self) -> &'static str {
        match self {
            Page::Landing => "Home",
            Page::Services => "Services",
            Page::Projects => "Projects",
            Page::Editor => "Editor",
            Page::Vhosts => "Virtual Hosts",
            Page::Settings => "Settings",
        }
    }

    /// The page a key names, or `None` when nothing does.
    pub fn from_key(key: &str) -> Option<Self> {
        Page::ALL.into_iter().find(|page| page.key() == key)
    }

    /// The status bar's first part after a page change: `Page: {key}`.
    pub fn status_text(self) -> String {
        format!("Page: {}", self.key())
    }
}

/// Where a sidebar button sits, in logical pixels.
pub fn sidebar_position(index: usize) -> (i32, i32) {
    (SIDE_X, SIDE_Y + index as i32 * (SIDE_H + SIDE_GAP))
}

// ---------------------------------------------------------------------------
// Service groups, tabs and cards
// ---------------------------------------------------------------------------

/// The four groups a card belongs to, which is also the order the original
/// built them in and the order the tabs filter by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceGroup {
    /// Apache, Nginx.
    Web,
    /// Databases and caches.
    Database,
    /// PHP and the other language runtimes.
    Language,
    /// Everything else: the managers and the tooling.
    Tool,
}

impl ServiceGroup {
    /// The group a service kind belongs to.
    ///
    /// The original's `groupForKind`, including the default: an unknown kind is
    /// an admin tool rather than an error, because a configuration a user edited
    /// by hand must still draw.
    pub fn of_kind(kind: &str) -> Self {
        match kind.to_ascii_lowercase().as_str() {
            "web" => Self::Web,
            "php" | "language" | "runtime" => Self::Language,
            "database" | "cache" => Self::Database,
            _ => Self::Tool,
        }
    }

    /// The group's name in the interface, from the original's `categoryLabel`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Web => "Web Server",
            Self::Language => "Language",
            Self::Database => "Database / Cache",
            Self::Tool => "Admin Tool",
        }
    }

    /// The order the cards are built and drawn in.
    pub const ORDER: [ServiceGroup; 4] = [
        ServiceGroup::Web,
        ServiceGroup::Database,
        ServiceGroup::Language,
        ServiceGroup::Tool,
    ];
}

/// One filter tab above the card grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tab {
    /// The tab's label.
    pub label: &'static str,
    /// The groups it shows; empty means every group.
    pub groups: &'static [ServiceGroup],
}

/// The tab strip, in the original's order.
pub const TABS: [Tab; 5] = [
    Tab {
        label: "All",
        groups: &[],
    },
    Tab {
        label: "Web",
        groups: &[ServiceGroup::Web],
    },
    Tab {
        label: "Database",
        groups: &[ServiceGroup::Database],
    },
    Tab {
        label: "Language",
        groups: &[ServiceGroup::Language],
    },
    Tab {
        label: "Tools",
        groups: &[ServiceGroup::Tool],
    },
];

/// Whether a tab shows a group.
pub fn tab_shows(tab: usize, group: ServiceGroup) -> bool {
    match TABS.get(tab) {
        Some(tab) if tab.groups.is_empty() => true,
        Some(tab) => tab.groups.contains(&group),
        None => true,
    }
}

/// Whether a tab click changes anything.
///
/// The original's rule, and its special case: selecting the tab that is already
/// selected does nothing, *except* for the first one, which is how a user can
/// reset the grid without moving to another tab first.
pub fn tab_click_changes(tab: usize, current: usize) -> bool {
    tab != current || tab == 0
}

/// Where a tab button sits, in logical pixels.
pub fn tab_position(index: usize) -> (i32, i32) {
    (GRID_X + index as i32 * (TAB_BTN_W + 4), TAB_STRIP_Y)
}

/// What a card's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    /// Start or stop the service.
    Toggle,
    /// Stop the service, pause, and start it again.
    Restart,
    /// Open the service's configuration, its URL, or a terminal.
    Configure,
    /// Pick another version.
    Version,
}

/// What a button means, which is what decides how it is painted.
///
/// The original's `ColorScheme`, with the same names: a scheme is a role -
/// "this is the primary action", "this one is destructive" - and the interface
/// decides what that looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// The main action of an area.
    Primary,
    /// Something that starts or adds.
    Success,
    /// Something that stops or removes.
    Danger,
    /// Something that restarts or changes.
    Warning,
    /// A secondary action.
    Neutral,
    /// A plain navigation button.
    Sidebar,
}

/// One button of a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardButton {
    /// What pressing it does.
    pub action: CardAction,
    /// The label, exactly as the original wrote it.
    pub label: &'static str,
    /// The role it is painted with.
    pub scheme: Scheme,
    /// Whether it accepts input.
    pub enabled: bool,
}

/// A service card, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardView {
    /// The service's name, with the version the catalogue names when it has one.
    pub name: String,
    /// The status line, from the stack's own matrix.
    pub status: String,
    /// The status dot: `●` running, `○` installed, `·` inactive or missing.
    pub dot: char,
    /// The start/stop button.
    pub toggle: CardButton,
    /// The restart button, which stops the service and starts it again.
    ///
    /// The original built this control and never placed it: its `btnRestart` sat at
    /// `(cardW+100, cardH+100)` at one pixel square, off the card, so the
    /// timeline behind it was unreachable from the interface. The Lambo panel
    /// draws it, because a control the original wrote - with its own pause and
    /// its own error line, both preserved in [`crate::stack::Stack::restart`] -
    /// is a behaviour to port, not a decoration to drop.
    pub restart: CardButton,
    /// The buttons beside it: configure, and - only for a component the
    /// catalogue offers in several versions - the version picker.
    pub buttons: Vec<CardButton>,
    /// The icon file under `assets/icons`, when the service has one.
    pub icon: Option<&'static str>,
    /// The group the card is drawn in.
    pub group: ServiceGroup,
    /// Whether the configuration enables it.
    pub enabled: bool,
    /// Whether the catalogue offers more than one version of it.
    pub has_variants: bool,
    /// Whether it is the installation's active web server (always true for a
    /// service that is not a web server).
    pub active: bool,
}

impl CardView {
    /// The configure button: the editor, the URL, or the service's terminal.
    pub fn configure(&self) -> Option<&CardButton> {
        self.buttons
            .iter()
            .find(|button| button.action == CardAction::Configure)
    }
}

/// Where a card's controls sit, in card-relative pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardLayout {
    /// The service's icon.
    pub icon: Rect,
    /// The status dot.
    pub dot: Rect,
    /// The name and version.
    pub name: Rect,
    /// The status line.
    pub status: Rect,
    /// The buttons, in the order [`CardView::toggle`] then
    /// [`CardView::buttons`]: the start/stop button, configure, version.
    pub buttons: Vec<Rect>,
}

impl CardLayout {
    /// Computes a card's layout. The button geometry is the original's: the
    /// start/stop button is wide when it is the only one, and the version picker
    /// only exists for a component with variants.
    pub fn compute(has_variants: bool) -> Self {
        let row_y = CARD_H - 28;
        // The original's row was the toggle and the configure button - 96 wide
        // and then 110, or 130 and then 146 - with the version picker added for
        // a component that has variants. The restart square is the one addition,
        // and the toggle gives up the room for it, so the row still ends at the
        // same edge.
        let buttons = if has_variants {
            vec![
                (8, row_y, 72, 22),
                (86, row_y, 22, 22),
                (114, row_y, 50, 22),
                (170, row_y, 50, 22),
            ]
        } else {
            vec![
                (8, row_y, 72, 22),
                (86, row_y, 22, 22),
                (114, row_y, 106, 22),
            ]
        };
        Self {
            icon: (8, 8, 32, 32),
            dot: (44, 10, 12, 14),
            name: (58, 10, CARD_W - 66, 16),
            status: (48, 28, CARD_W - 56, 14),
            buttons,
        }
    }
}

/// Builds a card's view from the service and the active web server.
pub fn card_view(service: &ManagedService, active_web_server: &str) -> CardView {
    let name = service.name().to_owned();
    let active = !service.is_web_kind() || name == active_web_server;
    let running = service.running();

    // The toggle's enabled state comes from the same decisions as the status
    // line, so the two cannot disagree: a web server that is not the active one
    // is started from the picker above, not from its card.
    let toggle = if !active {
        CardButton {
            action: CardAction::Toggle,
            label: "▶ Start",
            scheme: Scheme::Success,
            enabled: false,
        }
    } else if running {
        CardButton {
            action: CardAction::Toggle,
            label: "■ Stop",
            scheme: Scheme::Success,
            enabled: true,
        }
    } else {
        CardButton {
            action: CardAction::Toggle,
            label: "▶ Start",
            scheme: Scheme::Success,
            enabled: true,
        }
    };

    let has_variants =
        catalog_panel::find(&name).is_some_and(|component| !component.variants.is_empty());

    // Restart needs an engine to restart; a tool with no process of its own has
    // nothing to stop and start, and the original's handler returned immediately
    // for one.
    let restart = CardButton {
        action: CardAction::Restart,
        label: "↻",
        scheme: Scheme::Warning,
        enabled: service.service().is_some(),
    };
    let is_runtime = service.conf().kind.eq_ignore_ascii_case("runtime");

    let mut buttons = vec![CardButton {
        action: CardAction::Configure,
        label: if is_runtime { "⌨ Term" } else { "Conf" },
        scheme: if is_runtime {
            Scheme::Sidebar
        } else {
            Scheme::Primary
        },
        enabled: true,
    }];
    if has_variants {
        buttons.push(CardButton {
            action: CardAction::Version,
            label: "Ver ▾",
            scheme: Scheme::Warning,
            enabled: true,
        });
    }

    CardView {
        name: card_name_line(&name),
        status: service.status(active_web_server),
        dot: service.dot(active_web_server),
        toggle,
        restart,
        buttons,
        icon: icon_file(&name),
        group: ServiceGroup::of_kind(&service.conf().kind),
        enabled: service.conf().enabled,
        has_variants,
        active,
    }
}

/// The card's title: the service's name, plus the version the catalogue names.
///
/// The original's rule, including its quirk: for a component with variants the
/// catalogue's version string is shortened to the variant it starts with
/// (`8.4.22 NTS x64` becomes `8.4`), and the configured `active_version` is not
/// consulted here at all - what the version picker switched to is visible in the
/// picker's check mark.
pub fn card_name_line(name: &str) -> String {
    let Some(component) = catalog_panel::find(name) else {
        return name.to_owned();
    };

    let mut short = component.version;
    if !component.variants.is_empty() {
        let mut chosen = "";
        for variant in component.variants {
            if variant.version == short || short.starts_with(variant.version) {
                chosen = variant.version;
            }
        }
        if !chosen.is_empty() {
            short = chosen;
        }
    }

    if short.is_empty() {
        name.to_owned()
    } else {
        format!("{name}  {short}")
    }
}

/// One line of a card's version menu.
///
/// The label is the catalogue's own version string - the picker offers the
/// builds the installer knows, not a list the interface composed - and the
/// check mark is what says which one is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMenuItem {
    /// The variant's version, as the catalogue and the installer name it.
    pub label: String,
    /// Whether the line carries the check mark.
    pub checked: bool,
}

/// The lines of a card's version menu, in the catalogue's order.
///
/// The original's check rule, quirk included: a component that has an active
/// version marks the variant whose version equals it, and one that has none
/// marks the variant the catalogue's own version *starts with* - which is how a
/// fresh installation shows the build it actually unpacks (`8.4.22 NTS x64`
/// starts with `8.4`).
///
/// A service the catalogue does not know, or one with no variants, has no menu:
/// the card does not draw the button either, and the original returned before
/// creating the popup.
pub fn version_menu(service: &ManagedService) -> Vec<VersionMenuItem> {
    let Some(component) = catalog_panel::find(service.name()) else {
        return Vec::new();
    };
    let current = service.conf().active_version.as_str();
    component
        .variants
        .iter()
        .map(|variant| VersionMenuItem {
            label: variant.version.to_owned(),
            checked: variant.version == current
                || (current.is_empty() && component.version.starts_with(variant.version)),
        })
        .collect()
}

/// The version menu's disabled first line, which names the card it belongs to.
pub fn version_menu_title(name: &str) -> String {
    format!("Switch {name} version")
}

/// The icon file for a service name, under `assets/icons`.
///
/// The original's `serviceIconFiles`. It is data, not drawing: the interface
/// loads the file it names.
pub fn icon_file(name: &str) -> Option<&'static str> {
    Some(match name {
        "Apache" => "apache.ico",
        "Nginx" => "nginx.ico",
        "PHP-FPM" => "php.ico",
        "MySQL" => "mysql.ico",
        "PostgreSQL" => "postgresql.ico",
        "Redis" => "redis.ico",
        "phpMyAdmin" => "phpmyadmin.ico",
        "Adminer" => "adminer.ico",
        "Composer" => "composer.ico",
        "pgweb" => "pgweb.ico",
        "MinIO" => "minio.ico",
        "Mailpit" => "mailpit.ico",
        "RabbitMQ" => "rabbitmq.ico",
        "Node.js" => "nodejs.ico",
        "Python" => "python.ico",
        "Go" => "go.ico",
        "Java" => "java.ico",
        "Erlang" => "erlang.ico",
        "Julia" => "julia.ico",
        "Zig" => "zig.ico",
        "Dart" => "dart.ico",
        "Lua" => "lua.ico",
        "Ruby" => "ruby.ico",
        "Rust" => "rust.ico",
        "Kotlin" => "kotlin.ico",
        "Haskell" => "haskell.ico",
        "Elixir" => "elixir.ico",
        "Crystal" => "crystal.ico",
        "Scala" => "scala.ico",
        "Swift" => "swift.ico",
        _ => return None,
    })
}

/// Where a service's icon lives inside the installation.
///
/// The original read `{base}/assets/icons/<file>`; the same path, so an
/// installation that carries the assets the shipped `web/` folder describes keeps
/// working.
pub fn icon_path(base_dir: &Path, file: &str) -> PathBuf {
    base_dir.join("assets").join("icons").join(file)
}

/// What a card's configure button does, which is the original's branch table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigureAction {
    /// A language runtime: open a terminal with its binaries on `PATH`.
    Terminal,
    /// There is no configuration file, but the component has a URL.
    Url(String),
    /// Load this file in the editor.
    Editor(PathBuf),
    /// No configuration file and no URL.
    Nothing,
}

/// Decides what the configure button does for a service.
pub fn configure_action(service: &ManagedService, base_dir: &Path) -> ConfigureAction {
    let conf = service.conf();
    if conf.kind.eq_ignore_ascii_case("runtime") {
        return ConfigureAction::Terminal;
    }
    if conf.config_file.is_empty() {
        return if conf.open_url.is_empty() {
            ConfigureAction::Nothing
        } else {
            ConfigureAction::Url(conf.open_url.clone())
        };
    }
    ConfigureAction::Editor(PathBuf::from(expand_path(&conf.config_file, base_dir)))
}

/// What the log says when a card has no configuration file to open.
pub fn no_config_file_line(name: &str) -> String {
    format!("[{name}] no config file")
}

/// What the log says when the web-server picker changes the setting.
///
/// The original wrote it between the setting and the servers it had to stop;
/// the stopping itself is [`crate::stack::Stack::stop_other_web_servers`].
pub fn web_server_line(choice: &str) -> String {
    format!("active web server: {choice}")
}

/// What the log says when a card's start is refused because its web server is
/// not the active one.
pub fn inactive_web_server_line(name: &str) -> String {
    format!("[{name}] not the active web server — set Active Web Server to {name} first")
}

// ---------------------------------------------------------------------------
// The progress strip
// ---------------------------------------------------------------------------

/// What the progress strip shows: the label and the bar's position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressView {
    /// The text beside the bar.
    pub label: String,
    /// The bar's position, in `0..=PROGRESS_MAX`.
    pub position: u16,
}

impl ProgressView {
    /// The strip when nothing is happening.
    pub fn idle() -> Self {
        Self {
            label: "Idle".to_owned(),
            position: 0,
        }
    }

    /// The strip for one stage of an install.
    ///
    /// The original's `uiDownloadProgress`, stage for stage and number for
    /// number: an unknown total still says how far the download has got, an
    /// extraction with no entry count sits halfway, and the two stages that are
    /// really "finished" fill the bar.
    pub fn new(stage: Stage, name: &str, done: i64, total: i64) -> Self {
        let megabytes = |bytes: i64| bytes as f64 / (1024.0 * 1024.0);
        match stage {
            Stage::Idle => Self::idle(),
            Stage::Starting => Self {
                label: format!("Starting {name} ..."),
                position: 0,
            },
            Stage::Downloading => {
                if total > 0 {
                    let percent = done as f64 * 100.0 / total as f64;
                    Self {
                        label: format!(
                            "Downloading {name}  {percent:.1}%  {:.1} / {:.1} MB",
                            megabytes(done),
                            megabytes(total)
                        ),
                        position: position_of(done, total),
                    }
                } else {
                    Self {
                        label: format!("Downloading {name}  {:.1} MB", megabytes(done)),
                        position: 0,
                    }
                }
            }
            Stage::Extracting => {
                if total > 0 {
                    Self {
                        label: format!("Extracting {name}  {done} / {total} files"),
                        position: position_of(done, total),
                    }
                } else {
                    Self {
                        label: format!("Extracting {name} ..."),
                        position: PROGRESS_MAX / 2,
                    }
                }
            }
            Stage::PostInstall => Self {
                label: format!("Running post-install for {name} ..."),
                position: PROGRESS_MAX,
            },
            Stage::Done => Self {
                label: format!("Installed {name}"),
                position: PROGRESS_MAX,
            },
        }
    }
}

/// The bar's position for a fraction, truncated as the original truncated it.
fn position_of(done: i64, total: i64) -> u16 {
    (done as f64 * PROGRESS_MAX as f64 / total as f64) as u16
}

// ---------------------------------------------------------------------------
// The log panel
// ---------------------------------------------------------------------------

/// The log panel's buffer: timestamped lines, capped like the original's.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    text: String,
}

impl LogBuffer {
    /// An empty panel.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one line, prefixed with the time, and trims the buffer if it has
    /// grown past the limit.
    ///
    /// The timestamp is passed in rather than read from a clock so that the rule
    /// (one line per call, `\r\n` between them) is testable; the interface
    /// reads the local time itself.
    pub fn push_at(&mut self, timestamp: &str, line: &str) {
        self.text.push_str(timestamp);
        self.text.push(' ');
        self.text.push_str(line);
        self.text.push_str("\r\n");
        self.trim();
    }

    /// The text the panel shows.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many bytes are held.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Whether the panel is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Empties the panel.
    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// Drops the oldest quarter once the buffer is over the limit.
    ///
    /// The original cut at a quarter of the length and then advanced to just
    /// after the next newline, so the panel never starts mid-line. A cut that
    /// lands inside a multi-byte character is moved forward to the next one,
    /// which the byte-based original could not notice.
    fn trim(&mut self) {
        if self.text.len() <= LOG_LIMIT {
            return;
        }

        let mut cut = self.text.len() / 4;
        match self.text[cut..].find('\n') {
            Some(offset) => cut += offset + 1,
            None => cut = next_char_boundary(&self.text, cut),
        }
        self.text.drain(..cut);
    }
}

/// The next character boundary at or after `index`.
fn next_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Shortens text in the middle, keeping both ends: `C:\…\config.json`.
///
/// The original's `truncateMid`, which spent three characters on the ellipsis
/// and half of the rest on each end. It sliced bytes and could split a
/// multi-byte character; this keeps the same shape and moves the cut to the
/// nearest boundary.
pub fn truncate_mid(text: &str, max: usize) -> String {
    if text.len() <= max || max <= 3 {
        return text.to_owned();
    }
    let keep = (max - 3) / 2;
    let start = next_char_boundary(text, keep);
    let end = {
        let mut index = text.len() - keep;
        while index < text.len() && !text.is_char_boundary(index) {
            index += 1;
        }
        index
    };
    format!("{}...{}", &text[..start], &text[end..])
}

/// The local time as the log shows it: `15:04:05`.
///
/// The interface reads the clock - that is the one part of this that is Win32 -
/// and the formatting is here, where it can be checked.
pub fn time_of_day(hour: u16, minute: u16, second: u16) -> String {
    format!("{hour:02}:{minute:02}:{second:02}")
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

/// One file the editor offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorFile {
    /// The label shown in the dropdown: the service's name or the file's role.
    pub label: String,
    /// The file itself.
    pub path: PathBuf,
}

/// The files the editor offers, in the dropdown's order.
///
/// The original's `populateEditorDropdown`: every service with a configuration
/// file that exists, then the installation's own configuration, the Apache
/// vhost include and the system hosts file. A path that does not exist is left
/// out rather than offered as an error.
pub fn editor_files(
    base_dir: &Path,
    config: &PanelConfig,
    services: &[ManagedService],
) -> Vec<EditorFile> {
    let mut files = Vec::new();
    let mut add = |label: String, path: PathBuf| {
        if path.as_os_str().is_empty() || !path.exists() {
            return;
        }
        files.push(EditorFile { label, path });
    };

    for service in services {
        let file = service.conf().config_file.clone();
        if file.is_empty() {
            continue;
        }
        add(
            service.name().to_owned(),
            PathBuf::from(expand_path(&file, base_dir)),
        );
    }

    add("Lambo PHP config".to_owned(), config_path(base_dir));
    add(
        "Apache vhosts".to_owned(),
        PathBuf::from(expand_path(
            &config.settings.apache_vhosts_include,
            base_dir,
        )),
    );
    add(
        "Windows hosts".to_owned(),
        crate::vhost::default_hosts_file(),
    );

    files
}

/// The dropdown's text for a file: `Lambo PHP config  —  C:\Lambo\conf…`.
pub fn editor_label(file: &EditorFile) -> String {
    format!(
        "{}  —  {}",
        file.label,
        truncate_mid(&file.path.display().to_string(), 60)
    )
}

/// The editor's state: which files it offers, and the one it holds.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    /// The files the dropdown offers.
    pub files: Vec<EditorFile>,
    /// The one currently selected, when there is one.
    pub selected: Option<usize>,
    /// The file whose text is loaded, when one is.
    pub loaded: Option<PathBuf>,
    /// The text in the buffer.
    pub text: String,
    /// Whether the buffer has been changed since it was loaded.
    pub dirty: bool,
    /// Why the last load failed, when it did: the interface shows it in the
    /// path label the way the original did.
    pub failure: Option<String>,
}

impl Editor {
    /// An editor offering the installation's files, with the first one loaded.
    ///
    /// The original selected the first entry and loaded it, which is why
    /// opening the page always shows something.
    pub fn open(base_dir: &Path, config: &PanelConfig, services: &[ManagedService]) -> Self {
        let mut editor = Self {
            files: editor_files(base_dir, config, services),
            ..Self::default()
        };
        if !editor.files.is_empty() {
            editor.selected = Some(0);
            let path = editor.files[0].path.clone();
            let _ = editor.load(&path);
        }
        editor
    }

    /// The text the path label shows.
    pub fn status_text(&self) -> String {
        if let Some(failure) = &self.failure {
            return format!("(failed to read: {failure})");
        }
        match &self.loaded {
            Some(path) => path.display().to_string(),
            None => "(no file loaded)".to_owned(),
        }
    }

    /// Loads a file into the buffer, returning the line to log.
    ///
    /// The original read the file, normalised its line endings to CRLF (which is
    /// what the interface's edit control expects) and named it in the path
    /// label. A file that cannot be read clears the buffer and is reported, in
    /// the log and in the label.
    pub fn load(&mut self, path: &Path) -> String {
        match fs::read_to_string(path) {
            Ok(text) => {
                self.text = to_crlf(&text);
                self.loaded = Some(path.to_path_buf());
                self.failure = None;
                self.dirty = false;
                format!("editor: loaded {}", path.display())
            }
            Err(source) => {
                self.text.clear();
                self.loaded = None;
                self.dirty = false;
                let path_text = path.display().to_string();
                self.failure = Some(path_text);
                format!("editor: {}", crate::error::Error::io(path, source))
            }
        }
    }

    /// Loads the file at `index` in the dropdown, if there is one.
    pub fn select(&mut self, index: usize) -> Option<String> {
        let file = self.files.get(index)?;
        self.selected = Some(index);
        let path = file.path.clone();
        Some(self.load(&path))
    }

    /// Saves the buffer, returning the line to log.
    ///
    /// Nothing is written when no file is loaded, which is the original's
    /// `editor: no file loaded` - the same answer whether nothing was ever
    /// opened or the last load failed. Writes are atomic: a configuration file
    /// that is half-written because the machine went down mid-save is a broken
    /// installation.
    pub fn save(&mut self) -> String {
        let Some(path) = self.loaded.clone() else {
            return "editor: no file loaded".to_owned();
        };

        let bytes = self.text.len();
        match fsx::write_atomic(&path, &self.text) {
            Ok(()) => {
                self.dirty = false;
                format!("editor: saved {} ({bytes} bytes)", path.display())
            }
            Err(error) => format!("editor save: {error}"),
        }
    }

    /// Records that the buffer changed.
    pub fn set_text(&mut self, text: String) {
        self.dirty = text != self.text;
        self.text = text;
    }

    /// Whether the buffer differs from the file it came from.
    ///
    /// The original had no such idea and would simply overwrite on save. This is
    /// reported so the interface can mark the button or the title; no action is
    /// blocked by it.
    pub fn has_unsaved_changes(&self) -> bool {
        self.dirty
    }
}

/// The text with CRLF line endings, whatever it had.
fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// One `label: value` row of the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsRow {
    /// The row's label.
    pub label: &'static str,
    /// The value, already rendered.
    pub value: String,
}

/// One titled group of rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSection {
    /// The section's title.
    pub title: &'static str,
    /// Its rows.
    pub rows: Vec<SettingsRow>,
}

/// What a settings button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    /// Open the installation's configuration in the editor.
    EditConfig,
    /// Re-read the configuration and rebuild the stack.
    ReloadConfig,
    /// Turn "start with Windows" on or off.
    ToggleAutoStart,
    /// Relaunch as administrator.
    RestartAsAdmin,
    /// Put the installation's tools on the user's `PATH`.
    AddToPath,
    /// Take them off it again.
    RemoveFromPath,
    /// Open a PostgreSQL console.
    PsqlConsole,
    /// Open this product's repository in the default browser.
    OpenRepository,
    /// Stop everything and exit.
    Quit,
}

/// One button of the settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsButton {
    /// What pressing it does.
    pub action: SettingsAction,
    /// The label, exactly as the original wrote it, rebranded where the original
    /// named the other product.
    pub label: &'static str,
    /// The role it is painted with.
    pub scheme: Scheme,
    /// Whether it accepts input.
    pub enabled: bool,
}

/// One element of the settings page, in the order it is laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsBlock {
    /// A titled section with its rows.
    Section(SettingsSection),
    /// The page's action buttons, in a grid.
    Actions(Vec<SettingsButton>),
    /// Blank space before the next block.
    Gap(i32),
}

/// What the settings page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsView {
    /// Sections, the action grid and the gaps between them, in order.
    pub blocks: Vec<SettingsBlock>,
}

/// Everything the settings page needs, all of it already resolved.
#[derive(Debug, Clone, Copy)]
pub struct SettingsInput<'a> {
    /// The installation directory.
    pub base_dir: &'a Path,
    /// The installation's document.
    pub config: &'a PanelConfig,
    /// Whether this process is running as administrator.
    pub elevated: bool,
    /// Whether Windows is set to start Lambo at login.
    pub auto_start: bool,
    /// This build's version.
    pub version: &'a str,
    /// Where the source lives.
    pub repository: &'a str,
    /// The project's site.
    pub homepage: &'a str,
}

/// Builds the settings page.
///
/// The original's `buildSettingsPage` - the same labels, the same values, the
/// same order, the same gaps - with the branding rows naming this product, as
/// the migration requires: no other project's name, author or donation link is
/// presented as this application's.
pub fn settings_view(input: SettingsInput<'_>) -> SettingsView {
    let mut paths = vec![
        SettingsRow {
            label: "Install dir",
            value: input.base_dir.display().to_string(),
        },
        SettingsRow {
            label: "Config",
            value: config_path(input.base_dir).display().to_string(),
        },
        SettingsRow {
            label: "Apache vhosts",
            value: expand_path(&input.config.settings.apache_vhosts_include, input.base_dir),
        },
        SettingsRow {
            label: "Nginx sites",
            value: expand_path(&input.config.settings.nginx_sites_dir, input.base_dir),
        },
        SettingsRow {
            label: "Hosts file",
            value: crate::vhost::default_hosts_file().display().to_string(),
        },
        SettingsRow {
            label: "Downloads cache",
            value: input.base_dir.join(DOWNLOADS_DIR).display().to_string(),
        },
    ];

    // The original named the platform's hosts file and never the configured one,
    // which is what an apply uses when the setting is empty - and what it
    // *ignores* when it is not. Saying which is in force is the difference
    // between a path row and an answer.
    if !input.config.settings.hosts_file.is_empty() {
        paths.push(SettingsRow {
            label: "Hosts file (configured)",
            value: expand_path(&input.config.settings.hosts_file, input.base_dir),
        });
    }

    let state = vec![
        SettingsRow {
            label: "Services",
            value: format!(
                "{} total · {} enabled",
                input.config.services.len(),
                input.config.enabled_service_count()
            ),
        },
        SettingsRow {
            label: "Vhosts",
            value: input.config.vhosts.len().to_string(),
        },
        SettingsRow {
            label: "Projects",
            value: input.config.projects.len().to_string(),
        },
        SettingsRow {
            label: "Active web server",
            value: input.config.active_web_server().to_owned(),
        },
        SettingsRow {
            label: "Auto-start on boot",
            value: if input.auto_start {
                AUTO_START_ON.to_owned()
            } else {
                AUTO_START_OFF.to_owned()
            },
        },
        SettingsRow {
            label: "Running elevated",
            value: if input.elevated {
                "yes (administrator)".to_owned()
            } else {
                "no — vhost Apply needs admin".to_owned()
            },
        },
        SettingsRow {
            label: "Config version",
            value: format!(
                "{} (this build writes {CONFIG_VERSION})",
                input.config.version
            ),
        },
    ];

    let about = vec![
        SettingsRow {
            label: "Product",
            value: crate::PRODUCT.to_owned(),
        },
        SettingsRow {
            label: "Version",
            value: input.version.to_owned(),
        },
        SettingsRow {
            label: "Repository",
            value: input.repository.to_owned(),
        },
        SettingsRow {
            label: "Homepage",
            value: input.homepage.to_owned(),
        },
    ];

    SettingsView {
        blocks: vec![
            SettingsBlock::Section(SettingsSection {
                title: "PATHS",
                rows: paths,
            }),
            SettingsBlock::Gap(8),
            SettingsBlock::Section(SettingsSection {
                title: "STATE",
                rows: state,
            }),
            SettingsBlock::Gap(12),
            SettingsBlock::Actions(settings_actions(input.elevated)),
            SettingsBlock::Gap(16),
            SettingsBlock::Section(SettingsSection {
                title: "ABOUT",
                rows: about,
            }),
        ],
    }
}

/// The settings page's buttons, in the original's order.
///
/// `Restart as Admin` is offered only when the process is not already elevated:
/// the original built it behind exactly that condition, and a button that
/// relaunches what is already running as administrator is a button that does
/// nothing.
pub fn settings_actions(elevated: bool) -> Vec<SettingsButton> {
    let button = |action, label, scheme| SettingsButton {
        action,
        label,
        scheme,
        enabled: true,
    };

    let mut actions = vec![
        button(SettingsAction::EditConfig, "Edit config", Scheme::Primary),
        button(
            SettingsAction::ReloadConfig,
            "Reload config",
            Scheme::Neutral,
        ),
        button(
            SettingsAction::ToggleAutoStart,
            "Toggle Auto-start",
            Scheme::Primary,
        ),
    ];
    if !elevated {
        actions.push(button(
            SettingsAction::RestartAsAdmin,
            "Restart as Admin",
            Scheme::Warning,
        ));
    }
    actions.push(button(
        SettingsAction::AddToPath,
        "Add tools to PATH",
        Scheme::Success,
    ));
    actions.push(button(
        SettingsAction::RemoveFromPath,
        "Remove from PATH",
        Scheme::Warning,
    ));
    actions.push(button(
        SettingsAction::PsqlConsole,
        "Open psql Console",
        Scheme::Primary,
    ));
    actions.push(button(
        SettingsAction::Quit,
        "Quit Lambo PHP",
        Scheme::Danger,
    ));
    actions.push(button(
        SettingsAction::OpenRepository,
        "\u{2605} Star on GitHub",
        Scheme::Primary,
    ));
    actions
}

/// One laid-out element of the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsItem {
    /// What it is, which decides how it is drawn.
    pub kind: SettingsItemKind,
    /// Where it sits.
    pub rect: Rect,
    /// A section's title, a row's label or value, a button's text.
    pub text: String,
}

/// What a [`SettingsItem`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItemKind {
    /// A section title.
    Section,
    /// The rule under a section title.
    Divider,
    /// A row's label.
    Label,
    /// A row's value.
    Value,
    /// The button with this index in the actions block.
    Action(usize),
}

/// Lays the settings page out, top to bottom, the way the original did.
///
/// The original's own arithmetic: a section costs 26 pixels plus 18 per row, the
/// action buttons are a four-column grid of 170×28 with a 10-pixel gutter and an
/// 8-pixel row gap, and the gaps between blocks are what [`SettingsBlock::Gap`]
/// carries. Returns every element with the rectangle it belongs in.
pub fn settings_layout(view: &SettingsView) -> Vec<SettingsItem> {
    const ACTION_W: i32 = 170;
    const ACTION_H: i32 = 28;
    const ACTION_GAP_X: i32 = 10;
    const ACTION_GAP_Y: i32 = 8;
    const ACTION_COLUMNS: i32 = 4;
    const ACTION_X: i32 = 20;

    let mut items = Vec::new();
    let mut y = 10;

    for block in &view.blocks {
        match block {
            SettingsBlock::Gap(gap) => y += gap,
            SettingsBlock::Section(section) => {
                items.push(SettingsItem {
                    kind: SettingsItemKind::Section,
                    rect: (10, y, CONTENT_W - 20, 16),
                    text: section.title.to_owned(),
                });
                items.push(SettingsItem {
                    kind: SettingsItemKind::Divider,
                    rect: (10, y + 18, CONTENT_W - 20, 1),
                    text: String::new(),
                });
                y += 26;
                for row in &section.rows {
                    items.push(SettingsItem {
                        kind: SettingsItemKind::Label,
                        rect: (20, y, 150, 16),
                        text: row.label.to_owned(),
                    });
                    items.push(SettingsItem {
                        kind: SettingsItemKind::Value,
                        rect: (170, y, CONTENT_W - 180, 16),
                        text: row.value.clone(),
                    });
                    y += 18;
                }
            }
            SettingsBlock::Actions(actions) => {
                let mut column = 0;
                let mut row_y = y;
                // The row the last button went into, which is what the next
                // block is measured from.
                let mut last_row_y = y;
                for (index, action) in actions.iter().enumerate() {
                    last_row_y = row_y;
                    items.push(SettingsItem {
                        kind: SettingsItemKind::Action(index),
                        rect: (
                            ACTION_X + column * (ACTION_W + ACTION_GAP_X),
                            row_y,
                            ACTION_W,
                            ACTION_H,
                        ),
                        text: action.label.to_owned(),
                    });
                    column += 1;
                    if column >= ACTION_COLUMNS {
                        column = 0;
                        row_y += ACTION_H + ACTION_GAP_Y;
                    }
                }
                y = last_row_y + ACTION_H + ACTION_GAP_Y;
            }
        }
    }

    items
}

// ---------------------------------------------------------------------------
// The projects page
// ---------------------------------------------------------------------------

/// The projects page's form, as the user filled it in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectForm {
    /// The framework's name, from the selector.
    pub framework: String,
    /// The project's name.
    pub name: String,
    /// The domain's name part, without its extension.
    pub domain_name: String,
    /// The extension chosen from the list.
    pub extension: String,
}

impl ProjectForm {
    /// The domain the form asks for: `shop` and `.test` make `shop.test`.
    ///
    /// The original's composition, and its two defaults: an empty domain name
    /// becomes the project's own slug (the rule `lambo frameworks create` gets,
    /// because it is [`crate::frameworks::project_slug`]), and an empty
    /// extension becomes `.test`.
    pub fn domain(&self) -> String {
        let name = self.domain_name.trim();
        let name = if name.is_empty() {
            crate::frameworks::project_slug(&self.name)
        } else {
            name.to_owned()
        };
        let extension = if self.extension.is_empty() {
            ".test"
        } else {
            self.extension.as_str()
        };
        format!("{name}{extension}")
    }
}

/// What a project row's buttons do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectActionId {
    /// Open the project's domain in the default browser.
    OpenInBrowser,
    /// Open the project's directory in the file manager.
    OpenFolder,
    /// Delete the project, its directory and its domain.
    Delete,
}

/// One button of the projects page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectAction {
    /// What pressing it does.
    pub id: ProjectActionId,
    /// The label.
    pub label: &'static str,
    /// The role it is painted with.
    pub scheme: Scheme,
}

/// The projects page's buttons, in the original's order.
pub fn projects_actions() -> [ProjectAction; 3] {
    [
        ProjectAction {
            id: ProjectActionId::OpenInBrowser,
            label: "Open in Browser",
            scheme: Scheme::Primary,
        },
        ProjectAction {
            id: ProjectActionId::OpenFolder,
            label: "Open Folder",
            scheme: Scheme::Neutral,
        },
        ProjectAction {
            id: ProjectActionId::Delete,
            label: "Delete",
            scheme: Scheme::Danger,
        },
    ]
}

/// One row of the projects table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    /// The project's name, which is also its directory under `www/`.
    pub name: String,
    /// The framework it was scaffolded with.
    pub framework: String,
    /// The domain it answers on.
    pub domain: String,
    /// Its document root, as the document recorded it.
    pub docroot: String,
}

/// The projects table, in the document's order.
///
/// The original's `refreshProjectList`: the stored document root, not one
/// derived from the name, so a project whose root moved shows the root the
/// panel actually serves.
pub fn project_rows(projects: &[PanelProject]) -> Vec<ProjectRow> {
    projects
        .iter()
        .map(|project| ProjectRow {
            name: project.name.clone(),
            framework: project.framework.clone(),
            domain: project.domain.clone(),
            docroot: project.docroot.clone(),
        })
        .collect()
}

/// The framework names the selector offers, in the catalogue's order.
pub fn framework_names() -> Vec<&'static str> {
    crate::frameworks::frameworks()
        .iter()
        .map(|framework| framework.name)
        .collect()
}

/// The URL a project answers on.
///
/// Always `http`: a scaffolded project is served by the local web server, which
/// has no certificate, and the original opened the plain scheme.
pub fn project_url(project: &PanelProject) -> String {
    format!("http://{}", project.domain)
}

/// What the log says when a project's creation starts.
pub fn project_creation_log_line(name: &str, framework: &str, domain: &str) -> String {
    format!("projects: creating '{name}' ({framework}) at {domain} ...")
}

/// What the log says when a project's deletion starts.
pub fn project_delete_log_line(name: &str, domain: &str) -> String {
    format!("projects: deleting '{name}' ({domain})...")
}

// ---------------------------------------------------------------------------
// The virtual-hosts page
// ---------------------------------------------------------------------------

/// The server types the form offers, in the original's order.
///
/// The combo's contents, not a rule: what a stored type *means* is
/// [`crate::vhost::serves_apache`] and [`crate::vhost::serves_nginx`], and what
/// the form accepts is [`crate::vhost::read_vhost_form`].
pub const SERVER_OPTIONS: [&str; 3] = ["apache", "nginx", "both"];

/// The combo's index for a stored server type, and the original's default:
/// anything unrecognised selects `apache`.
pub fn server_index(server: &str) -> usize {
    SERVER_OPTIONS
        .iter()
        .position(|option| option.eq_ignore_ascii_case(server))
        .unwrap_or(0)
}

/// The server type the combo's index selects.
pub fn server_at(index: usize) -> &'static str {
    SERVER_OPTIONS
        .get(index)
        .copied()
        .unwrap_or(SERVER_OPTIONS[0])
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

/// What the window is called.
pub const WINDOW_TITLE: &str = "Lambo PHP — Local Web Stack Control Panel";

/// Which palette the system asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// The default light palette.
    Light,
    /// The dark palette, when Windows is set to dark mode.
    Dark,
}

/// The theme for what Windows reports.
///
/// The original asked the registry for `AppsUseLightTheme` and switched the
/// window's immersive-dark attribute when it was zero. Reading the registry is
/// Win32; deciding what it means is not, so the decision is here and the read is
/// in the interface.
pub fn theme(prefers_dark: bool) -> Theme {
    if prefers_dark {
        Theme::Dark
    } else {
        Theme::Light
    }
}

/// The status bar's three parts.
///
/// The original: the page, the installation directory in the middle so a user
/// always knows which installation they are looking at, and the product and
/// version on the right.
pub fn status_parts(page: Page, base_dir: &Path, version: &str) -> [String; 3] {
    [
        page.status_text(),
        truncate_mid(&base_dir.display().to_string(), 70),
        format!("{} {version}", crate::PRODUCT),
    ]
}

/// The three lines the log opens with.
pub fn startup_lines(
    base_dir: &Path,
    services: usize,
    vhosts: usize,
    started_at: &str,
) -> [String; 3] {
    [
        format!("{} started at {started_at}", crate::PRODUCT),
        format!("Base dir: {}", base_dir.display()),
        format!("Loaded {services} services, {vhosts} vhosts from config.json"),
    ]
}

/// What the log says when the tray icon cannot be installed.
pub fn tray_failure_line(error: &str) -> String {
    format!("tray: {error}")
}

/// What the log says when the PATH setting changes.
///
/// The original's five messages, rebranded: the count is what
/// [`crate::pathenv::add_to_user_path`] or
/// [`crate::pathenv::remove_from_user_path`] returned, and an error replaces the
/// lot.
pub fn path_lines(action: PathAction, changed: usize, error: Option<&str>) -> Vec<String> {
    if let Some(error) = error {
        return vec![format!("path: {error}")];
    }
    match action {
        PathAction::Add => {
            if changed == 0 {
                vec!["path: already on PATH (no changes)".to_owned()]
            } else {
                vec![
                    format!("path: added {changed} Lambo bin dirs to user PATH"),
                    "path: open a NEW terminal to use them".to_owned(),
                ]
            }
        }
        PathAction::Remove => {
            if changed == 0 {
                vec!["path: nothing to remove".to_owned()]
            } else {
                vec![format!(
                    "path: removed {changed} Lambo entries from user PATH"
                )]
            }
        }
    }
}

/// Which direction a PATH action goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAction {
    /// Put the tools on the user's `PATH`.
    Add,
    /// Take them off it.
    Remove,
}

/// The virtual hosts of a document, as the table lists them.
///
/// A thin walk over [`crate::vhost::vhost_row`], which owns the five cells.
pub fn vhost_list(vhosts: &[Vhost], base_dir: &Path) -> Vec<crate::vhost::VhostRow> {
    vhosts
        .iter()
        .map(|vhost| crate::vhost::vhost_row(vhost, base_dir))
        .collect()
}

#[cfg(test)]
#[path = "ui_state/tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// The controls the window creates
// ---------------------------------------------------------------------------
//
// The window is Win32 and this is not, so the boundary between the two is a
// list of controls: what to create, where, with which identifier, and what it
// says on creation. The window walks the list, creates one control per entry,
// and from then on changes only the text of an identifier it already has.
//
// Identifiers matter because Win32 reports events by number. They are assigned
// here, once, and `WidgetId::from_value` is their inverse, so the window never
// invents a number of its own. A card's version menu keeps the original's own
// base (6000), which is the one numbering `ui_tabs.go` uses.

/// What a card's own controls are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardPart {
    /// The service's icon.
    Icon,
    /// The status dot.
    Dot,
    /// The name and version.
    Name,
    /// The status line.
    Status,
    /// Start or stop.
    Toggle,
    /// Restart.
    Restart,
    /// The configuration, the URL, or the terminal.
    Configure,
    /// The version picker, when the component has variants.
    Version,
}

/// Where a card's version menu starts.
///
/// The original numbered the version entries from 6000, and those numbers are
/// kept: the range only has to stay clear of the tray's.
pub const VERSION_BASE: u16 = 6000;

/// The first number the tray owns, and the end of the widget numbering.
///
/// The tray's identifiers are the original's own (`tray::TrayCommand::id`), the
/// smallest of them is 40001, and they arrive as the same `WM_COMMAND` a control
/// sends - so the widget numbering stops short of them and
/// [`WidgetId::from_value`] answers `None` for a tray number, which is what keeps
/// one namespace from being read as the other.
pub const TRAY_BASE: u16 = 40000;

/// What a widget is called when it reports an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetId {
    /// One of the five sidebar buttons.
    Page(Page),
    /// One of the service tabs.
    Tab(usize),
    /// The web-server picker.
    WebPicker,
    /// `Start Stack`.
    StartStack,
    /// `Stop All`.
    StopAll,
    /// `Restart`.
    RestartStack,
    /// The landing page's `Start Stack & Open Welcome Page`.
    LandingStart,
    /// The landing page's `Open Welcome Page`.
    LandingWelcome,
    /// The landing page's `Open Dashboard`.
    LandingDashboard,
    /// One part of the status bar.
    Status(usize),
    /// The progress bar.
    Progress,
    /// The progress strip's label, beside the bar.
    ProgressLabel,
    /// The log panel.
    Log,
    /// One control of the card at this index of the stack.
    Card {
        /// Where the card is in the stack's order.
        index: usize,
        /// Which of its controls.
        part: CardPart,
    },
    /// One entry of a card's version menu.
    Version {
        /// Where the card is in the stack's order.
        index: usize,
        /// Which variant, in the catalogue's order.
        variant: usize,
    },
    /// The projects page's field holding the folder to load a project from.
    ProjectLocation,
    /// The projects page's `Browse…`: open the folder picker on it.
    ProjectBrowse,
    /// The projects page's `Open Project`: load the folder it names.
    ProjectAdopt,
    /// The editor's file picker.
    EditorFile,
    /// The editor's `Save`.
    EditorSave,
    /// The editor's `Reload`.
    EditorReload,
    /// The editor's text itself.
    EditorText,
    /// The editor's path label.
    EditorPath,
    /// The virtual-hosts table.
    VhostList,
    /// The domain, without its extension.
    VhostDomainName,
    /// The extension picker.
    VhostDomainExt,
    /// The port, as typed.
    VhostPort,
    /// The server-type picker.
    VhostServer,
    /// The document root.
    VhostDocroot,
    /// `Save`.
    VhostSave,
    /// `Delete`.
    VhostDelete,
    /// `Apply to System`.
    VhostApply,
    /// The framework selector.
    ProjectFramework,
    /// The project's name.
    ProjectName,
    /// The domain, without its extension.
    ProjectDomainName,
    /// The extension picker.
    ProjectDomainExt,
    /// `Create Project`.
    ProjectCreate,
    /// The projects table, and the two captions above it.
    ProjectList,
    /// One of the projects page's buttons.
    ProjectAction(ProjectActionId),
    /// One button of the settings page's action grid.
    Settings(SettingsAction),
    /// A control that is never routed: a caption, a section title, a card's own
    /// labels.
    ///
    /// Its value is zero, which is what [`WidgetId::from_value`] answers `None`
    /// for, so a caption can never be mistaken for the button beside it.
    Decoration,
}

impl WidgetId {
    /// The number the window reports this widget's events with.
    ///
    /// The ranges: pages, tabs and the toolbar from 1000, the status bar and the
    /// strip from 1300, cards from 2000 with sixteen slots each, the three
    /// single-control pages from 3000, the settings grid from 3300, and the card
    /// version menus from [`VERSION_BASE`]. Nothing reaches [`TRAY_BASE`], which
    /// the trays own menu identifiers hold.
    pub const fn value(self) -> u16 {
        match self {
            WidgetId::Page(page) => 1001 + page_index(page),
            WidgetId::Tab(index) => 1100 + index as u16,
            WidgetId::WebPicker => 1200,
            WidgetId::StartStack => 1201,
            WidgetId::StopAll => 1202,
            WidgetId::RestartStack => 1203,
            WidgetId::LandingStart => 1400,
            WidgetId::LandingWelcome => 1401,
            WidgetId::LandingDashboard => 1402,
            WidgetId::Status(part) => 1310 + part as u16,
            WidgetId::Progress => 1313,
            WidgetId::ProgressLabel => 1314,
            WidgetId::Log => 1315,
            WidgetId::Card { index, part } => 2000 + index as u16 * 16 + part_index(part),
            WidgetId::Version { index, variant } => {
                VERSION_BASE + index as u16 * 100 + variant as u16
            }
            WidgetId::EditorFile => 3000,
            WidgetId::EditorSave => 3001,
            WidgetId::EditorReload => 3002,
            WidgetId::EditorText => 3003,
            WidgetId::EditorPath => 3004,
            WidgetId::VhostList => 3100,
            WidgetId::VhostDomainName => 3101,
            WidgetId::VhostDomainExt => 3102,
            WidgetId::VhostPort => 3103,
            WidgetId::VhostServer => 3104,
            WidgetId::VhostDocroot => 3105,
            WidgetId::VhostSave => 3106,
            WidgetId::VhostDelete => 3107,
            WidgetId::VhostApply => 3108,
            WidgetId::ProjectFramework => 3200,
            WidgetId::ProjectName => 3201,
            WidgetId::ProjectDomainName => 3202,
            WidgetId::ProjectDomainExt => 3203,
            WidgetId::ProjectCreate => 3204,
            WidgetId::ProjectList => 3205,
            WidgetId::ProjectLocation => 3206,
            WidgetId::ProjectBrowse => 3207,
            WidgetId::ProjectAdopt => 3208,
            WidgetId::ProjectAction(action) => 3210 + project_action_index(action),
            WidgetId::Settings(action) => 3300 + settings_action_index(action),
            WidgetId::Decoration => 0,
        }
    }

    /// The widget a reported number belongs to, when it is one of ours.
    ///
    /// The inverse of [`WidgetId::value`] for every number this module hands
    /// out; zero - a caption - and anything outside the ranges answer `None`,
    /// which is what makes a stray message harmless.
    pub fn from_value(value: u16) -> Option<Self> {
        match value {
            1001..=1006 => Page::ALL
                .get((value - 1001) as usize)
                .copied()
                .map(WidgetId::Page),
            1100..=1104 => Some(WidgetId::Tab((value - 1100) as usize)),
            1200 => Some(WidgetId::WebPicker),
            1201 => Some(WidgetId::StartStack),
            1202 => Some(WidgetId::StopAll),
            1203 => Some(WidgetId::RestartStack),
            1400 => Some(WidgetId::LandingStart),
            1401 => Some(WidgetId::LandingWelcome),
            1402 => Some(WidgetId::LandingDashboard),
            1310..=1312 => Some(WidgetId::Status((value - 1310) as usize)),
            1313 => Some(WidgetId::Progress),
            1314 => Some(WidgetId::ProgressLabel),
            1315 => Some(WidgetId::Log),
            3000 => Some(WidgetId::EditorFile),
            3001 => Some(WidgetId::EditorSave),
            3002 => Some(WidgetId::EditorReload),
            3003 => Some(WidgetId::EditorText),
            3004 => Some(WidgetId::EditorPath),
            3100 => Some(WidgetId::VhostList),
            3101 => Some(WidgetId::VhostDomainName),
            3102 => Some(WidgetId::VhostDomainExt),
            3103 => Some(WidgetId::VhostPort),
            3104 => Some(WidgetId::VhostServer),
            3105 => Some(WidgetId::VhostDocroot),
            3106 => Some(WidgetId::VhostSave),
            3107 => Some(WidgetId::VhostDelete),
            3108 => Some(WidgetId::VhostApply),
            3200 => Some(WidgetId::ProjectFramework),
            3201 => Some(WidgetId::ProjectName),
            3202 => Some(WidgetId::ProjectDomainName),
            3203 => Some(WidgetId::ProjectDomainExt),
            3204 => Some(WidgetId::ProjectCreate),
            3205 => Some(WidgetId::ProjectList),
            3206 => Some(WidgetId::ProjectLocation),
            3207 => Some(WidgetId::ProjectBrowse),
            3208 => Some(WidgetId::ProjectAdopt),
            3210 => Some(WidgetId::ProjectAction(ProjectActionId::OpenInBrowser)),
            3211 => Some(WidgetId::ProjectAction(ProjectActionId::OpenFolder)),
            3212 => Some(WidgetId::ProjectAction(ProjectActionId::Delete)),
            3300..=3308 => settings_action((value - 3300) as usize).map(WidgetId::Settings),
            // The cards, and above them the version menus the original numbered
            // from 6000.
            2000..=5999 => {
                let slot = (value - 2000) % 16;
                let part = card_part(slot)?;
                Some(WidgetId::Card {
                    index: ((value - 2000) / 16) as usize,
                    part,
                })
            }
            VERSION_BASE..TRAY_BASE => Some(WidgetId::Version {
                index: ((value - VERSION_BASE) / 100) as usize,
                variant: ((value - VERSION_BASE) % 100) as usize,
            }),
            _ => None,
        }
    }
}

/// Where a page sits in the window's page numbering.
const fn page_index(page: Page) -> u16 {
    match page {
        Page::Landing => 0,
        Page::Services => 1,
        Page::Projects => 2,
        Page::Editor => 3,
        Page::Vhosts => 4,
        Page::Settings => 5,
    }
}

/// A card part's slot inside its card's range.
const fn part_index(part: CardPart) -> u16 {
    match part {
        CardPart::Icon => 0,
        CardPart::Dot => 1,
        CardPart::Name => 2,
        CardPart::Status => 3,
        CardPart::Toggle => 4,
        CardPart::Restart => 5,
        CardPart::Configure => 6,
        CardPart::Version => 7,
    }
}

/// The card part a slot names, when it is one of the eight.
const fn card_part(slot: u16) -> Option<CardPart> {
    match slot {
        0 => Some(CardPart::Icon),
        1 => Some(CardPart::Dot),
        2 => Some(CardPart::Name),
        3 => Some(CardPart::Status),
        4 => Some(CardPart::Toggle),
        5 => Some(CardPart::Restart),
        6 => Some(CardPart::Configure),
        7 => Some(CardPart::Version),
        _ => None,
    }
}

const fn project_action_index(action: ProjectActionId) -> u16 {
    match action {
        ProjectActionId::OpenInBrowser => 0,
        ProjectActionId::OpenFolder => 1,
        ProjectActionId::Delete => 2,
    }
}

/// The settings actions, in the order the grid lays them out.
///
/// `Restart as Admin` keeps the seventh slot even though the grid leaves it out
/// when the process is already elevated: a control's number must not move under
/// the window's feet.
const fn settings_action(index: usize) -> Option<SettingsAction> {
    Some(match index {
        0 => SettingsAction::EditConfig,
        1 => SettingsAction::ReloadConfig,
        2 => SettingsAction::ToggleAutoStart,
        3 => SettingsAction::AddToPath,
        4 => SettingsAction::RemoveFromPath,
        5 => SettingsAction::PsqlConsole,
        6 => SettingsAction::Quit,
        7 => SettingsAction::RestartAsAdmin,
        8 => SettingsAction::OpenRepository,
        _ => return None,
    })
}

const fn settings_action_index(action: SettingsAction) -> u16 {
    match action {
        SettingsAction::EditConfig => 0,
        SettingsAction::ReloadConfig => 1,
        SettingsAction::ToggleAutoStart => 2,
        SettingsAction::AddToPath => 3,
        SettingsAction::RemoveFromPath => 4,
        SettingsAction::PsqlConsole => 5,
        SettingsAction::Quit => 6,
        SettingsAction::RestartAsAdmin => 7,
        SettingsAction::OpenRepository => 8,
    }
}

/// What a widget is and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetKind {
    /// A push button.
    Button {
        /// Its text.
        label: String,
        /// The role it is painted with.
        scheme: Scheme,
        /// Whether it accepts input.
        enabled: bool,
    },
    /// Read-only text.
    Label(String),
    /// A single-line text field, with its contents.
    Edit(String),
    /// A drop-down: its items, and which one is selected.
    Combo {
        /// The items, in order.
        items: Vec<String>,
        /// The selected index.
        selected: usize,
    },
    /// A report-style list: its columns and its rows.
    List {
        /// The columns' titles and widths.
        columns: Vec<(&'static str, i32)>,
        /// The rows, one cell per column.
        rows: Vec<Vec<String>>,
    },
    /// The multi-line editor, with what it holds.
    Text(String),
    /// The progress strip.
    Progress {
        /// The text beside the bar.
        label: String,
        /// The bar's position.
        position: u16,
    },
    /// One service card, which the window draws from the view and the layout
    /// the stack already gave it.
    Card {
        /// Where the service sits in the stack's order.
        ///
        /// The frame itself routes nothing, but the controls inside it do, and
        /// their identifiers are built from this index - so the window needs it
        /// to create them, and a click can never address a card the tab moved.
        index: usize,
        /// What the card says.
        view: Box<CardView>,
        /// Where its controls sit.
        layout: Box<CardLayout>,
    },
}

/// One control of the window, ready to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widget {
    /// What it is called when it reports an event.
    pub id: WidgetId,
    /// Where it sits, in page-relative logical pixels.
    pub rect: Rect,
    /// What it is and what it says.
    pub kind: WidgetKind,
}

impl Widget {
    /// A button.
    pub fn button(id: WidgetId, rect: Rect, label: &str, scheme: Scheme, enabled: bool) -> Self {
        Self {
            id,
            rect,
            kind: WidgetKind::Button {
                label: label.to_owned(),
                scheme,
                enabled,
            },
        }
    }

    /// A read-only label.
    pub fn label(id: WidgetId, rect: Rect, text: impl Into<String>) -> Self {
        Self {
            id,
            rect,
            kind: WidgetKind::Label(text.into()),
        }
    }

    /// A text field.
    pub fn edit(id: WidgetId, rect: Rect, text: impl Into<String>) -> Self {
        Self {
            id,
            rect,
            kind: WidgetKind::Edit(text.into()),
        }
    }
}

/// The sidebar: one button per workroom page, the current one marked as the
/// primary. The landing page is not in the sidebar.
pub fn sidebar_widgets(current: Page) -> Vec<Widget> {
    Page::SIDEBAR
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let scheme = if *page == current {
                Scheme::Primary
            } else {
                Scheme::Sidebar
            };
            let (x, y) = sidebar_position(index);
            Widget::button(
                WidgetId::Page(*page),
                (x, y, SIDE_W, SIDE_H),
                page.label(),
                scheme,
                true,
            )
        })
        .collect()
}

/// The web servers the picker offers, in its order.
///
/// The original's two, whatever the configuration says: the setting the picker
/// writes is which of *those* servers owns the port. Named here because the
/// window reads a selection back out of the control, and the list a control was
/// filled from is the only thing that can name what the user chose.
pub const WEB_SERVERS: [&str; 2] = ["Apache", "Nginx"];

/// The services toolbar: the picker and the three stack buttons.
///
/// The picker's items are the original's two, whatever the configuration says:
/// the setting it writes is which of *those* servers owns the port.
pub fn toolbar_widgets(active_web_server: &str) -> Vec<Widget> {
    let items: Vec<String> = WEB_SERVERS.iter().map(|name| (*name).to_owned()).collect();
    let selected = items
        .iter()
        .position(|item| item == active_web_server)
        .unwrap_or(0);

    vec![
        Widget::label(WidgetId::Decoration, TOOLBAR_LABEL_RECT, "Web:"),
        Widget {
            id: WidgetId::WebPicker,
            rect: WEB_PICKER_RECT,
            kind: WidgetKind::Combo { items, selected },
        },
        Widget::button(
            WidgetId::StartStack,
            STACK_BUTTONS[0],
            "Start Stack",
            Scheme::Success,
            true,
        ),
        Widget::button(
            WidgetId::StopAll,
            STACK_BUTTONS[1],
            "Stop All",
            Scheme::Danger,
            true,
        ),
        Widget::button(
            WidgetId::RestartStack,
            STACK_BUTTONS[2],
            "Restart",
            Scheme::Warning,
            true,
        ),
    ]
}

/// The service tabs, with the current one marked as the primary.
pub fn tab_widgets(current: usize) -> Vec<Widget> {
    TABS.iter()
        .enumerate()
        .map(|(index, tab)| {
            let (x, y) = tab_position(index);
            let scheme = if index == current {
                Scheme::Primary
            } else {
                Scheme::Sidebar
            };
            Widget::button(
                WidgetId::Tab(index),
                (x, y, TAB_BTN_W, TAB_BTN_H),
                tab.label,
                scheme,
                true,
            )
        })
        .collect()
}

/// Which cards a tab shows, one entry per service of the stack.
pub fn card_visibility(services: &[ManagedService], tab: usize) -> Vec<bool> {
    services
        .iter()
        .map(|service| tab_shows(tab, ServiceGroup::of_kind(&service.conf().kind)))
        .collect()
}

/// Where the visible cards sit, once the tab's filter has hidden the rest.
///
/// One entry per service: `None` for a card the tab hides, and the position the
/// card is moved to otherwise. The original re-packed the grid on every tab
/// click, which is why the positions come from the filter and not from the
/// stack's order.
pub fn card_positions(
    services: &[ManagedService],
    tab: usize,
    layout: &Layout,
) -> Vec<Option<(i32, i32)>> {
    let mut visible = 0;
    services
        .iter()
        .map(|service| {
            if !tab_shows(tab, ServiceGroup::of_kind(&service.conf().kind)) {
                return None;
            }
            let position = layout.card_position(visible);
            visible += 1;
            Some(position)
        })
        .collect()
}

/// Everything the services page draws: the toolbar, the tabs and the cards.
///
/// Cards are placed in reading order over the grid the layout computed, and the
/// index the view was built from is the one the window reports back, so a click
/// can never address a different service than the one it drew.
pub fn services_widgets(
    services: &[ManagedService],
    active_web_server: &str,
    layout: &Layout,
    current_tab: usize,
) -> Vec<Widget> {
    let mut widgets = toolbar_widgets(active_web_server);
    widgets.extend(tab_widgets(current_tab));

    // The tab decides which cards are on the page at all: one the tab filters
    // out is not created, and the ones that remain are packed from the top-left.
    // That is the grid the original re-arranged on every tab click, and building
    // the list freshly each time is the same grid without the moving controls.
    let positions = card_positions(services, current_tab, layout);

    for (index, service) in services.iter().enumerate() {
        let Some((x, y)) = positions[index] else {
            continue;
        };
        let view = card_view(service, active_web_server);
        let card_layout = CardLayout::compute(view.has_variants);
        widgets.push(Widget {
            id: WidgetId::Decoration,
            rect: (x, y, CARD_W, CARD_H),
            kind: WidgetKind::Card {
                index,
                view: Box::new(view),
                layout: Box::new(card_layout),
            },
        });
    }

    widgets
}

/// One card's own controls, in card-relative pixels.
///
/// The window creates these as children of the card's frame, which is the entry
/// [`services_widgets`] describes.
pub fn card_children(
    index: usize,
    view: &CardView,
    layout: &CardLayout,
) -> Vec<(WidgetId, Rect, WidgetKind)> {
    let id = |part: CardPart| WidgetId::Card { index, part };

    let mut children = vec![
        (
            id(CardPart::Icon),
            layout.icon,
            WidgetKind::Label(String::new()),
        ),
        (
            id(CardPart::Dot),
            layout.dot,
            WidgetKind::Label(view.dot.to_string()),
        ),
        (
            id(CardPart::Name),
            layout.name,
            WidgetKind::Label(view.name.clone()),
        ),
        (
            id(CardPart::Status),
            layout.status,
            WidgetKind::Label(view.status.clone()),
        ),
    ];

    let mut buttons = vec![
        (&view.toggle, CardPart::Toggle),
        (&view.restart, CardPart::Restart),
    ];
    if let Some(configure) = view.configure() {
        buttons.push((configure, CardPart::Configure));
    }
    if let Some(version) = view
        .buttons
        .iter()
        .find(|button| button.action == CardAction::Version)
    {
        buttons.push((version, CardPart::Version));
    }

    for (slot, (button, part)) in buttons.iter().enumerate() {
        if let Some(rect) = layout.buttons.get(slot) {
            children.push((
                id(*part),
                *rect,
                WidgetKind::Button {
                    label: button.label.to_owned(),
                    scheme: button.scheme,
                    enabled: button.enabled,
                },
            ));
        }
    }

    children
}

/// The extension picker's items: the catalogue's list, in its order.
fn extension_items() -> Vec<String> {
    crate::frameworks::DOMAIN_EXTENSIONS
        .iter()
        .map(|extension| (*extension).to_owned())
        .collect()
}

/// The editor page's controls.
pub fn editor_widgets(editor: &Editor, layout: &Layout) -> Vec<Widget> {
    let page = EditorLayout::compute(layout);
    let items: Vec<String> = editor.files.iter().map(editor_label).collect();

    vec![
        Widget::label(WidgetId::Decoration, page.file_label, "File:"),
        Widget {
            id: WidgetId::EditorFile,
            rect: page.dropdown,
            kind: WidgetKind::Combo {
                items,
                selected: editor.selected.unwrap_or(0),
            },
        },
        Widget::button(
            WidgetId::EditorSave,
            page.save,
            "Save",
            Scheme::Success,
            true,
        ),
        Widget::button(
            WidgetId::EditorReload,
            page.reload,
            "Reload",
            Scheme::Neutral,
            true,
        ),
        Widget::label(WidgetId::Decoration, page.path_label, "Path:"),
        Widget::label(WidgetId::EditorPath, page.path_value, editor.status_text()),
        Widget {
            id: WidgetId::EditorText,
            rect: page.content,
            kind: WidgetKind::Text(editor.text.clone()),
        },
    ]
}

/// The virtual-hosts page's controls.
///
/// The form's *state* is a [`crate::vhost::VhostForm`] - the page keeps one and
/// its rules are the engine's - and the table's rows come from
/// [`vhost_list`], so the five cells the original showed are the ones
/// [`crate::vhost::vhost_row`] composed.
pub fn vhosts_widgets(
    form: &crate::vhost::VhostForm,
    rows: &[crate::vhost::VhostRow],
) -> Vec<Widget> {
    let page = VhostsLayout::compute(&Layout::compute(1));

    vec![
        Widget {
            id: WidgetId::VhostList,
            rect: page.list,
            kind: WidgetKind::List {
                columns: vec![
                    ("", 30),
                    ("Domain", 180),
                    ("Document Root", 360),
                    ("Port", 60),
                    ("Server", 90),
                ],
                rows: rows
                    .iter()
                    .map(|row| {
                        vec![
                            row.marker.to_owned(),
                            row.domain.clone(),
                            row.docroot.clone(),
                            row.port.to_string(),
                            row.server.clone(),
                        ]
                    })
                    .collect(),
            },
        },
        Widget::label(WidgetId::Decoration, page.domain_label, "Domain"),
        Widget::edit(
            WidgetId::VhostDomainName,
            page.domain_name,
            form.name.clone(),
        ),
        Widget {
            id: WidgetId::VhostDomainExt,
            rect: page.domain_ext,
            kind: WidgetKind::Combo {
                items: extension_items(),
                selected: crate::vhost::domain_extension_index(&form.extension),
            },
        },
        Widget::label(WidgetId::Decoration, page.port_label, "Port"),
        Widget::edit(WidgetId::VhostPort, page.port, form.port.clone()),
        Widget::label(WidgetId::Decoration, page.server_label, "Server"),
        Widget {
            id: WidgetId::VhostServer,
            rect: page.server,
            kind: WidgetKind::Combo {
                items: SERVER_OPTIONS
                    .iter()
                    .map(|option| (*option).to_owned())
                    .collect(),
                selected: server_index(&form.server),
            },
        },
        Widget::label(WidgetId::Decoration, page.docroot_label, "DocRoot"),
        Widget::edit(WidgetId::VhostDocroot, page.docroot, form.docroot.clone()),
        Widget::button(
            WidgetId::VhostSave,
            page.save,
            "Save",
            Scheme::Success,
            true,
        ),
        Widget::button(
            WidgetId::VhostDelete,
            page.delete,
            "Delete",
            Scheme::Danger,
            true,
        ),
        Widget::button(
            WidgetId::VhostApply,
            page.apply,
            "Apply to System",
            Scheme::Primary,
            true,
        ),
    ]
}

/// The projects page's controls.
/// The landing page the window opens on.
///
/// The entrance to the installation: start the stack - which is what serves
/// the welcome page on localhost - open that page directly, or go to the
/// dashboard, which is where a folder becomes a project.
pub fn landing_widgets() -> Vec<Widget> {
    let page = LandingLayout::compute(&Layout::compute(1));
    vec![
        Widget::label(WidgetId::Decoration, page.title, "Lambo PHP"),
        Widget::label(
            WidgetId::Decoration,
            page.tagline,
            "Your local PHP stack - Apache, PHP and MariaDB, one click away.",
        ),
        Widget::button(
            WidgetId::LandingStart,
            page.start,
            "Start Stack & Open Welcome Page",
            Scheme::Primary,
            true,
        ),
        Widget::button(
            WidgetId::LandingWelcome,
            page.welcome,
            "Open Welcome Page",
            Scheme::Neutral,
            true,
        ),
        Widget::button(
            WidgetId::LandingDashboard,
            page.dashboard,
            "Open Dashboard",
            Scheme::Neutral,
            true,
        ),
        Widget::label(
            WidgetId::Decoration,
            page.hint,
            "The dashboard is where you point Lambo at your project.",
        ),
    ]
}

pub fn projects_widgets(
    projects: &[PanelProject],
    form: &ProjectForm,
    runtime_status: &str,
    location: &str,
) -> Vec<Widget> {
    let page = ProjectsLayout::compute(&Layout::compute(1));
    let actions = projects_actions();
    let frameworks = framework_names();

    let mut widgets = vec![
        Widget::label(WidgetId::Decoration, page.framework_label, "Framework:"),
        Widget {
            id: WidgetId::ProjectFramework,
            rect: page.framework,
            kind: WidgetKind::Combo {
                items: frameworks.iter().map(|name| (*name).to_owned()).collect(),
                selected: frameworks
                    .iter()
                    .position(|name| *name == form.framework)
                    .unwrap_or(0),
            },
        },
        Widget::label(WidgetId::Decoration, page.name_label, "Name:"),
        Widget::edit(WidgetId::ProjectName, page.name, form.name.clone()),
        Widget::label(WidgetId::Decoration, page.domain_label, "Domain:"),
        Widget::edit(
            WidgetId::ProjectDomainName,
            page.domain_name,
            form.domain_name.clone(),
        ),
        Widget {
            id: WidgetId::ProjectDomainExt,
            rect: page.domain_ext,
            kind: WidgetKind::Combo {
                items: extension_items(),
                selected: crate::vhost::domain_extension_index(&form.extension),
            },
        },
        Widget::button(
            WidgetId::ProjectCreate,
            page.create,
            "Create Project",
            Scheme::Success,
            true,
        ),
        Widget::label(
            WidgetId::Decoration,
            page.runtime,
            format!("Runtime status: {runtime_status}"),
        ),
        Widget::label(
            WidgetId::Decoration,
            page.list_label,
            "Existing projects (drops in www/):",
        ),
        Widget {
            id: WidgetId::ProjectList,
            rect: page.list,
            kind: WidgetKind::List {
                columns: vec![
                    ("Name", 120),
                    ("Framework", 140),
                    ("Domain", 160),
                    ("Document Root", 340),
                ],
                rows: project_rows(projects)
                    .iter()
                    .map(|row| {
                        vec![
                            row.name.clone(),
                            row.framework.clone(),
                            row.domain.clone(),
                            row.docroot.clone(),
                        ]
                    })
                    .collect(),
            },
        },
    ];

    for (index, action) in actions.iter().enumerate() {
        widgets.push(Widget::button(
            WidgetId::ProjectAction(action.id),
            page.actions[index],
            action.label,
            action.scheme,
            true,
        ));
    }

    // Load a project the user already has: pick a folder - or type one - and
    // open it. Detection works out the framework, the document root and the
    // rest, the way `lambo init` does.
    widgets.push(Widget::label(
        WidgetId::Decoration,
        page.location_label,
        "Location:",
    ));
    widgets.push(Widget::edit(
        WidgetId::ProjectLocation,
        page.location,
        location.to_owned(),
    ));
    widgets.push(Widget::button(
        WidgetId::ProjectBrowse,
        page.browse,
        "Browse\u{2026}",
        Scheme::Neutral,
        true,
    ));
    widgets.push(Widget::button(
        WidgetId::ProjectAdopt,
        page.open,
        "Open Project",
        Scheme::Primary,
        true,
    ));

    widgets
}

/// The settings page's buttons, in the order its grid laid them out.
fn view_actions(view: &SettingsView) -> Vec<SettingsButton> {
    view.blocks
        .iter()
        .find_map(|block| match block {
            SettingsBlock::Actions(actions) => Some(actions.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The settings page's controls, from the page's own layout.
pub fn settings_widgets(view: &SettingsView) -> Vec<Widget> {
    let actions = view_actions(view);
    settings_layout(view)
        .into_iter()
        .filter_map(|item| match item.kind {
            SettingsItemKind::Divider => None,
            SettingsItemKind::Action(index) => {
                let action = *actions.get(index)?;
                Some(Widget::button(
                    WidgetId::Settings(action.action),
                    item.rect,
                    &item.text,
                    action.scheme,
                    action.enabled,
                ))
            }
            _ => Some(Widget::label(WidgetId::Decoration, item.rect, item.text)),
        })
        .collect()
}

/// The progress strip, the log panel and the status bar.
pub fn footer_widgets(
    progress: &ProgressView,
    log: &str,
    page: Page,
    base_dir: &Path,
    version: &str,
    layout: &Layout,
) -> Vec<Widget> {
    let status = status_parts(page, base_dir, version);
    let mut x = 0;
    let mut widgets = Vec::new();
    for (index, part) in status.iter().enumerate() {
        // A width of zero is the middle part, which takes what is left of the
        // window once the other two have taken theirs.
        let width = match STATUS_PART_WIDTHS[index] {
            0 => WINDOW_W - STATUS_PART_WIDTHS[0] - STATUS_PART_WIDTHS[2],
            width => width,
        };
        widgets.push(Widget::label(
            WidgetId::Status(index),
            (x, layout.window_h - 26, width, 22),
            part.clone(),
        ));
        x += width;
    }

    // The original's strip: the label at the left of the row, the bar from
    // x=400 to x=950.
    widgets.push(Widget::label(
        WidgetId::ProgressLabel,
        (GRID_X, layout.progress_y + 3, 380, 16),
        progress.label.clone(),
    ));
    widgets.push(Widget {
        id: WidgetId::Progress,
        rect: (400, layout.progress_y, 550, PROGRESS_H),
        kind: WidgetKind::Progress {
            label: progress.label.clone(),
            position: progress.position,
        },
    });

    widgets.push(Widget::label(
        WidgetId::Decoration,
        (
            LOG_X,
            layout.log_y - LOG_LABEL_ABOVE,
            LOG_W,
            LOG_LABEL_ABOVE,
        ),
        "Logs",
    ));
    widgets.push(Widget {
        id: WidgetId::Log,
        rect: (LOG_X, layout.log_y, LOG_W, LOG_H),
        kind: WidgetKind::Text(log.to_owned()),
    });
    widgets
}
