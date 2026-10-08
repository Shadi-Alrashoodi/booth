use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, Style, SystemTheme, TextStyle, Theme, ThemePreference, ViewportCommand, Visuals,
    style::ScrollAnimation, style::ScrollFadeStyle, style::ScrollStyle, style::WidgetVisuals, vec2,
};
use room::view::Level;

// Surfaces, each a little lighter as it comes forward. Regions are told apart
// by tone and space; the panel draws no gray lines between them.
pub const WINDOW: Color32 = Color32::from_rgb(0x14, 0x14, 0x12);
pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1B);
pub const CONTROL: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x26);
pub const RAISED: Color32 = Color32::from_rgb(0x35, 0x34, 0x2F);
// The outline of an unchecked checkbox or radio and the slider's empty track:
// marks found by their outline. Never a frame around anything.
pub const EDGE: Color32 = Color32::from_rgb(0x75, 0x73, 0x6B);

pub const CHALK: Color32 = Color32::from_rgb(0xE4, 0xE2, 0xDD);
pub const ASH: Color32 = Color32::from_rgb(0x9A, 0x97, 0x8E);
// Text on amber, and selected text on ash. The same number as the window,
// for a different job.
pub const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x12);

pub const AMBER: Color32 = Color32::from_rgb(0xE8, 0x91, 0x2F);
pub const AMBER_HOVER: Color32 = Color32::from_rgb(0xEE, 0x9C, 0x40);
pub const AMBER_PRESS: Color32 = Color32::from_rgb(0xD9, 0x84, 0x2A);

pub const SAGE: Color32 = Color32::from_rgb(0x6F, 0xA8, 0x6F);
// 47 degrees of hue, plainly yellow next to amber's 32, so a warning never
// reads as the accent.
pub const WARN: Color32 = Color32::from_rgb(0xCB, 0xB0, 0x4B);
pub const BAD: Color32 = Color32::from_rgb(0xDE, 0x74, 0x61);
pub const BAD_TINT: Color32 = Color32::from_rgb(0x3A, 0x25, 0x22);
pub const BAD_PRESS: Color32 = Color32::from_rgb(0x45, 0x2B, 0x27);

// The 8 px grid. HALF_STEP is only for a line that belongs to the control
// above it, a help line or an error under a field.
pub const HALF_STEP: f32 = 4.0;
pub const STEP: f32 = 8.0;
pub const FIELD_GAP: f32 = 16.0;
pub const SECTION_GAP: f32 = 24.0;
// Left and right of every screen and region, the title row and the strip
// included.
pub const SIDE: f32 = 16.0;

// Every button, field and list row.
pub const CONTROL_HEIGHT: f32 = 32.0;
pub const ROW_HEIGHT: f32 = CONTROL_HEIGHT;
pub const TITLE_ROW: f32 = 48.0;
// Inside a button or a field, either side of its text.
pub const TEXT_PAD: f32 = 12.0;
pub const PRIMARY_WIDTH: f32 = 80.0;

pub const CONTROL_RADIUS: u8 = 4;
pub const CHECK_RADIUS: u8 = 3;
pub const TRACK_RADIUS: u8 = 2;
pub const RING_RADIUS: u8 = 6;
pub const RING_WIDTH: f32 = 2.0;
pub const RING_GAP: f32 = 2.0;

pub const ICON_SIZE: f32 = 16.0;

const SANS_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-Regular.ttf");
const SANS_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-Medium.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexMono-Regular.ttf");
// IBM Plex Sans Arabic 1.1.0, the TTFs of github.com/IBM/plex at commit
// 1da12f02587b630c07e92692d21492d722f53614, TrueType hinted like Plex Sans.
// SHA-256 of Regular: 8e0f1046c736bf939d4939ee3ae0116acf61cbcd6592deae7656761627080981
// SHA-256 of Medium: eef162792cf2a6ba5af7943af9f86843b1286ab67aaf615c18c07fd8e97a4a90
const ARABIC_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSansArabic-Regular.ttf");
const ARABIC_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSansArabic-Medium.ttf");

