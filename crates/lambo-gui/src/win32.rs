//! The Win32 window: it creates the controls, paints the panel, and performs
//! what the user asked for.
//!
//! This is the one part of the product that cannot be exercised where it is
//! written - there is no Windows desktop here, so it is cross-compiled and
//! type-checked and nothing more - so it does as little as it can. `ui_state`
//! decides what is on screen, [`crate::view`] decides how it looks,
//! [`crate::state::Panel`] decides what a click means, and [`crate::ops`] calls
//! the engine. What is left here is deliberately mechanical: create a control per
//! widget, paint the rectangles the model described, and pump messages.
//!
//! # Two kinds of control, and why
//!
//! Anything the user *types into* is a real Win32 control - a button, an edit
//! field, a combo box - because the platform's own keyboard handling, focus ring
//! and IME are better than anything reimplemented here. Anything the user only
//! *reads* - a service card, a table, the progress strip - is painted in
//! `WM_PAINT`, because those are the places the panel has to look like Lambo PHP
//! rather than like a form: the accent bar that says how a card is doing, the
//! status dot, the row highlight. The model describes both the same way; this
//! module is where the distinction is made, in [`build`] and its counterpart
//! [`paint`].
//!
//! # Rebuilding rather than moving
//!
//! The original moved its cards around when the services tab changed, hiding the
//! ones the tab did not cover and re-packing the rest. What a user sees is the
//! re-pack, so the widgets a tab filters out are simply not built and the ones
//! that remain are created in their packed positions. When only the *text* of a
//! control changes - a card's status line, the log growing, a button going from
//! `Start` to `Stop` - the existing controls are updated in place instead, and
//! that difference is the whole of [`structure`]: which controls exist and where,
//! not what they say.
//!
//! # Safety
//!
//! Win32 is FFI, so `unsafe` is unavoidable and is confined to this module. The
//! invariants that make it sound are narrow: a handle is used while its window
//! lives (the `Ui` in the thread-local owns every control it created, and they
//! are destroyed before the next set is created), every wide string passed to
//! Win32 is NUL-terminated and outlives the call, and the thread-local is only
//! touched from this thread's message loop.
//!
//! One hazard deserves naming: setting a control's text sends its notification
//! back to this window *reentrantly*, while the state is still borrowed. Every
//! handler therefore takes the state with `try_borrow_mut` and does nothing when
//! it is already held - which is exactly the right answer, because the model is
//! what asked for the text in the first place and the message carries nothing it
//! does not already know.

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lambo_core::panel::{CONFIG_FILE, PanelConfig};
use lambo_core::pathenv;
use lambo_core::paths::Paths;
use lambo_core::tray::{self, TrayCommand, TrayItem};
use lambo_core::ui_state::{
    CardLayout, CardPart, CardView, Page, Rect, Scheme, Widget, WidgetId, WidgetKind,
    card_children, version_menu, version_menu_title,
};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint,
    FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, FillRect, FrameRect, HBRUSH, HDC, HFONT, HGDIOBJ,
    InvalidateRect, PAINTSTRUCT, ScreenToClient, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
// `SS_LEFT` and `SS_NOPREFIX` are the static-control styles, and windows-sys
// files them with the rest of the window classes rather than with the controls.
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{SS_LEFT, SS_NOPREFIX};
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_SETLIMITTEXT, ODS_DISABLED, ODS_SELECTED, ODT_BUTTON,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{
    BIF_RETURNONLYFSDIRS, BROWSEINFOW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW, SHBrowseForFolderW, SHGetPathFromIDListW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BN_CLICKED, BS_OWNERDRAW, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBN_SELCHANGE,
    CBS_DROPDOWNLIST, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DI_NORMAL, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, DrawIconEx, EN_CHANGE,
    ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_LEFT, ES_MULTILINE, ES_READONLY, ES_WANTRETURN,
    GetClientRect, GetCursorPos, GetMessageW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    HICON, HMENU, IMAGE_ICON, KillTimer, LR_DEFAULTSIZE, LR_LOADFROMFILE, LoadImageW, MF_CHECKED,
    MF_DISABLED, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, SB_BOTTOM, SC_MINIMIZE, SW_HIDE, SW_RESTORE, SW_SHOW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowTextW, ShowWindow, TPM_LEFTBUTTON, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM,
    WM_GETMINMAXINFO, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_PAINT, WM_SETFONT, WM_SIZE,
    WM_SYSCOMMAND, WM_TIMER, WM_VSCROLL, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};

use crate::ops::{self, FollowUp};
use crate::state::{Action, Clock, Panel};
use crate::view::{self, Font, FontWeight, Palette, Rgb};

/// The window class this application registers.
const CLASS_NAME: &str = "LamboPHP.MainWindow";

/// How often the panel asks the engine whether anything has moved.
///
/// The engine has no background threads: a window that wants to know runs a
/// timer. Twice a second is enough for a service to appear to come up and cheap
/// enough to leave running.
const REFRESH_MS: u32 = 500;
const TIMER_REFRESH: usize = 1;

/// The notification icon's identifier, and the message it calls back with.
const TRAY_UID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the engine's state callbacks, from whichever thread noticed.
const WM_WAKE: u32 = WM_APP + 2;

/// The tray callback's `lParam` values: a message number, not a code.
const WM_LBUTTONUP_: u32 = 0x0202;
const WM_RBUTTONUP_: u32 = 0x0205;

/// What the window's caption and border take, on top of the layout's own size.
///
/// The panel is laid out in logical pixels, so the *client* area is what the
/// layout is scaled into and the frame is added around it.
const FRAME_W: i32 = 16;
const FRAME_H: i32 = 39;

/// A table's header height and row height, in logical pixels.
const HEADER_H: i32 = 24;
const ROW_H: i32 = 20;

