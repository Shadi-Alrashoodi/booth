use std::ops::RangeInclusive;

use eframe::egui::emath::GuiRounding;
use eframe::egui::text::LayoutJob;
use eframe::egui::{
    Align, Color32, Context, CursorIcon, Event, EventFilter, FontId, Frame, Id, Key,
    KeyboardShortcut, Label, LayerId, Layout, Margin, Modifiers, Order, Painter, Rect, Response,
    RichText, ScrollArea, Sense, Shape, Stroke, StrokeKind, TextBuffer, TextEdit, Ui, UiBuilder,
    Vec2, WidgetInfo, WidgetType, accesskit, pos2, vec2,
};

use crate::theme::{
    self, AMBER, ASH, CHALK, CHECK_RADIUS, CONTROL_HEIGHT, CONTROL_RADIUS, EDGE, FIELD_FILL,
    FIELD_FOCUS, FIELD_HINT, FIELD_OFF, FIELD_TEXT, FOCUS_RING, ICON_SIZE, INK, PANEL,
    PRIMARY_WIDTH, RING_GAP, RING_RADIUS, RING_WIDTH, ROW_HEIGHT, Role, SIDE, STEP, TEXT_PAD,
    TITLE_ROW, TOGGLE_ON, TRACK_RADIUS,
};

// The slider's track and the meter's, and the handle on the slider: a chalk
// bar as tall as a line of text.
const TRACK: f32 = 4.0;
const HANDLE: Vec2 = vec2(6.0, 16.0);
const COMPOSER_ROWS: f32 = 4.0;
const CHOICE_MARK: f32 = 16.0;
const RADIO_DOT: f32 = 6.0;
// Sentences on the start screen, the firewall screen and in settings stop at
// about this many characters, measured on an ordinary sentence.
const PROSE_CHARS: f32 = 60.0;
const SAMPLE: &str = "the quick brown fox jumps over the lazy dog";
const KEYBOARD_FOCUS: &str = "focus came from the keyboard";
const FOCUS_LAYER: &str = "focus rings";

// What sits at the left of the title row: nothing on the screens before a
// room, where the window's own title bar already shows the mark, or the
// screen's title.
#[derive(Clone, Copy)]
pub enum Lead<'a> {
    Empty,
    Title(&'a str),
}

// 48 px in window tone, with its 32 px row in the middle: the title, if any,
// on the left, the verbs at the right in reading order, and no line
// under it; what follows starts with its own tone or its own first row.
// Returns which verb was pressed; one that is not enabled never is.
pub fn title_row(ui: &mut Ui, lead: Lead, verbs: &[Button]) -> Option<usize> {
    let (outer, _) = ui.allocate_exact_size(vec2(ui.available_width(), TITLE_ROW), Sense::hover());
    let rect = outer.shrink2(vec2(SIDE, (TITLE_ROW - CONTROL_HEIGHT) / 2.0));
    // The row is its own clip, so a focus ring on a verb stays inside the
    // window at the top and above the region below, which paints later.
    let mut row = ui.new_child(UiBuilder::new().max_rect(outer));
    row.set_clip_rect(outer.intersect(ui.clip_rect()));
    let width = buttons_width(&row, verbs);
    let mut pressed = None;
    split_row(
        &mut row,
        rect,
        width,
        |ui| match lead {
            Lead::Empty => {}
            Lead::Title(title) => {
                one_line(ui, title, theme::title(), CHALK);
            }
        },
        |ui| pressed = buttons(ui, verbs, rect.height()),
    );
    pressed
}

// Buttons side by side with the usual gap, as wide as buttons() lays them.
pub fn buttons_width(ui: &Ui, buttons: &[Button]) -> f32 {
    let gap = ui.spacing().item_spacing.x;
    let widths: f32 = buttons.iter().map(|button| button.width(ui)).sum();
    widths + gap * buttons.len().saturating_sub(1) as f32
}

// Left to right in a space `height` tall, for the right side of a split_row,
// which lays out from the right edge. Returns which one was pressed. Each
// button's id comes from its word: by its place alone, Settings on the start
// screen would take over the id of Leave, which stood in the same place a
// frame before, and with it the keyboard focus Leave had.
pub fn buttons(ui: &mut Ui, buttons: &[Button], height: f32) -> Option<usize> {
    let width = buttons_width(ui, buttons);
    let layout = Layout::left_to_right(Align::Center);
    let mut pressed = None;
    ui.allocate_ui_with_layout(vec2(width, height), layout, |ui| {
        for (i, button) in buttons.iter().enumerate() {
            let clicked = ui
                .push_id(button.text, |ui| button.show(ui))
                .inner
                .clicked();
            if clicked && button.enabled {
                pressed = Some(i);
            }
        }
    });
    pressed
}

// One row with a name on the left and one thing `right_width` wide at the
// right edge, at least 8 px apart. The width is known up front so the left
// side is laid out first, in reading order for Tab and screen readers, and
// is what gets cut when the row is narrow.
pub fn split_row(
    ui: &mut Ui,
    rect: Rect,
    right_width: f32,
    left: impl FnOnce(&mut Ui),
    right: impl FnOnce(&mut Ui),
) {
    let left_end = if right_width > 0.0 {
        rect.right() - right_width - STEP
    } else {
        rect.right()
    };
    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                rect.min,
                pos2(left_end.max(rect.left()), rect.bottom()),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    left(&mut left_ui);
    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    right(&mut right_ui);
}

