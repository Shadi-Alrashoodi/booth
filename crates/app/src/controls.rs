use std::ops::RangeInclusive;

use eframe::egui::emath::GuiRounding;
use eframe::egui::{
    Align, Color32, Context, CursorIcon, Event, EventFilter, FontId, Frame, Id, Key,
    KeyboardShortcut, Label, Layout, Margin, Mesh, Modifiers, Painter, Rect, Response, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder, Vec2, WidgetInfo, WidgetType, accesskit,
    pos2, vec2,
};

use crate::theme::{
    self, AMBER, CHALK, CONTROL_HEIGHT, LINE, LINE_STRONG, PANEL, ROW_HEIGHT, SIDE,
};

const TEXT_PAD: f32 = 12.0;
// The slider's line is two points of line strong, the colour of anything
// found by its edge, filled in chalk up to the handle, a chalk bar as tall as
// a line of text.
const TRACK: f32 = 2.0;
const HANDLE: Vec2 = vec2(6.0, 16.0);
const COMPOSER_ROWS: f32 = 4.0;
const FIELD_PAD: i8 = 10;
const RING_WIDTH: f32 = 2.0;
const RING_GAP: f32 = 1.0;
const KEYBOARD_FOCUS: &str = "focus came from the keyboard";

// The room name on the left, the verbs for the whole room on the right in
// reading order, and a hairline under it. The row is as tall as its buttons.
// Returns which verb was pressed; one that is not enabled never is.
pub fn title_row(ui: &mut Ui, title: &str, verbs: &[Button]) -> Option<usize> {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), CONTROL_HEIGHT), Sense::hover());
    // Under the buttons, so their own borders are the edge they sit on.
    hairline(ui, rect.bottom());
    // The row is its own clip, so a focus ring on a verb stays inside the
    // window at the top and above the region below, which paints later.
    let mut row = ui.new_child(UiBuilder::new().max_rect(rect));
    row.set_clip_rect(rect.intersect(ui.clip_rect()));
    let width = buttons_width(&row, verbs);
    let mut pressed = None;
    split_row(
        &mut row,
        rect.shrink2(vec2(SIDE, 0.0)),
        width,
        |ui| {
            one_line(ui, title, theme::title(), CHALK);
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
// which lays out from the right edge. Returns which one was pressed.
pub fn buttons(ui: &mut Ui, buttons: &[Button], height: f32) -> Option<usize> {
    let width = buttons_width(ui, buttons);
    let layout = Layout::left_to_right(Align::Center);
    let mut pressed = None;
    ui.allocate_ui_with_layout(vec2(width, height), layout, |ui| {
        for (i, button) in buttons.iter().enumerate() {
            if button.show(ui).clicked() && button.enabled {
                pressed = Some(i);
            }
        }
    });
    pressed
}

// One row with a name on the left and one thing `right_width` wide at the
// right edge. The width is known up front so the left side is laid out
// first, in reading order for Tab and screen readers, and is what gets cut
// when the row is narrow.
pub fn split_row(
    ui: &mut Ui,
    rect: Rect,
    right_width: f32,
    left: impl FnOnce(&mut Ui),
    right: impl FnOnce(&mut Ui),
) {
    let left_end = if right_width > 0.0 {
        rect.right() - right_width - SIDE
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

// The body of a screen: the side gutter, and room above the first line.
pub fn page<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .inner_margin(Margin {
            left: SIDE as i8,
            right: SIDE as i8,
            top: 16,
            bottom: 16,
        })
        .show(ui, add)
        .inner
}

// A 1 px line in the line colour whose bottom edge is at `bottom`.
pub fn hairline(ui: &Ui, bottom: f32) {
    let per_point = ui.pixels_per_point();
    let rect = ui.max_rect();
    let bottom = bottom.round_to_pixels(per_point);
    let line = Rect::from_min_max(
        pos2(rect.left(), bottom - thickness(1.0, per_point)),
        pos2(rect.right(), bottom),
    );
    fill_pixels(ui.painter(), &[line.round_to_pixels(per_point)], LINE);
}

// A border `width` points thick inside `rect`.
pub fn border(painter: &Painter, rect: Rect, width: f32, color: Color32) {
    let per_point = painter.pixels_per_point();
    fill_pixels(painter, &border_rects(rect, width, per_point), color);
}

// The four sides of a border on whole device pixels. The width is rounded
// down to whole pixels and never under one, so at 150 percent a 1 px line is
// one crisp row in its own colour instead of two rows in a colour between.
// egui's own strokes land wherever the maths puts them.
pub fn border_rects(rect: Rect, width: f32, per_point: f32) -> [Rect; 4] {
    let outer = rect.round_to_pixels(per_point);
    let t = thickness(width, per_point);
    let (top, bottom) = (outer.top() + t, outer.bottom() - t);
    [
        Rect::from_min_max(outer.min, pos2(outer.right(), top)),
        Rect::from_min_max(pos2(outer.left(), bottom), outer.max),
        Rect::from_min_max(pos2(outer.left(), top), pos2(outer.left() + t, bottom)),
        Rect::from_min_max(pos2(outer.right() - t, top), pos2(outer.right(), bottom)),
    ]
}

fn thickness(width: f32, per_point: f32) -> f32 {
    (width * per_point).floor().max(1.0) / per_point
}

// As one mesh, because egui feathers the edges of a filled rect and on a
// line one pixel thick the feathering is most of the line.
pub fn fill_pixels(painter: &Painter, rects: &[Rect], color: Color32) {
    let mut mesh = Mesh::default();
    for rect in rects {
        mesh.add_colored_rect(*rect, color);
    }
    painter.add(mesh);
}

// egui's focus look is its pressed look, so the ring is drawn here: 2 px,
// one pixel clear of the control's own border. Where the clip would cut it
// (a button that fills the title row) it moves in rather than vanish.
pub fn ring(ui: &Ui, rect: Rect, color: Color32) {
    let outer = rect.expand(RING_GAP + RING_WIDTH).intersect(ui.clip_rect());
    border(ui.painter(), outer, RING_WIDTH, color);
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
    color: Color32,
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
            color: CHALK,
            height: CONTROL_HEIGHT,
            min_width: 0.0,
            enter_target: false,
            enabled: true,
        }
    }

    pub fn color(mut self, color: Color32) -> Button<'a> {
        self.color = color;
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

    // Set while a field has focus whose Enter presses this button, so the
    // button carries the amber ring that says so.
    pub fn enter_target(mut self, yes: bool) -> Button<'a> {
        self.enter_target = yes;
        self
    }

    pub fn width(&self, ui: &Ui) -> f32 {
        (text_width(ui, self.text, theme::body()) + 2.0 * TEXT_PAD)
            .max(self.height)
            .max(self.min_width)
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let galley = ui
            .painter()
            .layout_no_wrap(self.text.to_owned(), theme::body(), self.color);
        let width = self.width(ui);
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), Sense::click());
        let label = self.label.unwrap_or(self.text);
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, label));
        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            border(painter, rect, 1.0, LINE_STRONG);
            let at = (rect.center() - galley.size() / 2.0).round();
            painter.galley(at, galley, self.color);
            if (response.has_focus() && keyboard_focus(ui)) || self.enter_target {
                ring(ui, rect, AMBER);
            }
        }
        if self.enabled {
            response.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            response
        }
    }
}

