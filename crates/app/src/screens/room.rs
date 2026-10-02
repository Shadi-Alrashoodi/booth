use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{
    Align, Color32, FontId, Frame, Key, Layout, Margin, Modifiers, Rect, Response, ScrollArea,
    Sense, Stroke, Ui, pos2, vec2,
};
use invite::ReplyCode;
use room::Room;
use room::view::{
    InviteView, LinkState, MappingWord, Notice, OwnShare, PasteState, Person, ReplyState,
    ReplyView, Role, ShareView, TalkMode, View, Voice,
};
use voice::audio::{AudioError, Choice};

use crate::controls::{self, Button};
use crate::hotkeys::{self, Target};
use crate::messages;
use crate::monitors::{self, Pick};
use crate::screens::chat::Chat;
use crate::screens::control::{self, AllowWait, Answer, Decline, MenuItem, PanicKey, RowMenu};
use crate::screens::stats;
use crate::sound::PeriodsAsk;
use crate::strip;
use crate::theme::{self, AMBER, ASH, BAD, CHALK, CONTROL_HEIGHT, PANEL, ROW_HEIGHT, SIDE, WARN};
use crate::win;

const TOGGLE_HEIGHT: f32 = 24.0;
// A reply code is about 110 characters. The cap is the invite field's: far
// above any code, far below a document pasted by mistake.
const PASTE_CHARS: usize = 2048;

pub struct InRoom {
    // The hotkey thread holds it too while the room is open, through the
    // target.
    pub room: Arc<Room>,
    // What was in the room name field, handed back to the start screen.
    pub typed_room_name: String,
    // Why the last Copy did not reach the clipboard, until the next one does.
    copy_error: Option<String>,
    log_file: Option<PathBuf>,
    // The stats word; settled before any room can start.
    firewall: &'static str,
    // The host's field for codes friends send back, and why the last thing
    // pasted there was not one. Refusals come back in the view.
    paste: String,
    paste_error: Option<String>,
    chat: Chat,
    // The devices from settings as the room opened, and their periods,
    // asked each time the stats panel opens.
    devices: [Choice; 2],
    periods: PeriodsAsk,
    hold: Hold,
    target: Target,
    // The hotkeys as they are this frame.
    keys: hotkeys::State,
    // Whether the panel offers remote control at all (control.rs).
    control_offered: bool,
    // The monitors Share offered, while the list under the title row is
    // open.
    monitors: Option<Vec<Pick>>,
    panic: PanicKey,
    allow_wait: AllowWait,
    decline: Decline,
    menu: RowMenu,
}

// Hold to talk as the room was last told, and whether the button was drawn
// this frame. A frame that does not draw it lets go, whatever the mouse is
// doing: the stats panel over the list, a notice in its place, open mic, a
// room that ended. Nothing may go on sending with the button out of sight.
#[derive(Default)]
struct Hold {
    held: bool,
    drawn: bool,
    drawn_before: bool,
}

impl Hold {
    fn frame_begins(&mut self) {
        self.drawn_before = self.drawn;
        self.drawn = false;
    }

    // Pressing the button brings the panel to the front, which ends a pause
    // for an administrator window, and the button would go from under the
    // mouse that holds it; the press can even land after it went. So once
    // on screen it stays while the mouse is down.
    fn stays(&self, pointer_down: bool) -> bool {
        self.drawn_before && (pointer_down || self.held)
    }

    // The button was drawn, down or not. What to tell the room, if anything.
    fn drawn(&mut self, down: bool) -> Option<bool> {
        self.drawn = true;
        self.set(down)
    }

    fn frame_ends(&mut self) -> Option<bool> {
        if self.drawn { None } else { self.set(false) }
    }

    fn set(&mut self, down: bool) -> Option<bool> {
        (down != self.held).then(|| {
            self.held = down;
            down
        })
    }
}

pub enum Step {
    Leave,
    NewInvite { multi_use: bool },
    NewCode,
    // A monitor was picked from the list, and its Windows name goes in the
    // settings for the share key.
    Remember(String),
}

impl InRoom {
    pub fn new(
        room: Arc<Room>,
        typed_room_name: String,
        log_file: Option<PathBuf>,
        firewall: &'static str,
        devices: [Choice; 2],
        target: Target,
        panic: PanicKey,
    ) -> InRoom {
        InRoom {
            room,
            typed_room_name,
            copy_error: None,
            log_file,
            firewall,
            paste: String::new(),
            paste_error: None,
            chat: Chat::new(),
            devices,
            periods: PeriodsAsk::default(),
            hold: Hold::default(),
            target,
            keys: hotkeys::State::default(),
            control_offered: false,
            monitors: None,
            panic,
            allow_wait: AllowWait::default(),
            decline: Decline::default(),
            menu: RowMenu::default(),
        }
    }

    // From the app's logic, not from show: the panel is minimized over a
    // game when a request comes, and the asker would wait until it opens.
    pub fn decline_unshown(&mut self, view: &View, control_offered: bool) {
        if let Some(number) = self.decline.follow(view, control_offered) {
            self.room.answer_control(number, false);
        }
    }

    // The share key starts a share by itself, so a list left open would
    // offer a choice that no longer applies.
    pub fn close_monitors(&mut self) {
        self.monitors = None;
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        view: &View,
        stats_open: bool,
        keys: hotkeys::State,
        control_offered: bool,
    ) -> Option<Step> {
        self.keys = keys;
        self.control_offered = control_offered;
        self.hold.frame_begins();
        let step = self.body(ui, view, stats_open);
        if let Some(held) = self.hold.frame_ends() {
            self.target.button(held);
        }
        step
    }