pub fn text_width(ui: &Ui, text: &str, font: FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, CHALK)
        .size()
        .x
        .ceil()
}

// A row of text cut with an ellipsis at the end rather than wrapped.
pub fn one_line(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> Response {
    let line_height = line_height(&font);
    ui.add(
        Label::new(
            RichText::new(text)
                .font(font)
                .color(color)
                .line_height(Some(line_height)),
        )
        .truncate()
        .show_tooltip_when_elided(false),
    )
}

// The same, painted so its baseline sits at `baseline` when one is given:
// smaller text after a name on one row, which centring alone would put a
// pixel or two above the name's baseline. Returns the label and where its
// baseline is, in points from the top of the window.
pub fn one_line_on(
    ui: &mut Ui,
    text: &str,
    font: FontId,
    color: Color32,
    baseline: Option<f32>,
) -> (Response, f32) {
    let line_height = line_height(&font);
    let label = Label::new(
        RichText::new(text)
            .font(font)
            .color(color)
            .line_height(Some(line_height)),
    )
    .truncate()
    .show_tooltip_when_elided(false);
    let (at, galley, response) = label.layout_in_ui(ui);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, galley.text()));
    let mut at = at.round_to_pixels(ui.pixels_per_point());
    let own = galley
        .rows
        .first()
        .and_then(|row| row.glyphs.first().map(|glyph| row.pos.y + glyph.pos.y))
        .unwrap_or(0.0);
    if let Some(baseline) = baseline {
        at.y = baseline - own;
    }
    if ui.is_rect_visible(response.rect) {
        ui.painter().galley(at, galley, color);
    }
    (response, at.y + own)
}

// How far below the top of a row `font`'s baseline sits when the row is laid
// out at that font's own height, before any rounding.
pub fn ascent(ui: &Ui, font: &FontId) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap(String::from("0"), font.clone(), CHALK);
    galley
        .rows
        .first()
        .and_then(|row| row.glyphs.first())
        .map_or(0.0, |glyph| {
            glyph.font_face_ascent + 0.5 * (glyph.font_height - glyph.font_face_height)
        })
}

// The body of a screen: the side gutter, and the first line 16 px below the
// title row's controls, which have 8 of their own under them.
pub fn page<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    gutter(ui, STEP, SIDE, add)
}

// The same side gutter with its own space above and below: after a block in
// panel tone, or for a line that belongs to the row above it.
pub fn gutter<R>(ui: &mut Ui, top: f32, bottom: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .inner_margin(Margin {
            left: SIDE as i8,
            right: SIDE as i8,
            top: top as i8,
            bottom: bottom as i8,
        })
        .show(ui, add)
        .inner
}

// A block set into the window in panel tone, edge to edge with square
// corners, since it meets the window's straight sides: the invite, a code to
// send back, a control request, the monitor list. The gutter on all four
// sides, so a 32 px row at the top sits 16 inside it as a sentence does, and
// the block's top and bottom match.
pub fn region<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(PANEL)
        .inner_margin(Margin::same(SIDE as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

// Whole device pixels, never under one, so at 150 percent a 1 px edge is one
// crisp row in its own colour instead of two rows in a colour between.
pub fn thickness(width: f32, per_point: f32) -> f32 {
    (width * per_point).floor().max(1.0) / per_point
}

// egui's focus look is its pressed look, so the ring is drawn here: 2 px of
// chalk, 2 px clear of the control, rounded 6 around a control rounded 4.
// Where the clip would cut it (a button in the title row, the strip on the
// window's bottom edge) it moves in rather than vanish.
pub fn ring(ui: &Ui, rect: Rect) {
    ring_within(ui.painter(), rect, ui.clip_rect());
}

// On a layer above the panel, so a region painted later, like the chat under
// the last row of people, cannot cover the part that reaches past the row.
pub fn ring_within(painter: &Painter, rect: Rect, bounds: Rect) {
    let painter = painter
        .clone()
        .with_layer_id(LayerId::new(Order::Foreground, Id::new(FOCUS_LAYER)))
        .with_clip_rect(bounds);
    let per_point = painter.pixels_per_point();
    let width = thickness(RING_WIDTH, per_point);
    let mut outer = rect.expand(RING_GAP + width);
    outer.min = outer.min.max(bounds.min);
    outer.max = outer.max.min(bounds.max);
    painter.rect_stroke(
        outer.round_to_pixels(per_point),
        RING_RADIUS,
        Stroke::new(width, FOCUS_RING),
        StrokeKind::Inside,
    );
}

// The one edge a field ever has: 1 px of amber just inside it while it has
// focus, clicked or tabbed to. The fill does not change.
fn focus_edge(ui: &Ui, rect: Rect) {
    let per_point = ui.pixels_per_point();
    ui.painter().rect_stroke(
        rect.round_to_pixels(per_point),
        CONTROL_RADIUS,
        Stroke::new(thickness(1.0, per_point), FIELD_FOCUS),
        StrokeKind::Inside,
    );
}

// egui also focuses a control that was clicked, and a ring left behind after
// every click would pull the eye in a panel that sits next to a game. So
// rings on buttons and the strip follow focus only when Tab, an arrow key or
// a screen reader moved it, and the next mouse press puts them away again.
pub fn note_focus_source(ctx: &Context) {
    let id = Id::new(KEYBOARD_FOCUS);
    let before = ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false);
    let keyboard = ctx.input(|input| {
        input.events.iter().fold(before, |keyboard, event| match event {
            Event::PointerButton { pressed: true, .. } => false,
            Event::Key {
                key: Key::Tab | Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight,
                pressed: true,
                ..
            } => true,
            Event::AccessKitActionRequest(request)
                if request.action == accesskit::Action::Focus =>
            {
                true
            }
            _ => keyboard,
        })
    });
    if keyboard != before {
        ctx.data_mut(|data| data.insert_temp(id, keyboard));
    }
}

pub fn keyboard_focus(ui: &Ui) -> bool {
    ui.ctx()
        .data(|data| data.get_temp::<bool>(Id::new(KEYBOARD_FOCUS)))
        .unwrap_or(false)
}

#[derive(Clone, Copy)]
pub struct Button<'a> {
    text: &'a str,
    // What a screen reader says, when the text alone does not say enough.
    label: Option<&'a str>,
    role: Role,
    // A secondary that keeps the control fill at rest: a toggle that is on,
    // Unmute or Undeafen, or Save while its known host row is open.
    on: bool,
    // The label's colour in place of the role's, for a word that is live,
    // a warning, or waiting.
    color: Option<Color32>,
    icon: Option<&'a str>,
    height: f32,
    min_width: f32,
    enter_target: bool,
    enabled: bool,
}