thread_local! {
    static STATE: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Everything the window needs between messages.
struct Ui {
    /// The panel: its own state, its routing table and the engine handle.
    panel: Panel,
    /// The widgets the current page is built from, as the model has them now.
    widgets: Vec<Widget>,
    /// The controls that exist, by the number they report events with.
    controls: BTreeMap<u16, HWND>,
    /// What each control last said, so a repaint only writes what changed.
    texts: BTreeMap<u16, String>,
    /// Which controls exist and where, so a structure change is noticed.
    structure: Vec<(u16, Rect, bool)>,
    /// The colours the owner-drawn buttons are painted with, by control number.
    schemes: BTreeMap<u16, Scheme>,
    /// The fonts, keyed by size, weight and pitch, at the current scale.
    fonts: BTreeMap<(i32, u8, bool), HFONT>,
    /// The service icons, by service name: loaded once, at the current size.
    icons: BTreeMap<String, Option<HICON>>,
    /// The brushes the `WM_CTLCOLOR` handlers hand back to Win32.
    panel_brush: HBRUSH,
    log_brush: HBRUSH,
    /// The scale the panel is drawn at, in dots per inch.
    dpi: u32,
    /// The palette the system's own light/dark preference selected.
    palette: Palette,
    /// Whether the notification icon is installed.
    tray: bool,
    /// Whether the window is hidden in the tray.
    hidden: bool,
    /// Set once the engine has been told to stop everything and the window is on
    /// its way out, so a close destroys it instead of hiding it.
    quitting: bool,
}

impl Ui {
    /// The panel, its brushes and its scale, or `None` when there is no
    /// installation to show.
    unsafe fn open(hidden: bool) -> Option<Self> {
        let paths = match Paths::detect() {
            Ok(paths) => paths,
            Err(error) => {
                fail(&error.to_string());
                return None;
            }
        };
        if let Err(error) = paths.ensure_layout() {
            fail(&format!("could not create the installation: {error}"));
            return None;
        }

        let base_dir = paths.root().to_path_buf();
        let config = match PanelConfig::load(&base_dir) {
            Ok(config) => config,
            Err(error) => {
                fail(&format!(
                    "could not read {}: {error}",
                    base_dir.join(CONFIG_FILE).display()
                ));
                return None;
            }
        };

        let clock: Clock = Arc::new(now);
        let palette = Palette::for_dark_mode(prefers_dark_mode());
        let mut ui = Self {
            panel: Panel::open(&base_dir, config, clock),
            widgets: Vec::new(),
            controls: BTreeMap::new(),
            texts: BTreeMap::new(),
            structure: Vec::new(),
            schemes: BTreeMap::new(),
            fonts: BTreeMap::new(),
            icons: BTreeMap::new(),
            panel_brush: CreateSolidBrush(palette.panel.as_u32()),
            log_brush: CreateSolidBrush(palette.log.as_u32()),
            dpi: view_dpi(std::ptr::null_mut()),
            palette,
            tray: false,
            hidden,
            quitting: false,
        };

        // What the panel shows before anything has been pressed: whether this
        // process is elevated, and whether a login launch is set up.
        ui.panel.set_elevated(pathenv::is_elevated());
        ui.panel.set_auto_start(ops::auto_start_enabled());
        Some(ui)
    }

    /// A logical length in this window's pixels.
    fn scale(&self, logical: i32) -> i32 {
        (logical * self.dpi as i32) / crate::view::DPI_DEFAULT as i32
    }

    /// A logical rectangle in this window's pixels.
    fn rect(&self, rect: Rect) -> RECT {
        let left = self.scale(rect.0);
        let top = self.scale(rect.1);
        RECT {
            left,
            top,
            right: left + self.scale(rect.2),
            bottom: top + self.scale(rect.3),
        }
    }

    /// A card's tile, in this window's pixels: the card's frame moved into place.
    fn tile(&self, frame: Rect, inner: Rect) -> RECT {
        let mut rect = self.rect(inner);
        let (dx, dy) = (self.scale(frame.0), self.scale(frame.1));
        rect.left += dx;
        rect.right += dx;
        rect.top += dy;
        rect.bottom += dy;
        rect
    }

    /// The card under a point in the window's own pixels.
    ///
    /// The cards are painted, so their rectangles live in the model and nowhere
    /// else: this is the same hit test the original ran over the card
    /// rectangles it had just drawn, for the same reason - a right-click
    /// anywhere on a card opens that card's menu.
    fn card_at(&self, x: i32, y: i32) -> Option<usize> {
        self.widgets.iter().find_map(|widget| match &widget.kind {
            WidgetKind::Card { index, .. } => {
                let rect = self.rect(widget.rect);
                (x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom)
                    .then_some(*index)
            }
            _ => None,
        })
    }

    /// One of the panel's fonts, already created at this scale.
    fn font(&self, font: Font) -> HFONT {
        self.fonts
            .get(&font_key(font))
            .copied()
            .unwrap_or(std::ptr::null_mut())
    }

    /// Creates the fonts for this scale, replacing any earlier set.
    unsafe fn prepare_fonts(&mut self) {
        delete_fonts(self);
        for font in [Font::UI, Font::CARD_NAME, Font::CARD_STATUS, Font::MONO] {
            let handle = create_font(font, self.dpi);
            self.fonts.insert(font_key(font), handle);
        }
    }

    /// The installation's own icon file: the window's icon and the tray's.
    unsafe fn installation_icon(&mut self) -> HICON {
        let path = match tray::icon_path(self.panel.base_dir()) {
            Ok(path) => path,
            Err(error) => {
                self.panel.log(&error.to_string());
                return std::ptr::null_mut();
            }
        };
        self.icon(&path)
    }

    /// An icon from disk, cached: the tray, the window and the cards that share
    /// a file share the handle.
    unsafe fn icon(&mut self, path: &Path) -> HICON {
        let key = path.display().to_string();
        if let Some(icon) = self.icons.get(&key) {
            return icon.unwrap_or(std::ptr::null_mut());
        }
        let buffer = wide(&key);
        let handle = LoadImageW(
            std::ptr::null_mut(),
            buffer.as_ptr(),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        ) as HICON;
        let icon = (!handle.is_null()).then_some(handle);
        self.icons.insert(key, icon);
        icon.unwrap_or(std::ptr::null_mut())
    }

    /// Loads the icons the cards on screen need, by service name.
    ///
    /// Done while the page is built because loading is a change to the cache;
    /// painting then only reads it.
    unsafe fn load_card_icons(&mut self) {
        let names: Vec<String> = self
            .panel
            .stack()
            .services()
            .iter()
            .map(|service| service.name().to_owned())
            .collect();
        for name in names {
            if self.icons.contains_key(&name) {
                continue;
            }
            let icon = view::service_icon(self.panel.base_dir(), &name)
                .map(|path| self.icon(&path))
                .filter(|handle| !handle.is_null());
            self.icons.insert(name, icon);
        }
    }

    /// The icon of the service at this index of the stack, when it has one.
    fn card_icon(&self, index: usize) -> Option<HICON> {
        let name = self.panel.stack().services().get(index)?.name();
        self.icons.get(name).copied().flatten()
    }
}

/// The height, weight and pitch that identify a font.
///
/// A `Font` is not `Ord` - its weight is an enum and its size a number - so the
/// key is built from the parts, which is also what the font is created from.
fn font_key(font: Font) -> (i32, u8, bool) {
    let weight = match font.weight {
        FontWeight::Regular => FW_NORMAL,
        FontWeight::Semibold => FW_SEMIBOLD,
    };
    (font.size, u8::try_from(weight).unwrap_or(0), font.mono)
}

/// Creates one font at a scale.
unsafe fn create_font(font: Font, dpi: u32) -> HFONT {
    let (_, weight, mono) = font_key(font);
    let face = wide(if mono { "Consolas" } else { "Segoe UI" });
    CreateFontW(
        -((font.size * dpi as i32) / crate::view::DPI_DEFAULT as i32),
        0,
        0,
        0,
        weight as i32,
        0,
        0,
        0,
        u32::from(DEFAULT_CHARSET),
        0,
        0,
        u32::from(CLEARTYPE_QUALITY),
        u32::from(FF_DONTCARE),
        face.as_ptr(),
    )
}

/// Registers the window class, creates the window and runs the message loop.
///
/// Returns the process exit code.
pub fn run(hidden: bool) -> i32 {
    unsafe {
        // The panel comes first: the window class carries the installation's own
        // icon, and a window whose class was registered without one never gets
        // it back.
        let Some(mut ui) = Ui::open(hidden) else {
            return 1;
        };
        let icon = ui.installation_icon();
        let class_name = wide(CLASS_NAME);
        let title = wide(view::window_title());

        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: icon,
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&class);

        let (width, height) = window_size(&ui);
        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        if window.is_null() {
            return 1;
        }

        STATE.with(|cell| *cell.borrow_mut() = Some(ui));

        if !hidden {
            ShowWindow(window, SW_SHOW);
        }

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    0
}

/// The window's size: the layout's logical size, scaled for this display, plus
/// the frame Win32 will draw around it.
unsafe fn window_size(ui: &Ui) -> (i32, i32) {
    let services = ui.panel.stack().services().len();
    let (logical_w, logical_h) = view::window_size(services);
    (ui.scale(logical_w) + FRAME_W, ui.scale(logical_h) + FRAME_H)
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
            with(|ui| {
                // The frame, first: the original set its corner preference and
                // its dark title bar here, and the panel's own palette is what
                // decides whether the system wants the dark one.
                set_window_attributes(window, ui.palette == Palette::DARK);

                // The original killed orphaned children before showing
                // anything: a service whose parent is gone would otherwise hold
                // its port, and the panel would be told the port is taken with
                // no way to find out by whom.
                for line in ui.panel.stack().sweep() {
                    ui.panel.log(&line);
                }
                // The banner, Apache's self-heal and `settings.auto_start`,
                // which is what the original's own creation pass did.
                ui.panel.startup();
                install_tray(window, ui);
                build(window, ui);
                state_callbacks(window, ui);
                SetTimer(window, TIMER_REFRESH, REFRESH_MS, None);
            });
            0
        }
        WM_TIMER => {
            with(|ui| {
                let moved = ui.panel.poll();
                let rebuilt = sync(window, ui);
                if moved || rebuilt {
                    InvalidateRect(window, std::ptr::null(), 0);
                }
            });
            0
        }
        // A service changed state, on a thread that is not this one: look at
        // the engine now rather than waiting for the next tick.
        _ if message == WM_WAKE => {
            with(|ui| {
                if ui.panel.poll() {
                    InvalidateRect(window, std::ptr::null(), 0);
                }
            });
            0
        }
        WM_SIZE => {
            with(|ui| {
                let dpi = view_dpi(window);
                if dpi != ui.dpi {
                    ui.dpi = dpi;
                    build(window, ui);
                }
                InvalidateRect(window, std::ptr::null(), 0);
            });
            0
        }
        WM_GETMINMAXINFO => {
            with(|ui| {
                let (width, height) = window_size(ui);
                let info = &mut *(lparam as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: width,
                    y: height,
                };
            });
            0
        }
        WM_PAINT => {
            with(|ui| paint(window, ui));
            0
        }
        WM_DRAWITEM => with_value(|ui| {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            if item.CtlType == ODT_BUTTON {
                draw_button(ui, item);
                return 1;
            }
            0
        })
        .unwrap_or(0),
        // A static, an edit and a read-only edit all arrive here, and all of
        // them want the panel's own colour rather than the platform's grey.
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => with_value(|ui| {
            let hdc = wparam as HDC;
            let control = lparam as HWND;
            let id = id_of(ui, control).and_then(WidgetId::from_value);
            let (text, background, brush) = if id == Some(WidgetId::Log) {
                (ui.palette.log_text, ui.palette.log, ui.log_brush)
            } else if matches!(id, Some(WidgetId::Status(_)) | Some(WidgetId::EditorPath)) {
                (ui.palette.muted, ui.palette.panel, ui.panel_brush)
            } else {
                (ui.palette.text, ui.palette.panel, ui.panel_brush)
            };
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, text.as_u32());
            SetBkColor(hdc, background.as_u32());
            brush as LRESULT
        })
        .unwrap_or(0),
        WM_COMMAND => {
            with(|ui| {
                let id = (wparam as u32 & 0xffff) as u16;
                // The high word is the notification code; the low word is the control.
                let code = (wparam as u32) >> 16;

                // The tray's numbers are its own, and its menu never sends this
                // message - but the numbers come through the same channel, so
                // they are answered first and never mistaken for a control's.
                if let Some(command) = view::tray_command(id) {
                    tray_command(window, ui, command);
                    return;
                }

                let Some(widget) = WidgetId::from_value(id) else {
                    return;
                };
                match code {
                    BN_CLICKED => {
                        // The card's `Ver ▾` button does not act on its own: it
                        // opens the catalogue's version menu under itself, which
                        // is what the original's own handler did.
                        if let WidgetId::Card {
                            index,
                            part: CardPart::Version,
                        } = widget
                            && let Some(control) = ui.controls.get(&id).copied()
                            && let Some(rect) = window_rect(control)
                        {
                            card_version_menu(
                                window,
                                ui,
                                index,
                                POINT {
                                    x: rect.left,
                                    y: rect.bottom,
                                },
                            );
                            return;
                        }
                        route(window, ui, widget)
                    }
                    CBN_SELCHANGE => {
                        let control = ui
                            .controls
                            .get(&id)
                            .copied()
                            .unwrap_or(std::ptr::null_mut());
                        let index = SendMessageW(control, CB_GETCURSEL, 0, 0);
                        if index >= 0 {
                            let action = ui.panel.select_combo(widget, index as usize);
                            act(window, ui, action);
                        }
                    }
                    // A field the user is typing into: the model follows it.
                    EN_CHANGE => {
                        if let Some(control) = ui.controls.get(&id).copied() {
                            let text = window_text(control);
                            ui.panel.set_edit(widget, text);
                        }
                    }
                    _ => {}
                }
            });
            0
        }
        WM_CONTEXTMENU => {
            // A right-click on the services page shows the card's version menu,
            // which is the other way the original opened it. The point arrives in
            // screen coordinates - or as -1 when the keyboard asked for it, in
            // which case the cursor is where it is opened.
            with(|ui| {
                let mut point = POINT {
                    x: (lparam as u32 & 0xffff) as i16 as i32,
                    y: ((lparam as u32 >> 16) & 0xffff) as i16 as i32,
                };
                if point.x == -1 && point.y == -1 {
                    GetCursorPos(&mut point);
                }
                let mut client = point;
                ScreenToClient(window, &mut client);
                if let Some(index) = ui.card_at(client.x, client.y) {
                    card_version_menu(window, ui, index, point);
                }
            });
            0
        }
        WM_SYSCOMMAND => {
            // Minimising puts the panel in the tray, which is where the original
            // put it: this is a control panel, not something to leave on the
            // taskbar for hours.
            if (wparam & 0xfff0) as u32 == SC_MINIMIZE {
                with(|ui| {
                    ShowWindow(window, SW_HIDE);
                    ui.hidden = true;
                });
                return 0;
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_CLOSE => {
            // Closing hides it too. Services keep running: a control panel is not
            // a service supervisor, and closing it must not take a project down -
            // which is what `Quit` is for.
            with(|ui| {
                if ui.quitting {
                    DestroyWindow(window);
                } else {
                    ShowWindow(window, SW_HIDE);
                    ui.hidden = true;
                }
            });
            0
        }
        WM_DESTROY => {
            STATE.with(|cell| {
                let mut borrow = cell.borrow_mut();
                if let Some(ui) = borrow.as_mut() {
                    remove_tray(window, ui);
                    KillTimer(window, TIMER_REFRESH);
                }
                if let Some(ui) = borrow.take() {
                    dispose(ui);
                }
            });
            PostQuitMessage(0);
            0
        }
        // A click on a table row, or a double-click on the panel's own surface:
        // the tables are painted, so their rows are hit-tested here.
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            with(|ui| {
                let x = (lparam as u32 & 0xffff) as i16 as i32;
                let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
                select_row(window, ui, x, y);
            });
            0
        }
        _ if message == WM_TRAY => {
            match lparam as u32 {
                WM_RBUTTONUP_ => with(|ui| tray_menu(window, ui)),
                WM_LBUTTONUP_ => with(|ui| show_window(window, ui)),
                _ => {}
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Runs `body` with the window's state, doing nothing when it is already in use.
///
/// The reentrancy is not hypothetical: setting a control's text sends its
/// notification back here, and the borrow that set it is still held.
fn with(body: impl FnOnce(&mut Ui)) {
    STATE.with(|cell| {
        if let Ok(mut borrow) = cell.try_borrow_mut()
            && let Some(ui) = borrow.as_mut()
        {
            body(ui);
        }
    });
}

/// The same, for a handler that has to answer Win32 with a value.
fn with_value(body: impl FnOnce(&mut Ui) -> LRESULT) -> Option<LRESULT> {
    STATE.with(|cell| {
        let mut borrow = cell.try_borrow_mut().ok()?;
        let ui = borrow.as_mut()?;
        Some(body(ui))
    })
}

// ---------------------------------------------------------------------------
// Building the controls
// ---------------------------------------------------------------------------

/// What identifies the controls on screen: their numbers, their places and
/// whether they accept input.
///
/// Two lists with the same structure are reconciled by updating text; two with
/// different structures cannot be, and the controls are rebuilt instead. What the
/// controls *say* is deliberately not here - which is what makes a status line
/// cheap.
fn structure(widgets: &[Widget]) -> Vec<(u16, Rect, bool)> {
    let mut key = Vec::new();
    for widget in widgets {
        match &widget.kind {
            // A card is painted, so what matters about it is the controls
            // inside it: they are what the window created.
            WidgetKind::Card {
                index,
                view,
                layout,
            } => {
                for (id, rect, kind) in card_children(*index, view, layout) {
                    let enabled = matches!(kind, WidgetKind::Button { enabled: true, .. });
                    key.push((id.value(), rect, enabled));
                }
            }
            WidgetKind::Button { enabled, .. } => {
                key.push((widget.id.value(), widget.rect, *enabled));
            }
            _ => key.push((widget.id.value(), widget.rect, true)),
        }
    }
    key
}

/// Creates the controls the current page needs, in the model's own order.
unsafe fn build(window: HWND, ui: &mut Ui) {
    destroy_controls(ui);
    ui.prepare_fonts();
    ui.load_card_icons();

    let widgets = ui.panel.widgets();
    for widget in &widgets {
        match &widget.kind {
            // A card is painted; only its buttons are controls, because they are
            // what the user presses and Win32 gives them focus and the keyboard
            // for free.
            WidgetKind::Card {
                index,
                view,
                layout,
            } => {
                for (id, rect, kind) in card_children(*index, view, layout) {
                    if let WidgetKind::Button {
                        label,
                        scheme,
                        enabled,
                    } = kind
                    {
                        create_button(
                            window,
                            ui,
                            id,
                            ui.tile(widget.rect, rect),
                            &label,
                            scheme,
                            enabled,
                        );
                    }
                }
            }
            // The tables and the strip are painted where they lie.
            WidgetKind::List { .. } | WidgetKind::Progress { .. } => {}
            _ => create_control(window, ui, widget),
        }
    }

    ui.structure = structure(&widgets);
    ui.texts.clear();
    for widget in &widgets {
        sync_one(ui, widget, true);
    }
    ui.widgets = widgets;
}

/// Creates one control for a widget that is not painted.
unsafe fn create_control(window: HWND, ui: &mut Ui, widget: &Widget) {
    match &widget.kind {
        WidgetKind::Button {
            label,
            scheme,
            enabled,
        } => create_button(
            window,
            ui,
            widget.id,
            ui.rect(widget.rect),
            label,
            *scheme,
            *enabled,
        ),
        WidgetKind::Label(text) => {
            let rect = ui.rect(widget.rect);
            let control = child(
                window,
                "STATIC",
                text,
                SS_LEFT | SS_NOPREFIX,
                rect,
                widget.id,
            );
            let font = ui.font(view::widget_font(widget));
            SendMessageW(control, WM_SETFONT, font as usize, 1);
            ui.controls.insert(widget.id.value(), control);
        }
        WidgetKind::Edit(text) => {
            let rect = ui.rect(widget.rect);
            let control = child(
                window,
                "EDIT",
                text,
                (ES_LEFT | ES_AUTOHSCROLL) as u32,
                rect,
                widget.id,
            );
            let font = ui.font(view::widget_font(widget));
            SendMessageW(control, WM_SETFONT, font as usize, 1);
            // A configuration file is longer than the default limit, and a
            // silently truncated save is a broken installation.
            SendMessageW(control, EM_SETLIMITTEXT, 0, 0);
            ui.controls.insert(widget.id.value(), control);
        }
        WidgetKind::Combo { items, selected } => {
            let rect = ui.rect(widget.rect);
            let control = CreateWindowExW(
                0,
                wide("COMBOBOX").as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                window,
                widget.id.value() as HMENU,
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            for item in items {
                let item = wide(item);
                SendMessageW(control, CB_ADDSTRING, 0, item.as_ptr() as isize);
            }
            SendMessageW(control, CB_SETCURSEL, *selected, 0);
            let font = ui.font(view::widget_font(widget));
            SendMessageW(control, WM_SETFONT, font as usize, 1);
            ui.controls.insert(widget.id.value(), control);
        }
        WidgetKind::Text(text) => {
            let rect = ui.rect(widget.rect);
            // The log is read-only: it is the engine's output, not a document.
            let read_only = widget.id == WidgetId::Log;
            // The edit-control styles are the one family windows-sys declares as
            // `i32` rather than `u32`, so the style word is assembled from them
            // and widened once.
            let edit = ES_LEFT
                | ES_MULTILINE
                | ES_AUTOVSCROLL
                | if read_only {
                    ES_READONLY
                } else {
                    ES_WANTRETURN
                };
            let style = edit as u32 | WS_VSCROLL | WS_TABSTOP;
            let control = child(
                window,
                "EDIT",
                if read_only { "" } else { text },
                style,
                rect,
                widget.id,
            );
            let font = ui.font(view::widget_font(widget));
            SendMessageW(control, WM_SETFONT, font as usize, 1);
            SendMessageW(control, EM_SETLIMITTEXT, 0, 0);
            ui.controls.insert(widget.id.value(), control);
        }
        // Painted, and never reached: `build` sends them past this function.
        WidgetKind::Card { .. } | WidgetKind::List { .. } | WidgetKind::Progress { .. } => {}
    }
}

/// Creates one owner-drawn button.
///
/// Owner-drawn because a button's colour is its *meaning* here - green to start,
/// red to stop, the primary colour to publish - and the platform's own buttons
/// have one colour. Everything else about them is a `BUTTON`: the focus ring, the
/// keyboard, the press.
unsafe fn create_button(
    window: HWND,
    ui: &mut Ui,
    id: WidgetId,
    rect: RECT,
    label: &str,
    scheme: Scheme,
    enabled: bool,
) {
    let control = CreateWindowExW(
        0,
        wide("BUTTON").as_ptr(),
        wide(label).as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        window,
        id.value() as HMENU,
        GetModuleHandleW(std::ptr::null()),
        std::ptr::null(),
    );
    let font = ui.font(Font::UI);
    SendMessageW(control, WM_SETFONT, font as usize, 1);
    if !enabled {
        EnableWindow(control, 0);
    }
    ui.controls.insert(id.value(), control);
    ui.schemes.insert(id.value(), scheme);
}

/// Destroys every control, so the next page starts from nothing.
unsafe fn destroy_controls(ui: &mut Ui) {
    for control in std::mem::take(&mut ui.controls).into_values() {
        if !control.is_null() {
            DestroyWindow(control);
        }
    }
    ui.schemes.clear();
    ui.texts.clear();
}

/// Updates what the existing controls say, and reports whether the page was
/// rebuilt.
unsafe fn sync(window: HWND, ui: &mut Ui) -> bool {
    let widgets = ui.panel.widgets();
    if structure(&widgets) != ui.structure {
        build(window, ui);
        return true;
    }

    for widget in &widgets {
        sync_one(ui, widget, false);
    }
    ui.widgets = widgets;
    false
}

/// Writes a widget's text and enablement to its controls, when they differ from
/// what is already there.
///
/// `fresh` is set while the controls are being created, when they already carry
/// the text and this only records it.
unsafe fn sync_one(ui: &mut Ui, widget: &Widget, fresh: bool) {
    match &widget.kind {
        WidgetKind::Card {
            index,
            view,
            layout,
        } => {
            for (id, _, kind) in card_children(*index, view, layout) {
                if let WidgetKind::Button {
                    label,
                    scheme,
                    enabled,
                } = kind
                {
                    ui.schemes.insert(id.value(), scheme);
                    set_control_text(ui, id, &label, fresh);
                    set_control_enabled(ui, id, enabled);
                }
            }
        }
        WidgetKind::Button {
            label,
            scheme,
            enabled,
        } => {
            // Matched through a reference, so the two copies are explicit.
            ui.schemes.insert(widget.id.value(), *scheme);
            set_control_text(ui, widget.id, label, fresh);
            set_control_enabled(ui, widget.id, *enabled);
        }
        WidgetKind::Label(text) => set_control_text(ui, widget.id, text, fresh),
        // A drop-down's items do not change while the page is up - the settings
        // it lists are read when the page is built - but its *selection* does,
        // when the picker is the thing that changed it.
        WidgetKind::Combo { selected, .. } => {
            if !fresh && let Some(control) = ui.controls.get(&widget.id.value()).copied() {
                SendMessageW(control, CB_SETCURSEL, *selected, 0);
            }
        }
        // A field the user is typing into is theirs: the model follows it, not
        // the other way round. The log is the engine's, so it is written to - but
        // only when it has grown, which is what keeps a quarter-megabyte buffer
        // from being copied into a control twice a second.
        WidgetKind::Text(text) => {
            if widget.id == WidgetId::Log && ui.texts.get(&widget.id.value()) != Some(text) {
                if let Some(control) = ui.controls.get(&widget.id.value()).copied() {
                    set_text(control, text);
                    scroll_to_end(control);
                }
                ui.texts.insert(widget.id.value(), text.clone());
            }
        }
        WidgetKind::Edit(text) => {
            if fresh {
                ui.texts.insert(widget.id.value(), text.clone());
            }
        }
        WidgetKind::List { .. } | WidgetKind::Progress { .. } => {}
    }
}

/// Sets a control's text, remembering what it says.
unsafe fn set_control_text(ui: &mut Ui, id: WidgetId, text: &str, fresh: bool) {
    if !fresh && ui.texts.get(&id.value()).is_some_and(|known| known == text) {
        return;
    }
    if let Some(control) = ui.controls.get(&id.value()).copied() {
        set_text(control, text);
    }
    ui.texts.insert(id.value(), text.to_owned());
}

/// Enables or disables a control, when that is not what it already is.
unsafe fn set_control_enabled(ui: &mut Ui, id: WidgetId, enabled: bool) {
    let Some(control) = ui.controls.get(&id.value()).copied() else {
        return;
    };
    if control.is_null() {
        return;
    }
    EnableWindow(control, if enabled { 1 } else { 0 });
}

/// The number a control reports events with.
unsafe fn id_of(ui: &Ui, control: HWND) -> Option<u16> {
    ui.controls
        .iter()
        .find(|(_, handle)| **handle == control)
        .map(|(id, _)| *id)
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Paints what is not a control: the background, the cards, the tables and the
/// strip.
unsafe fn paint(window: HWND, ui: &Ui) {
    let mut paint: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(window, &mut paint);
    let client = client_rect(window);

    // The window, then the content panel to the right of the sidebar. The
    // sidebar's own width comes from the page buttons themselves - the model
    // placed them, and a second copy of that number could disagree with it.
    fill(hdc, client, ui.palette.window);
    if let Some(strip) = sidebar_rect(ui, client) {
        fill(hdc, strip, ui.palette.sidebar);
        let content = RECT {
            left: strip.right,
            top: 0,
            right: client.right,
            bottom: client.bottom,
        };
        fill(hdc, content, ui.palette.panel);
    }

    for widget in &ui.widgets {
        match &widget.kind {
            WidgetKind::Card {
                index,
                view,
                layout,
            } => draw_card(hdc, ui, ui.rect(widget.rect), *index, view, layout),
            WidgetKind::List { columns, rows } => {
                draw_table(
                    hdc,
                    ui,
                    widget.rect,
                    columns,
                    rows,
                    selected_row(ui, widget.id),
                );
            }
            WidgetKind::Progress { position, .. } => draw_progress(hdc, ui, widget.rect, *position),
            _ => {}
        }
    }

    EndPaint(window, &paint);
}

/// The sidebar strip: as wide as the page buttons plus their own margin.
fn sidebar_rect(ui: &Ui, client: RECT) -> Option<RECT> {
    let button = ui
        .widgets
        .iter()
        .find(|widget| widget.id == WidgetId::Page(Page::Services))?;
    let rect = ui.rect(button.rect);
    Some(RECT {
        left: 0,
        top: 0,
        right: rect.right,
        bottom: client.bottom,
    })
}

/// Which row of a table the model says is selected.
fn selected_row(ui: &Ui, id: WidgetId) -> Option<String> {
    match id {
        WidgetId::ProjectList => ui.panel.project_selected().map(str::to_owned),
        WidgetId::VhostList => ui.panel.vhost_current().map(str::to_owned),
        _ => None,
    }
}

/// One service card: its frame, the accent bar, the icon, the dot and the two
/// lines of text.
unsafe fn draw_card(
    hdc: HDC,
    ui: &Ui,
    frame: RECT,
    index: usize,
    view: &CardView,
    layout: &CardLayout,
) {
    fill(hdc, frame, ui.palette.card);
    let border = CreateSolidBrush(ui.palette.card_border.as_u32());
    FrameRect(hdc, &frame, border);
    DeleteObject(border as HGDIOBJ);

    // The accent bar on the leading edge: the card's own state, readable at a
    // glance and without reading the status line.
    let bar = RECT {
        left: frame.left,
        top: frame.top,
        right: frame.left + ui.scale(3),
        bottom: frame.bottom,
    };
    let accent = CreateSolidBrush(ui.palette.card_accent(view).as_u32());
    FillRect(hdc, &bar, accent);
    DeleteObject(accent as HGDIOBJ);

    let (x, y) = (frame.left, frame.top);
    if let Some(icon) = ui.card_icon(index) {
        let tile = ui.rect(layout.icon);
        DrawIconEx(
            hdc,
            x + tile.left,
            y + tile.top,
            icon,
            tile.right - tile.left,
            tile.bottom - tile.top,
            0,
            std::ptr::null_mut(),
            DI_NORMAL,
        );
    }

    // The dot: the stack's own `●`, `○` or `·`, in the colour that means it.
    let mut dot = ui.rect(layout.dot);
    shift(&mut dot, x, y);
    draw_text(
        hdc,
        ui.font(Font::CARD_NAME),
        &view.dot.to_string(),
        &mut dot,
        ui.palette.dot(view.dot),
        DT_LEFT,
    );

    let mut name = ui.rect(layout.name);
    shift(&mut name, x, y);
    draw_text(
        hdc,
        ui.font(Font::CARD_NAME),
        &view.name,
        &mut name,
        ui.palette.text,
        DT_LEFT,
    );

    let mut status = ui.rect(layout.status);
    shift(&mut status, x, y);
    draw_text(
        hdc,
        ui.font(Font::CARD_STATUS),
        &view.status,
        &mut status,
        ui.palette.muted,
        DT_LEFT,
    );
}

/// A table: its header, its rows, and the row the model has selected.
unsafe fn draw_table(
    hdc: HDC,
    ui: &Ui,
    rect: Rect,
    columns: &[(&'static str, i32)],
    rows: &[Vec<String>],
    selected: Option<String>,
) {
    let frame = ui.rect(rect);
    fill(hdc, frame, ui.palette.card);

    let header_bottom = frame.top + ui.scale(HEADER_H);
    let mut x = frame.left;
    for (title, width) in columns {
        let mut cell = RECT {
            left: x + ui.scale(6),
            top: frame.top,
            right: x + ui.scale(*width) - ui.scale(4),
            bottom: header_bottom,
        };
        draw_text(
            hdc,
            ui.font(Font::CARD_STATUS),
            title,
            &mut cell,
            ui.palette.muted,
            DT_LEFT,
        );
        x += ui.scale(*width);
    }

    let rule = CreateSolidBrush(ui.palette.card_border.as_u32());
    let line = RECT {
        left: frame.left,
        top: header_bottom,
        right: frame.right,
        bottom: header_bottom + ui.scale(1),
    };
    FillRect(hdc, &line, rule);
    DeleteObject(rule as HGDIOBJ);

    for (index, row) in rows.iter().enumerate() {
        let top = header_bottom + ui.scale(1) + ui.scale(ROW_H) * index as i32;
        let row_rect = RECT {
            left: frame.left,
            top,
            right: frame.right,
            bottom: top + ui.scale(ROW_H),
        };
        if row_rect.bottom > frame.bottom {
            break;
        }

        // Both tables name the selected row by one of its own cells - a project
        // by its name, a virtual host by its domain - and those sit in different
        // columns, so the row is compared rather than a column assumed.
        let is_selected = selected
            .as_deref()
            .is_some_and(|name| !name.is_empty() && row.iter().any(|cell| cell == name));
        if is_selected {
            fill(hdc, row_rect, ui.palette.primary);
        } else if index % 2 == 1 {
            fill(hdc, row_rect, ui.palette.window);
        }

        let colour = if is_selected {
            ui.palette.on_accent()
        } else {
            ui.palette.text
        };
        x = frame.left;
        for (column, (_, width)) in columns.iter().enumerate() {
            let text = row.get(column).map(String::as_str).unwrap_or_default();
            let mut cell = RECT {
                left: x + ui.scale(6),
                top,
                right: x + ui.scale(*width) - ui.scale(4),
                bottom: row_rect.bottom,
            };
            draw_text(
                hdc,
                ui.font(Font::CARD_STATUS),
                text,
                &mut cell,
                colour,
                DT_LEFT,
            );
            x += ui.scale(*width);
        }
    }
}

/// The progress strip: a track, and the part of it the engine has finished.
unsafe fn draw_progress(hdc: HDC, ui: &Ui, rect: Rect, position: u16) {
    let frame = ui.rect(rect);
    fill(hdc, frame, ui.palette.window);
    let border = CreateSolidBrush(ui.palette.card_border.as_u32());
    FrameRect(hdc, &frame, border);
    DeleteObject(border as HGDIOBJ);

    let width = frame.right - frame.left - ui.scale(2);
    let done = width * i32::from(position.min(lambo_core::ui_state::PROGRESS_MAX))
        / i32::from(lambo_core::ui_state::PROGRESS_MAX);
    if done <= 0 {
        return;
    }
    let filled = RECT {
        left: frame.left + ui.scale(1),
        top: frame.top + ui.scale(1),
        right: frame.left + ui.scale(1) + done,
        bottom: frame.bottom - ui.scale(1),
    };
    let colour = CreateSolidBrush(ui.palette.progress(position).as_u32());
    FillRect(hdc, &filled, colour);
    DeleteObject(colour as HGDIOBJ);
}

/// One owner-drawn button: its scheme's colour, its label, and its state.
unsafe fn draw_button(ui: &Ui, item: &DRAWITEMSTRUCT) {
    let scheme = ui
        .schemes
        .get(&(item.CtlID as u16))
        .copied()
        .unwrap_or(Scheme::Neutral);
    let colours = ui.palette.scheme(scheme);
    let disabled = item.itemState & ODS_DISABLED != 0;
    let pressed = item.itemState & ODS_SELECTED != 0;

    let background = if disabled {
        ui.palette.window
    } else if pressed {
        colours.hover
    } else {
        colours.background
    };
    fill(item.hDC, item.rcItem, background);

    let colour = if disabled {
        ui.palette.muted
    } else {
        colours.text
    };
    let label = window_text(item.hwndItem);
    let mut rect = item.rcItem;
    draw_text(
        item.hDC,
        ui.font(Font::UI),
        &label,
        &mut rect,
        colour,
        DT_CENTER,
    );
}

// ---------------------------------------------------------------------------
// Rows, and the tray
// ---------------------------------------------------------------------------

/// Selects the table row under a click, when there is one.
unsafe fn select_row(window: HWND, ui: &mut Ui, x: i32, y: i32) {
    let hit = ui.widgets.iter().find_map(|widget| {
        let WidgetKind::List { .. } = widget.kind else {
            return None;
        };
        let frame = ui.rect(widget.rect);
        if x < frame.left || x > frame.right || y < frame.top || y > frame.bottom {
            return None;
        }
        Some((widget.id, frame))
    });

    let Some((id, frame)) = hit else {
        return;
    };
    let top = frame.top + ui.scale(HEADER_H) + ui.scale(1);
    if y < top {
        return;
    }
    let row = ((y - top) / ui.scale(ROW_H)) as usize;
    match id {
        WidgetId::ProjectList => ui.panel.select_project(Some(row)),
        WidgetId::VhostList => ui.panel.edit_vhost(Some(row)),
        _ => return,
    }
    InvalidateRect(window, std::ptr::null(), 0);
}

/// Routes a control's number and performs what it means.
unsafe fn route(window: HWND, ui: &mut Ui, widget: WidgetId) {
    let action = ui.panel.route(widget);
    act(window, ui, action);
}

/// Performs an action, and follows up on what the window still has to do.
unsafe fn act(window: HWND, ui: &mut Ui, action: Option<Action>) {
    let Some(action) = action else {
        return;
    };
    if ops::perform(&mut ui.panel, action) == FollowUp::Quit {
        ui.quitting = true;
        DestroyWindow(window);
        return;
    }
    sync(window, ui);
    InvalidateRect(window, std::ptr::null(), 0);
}

/// Shows the window out of the tray, on top of whatever else is open.
unsafe fn show_window(window: HWND, ui: &mut Ui) {
    ShowWindow(window, SW_RESTORE);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    ui.hidden = false;
    InvalidateRect(window, std::ptr::null(), 0);
}

/// Installs the notification icon.
unsafe fn install_tray(window: HWND, ui: &mut Ui) {
    if ui.tray {
        return;
    }
    let icon = ui.installation_icon();
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = TRAY_UID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon;
    let tip = wide(view::window_title());
    for (slot, unit) in data.szTip.iter_mut().zip(tip.iter()) {
        *slot = *unit;
    }

    if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
        ui.tray = true;
    } else {
        ui.panel
            .log("tray: the notification icon could not be added");
    }
}

/// Asks the engine to wake the window when a service's state changes.
///
/// The original set one callback per service and had it post a refresh onto the
/// UI thread; this is the same thing through the same door, a `PostMessage`
/// from whichever thread noticed. The handle travels as an integer because the
/// callback has to be `Send + Sync`, which a raw pointer is not.
unsafe fn state_callbacks(window: HWND, ui: &mut Ui) {
    let target = window as usize;
    ui.panel
        .stack()
        .set_state_callback(Arc::new(move |_running: bool, _pid: u32| {
            PostMessageW(target as HWND, WM_WAKE, 0, 0);
        }));
}

/// Removes the notification icon, so it does not linger after the window does.
unsafe fn remove_tray(window: HWND, ui: &mut Ui) {
    if !ui.tray {
        return;
    }
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = TRAY_UID;
    Shell_NotifyIconW(NIM_DELETE, &data);
    ui.tray = false;
}

/// A card's version menu: the builds the catalogue offers, the active one
/// checked.
///
/// The original opened it two ways - from the card's `Ver ▾` button, under the
/// button's bottom-left corner, and from a right-click on the card, at the
/// cursor - and [the window](window_proc) does both, with the point as the only
/// difference. What the lines are, and which of them carries the check mark, is
/// [`lambo_core::ui_state::version_menu`]'s, so the menu can only ever offer what
/// the installer can actually install.
unsafe fn card_version_menu(window: HWND, ui: &mut Ui, index: usize, point: POINT) {
    let (items, title) = {
        let Some(service) = ui.panel.stack().services().get(index) else {
            return;
        };
        (
            version_menu(service),
            wide(&version_menu_title(service.name())),
        )
    };
    if items.is_empty() {
        return;
    }

    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    // The disabled first line names the card the menu belongs to, as the
    // original's did; the separator under it is what makes it a title.
    AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, title.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    for (variant, item) in items.iter().enumerate() {
        let flags = MF_STRING | if item.checked { MF_CHECKED } else { 0 };
        let label = wide(&item.label);
        // The identifier a chosen line reports is the model's own - the same
        // number the routing table turns back into "switch this service to this
        // build" - so nothing here decides what a choice means.
        let id = WidgetId::Version { index, variant }.value() as usize;
        AppendMenuW(menu, flags, id, label.as_ptr());
    }

    // A menu opened over a panel needs its owner in the foreground, or it stays
    // up after the user clicks elsewhere.
    SetForegroundWindow(window);
    let chosen = TrackPopupMenu(
        menu,
        TPM_LEFTBUTTON | TPM_RIGHTBUTTON | TPM_RETURNCMD,
        point.x,
        point.y,
        0,
        window,
        std::ptr::null(),
    );
    DestroyMenu(menu);

    if chosen > 0
        && let Some(widget) = WidgetId::from_value(chosen as u16)
    {
        route(window, ui, widget);
    }
}

/// The tray's context menu: the original's own lines, in its order.
unsafe fn tray_menu(window: HWND, ui: &mut Ui) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }

    for item in tray::menu(ui.panel.auto_start()) {
        match item {
            TrayItem::Separator => {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            }
            TrayItem::Command { command, checked } => {
                let flags = MF_STRING | if checked { MF_CHECKED } else { 0 };
                let label = wide(command.label());
                AppendMenuW(menu, flags, command.id() as usize, label.as_ptr());
            }
        }
    }

    let mut point = POINT { x: 0, y: 0 };
    GetCursorPos(&mut point);
    // A menu opened from a tray icon needs its owner in the foreground, or it
    // stays up after the user clicks elsewhere: that is why the window is
    // brought forward before the menu is shown, not after.
    SetForegroundWindow(window);
    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        window,
        std::ptr::null(),
    );
    DestroyMenu(menu);

    if chosen > 0
        && let Some(command) = view::tray_command(chosen as u16)
    {
        tray_command(window, ui, command);
    }
}

/// What one of the tray's lines does.
///
/// Which action a line means is the panel's ([`Panel::tray_action`]); showing
/// the window is the only one that is not an engine operation, and it stays
/// here because bringing a window forward is Win32's.
unsafe fn tray_command(window: HWND, ui: &mut Ui, command: TrayCommand) {
    if command == TrayCommand::Show {
        show_window(window, ui);
        return;
    }
    let action = ui.panel.tray_action(command);
    act(window, ui, Some(action));
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Releases the GDI objects the window owns.
unsafe fn dispose(ui: Ui) {
    for font in ui.fonts.into_values() {
        DeleteObject(font as HGDIOBJ);
    }
    DeleteObject(ui.panel_brush as HGDIOBJ);
    DeleteObject(ui.log_brush as HGDIOBJ);
    for icon in ui.icons.into_values().flatten() {
        DestroyIcon(icon);
    }
}

/// Deletes the fonts, so they can be created again at another scale.
unsafe fn delete_fonts(ui: &mut Ui) {
    for font in std::mem::take(&mut ui.fonts).into_values() {
        DeleteObject(font as HGDIOBJ);
    }
}

/// The scale the panel is drawn at.
///
/// The display's own DPI is the floor, and the client area's width raises it: the
/// layout is a fixed arrangement of logical pixels, so a window the user made
/// larger shows the same panel bigger rather than a panel with a gap under it -
/// which is what a control panel should do, and what the original's own `Dpi`
/// scaling did.
unsafe fn view_dpi(window: HWND) -> u32 {
    let native = if window.is_null() {
        GetDpiForSystem()
    } else {
        GetDpiForWindow(window)
    };
    let native = view::clamp_dpi(native);
    if window.is_null() {
        return native;
    }

    let client = client_rect(window);
    let (logical_w, _) = view::window_size(0);
    if logical_w <= 0 || client.right <= 0 {
        return native;
    }
    let by_width = (view::DPI_DEFAULT as i32 * client.right) / logical_w;
    view::clamp_dpi(u32::try_from(by_width.max(native as i32)).unwrap_or(native))
}

/// Whether Windows is using its dark theme for applications.
///
/// A missing value means light, which is what every version with the key
/// defaults to; failing to read it is not worth reporting, because the palette it
/// selects is a preference rather than a setting.
fn prefers_dark_mode() -> bool {
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut light: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            std::ptr::from_mut(&mut light).cast(),
            &mut size,
        )
    };
    status == 0 && light == 0
}

/// The log's clock: the local time, as `HH:MM:SS`.
///
/// `std` has no local time, so the platform's is read; the panel formats what it
/// is handed, which is why the format is not repeated here.
fn now() -> String {
    let mut time: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };
    lambo_core::ui_state::time_of_day(time.wHour, time.wMinute, time.wSecond)
}

/// A wide, NUL-terminated string that outlives the call it is passed to.
///
/// Win32 reads to the terminator, so the terminator is not optional, and the
/// buffer must stay alive for the duration of the call - which it does, being
/// owned by the caller's local.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A control's text, as it is now.
unsafe fn window_text(control: HWND) -> String {
    let length = GetWindowTextLengthW(control);
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let written = GetWindowTextW(control, buffer.as_mut_ptr(), length + 1);
    String::from_utf16_lossy(&buffer[..written.max(0) as usize])
}

/// Sets a control's text.
unsafe fn set_text(control: HWND, text: &str) {
    let buffer = wide(text);
    SetWindowTextW(control, buffer.as_ptr());
}

/// The system's folder picker, and the folder it picked.
///
/// The projects page's `Browse…` asks the user for the folder a project is
/// loaded from. Cancelling is a normal answer: `None` leaves the location
/// field as it is.
pub fn pick_folder() -> Option<PathBuf> {
    let title = wide("Choose your project's folder");
    let mut display = [0u16; 260];
    let info = BROWSEINFOW {
        hwndOwner: std::ptr::null_mut(),
        pidlRoot: std::ptr::null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS,
        lpfn: None,
        lParam: 0,
        iImage: 0,
    };
    // SAFETY: `info` and both buffers outlive the call, the title is
    // NUL-terminated, and the item-id list the picker returns is freed with
    // the allocator it came from before this function ends.
    unsafe {
        let pidl = SHBrowseForFolderW(&info);
        if pidl.is_null() {
            return None;
        }
        let mut buffer = [0u16; 4096];
        let written = SHGetPathFromIDListW(pidl, buffer.as_mut_ptr());
        CoTaskMemFree(pidl as *const _);
        if written == 0 {
            return None;
        }
        let end = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..end])))
    }
}