    fn body(&mut self, ui: &mut Ui, view: &View, stats_open: bool) -> Option<Step> {
        let mut step = None;
        let title = if view.room_name.is_empty() {
            "Booth"
        } else {
            view.room_name.as_str()
        };
        // When the room is over or never started, the way out sits under the
        // sentence that explains it, and the title row stays empty.
        let share = share_verb(view, self.control_offered);
        if share != Some(ShareVerb::Share) {
            self.monitors = None;
        }
        let words = share.as_ref().map(ShareVerb::words);
        let mut verbs: Vec<Button> = words.iter().map(VerbWords::button).collect();
        if !ends_here(view) {
            verbs.push(Button::new(leave_word(view.role)));
        }
        match controls::title_row(ui, title, &verbs) {
            Some(0) if share.is_some() => self.share_pressed(share.as_ref()),
            Some(_) => step = Some(Step::Leave),
            None => {}
        }
        if self.monitors.is_some()
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.monitors = None;
        }
        if let Some(picks) = &self.monitors
            && let Some(i) = monitor_row(ui, picks)
        {
            let pick = &picks[i];
            self.room.share(pick.fps, Some(pick.id.clone()));
            step = Some(Step::Remember(pick.id.device_name.clone()));
            self.monitors = None;
        }
        self.request(ui, view);
        if stats_open {
            self.periods.ask(ui.ctx(), &self.devices);
            let periods = self.periods.periods();
            stats::show(
                ui,
                view,
                self.log_file.as_deref(),
                self.firewall,
                periods.as_ref(),
                self.control_offered,
            );
            return step;
        }
        self.periods.closed();
        if !chat_shown(view) {
            let scroll = ScrollArea::vertical().id_salt("room").auto_shrink(false);
            scroll.show(ui, |ui| match view.role {
                Role::Host => self.host(ui, view, &mut step),
                Role::Client => self.client(ui, view, &mut step),
            });
            return step;
        }
        // At the smallest window the invite block and eight people are taller
        // than the body. They scroll on their own once they would take more
        // than half of it, so the chat always keeps the other half.
        let most = ((ui.available_height() - CONTROL_HEIGHT) / 2.0).max(ROW_HEIGHT);
        let scroll = ScrollArea::vertical()
            .id_salt("room")
            .max_height(most)
            .auto_shrink([false, true]);
        scroll.show(ui, |ui| match view.role {
            Role::Host => self.host(ui, view, &mut step),
            Role::Client => self.client(ui, view, &mut step),
        });
        let enabled = composer_enabled(view);
        self.chat
            .show(ui, &view.chat, &view.people, &self.room, enabled);
        step
    }

    // Share on a PC with one monitor starts on it at once; with more, the
    // list opens, and Share again closes it. Stop sharing needs no answer.
    fn share_pressed(&mut self, verb: Option<&ShareVerb>) {
        match verb {
            Some(ShareVerb::Share) if self.monitors.is_some() => self.monitors = None,
            Some(ShareVerb::Share) => match monitors::list() {
                Ok(list) if list.len() > 1 => self.monitors = Some(monitors::picks(&list)),
                Ok(list) => {
                    let only = list.first();
                    let fps = only.map_or(monitors::MOST_FPS, |m| monitors::fps(m.refresh_hz));
                    self.room.share(fps, only.map(|m| m.id.clone()));
                }
                // The room tries the primary monitor, and when that cannot
                // open either, its problem line in the chat says why.
                Err(_) => self.room.share(monitors::MOST_FPS, None),
            },
            Some(ShareVerb::StopSharing) => self.room.stop_sharing(),
            Some(ShareVerb::StopControl) => self.room.stop_control(),
            Some(ShareVerb::Starting | ShareVerb::Busy(_)) | None => {}
        }
    }

    // The control request block, under the title row whatever the body
    // shows, the stats panel included: who may use this PC is never out
    // of sight while it is being decided or done.
    fn request(&mut self, ui: &mut Ui, view: &View) {
        let request = control::request(view, self.control_offered);
        let wait = self.allow_wait.follow(request, Instant::now());
        if let Some(left) = wait {
            ui.ctx().request_repaint_after(left);
        }
        let Some(request) = request else {
            return;
        };
        match control::request_block(ui, request, &self.panic.words, wait.is_none()) {
            Some(Answer::Allow(number)) => {
                self.panic.flags.allow();
                self.room.answer_control(number, true);
            }
            Some(Answer::DontAllow(number)) => self.room.answer_control(number, false),
            Some(Answer::Stop) => self.room.stop_control(),
            None => {}
        }
    }

    fn host(&mut self, ui: &mut Ui, view: &View, step: &mut Option<Step>) {
        if let Some(notice) = &view.notice {
            controls::page(ui, |ui| explain(ui, notice, step));
            return;
        }
        // Above the invite, since a new invite is one of the two ways back.
        let lost = messages::friends_lost(view.address_changed);
        if lost.is_some() || view.invite.is_some() {
            controls::page(ui, |ui| {
                if let Some(text) = lost {
                    controls::text(ui, text, theme::body(), CHALK);
                }
                let Some(invite) = &view.invite else {
                    return;
                };
                if lost.is_some() {
                    ui.add_space(16.0);
                }
                self.invite(ui, invite, view.numbers.local_port, &view.share, step);
                // A punch from a router that changes ports opens a port
                // nobody will come to, so the field is not offered then.
                let codes_help = view.numbers.mapping != Some(MappingWord::Hard);
                if !invite.code.is_empty() && codes_help {
                    let moved = view
                        .address_changed
                        .is_some_and(|changed| changed.codes_cannot_help);
                    self.paste_field(ui, view.paste.as_ref(), moved);
                }
            });
            controls::hairline(ui, ui.cursor().top());
        }
        self.people(ui, view);
        if view.people.iter().all(|person| person.is_you) {
            controls::page(ui, |ui| {
                controls::text(ui, messages::EMPTY_ROOM, theme::body(), ASH);
            });
        }
    }

    fn invite(
        &mut self,
        ui: &mut Ui,
        invite: &InviteView,
        port: u16,
        share: &ShareView,
        step: &mut Option<Step>,
    ) {
        let rect = row_rect(ui, TOGGLE_HEIGHT);
        let toggle = if invite.multi_use {
            Button::new("anyone, 24 h").color(WARN)
        } else {
            Button::new("single use, 10 min")
        }
        .height(TOGGLE_HEIGHT);
        let toggle_width = toggle.width(ui);
        controls::split_row(
            ui,
            rect,
            toggle_width,
            |ui| {
                controls::one_line(ui, "Invite", theme::medium(), CHALK);
            },
            |ui| {
                if toggle.show(ui).clicked() {
                    *step = Some(Step::NewInvite {
                        multi_use: !invite.multi_use,
                    });
                }
            },
        );

        let dead = invite.used || invite.expired;
        let outdated = outdated_by(invite);
        if !invite.code.is_empty() {
            ui.add_space(8.0);
            // A used or expired code comes with the way to a fresh one
            // instead of Copy. Copy keeps its name after a press: buttons say
            // what they do, and a longer label would move the code over under
            // the mouse.
            let buttons: &[&str] = if dead {
                &["New invite"]
            } else if outdated.is_some() {
                &["Copy", "New invite"]
            } else {
                &["Copy"]
            };
            let (text, mono, color) = invite_text(invite, share);
            match code_row(ui, text, mono, color, buttons).map(|i| buttons[i]) {
                Some("Copy") => {
                    self.copy_error = win::copy_private(&invite.code)
                        .err()
                        .map(|err| messages::copy_error(&err));
                }
                Some(_) => {
                    *step = Some(Step::NewInvite {
                        multi_use: invite.multi_use,
                    });
                }
                None => {}
            }
        }

        ui.add_space(8.0);
        let line = if invite.used {
            String::from("This invite has been used.")
        } else if invite.expired {
            String::from("This invite has expired.")
        } else if invite.multi_use && !invite.code.is_empty() {
            String::from(messages::ANYONE)
        } else {
            messages::router(invite.router, port)
        };
        controls::text(ui, line, theme::small(), ASH);
        if let Some(why) = outdated {
            controls::text(ui, why, theme::small(), ASH);
        }
        if let Some(error) = &self.copy_error {
            ui.add_space(8.0);
            controls::text(ui, error.as_str(), theme::body(), BAD);
        }
    }

    // Enter hands the code to the room, which checks it and punches. The
    // one line under the field says what came of it. `moved` is this PC's
    // own address having just changed with friends still out.
    fn paste_field(&mut self, ui: &mut Ui, last: Option<&PasteState>, moved: bool) {
        ui.add_space(12.0);
        let field = controls::field(
            ui,
            "reply code",
            &mut self.paste,
            messages::PASTE_HINT,
            theme::mono(),
            PASTE_CHARS,
        );
        if field.changed() {
            self.paste_error = None;
        }
        if entered(ui, &field) {
            match ReplyCode::decode(self.paste.trim()) {
                Ok(code) => {
                    self.paste_error = None;
                    // Refused codes stay in the field for another look.
                    if self.room.accept_reply(code).is_ok() {
                        self.paste.clear();
                    }
                }
                Err(err) => self.paste_error = messages::reply_code_error(&err),
            }
        }
        if let Some((text, color)) = paste_line(self.paste_error.as_deref(), moved, last) {
            ui.add_space(8.0);
            controls::text(ui, text, theme::body(), color);
        }
    }

    fn client(&mut self, ui: &mut Ui, view: &View, step: &mut Option<Step>) {
        if view.people.is_empty() {
            controls::page(ui, |ui| match &view.notice {
                Some(Notice::StillTrying) => self.still_trying(ui, view.reply.as_ref(), step),
                Some(notice) => explain(ui, notice, step),
                None => {
                    if Button::new("Cancel").show(ui).clicked() {
                        *step = Some(Step::Leave);
                    }
                }
            });
            return;
        }
        let code = code_in_room(view);
        if view.notice.is_some() || code.is_some() {
            controls::page(ui, |ui| {
                if let Some(notice) = &view.notice {
                    explain(ui, notice, step);
                }
                let Some(reply) = code else {
                    return;
                };
                if view.notice.is_some() {
                    ui.add_space(16.0);
                }
                // The title row already has Leave, so New code is the one
                // button here.
                if self.code_block(ui, reply) {
                    ui.add_space(16.0);
                    if Button::new("New code").show(ui).clicked() {
                        *step = Some(Step::NewCode);
                    }
                }
            });
            controls::hairline(ui, ui.cursor().top());
        }
        self.people(ui, view);
    }

    // Joining when the host cannot be reached: the code to send back, or the
    // sentence that says why none would help, under the line that says the
    // tries go on.
    fn still_trying(&mut self, ui: &mut Ui, reply: Option<&ReplyView>, step: &mut Option<Step>) {
        controls::text(
            ui,
            messages::notice(&Notice::StillTrying),
            theme::body(),
            CHALK,
        );
        let mut expired = false;
        if let Some(reply) = reply {
            ui.add_space(16.0);
            expired = self.code_block(ui, reply);
        }
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if expired && Button::new("New code").show(ui).clicked() {
                *step = Some(Step::NewCode);
            }
            if Button::new("Cancel").show(ui).clicked() {
                *step = Some(Step::Leave);
            }
        });
    }

    // The code with its Copy button, or what failed once it expired, or why
    // no code would help. The same block on the joining screen and above
    // the people list. True when it expired, which is when New code is
    // offered.
    fn code_block(&mut self, ui: &mut Ui, reply: &ReplyView) -> bool {
        let mut expired = false;
        match reply.state {
            ReplyState::Code { second_router } => {
                controls::text(ui, messages::SEND_CODE_BACK, theme::body(), CHALK);
                ui.add_space(8.0);
                if code_row(ui, &reply.code, true, CHALK, &["Copy"]).is_some() {
                    // The code holds no secret, but it does hold this PC's
                    // home address, which history has no use for.
                    self.copy_error = win::copy_private(&reply.code)
                        .err()
                        .map(|err| messages::copy_error(&err));
                }
                if second_router {
                    ui.add_space(8.0);
                    controls::text(ui, messages::CODE_SECOND_ROUTER, theme::small(), ASH);
                }
            }
            ReplyState::Expired { .. } => {
                expired = true;
                controls::text(ui, messages::CODE_EXPIRED, theme::body(), CHALK);
                if let Some(rung) = messages::reply(reply.state, reply.host_port) {
                    ui.add_space(8.0);
                    controls::text(ui, rung, theme::body(), ASH);
                }
            }
            state => {
                if let Some(why) = messages::reply(state, reply.host_port) {
                    controls::text(ui, why, theme::body(), CHALK);
                }
            }
        }
        if let Some(error) = &self.copy_error {
            ui.add_space(8.0);
            controls::text(ui, error.as_str(), theme::body(), BAD);
        }
        expired
    }
}

