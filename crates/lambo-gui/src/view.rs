//! How the panel looks: colours, fonts, scaling, and the conversions Win32
//! needs before it can draw anything.
//!
//! `ui_state` decides *what* is on screen - the geometry, the text, which control
//! is enabled - and `win32.rs` creates the controls and draws them. Between the
//! two sits this file: which colour a [`Scheme`] is painted in, what a status dot
//! means, which font a control is created with, how a logical rectangle becomes a
//! physical one, and how a widget's identifier becomes the number a `WM_COMMAND`
//! carries.
//!
//! None of that is Win32, so none of it is in `win32.rs`: this file compiles on
//! every platform and the local harness runs its tests, which is what keeps the
//! unverifiable half of the interface to drawing and event wiring.
//!
//! # The palette
//!
//! The original panel's colours are kept as the light theme - a soft grey
//! window, white
//! panels, a dark navy sidebar, and one blue for the primary action - because
//! they are the product's own look and a rewrite is not a licence to change how
//! it reads. What changed is that they are now one [`Palette`] value rather than
//! globals, so the dark theme beside them is the same structure and the window
//! can follow the system's `AppsUseLightTheme` without a second code path.

use std::path::{Path, PathBuf};

use lambo_core::tray::TrayCommand;
use lambo_core::ui_state::{
    CardPart, Layout, PROGRESS_MAX, Rect, Scheme, Theme, WINDOW_TITLE, WINDOW_W, Widget, WidgetId,
    WidgetKind, icon_file, icon_path, theme,
};

/// A colour, held the way Win32 wants it: `0x00BBGGRR`.
///
/// Storing it in that order rather than as three fields means the value handed to
/// `CreateSolidBrush`, `SetTextColor` and a `WM_CTLCOLOR` handler is the value
/// kept here - no conversion on the way to the call, which is exactly where a
/// red/blue swap would hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u32);

impl Rgb {
    /// A colour from its three channels.
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self((red as u32) | ((green as u32) << 8) | ((blue as u32) << 16))
    }

    /// The colour's blue, green and red channels, in that order.
    pub const fn channels(self) -> (u8, u8, u8) {
        (
            (self.0 & 0xff) as u8,
            ((self.0 >> 8) & 0xff) as u8,
            ((self.0 >> 16) & 0xff) as u8,
        )
    }

    /// The value to pass to Win32.
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// This colour moved `percent` of the way towards `other`.
    ///
    /// Used for the hover shade of every scheme, so a scheme is one colour and a
    /// shade rather than two colours that have to be kept in step by hand.
    pub const fn blend(self, other: Self, percent: u8) -> Self {
        let (red, green, blue) = self.channels();
        let (other_red, other_green, other_blue) = other.channels();
        let percent = if percent > 100 { 100 } else { percent } as u32;
        Self::new(
            mix_channel(red, other_red, percent),
            mix_channel(green, other_green, percent),
            mix_channel(blue, other_blue, percent),
        )
    }
}

/// One channel moved `percent` of the way from `from` to `to`.
const fn mix_channel(from: u8, to: u8, percent: u32) -> u8 {
    ((from as u32 * (100 - percent) + to as u32 * percent + 50) / 100) as u8
}

/// The three colours one control is painted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorScheme {
    /// Its fill.
    pub background: Rgb,
    /// Its fill while the user is on it.
    pub hover: Rgb,
    /// Its text.
    pub text: Rgb,
}

/// Every colour the panel paints with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The window behind everything.
    pub window: Rgb,
    /// The content panel's fill.
    pub panel: Rgb,
    /// A service card's fill.
    pub card: Rgb,
    /// A service card's outline.
    pub card_border: Rgb,
    /// The sidebar's fill.
    pub sidebar: Rgb,
    /// The sidebar entry the current page is on.
    pub sidebar_active: Rgb,
    /// Normal text.
    pub text: Rgb,
    /// Secondary text: status lines, captions, the status bar.
    pub muted: Rgb,
    /// The primary action.
    pub primary: Rgb,
    /// Something up and running.
    pub success: Rgb,
    /// Stopping things, deleting things.
    pub danger: Rgb,
    /// Starting things again, and states that need a look.
    pub warning: Rgb,
    /// Neither one nor the other.
    pub neutral: Rgb,
    /// The log panel's fill.
    pub log: Rgb,
    /// The log panel's text.
    pub log_text: Rgb,
}