impl<'a> Button<'a> {
    pub fn new(text: &'a str) -> Button<'a> {
        Button {
            text,
            label: None,
            role: Role::Secondary,
            on: false,
            color: None,
            icon: None,
            height: CONTROL_HEIGHT,
            min_width: 0.0,
            enter_target: false,
            enabled: true,
        }
    }

    pub fn role(mut self, role: Role) -> Button<'a> {
        self.role = role;
        self
    }

    pub fn on(mut self, on: bool) -> Button<'a> {
        self.on = on;
        self
    }

    pub fn color(mut self, color: Color32) -> Button<'a> {
        self.color = Some(color);
        self
    }

    pub fn icon(mut self, icon: &'a str) -> Button<'a> {
        self.icon = Some(icon);
        self
    }

    pub fn label(mut self, label: &'a str) -> Button<'a> {
        self.label = Some(label);
        self
    }

    // A button that stays where it is and can still be reached with Tab,
    // so its name can say why it does nothing now. Screen readers call it
    // unavailable, the pointer stays an arrow over it, and title_row and
    // buttons() never report it pressed.
    pub fn enabled(mut self, enabled: bool) -> Button<'a> {
        self.enabled = enabled;
        self
    }

    pub fn height(mut self, height: f32) -> Button<'a> {
        self.height = height;
        self
    }

    // For a button whose word changes while it is used, so it keeps its
    // size and nothing beside it moves under the mouse.
    pub fn min_width(mut self, width: f32) -> Button<'a> {
        self.min_width = width;
        self
    }

    // Set while a field has focus whose Enter presses this button. A
    // primary needs no other mark; a secondary shows the fill it has under
    // the mouse.
    pub fn enter_target(mut self, yes: bool) -> Button<'a> {
        self.enter_target = yes;
        self
    }

    fn icon_width(&self) -> f32 {
        if self.icon.is_some() {
            ICON_SIZE + STEP
        } else {
            0.0
        }
    }

    pub fn width(&self, ui: &Ui) -> f32 {
        let least = if self.role == Role::Primary {
            PRIMARY_WIDTH
        } else {
            0.0
        };
        (text_width(ui, self.text, theme::medium()) + self.icon_width() + 2.0 * TEXT_PAD)
            .max(self.height)
            .max(self.min_width)
            .max(least)
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let width = self.width(ui);
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), Sense::click());
        let label = self.label.unwrap_or(self.text);
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, label));
        if ui.is_rect_visible(rect) {
            let keyboard = response.has_focus() && keyboard_focus(ui);
            let look = if self.on { TOGGLE_ON } else { self.role.look() };
            let lit = response.hovered()
                || (keyboard && self.role == Role::Destructive)
                || (self.enter_target && self.role != Role::Primary);
            // Disabled is ash with no fill whatever the button would show,
            // so a toggle left on in a room that has ended does not look
            // as if it still did something.
            let paint = if !self.enabled {
                theme::Paint {
                    fill: Color32::TRANSPARENT,
                    label: ASH,
                }
            } else if response.is_pointer_button_down_on() {
                look.press
            } else if lit {
                look.hover
            } else {
                look.rest
            };
            let label_color = match self.color {
                Some(color) if self.enabled && self.role == Role::Secondary => color,
                _ => paint.label,
            };
            let painter = ui.painter();
            if paint.fill != Color32::TRANSPARENT {
                painter.rect_filled(rect, CONTROL_RADIUS, paint.fill);
            }
            let galley = painter.layout_no_wrap(self.text.to_owned(), theme::medium(), label_color);
            let whole = self.icon_width() + galley.size().x;
            let left = rect.center().x - whole / 2.0;
            if let Some(icon) = self.icon {
                let glyph = painter.layout_no_wrap(icon.to_owned(), theme::icon(), label_color);
                let at = pos2(left, rect.center().y - glyph.size().y / 2.0);
                painter.galley(at.round(), glyph, label_color);
            }
            let at = pos2(
                left + self.icon_width(),
                rect.center().y - galley.size().y / 2.0,
            );
            painter.galley(at.round(), galley, label_color);
            if keyboard {
                ring(ui, rect);
            }
        }
        if self.enabled {
            response.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            response
        }
    }
}

