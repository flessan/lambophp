//! Tests for the presentation layer.
//!
//! Everything here runs in the local harness as well as under `cargo test`: the
//! colours, the scaling and the conversions are decisions, and a decision that
//! cannot be checked on the machine it is written on is a decision that gets
//! checked by a user instead.

use super::*;
use lambo_core::ui_state::{
    CardAction, CardButton, CardView, PROGRESS_MAX, Page, Scheme, ServiceGroup, Theme,
    WINDOW_TITLE, WINDOW_W,
};

#[test]
fn a_colour_keeps_the_order_win32_wants() {
    // Red is the low byte, blue the high one: `CreateSolidBrush(0x00BBGGRR)`.
    assert_eq!(Rgb::new(0xff, 0x00, 0x00).as_u32(), 0x0000_00ff);
    assert_eq!(Rgb::new(0x00, 0xff, 0x00).as_u32(), 0x0000_ff00);
    assert_eq!(Rgb::new(0x00, 0x00, 0xff).as_u32(), 0x00ff_0000);
    assert_eq!(Rgb::new(0x12, 0x34, 0x56).channels(), (0x12, 0x34, 0x56));
    assert_eq!(Rgb::new(0x12, 0x34, 0x56).as_u32(), 0x0056_3412);

    // Blending is what makes a scheme one colour plus a hover shade.
    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);
    assert_eq!(black.blend(white, 0), black);
    assert_eq!(black.blend(white, 100), white);
    assert_eq!(black.blend(white, 50).channels(), (128, 128, 128));
    // A percentage above 100 is clamped rather than wrapping into a dark colour.
    assert_eq!(black.blend(white, 200), white);
}

#[test]
fn the_two_themes_are_different_palettes_with_the_same_meanings() {
    let light = Palette::for_theme(Theme::Light);
    let dark = Palette::for_theme(Theme::Dark);
    assert_eq!(light, Palette::LIGHT);
    assert_eq!(dark, Palette::DARK);
    assert_ne!(light.window, dark.window);
    assert_ne!(light.text, dark.text);

    // The theme the window asks for comes from the system's own answer.
    assert_eq!(Palette::for_dark_mode(false), Palette::LIGHT);
    assert_eq!(Palette::for_dark_mode(true), Palette::DARK);

    // Light text on a light window would be unreadable whichever theme is on.
    for palette in [light, dark] {
        let darkness = |colour: Rgb| {
            let (red, green, blue) = colour.channels();
            red as u32 + green as u32 + blue as u32
        };
        let (text, window) = (darkness(palette.text), darkness(palette.window));
        let apart = text.abs_diff(window);
        assert!(apart > 250, "text and window are too close: {apart}");
    }
}

#[test]
fn every_scheme_has_a_hover_shade_and_readable_text() {
    let schemes = [
        Scheme::Primary,
        Scheme::Success,
        Scheme::Danger,
        Scheme::Warning,
        Scheme::Neutral,
        Scheme::Sidebar,
    ];
    for palette in [Palette::LIGHT, Palette::DARK] {
        for scheme in schemes {
            let colours = palette.scheme(scheme);
            assert_ne!(colours.background, colours.hover, "{scheme:?}");
            assert_ne!(colours.background, colours.text, "{scheme:?}");
            assert_eq!(colours.hover, colours.background.blend(colours.text, 18));
        }
    }

    // The meanings are the original's: starting is green, stopping is red,
    // restarting is amber, and the first action is the blue one.
    assert_eq!(
        Palette::LIGHT.scheme(Scheme::Primary).background,
        Palette::LIGHT.primary
    );
    assert_eq!(
        Palette::LIGHT.scheme(Scheme::Success).background,
        Palette::LIGHT.success
    );
    assert_eq!(
        Palette::LIGHT.scheme(Scheme::Danger).background,
        Palette::LIGHT.danger
    );
    assert_eq!(
        Palette::LIGHT.scheme(Scheme::Warning).background,
        Palette::LIGHT.warning
    );
    assert_eq!(
        Palette::LIGHT.scheme(Scheme::Danger).text,
        Palette::LIGHT.on_accent()
    );
}