impl Palette {
    /// The light theme: the original's own colours.
    pub const LIGHT: Self = Self {
        window: Rgb::new(0xf2, 0xf4, 0xf8),
        panel: Rgb::new(0xff, 0xff, 0xff),
        card: Rgb::new(0xff, 0xff, 0xff),
        card_border: Rgb::new(0xd7, 0xdd, 0xe8),
        sidebar: Rgb::new(0x1e, 0x24, 0x36),
        sidebar_active: Rgb::new(0x3a, 0x7a, 0xef),
        text: Rgb::new(0x1a, 0x1d, 0x29),
        muted: Rgb::new(0x6a, 0x76, 0x8a),
        primary: Rgb::new(0x3a, 0x7a, 0xef),
        success: Rgb::new(0x22, 0xa3, 0x4f),
        danger: Rgb::new(0xdc, 0x3a, 0x3a),
        warning: Rgb::new(0xed, 0x9d, 0x1d),
        neutral: Rgb::new(0x6a, 0x76, 0x8a),
        log: Rgb::new(0xf7, 0xf9, 0xfc),
        log_text: Rgb::new(0x33, 0x3b, 0x4d),
    };

    /// The dark theme, for a system that asked for dark mode.
    ///
    /// The same structure and the same meanings; the accents are lifted a step so
    /// they still read as *accents* against a dark fill instead of sinking into
    /// it.
    pub const DARK: Self = Self {
        window: Rgb::new(0x14, 0x18, 0x1f),
        panel: Rgb::new(0x1b, 0x21, 0x29),
        card: Rgb::new(0x1f, 0x27, 0x31),
        card_border: Rgb::new(0x2a, 0x34, 0x41),
        sidebar: Rgb::new(0x10, 0x14, 0x1b),
        sidebar_active: Rgb::new(0x3a, 0x7a, 0xef),
        text: Rgb::new(0xe6, 0xeb, 0xf3),
        muted: Rgb::new(0x8b, 0x98, 0xab),
        primary: Rgb::new(0x4c, 0x8b, 0xf5),
        success: Rgb::new(0x2f, 0xbf, 0x6b),
        danger: Rgb::new(0xe0, 0x57, 0x4f),
        warning: Rgb::new(0xf0, 0xad, 0x3a),
        neutral: Rgb::new(0x6a, 0x76, 0x8a),
        log: Rgb::new(0x10, 0x14, 0x1b),
        log_text: Rgb::new(0xcd, 0xd6, 0xe3),
    };

    /// The palette for a theme.
    pub const fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Light => Self::LIGHT,
            Theme::Dark => Self::DARK,
        }
    }

    /// The palette for what Windows reports about its apps' theme.
    pub fn for_dark_mode(prefers_dark: bool) -> Self {
        Self::for_theme(theme(prefers_dark))
    }

    /// The colours of one scheme, with its hover shade.
    ///
    /// The schemes are the original's, one per meaning, so a control's role is
    /// what picks its colour: a Start button is green because starting is green,
    /// not because of where it is on the page.
    pub const fn scheme(&self, scheme: Scheme) -> ColorScheme {
        let (background, text) = match scheme {
            Scheme::Primary => (self.primary, self.on_accent()),
            Scheme::Success => (self.success, self.on_accent()),
            Scheme::Danger => (self.danger, self.on_accent()),
            Scheme::Warning => (self.warning, self.text),
            Scheme::Neutral => (self.neutral, self.on_accent()),
            Scheme::Sidebar => (self.sidebar, self.on_accent()),
        };
        ColorScheme {
            background,
            hover: background.blend(text, 18),
            text,
        }
    }

    /// The colour that reads on a filled accent.
    pub const fn on_accent(&self) -> Rgb {
        Rgb::new(0xff, 0xff, 0xff)
    }

    /// What a card's status dot is drawn in.
    ///
    /// The characters are the stack's: `●` running, `○` installed but stopped,
    /// `·` inactive or not installed. A character the stack never produces is
    /// drawn as the muted case rather than being a panic: a dot is not worth
    /// taking the window down for.
    pub const fn dot(&self, dot: char) -> Rgb {
        match dot {
            '●' => self.success,
            '○' => self.warning,
            _ => self.muted,
        }
    }

    /// The progress bar's fill for a position in `0..=PROGRESS_MAX`.
    ///
    /// An unfinished bar is the primary colour and a finished one is green, which
    /// is what makes "the stack is up" readable without reading the label.
    pub const fn progress(&self, position: u16) -> Rgb {
        if position >= PROGRESS_MAX {
            self.success
        } else {
            self.primary
        }
    }

    /// The colour of a card's own text, by how the card is doing.
    pub fn card_accent(&self, view: &lambo_core::ui_state::CardView) -> Rgb {
        self.dot(view.dot)
    }
}

/// How heavy a font is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
    /// Body text and buttons.
    Regular,
    /// Card names and section titles.
    Semibold,
}

/// The font a control is created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Font {
    /// The height, in logical pixels.
    pub size: i32,
    /// How heavy it is.
    pub weight: FontWeight,
    /// Whether it is a fixed-pitch face, which the log and the editor are.
    pub mono: bool,
}

