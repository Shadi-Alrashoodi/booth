use eframe::egui::{
    Align, CursorIcon, Id, Key, Layout, Rect, Response, ScrollArea, Sense, Shape, Ui, UiBuilder,
    WidgetInfo, WidgetType, pos2, vec2,
};
use room::KnownHost;

use crate::controls::{self, Button, Lead};
use crate::messages;
use crate::theme::{
    self, ASH, BAD, CHALK, CONTROL_HEIGHT, FIELD_GAP, HALF_STEP, PANEL, Role, SECTION_GAP, SIDE,
    STEP,
};
use crate::update::{Shown, Tone};

const ROOM_NAME_CHARS: usize = 32;
// A real invite is about 160 characters. The cap is far above that, so a
// longer code from a later version is still recognised as one, but a log or
// a document pasted by mistake does not make every frame lay out and send
// all of it to a screen reader.
const PASTE_CHARS: usize = 2048;
// A name can be 253 characters; the rule, not the field, says no to more.
const ADDRESS_CHARS: usize = 512;

#[derive(Default)]
pub struct Start {
    pub room_name: String,
    pub paste: String,
    // The paste field's widget, so its undo history can be dropped with the
    // invite once a join has used it.
    pub paste_id: Option<Id>,
    pub host_error: Option<String>,
    pub join_error: Option<String>,
    // What came of the firewall prompt, above everything until a room opens.
    pub note: Option<Note>,
    // The hosts this PC joined before, newest first, as read when this
    // screen was made.
    pub known: Vec<KnownHost>,
    // The known host whose row is open, what is typed in its field, and
    // whether the field still has to take focus.
    pub opened: Option<[u8; 32]>,
    pub address: String,
    pub focus_address: bool,
    pub address_error: Option<String>,
    // Why the last rejoin or Forget did not work.
    pub known_error: Option<String>,
    // After Forget, for as long as this screen lasts.
    pub forgot: Option<String>,
}

pub struct Note {
    text: String,
    // Closing the prompt was the user's own choice, so it is not shown as
    // an error.
    failed: bool,
}

impl Note {
    pub fn said(text: &str) -> Note {
        Note {
            text: text.to_owned(),
            failed: false,
        }
    }

    pub fn failed(text: String) -> Note {
        Note { text, failed: true }
    }

    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.text
    }
}

pub enum Choice {
    Host,
    Join,
    Settings,
    // The newer release the update check found.
    Download,
    Rejoin([u8; 32]),
    SaveAddress([u8; 32]),
    Forget([u8; 32]),
}

impl Start {
    // Clicking a row opens it with its own address in the field; clicking
    // it again, or another, closes it.
    fn toggle(&mut self, key: [u8; 32]) {
        if self.opened == Some(key) {
            self.opened = None;
            return;
        }
        self.opened = Some(key);
        self.address = self
            .known
            .iter()
            .find(|host| *host.host_key() == key)
            .and_then(KnownHost::manual)
            .map(ToString::to_string)
            .unwrap_or_default();
        self.address_error = None;
        self.focus_address = true;
    }
}