// A code in Plex Mono on one line, cut in the middle, with its buttons at the
// right edge in reading order; or, when `code` is false, a line of text in
// its place. Returns which button was pressed.
fn code_row(
    ui: &mut Ui,
    text: &str,
    code: bool,
    color: Color32,
    buttons: &[&str],
) -> Option<usize> {
    let rect = row_rect(ui, CONTROL_HEIGHT);
    let buttons: Vec<Button> = buttons.iter().map(|text| Button::new(text)).collect();
    let width = controls::buttons_width(ui, &buttons);
    let mut pressed = None;
    controls::split_row(
        ui,
        rect,
        width,
        |ui| {
            if code {
                let font = theme::mono();
                let room = ui.available_width();
                let shown = controls::elide_middle(ui, text, &font, room);
                controls::one_line(ui, &shown, font, color);
            } else {
                controls::one_line(ui, text, theme::body(), color);
            }
        },
        |ui| pressed = controls::buttons(ui, &buttons, rect.height()),
    );
    pressed
}

// A single-line field gives up focus on Enter, so that pair is the press.
fn entered(ui: &Ui, field: &Response) -> bool {
    field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter))
}

// One sentence for the notice, and the button that is the way out of it when
// the room cannot go on.
fn explain(ui: &mut Ui, notice: &Notice, step: &mut Option<Step>) {
    controls::text(ui, messages::notice(notice), theme::body(), CHALK);
    // The room keeps trying under the two about a silent host, and Leave is
    // in the title row.
    let button = match notice {
        Notice::StillTrying => Some("Cancel"),
        Notice::LostHost | Notice::HostMoved => None,
        Notice::RoomClosed
        | Notice::InviteExpired
        | Notice::SocketFailed
        | Notice::OtherVersion { .. }
        | Notice::UnversionedHost => Some("Leave"),
    };
    if let Some(label) = button {
        ui.add_space(16.0);
        if Button::new(label).show(ui).clicked() {
            *step = Some(Step::Leave);
        }
    }
}

// The chat is part of a room this PC is in: the host's while it runs, a
// client's once it has joined, and after the host is lost or closed the room,
// for reading back.
fn chat_shown(view: &View) -> bool {
    match view.role {
        Role::Host => view.notice.is_none(),
        Role::Client => !view.people.is_empty(),
    }
}

// Disabled while the host is lost. While it is only quiet a line waits in
// the room and goes when the host is back.
fn composer_enabled(view: &View) -> bool {
    match view.role {
        Role::Host => view.notice.is_none(),
        Role::Client => matches!(view.strip.state, LinkState::Live | LinkState::Reconnecting),
    }
}

fn ends_here(view: &View) -> bool {
    match &view.notice {
        Some(Notice::StillTrying) => true,
        Some(Notice::LostHost | Notice::HostMoved) => false,
        Some(
            Notice::RoomClosed
            | Notice::InviteExpired
            | Notice::SocketFailed
            | Notice::OtherVersion { .. }
            | Notice::UnversionedHost,
        ) => true,
        None => view.role == Role::Client && view.people.is_empty(),
    }
}

// The code block above the people list, while the host is silent and this
// PC's own address changed. Not under a notice that ends the room,
// since nobody would come for the code.
fn code_in_room(view: &View) -> Option<&ReplyView> {
    if view.role != Role::Client || view.people.is_empty() || ends_here(view) {
        return None;
    }
    view.reply.as_ref()
}

// Worth a new invite when the one on show no longer carries the best way in.
// A changed outside address is the bigger reason: the old one leads nowhere,
// where a port opened later only adds a way.
fn outdated_by(invite: &InviteView) -> Option<&'static str> {
    if invite.used || invite.expired || invite.code.is_empty() {
        return None;
    }
    if invite.address_changed_since {
        Some(messages::ADDRESS_CHANGED_SINCE)
    } else if invite.mapped_since {
        Some(messages::MAPPED_SINCE)
    } else {
        None
    }
}

// The one line under the paste field. Something pasted that is not a code
// at all comes first, since it answers what was just typed. While this PC's
// own address has just changed no code can help, whatever the room made of
// the last one, so that is said instead.
fn paste_line(
    typed: Option<&str>,
    moved: bool,
    last: Option<&PasteState>,
) -> Option<(String, Color32)> {
    if let Some(error) = typed {
        return Some((error.to_owned(), BAD));
    }
    if moved {
        return Some((String::from(messages::CODES_CANNOT_HELP), ASH));
    }
    let last = last?;
    let color = if *last == PasteState::Sent { ASH } else { BAD };
    messages::paste(last).map(|text| (text, color))
}

fn leave_word(role: Role) -> &'static str {
    match role {
        Role::Host => "Close room",
        Role::Client => "Leave",
    }
}

// The title row's verb for this PC's share. One share at a time: while
// someone else shares, Share stays in its place in ash and does nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ShareVerb {
    Share,
    // Asked of the host and not granted yet, which is a round trip.
    Starting,
    StopSharing,
    Busy(String),
    // While this PC controls someone's share, in Share's place.
    StopControl,
}

struct VerbWords {
    text: &'static str,
    color: Color32,
    // What a screen reader hears instead of the text.
    label: Option<String>,
    enabled: bool,
}

impl ShareVerb {
    fn words(&self) -> VerbWords {
        let (text, color, label, enabled) = match self {
            ShareVerb::Share => ("Share", CHALK, None, true),
            ShareVerb::Starting => ("Starting", ASH, None, false),
            ShareVerb::StopSharing => ("Stop sharing", AMBER, None, true),
            ShareVerb::Busy(name) => ("Share", ASH, Some(messages::one_share(name)), false),
            ShareVerb::StopControl => ("Stop control", AMBER, None, true),
        };
        VerbWords {
            text,
            color,
            label,
            enabled,
        }
    }
}