#[test]
fn a_status_dot_says_what_the_stack_says() {
    let palette = Palette::LIGHT;
    assert_eq!(palette.dot('●'), palette.success, "running");
    assert_eq!(palette.dot('○'), palette.warning, "installed but stopped");
    assert_eq!(palette.dot('·'), palette.muted, "inactive or missing");
    // A character the stack never produces is drawn muted, not by panicking.
    assert_eq!(palette.dot('?'), palette.muted);
}

#[test]
fn a_card_is_accented_by_how_it_is_doing() {
    let palette = Palette::DARK;
    let card = |dot| CardView {
        name: "Apache  2.4.68".to_owned(),
        status: "Running  (pid 4)".to_owned(),
        dot,
        toggle: CardButton {
            label: "■ Stop",
            scheme: Scheme::Danger,
            enabled: true,
            action: CardAction::Toggle,
        },
        restart: CardButton {
            label: "↻",
            scheme: Scheme::Warning,
            enabled: true,
            action: CardAction::Restart,
        },
        buttons: Vec::new(),
        icon: None,
        group: ServiceGroup::Web,
        enabled: true,
        has_variants: false,
        active: true,
    };

    assert_eq!(palette.card_accent(&card('●')), palette.success);
    assert_eq!(palette.card_accent(&card('○')), palette.warning);
    assert_eq!(palette.card_accent(&card('·')), palette.muted);
}

#[test]
fn the_bar_is_blue_until_it_is_done() {
    let palette = Palette::LIGHT;
    assert_eq!(palette.progress(0), palette.primary);
    assert_eq!(palette.progress(PROGRESS_MAX / 2), palette.primary);
    assert_eq!(palette.progress(PROGRESS_MAX - 1), palette.primary);
    assert_eq!(
        palette.progress(PROGRESS_MAX),
        palette.success,
        "the stack is up"
    );
}

#[test]
fn a_font_follows_what_the_control_is() {
    assert_eq!(
        font_for(&WidgetKind::Button {
            label: "Start Stack".to_owned(),
            scheme: Scheme::Success,
            enabled: true,
        }),
        Font::UI
    );
    assert_eq!(font_for(&WidgetKind::Label("Logs".to_owned())), Font::UI);
    assert_eq!(font_for(&WidgetKind::Edit(String::new())), Font::UI);
    assert_eq!(
        font_for(&WidgetKind::Combo {
            items: vec!["Apache".to_owned()],
            selected: 0,
        }),
        Font::UI
    );
    assert_eq!(
        font_for(&WidgetKind::List {
            columns: vec![("Domain", 180)],
            rows: Vec::new(),
        }),
        Font::UI
    );
    assert_eq!(
        font_for(&WidgetKind::Progress {
            label: "Idle".to_owned(),
            position: 0,
        }),
        Font::UI
    );
    // The log, the editor and the version picker's text are fixed-pitch: the log
    // is a column of timestamps.
    assert_eq!(font_for(&WidgetKind::Text(String::new())), Font::MONO);
    const { assert!(Font::MONO.mono) };

    // A card's name is the loudest thing on it, and its status line the quietest.
    assert_eq!(card_font(CardPart::Name), Font::CARD_NAME);
    assert_eq!(card_font(CardPart::Status), Font::CARD_STATUS);
    assert_eq!(card_font(CardPart::Toggle), Font::UI);
    const { assert!(Font::CARD_NAME.size > Font::UI.size) };
    const { assert!(Font::CARD_STATUS.size < Font::UI.size) };
    assert_eq!(Font::CARD_NAME.weight, FontWeight::Semibold);
}

#[test]
fn a_widget_asks_for_its_own_font() {
    let label = Widget::label(WidgetId::Decoration, (0, 0, 10, 10), "Logs");
    assert_eq!(widget_font(&label), Font::UI);
    let card_part = Widget::label(
        WidgetId::Card {
            index: 0,
            part: CardPart::Name,
        },
        (0, 0, 10, 10),
        "Apache",
    );
    assert_eq!(widget_font(&card_part), Font::CARD_NAME);
}