// `blocked` is why this PC cannot host or join at all (the identity key could
// not be loaded); then there is nothing to offer but the reason, not even
// Settings, which would only apply to a room that cannot start. `lines` stay
// for the whole run, unlike the note. `update` is the update check's line,
// when it has one.
pub fn show(
    ui: &mut Ui,
    start: &mut Start,
    blocked: Option<&str>,
    lines: &[String],
    update: Option<&Shown>,
) -> Option<Choice> {
    let settings: Vec<Button> = blocked
        .is_none()
        .then(|| Button::new("Settings").icon(theme::icons::regular::GEAR_SIX))
        .into_iter()
        .collect();
    let settings_pressed = controls::title_row(ui, Lead::Mark, &settings).is_some();
    // Laid out whatever was pressed, so the frame that shows the press is
    // not an empty page. With a few known hosts, one of them open, the page
    // is taller than the window, so it scrolls under the title row.
    let scroll = ScrollArea::vertical().id_salt("start").auto_shrink(false);
    let choice = scroll
        .show(ui, |ui| {
            controls::page(ui, |ui| {
                if let Some(reason) = blocked {
                    controls::prose(ui, reason, theme::body(), BAD);
                    return None;
                }
                let mut choice = None;
                if notes(ui, start.note.as_ref(), lines, update) {
                    choice = Some(Choice::Download);
                }

                section(
                    ui,
                    "Host a room",
                    "Make an invite and send it to your friends.",
                );
                let (_, host) = field_row(ui, "Host", |ui| {
                    controls::field(
                        ui,
                        "room name",
                        &mut start.room_name,
                        "Room name (optional)",
                        theme::body(),
                        ROOM_NAME_CHARS,
                    )
                });
                if host {
                    choice = Some(Choice::Host);
                }
                error(ui, start.host_error.as_deref());

                ui.add_space(SECTION_GAP);
                section(ui, "Join a room", "Paste the invite you were sent.");
                let (paste, join) = field_row(ui, "Join", |ui| {
                    controls::field(
                        ui,
                        "invite",
                        &mut start.paste,
                        "Paste invite",
                        theme::mono(),
                        PASTE_CHARS,
                    )
                });
                start.paste_id = Some(paste.id);
                if paste.changed() {
                    start.join_error = None;
                }
                if join {
                    choice = Some(Choice::Join);
                }
                error(ui, start.join_error.as_deref());

                if let Some(known) = known_hosts(ui, start) {
                    choice = Some(known);
                }
                choice
            })
        })
        .inner;
    if settings_pressed {
        Some(Choice::Settings)
    } else {
        choice
    }
}

// What came of the firewall prompt, the lines that last for this run and the
// update check's line, as one block 8 px apart above the two sections. True
// when Download was pressed.
fn notes(ui: &mut Ui, note: Option<&Note>, lines: &[String], update: Option<&Shown>) -> bool {
    let mut said = false;
    let mut gap = |ui: &mut Ui| {
        if std::mem::replace(&mut said, true) {
            ui.add_space(STEP);
        }
    };
    if let Some(note) = note {
        gap(ui);
        let color = if note.failed { BAD } else { ASH };
        controls::prose(ui, note.text.as_str(), theme::body(), color);
    }
    for line in lines {
        gap(ui);
        controls::prose(ui, line.as_str(), theme::body(), ASH);
    }
    let mut download = false;
    if let Some(shown) = update {
        gap(ui);
        download = new_version(ui, shown);
    }
    if said {
        ui.add_space(SECTION_GAP);
    }
    download
}

// A field and the primary that acts on it, on one row 8 px apart, the field
// taking the rest of the width so Host and Join stand in one column at the
// right edge. Enter in the field presses the button. Returns the field and
// whether the button was pressed.
fn field_row(ui: &mut Ui, verb: &str, field: impl FnOnce(&mut Ui) -> Response) -> (Response, bool) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), CONTROL_HEIGHT), Sense::hover());
    let mut button = Button::new(verb).role(Role::Primary);
    let width = button.width(ui);
    let field_end = (rect.right() - width - STEP).max(rect.left());
    let mut left = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(rect.min, pos2(field_end, rect.bottom())))
            .layout(Layout::top_down(Align::Min)),
    );
    let field = field(&mut left);
    button = button.enter_target(field.has_focus());
    let mut right = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(rect.right() - width, rect.top()),
                rect.max,
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let pressed = button.show(&mut right).clicked() || entered(ui, &field);
    (field, pressed)
}