// A glyph from the icon font, 16 px, in the colour of the text it belongs
// to.
pub fn icon(ui: &mut Ui, glyph: &str, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ICON_SIZE, ICON_SIZE), Sense::hover());
    if ui.is_rect_visible(rect) {
        let galley = ui
            .painter()
            .layout_no_wrap(glyph.to_owned(), theme::icon(), color);
        let at = rect.center() - galley.size() / 2.0;
        ui.painter().galley(at.round(), galley, color);
    }
    response
}

// A whole number picked along a line, with the number and its unit written
// at the right, since the line alone does not say how much. Drag or click
// on the line; with focus, the arrow keys step by one, Page Up and Page Down
// by ten, Home and End go to the ends. The row is 32 px tall, and all of it
// left of the number is the target.
pub fn slider(
    ui: &mut Ui,
    id: Id,
    value: &mut u32,
    range: RangeInclusive<u32>,
    unit: &str,
    label: &str,
) -> Response {
    let (lowest, highest) = (*range.start(), (*range.end()).max(*range.start()));
    let widest = text_width(ui, &format!("{highest} {unit}"), theme::mono());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let track_rect = Rect::from_min_max(
        rect.min,
        pos2(
            (rect.right() - widest - SIDE).max(rect.left()),
            rect.bottom(),
        ),
    );
    let mut response = ui.interact(track_rect, id, Sense::click_and_drag());
    // The handle's middle runs from half a handle in at each end, so the
    // handle never hangs out past the line.
    let (left, right) = (
        track_rect.left() + HANDLE.x / 2.0,
        (track_rect.right() - HANDLE.x / 2.0).max(track_rect.left() + HANDLE.x / 2.0),
    );
    let before = *value;
    let mut next = (*value).clamp(lowest, highest);
    if let Some(pointer) = response.interact_pointer_pos() {
        next = value_at(pointer.x, left, right, lowest, highest);
    }
    if response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                id,
                EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..EventFilter::default()
                },
            );
        });
        next = ui.input(|input| {
            let presses = |key| input.num_presses(key) as i64;
            if presses(Key::Home) > 0 {
                return lowest;
            }
            if presses(Key::End) > 0 {
                return highest;
            }
            let step = presses(Key::ArrowRight) + presses(Key::ArrowUp)
                - presses(Key::ArrowLeft)
                - presses(Key::ArrowDown)
                + 10 * (presses(Key::PageUp) - presses(Key::PageDown));
            (i64::from(next) + step).clamp(i64::from(lowest), i64::from(highest)) as u32
        });
    }
    ui.input(|input| {
        let requests = |action| input.num_accesskit_action_requests(id, action) as i64;
        let step = requests(accesskit::Action::Increment) - requests(accesskit::Action::Decrement);
        next = (i64::from(next) + step).clamp(i64::from(lowest), i64::from(highest)) as u32;
    });
    *value = next;
    if next != before {
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::slider(true, f64::from(next), label));
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_min_numeric_value(f64::from(lowest));
        node.set_max_numeric_value(f64::from(highest));
        node.set_numeric_value_step(1.0);
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let per_point = ui.pixels_per_point();
    let at = if highest > lowest {
        left + (right - left) * (next - lowest) as f32 / (highest - lowest) as f32
    } else {
        left
    };
    let line = |from: f32, to: f32| {
        Rect::from_min_max(
            pos2(from, rect.center().y - TRACK / 2.0),
            pos2(to, rect.center().y + TRACK / 2.0),
        )
        .round_to_pixels(per_point)
    };
    let painter = ui.painter();
    painter.rect_filled(
        line(track_rect.left(), track_rect.right()),
        TRACK_RADIUS,
        EDGE,
    );
    painter.rect_filled(line(track_rect.left(), at), TRACK_RADIUS, AMBER);
    let handle =
        Rect::from_center_size(pos2(at, rect.center().y), HANDLE).round_to_pixels(per_point);
    painter.rect_filled(handle, TRACK_RADIUS, CHALK);
    let shown = format!("{next} {unit}");
    let mut text = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(track_rect.right(), rect.top()),
                rect.max,
            ))
            .layout(Layout::right_to_left(Align::Center)),
    );
    one_line(&mut text, &shown, theme::mono(), CHALK);
    if response.has_focus() && keyboard_focus(ui) {
        ring(
            ui,
            track_rect.shrink2(vec2(0.0, (ROW_HEIGHT - HANDLE.y) / 2.0)),
        );
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

// The whole number nearest to where the pointer is along the line.
fn value_at(x: f32, left: f32, right: f32, lowest: u32, highest: u32) -> u32 {
    if right <= left || highest <= lowest {
        return lowest;
    }
    let fraction = ((x - left) / (right - left)).clamp(0.0, 1.0);
    lowest + (fraction * (highest - lowest) as f32).round() as u32
}

// A level along the same 4 px track as the slider, in chalk up to the level:
// a level is not a verdict, so it is never sage.
pub fn meter(ui: &mut Ui, fraction: f32, label: &str) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), TRACK), Sense::hover());
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::ProgressIndicator, true, label);
        info.value = Some(f64::from(fraction));
        info
    });
    if !ui.is_rect_visible(rect) {
        return;
    }
    let per_point = ui.pixels_per_point();
    let track = rect.round_to_pixels(per_point);
    ui.painter()
        .rect_filled(track, TRACK_RADIUS, theme::CONTROL);
    if fraction > 0.0 {
        let mut bar = track;
        bar.set_right(track.left() + track.width() * fraction.min(1.0));
        ui.painter()
            .rect_filled(bar.round_to_pixels(per_point), TRACK_RADIUS, CHALK);
    }
}

