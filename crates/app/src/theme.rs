use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, Style, SystemTheme, TextStyle, Theme, ThemePreference, ViewportCommand, Visuals,
    style::ScrollAnimation, style::ScrollFadeStyle, style::ScrollStyle, style::WidgetVisuals, vec2,
};
use room::view::Level;

pub const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x12);
pub const PANEL: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x19);
pub const LINE: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x26);
pub const LINE_STRONG: Color32 = Color32::from_rgb(0x6A, 0x68, 0x60);
pub const CHALK: Color32 = Color32::from_rgb(0xE4, 0xE2, 0xDD);
pub const ASH: Color32 = Color32::from_rgb(0x9A, 0x97, 0x8E);
pub const AMBER: Color32 = Color32::from_rgb(0xE8, 0x91, 0x2F);
pub const SAGE: Color32 = Color32::from_rgb(0x6F, 0xA8, 0x6F);
pub const WARN: Color32 = Color32::from_rgb(0xD9, 0xA4, 0x41);
pub const BAD: Color32 = Color32::from_rgb(0xDE, 0x74, 0x61);

pub const SIDE: f32 = 12.0;
pub const CONTROL_HEIGHT: f32 = 32.0;
pub const ROW_HEIGHT: f32 = 28.0;

const SANS_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-Regular.ttf");
const SANS_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-Medium.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexMono-Regular.ttf");
// IBM Plex Sans Arabic 1.1.0, the TTFs of github.com/IBM/plex at commit
// 1da12f02587b630c07e92692d21492d722f53614, TrueType hinted like Plex Sans.
// SHA-256 of Regular: 8e0f1046c736bf939d4939ee3ae0116acf61cbcd6592deae7656761627080981
// SHA-256 of Medium: eef162792cf2a6ba5af7943af9f86843b1286ab67aaf615c18c07fd8e97a4a90
const ARABIC_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSansArabic-Regular.ttf");
const ARABIC_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSansArabic-Medium.ttf");

const MEDIUM: &str = "medium";

pub fn body() -> FontId {
    FontId::new(13.0, FontFamily::Proportional)
}

pub fn medium() -> FontId {
    FontId::new(13.0, FontFamily::Name(MEDIUM.into()))
}

pub fn small() -> FontId {
    FontId::new(12.0, FontFamily::Proportional)
}

pub fn title() -> FontId {
    FontId::new(15.0, FontFamily::Name(MEDIUM.into()))
}

pub fn mono() -> FontId {
    FontId::new(13.0, FontFamily::Monospace)
}

pub fn level_color(level: Level) -> Color32 {
    match level {
        Level::Good => SAGE,
        Level::Warn => WARN,
        Level::Bad => BAD,
    }
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    let style = Arc::new(style());
    ctx.set_style_of(Theme::Dark, Arc::clone(&style));
    // The panel is dark whatever Windows is set to, so the light slot gets
    // the same style in case egui ever picks it.
    ctx.set_style_of(Theme::Light, style);
    ctx.set_theme(ThemePreference::Dark);
    // The title bar follows the Windows setting unless the window asks;
    // winit turns this into DWMWA_USE_IMMERSIVE_DARK_MODE.
    ctx.send_viewport_cmd(ViewportCommand::SetTheme(SystemTheme::Dark));
}