// The hosts joined before, only when there are any. A row joins from its
// Join button, or from Enter while it has focus; clicking anywhere else on
// it opens its address in place.
fn known_hosts(ui: &mut Ui, start: &mut Start) -> Option<Choice> {
    let shown = !start.known.is_empty() || start.forgot.is_some() || start.known_error.is_some();
    if !shown {
        return None;
    }
    ui.add_space(SECTION_GAP);
    let mut choice = None;
    if !start.known.is_empty() {
        controls::text(ui, "Known hosts", theme::section(), CHALK);
        ui.add_space(STEP);
    }
    let keys: Vec<[u8; 32]> = start.known.iter().map(|host| *host.host_key()).collect();
    for (i, key) in keys.into_iter().enumerate() {
        let opened = start.opened == Some(key);
        // Painted once the opened row's height is known, under what it holds.
        let tone = opened.then(|| (ui.painter().add(Shape::Noop), ui.cursor().top()));
        match host_row(ui, &start.known[i]) {
            Some(Row::Join) => choice = Some(Choice::Rejoin(key)),
            Some(Row::Toggle) => start.toggle(key),
            None => {}
        }
        if opened && let Some(picked) = address(ui, start, key) {
            choice = Some(picked);
        }
        if let Some((slot, top)) = tone {
            let column = ui.max_rect();
            let block = Rect::from_min_max(
                pos2(column.left() - SIDE, top),
                pos2(column.right() + SIDE, ui.cursor().top()),
            );
            ui.painter().set(slot, Shape::rect_filled(block, 0, PANEL));
        }
    }
    if let Some(forgot) = &start.forgot {
        if !start.known.is_empty() {
            ui.add_space(FIELD_GAP);
        }
        controls::prose(ui, forgot.as_str(), theme::body(), ASH);
    }
    error(ui, start.known_error.as_deref());
    choice
}

enum Row {
    Join,
    Toggle,
}

// Room name first and bigger, the host's fingerprint in a column of its own
// just before Join, and Join. Nothing changes under the mouse but the
// pointer.
fn host_row(ui: &mut Ui, host: &KnownHost) -> Option<Row> {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), CONTROL_HEIGHT), Sense::hover());
    let id = ui.id().with(("known host", host.host_key()));
    // Made before the button, which sits on top of it and keeps its own
    // clicks.
    let row = ui.interact(rect, id, Sense::click());
    let room = host.room_name().to_owned();
    row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &room));
    let focused = row.has_focus();
    let join = Button::new("Join").enter_target(focused);
    let fingerprint = keys::fingerprint(host.host_key());
    let print = theme::mono_caption();
    let print_width = controls::text_width(ui, &fingerprint, print.clone());
    let right = print_width + STEP + join.width(ui);
    let mut pressed = None;
    controls::split_row(
        ui,
        rect,
        right,
        |ui| {
            controls::one_line(ui, &room, theme::name(), CHALK);
        },
        |ui| {
            if join.show(ui).clicked() {
                pressed = Some(Row::Join);
            }
            controls::one_line(ui, &fingerprint, print, ASH);
        },
    );
    if focused && controls::keyboard_focus(ui) {
        controls::ring(ui, rect);
    }
    if pressed.is_some() {
        return pressed;
    }
    // Space opens the row, as a click does, and Enter joins.
    if focused && ui.input(|input| input.key_pressed(Key::Enter)) {
        return Some(Row::Join);
    }
    let row = row.on_hover_cursor(CursorIcon::PointingHand);
    row.clicked().then_some(Row::Toggle)
}

// The opened row: the address or name typed for it with its label above,
// then Save, the field's Enter target, and Forget.
fn address(ui: &mut Ui, start: &mut Start, key: [u8; 32]) -> Option<Choice> {
    ui.add_space(STEP);
    controls::text(ui, messages::ADDRESS_OR_NAME, theme::body(), ASH);
    ui.add_space(STEP);
    let field: Response = controls::field(
        ui,
        "known host address",
        &mut start.address,
        messages::MANUAL_HINT,
        theme::mono(),
        ADDRESS_CHARS,
    );
    ui.ctx()
        .accesskit_node_builder(field.id, |node| node.set_label(messages::ADDRESS_OR_NAME));
    let just_opened = std::mem::take(&mut start.focus_address);
    if just_opened {
        field.request_focus();
        // Focus shows from the next frame, and nothing else would ask for it.
        ui.ctx().request_repaint();
    }
    if field.changed() {
        start.address_error = None;
    }
    error(ui, start.address_error.as_deref());
    ui.add_space(FIELD_GAP);
    let mut choice = None;
    ui.horizontal(|ui| {
        let save = Button::new("Save").enter_target(field.has_focus()).show(ui);
        if save.clicked() || entered(ui, &field) {
            choice = Some(Choice::SaveAddress(key));
        }
        if Button::new("Forget")
            .role(Role::Destructive)
            .show(ui)
            .clicked()
        {
            choice = Some(Choice::Forget(key));
        }
    });
    if just_opened {
        let bottom = pos2(field.rect.right(), ui.cursor().top());
        ui.scroll_to_rect(Rect::from_min_max(field.rect.min, bottom), None);
    }
    ui.add_space(FIELD_GAP);
    choice
}

