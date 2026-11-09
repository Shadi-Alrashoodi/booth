use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{
    Align, Color32, Context, CursorIcon, Event, Id, Key, Layout, Modifiers, PointerButton, Rect,
    Response, ScrollArea, Sense, Shape, Ui, UiBuilder, WidgetInfo, WidgetType, pos2, vec2,
};
use invite::{CodeError, Invite};
use room::KnownHost;

use crate::controls::{self, Button, Lead};
use crate::messages;
use crate::theme::{
    self, ASH, BAD, CHALK, CONTROL_HEIGHT, FIELD_GAP, HALF_STEP, PANEL, PRIMARY_WIDTH, Role,
    SECTION_GAP, SIDE, STEP, WARN,
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
    // invite once a room opens.
    pub paste_id: Option<Id>,
    pub host_error: Option<String>,
    pub join_error: Option<String>,
    // What came of the firewall prompt, above everything until a room opens.
    pub note: Option<Note>,
    // The hosts this PC joined before, newest first, as read when this
    // screen was made.
    pub known: Vec<KnownHost>,
    // The known host whose row is open, when it opened or this screen came
    // back from Settings, what is typed in its field, and whether the field
    // still has to take focus.
    pub opened: Option<[u8; 32]>,
    pub opened_at: Option<Instant>,
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
    JoinLink,
    DismissLink,
}

pub enum Waiting {
    Shown {
        invite: Invite,
        wait: LinkWait,
        error: Option<String>,
        dead: bool,
    },
    Refused(String),
}

impl Waiting {
    pub fn restart(&mut self) {
        if let Waiting::Shown { wait, .. } = self {
            wait.restart();
        }
    }
}

#[derive(Default)]
pub struct LinkWait {
    since: Option<Instant>,
    pointer_left: bool,
}

impl LinkWait {
    pub fn restart(&mut self) {
        *self = Self::default();
    }

    pub fn follow(&mut self, active: bool, now: Instant, delay: Duration) -> Option<Duration> {
        if !active {
            self.restart();
            return Some(delay);
        }
        let since = *self.since.get_or_insert(now);
        let left = delay
            .checked_sub(now.saturating_duration_since(since))
            .filter(|left| !left.is_zero());
        if left.is_some() {
            self.pointer_left = false;
        }
        left
    }

    // Only an earlier frame counts: a release and a move off Join can come in
    // one frame, and the click was made before the move.
    fn pointer_ready(&mut self, ui: &Ui, rect: Rect) -> bool {
        let ready = self.pointer_left;
        // egui hands a press to the nearest widget within interact_radius, so
        // a pointer that close is still on Join.
        let reach = rect.expand(ui.style().interaction.interact_radius);
        if ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_none_or(|pos| !reach.contains(pos))
        }) {
            self.pointer_left = true;
        }
        ready
    }
}

pub struct LinkPage<'a> {
    pub waiting: &'a mut Waiting,
    pub delay: Duration,
    pub open_mic: bool,
}