egui_phosphor::subset! {
    pub mod icons {
        use regular::{EAR_SLASH, GEAR_SIX, MICROPHONE_SLASH};
    }
}

const MEDIUM: &str = "medium";
const ICONS: &str = "icons";

// The four steps of the type scale. Hierarchy comes from weight and colour
// before size, so neighbouring steps also differ in one of those.
pub fn title() -> FontId {
    FontId::new(16.0, FontFamily::Name(MEDIUM.into()))
}

pub fn section() -> FontId {
    FontId::new(14.0, FontFamily::Name(MEDIUM.into()))
}

// Names in the people list and the known hosts: the one thing set at the
// section size in Regular, since who is here is the first question.
pub fn name() -> FontId {
    FontId::new(14.0, FontFamily::Proportional)
}

pub fn body() -> FontId {
    FontId::new(13.0, FontFamily::Proportional)
}

// Button labels and the author line in chat.
pub fn medium() -> FontId {
    FontId::new(13.0, FontFamily::Name(MEDIUM.into()))
}

pub fn caption() -> FontId {
    FontId::new(12.0, FontFamily::Proportional)
}

// Codes, typed ports and addresses, the stats values.
pub fn mono() -> FontId {
    FontId::new(13.0, FontFamily::Monospace)
}

// The strip, the per-person ping, timestamps, a fingerprint after a name.
pub fn mono_caption() -> FontId {
    FontId::new(12.0, FontFamily::Monospace)
}

pub fn icon() -> FontId {
    FontId::new(ICON_SIZE, FontFamily::Name(ICONS.into()))
}

pub fn level_color(level: Level) -> Color32 {
    match level {
        Level::Good => SAGE,
        Level::Warn => WARN,
        Level::Bad => BAD,
    }
}

// A button's role, chosen by what it does, never by where it sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    // The one action a screen is for.
    Primary,
    Secondary,
    // Leave, Close room, Forget, Remove: tinted only under the mouse or with
    // keyboard focus, so the warning comes before the click.
    Destructive,
}

// A fill and the label on it. A transparent fill is no fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paint {
    pub fill: Color32,
    pub label: Color32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonLook {
    pub rest: Paint,
    pub hover: Paint,
    pub press: Paint,
}

const fn paint(fill: Color32, label: Color32) -> Paint {
    Paint { fill, label }
}

const NONE: Color32 = Color32::TRANSPARENT;

pub const PRIMARY: ButtonLook = ButtonLook {
    rest: paint(AMBER, INK),
    hover: paint(AMBER_HOVER, INK),
    press: paint(AMBER_PRESS, INK),
};

pub const SECONDARY: ButtonLook = ButtonLook {
    rest: paint(NONE, CHALK),
    hover: paint(CONTROL, CHALK),
    press: paint(RAISED, CHALK),
};

// A secondary toggle while it is on, Unmute and Undeafen: it keeps the
// control fill, so a muted microphone shows from across the room.
pub const TOGGLE_ON: ButtonLook = ButtonLook {
    rest: paint(CONTROL, CHALK),
    hover: paint(CONTROL, CHALK),
    press: paint(RAISED, CHALK),
};

pub const DESTRUCTIVE: ButtonLook = ButtonLook {
    rest: paint(NONE, CHALK),
    hover: paint(BAD_TINT, BAD),
    press: paint(BAD_PRESS, CHALK),
};

impl Role {
    pub fn look(self) -> ButtonLook {
        match self {
            Role::Primary => PRIMARY,
            Role::Secondary => SECONDARY,
            Role::Destructive => DESTRUCTIVE,
        }
    }
}

// Fields are filled, with no edge at rest. The amber edge shows whenever the
// field has focus, since that is where typing goes.
pub const FIELD_FILL: Color32 = CONTROL;
pub const FIELD_TEXT: Color32 = CHALK;
pub const FIELD_HINT: Color32 = ASH;
pub const FIELD_FOCUS: Color32 = AMBER;
// Around buttons, choice rows, the slider and the strip with keyboard focus.
// Chalk reads on every surface and around an amber button.
pub const FOCUS_RING: Color32 = CHALK;

