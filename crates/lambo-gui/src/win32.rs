//! The Win32 window.
//!
//! This is the one part of the product that cannot be exercised in CI: there is
//! no Windows desktop here, so it is cross-compiled and type-checked, and
//! nothing more. To keep that limitation from spreading, this module does as
//! little as possible - it creates controls, pumps messages, and forwards
//! button presses to [`lambo_core::app`]. Every decision about *what to show*
//! is made in [`crate::view`], which is ordinary Rust and is tested on Linux.
//!
//! # Safety
//!
//! `unsafe` here is unavoidable: Win32 is FFI. It is confined to this module,
//! and the invariants that make it sound are narrow and stated where they are
//! relied on - chiefly that window handles outlive the message loop that
//! created them, and that every wide string passed to Win32 is NUL-terminated
//! and outlives the call.
//!
//! The `unsafe_op_in_unsafe_fn` lint is allowed for this module because every
//! statement in it is an FFI call. Edition 2024 wants each unsafe operation in
//! its own block so the unsafe scope is explicit; in a module with no safe code
//! at all that marking would be noise, and it would obscure the invariants that
//! actually matter, which are documented above and at each function.

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;

use lambo_core::app::App;
use lambo_core::project::Project;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::SS_NOTIFY;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem,
    GetMessageW, HMENU, MSG, PostQuitMessage, RegisterClassW, STN_CLICKED, SW_SHOW, SetWindowTextW,
    ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_TIMER, WNDCLASSW,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// Control identifiers.
///
/// Distinct ranges so a new control cannot silently collide with an existing
/// one, which would route a button press to the wrong handler.
const ID_START: i32 = 101;
const ID_STOP: i32 = 102;
const ID_OPEN: i32 = 103;
const ID_DBUI: i32 = 104;
/// First of the navigation buttons; they are laid out consecutively.
const ID_NAV_BASE: i32 = 300;
const ID_PROJECT: i32 = 201;
const ID_STATE: i32 = 202;
/// First of the status lines; they are laid out consecutively.
const ID_LINE_BASE: i32 = 210;
/// How many status lines are reserved.
///
/// Sized for the longest screen (Logs, Settings), not for the dashboard: a
/// screen that truncates silently is worse than a taller window.
const LINE_SLOTS: usize = 14;

/// The screens the window can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Dashboard,
    Projects,
    Php,
    Services,
    Database,
    Logs,
    Settings,
    About,
}

impl Screen {
    /// Every screen, in navigation order.
    const ALL: [Screen; 8] = [
        Screen::Dashboard,
        Screen::Projects,
        Screen::Php,
        Screen::Services,
        Screen::Database,
        Screen::Logs,
        Screen::Settings,
        Screen::About,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Projects => "Projects",
            Self::Php => "PHP",
            Self::Services => "Services",
            Self::Database => "Database",
            Self::Logs => "Logs",
            Self::Settings => "Settings",
            Self::About => "About",
        }
    }
}
const ID_NOTICE: i32 = 230;

/// Refresh interval, in milliseconds.
///
/// Polling rather than pushing: the engine has no background threads, so the
/// window decides when to look. Two seconds is responsive enough for a service
/// to appear to come up and cheap enough to leave running.
const REFRESH_MS: u32 = 2_000;
const TIMER_ID: usize = 1;

// The window's state.
//
// Held in a thread-local because a window procedure is a plain function pointer
// with no receiver. A single-window application has exactly one instance, so
// this is the state for it. (A doc comment would not attach to a macro
// invocation, hence this one is ordinary.)
thread_local! {
    static STATE: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Everything the window needs between messages.
struct Ui {
    app: App,
    project: Option<Project>,
    /// Status-line controls, in display order.
    lines: Vec<HWND>,
    /// The screen currently on display.
    screen: Screen,
}

/// A wide, NUL-terminated string that outlives the call it is passed to.
///
/// Win32 reads to the terminator, so the terminator is not optional, and the
/// buffer must stay alive for the duration of the call - which it does, being
/// owned by the caller's local.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates the window and runs the message loop.
///
/// Returns the process exit code.
pub fn run() -> i32 {
    let class_name = wide("LamboPHP.MainWindow");
    let title = wide("Lambo PHP");

    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(std::ptr::null()),
            lpszClassName: class_name.as_ptr(),
            lpszMenuName: std::ptr::null(),
            // A null background brush means the window is not erased for us;
            // the static controls cover the client area, so nothing flashes.
            hbrBackground: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            style: 0,
            cbClsExtra: 0,
            cbWndExtra: 0,
        };
        RegisterClassW(&class);

        // A fixed-size window: the dashboard is a small, stable layout, and
        // making it resizable would mean re-laying out controls for no benefit.
        // WS_MAXIMIZEBOX and WS_THICKFRAME are removed from the overlapped set.
        let style = WS_OVERLAPPEDWINDOW & !0x0001_0000 & !0x0002_0000;
        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            560,
            500,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        if window.is_null() {
            return 1;
        }

        ShowWindow(window, SW_SHOW);

        let mut message = MSG {
            hwnd: std::ptr::null_mut(),
            message: 0,
            wParam: 0,
            lParam: 0,
            time: 0,
            pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
        };
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    0
}