impl VerbWords {
    fn button(&self) -> Button<'_> {
        let button = Button::new(self.text)
            .color(self.color)
            .enabled(self.enabled);
        match &self.label {
            Some(label) => button.label(label),
            None => button,
        }
    }
}

// None while the room is over, and on a client that lost the host, which
// could only be refused; a share under way can always be stopped.
fn share_verb(view: &View, control_offered: bool) -> Option<ShareVerb> {
    if ends_here(view) {
        return None;
    }
    if matches!(view.share.own, OwnShare::Sharing { .. }) {
        return Some(ShareVerb::StopSharing);
    }
    // The way out of controlling stays, a lost host included.
    if control::stop_control_in_title(view, control_offered) {
        return Some(ShareVerb::StopControl);
    }
    let lost = matches!(view.strip.state, LinkState::Lost | LinkState::Closed);
    if view.role == Role::Client && lost {
        return None;
    }
    if let Some(current) = view.share.current.as_ref().filter(|share| !share.yours) {
        return Some(ShareVerb::Busy(current.name.clone()));
    }
    if matches!(view.share.own, OwnShare::Asking { .. }) {
        return Some(ShareVerb::Starting);
    }
    Some(ShareVerb::Share)
}

// Asked for counts too: one press of the share key takes back an ask the
// host has not answered yet, and addresses_hidden hides the invite code and
// the addresses from the ask on.
pub fn own_share_on(own: &OwnShare) -> bool {
    matches!(own, OwnShare::Sharing { .. } | OwnShare::Asking { .. })
}

// The invite row and the stats panel may be on the screen being shared, and
// a friend's view would show the host's way into the room and everyone's
// home addresses. From the ask until the capture has closed, so no frame of
// the share can hold one: Stop sharing turns the share Off a moment before
// its thread lets go of the screen.
pub fn addresses_hidden(share: &ShareView) -> bool {
    own_share_on(&share.own) || share.running.is_some()
}

// On a PC with more than one monitor, Share opens a one-line list of them by
// size and position under the title row, and one click starts the share. At
// the narrowest window three of them wrap onto a second line.
fn monitor_row(ui: &mut Ui, picks: &[Pick]) -> Option<usize> {
    let mut pressed = None;
    let margin = Margin {
        left: SIDE as i8,
        right: SIDE as i8,
        top: 8,
        bottom: 8,
    };
    Frame::new().inner_margin(margin).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            for (i, pick) in picks.iter().enumerate() {
                if Button::new(&pick.text)
                    .label(&pick.spoken)
                    .show(ui)
                    .clicked()
                {
                    pressed = Some(i);
                }
            }
        });
    });
    controls::hairline(ui, ui.cursor().top());
    pressed
}

// While someone shares, their row has Watch for everyone else,
// which reads Stop watching while this PC watches. None on every other row,
// and while the host is lost, since nothing could be watched then.
fn watch_word(view: &View, person: &Person) -> Option<&'static str> {
    let current = view.share.current.as_ref()?;
    if person.is_you || current.yours || current.key != person.key {
        return None;
    }
    if view.role == Role::Client && matches!(view.strip.state, LinkState::Lost | LinkState::Closed)
    {
        return None;
    }
    Some(if view.share.watching {
        "Stop watching"
    } else {
        "Watch"
    })
}

// Which of a person's row buttons was pressed.
#[derive(Clone, Copy)]
enum RowPress {
    Watch,
    Control,
}

// From the ask until the capture has closed (addresses_hidden) the code is
// not on the screen for everyone watching to join with, used or expired
// too, and Copy still copies it. Otherwise a used or expired code shows in
// ash for reference.
fn invite_text<'a>(invite: &'a InviteView, share: &ShareView) -> (&'a str, bool, Color32) {
    if addresses_hidden(share) {
        return (messages::HIDDEN_WHILE_SHARING, false, ASH);
    }
    let color = if invite.used || invite.expired {
        ASH
    } else {
        CHALK
    };
    (invite.code.as_str(), true, color)
}

enum VoicePress {
    Mute,
    Deafen,
}

impl InRoom {
    // On a client the list is the host's word, so it goes to ash with the
    // link: the numbers while the host is quiet, everything once it is lost.
    // Your own row holds the voice buttons, and under it goes why the
    // microphone or the speakers are not running.
    fn people(&mut self, ui: &mut Ui, view: &View) {
        let client_link = (view.role == Role::Client).then_some(view.strip.state);
        let gone = matches!(client_link, Some(LinkState::Lost | LinkState::Closed));
        let stale = gone || client_link == Some(LinkState::Reconnecting);
        let offered = self.control_offered;
        let with_menu: Vec<[u8; 32]> = view
            .people
            .iter()
            .filter(|person| !control::menu_items(view, person, offered).is_empty())
            .map(|person| person.key)
            .collect();
        self.menu.keep_only(&with_menu);
        for person in &view.people {
            let same_name = view
                .people
                .iter()
                .filter(|other| other.name == person.name)
                .count()
                > 1;
            let dim = gone || person.reconnecting;
            let word = control::state_word(view, person, offered);
            if person.is_you {
                self.your_row(ui, &view.voice, person, word, dim);
                let paused = paused_line(self.keys);
                let problem = voice_problem(&view.voice);
                if paused.is_some() || problem.is_some() {
                    controls::page(ui, |ui| {
                        if let Some(paused) = paused {
                            controls::text(ui, paused, theme::small(), WARN);
                        }
                        if let Some(problem) = problem {
                            controls::text(ui, problem, theme::small(), BAD);
                        }
                    });
                }
                continue;
            }
            let rect = row_rect(ui, ROW_HEIGHT);
            ui.painter().rect_filled(rect, 0, PANEL);
            let items = control::menu_items(view, person, offered);
            if !items.is_empty() {
                // Under the row's own buttons, which take their clicks first.
                let row = ui.interact(rect, ui.id().with(("row", person.key)), Sense::click());
                self.menu.follow_row(ui, &row, person.key, &person.name);
            }
            let rtt = person.rtt_ms.filter(|_| !dim).map(strip::round_trip);
            let rtt_width = rtt
                .as_deref()
                .map_or(0.0, |text| controls::text_width(ui, text, theme::small()));
            // Watch, then Control, each the height of its row.
            let mut presses = Vec::with_capacity(2);
            if let Some(word) = watch_word(view, person) {
                presses.push((RowPress::Watch, Button::new(word).height(ROW_HEIGHT)));
            }
            if let Some(control) = control::row_control(view, person, offered) {
                presses.push((RowPress::Control, control.button()));
            }
            let buttons: Vec<Button> = presses.iter().map(|(_, button)| *button).collect();
            let gap = ui.spacing().item_spacing.x;
            let buttons_width = controls::buttons_width(ui, &buttons);
            let right_width = if buttons.is_empty() {
                rtt_width
            } else if rtt_width > 0.0 {
                buttons_width + gap + rtt_width
            } else {
                buttons_width
            };
            let mut pressed = None;
            controls::split_row(
                ui,
                rect.shrink2(vec2(SIDE, 0.0)),
                right_width,
                |ui| row_name(ui, person, word, same_name, dim),
                // From the right edge: the round trip keeps its column
                // whether or not the row has buttons.
                |ui| {
                    if let Some(text) = &rtt {
                        let color = if stale {
                            ASH
                        } else {
                            theme::level_color(person.rtt_level)
                        };
                        controls::one_line(ui, text, theme::small(), color);
                    }
                    if !buttons.is_empty() {
                        pressed = controls::buttons(ui, &buttons, ROW_HEIGHT);
                    }
                },
            );
            let current = view.share.current.as_ref();
            match (pressed.map(|i| presses[i].0), current) {
                (Some(RowPress::Watch), Some(current)) => {
                    self.room.watch(current.number, !view.share.watching);
                }
                (Some(RowPress::Control), Some(current)) => self.room.ask_control(current.number),
                _ => {}
            }
            if let Some(MenuItem::EndControl) = self.menu.show(ui, person.key, &items) {
                self.room.end_control();
            }
        }
        if !view.people.is_empty() {
            controls::hairline(ui, ui.cursor().top());
        }
    }