// A checkbox for a setting that is on or off, a radio for one of a set: the
// shape says which kind of choice it is, and the amber fill says which is
// chosen. The whole 32 px row is the target, the name 8 px after the mark,
// and a note after the name in caption ash.
pub fn choice_row(
    ui: &mut Ui,
    id: Id,
    selected: bool,
    name: &str,
    note: Option<&str>,
    kind: WidgetType,
) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::selected(kind, true, selected, name));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let per_point = ui.pixels_per_point();
    let mark = Rect::from_min_size(
        pos2(rect.left(), rect.center().y - CHOICE_MARK / 2.0),
        vec2(CHOICE_MARK, CHOICE_MARK),
    )
    .round_to_pixels(per_point);
    let painter = ui.painter();
    let edge = thickness(1.0, per_point);
    match (kind, selected) {
        (WidgetType::RadioButton, false) => {
            painter.circle_stroke(
                mark.center(),
                CHOICE_MARK / 2.0 - edge / 2.0,
                Stroke::new(edge, EDGE),
            );
        }
        (WidgetType::RadioButton, true) => {
            painter.circle_filled(mark.center(), CHOICE_MARK / 2.0, AMBER);
            painter.circle_filled(mark.center(), RADIO_DOT / 2.0, INK);
        }
        (_, false) => {
            painter.rect_stroke(
                mark,
                CHECK_RADIUS,
                Stroke::new(edge, EDGE),
                StrokeKind::Inside,
            );
        }
        (_, true) => {
            painter.rect_filled(mark, CHECK_RADIUS, AMBER);
            let at = |x: f32, y: f32| mark.min + vec2(x, y);
            painter.add(Shape::line(
                vec![at(4.0, 8.5), at(7.0, 11.5), at(12.0, 5.0)],
                Stroke::new(2.0, INK),
            ));
        }
    }
    let text = Rect::from_min_max(pos2(mark.right() + STEP, rect.top()), rect.max);
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(text)
            .layout(Layout::left_to_right(Align::Center)),
    );
    one_line(&mut row, name, theme::body(), CHALK);
    if let Some(note) = note {
        one_line(&mut row, note, theme::caption(), ASH);
    }
    if response.has_focus() && keyboard_focus(ui) {
        ring(ui, rect);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

fn field_frame(vertical: i8) -> Frame {
    Frame::new()
        .fill(FIELD_FILL)
        .corner_radius(CONTROL_RADIUS)
        .inner_margin(Margin::symmetric(TEXT_PAD as i8, vertical))
}

pub fn field(
    ui: &mut Ui,
    id: &str,
    text: &mut String,
    hint: &str,
    font: FontId,
    char_limit: usize,
) -> Response {
    let output = TextEdit::singleline(text)
        .id_salt(id)
        .char_limit(char_limit)
        .hint_text(RichText::new(hint).font(theme::body()).color(FIELD_HINT))
        .font(font)
        .text_color(FIELD_TEXT)
        .frame(field_frame(0))
        .vertical_align(Align::Center)
        .desired_width(f32::INFINITY)
        .min_size(vec2(0.0, CONTROL_HEIGHT))
        .show(ui);
    let response = output.response.response;
    // A screen reader reads the placeholder as the field's name, unless the
    // caller names it after a label above it.
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(hint));
    if response.has_focus() {
        focus_edge(ui, response.rect);
    }
    response
}