/// The window procedure.
///
/// `extern "system"` because Win32 calls it with the platform calling
/// convention, not Rust's.
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            create_controls(window);
            refresh(window);
            // The engine has no background threads, so the window polls it.
            windows_sys::Win32::UI::WindowsAndMessaging::SetTimer(
                window, TIMER_ID, REFRESH_MS, None,
            );
            0
        }
        WM_TIMER => {
            refresh(window);
            0
        }
        WM_COMMAND => {
            // The control id is the low word of wParam, the notification code
            // the high word. A static only sends STN_CLICKED when it was
            // created with SS_NOTIFY.
            let id = (wparam as u32 & 0xffff) as i32;
            let code = wparam as u32 >> 16;
            if code == STN_CLICKED && id >= ID_LINE_BASE && id < ID_LINE_BASE + LINE_SLOTS as i32 {
                let index = (id - ID_LINE_BASE) as usize;
                // Only the Projects screen has selectable rows; elsewhere the
                // lines are read-only and a click means nothing.
                let on_projects = STATE.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .is_some_and(|ui| ui.screen == Screen::Projects)
                });
                if on_projects {
                    select_project(window, index);
                    return 0;
                }
            }
            handle_command(window, id);
            0
        }
        WM_CLOSE => {
            // Closing the window stops the message loop. Services keep running,
            // which is the documented behaviour: a dashboard is not a service
            // supervisor, and quitting it must not take a project down.
            DestroyWindow(window);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Creates the static text and buttons.
unsafe fn create_controls(window: HWND) {
    let static_class = wide("STATIC");
    let button_class = wide("BUTTON");
    let instance = GetModuleHandleW(std::ptr::null());
    let child = WS_CHILD | WS_VISIBLE;

    let make = |class: &[u16], text: &[u16], id: i32, x: i32, y: i32, w: i32, h: i32| -> HWND {
        CreateWindowExW(
            0,
            class.as_ptr(),
            text.as_ptr(),
            child,
            x,
            y,
            w,
            h,
            window,
            id as HMENU,
            instance,
            std::ptr::null(),
        )
    };

    // The navigation row. Plain buttons rather than a tab control: the tab
    // common control needs an initialization call and notification handling
    // that buys nothing here, where the screens are a fixed set.
    for (index, screen) in Screen::ALL.iter().enumerate() {
        make(
            &button_class,
            &wide(screen.title()),
            ID_NAV_BASE + index as i32,
            12 + index as i32 * 68,
            12,
            64,
            24,
        );
    }

    make(
        &static_class,
        &wide("No project selected"),
        ID_PROJECT,
        20,
        48,
        500,
        24,
    );
    make(&static_class, &wide("Stopped"), ID_STATE, 20, 72, 500, 20);

    let lines = (0..LINE_SLOTS)
        .map(|index| {
            CreateWindowExW(
                // SS_NOTIFY, so a row on the Projects screen can be clicked to
                // select that project. The other screens ignore the click.
                SS_NOTIFY,
                static_class.as_ptr(),
                wide("").as_ptr(),
                child,
                20,
                100 + index as i32 * 22,
                520,
                20,
                window,
                (ID_LINE_BASE + index as i32) as HMENU,
                instance,
                std::ptr::null(),
            )
        })
        .collect();

    make(&static_class, &wide(""), ID_NOTICE, 20, 414, 520, 20);

    let buttons = [
        (ID_OPEN, "Open localhost"),
        (ID_DBUI, "Open phpMyAdmin"),
        (ID_START, "Start"),
        (ID_STOP, "Stop"),
    ];
    for (offset, (id, label)) in buttons.iter().enumerate() {
        make(
            &button_class,
            &wide(label),
            *id,
            20 + offset as i32 * 105,
            440,
            100,
            30,
        );
    }

    STATE.with(|state| {
        let app = match App::open() {
            Ok(app) => app,
            // The window still comes up; `refresh` reports the failure in the
            // notice line rather than the process dying with no explanation.
            Err(error) => {
                set_text_by_id(window, ID_NOTICE, &format!("Lambo error: {error}"));
                return;
            }
        };
        // Open on the first registered project rather than on none. Without
        // this the window comes up with Start permanently refused and the user
        // has to discover the Projects screen before anything works at all -
        // and with one project registered, which is the common case, there is
        // nothing to choose between anyway. The Projects screen still lets you
        // switch.
        let initial = app
            .projects()
            .ok()
            .and_then(|projects| projects.into_iter().next())
            .and_then(|info| Project::load(&info.path).ok());

        *state.borrow_mut() = Some(Ui {
            app,
            project: initial,
            lines,
            screen: Screen::Dashboard,
        });
    });
}

/// Repaints the current screen from the engine's current state.
///
/// Every screen is derived from the same `lambo_core::app` calls the CLI
/// makes, so the window and a terminal cannot disagree about what is running.
unsafe fn refresh(window: HWND) {
    STATE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(ui) = borrow.as_mut() else {
            return;
        };

        let dashboard = match ui.app.dashboard(ui.project.as_ref()) {
            Ok(dashboard) => dashboard,
            Err(error) => {
                set_text_by_id(window, ID_NOTICE, &format!("Error: {error}"));
                return;
            }
        };
        let view = crate::view::render(&dashboard, ui.project.is_some());

        set_text_by_id(window, ID_PROJECT, &view.project);
        set_text_by_id(window, ID_STATE, &view.state);

        // The dashboard keeps its own header and notice; the other screens
        // replace the line area with their own content and keep the dashboard's
        // notice, so a failure stays visible wherever the user navigates to.
        let screen_lines = match ui.screen {
            Screen::Dashboard => view.lines.clone(),
            Screen::Projects => match ui.app.projects() {
                Ok(projects) => {
                    // Bound before borrowing: `name()` returns an owned String,
                    // so the borrow has to outlive the call that made it.
                    let active_name = ui.project.as_ref().map(|project| project.name());
                    crate::view::render_projects(&projects, active_name.as_deref())
                }
                Err(error) => error_lines(&error.to_string()),
            },
            Screen::Php => match ui.app.runtimes() {
                Ok(runtimes) => crate::view::render_runtimes(&runtimes),
                Err(error) => error_lines(&error.to_string()),
            },
            Screen::Services => crate::view::render_services(&dashboard),
            Screen::Database => crate::view::render_database(&dashboard),
            Screen::Logs => match ui.app.logs(5) {
                Ok(logs) => crate::view::render_logs(&logs, 5),
                Err(error) => error_lines(&error.to_string()),
            },
            Screen::Settings => crate::view::render_settings(&ui.app.config),
            Screen::About => crate::view::render_about(&ui.app.about()),
        };

        // Cloned so the control handles can be read without holding two
        // mutable borrows of `ui` at once.
        let lines = ui.lines.clone();
        for (index, slot) in lines.iter().enumerate() {
            let text = match screen_lines.get(index) {
                Some(line) => {
                    let mark = if line.ok { "\u{2713}" } else { "\u{2014}" };
                    format!("{}  {:<12} {}", mark, line.label, line.value)
                }
                None => String::new(),
            };
            set_text(*slot, &text);
        }

        set_text_by_id(window, ID_NOTICE, view.notice.as_deref().unwrap_or(""));

        enable(window, ID_STOP, view.can_stop);
        enable(window, ID_START, view.can_start);
        // The buttons exist whether or not there is a URL; they are disabled
        // rather than hidden so the layout does not move.
        enable(window, ID_OPEN, view.url.is_some());
        enable(window, ID_DBUI, view.database_ui_url.is_some());
    });
}