pub fn link_focused(ctx: &Context) -> bool {
    ctx.input(|input| {
        let view = input.viewport();
        view.focused == Some(true)
            && view.minimized != Some(true)
            && !input
                .events
                .iter()
                .any(|event| matches!(event, Event::WindowFocused(false)))
    })
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Start {
    // Clicking a row opens it with its own address in the field; clicking
    // it again, or another, closes it.
    fn toggle(&mut self, key: [u8; 32], now: Instant) {
        if self.opened == Some(key) {
            self.opened = None;
            return;
        }
        self.opened = Some(key);
        self.opened_at = Some(now);
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

    // A room that could not open may have said to change the port, with the
    // old one in the sentence. Host or Join says it again if it still holds.
    pub fn settings_changed(&mut self) {
        self.host_error = None;
        self.join_error = None;
        self.known_error = None;
    }
}

// `blocked` is why this PC cannot host or join at all (the identity key could
// not be loaded); then there is nothing to offer but the reason, not even
// Settings, which would only apply to a room that cannot start. `on_cpu` and
// `lines` stay for the whole run, unlike the note. `update` is the update
// check's line, when it has one.
pub fn show(
    ui: &mut Ui,
    start: &mut Start,
    blocked: Option<&str>,
    on_cpu: Option<&str>,
    lines: &[String],
    update: Option<&Shown>,
    link: Option<LinkPage<'_>>,
) -> Option<Choice> {
    let settings: Vec<Button> = blocked
        .is_none()
        .then(|| Button::new("Settings").icon(theme::icons::regular::GEAR_SIX))
        .into_iter()
        .collect();
    let settings_pressed = controls::title_row(ui, Lead::Empty, &settings).is_some();
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
                if let Some(page) = link {
                    return show_link(ui, &start.known, page, Instant::now(), unix_now());
                }
                let mut choice = None;
                if notes(ui, start.note.as_ref(), on_cpu, lines, update) {
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
                field_error(ui, "Host", start.host_error.as_deref());

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
                field_error(ui, "Join", start.join_error.as_deref());

                if let Some(known) = known_hosts(ui, start, Instant::now()) {
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

fn show_link(
    ui: &mut Ui,
    known: &[KnownHost],
    page: LinkPage<'_>,
    now: Instant,
    unix: u64,
) -> Option<Choice> {
    // Enter may still be held from the program that opened the link.
    ui.input_mut(|input| {
        input.events.retain(|event| {
            !matches!(
                event,
                Event::Key {
                    key: Key::Enter,
                    ..
                }
            )
        })
    });
    if ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
        return Some(Choice::DismissLink);
    }
    let Waiting::Shown {
        invite,
        wait,
        error,
        ..
    } = page.waiting
    else {
        if let Waiting::Refused(reason) = page.waiting {
            controls::text(ui, messages::LINK_REFUSED, theme::section(), CHALK);
            ui.add_space(STEP);
            controls::prose(ui, reason.as_str(), theme::body(), CHALK);
            ui.add_space(STEP);
            if controls::buttons(ui, &[Button::new("Back")], CONTROL_HEIGHT).is_some() {
                return Some(Choice::DismissLink);
            }
        }
        return None;
    };
    let active = link_focused(ui.ctx());
    let left = wait.follow(active, now, page.delay);
    let expired = invite.is_expired(unix);
    if let Some(left) = left {
        ui.ctx().request_repaint_after(left);
    }
    if !expired {
        let expiry = UNIX_EPOCH + Duration::from_secs(invite.expires_at);
        ui.ctx()
            .request_repaint_after(expiry.duration_since(SystemTime::now()).unwrap_or_default());
    }
    controls::text(ui, messages::LINK_HEAD, theme::section(), CHALK);
    ui.add_space(STEP);
    let room = known
        .iter()
        .find(|host| host.host_key() == &invite.host_key)
        .map(KnownHost::room_name);
    controls::prose(ui, messages::link_from(room), theme::body(), CHALK);
    ui.add_space(STEP);
    controls::text(
        ui,
        keys::fingerprint(&invite.host_key),
        theme::mono_caption(),
        ASH,
    );
    ui.add_space(SIDE);
    controls::prose(ui, messages::LINK_ABOUT, theme::caption(), ASH);
    ui.add_space(STEP);
    // Talk mode must not move Join under a pointer when Settings closes.
    ui.scope(|ui| {
        ui.set_min_height(controls::prose_height(
            ui,
            messages::LINK_OPEN_MIC,
            theme::caption(),
        ));
        if page.open_mic {
            controls::prose(ui, messages::LINK_OPEN_MIC, theme::caption(), WARN);
        }
    });
    ui.add_space(STEP);
    let enabled = left.is_none() && !expired;
    let mut choice = None;
    ui.horizontal(|ui| {
        let join = ui
            .push_id("link join", |ui| {
                Button::new("Join")
                    .label("Join this room")
                    .enabled(enabled)
                    .show(ui)
            })
            .inner;
        let pointer_ready = enabled && wait.pointer_ready(ui, join.rect);
        if controls::button_pressed(ui, &join, enabled)
            && (!join.clicked_by(PointerButton::Primary) || pointer_ready)
        {
            choice = Some(Choice::JoinLink);
        }
        if controls::buttons(ui, &[Button::new("Not now")], CONTROL_HEIGHT).is_some() {
            choice = Some(Choice::DismissLink);
        }
    });
    if expired {
        under_field(ui, &messages::code_error(&CodeError::Expired), ASH);
    } else if let Some(error) = error {
        under_field(ui, error, BAD);
    }
    choice
}

// What came of the firewall prompt, the lines that last for this run and the
// update check's line, as one block 8 px apart above the two sections. True
// when Download was pressed.
fn notes(
    ui: &mut Ui,
    note: Option<&Note>,
    on_cpu: Option<&str>,
    lines: &[String],
    update: Option<&Shown>,
) -> bool {
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
    if let Some(line) = on_cpu {
        gap(ui);
        controls::prose(ui, line, theme::body(), WARN);
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
fn known_hosts(ui: &mut Ui, start: &mut Start, now: Instant) -> Option<Choice> {
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
            Some(Row::Toggle) => start.toggle(key, now),
            None => {}
        }
        if opened && let Some(picked) = address(ui, start, key, now) {
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
    // As wide as Host and Join above it, so its word stands in their column.
    let join = Button::new("Join")
        .enter_target(focused)
        .min_width(PRIMARY_WIDTH);
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

// The opened row: the address or name typed for it with its label above and
// an example under it, then Save, the field's Enter target, and Forget. The
// example is a help line rather than a placeholder, which the field cut short
// at the default width; the label already names the field. The name row
// stays on the block's top edge, so opening a row moves nothing, and Save
// sits 8 above the block's bottom to match. A row opened while one above it is
// open does move up as that one closes, and its Save and Forget come to lie
// where it was, under a second click, so they wait until the list has settled.
fn address(ui: &mut Ui, start: &mut Start, key: [u8; 32], now: Instant) -> Option<Choice> {
    ui.add_space(STEP);
    controls::text(ui, messages::ADDRESS_OR_NAME, theme::body(), ASH);
    ui.add_space(STEP);
    let field: Response = controls::field(
        ui,
        "known host address",
        &mut start.address,
        "",
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
    match start.address_error.as_deref() {
        Some(text) => under_field(ui, text, BAD),
        None => under_field(ui, messages::MANUAL_HELP, ASH),
    }
    ui.add_space(FIELD_GAP);
    let settled = controls::settled(ui, start.opened_at, now);
    // Save is the field's Enter target, and shows it with the control fill
    // for as long as the row is open once it has settled, not only while the
    // field has focus: otherwise it is a box while typing and then a lone
    // word, set in from the field above it, once focus moves on.
    let save = Button::new("Save").on(true).enabled(settled);
    let forget = Button::new("Forget")
        .role(Role::Destructive)
        .enabled(settled);
    let mut choice = match controls::buttons(ui, &[save, forget], CONTROL_HEIGHT) {
        Some(0) => Some(Choice::SaveAddress(key)),
        Some(_) => Some(Choice::Forget(key)),
        None => None,
    };
    // A key press in the field does not wait: nothing moves under a key.
    if entered(ui, &field) {
        choice = Some(Choice::SaveAddress(key));
    }
    if just_opened {
        let bottom = pos2(field.rect.right(), ui.cursor().top());
        ui.scroll_to_rect(Rect::from_min_max(field.rect.min, bottom), None);
    }
    ui.add_space(STEP);
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
        under_field(ui, text, BAD);
    }
}

// A help line in ash or an error in bad, in the same place.
fn under_field(ui: &mut Ui, text: &str, color: Color32) {
    ui.add_space(HALF_STEP);
    controls::prose(ui, text, theme::caption(), color);
}

// The same under the field of a field row, wrapped at the field's width, so
// it stays under the field and does not run on under the `verb` beside it.
fn field_error(ui: &mut Ui, verb: &str, text: Option<&str>) {
    if text.is_none() {
        return;
    }
    let button = Button::new(verb).role(Role::Primary).width(ui);
    let width = (ui.available_width() - button - STEP).max(0.0);
    ui.scope(|ui| {
        ui.set_max_width(width);
        error(ui, text);
    });
}

// A single-line field gives up focus on Enter, so that pair is the press.
fn entered(ui: &Ui, field: &Response) -> bool {
    field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use eframe::egui::accesskit::Role;
    use eframe::egui::{Context, Event, Modifiers, PlatformOutput, RawInput, pos2};
    use std::time::Duration;

    use crate::controls::tests::{frame, press, release};
    use crate::screens::room::tests::{test_friend, test_room};

    fn waiting() -> Waiting {
        Waiting::Shown {
            invite: Invite::new(
                [7; 32],
                Vec::new(),
                invite::Mapping::Easy,
                false,
                false,
                false,
                None,
                false,
                unix_now(),
            )
            .unwrap(),
            wait: LinkWait::default(),
            error: None,
            dead: false,
        }
    }

    struct LinkDraw {
        ctx: Context,
        at: Instant,
        focus: Option<bool>,
        minimized: bool,
        mic: bool,
        unix: u64,
    }

    impl LinkDraw {
        fn new() -> Self {
            let ctx = Context::default();
            theme::apply(&ctx);
            ctx.enable_accesskit();
            Self {
                ctx,
                at: Instant::now(),
                focus: Some(true),
                minimized: false,
                mic: false,
                unix: unix_now(),
            }
        }

        fn draw(
            &self,
            waiting: &mut Waiting,
            ms: u64,
            events: Vec<Event>,
        ) -> (Option<Choice>, PlatformOutput) {
            let mut input = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 640.0))),
                events,
                focused: self.focus == Some(true),
                ..RawInput::default()
            };
            let viewport = input
                .viewports
                .get_mut(&eframe::egui::ViewportId::ROOT)
                .unwrap();
            viewport.focused = self.focus;
            viewport.minimized = Some(self.minimized);
            let mut choice = None;
            let mut output = self.ctx.run_ui(input, |ui| {
                choice = controls::page(ui, |ui| {
                    show_link(
                        ui,
                        &[],
                        LinkPage {
                            waiting,
                            delay: Duration::from_secs(1),
                            open_mic: self.mic,
                        },
                        self.at + Duration::from_millis(ms),
                        self.unix,
                    )
                });
            });
            let platform = std::mem::take(&mut output.platform_output);
            output.drop_without_applying_deltas();
            (choice, platform)
        }
    }

    fn join_off(output: &PlatformOutput) -> bool {
        output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Join this room"))
            .expect("Join stays in the tree")
            .1
            .is_disabled()
    }

    fn key_event(key: Key) -> Vec<Event> {
        vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]
    }

    #[test]
    fn link_page_replaces_the_start() {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let mut start = Start {
            known: two_known_hosts("link-start"),
            paste: String::from("booth1-private"),
            ..Start::default()
        };
        let mut link = waiting();
        let (choice, output) = frame(&ctx, Vec::new(), |ui| {
            show(
                ui,
                &mut start,
                None,
                None,
                &[String::from("a note")],
                None,
                Some(LinkPage {
                    waiting: &mut link,
                    delay: Duration::from_secs(1),
                    open_mic: false,
                }),
            )
        });
        assert!(choice.is_none());
        let nodes = output.accesskit_update.unwrap().nodes;
        assert!(
            nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Join this room"))
        );
        for words in [
            "Host",
            "Known hosts",
            "Paste invite",
            "booth1-private",
            "a note",
            "first",
            "second",
        ] {
            assert!(
                !nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(words) || node.value() == Some(words)),
                "{words}"
            );
        }
        assert!(!nodes.iter().any(|(_, node)| node.role() == Role::TextInput));
    }

    #[test]
    fn link_join_waits_for_focus() {
        let mut draw = LinkDraw::new();
        let mut link = waiting();
        assert!(join_off(&draw.draw(&mut link, 0, Vec::new()).1));
        assert!(join_off(&draw.draw(&mut link, 999, Vec::new()).1));
        assert!(!join_off(&draw.draw(&mut link, 1000, Vec::new()).1));
        for (focus, minimized) in [(None, false), (Some(false), false), (Some(true), true)] {
            draw.focus = focus;
            draw.minimized = minimized;
            assert!(join_off(&draw.draw(&mut link, 2000, Vec::new()).1));
            draw.focus = Some(true);
            draw.minimized = false;
            assert!(join_off(&draw.draw(&mut link, 3000, Vec::new()).1));
            assert!(!join_off(&draw.draw(&mut link, 4000, Vec::new()).1));
        }
        link.restart();
        assert!(join_off(&draw.draw(&mut link, 5000, Vec::new()).1));
        assert!(join_off(&draw.draw(&mut link, 5999, Vec::new()).1));
        assert!(!join_off(&draw.draw(&mut link, 6000, Vec::new()).1));
        assert!(join_off(
            &draw
                .draw(
                    &mut link,
                    7000,
                    vec![Event::WindowFocused(false), Event::WindowFocused(true)]
                )
                .1
        ));
        assert!(join_off(&draw.draw(&mut link, 7010, Vec::new()).1));
        assert!(join_off(&draw.draw(&mut link, 8009, Vec::new()).1));
        assert!(!join_off(&draw.draw(&mut link, 8010, Vec::new()).1));
    }

    #[test]
    fn link_join_requires_the_pointer_to_leave() {
        let draw = LinkDraw::new();
        let mut link = waiting();
        let (_, output) = draw.draw(&mut link, 0, Vec::new());
        let at = places(&output, "Join this room")[0].center();
        draw.draw(&mut link, 10, vec![Event::PointerMoved(at)]);
        draw.draw(&mut link, 990, press(at));
        assert!(draw.draw(&mut link, 1010, release(at)).0.is_none());
        for ms in [1100, 1500, 2000] {
            draw.draw(&mut link, ms, press(at));
            assert!(draw.draw(&mut link, ms + 16, release(at)).0.is_none());
        }
        draw.draw(&mut link, 2100, vec![Event::PointerMoved(pos2(1.0, 1.0))]);
        draw.draw(&mut link, 2200, press(at));
        assert!(matches!(
            draw.draw(&mut link, 2216, release(at)).0,
            Some(Choice::JoinLink)
        ));
    }

    #[test]
    fn link_join_refuses_a_click_that_leaves_in_its_own_frame() {
        let draw = LinkDraw::new();
        let mut link = waiting();
        let (_, output) = draw.draw(&mut link, 0, Vec::new());
        let at = places(&output, "Join this room")[0].center();
        draw.draw(&mut link, 10, vec![Event::PointerMoved(at)]);
        draw.draw(&mut link, 1000, Vec::new());
        draw.draw(&mut link, 1100, press(at));
        let mut away = release(at);
        away.push(Event::PointerMoved(pos2(1.0, 1.0)));
        assert!(draw.draw(&mut link, 1116, away).0.is_none());
        draw.draw(&mut link, 1200, press(at));
        assert!(matches!(
            draw.draw(&mut link, 1216, release(at)).0,
            Some(Choice::JoinLink)
        ));
    }

    #[test]
    fn link_join_refuses_a_press_begun_while_off() {
        let draw = LinkDraw::new();
        let mut link = waiting();
        let (_, output) = draw.draw(&mut link, 0, Vec::new());
        let rect = places(&output, "Join this room")[0];
        // Off Join's 5-point reach and back, within the 6 points that keep
        // the release a click.
        let at = pos2(rect.left() + 0.25, rect.center().y);
        draw.draw(&mut link, 900, press(at));
        draw.draw(
            &mut link,
            1000,
            vec![Event::PointerMoved(pos2(rect.left() - 5.25, at.y))],
        );
        draw.draw(&mut link, 1005, vec![Event::PointerMoved(at)]);
        assert!(draw.draw(&mut link, 1010, release(at)).0.is_none());
        draw.draw(&mut link, 1100, press(at));
        assert!(matches!(
            draw.draw(&mut link, 1116, release(at)).0,
            Some(Choice::JoinLink)
        ));
    }

    // egui gives a press a few points beside Join to Join, so resting there
    // through the wait is resting on it.
    #[test]
    fn link_join_counts_the_pointer_beside_it_as_on_it() {
        let draw = LinkDraw::new();
        let mut link = waiting();
        let (_, output) = draw.draw(&mut link, 0, Vec::new());
        let rect = places(&output, "Join this room")[0];
        let at = pos2(rect.right() + 3.0, rect.center().y);
        draw.draw(&mut link, 10, vec![Event::PointerMoved(at)]);
        for ms in [1000, 1500, 2000] {
            draw.draw(&mut link, ms, press(at));
            assert!(draw.draw(&mut link, ms + 16, release(at)).0.is_none());
        }
    }

    #[test]
    fn link_page_keys_and_reader() {
        let draw = LinkDraw::new();
        let mut link = waiting();
        draw.draw(&mut link, 0, Vec::new());
        draw.draw(&mut link, 1000, key_event(Key::Tab));
        assert!(
            draw.draw(&mut link, 1010, key_event(Key::Enter))
                .0
                .is_none()
        );
        assert!(matches!(
            draw.draw(&mut link, 1020, key_event(Key::Space)).0,
            Some(Choice::JoinLink)
        ));
        assert!(matches!(
            draw.draw(&mut link, 1030, key_event(Key::Escape)).0,
            Some(Choice::DismissLink)
        ));
        let (_, output) = draw.draw(&mut link, 1040, Vec::new());
        let tree = output.accesskit_update.unwrap();
        let id = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Join this room"))
            .unwrap()
            .0;
        let click = Event::AccessKitActionRequest(eframe::egui::accesskit::ActionRequest {
            action: eframe::egui::accesskit::Action::Click,
            target_tree: eframe::egui::accesskit::TreeId::ROOT,
            target_node: id,
            data: None,
        });
        assert!(matches!(
            draw.draw(&mut link, 1050, vec![click]).0,
            Some(Choice::JoinLink)
        ));
    }

    #[test]
    fn link_page_join_stays_put_and_expires() {
        let mut draw = LinkDraw::new();
        let mut link = waiting();
        let (_, output) = draw.draw(&mut link, 0, Vec::new());
        let rect = places(&output, "Join this room")[0];
        draw.mic = true;
        if let Waiting::Shown { error, .. } = &mut link {
            *error = Some(String::from("Could not join. Try again."));
        }
        let (_, output) = draw.draw(&mut link, 1000, Vec::new());
        assert_eq!(places(&output, "Join this room")[0], rect);
        assert!(!join_off(&output));
        draw.unix += invite::SINGLE_USE_SECS;
        let (_, output) = draw.draw(&mut link, 2000, Vec::new());
        assert_eq!(places(&output, "Join this room")[0], rect);
        assert!(join_off(&output));
    }

    // On push to talk the open mic line only holds its room: a screen reader
    // must not be told the room hears everything.
    #[test]
    fn link_page_says_open_mic_only_when_on() {
        let mut draw = LinkDraw::new();
        let mut link = waiting();
        let said = |output: PlatformOutput| {
            output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .any(|(_, node)| node.label().or(node.value()) == Some(messages::LINK_OPEN_MIC))
        };
        assert!(!said(draw.draw(&mut link, 0, Vec::new()).1));
        draw.mic = true;
        assert!(said(draw.draw(&mut link, 10, Vec::new()).1));
    }

    #[test]
    fn refused_link_page_has_only_back() {
        let draw = LinkDraw::new();
        let mut link = Waiting::Refused(String::from(messages::LINK_NOT_BOOTH));
        let (_, output) = draw.draw(&mut link, 0, Vec::new());
        let nodes = output.accesskit_update.as_ref().unwrap();
        let buttons: Vec<_> = nodes
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::Button)
            .filter_map(|(_, node)| node.label())
            .collect();
        assert_eq!(buttons, ["Back"]);
        let at = places(&output, "Back")[0].center();
        draw.draw(&mut link, 10, press(at));
        assert!(matches!(
            draw.draw(&mut link, 26, release(at)).0,
            Some(Choice::DismissLink)
        ));
    }

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
            let choice = show(ui, &mut Start::default(), None, None, &[], update, None);
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

    // Two hosts joined over loopback and read back from the file.
    fn two_known_hosts(name: &str) -> Vec<KnownHost> {
        let dir = std::env::temp_dir().join(format!("booth-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let friend_dir = dir.join("friend");
        std::fs::create_dir_all(&friend_dir).unwrap();
        for name in ["first", "second"] {
            let host_dir = dir.join(name);
            std::fs::create_dir_all(&host_dir).unwrap();
            let host = test_room(&host_dir);
            test_friend(&host, &friend_dir).leave();
            host.leave();
        }
        let known = room::Room::known_hosts(&friend_dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(known.len(), 2);
        known
    }

    pub(crate) fn places(output: &PlatformOutput, label: &str) -> Vec<Rect> {
        output
            .accesskit_update
            .as_ref()
            .expect("accesskit is on")
            .nodes
            .iter()
            .filter(|(_, node)| node.label() == Some(label))
            .filter_map(|(_, node)| node.bounds())
            .map(|bounds| {
                Rect::from_min_max(
                    pos2(bounds.x0 as f32, bounds.y0 as f32),
                    pos2(bounds.x1 as f32, bounds.y1 as f32),
                )
            })
            .collect()
    }

    // With a row open above it, a row clicked moves up as that one closes,
    // and its Save and Forget come to lie where it was. The second click of a
    // double click there presses neither, nor does a press made in the wait
    // and let go after, and both work once the wait is over. Enter in the
    // field does not wait.
    #[test]
    fn a_second_click_on_a_row_that_moved_up_presses_nothing() {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let known = two_known_hosts("start");
        let (above, clicked) = (*known[0].host_key(), *known[1].host_key());
        let mut start = Start {
            known,
            ..Start::default()
        };
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let draw = |start: &mut Start, events: Vec<Event>, ms: u64| {
            frame(&ctx, events, |ui| {
                controls::page(ui, |ui| known_hosts(ui, start, at(ms)))
            })
        };

        // Where Forget and Save lie with that row open, and that the same
        // place is on the row while the other one is open.
        start.toggle(clicked, at(0));
        draw(&mut start, Vec::new(), 1000);
        let (_, output) = draw(&mut start, Vec::new(), 1000);
        let forget = places(&output, "Forget")[0].center();
        let save = places(&output, "Save")[0].center();
        start.toggle(above, at(1000));
        draw(&mut start, Vec::new(), 2000);
        let (_, output) = draw(&mut start, Vec::new(), 2000);
        let row = places(&output, "Join")
            .into_iter()
            .max_by(|a, b| a.top().total_cmp(&b.top()))
            .unwrap();
        for aim in [forget, save] {
            assert!(row.y_range().contains(aim.y), "{row:?} {aim:?}");
        }

        // The first click opens it.
        draw(&mut start, press(forget), 3000);
        let (choice, _) = draw(&mut start, release(forget), 3016);
        assert!(choice.is_none());
        assert_eq!(start.opened, Some(clicked));
        let (_, output) = draw(&mut start, Vec::new(), 3032);
        assert_eq!(places(&output, "Forget")[0].center(), forget);

        for (ms, events) in [
            (3100, press(forget)),
            (3116, release(forget)),
            (3150, press(save)),
            (3166, release(save)),
            (3400, press(forget)),
            (3520, release(forget)),
        ] {
            let (choice, _) = draw(&mut start, events, ms);
            assert!(choice.is_none(), "at {ms} ms");
        }

        draw(&mut start, press(forget), 3600);
        let (choice, _) = draw(&mut start, release(forget), 3616);
        assert!(matches!(choice, Some(Choice::Forget(key)) if key == clicked));
        draw(&mut start, press(save), 3700);
        let (choice, _) = draw(&mut start, release(save), 3716);
        assert!(matches!(choice, Some(Choice::SaveAddress(key)) if key == clicked));

        // Close it and open it again: Enter saves while the buttons wait.
        start.toggle(clicked, at(4000));
        start.toggle(clicked, at(4000));
        draw(&mut start, Vec::new(), 4000);
        draw(&mut start, Vec::new(), 4016);
        let enter = vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }];
        let (choice, _) = draw(&mut start, enter, 4032);
        assert!(matches!(choice, Some(Choice::SaveAddress(key)) if key == clicked));
    }
}