    // Your row is 32 px, with Hold to talk in push-to-talk mode
    // while the hotkeys cannot do it, then Mute and Deafen.
    fn your_row(
        &mut self,
        ui: &mut Ui,
        voice: &Voice,
        person: &Person,
        word: Option<&str>,
        dim: bool,
    ) {
        let rect = row_rect(ui, CONTROL_HEIGHT);
        ui.painter().rect_filled(rect, 0, PANEL);
        let pointer_down = ui.input(|input| input.pointer.any_down());
        let hold = hold_shown(voice, self.keys, self.hold.stays(pointer_down));
        let buttons = your_buttons(voice, self.hold.held, hold);
        // Hold to talk keeps its size when it reads Talking, so Mute and
        // Deafen do not move under the mouse.
        let hold_width = [HOLD, TALKING]
            .iter()
            .map(|text| Button::new(text).width(ui))
            .fold(0.0, f32::max);
        let widths: Vec<f32> = buttons
            .iter()
            .enumerate()
            .map(|(i, (text, _))| {
                let width = Button::new(text).width(ui);
                if hold && i == 0 {
                    width.max(hold_width)
                } else {
                    width
                }
            })
            .collect();
        let gap = ui.spacing().item_spacing.x;
        let width = widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
        let mut held = false;
        let mut pressed = None;
        controls::split_row(
            ui,
            rect.shrink2(vec2(SIDE, 0.0)),
            width,
            |ui| row_name(ui, person, word, false, dim),
            |ui| {
                let layout = Layout::left_to_right(Align::Center);
                ui.allocate_ui_with_layout(vec2(width, rect.height()), layout, |ui| {
                    for (i, (text, color)) in buttons.iter().enumerate() {
                        let button = Button::new(text).color(*color);
                        if hold && i == 0 {
                            let response = button.min_width(hold_width).show(ui);
                            held = response.is_pointer_button_down_on();
                            continue;
                        }
                        if button.show(ui).clicked() {
                            let mute = i == usize::from(hold);
                            pressed = Some(if mute {
                                VoicePress::Mute
                            } else {
                                VoicePress::Deafen
                            });
                        }
                    }
                });
            },
        );
        if hold && let Some(held) = self.hold.drawn(held) {
            self.target.button(held);
        }
        match pressed {
            Some(VoicePress::Mute) => self.room.mute(!voice.muted),
            Some(VoicePress::Deafen) => self.room.deafen(!voice.deafened),
            None => {}
        }
    }
}

const HOLD: &str = "Hold to talk";
const TALKING: &str = "Talking";

// The words on your row's buttons, in order, and their colour: Hold to talk
// when `hold` says it shows, which reads Talking in amber while held and
// voice goes out, then Mute or Unmute, then Deafen or Undeafen. Held while
// muted, deafened or with no microphone, nothing goes out, and the button
// says so by not changing.
fn your_buttons(voice: &Voice, held: bool, hold: bool) -> Vec<(&'static str, Color32)> {
    let mut buttons = Vec::with_capacity(3);
    if hold {
        buttons.push(if held && voice.sending {
            (TALKING, AMBER)
        } else {
            (HOLD, CHALK)
        });
    }
    buttons.push((if voice.muted { "Unmute" } else { "Mute" }, CHALK));
    buttons.push((if voice.deafened { "Undeafen" } else { "Deafen" }, CHALK));
    buttons
}

// Hold to talk shows in push-to-talk mode only while the hotkeys
// cannot do its job, or while it `stays` under the mouse.
fn hold_shown(voice: &Voice, keys: hotkeys::State, stays: bool) -> bool {
    voice.mode == TalkMode::PushToTalk && (keys.button_needed() || stays)
}

// Said under your row while an administrator window has focus, with Hold to
// talk in the row until it clears.
fn paused_line(keys: hotkeys::State) -> Option<&'static str> {
    keys.paused.then_some(messages::HOTKEYS_PAUSED)
}

// The line under your row: why the microphone, or else the speakers, is not
// running. The room goes on without them. A wait for the last room's headset
// ends by itself and has no time limit, so a device that will not run is
// said before it rather than hidden behind it for the whole wait.
fn voice_problem(voice: &Voice) -> Option<String> {
    let err = [&voice.microphone, &voice.speakers]
        .into_iter()
        .flatten()
        .min_by_key(|err| matches!(err, AudioError::StillClosing { .. }))?;
    Some(messages::sentence(&err.to_string()))
}

// A talking person gets a 2 px amber ring before the name, and
// the name in Medium. The ring's place is kept on every row, so names do not
// move when someone starts talking.
const RING: f32 = 12.0;
const RING_GAP: f32 = 8.0;

fn name_font(person: &Person) -> FontId {
    if person.talking {
        theme::medium()
    } else {
        theme::body()
    }
}

fn row_name(ui: &mut Ui, person: &Person, word: Option<&str>, same_name: bool, dim: bool) {
    let (slot, _) = ui.allocate_exact_size(vec2(RING + RING_GAP, RING), Sense::hover());
    if person.talking && ui.is_rect_visible(slot) {
        let center = pos2(slot.left() + RING / 2.0, slot.center().y);
        let stroke = Stroke::new(2.0, AMBER);
        ui.painter().circle_stroke(center, RING / 2.0 - 1.0, stroke);
    }
    let name = if person.is_you {
        "You"
    } else {
        person.name.as_str()
    };
    let fingerprint = !person.is_you && (person.joined_by_invite || same_name);
    // The fingerprint and the state word are measured first so the name is
    // what gets cut.
    let gap = ui.spacing().item_spacing.x;
    let mut reserve = 0.0;
    if fingerprint {
        let font = theme::mono();
        reserve += ui.fonts_mut(|fonts| fonts.glyph_width(&font, '0'))
            * person.fingerprint.len() as f32
            + gap;
    }
    if let Some(word) = word {
        reserve += controls::text_width(ui, word, theme::small()) + gap;
    }
    let name_width = (ui.available_width() - reserve).max(0.0);
    ui.scope(|ui| {
        ui.set_max_width(name_width);
        let color = if dim { ASH } else { CHALK };
        controls::one_line(ui, name, name_font(person), color);
    });
    if fingerprint {
        controls::one_line(ui, &person.fingerprint, theme::mono(), ASH);
    }
    if let Some(word) = word {
        controls::one_line(ui, word, theme::small(), ASH);
    }
}

// A full-width row of the Ui it is in: inside the gutter on a page, edge to
// edge for list rows that carry a fill.
fn row_rect(ui: &mut Ui, height: f32) -> Rect {
    ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover())
        .0
}

#[cfg(test)]
mod tests {
    use super::*;
    use room::ReplyRefused;
    use room::view::{
        ControlView, CurrentShare, Numbers, Party, Paused, Refusal, RouterState, RunningShare,
        Strip,
    };

    // The control switch (control.rs): on once the app has an injector.
    const ON: bool = true;
    const OFF: bool = false;

    fn person(name: &str, is_you: bool) -> Person {
        Person {
            key: [u8::from(is_you); 32],
            name: name.to_owned(),
            fingerprint: String::from("a7f3 9c21 0d4e"),
            rtt_ms: None,
            rtt_level: Default::default(),
            is_you,
            is_host: !is_you,
            joined_by_invite: false,
            reconnecting: false,
            talking: false,
            sharing: false,
        }
    }

    fn code() -> ReplyView {
        ReplyView {
            state: ReplyState::Code {
                second_router: false,
            },
            code: String::from("booth1-r-m3k9q7"),
            expires_at_unix: 0,
            host_port: Some(41000),
        }
    }

    // A client in a room whose host went quiet, with this PC's code on show.
    fn in_room() -> View {
        View {
            role: Role::Client,
            room_name: String::from("Tuesday night"),
            strip: Strip {
                state: LinkState::Reconnecting,
                ..Strip::default()
            },
            people: vec![person("Mara", false), person("Tom", true)],
            invite: None,
            numbers: Numbers::default(),
            chat: Default::default(),
            notice: None,
            reply: Some(code()),
            paste: None,
            address_changed: None,
            list_problem: None,
            voice: Default::default(),
            share: Default::default(),
        }
    }