// The update check's line, and Download under it when there is a release
// to fetch. News is in chalk, a check that could not be made in ash, and a
// download that failed in the error colour. True when Download was pressed.
fn new_version(ui: &mut Ui, shown: &Shown) -> bool {
    let color = match shown.tone {
        Tone::Plain => CHALK,
        Tone::Quiet => ASH,
        Tone::Problem => BAD,
    };
    controls::prose(ui, shown.text.as_str(), theme::body(), color);
    if !shown.download {
        return false;
    }
    ui.add_space(STEP);
    Button::new("Download").show(ui).clicked()
}

// The head one step up in Medium chalk, the sentence under it in Regular ash:
// the two differ in size, weight and colour at once.
fn section(ui: &mut Ui, name: &str, line: &str) {
    controls::text(ui, name, theme::section(), CHALK);
    ui.add_space(STEP);
    controls::prose(ui, line, theme::body(), ASH);
    ui.add_space(STEP);
}

// Under the field it belongs to, 4 px down, in caption.
fn error(ui: &mut Ui, text: Option<&str>) {
    if let Some(text) = text {
        ui.add_space(HALF_STEP);
        controls::prose(ui, text, theme::caption(), BAD);
    }
}

// A single-line field gives up focus on Enter, so that pair is the press.
fn entered(ui: &Ui, field: &Response) -> bool {
    field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::accesskit::Role;
    use eframe::egui::{Context, RawInput, pos2};

    // What a screen reader is given for the start screen: each node's role
    // and its words.
    fn spoken(update: Option<&Shown>) -> Vec<(Role, String)> {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 640.0))),
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let choice = show(ui, &mut Start::default(), None, &[], update);
            assert!(choice.is_none());
        });
        let nodes = std::mem::take(&mut output.platform_output)
            .accesskit_update
            .expect("accesskit is on")
            .nodes;
        output.drop_without_applying_deltas();
        nodes
            .iter()
            .map(|(_, node)| {
                let words = node.label().or(node.value()).unwrap_or_default();
                (node.role(), words.to_owned())
            })
            .collect()
    }

    fn has(nodes: &[(Role, String)], role: Role, words: &str) -> bool {
        nodes.iter().any(|(r, w)| *r == role && w == words)
    }

    #[test]
    fn newer_release_line() {
        let newer = Shown {
            text: String::from("Booth 0.2.0 is out. You have 0.1.0."),
            tone: Tone::Plain,
            download: true,
        };
        let nodes = spoken(Some(&newer));
        assert!(
            nodes.iter().any(|(_, words)| *words == newer.text),
            "{nodes:?}"
        );
        assert!(has(&nodes, Role::Button, "Download"), "{nodes:?}");

        let failed = Shown {
            text: String::from("Could not check for a new version."),
            tone: Tone::Quiet,
            download: false,
        };
        let nodes = spoken(Some(&failed));
        assert!(
            nodes.iter().any(|(_, words)| *words == failed.text),
            "{nodes:?}"
        );
        assert!(!has(&nodes, Role::Button, "Download"), "{nodes:?}");

        let nodes = spoken(None);
        assert!(!has(&nodes, Role::Button, "Download"), "{nodes:?}");
        assert!(has(&nodes, Role::Button, "Host"), "{nodes:?}");
    }
}