// A whole number picked along a line, with the number and its unit written
// at the right, since the line alone does not say how much. Drag or click
// on the line; with focus, the arrow keys step by one, Page Up and Page Down
// by ten, Home and End go to the ends. The row is ROW_HEIGHT tall, and all
// of it left of the number is the target.
pub fn slider(
    ui: &mut Ui,
    id: Id,
    value: &mut u32,
    range: RangeInclusive<u32>,
    unit: &str,
    label: &str,
) -> Response {
    let (lowest, highest) = (*range.start(), (*range.end()).max(*range.start()));
    let widest = text_width(ui, &format!("{highest} {unit}"), theme::body());
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
    fill_pixels(
        ui.painter(),
        &[line(track_rect.left(), track_rect.right())],
        LINE_STRONG,
    );
    fill_pixels(ui.painter(), &[line(track_rect.left(), at)], CHALK);
    let handle =
        Rect::from_center_size(pos2(at, rect.center().y), HANDLE).round_to_pixels(per_point);
    fill_pixels(ui.painter(), &[handle], CHALK);
    let shown = format!("{next} {unit}");
    let mut text = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(track_rect.right(), rect.top()),
                rect.max,
            ))
            .layout(Layout::right_to_left(Align::Center)),
    );
    one_line(&mut text, &shown, theme::body(), CHALK);
    if response.has_focus() && keyboard_focus(ui) {
        ring(ui, track_rect, LINE_STRONG);
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
        .hint_text(RichText::new(hint).font(theme::body()))
        .font(font)
        .text_color(CHALK)
        .frame(
            Frame::new()
                .fill(PANEL)
                // Keeps the room for the border, which is drawn below on
                // whole pixels.
                .stroke(Stroke::new(1.0, Color32::TRANSPARENT))
                .inner_margin(Margin::symmetric(FIELD_PAD, 0)),
        )
        .vertical_align(Align::Center)
        .desired_width(f32::INFINITY)
        .min_size(vec2(0.0, CONTROL_HEIGHT))
        .show(ui);
    let response = output.response.response;
    border(ui.painter(), response.rect, 1.0, LINE_STRONG);
    // A screen reader reads the placeholder as the field's name, since
    // there is no label above it.
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(hint));
    // A field shows its ring on a click too: the caret alone is easy to lose.
    if response.has_focus() {
        ring(ui, response.rect, LINE_STRONG);
    }
    response
}