impl Font {
    /// The interface's own size, for a button or a caption.
    pub const UI: Self = Self {
        size: 13,
        weight: FontWeight::Regular,
        mono: false,
    };

    /// A card's name.
    pub const CARD_NAME: Self = Self {
        size: 14,
        weight: FontWeight::Semibold,
        mono: false,
    };

    /// A card's status line.
    pub const CARD_STATUS: Self = Self {
        size: 12,
        weight: FontWeight::Regular,
        mono: false,
    };

    /// The log, the editor and anything else that is aligned in columns.
    pub const MONO: Self = Self {
        size: 12,
        weight: FontWeight::Regular,
        mono: true,
    };
}

/// The font for a control, from what the control is.
pub const fn font_for(kind: &WidgetKind) -> Font {
    match kind {
        WidgetKind::Text(_) => Font::MONO,
        WidgetKind::Card { .. } => Font::CARD_NAME,
        WidgetKind::Label(_)
        | WidgetKind::Button { .. }
        | WidgetKind::Edit(_)
        | WidgetKind::Combo { .. }
        | WidgetKind::List { .. }
        | WidgetKind::Progress { .. } => Font::UI,
    }
}

/// The font for one control inside a card.
///
/// A card is drawn by the window rather than created as a control, so its parts
/// ask for their own font by part rather than by widget kind.
pub const fn card_font(part: CardPart) -> Font {
    match part {
        CardPart::Name => Font::CARD_NAME,
        CardPart::Status => Font::CARD_STATUS,
        _ => Font::UI,
    }
}

/// The font for a widget, children included.
pub fn widget_font(widget: &Widget) -> Font {
    match (&widget.kind, widget.id) {
        (WidgetKind::Label(_), WidgetId::Card { part, .. }) => card_font(part),
        (kind, _) => font_for(kind),
    }
}

/// What Windows says the screen is scaled by, as dots per inch.
pub const DPI_DEFAULT: u32 = 96;

/// The largest scale the panel honours: 500% is where Windows stops.
pub const DPI_MAX: u32 = 480;

/// A report of the screen's scale, made usable.
///
/// Zero is what a failed query returns, and a scale below the default would
/// shrink the interface to nothing, so both fall back to 96.
pub const fn clamp_dpi(dpi: u32) -> u32 {
    if dpi < DPI_DEFAULT {
        DPI_DEFAULT
    } else if dpi > DPI_MAX {
        DPI_MAX
    } else {
        dpi
    }
}

/// A logical measurement in physical pixels.
///
/// The layout is written in the units the original layout used, which are pixels
/// at 100%; a 150% screen gets half again as many, or a control's text is right
/// and its box is not.
pub const fn scale(logical: i32, dpi: u32) -> i32 {
    let dpi = clamp_dpi(dpi) as i64;
    (((logical as i64) * dpi + DPI_DEFAULT as i64 / 2) / DPI_DEFAULT as i64) as i32
}

/// A logical rectangle in physical pixels.
pub const fn scale_rect(rect: Rect, dpi: u32) -> (i32, i32, i32, i32) {
    (
        scale(rect.0, dpi),
        scale(rect.1, dpi),
        scale(rect.2, dpi),
        scale(rect.3, dpi),
    )
}

/// The size the window opens at, for a stack of this many services.
///
/// The layout owns the arithmetic - it is what decides how many rows of cards
/// there are - so the window asks rather than computing a height of its own.
pub fn window_size(services: usize) -> (i32, i32) {
    (WINDOW_W, Layout::compute(services).window_h)
}

/// The window's title.
pub const fn window_title() -> &'static str {
    WINDOW_TITLE
}

/// The number Win32 reports a control's events with.
///
/// `WidgetId::value` is a `u16` because the menu and control identifiers it
/// shares a namespace with are; a control's identifier is an `i32`.
pub const fn control_id(id: WidgetId) -> i32 {
    id.value() as i32
}

/// Where a service's icon is, when the installation carries one.
pub fn service_icon(base_dir: &Path, name: &str) -> Option<PathBuf> {
    icon_file(name).map(|file| icon_path(base_dir, file))
}

/// The tray command a menu identifier belongs to.
///
/// The tray's numbers are the original's (40001 and up) and are not part of the
/// widget numbering: they arrive as ordinary `WM_COMMAND`s from a popup menu, so
/// the window has to tell the two apart before it routes anything.
pub fn tray_command(id: u16) -> Option<TrayCommand> {
    [
        TrayCommand::Show,
        TrayCommand::Start,
        TrayCommand::Stop,
        TrayCommand::ToggleAutoStart,
        TrayCommand::Quit,
    ]
    .into_iter()
    .find(|command| command.id() == id)
}

#[cfg(test)]
#[path = "view/tests.rs"]
mod tests;