    #[test]
    fn code_block_in_room() {
        let view = in_room();
        assert_eq!(code_in_room(&view), Some(&code()));
        for notice in [Notice::LostHost, Notice::HostMoved] {
            let view = View {
                notice: Some(notice),
                ..in_room()
            };
            assert!(!ends_here(&view), "{:?} ends the room", view.notice);
            assert_eq!(code_in_room(&view), Some(&code()), "{:?}", view.notice);
        }
        let expired = ReplyView {
            state: ReplyState::Expired {
                second_router: false,
            },
            code: String::new(),
            ..code()
        };
        let view = View {
            reply: Some(expired.clone()),
            ..in_room()
        };
        assert_eq!(code_in_room(&view), Some(&expired));
    }

    #[test]
    fn composer_and_a_quiet_host() {
        let client = |state| View {
            strip: Strip {
                state,
                ..Strip::default()
            },
            ..in_room()
        };
        for state in [LinkState::Live, LinkState::Reconnecting] {
            assert!(chat_shown(&client(state)));
            assert!(composer_enabled(&client(state)), "{state:?}");
        }
        // Still shown to read back, but nothing can be sent.
        for state in [LinkState::Lost, LinkState::Closed] {
            assert!(chat_shown(&client(state)));
            assert!(!composer_enabled(&client(state)), "{state:?}");
        }
        let joining = View {
            people: Vec::new(),
            ..client(LinkState::Connecting)
        };
        assert!(!chat_shown(&joining));

        // The host is the room: its chat runs as long as the room does.
        let host = View {
            role: Role::Host,
            ..client(LinkState::Alone)
        };
        assert!(chat_shown(&host) && composer_enabled(&host));
        let failed = View {
            notice: Some(Notice::SocketFailed),
            ..host
        };
        assert!(!chat_shown(&failed) && !composer_enabled(&failed));
    }

    #[test]
    fn no_code_block_where_nobody_would_come_for_it() {
        for notice in [Notice::RoomClosed, Notice::SocketFailed] {
            let view = View {
                notice: Some(notice),
                ..in_room()
            };
            assert_eq!(code_in_room(&view), None, "{:?}", view.notice);
        }
        // The first packet back takes it away.
        let back = View {
            reply: None,
            ..in_room()
        };
        assert_eq!(code_in_room(&back), None);
        // Joining has its own screen for the code.
        let joining = View {
            people: Vec::new(),
            notice: Some(Notice::StillTrying),
            ..in_room()
        };
        assert_eq!(code_in_room(&joining), None);
        let host = View {
            role: Role::Host,
            ..in_room()
        };
        assert_eq!(code_in_room(&host), None);
    }

    fn invite() -> InviteView {
        InviteView {
            code: String::from("booth1-k7q2m9x4"),
            multi_use: false,
            expires_at_unix: 0,
            used: false,
            expired: false,
            router: RouterState::Easy,
            mapped_since: false,
            address_changed_since: false,
        }
    }

    #[test]
    fn an_invite_made_before_the_address_changed_says_so() {
        assert_eq!(outdated_by(&invite()), None);
        let moved = InviteView {
            address_changed_since: true,
            ..invite()
        };
        assert_eq!(outdated_by(&moved), Some(messages::ADDRESS_CHANGED_SINCE));
        let mapped = InviteView {
            mapped_since: true,
            ..invite()
        };
        assert_eq!(outdated_by(&mapped), Some(messages::MAPPED_SINCE));
        let both = InviteView {
            mapped_since: true,
            ..moved.clone()
        };
        assert_eq!(outdated_by(&both), Some(messages::ADDRESS_CHANGED_SINCE));
        // A used or expired invite already shows New invite and says why.
        for dead in [
            InviteView {
                used: true,
                ..moved.clone()
            },
            InviteView {
                expired: true,
                ..moved.clone()
            },
        ] {
            assert_eq!(outdated_by(&dead), None);
        }
    }

    #[test]
    fn paste_line_after_address_change() {
        let cannot_help = Some((String::from(messages::CODES_CANNOT_HELP), ASH));
        assert_eq!(paste_line(None, true, None), cannot_help);
        assert_eq!(paste_line(None, true, Some(&PasteState::Sent)), cannot_help);
        let too_soon = PasteState::Refused(ReplyRefused::TooSoon);
        assert_eq!(paste_line(None, true, Some(&too_soon)), cannot_help);
        // Something that is not a code at all is still answered.
        let typed = "This is an invite, not a reply code. Paste it in Join.";
        assert_eq!(
            paste_line(Some(typed), true, None),
            Some((typed.to_owned(), BAD))
        );
    }

    #[test]
    fn paste_line_last_paste() {
        assert_eq!(paste_line(None, false, None), None);
        assert_eq!(
            paste_line(None, false, Some(&PasteState::Sent)),
            Some((String::from(messages::PASTE_SENT), ASH))
        );
        assert_eq!(paste_line(None, false, Some(&PasteState::Joined)), None);
        let too_soon = PasteState::Refused(ReplyRefused::TooSoon);
        let (_, color) = paste_line(None, false, Some(&too_soon)).unwrap();
        assert_eq!(color, BAD);
    }

    fn voice(mode: TalkMode) -> Voice {
        Voice {
            mode,
            ..Voice::default()
        }
    }