/// Makes the project at `index` in the registry the active one.
///
/// The registry is re-read rather than cached, so a project added or removed
/// since the window opened is reflected instead of selecting by a stale index.
unsafe fn select_project(window: HWND, index: usize) {
    STATE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(ui) = borrow.as_mut() else {
            return;
        };
        let projects = match ui.app.projects() {
            Ok(projects) => projects,
            Err(error) => {
                set_text_by_id(
                    window,
                    ID_NOTICE,
                    &format!("Could not list projects: {error}"),
                );
                return;
            }
        };
        let Some(info) = projects.get(index) else {
            // A row with no project behind it is an empty slot.
            return;
        };
        match Project::load(&info.path) {
            Ok(project) => {
                ui.project = Some(project);
                set_text_by_id(window, ID_NOTICE, &format!("Selected {}", info.name));
            }
            Err(error) => set_text_by_id(
                window,
                ID_NOTICE,
                &format!("Could not open {}: {error}", info.name),
            ),
        }
    });
    refresh(window);
}

/// Renders a screen that could not load its data.
///
/// Shown in the line area rather than only the notice, because an empty screen
/// with a small grey line under it reads as "nothing here" rather than as
/// "this failed".
fn error_lines(message: &str) -> Vec<crate::view::StatusLine> {
    vec![crate::view::StatusLine {
        label: "Error".to_owned(),
        value: message.to_owned(),
        ok: false,
    }]
}