/// Puts a read-only edit control at its end, so the newest line is the one on
/// screen.
unsafe fn scroll_to_end(control: HWND) {
    SendMessageW(control, WM_VSCROLL, SB_BOTTOM as usize, 0);
}

/// Creates a child control of one of the standard classes.
unsafe fn child(
    window: HWND,
    class: &str,
    text: &str,
    style: u32,
    rect: RECT,
    id: WidgetId,
) -> HWND {
    CreateWindowExW(
        0,
        wide(class).as_ptr(),
        wide(text).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        window,
        id.value() as HMENU,
        GetModuleHandleW(std::ptr::null()),
        std::ptr::null(),
    )
}

/// The window's client area.
unsafe fn client_rect(window: HWND) -> RECT {
    let mut rect: RECT = std::mem::zeroed();
    GetClientRect(window, &mut rect);
    rect
}

/// A control's rectangle on the screen, or `None` when Win32 will not say.
unsafe fn window_rect(control: HWND) -> Option<RECT> {
    let mut rect: RECT = std::mem::zeroed();
    (GetWindowRect(control, &mut rect) != 0).then_some(rect)
}

/// Rounds the window's corners and asks for a dark frame when the system prefers
/// one.
///
/// Both are the original's, set in the same message: rounded corners always, and
/// the immersive dark title bar only when the user's `AppsUseLightTheme` says so.
/// Both are best-effort - a Windows build that does not know an attribute answers
/// an error, and the window is perfectly usable without either of them.
unsafe fn set_window_attributes(window: HWND, prefer_dark: bool) {
    let corner = DWMWCP_ROUND;
    DwmSetWindowAttribute(
        window,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        std::ptr::from_ref(&corner).cast(),
        std::mem::size_of_val(&corner) as u32,
    );
    if prefer_dark {
        let dark = 1i32;
        DwmSetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            std::ptr::from_ref(&dark).cast(),
            std::mem::size_of_val(&dark) as u32,
        );
    }
}

/// Fills a rectangle with a colour.
unsafe fn fill(hdc: HDC, rect: RECT, colour: Rgb) {
    let brush = CreateSolidBrush(colour.as_u32());
    FillRect(hdc, &rect, brush);
    DeleteObject(brush as HGDIOBJ);
}

/// Moves a rectangle by an offset.
fn shift(rect: &mut RECT, x: i32, y: i32) {
    rect.left += x;
    rect.right += x;
    rect.top += y;
    rect.bottom += y;
}

/// Draws one line of text inside a rectangle.
unsafe fn draw_text(
    hdc: HDC,
    font: HFONT,
    text: &str,
    rect: &mut RECT,
    colour: Rgb,
    alignment: u32,
) {
    let previous = SelectObject(hdc, font as HGDIOBJ);
    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, colour.as_u32());
    let buffer = wide(text);
    DrawTextW(
        hdc,
        buffer.as_ptr(),
        buffer.len() as i32 - 1,
        rect,
        alignment | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
    SelectObject(hdc, previous);
}

/// Reports something the panel cannot start without.
fn fail(message: &str) {
    eprintln!("Lambo PHP: {message}");
}