    fn words(buttons: &[(&'static str, Color32)]) -> Vec<&'static str> {
        buttons.iter().map(|(text, _)| *text).collect()
    }

    // Hold to talk before Mute and Deafen in push-to-talk mode, reading
    // Talking in amber while held; each of the other two reads what pressing
    // it does.
    #[test]
    fn your_row_says_what_each_button_does_now() {
        let push = voice(TalkMode::PushToTalk);
        assert_eq!(
            words(&your_buttons(&push, false, true)),
            ["Hold to talk", "Mute", "Deafen"]
        );
        assert!(
            your_buttons(&push, false, true)
                .iter()
                .all(|(_, color)| *color == CHALK)
        );
        let sending = Voice {
            sending: true,
            ..push.clone()
        };
        assert_eq!(your_buttons(&sending, true, true)[0], ("Talking", AMBER));
        // Held while muted, deafened or with no microphone, nothing goes out.
        let muted = Voice {
            muted: true,
            ..push.clone()
        };
        assert_eq!(your_buttons(&muted, true, true)[0], ("Hold to talk", CHALK));
        // Let go, the last frame is still on its way out.
        assert_eq!(
            your_buttons(&sending, false, true)[0],
            ("Hold to talk", CHALK)
        );

        let open = voice(TalkMode::OpenMic);
        assert_eq!(
            words(&your_buttons(&open, false, false)),
            ["Mute", "Deafen"]
        );
        let quiet = Voice {
            muted: true,
            deafened: true,
            ..open
        };
        assert_eq!(
            words(&your_buttons(&quiet, false, false)),
            ["Unmute", "Undeafen"]
        );
    }

    // The hotkeys do push to talk while they can hear keys. While an
    // administrator window has focus, or when they did not start, the
    // button is back, and the pause is said under your row.
    #[test]
    fn hold_to_talk_shown() {
        let live = hotkeys::State {
            running: true,
            paused: false,
        };
        let paused = hotkeys::State {
            paused: true,
            ..live
        };
        let push = voice(TalkMode::PushToTalk);
        assert!(!hold_shown(&push, live, false));
        assert_eq!(
            words(&your_buttons(&push, false, hold_shown(&push, live, false))),
            ["Mute", "Deafen"]
        );
        assert!(hold_shown(&push, paused, false));
        assert!(hold_shown(&push, hotkeys::State::default(), false));
        assert!(!hold_shown(&voice(TalkMode::OpenMic), paused, false));
        assert!(!hold_shown(&voice(TalkMode::OpenMic), live, true));

        assert_eq!(paused_line(live), None);
        assert_eq!(
            paused_line(paused),
            Some("Hotkeys paused while an administrator window has focus.")
        );
        assert_eq!(paused_line(hotkeys::State::default()), None);
    }

    // Held with the mouse, then the stats panel opened over the list, or a
    // notice took its place: the next frame without the button lets go.
    #[test]
    fn hold_lets_go_when_not_drawn() {
        let mut hold = Hold::default();
        hold.frame_begins();
        assert_eq!(hold.drawn(true), Some(true));
        assert_eq!(hold.frame_ends(), None);
        hold.frame_begins();
        assert_eq!(hold.drawn(true), None, "the room is told once");
        assert_eq!(hold.frame_ends(), None);

        hold.frame_begins();
        assert_eq!(hold.frame_ends(), Some(false));
        assert!(!hold.held);
        hold.frame_begins();
        assert_eq!(hold.frame_ends(), None, "and once");

        // Back on screen with the mouse still down, it takes up again.
        hold.frame_begins();
        assert_eq!(hold.drawn(true), Some(true));
        assert_eq!(hold.frame_ends(), None);
        hold.frame_begins();
        assert_eq!(hold.drawn(false), Some(false));
        assert_eq!(hold.frame_ends(), None);
    }

    // One frame of your row as your_row runs it: whether Hold to talk shows,
    // and what the room is told, if anything. `on_it` is the mouse down on
    // the button, which it can only be while the button shows.
    fn frame(
        hold: &mut Hold,
        keys: hotkeys::State,
        pointer_down: bool,
        on_it: bool,
    ) -> (bool, Option<bool>) {
        hold.frame_begins();
        let shown = hold_shown(&voice(TalkMode::PushToTalk), keys, hold.stays(pointer_down));
        let told = if shown { hold.drawn(on_it) } else { None };
        let ends = hold.frame_ends();
        (shown, told.or(ends))
    }

    // An administrator game has focus and the panel sits on another screen.
    // Pressing Hold to talk brings the panel to the front, which ends the
    // pause, and the button stays under the mouse until it is let go.
    #[test]
    fn hold_to_talk_stays_while_held() {
        let live = hotkeys::State {
            running: true,
            paused: false,
        };
        let paused = hotkeys::State {
            paused: true,
            ..live
        };

        // The press is read on a pass that still has the pause.
        let mut hold = Hold::default();
        assert_eq!(frame(&mut hold, paused, false, false), (true, None));
        assert_eq!(frame(&mut hold, paused, true, true), (true, Some(true)));
        assert_eq!(frame(&mut hold, live, true, true), (true, None));
        assert_eq!(frame(&mut hold, live, false, false), (true, Some(false)));
        assert_eq!(frame(&mut hold, live, false, false), (false, None));

        // The pause ended on the pass that reads the press.
        let mut hold = Hold::default();
        assert_eq!(frame(&mut hold, paused, false, false), (true, None));
        assert_eq!(frame(&mut hold, live, true, true), (true, Some(true)));
        assert_eq!(frame(&mut hold, live, false, false), (true, Some(false)));
        assert_eq!(frame(&mut hold, live, false, false), (false, None));

        // Not on screen before, a mouse held down brings nothing back.
        let mut hold = Hold::default();
        assert_eq!(frame(&mut hold, live, true, false), (false, None));
        // And a frame that could not draw your row lets go as before.
        let mut hold = Hold::default();
        frame(&mut hold, paused, true, true);
        hold.frame_begins();
        assert_eq!(hold.frame_ends(), Some(false));
        assert_eq!(frame(&mut hold, live, true, true), (false, None));
    }

    #[test]
    fn a_talking_person_is_set_in_medium() {
        let quiet = person("Mara", false);
        let talking = Person {
            talking: true,
            ..person("Mara", false)
        };
        assert_eq!(name_font(&quiet), theme::body());
        assert_eq!(name_font(&talking), theme::medium());
    }

    // A microphone that cannot open is said under your row, as a sentence,
    // and the room goes on for listening.
    #[test]
    fn device_problem_under_your_row() {
        use voice::audio::{AudioError, Direction};
        assert_eq!(voice_problem(&voice(TalkMode::PushToTalk)), None);
        let no_mic = Voice {
            microphone: Some(AudioError::NoDevice(Direction::Input)),
            ..voice(TalkMode::PushToTalk)
        };
        assert_eq!(
            voice_problem(&no_mic).as_deref(),
            Some(
                "Could not open the microphone: Windows has no microphone turned on. Plug one in, or turn one on in Settings, System, Sound."
            )
        );
        let lost_speakers = Voice {
            speakers: Some(AudioError::Lost {
                direction: Direction::Output,
                name: String::from("Headset"),
            }),
            ..voice(TalkMode::OpenMic)
        };
        let line = voice_problem(&lost_speakers).unwrap();
        assert!(
            line.starts_with("Headset stopped: it was unplugged"),
            "{line}"
        );
        // The microphone's comes first when both are out.
        let both = Voice {
            microphone: no_mic.microphone.clone(),
            ..lost_speakers.clone()
        };
        assert_eq!(voice_problem(&both), voice_problem(&no_mic));
    }

    // The wait for the last room's headset has no time limit, so speakers
    // that will not run are not hidden behind the microphone's wait.
    #[test]
    fn failed_device_before_closing_one() {
        use voice::audio::{AudioError, Direction};
        let closing = |direction| {
            Some(AudioError::StillClosing {
                direction,
                default: true,
            })
        };
        let waiting = Voice {
            microphone: closing(Direction::Input),
            ..voice(TalkMode::OpenMic)
        };
        let line = voice_problem(&waiting).unwrap();
        assert!(
            line.starts_with("The last room's microphone is still closing."),
            "{line}"
        );
        let in_use = AudioError::InUse {
            direction: Direction::Output,
            name: String::from("Headset"),
        };
        let speakers_out = Voice {
            speakers: Some(in_use.clone()),
            ..waiting.clone()
        };
        assert_eq!(
            voice_problem(&speakers_out),
            Some(messages::sentence(&in_use.to_string()))
        );
        // Both waiting, the microphone's comes first as for any two lines.
        let both = Voice {
            speakers: closing(Direction::Output),
            ..waiting
        };
        assert_eq!(voice_problem(&both), Some(line));
    }

    // Mara shares, as the view has it on everyone's panel.
    fn mara_shares(yours: bool) -> CurrentShare {
        CurrentShare {
            key: [u8::from(yours); 32],
            name: String::from(if yours { "Tom" } else { "Mara" }),
            number: 7,
            fps: 120,
            yours,
            watchers: None,
        }
    }

    // A client whose link is live.
    fn live(share: ShareView) -> View {
        View {
            strip: Strip {
                state: LinkState::Live,
                ..Strip::default()
            },
            reply: None,
            share,
            ..in_room()
        }
    }

    fn verb(view: &View) -> Option<(&'static str, Color32, Option<String>, bool)> {
        let words = share_verb(view, ON)?.words();
        Some((words.text, words.color, words.label, words.enabled))
    }

    // Share, Stop sharing in amber while sharing, and
    // Share in ash that does nothing while someone else shares, whose
    // accessible name says who. Asked and not granted yet, it says so.
    #[test]
    fn the_title_row_says_what_share_does_now() {
        let idle = live(ShareView::default());
        assert_eq!(verb(&idle), Some(("Share", CHALK, None, true)));
        let sharing = live(ShareView {
            own: OwnShare::Sharing {
                number: 7,
                fps: 120,
            },
            current: Some(mara_shares(true)),
            ..ShareView::default()
        });
        assert_eq!(verb(&sharing), Some(("Stop sharing", AMBER, None, true)));
        let asking = live(ShareView {
            own: OwnShare::Asking { fps: 120 },
            ..ShareView::default()
        });
        assert_eq!(verb(&asking), Some(("Starting", ASH, None, false)));
        let busy = live(ShareView {
            current: Some(mara_shares(false)),
            ..ShareView::default()
        });
        let spoken = Some(String::from("Share. Mara is sharing. One share at a time."));
        assert_eq!(verb(&busy), Some(("Share", ASH, spoken.clone(), false)));
        // Someone else's share wins over an ask the host will refuse.
        let refused = live(ShareView {
            own: OwnShare::Asking { fps: 60 },
            current: Some(mara_shares(false)),
            ..ShareView::default()
        });
        assert_eq!(verb(&refused), Some(("Share", ASH, spoken, false)));
        // After a refusal, Share is Share again.
        let after = live(ShareView {
            own: OwnShare::Refused(Refusal::TooSoon),
            ..ShareView::default()
        });
        assert_eq!(verb(&after), Some(("Share", CHALK, None, true)));
    }

    // Stop control takes Share's place, in amber like Stop sharing, and stays
    // the way out while the room has not yet said the lost host ended it.
    #[test]
    fn stop_control_takes_share_s_place_while_controlling() {
        let controlling = live(ShareView {
            current: Some(mara_shares(false)),
            watching: true,
            control: ControlView {
                controller: Some([1; 32]),
                controlling: Some(Party {
                    key: [0; 32],
                    name: String::from("Mara"),
                }),
                ..ControlView::default()
            },
            ..ShareView::default()
        });
        assert_eq!(
            verb(&controlling),
            Some(("Stop control", AMBER, None, true))
        );
        let lost = View {
            strip: Strip {
                state: LinkState::Lost,
                ..Strip::default()
            },
            ..controlling.clone()
        };
        assert_eq!(share_verb(&lost, ON), Some(ShareVerb::StopControl));
        // Asked and not answered yet: Share stays as it was.
        let asking = live(ShareView {
            current: Some(mara_shares(false)),
            watching: true,
            control: ControlView {
                asking: Some(7),
                ..ControlView::default()
            },
            ..ShareView::default()
        });
        assert_eq!(
            share_verb(&asking, ON),
            Some(ShareVerb::Busy(String::from("Mara")))
        );

        // With the switch off there is no Stop control, even where the view
        // says this PC controls, and the title row is what it would be
        // without control.
        assert_eq!(
            share_verb(&controlling, OFF),
            Some(ShareVerb::Busy(String::from("Mara")))
        );
        assert_eq!(share_verb(&lost, OFF), None);
    }

    #[test]
    fn share_is_offered_only_where_it_can_work() {
        // A quiet host is not a lost one.
        assert_eq!(share_verb(&in_room(), ON), Some(ShareVerb::Share));
        for state in [LinkState::Lost, LinkState::Closed] {
            let view = View {
                strip: Strip {
                    state,
                    ..Strip::default()
                },
                ..in_room()
            };
            assert_eq!(share_verb(&view, ON), None, "{state:?}");
            // The roster the host sent last is no reason to show a share.
            let stale = View {
                share: ShareView {
                    current: Some(mara_shares(false)),
                    ..ShareView::default()
                },
                ..view
            };
            assert_eq!(share_verb(&stale, ON), None, "{state:?}");
        }
        let joining = View {
            people: Vec::new(),
            ..in_room()
        };
        assert_eq!(share_verb(&joining, ON), None);
        let host = View {
            role: Role::Host,
            strip: Strip::default(),
            ..in_room()
        };
        assert_eq!(share_verb(&host, ON), Some(ShareVerb::Share));
        let failed = View {
            notice: Some(Notice::SocketFailed),
            ..host
        };
        assert_eq!(share_verb(&failed, ON), None);
    }

    // The sharer's row says sharing, with Watch for everyone else,
    // which reads Stop watching while this PC watches. Your own row says
    // sharing while you share, and has no Watch.
    #[test]
    fn sharer_row_watch() {
        let mara = Person {
            sharing: true,
            ..person("Mara", false)
        };
        let view = live(ShareView {
            current: Some(mara_shares(false)),
            ..ShareView::default()
        });
        assert_eq!(control::state_word(&view, &mara, ON), Some("sharing"));
        assert_eq!(
            control::state_word(&view, &person("Jonas", false), ON),
            None
        );
        assert_eq!(watch_word(&view, &mara), Some("Watch"));
        let watching = View {
            share: ShareView {
                watching: true,
                ..view.share.clone()
            },
            ..view.clone()
        };
        assert_eq!(watch_word(&watching, &mara), Some("Stop watching"));
        let jonas = Person {
            key: [5; 32],
            ..person("Jonas", false)
        };
        assert_eq!(watch_word(&view, &jonas), None);
        let lost = View {
            strip: Strip {
                state: LinkState::Lost,
                ..Strip::default()
            },
            ..view.clone()
        };
        assert_eq!(watch_word(&lost, &mara), None);

        let you = Person {
            sharing: true,
            ..person("Tom", true)
        };
        let yours = live(ShareView {
            own: OwnShare::Sharing {
                number: 7,
                fps: 120,
            },
            current: Some(mara_shares(true)),
            ..ShareView::default()
        });
        assert_eq!(control::state_word(&yours, &you, ON), Some("sharing"));
        assert_eq!(watch_word(&yours, &you), None);
        assert_eq!(watch_word(&yours, &mara), None);
    }

    // From asking to share until the capture has closed, the
    // invite row reads "Hidden while you share" in ash in place of the code,
    // by the same rule as the stats panel's address lines. Copy stays, and
    // copies the code itself.
    #[test]
    fn invite_hidden_while_sharing() {
        let capture = |paused| {
            Some(RunningShare {
                software: false,
                paused,
            })
        };
        let sharing = OwnShare::Sharing { number: 1, fps: 60 };
        let busy = Refusal::Busy {
            name: String::from("Mara"),
        };
        let cases = [
            (OwnShare::Off, None, false),
            (OwnShare::Asking { fps: 60 }, None, true),
            // Granted, before the capture and the encoder are open.
            (sharing.clone(), None, true),
            (sharing.clone(), capture(None), true),
            (sharing, capture(Some(Paused::SecureDesktop)), true),
            // Stop sharing pressed, and the thread still holds the screen.
            (OwnShare::Off, capture(None), true),
            (OwnShare::Refused(Refusal::TooSoon), None, false),
            (OwnShare::Refused(busy), None, false),
            (OwnShare::Refused(Refusal::NotLive), None, false),
        ];
        let used = InviteView {
            used: true,
            ..invite()
        };
        let expired = InviteView {
            expired: true,
            ..invite()
        };
        let hidden = ("Hidden while you share", false, ASH);
        for (own, running, hide) in cases {
            let share = ShareView {
                own: own.clone(),
                running,
                ..ShareView::default()
            };
            assert_eq!(addresses_hidden(&share), hide, "{own:?}");
            let shown = |color| {
                if hide {
                    hidden
                } else {
                    ("booth1-k7q2m9x4", true, color)
                }
            };
            assert_eq!(invite_text(&invite(), &share), shown(CHALK), "{own:?}");
            assert_eq!(invite_text(&used, &share), shown(ASH), "{own:?}");
            assert_eq!(invite_text(&expired, &share), shown(ASH), "{own:?}");
        }
    }

    fn picks() -> Vec<Pick> {
        ["2560x1440, left", "1920x1080, right"]
            .into_iter()
            .enumerate()
            .map(|(i, text)| Pick {
                id: room::MonitorId {
                    device_name: format!(r"\\.\DISPLAY{}", i + 1),
                    adapter_luid: 7,
                },
                fps: 120,
                text: text.to_owned(),
                spoken: format!("Share the monitor, {text}"),
            })
            .collect()
    }

    #[test]
    fn one_click_in_the_monitor_list_picks_that_monitor() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::Context;
        let ctx = Context::default();
        theme::apply(&ctx);
        let picks = picks();
        let (second, _) = frame(&ctx, Vec::new(), |ui| {
            assert_eq!(monitor_row(ui, &picks), None);
            let first = Button::new(&picks[0].text).width(ui);
            let gap = ui.spacing().item_spacing.x;
            pos2(SIDE + first + gap + 10.0, 8.0 + CONTROL_HEIGHT / 2.0)
        });
        frame(&ctx, press(second), |ui| monitor_row(ui, &picks));
        let (pressed, _) = frame(&ctx, release(second), |ui| monitor_row(ui, &picks));
        assert_eq!(pressed, Some(1));
        let first = pos2(SIDE + 10.0, 8.0 + CONTROL_HEIGHT / 2.0);
        frame(&ctx, press(first), |ui| monitor_row(ui, &picks));
        let (pressed, _) = frame(&ctx, release(first), |ui| monitor_row(ui, &picks));
        assert_eq!(pressed, Some(0));
    }
}