// egui's own fonts are left out: every glyph comes from the Plex files, and
// the mono and medium faces fall back to Plex Sans rather than to egui's.
// Plex Sans Arabic comes after Plex Sans, so Latin, digits and spaces keep
// Plex Sans and only what it lacks, Arabic, comes from the Arabic face.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::empty();
    for (name, bytes) in [
        ("plex-sans", SANS_REGULAR),
        ("plex-sans-medium", SANS_MEDIUM),
        ("plex-mono", MONO_REGULAR),
        ("plex-sans-arabic", ARABIC_REGULAR),
        ("plex-sans-arabic-medium", ARABIC_MEDIUM),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    fonts.families.insert(
        FontFamily::Proportional,
        vec!["plex-sans".to_owned(), "plex-sans-arabic".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Monospace,
        vec!["plex-mono".to_owned(), "plex-sans".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name(MEDIUM.into()),
        vec![
            "plex-sans-medium".to_owned(),
            "plex-sans-arabic-medium".to_owned(),
            "plex-sans".to_owned(),
        ],
    );
    fonts
}

fn style() -> Style {
    let mut style = Style {
        text_styles: [
            (TextStyle::Small, small()),
            (TextStyle::Body, body()),
            (TextStyle::Button, body()),
            (TextStyle::Heading, title()),
            (TextStyle::Monospace, mono()),
        ]
        .into(),
        animation_time: 0.0,
        scroll_animation: ScrollAnimation::none(),
        explanation_tooltips: false,
        url_in_tooltip: false,
        visuals: visuals(),
        ..Style::default()
    };

    let spacing = &mut style.spacing;
    spacing.item_spacing = vec2(8.0, 0.0);
    spacing.button_padding = vec2(12.0, 0.0);
    spacing.interact_size = vec2(24.0, 24.0);
    spacing.window_margin = Margin::same(12);
    spacing.scroll = scroll_style();

    style.interaction.selectable_labels = false;
    style.interaction.multi_widget_text_select = false;
    style.interaction.tooltip_delay = f32::INFINITY;

    style
}

// Always drawn, same look whether or not the mouse is over it: no fading in
// and no widening on hover. egui also lays a gradient over the last lines
// that follows the scroll offset; the bar already says there is more.
fn scroll_style() -> ScrollStyle {
    ScrollStyle {
        bar_width: 6.0,
        bar_inner_margin: 2.0,
        dormant_background_opacity: 0.0,
        active_background_opacity: 0.0,
        interact_background_opacity: 0.0,
        dormant_handle_opacity: 1.0,
        active_handle_opacity: 1.0,
        interact_handle_opacity: 1.0,
        fade: ScrollFadeStyle {
            strength: 0.0,
            size: 0.0,
        },
        ..ScrollStyle::solid()
    }
}

fn visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    visuals.dark_mode = true;
    visuals.text_options.font_hinting = true;
    visuals.text_options.subpixel_binning = false;
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(ASH);

    visuals.widgets.noninteractive = WidgetVisuals {
        bg_fill: INK,
        weak_bg_fill: INK,
        bg_stroke: Stroke::new(1.0, LINE),
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0, CHALK),
        expansion: 0.0,
    };
    // One look for every state. Focus is a ring the app draws itself, and
    // nothing changes under the mouse.
    let control = WidgetVisuals {
        bg_fill: LINE_STRONG,
        weak_bg_fill: INK,
        bg_stroke: Stroke::new(1.0, LINE_STRONG),
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0, CHALK),
        expansion: 0.0,
    };
    visuals.widgets.inactive = control;
    visuals.widgets.hovered = control;
    visuals.widgets.active = control;
    visuals.widgets.open = control;

    // Selected text turns ink on ash (6.32:1), the way text on amber is ink.
    // Chalk on line strong would be 4.31:1, under what 13 px text needs.
    visuals.selection.bg_fill = ASH;
    visuals.selection.stroke = Stroke::new(1.0, INK);
    visuals.hyperlink_color = AMBER;
    visuals.faint_bg_color = PANEL;
    visuals.extreme_bg_color = PANEL;
    visuals.text_edit_bg_color = Some(PANEL);
    visuals.code_bg_color = PANEL;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = BAD;
    visuals.window_corner_radius = CornerRadius::ZERO;
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0, LINE_STRONG);
    visuals.window_highlight_topmost = false;
    visuals.menu_corner_radius = CornerRadius::ZERO;
    visuals.panel_fill = INK;
    visuals.text_cursor.stroke = Stroke::new(2.0, CHALK);
    visuals.text_cursor.blink = false;
    visuals.text_cursor.preview = false;
    visuals.interact_cursor = None;
    visuals.striped = false;
    visuals
}