// The talking ring and the quiet dot in a person's ring slot.
pub const TALKING: Color32 = AMBER;
pub const QUIET_DOT: Color32 = EDGE;

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
// Plex Sans and only what it lacks, Arabic, comes from the Arabic face. The
// icon subset maps a to z to the blank glyphs its ligatures are built from,
// so it only ever comes after Plex Sans, in a family of its own.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::empty();
    for (name, bytes) in [
        ("plex-sans", SANS_REGULAR),
        ("plex-sans-medium", SANS_MEDIUM),
        ("plex-mono", MONO_REGULAR),
        ("plex-sans-arabic", ARABIC_REGULAR),
        ("plex-sans-arabic-medium", ARABIC_MEDIUM),
        ("phosphor", &icons::regular::FONT),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    fonts.families.insert(
        FontFamily::Proportional,
        vec!["plex-sans".to_owned(), "plex-sans-arabic".to_owned()],
    );
    // A friend's name can turn up among the stats values, which are mono.
    fonts.families.insert(
        FontFamily::Monospace,
        vec![
            "plex-mono".to_owned(),
            "plex-sans".to_owned(),
            "plex-sans-arabic".to_owned(),
        ],
    );
    fonts.families.insert(
        FontFamily::Name(MEDIUM.into()),
        vec![
            "plex-sans-medium".to_owned(),
            "plex-sans-arabic-medium".to_owned(),
            "plex-sans".to_owned(),
        ],
    );
    fonts.families.insert(
        FontFamily::Name(ICONS.into()),
        vec!["plex-sans".to_owned(), "phosphor".to_owned()],
    );
    fonts
}

fn style() -> Style {
    let mut style = Style {
        text_styles: [
            (TextStyle::Small, caption()),
            (TextStyle::Body, body()),
            (TextStyle::Button, medium()),
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
    spacing.item_spacing = vec2(STEP, 0.0);
    spacing.button_padding = vec2(TEXT_PAD, 0.0);
    spacing.interact_size = vec2(24.0, 24.0);
    spacing.window_margin = Margin::same(SIDE as i8);
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
        bg_fill: WINDOW,
        weak_bg_fill: WINDOW,
        bg_stroke: Stroke::NONE,
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0, CHALK),
        expansion: 0.0,
    };
    // Booth paints its own controls; what egui still draws from these is the
    // scroll bar's handle, in edge, as a mark found by its shape.
    let control = WidgetVisuals {
        bg_fill: EDGE,
        weak_bg_fill: WINDOW,
        bg_stroke: Stroke::NONE,
        corner_radius: CornerRadius::same(TRACK_RADIUS),
        fg_stroke: Stroke::new(1.0, CHALK),
        expansion: 0.0,
    };
    visuals.widgets.inactive = control;
    visuals.widgets.hovered = control;
    visuals.widgets.active = control;
    visuals.widgets.open = control;

    // Selected text turns ink on ash (6.32:1), the way text on amber is ink.
    visuals.selection.bg_fill = ASH;
    visuals.selection.stroke = Stroke::new(1.0, INK);
    visuals.hyperlink_color = CHALK;
    visuals.faint_bg_color = PANEL;
    visuals.extreme_bg_color = CONTROL;
    visuals.text_edit_bg_color = Some(FIELD_FILL);
    visuals.code_bg_color = PANEL;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = BAD;
    visuals.window_corner_radius = CornerRadius::same(RING_RADIUS);
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::NONE;
    visuals.window_highlight_topmost = false;
    visuals.menu_corner_radius = CornerRadius::same(RING_RADIUS);
    visuals.panel_fill = WINDOW;
    visuals.text_cursor.stroke = Stroke::new(2.0, CHALK);
    visuals.text_cursor.blink = false;
    visuals.text_cursor.preview = false;
    visuals.interact_cursor = None;
    visuals.striped = false;
    visuals
}