// A field for a short list, one entry per line. Enter starts a new line, so
// nothing is pressed by it. `label` is what a screen reader calls it.
pub fn lines_field(
    ui: &mut Ui,
    id: &str,
    text: &mut String,
    label: &str,
    rows: usize,
    char_limit: usize,
) -> Response {
    let font = theme::body();
    let output = TextEdit::multiline(text)
        .id_salt(id)
        .char_limit(char_limit)
        .font(font.clone())
        .text_color(CHALK)
        .frame(
            Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, Color32::TRANSPARENT))
                .inner_margin(Margin::symmetric(FIELD_PAD, 7)),
        )
        .desired_width(f32::INFINITY)
        .desired_rows(rows)
        .show(ui);
    let response = output.response.response;
    border(ui.painter(), response.rect, 1.0, LINE_STRONG);
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(label));
    if response.has_focus() {
        ring(ui, response.rect, LINE_STRONG);
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

// The chat's field, the width of the panel: one line that grows to four and
// then scrolls. Shift+Enter starts a new line; plain Enter is left to the
// caller, which sends. Tab still moves on, as in any other field. What is
// typed sits at the right edge while it starts with a right-to-left letter,
// the way the message will show in the chat.
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
    let frame = Frame::new().fill(PANEL).inner_margin(Margin {
        left: SIDE as i8,
        right: SIDE as i8,
        top: pad as i8,
        bottom: pad as i8,
    });
    let shown = frame.show(ui, |ui| {
        ui.set_min_height(CONTROL_HEIGHT - 2.0 * pad);
        ScrollArea::vertical()
            .id_salt(id)
            .max_height(COMPOSER_ROWS * row)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                TextEdit::multiline(text)
                    .id_salt(id)
                    .char_limit(char_limit)
                    .hint_text(RichText::new(hint).font(theme::body()))
                    .font(font)
                    .text_color(CHALK)
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
    let outer = shown.response.rect;
    border(ui.painter(), outer, 1.0, LINE_STRONG);
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(hint));
    if response.has_focus() {
        ring(ui, outer, LINE_STRONG);
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

// The type scale's line height for each size.
pub fn line_height(font: &FontId) -> f32 {
    if font.size >= 15.0 {
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
            Button::new("Leave"),
        ]
    }

    // Share and Leave at the right of the title row in that order. While
    // someone else shares, Share stays in ash, a click on it does nothing,
    // and a screen reader hears who shares.
    #[test]
    fn title_row_verbs() {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let busy = "Share. Ines is sharing. One share at a time.";
        let ((share, leave), output) = frame(&ctx, Vec::new(), |ui| {
            let verbs = verbs(busy);
            let width = buttons_width(ui, &verbs);
            let (share, leave) = (verbs[0].width(ui), verbs[1].width(ui));
            assert_eq!(title_row(ui, "Tuesday night", &verbs), None);
            // From the right edge, less the gutter, in reading order.
            let right = 360.0 - SIDE;
            (
                pos2(right - width + share / 2.0, 16.0),
                pos2(right - leave / 2.0, 16.0),
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
            frame(&ctx, press(at), |ui| {
                title_row(ui, "Tuesday night", &verbs(busy))
            });
            let (got, _) = frame(&ctx, release(at), |ui| {
                title_row(ui, "Tuesday night", &verbs(busy))
            });
            assert_eq!(got, pressed, "{at:?}");
        }
        let (enabled, _) = frame(&ctx, press(share), |ui| {
            title_row(
                ui,
                "Tuesday night",
                &[Button::new("Share"), Button::new("Leave")],
            )
        });
        assert_eq!(enabled, None);
        let (enabled, _) = frame(&ctx, release(share), |ui| {
            title_row(
                ui,
                "Tuesday night",
                &[Button::new("Share"), Button::new("Leave")],
            )
        });
        assert_eq!(enabled, Some(0));
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

    #[test]
    fn borders_land_on_whole_pixels() {
        for per_point in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let rect = Rect::from_min_max(pos2(12.3, 40.7), pos2(80.1, 72.7));
            for side in border_rects(rect, 1.0, per_point) {
                for edge in [side.left(), side.right(), side.top(), side.bottom()] {
                    let px = edge * per_point;
                    assert!((px - px.round()).abs() < 1e-3, "{per_point}: {edge}");
                }
            }
            // One device pixel thick until the scale reaches 200 percent.
            let top = border_rects(rect, 1.0, per_point)[0];
            let rows = top.height() * per_point;
            assert!((rows - per_point.floor()).abs() < 1e-3, "{per_point}");
        }
    }
}