// A field for a short list of addresses, one per line, typed in Plex Mono.
// Enter starts a new line, so nothing is pressed by it. `label` is what a
// screen reader calls it. As tall as its lines on the body's 18 px, with the
// same space above and below them as a one-line field, so one line is 32 px
// and two are 50, and no empty line waits under the last.
pub fn lines_field(
    ui: &mut Ui,
    id: &str,
    text: &mut String,
    label: &str,
    char_limit: usize,
) -> Response {
    let font = theme::mono();
    let row = line_height(&font);
    let lines = text.split('\n').count();
    let mut layouter = |ui: &Ui, text: &dyn TextBuffer, wrap_width: f32| {
        let mut job = LayoutJob::simple(
            text.as_str().to_owned(),
            font.clone(),
            FIELD_TEXT,
            wrap_width,
        );
        job.keep_trailing_whitespace = true;
        for section in &mut job.sections {
            section.format.line_height = Some(row);
        }
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    };
    let output = TextEdit::multiline(text)
        .id_salt(id)
        .char_limit(char_limit)
        .font(theme::mono())
        .layouter(&mut layouter)
        .frame(field_frame(((CONTROL_HEIGHT - row) / 2.0).floor() as i8))
        .desired_width(f32::INFINITY)
        .desired_rows(lines)
        .min_size(vec2(0.0, CONTROL_HEIGHT))
        .show(ui);
    let response = output.response.response;
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(label));
    if response.has_focus() {
        focus_edge(ui, response.rect);
    }
    response
}

// Whether text reads right to left, by its first letter that has a direction,
// as Unicode decides it for a paragraph with nothing else to go by: Arabic or
// Hebrew first is right to left whatever follows, and digits, punctuation or
// spaces before that letter count for nothing.
pub fn right_to_left(text: &str) -> bool {
    unicode_bidi::get_base_direction_full(text) == unicode_bidi::Direction::Rtl
}

// The chat's field: one line that grows to four and then scrolls. Shift+Enter
// starts a new line; plain Enter is left to the caller, which sends. Tab
// still moves on, as in any other field. What is typed sits at the right
// edge while it starts with a right-to-left letter, the way the message will
// show in the chat. Disabled, in a Ui that is, it is filled in the window
// tone and its text is in ash, so it keeps its shape but no longer looks
// like the place to type.
pub fn composer(
    ui: &mut Ui,
    id: &str,
    text: &mut String,
    hint: &str,
    char_limit: usize,
) -> Response {
    let font = theme::body();
    let row = ui.fonts_mut(|fonts| fonts.row_height(&font));
    let pad = ((CONTROL_HEIGHT - row) / 2.0).floor();
    let rtl = right_to_left(text);
    let enabled = ui.is_enabled();
    let frame = if enabled {
        field_frame(pad as i8)
    } else {
        field_frame(pad as i8).fill(FIELD_OFF)
    };
    let shown = frame.show(ui, |ui| {
        ui.set_min_height(CONTROL_HEIGHT - 2.0 * pad);
        // The scroll handle floats over the right edge of what it scrolls,
        // which here is text: the lines wrap short of it instead. The field's
        // own padding already keeps it off the field's edge.
        let scroll = &mut ui.spacing_mut().scroll;
        scroll.bar_outer_margin = 0.0;
        scroll.floating_allocated_width = scroll.bar_width + scroll.bar_inner_margin;
        ScrollArea::vertical()
            .id_salt(id)
            .max_height(COMPOSER_ROWS * row)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                TextEdit::multiline(text)
                    .id_salt(id)
                    .char_limit(char_limit)
                    .hint_text(RichText::new(hint).font(theme::body()).color(FIELD_HINT))
                    .font(font)
                    .text_color(if enabled { FIELD_TEXT } else { ASH })
                    .frame(Frame::NONE)
                    .margin(Margin::ZERO)
                    .desired_rows(1)
                    .desired_width(f32::INFINITY)
                    .horizontal_align(if rtl { Align::RIGHT } else { Align::LEFT })
                    .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                    .show(ui)
                    .response
                    .response
            })
            .inner
    });
    // The side is chosen before this frame's keys reach the text, so a key
    // that turned it around was laid out the old way; the next frame, asked
    // for at once, puts it right.
    if right_to_left(text) != rtl {
        ui.ctx().request_repaint();
    }
    let response = shown.inner;
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(hint));
    if response.has_focus() {
        focus_edge(ui, shown.response.rect);
    }
    response
}

// True when this frame's input has a plain Enter: Shift+Enter is a new line.
pub fn enter_pressed(ui: &Ui) -> bool {
    ui.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    modifiers,
                    ..
                } if !modifiers.shift && !modifiers.ctrl && !modifiers.alt
            )
        })
    })
}

// The type scale's line height for each step.
pub fn line_height(font: &FontId) -> f32 {
    if font.size >= 16.0 {
        24.0
    } else if font.size >= 14.0 {
        20.0
    } else if font.size >= 13.0 {
        18.0
    } else {
        16.0
    }
}

pub fn text(ui: &mut Ui, text: impl Into<String>, font: FontId, color: Color32) -> Response {
    let line_height = line_height(&font);
    ui.label(
        RichText::new(text)
            .font(font)
            .color(color)
            .line_height(Some(line_height)),
    )
}

// A sentence or two, wrapped at about 60 characters however wide the window
// is: short lines are read at a glance, beside a game.
pub fn prose(ui: &mut Ui, words: impl Into<String>, font: FontId, color: Color32) -> Response {
    let sample = text_width(ui, SAMPLE, font.clone());
    let most = PROSE_CHARS * sample / SAMPLE.chars().count() as f32;
    ui.scope(|ui| {
        ui.set_max_width(ui.available_width().min(most));
        text(ui, words, font, color)
    })
    .inner
}