/// Handles a button press.
unsafe fn handle_command(window: HWND, id: i32) {
    // Starting and stopping are synchronous and can take several seconds while
    // a runtime downloads or a database initialises. Painting the in-progress
    // state first is what separates "working" from "hung": without it the
    // window shows the previous state until the operation returns, and a user
    // who clicks twice is not being told anything.
    if matches!(id, ID_START | ID_STOP) {
        let verb = if id == ID_START {
            "Starting"
        } else {
            "Stopping"
        };
        set_text_by_id(window, ID_STATE, &format!("{verb}\u{2026}"));
        set_text_by_id(window, ID_NOTICE, "");
        // Both are refused for the duration: a second Start while one is
        // already running would race it, and Stop cannot interrupt a start.
        enable(window, ID_START, false);
        enable(window, ID_STOP, false);
    }

    // The action is taken with the state borrowed mutably, then the screen is
    // repainted after the borrow is released: a button that starts a service
    // must show the new state, not the one from before the click.
    let outcome = STATE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let ui = borrow.as_mut()?;
        match id {
            ID_START => match &ui.project {
                Some(project) => Some(ui.app.start_all(project, true)),
                // No project is open, so there is nothing to start. The button
                // says so rather than doing nothing silently.
                None => {
                    set_text_by_id(
                        window,
                        ID_NOTICE,
                        "No project is open. Run `lambo init` in the project folder.",
                    );
                    None
                }
            },
            ID_STOP => Some(ui.app.stop_all()),
            ID_OPEN => {
                if let Some(url) = ui
                    .app
                    .dashboard(ui.project.as_ref())
                    .ok()
                    .and_then(|d| d.url)
                {
                    let _ = lambo_core::browser::open(&url, ui.app.os);
                }
                None
            }
            ID_DBUI => {
                let _ = ui.app.open_database_ui(ui.project.as_ref());
                None
            }
            // Navigation. Switching screens is not an operation on the
            // engine, so nothing is reported and nothing can fail here.
            id if id >= ID_NAV_BASE && id < ID_NAV_BASE + Screen::ALL.len() as i32 => {
                let index = (id - ID_NAV_BASE) as usize;
                if let Some(screen) = Screen::ALL.get(index) {
                    ui.screen = *screen;
                }
                None
            }
            _ => None,
        }
    });

    if let Some(outcome) = outcome {
        if !outcome.ok {
            // Not just the error string: the engine also worked out the likely
            // causes and the command to run next, and a notice that drops both
            // leaves the user to go and find the CLI.
            set_text_by_id(window, ID_NOTICE, &crate::view::failure_message(&outcome));
        }
    }
    refresh(window);
}

/// Sets a control's text by control id.
unsafe fn set_text_by_id(window: HWND, id: i32, text: &str) {
    let control = GetDlgItem(window, id);
    if !control.is_null() {
        set_text(control, text);
    }
}

/// Sets a control's text.
///
/// # Safety
///
/// `control` must be a valid window handle. The wide buffer is owned by this
/// function's frame and so outlives the `SetWindowTextW` call.
unsafe fn set_text(control: HWND, text: &str) {
    let buffer = wide(text);
    SetWindowTextW(control, buffer.as_ptr());
}

/// Enables or disables a control.
unsafe fn enable(window: HWND, id: i32, enabled: bool) {
    let control = GetDlgItem(window, id);
    if !control.is_null() {
        EnableWindow(control, if enabled { 1 } else { 0 });
    }
}