#[test]
fn scaling_is_arithmetic_and_nothing_else() {
    assert_eq!(DPI_DEFAULT, 96);
    // At 100% a logical pixel is a physical one.
    assert_eq!(scale(100, 96), 100);
    assert_eq!(scale(0, 96), 0);
    // 150% and 125% are the two Windows offers besides 100%.
    assert_eq!(scale(100, 144), 150);
    assert_eq!(scale(100, 120), 125);
    assert_eq!(scale(7, 192), 14);
    assert_eq!(scale(1, 120), 1, "half a pixel rounds to the nearer one");
    assert_eq!(scale(3, 120), 4);

    // A failed query answers zero, and a scale below 100% is not a thing.
    assert_eq!(clamp_dpi(0), 96);
    assert_eq!(clamp_dpi(72), 96);
    assert_eq!(clamp_dpi(120), 120);
    assert_eq!(clamp_dpi(480), 480);
    assert_eq!(clamp_dpi(960), 480, "beyond 500% Windows does not go");

    // A rectangle scales as four numbers, not as a width and a height that
    // happen to fit.
    assert_eq!(scale_rect((10, 4, 110, 26), 144), (15, 6, 165, 39));
    assert_eq!(scale_rect((0, 0, 0, 0), 96), (0, 0, 0, 0));
}

#[test]
fn the_window_is_the_layouts_size_and_the_products_title() {
    assert_eq!(window_size(4), (WINDOW_W, Layout::compute(4).window_h));
    assert_eq!(window_size(0).0, WINDOW_W);
    assert_eq!(window_title(), WINDOW_TITLE);
    assert!(
        window_title().starts_with("Lambo PHP"),
        "{}",
        window_title()
    );
    assert!(
        !window_title().to_lowercase().contains("goampp"),
        "the title is this product's"
    );
}

#[test]
fn a_widget_number_reaches_win32_unchanged() {
    let ids = [
        WidgetId::Page(Page::Services),
        WidgetId::WebPicker,
        WidgetId::StartStack,
        WidgetId::Log,
        WidgetId::Card {
            index: 3,
            part: CardPart::Toggle,
        },
        WidgetId::Version {
            index: 1,
            variant: 2,
        },
        WidgetId::Settings(lambo_core::ui_state::SettingsAction::Quit),
    ];
    for id in ids {
        assert_eq!(control_id(id), id.value() as i32);
    }
    // A caption is zero, which the window routes nowhere.
    assert_eq!(control_id(WidgetId::Decoration), 0);
    assert_ne!(
        control_id(WidgetId::Decoration),
        control_id(WidgetId::StartStack)
    );
}

#[test]
fn the_trays_numbers_are_the_originals_and_are_not_widgets() {
    for command in [
        TrayCommand::Show,
        TrayCommand::Start,
        TrayCommand::Stop,
        TrayCommand::ToggleAutoStart,
        TrayCommand::Quit,
    ] {
        assert_eq!(tray_command(command.id()), Some(command), "{command:?}");
        // The tray's range is its own: 40001 and up, far above any widget.
        assert!(command.id() >= 40001);
        assert!(WidgetId::from_value(command.id()).is_none());
    }
    assert_eq!(TrayCommand::Show.id(), 40001);
    assert_eq!(TrayCommand::Quit.id(), 40005);
    assert_eq!(tray_command(40000), None);
    assert_eq!(tray_command(40006), None);
    assert_eq!(tray_command(0), None);
}

#[test]
fn a_service_icon_is_the_file_the_installation_carries() {
    let base = Path::new("C:\\Lambo");
    assert_eq!(
        service_icon(base, "Apache"),
        Some(base.join("assets").join("icons").join("apache.ico"))
    );
    assert_eq!(
        service_icon(base, "Node.js"),
        Some(base.join("assets").join("icons").join("nodejs.ico"))
    );
    // A component the map does not know has no icon to draw, rather than a path
    // that would fail at `LoadImage`.
    assert_eq!(service_icon(base, "Something Else"), None);
}