// Codes are read from both ends when people compare them, so the cut goes in
// the middle and the last characters stay.
pub fn elide_middle(ui: &Ui, text: &str, font: &FontId, width: f32) -> String {
    const KEEP_END: usize = 4;
    let advance = ui.fonts_mut(|fonts| fonts.glyph_width(font, '0'));
    if advance <= 0.0 {
        return text.to_owned();
    }
    let fits = (width / advance).floor() as usize;
    let count = text.chars().count();
    if count <= fits {
        return text.to_owned();
    }
    let start = fits.saturating_sub(KEEP_END + 1);
    let mut out: String = text.chars().take(start).collect();
    out.push('\u{2026}');
    out.extend(text.chars().skip(count - KEEP_END));
    out
}

// For proportional text, measured: a file path is read from both ends, the
// drive and the file name, so the cut goes in the middle.
pub fn fit_middle(ui: &Ui, text: &str, font: &FontId, width: f32) -> String {
    let fits = |text: &str| text_width(ui, text, font.clone()) <= width;
    if fits(text) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let cut = |keep: usize| {
        let head = keep / 2;
        let mut out: String = chars[..head].iter().collect();
        out.push('\u{2026}');
        out.extend(&chars[chars.len() - (keep - head)..]);
        out
    };
    // Keeping none always counts as fitting; keeping all never does.
    let (mut fitting, mut too_long) = (0, chars.len());
    while too_long - fitting > 1 {
        let keep = (fitting + too_long) / 2;
        if fits(&cut(keep)) {
            fitting = keep;
        } else {
            too_long = keep;
        }
    }
    cut(fitting)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use eframe::egui::{PointerButton, Pos2, RawInput};

    // One frame of `run` in the panel's smallest default window, 360 by 640,
    // with these events.
    pub(crate) fn frame<R>(
        ctx: &Context,
        events: Vec<Event>,
        run: impl FnOnce(&mut Ui) -> R,
    ) -> (R, eframe::egui::PlatformOutput) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 640.0))),
            events,
            ..RawInput::default()
        };
        let mut out = None;
        let mut run = Some(run);
        let mut output = ctx.run_ui(input, |ui| {
            if let Some(run) = run.take() {
                out = Some(run(ui));
            }
        });
        let platform = std::mem::take(&mut output.platform_output);
        output.drop_without_applying_deltas();
        (out.expect("the frame ran"), platform)
    }

    // A click is a press on one frame and the release on the next.
    pub(crate) fn press(at: Pos2) -> Vec<Event> {
        vec![
            Event::PointerMoved(at),
            Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ]
    }

    pub(crate) fn release(at: Pos2) -> Vec<Event> {
        vec![Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }]
    }

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn verbs<'a>(busy: &'a str) -> [Button<'a>; 2] {
        [
            Button::new("Share")
                .color(theme::ASH)
                .enabled(false)
                .label(busy),
            Button::new("Leave").role(Role::Destructive),
        ]
    }

    const TUESDAY: Lead = Lead::Title("Tuesday night");

    // Share and Leave at the right of the title row in that order. While
    // someone else shares, Share stays in ash, a click on it does nothing,
    // and a screen reader hears who shares.
    #[test]
    fn title_row_verbs() {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let busy = "Share. Ines is sharing. One share at a time.";
        // The middle of the 32 px row inside the 48 px title row.
        let middle = TITLE_ROW / 2.0;
        let ((share, leave), output) = frame(&ctx, Vec::new(), |ui| {
            let verbs = verbs(busy);
            let width = buttons_width(ui, &verbs);
            let (share, leave) = (verbs[0].width(ui), verbs[1].width(ui));
            assert_eq!(title_row(ui, TUESDAY, &verbs), None);
            // From the right edge, less the gutter, in reading order.
            let right = 360.0 - SIDE;
            (
                pos2(right - width + share / 2.0, middle),
                pos2(right - leave / 2.0, middle),
            )
        });
        let nodes = output.accesskit_update.unwrap().nodes;
        let spoken = nodes
            .iter()
            .find(|(_, node)| node.label() == Some(busy))
            .map(|(_, node)| node.clone())
            .expect("Share says who shares");
        assert!(spoken.is_disabled());

        for (at, pressed) in [(share, None), (leave, Some(1))] {
            frame(&ctx, press(at), |ui| title_row(ui, TUESDAY, &verbs(busy)));
            let (got, _) = frame(&ctx, release(at), |ui| title_row(ui, TUESDAY, &verbs(busy)));
            assert_eq!(got, pressed, "{at:?}");
        }
        let enabled = || [Button::new("Share"), Button::new("Leave")];
        let (got, _) = frame(&ctx, press(share), |ui| title_row(ui, TUESDAY, &enabled()));
        assert_eq!(got, None);
        let (got, _) = frame(&ctx, release(share), |ui| {
            title_row(ui, TUESDAY, &enabled())
        });
        assert_eq!(got, Some(0));
    }

    // Leave on a room that has ended and Settings on the start screen stand
    // in the same place in the title row. Their ids come from their words,
    // so the keyboard focus Leave had does not pass to Settings.
    #[test]
    fn a_verb_in_the_same_place_is_another_widget() {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let node = |word: &str| {
            let (_, output) = frame(&ctx, Vec::new(), |ui| {
                title_row(ui, TUESDAY, &[Button::new(word)]);
            });
            let nodes = output.accesskit_update.unwrap().nodes;
            nodes
                .iter()
                .find(|(_, node)| node.label() == Some(word))
                .map(|(id, _)| *id)
                .expect("the verb is there")
        };
        let leave = node("Leave");
        assert_ne!(leave, node("Settings"));
        assert_eq!(leave, node("Leave"));
    }

    // The STUN list is as tall as its lines on the body's 18 px, with 7
    // above and below them as a one-line field has: no empty line under the
    // last, and an empty list the height of any field.
    #[test]
    fn a_list_field_is_as_tall_as_its_lines() {
        let ctx = Context::default();
        theme::apply(&ctx);
        for (text, height) in [
            ("", CONTROL_HEIGHT),
            ("stun.cloudflare.com:3478", CONTROL_HEIGHT),
            ("stun.cloudflare.com:3478\nstun.l.google.com:19302", 50.0),
            ("a:1\nb:2\nc:3", 68.0),
        ] {
            let mut text = String::from(text);
            let (rect, _) = frame(&ctx, Vec::new(), |ui| {
                lines_field(ui, "stun", &mut text, "STUN servers", 2048).rect
            });
            assert_eq!(rect.height(), height, "{text:?}");
        }
    }

    // Host and Join line up as one column whatever their words.
    #[test]
    fn primaries_are_at_least_80_wide() {
        let ctx = Context::default();
        theme::apply(&ctx);
        frame(&ctx, Vec::new(), |ui| {
            let host = Button::new("Host").role(Role::Primary);
            let join = Button::new("Join").role(Role::Primary);
            assert_eq!(host.width(ui), PRIMARY_WIDTH);
            assert_eq!(join.width(ui), PRIMARY_WIDTH);
            assert!(Button::new("Join").width(ui) < PRIMARY_WIDTH);
            let gear = Button::new("Settings").icon(theme::icons::regular::GEAR_SIX);
            let plain = Button::new("Settings");
            assert_eq!(gear.width(ui), plain.width(ui) + ICON_SIZE + STEP);
        });
    }

    // The arrow keys step by one, Page Up and Page Down by ten, Home and End
    // go to the ends, and the value never leaves them.
    #[test]
    fn slider_keys() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let id = Id::new("video upload");
        let mut value = 15;
        let step = |events: Vec<Event>, value: &mut u32| {
            frame(&ctx, events, |ui| {
                slider(ui, id, value, 1..=80, "Mbit/s", "Video upload").changed()
            })
            .0
        };
        step(Vec::new(), &mut value);
        ctx.memory_mut(|memory| memory.request_focus(id));
        // Focus arrives on this frame, and the arrows stay with the slider
        // from the next.
        step(Vec::new(), &mut value);
        let right = vec![
            key(Key::ArrowRight),
            key(Key::ArrowRight),
            key(Key::ArrowUp),
        ];
        assert!(step(right, &mut value));
        assert_eq!(value, 18);
        assert!(
            ctx.memory(|memory| memory.has_focus(id)),
            "the arrows kept it"
        );
        step(vec![key(Key::PageDown)], &mut value);
        assert_eq!(value, 8);
        step(vec![key(Key::ArrowLeft); 20], &mut value);
        assert_eq!(value, 1);
        step(vec![key(Key::End)], &mut value);
        assert_eq!(value, 80);
        assert!(!step(vec![key(Key::PageUp)], &mut value));
        assert_eq!(value, 80);
        step(vec![key(Key::Home)], &mut value);
        assert_eq!(value, 1);
    }

    #[test]
    fn slider_click() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let id = Id::new("video upload");
        let mut value = 15;
        let mut line = None;
        frame(&ctx, Vec::new(), |ui| {
            let response = slider(ui, id, &mut value, 1..=80, "Mbit/s", "Video upload");
            line = Some(response.rect);
        });
        let line = line.unwrap();
        let end = pos2(line.right() - 1.0, line.center().y);
        frame(&ctx, press(end), |ui| {
            slider(ui, id, &mut value, 1..=80, "Mbit/s", "Video upload")
        });
        assert_eq!(value, 80);
        frame(&ctx, release(end), |ui| {
            slider(ui, id, &mut value, 1..=80, "Mbit/s", "Video upload")
        });
        assert_eq!(value, 80);
        assert_eq!(
            value_at(line.left(), line.left() + 3.0, line.right() - 3.0, 1, 80),
            1
        );
        let middle = line.center().x;
        let at = value_at(middle, line.left() + 3.0, line.right() - 3.0, 1, 80);
        assert!((40..=41).contains(&at), "{at}");
    }

    // One device pixel until the scale reaches 200 percent, never a part of
    // one.
    #[test]
    fn edges_are_whole_pixels() {
        for per_point in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let rows = thickness(1.0, per_point) * per_point;
            assert!((rows - per_point.floor()).abs() < 1e-3, "{per_point}");
            let ring = thickness(RING_WIDTH, per_point) * per_point;
            assert!((ring - ring.round()).abs() < 1e-3, "{per_point}");
        }
    }
}
