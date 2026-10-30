use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::emath::GuiRounding;
use eframe::egui::{
    Align, Color32, Context, CursorIcon, Key, Layout, Modifiers, Rect, Response, ScrollArea, Sense,
    Stroke, Ui, WidgetInfo, WidgetType, vec2,
};
use invite::ReplyCode;
use room::view::{
    InviteView, LinkState, MappingWord, NameView, Notice, OwnShare, OwnSound, PasteState, Person,
    ReplyState, ReplyView, Role, ShareView, TalkMode, View, Voice,
};
use room::{MOST_VOLUME, Room};
use voice::audio::{AudioError, Choice};

use crate::controls::{self, Button, Lead};
use crate::hotkeys::{self, Target};
use crate::messages;
use crate::monitors::{self, Pick};
use crate::region::{self, Picked};
use crate::screens::chat::{Chat, Composer};
use crate::screens::control::{
    self, AllowWait, Answer, Decline, MenuItem, PanicKey, RowMenu, SoundWord,
};
use crate::screens::stats;
use crate::sound::PeriodsAsk;
use crate::strip;
use crate::theme::{
    self, AMBER, ASH, BAD, CHALK, CONTROL_HEIGHT, FIELD_GAP, HALF_STEP, PANEL, QUIET_DOT,
    ROW_HEIGHT, SIDE, STEP, TALKING, WARN,
};
use crate::volumes::Volumes;
use crate::win;

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
    // open, and the windows under them.
    monitors: Option<Vec<Pick>>,
    windows: Vec<WindowPick>,
    list_room: ListRoom,
    // The region picker's answer, while it is open.
    region_pick: Option<Receiver<Result<Option<Picked>, String>>>,
    // The share started last from the panel was a region or a window: the
    // share key then opens the list rather than show a whole monitor.
    list_next: bool,
    panic: PanicKey,
    allow_wait: AllowWait,
    block_wait: BlockWait,
    decline: Decline,
    menu: RowMenu,
    // How loud this PC plays each person, and the PC sound of their
    // shares, set in their row's menu and written when it closes.
    volumes: Volumes,
    sound_volumes: Volumes,
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
    // For the log, which the panel has no line for.
    Log(String),
}

impl InRoom {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        room: Arc<Room>,
        typed_room_name: String,
        log_file: Option<PathBuf>,
        firewall: &'static str,
        devices: [Choice; 2],
        target: Target,
        panic: PanicKey,
        [volumes, sound_volumes]: [Volumes; 2],
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
            region_pick: None,
            windows: Vec::new(),
            list_room: ListRoom::default(),
            list_next: false,
            panic,
            allow_wait: AllowWait::default(),
            block_wait: BlockWait::default(),
            decline: Decline::default(),
            menu: RowMenu::default(),
            volumes,
            sound_volumes,
        }
    }

    // From the app's logic while the panel is minimized: it comes back with
    // Resume and Share PC sound waiting as if their block had just appeared,
    // so a click meant for a game as the panel comes forward neither resumes
    // a pause nor turns PC sound on.
    pub fn out_of_sight(&mut self) {
        self.block_wait.out_of_sight();
    }

    // The volume files' log lines since the last frame, with the paths their
    // line leaves out.
    pub fn take_log(&mut self) -> Vec<String> {
        [&mut self.volumes, &mut self.sound_volumes]
            .into_iter()
            .filter_map(Volumes::take_log)
            .collect()
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

    pub fn list_next(&self) -> bool {
        self.list_next
    }

    // For the share key after a region or window share: the list, as Share
    // opens it.
    pub fn open_share_list(&mut self) {
        if let Ok(list) = monitors::list()
            && !list.is_empty()
        {
            if self.monitors.is_some() {
                self.list_room.refilled(Instant::now());
            }
            self.monitors = Some(monitors::picks(&list));
            self.windows = window_picks();
        }
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
        let now = Instant::now();
        let mut step = None;
        let title = if view.room_name.is_empty() {
            "Booth"
        } else {
            view.room_name.as_str()
        };
        // The way out is always at the right of the title row, also when the
        // room is over or never started, so it is never a lone word under
        // the sentence that explains why.
        let share = share_verb(view, self.control_offered);
        if share != Some(ShareVerb::Share) {
            self.monitors = None;
        }
        let words = share.as_ref().map(ShareVerb::words);
        let mut verbs: Vec<Button> = words.iter().map(VerbWords::button).collect();
        let (exit, role) = way_out(view);
        verbs.push(Button::new(exit).role(role));
        match controls::title_row(ui, Lead::Title(title), &verbs) {
            Some(0) if share.is_some() => self.share_pressed(share.as_ref()),
            Some(_) => step = Some(Step::Leave),
            None => {}
        }
        if self.monitors.is_some()
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.monitors = None;
            self.list_room.clear();
        }
        let top = ui.cursor().top();
        // Whether a block in panel tone ends right above the body.
        let mut block = false;
        if let Some(picks) = &self.monitors {
            block = true;
            let picked = share_list(ui, picks, &self.windows);
            if let Some((room, left)) = self.list_room.refill_room(now) {
                keep_room(ui, top, room);
                ui.ctx().request_repaint_after(left);
            }
            self.list_room.measured(ui.cursor().top() - top);
            match picked {
                Some(RowPick::Monitor(i)) => {
                    let pick = &picks[i];
                    self.room.share(pick.fps, Some(pick.id.clone()));
                    step = Some(Step::Remember(pick.id.device_name.clone()));
                    self.monitors = None;
                    self.list_next = false;
                }
                Some(RowPick::Region) => {
                    self.monitors = None;
                    step = self.pick_region(ui.ctx());
                }
                Some(RowPick::Window(i)) => {
                    let id = self.windows[i].id;
                    if id.exists() {
                        let fps = room::window_monitor(id).map_or(monitors::MOST_FPS, |monitor| {
                            monitors::fps(monitor.refresh_hz)
                        });
                        self.room.share_window(fps, id);
                        self.monitors = None;
                        self.list_next = true;
                    } else {
                        // Closed since the list was taken: a list as it is
                        // now, rather than a share that cannot start.
                        self.windows = window_picks();
                        self.list_room.refilled(now);
                    }
                }
                None => {}
            }
        }
        // However the list went, what was under it must not move up yet.
        if self.monitors.is_none() {
            self.list_room.give_way(now);
        }
        if let Some(answer) = &self.region_pick {
            match answer.try_recv() {
                Ok(Ok(Some(picked))) => {
                    let fps = monitors::fps(picked.refresh_hz);
                    self.room.share_region(fps, picked.monitor, picked.region);
                    self.list_next = true;
                    self.region_pick = None;
                }
                Ok(Err(why)) => {
                    step = Some(Step::Log(why));
                    self.region_pick = None;
                }
                Ok(Ok(None)) | Err(TryRecvError::Disconnected) => self.region_pick = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let waits = self.block_wait.follow(&view.share, now);
        for left in [waits.placed, waits.switch].into_iter().flatten() {
            ui.ctx().request_repaint_after(left);
        }
        if let Some(running) = &view.share.running {
            block = true;
            match share_block(ui, running.held, &view.share.own_sound, waits) {
                Some(SharePress::Resume) => self.room.pause_share(false),
                Some(SharePress::Sound(on)) => self.room.share_sound(on),
                None => {}
            }
        }
        block |= self.request(ui, view);
        // After the request block, which comes only while the share runs, so
        // the room closing never moves Allow.
        let placed = waits.placed.is_none();
        if let Some((room, left)) = self.list_room.follow(&view.share, placed, now) {
            block = true;
            keep_room(ui, top, room);
            if let Some(left) = left {
                ui.ctx().request_repaint_after(left);
            }
        }
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
                Role::Host => self.host(ui, view, block, &mut step),
                Role::Client => self.client(ui, view, block, &mut step),
            });
            return step;
        }
        // At the smallest window the invite block and eight people are taller
        // than the body. They scroll on their own once they would take more
        // than half of it, so the chat, its composer included, keeps the
        // other half, less the 8 px under the last row: they are the list's
        // edge, not one of its lines.
        let most = (ui.available_height() / 2.0).max(ROW_HEIGHT) + STEP;
        let scroll = ScrollArea::vertical()
            .id_salt("room")
            .max_height(most)
            .auto_shrink([false, true]);
        scroll.show(ui, |ui| match view.role {
            Role::Host => self.host(ui, view, block, &mut step),
            Role::Client => self.client(ui, view, block, &mut step),
        });
        self.chat
            .show(ui, &view.chat, &view.people, &self.room, composer(view));
        step
    }

    // Share opens the list of monitors, Region and the windows under the
    // title row, one monitor or more, and Share again closes it. Stop
    // sharing needs no answer.
    fn share_pressed(&mut self, verb: Option<&ShareVerb>) {
        match verb {
            Some(ShareVerb::Share) if self.monitors.is_some() => {
                self.monitors = None;
                self.list_room.clear();
            }
            Some(ShareVerb::Share) => match monitors::list() {
                Ok(list) if !list.is_empty() => {
                    self.monitors = Some(monitors::picks(&list));
                    self.windows = window_picks();
                }
                // The room tries the primary monitor, and when that cannot
                // open either, its problem line in the chat says why.
                Ok(_) | Err(_) => self.room.share(monitors::MOST_FPS, None),
            },
            Some(ShareVerb::StopSharing) => self.room.stop_sharing(),
            Some(ShareVerb::StopControl) => self.room.stop_control(),
            Some(ShareVerb::Starting | ShareVerb::Busy(_)) | None => {}
        }
    }

    // The control request block, under the title row whatever the body
    // shows, the stats panel included: who may use this PC is never out
    // of sight while it is being decided or done. True when it shows.
    fn request(&mut self, ui: &mut Ui, view: &View) -> bool {
        let request = control::request(view, self.control_offered);
        let wait = self.allow_wait.follow(request, Instant::now());
        if let Some(left) = wait {
            ui.ctx().request_repaint_after(left);
        }
        let Some(request) = request else {
            return false;
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
        true
    }

    // `block` is true when a block in panel tone ends right above.
    fn host(&mut self, ui: &mut Ui, view: &View, block: bool, step: &mut Option<Step>) {
        if let Some(notice) = &view.notice {
            controls::page(ui, |ui| explain(ui, notice));
            return;
        }
        let mut block = block;
        // Above the invite block, in the window's tone, since a new invite
        // is one of the two ways back.
        if let Some(text) = messages::friends_lost(view.address_changed) {
            controls::page(ui, |ui| {
                controls::prose(ui, text, theme::body(), CHALK);
            });
            block = false;
        }
        if let Some(invite) = &view.invite {
            block = true;
            controls::region(ui, |ui| {
                self.invite(
                    ui,
                    invite,
                    view.numbers.local_port,
                    view.numbers.address_name.as_ref(),
                    &view.share,
                    step,
                );
                // A punch from a router that changes ports opens a port
                // nobody will come to, so the field is not offered then.
                let codes_help = view.numbers.mapping != Some(MappingWord::Hard);
                if !invite.code.is_empty() && codes_help {
                    let moved = view
                        .address_changed
                        .is_some_and(|changed| changed.codes_cannot_help);
                    self.paste_field(ui, view.paste.as_ref(), moved, view.numbers.local_port);
                }
            });
        }
        list_top(ui, block);
        let row_last = self.people(ui, view);
        // 8 px under a row, which the list has left already, and 16 under
        // the line about the microphone, so the two sentences stay apart.
        if view.people.iter().all(|person| person.is_you) {
            let top = if row_last { 0.0 } else { STEP };
            controls::gutter(ui, top, STEP, |ui| {
                controls::prose(ui, messages::EMPTY_ROOM, theme::body(), ASH);
            });
        }
    }

    fn invite(
        &mut self,
        ui: &mut Ui,
        invite: &InviteView,
        port: u16,
        name: Option<&NameView>,
        share: &ShareView,
        step: &mut Option<Step>,
    ) {
        if lifetime_row(ui, invite.multi_use) {
            *step = Some(Step::NewInvite {
                multi_use: !invite.multi_use,
            });
        }

        let dead = invite.used || invite.expired;
        let outdated = outdated_by(invite);
        if !invite.code.is_empty() {
            ui.add_space(STEP);
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

        // One sentence that belongs to the code above it, in caption, right
        // under its 32 px row, which already leaves room under the code.
        let line = if invite.used {
            String::from("This invite has been used.")
        } else if invite.expired {
            String::from("This invite has expired.")
        } else if invite.multi_use && !invite.code.is_empty() {
            String::from(messages::ANYONE)
        } else {
            messages::router(invite.router, port)
        };
        controls::text(ui, line, theme::caption(), ASH);
        if let Some(why) = outdated {
            controls::text(ui, why, theme::caption(), ASH);
        }
        // The name is as private as the address it leads to.
        if !addresses_hidden(share)
            && let Some(line) = messages::name_line(Role::Host, name)
        {
            controls::text(ui, line, theme::caption(), ASH);
        }
        if let Some(error) = &self.copy_error {
            ui.add_space(HALF_STEP);
            controls::text(ui, error.as_str(), theme::caption(), BAD);
        }
    }

    // Enter hands the code to the room, which checks it and punches. The
    // one line under the field says what came of it. `moved` is this PC's
    // own address having just changed with friends still out.
    fn paste_field(&mut self, ui: &mut Ui, last: Option<&PasteState>, moved: bool, port: u16) {
        ui.add_space(FIELD_GAP);
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
        if let Some((text, color)) = paste_line(self.paste_error.as_deref(), moved, last, port) {
            ui.add_space(HALF_STEP);
            controls::text(ui, text, theme::caption(), color);
        }
    }

    // `block` as for host. Cancel, or Leave once the room has ended, is in
    // the title row.
    fn client(&mut self, ui: &mut Ui, view: &View, block: bool, step: &mut Option<Step>) {
        if view.people.is_empty() {
            match &view.notice {
                Some(Notice::StillTrying) => self.still_trying(ui, view, step),
                Some(notice) => controls::page(ui, |ui| explain(ui, notice)),
                None => {}
            }
            return;
        }
        let mut block = block;
        if let Some(notice) = &view.notice {
            controls::page(ui, |ui| explain(ui, notice));
            block = false;
        }
        if let Some(reply) = code_in_room(view) {
            block = true;
            if self.code_region(ui, reply) {
                *step = Some(Step::NewCode);
            }
        }
        list_top(ui, block);
        self.people(ui, view);
    }

    // Joining when the host cannot be reached: the code to send back in its
    // own block, or the sentence that says why none would help, under the
    // line that says the tries go on.
    fn still_trying(&mut self, ui: &mut Ui, view: &View, step: &mut Option<Step>) {
        controls::page(ui, |ui| {
            let still = messages::notice(&Notice::StillTrying);
            controls::prose(ui, still, theme::body(), CHALK);
            if let Some(line) =
                messages::name_line(Role::Client, view.numbers.address_name.as_ref())
            {
                ui.add_space(HALF_STEP);
                controls::prose(ui, line, theme::body(), CHALK);
            }
        });
        if let Some(reply) = view.reply.as_ref()
            && self.code_region(ui, reply)
        {
            *step = Some(Step::NewCode);
        }
    }

    // The code block in its panel-tone region. True when New code was
    // pressed.
    fn code_region(&mut self, ui: &mut Ui, reply: &ReplyView) -> bool {
        controls::region(ui, |ui| self.code_block(ui, reply))
    }

    // The code with its Copy button, or what failed once it expired with
    // New code beside it, as New invite takes Copy's place on the host, or
    // why no code would help. The same block on the joining screen and above
    // the people list. True when New code was pressed.
    fn code_block(&mut self, ui: &mut Ui, reply: &ReplyView) -> bool {
        let mut new_code = false;
        match reply.state {
            ReplyState::Code { second_router } => {
                controls::text(ui, messages::SEND_CODE_BACK, theme::body(), CHALK);
                ui.add_space(STEP);
                if code_row(ui, &reply.code, true, CHALK, &["Copy"]).is_some() {
                    // The code holds no secret, but it does hold this PC's
                    // home address, which history has no use for.
                    self.copy_error = win::copy_private(&reply.code)
                        .err()
                        .map(|err| messages::copy_error(&err));
                }
                ui.add_space(HALF_STEP);
                controls::prose(ui, messages::CODE_NOT_PASTED, theme::caption(), ASH);
                if second_router {
                    ui.add_space(HALF_STEP);
                    controls::prose(ui, messages::CODE_SECOND_ROUTER, theme::caption(), ASH);
                }
            }
            ReplyState::Expired { .. } => {
                let buttons = ["New code"];
                new_code = code_row(ui, messages::CODE_EXPIRED, false, CHALK, &buttons).is_some();
                if let Some(rung) = messages::reply(reply.state, reply.host_port) {
                    ui.add_space(STEP);
                    controls::prose(ui, rung, theme::body(), ASH);
                }
            }
            state => {
                if let Some(why) = messages::reply(state, reply.host_port) {
                    controls::prose(ui, why, theme::body(), CHALK);
                }
            }
        }
        if let Some(error) = &self.copy_error {
            ui.add_space(HALF_STEP);
            controls::prose(ui, error.as_str(), theme::caption(), BAD);
        }
        new_code
    }
}

// The head and the lifetime toggle share one row, a head at the left and a
// bare verb at the right, as in the title row. The toggle is a plain
// secondary in both states: the control fill at rest means a toggle that is
// on or a button that Enter presses, and here it read as on and outweighed
// Copy. True when the toggle was pressed.
fn lifetime_row(ui: &mut Ui, multi_use: bool) -> bool {
    let rect = row_rect(ui, CONTROL_HEIGHT);
    let toggle = if multi_use {
        Button::new("anyone, 24 h").color(WARN)
    } else {
        Button::new("single use, 10 min")
    };
    let toggle_width = toggle.width(ui);
    let mut pressed = false;
    controls::split_row(
        ui,
        rect,
        toggle_width,
        |ui| {
            controls::one_line(ui, "Invite", theme::section(), CHALK);
        },
        |ui| pressed = toggle.show(ui).clicked(),
    );
    pressed
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

// One sentence for the notice. The way out of it is in the title row.
fn explain(ui: &mut Ui, notice: &Notice) {
    controls::prose(ui, messages::notice(notice), theme::body(), CHALK);
}

// The last verb in the title row. Cancel while joining, since nothing has
// been joined yet; Leave once the room has ended, on the host too, since
// there is no room left to close; otherwise Leave, or Close room on the host.
fn way_out(view: &View) -> (&'static str, theme::Role) {
    let joining = view.role == Role::Client
        && view.people.is_empty()
        && matches!(view.notice, None | Some(Notice::StillTrying));
    if joining {
        ("Cancel", theme::Role::Secondary)
    } else if ends_here(view) {
        ("Leave", theme::Role::Destructive)
    } else {
        (leave_word(view.role), theme::Role::Destructive)
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

// Gone once the room has ended, as Hold to talk goes: a field that can never
// take anything again would only be a stray word on the chat's tone.
fn composer(view: &View) -> Composer {
    if ends_here(view) {
        Composer::Gone
    } else if composer_enabled(view) {
        Composer::Open
    } else {
        Composer::Off
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
// the last one, so that is shown instead.
fn paste_line(
    typed: Option<&str>,
    moved: bool,
    last: Option<&PasteState>,
    port: u16,
) -> Option<(String, Color32)> {
    if let Some(error) = typed {
        return Some((error.to_owned(), BAD));
    }
    if moved {
        return Some((String::from(messages::CODES_CANNOT_HELP), ASH));
    }
    let last = last?;
    let color = if *last == PasteState::Sent { ASH } else { BAD };
    messages::paste(last, port).map(|text| (text, color))
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
    // Stop sharing turns primary, the room's only one: showing your screen
    // is the one state that must never go unnoticed.
    role: theme::Role,
    // What a screen reader hears instead of the text.
    label: Option<String>,
    enabled: bool,
}

impl ShareVerb {
    fn words(&self) -> VerbWords {
        use theme::Role::{Primary, Secondary};
        let (text, role, label, enabled) = match self {
            ShareVerb::Share => ("Share", Secondary, None, true),
            ShareVerb::Starting => ("Starting", Secondary, None, false),
            ShareVerb::StopSharing => ("Stop sharing", Primary, None, true),
            ShareVerb::Busy(name) => ("Share", Secondary, Some(messages::one_share(name)), false),
            ShareVerb::StopControl => ("Stop control", Primary, None, true),
        };
        VerbWords {
            text,
            role,
            label,
            enabled,
        }
    }
}

impl VerbWords {
    fn button(&self) -> Button<'_> {
        let button = Button::new(self.text).role(self.role).enabled(self.enabled);
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

#[derive(Debug, PartialEq, Eq)]
enum RowPick {
    Monitor(usize),
    Region,
    Window(usize),
}

// A window Share offers, as its row shows it and a screen reader names it.
// The title stays in the panel: it never goes into a message, the log or the
// settings, so no Debug either.
struct WindowPick {
    id: room::WindowId,
    title: String,
    // The program's name and whether the window is minimized, quiet after
    // the title.
    quiet: String,
    spoken: String,
}

// The windows as they are now; none when they cannot be listed.
fn window_picks() -> Vec<WindowPick> {
    let windows = room::top_windows().unwrap_or_default();
    windows
        .into_iter()
        .map(|window| {
            let quiet = match (&window.program, window.minimized) {
                (Some(program), true) => format!("{program}, minimized"),
                (Some(program), false) => program.clone(),
                (None, true) => String::from("minimized"),
                (None, false) => String::new(),
            };
            let spoken = format!(
                "Share the window {}{}",
                messages::isolated(&window.title),
                if window.minimized { ", minimized" } else { "" }
            );
            WindowPick {
                id: window.id,
                title: window.title,
                quiet,
                spoken,
            }
        })
        .collect()
}

// Share opens a one-line list of the monitors by size and position under the
// title row, with Region last, and the windows under it, one row each, and
// one click starts the share or the region picker. At the narrowest window
// three monitors wrap onto a second line. The windows scroll on their own
// once they would take more than half the panel.
fn share_list(ui: &mut Ui, picks: &[Pick], windows: &[WindowPick]) -> Option<RowPick> {
    let mut pressed = None;
    controls::region(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.y = STEP;
            for (i, pick) in picks.iter().enumerate() {
                if Button::new(&pick.text)
                    .label(&pick.spoken)
                    .show(ui)
                    .clicked()
                {
                    pressed = Some(RowPick::Monitor(i));
                }
            }
            let region = Button::new("Region").label("Share a region of a monitor");
            if region.show(ui).clicked() {
                pressed = Some(RowPick::Region);
            }
        });
        if windows.is_empty() {
            return;
        }
        let most = (ui.available_height() / 2.0).max(2.0 * ROW_HEIGHT);
        let scroll = ScrollArea::vertical()
            .id_salt("windows")
            .max_height(most)
            .auto_shrink([false, true]);
        scroll.show(ui, |ui| {
            for (i, window) in windows.iter().enumerate() {
                if window_row(ui, window) {
                    pressed = Some(RowPick::Window(i));
                }
            }
        });
    });
    pressed
}

// One window: its title cut on one line, its program and whether it is
// minimized quiet at the right. The whole row is the button, as a known
// host's is on the start screen.
fn window_row(ui: &mut Ui, window: &WindowPick) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let row = ui.interact(
        rect,
        ui.id().with(("window", window.id.hwnd)),
        Sense::click(),
    );
    row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &window.spoken));
    let quiet = theme::caption();
    let right = controls::text_width(ui, &window.quiet, quiet.clone());
    controls::split_row(
        ui,
        rect,
        right,
        |ui| {
            controls::one_line(ui, &window.title, theme::body(), CHALK);
        },
        |ui| {
            controls::one_line(ui, &window.quiet, quiet, ASH);
        },
    );
    let focused = row.has_focus();
    if focused && controls::keyboard_focus(ui) {
        controls::ring(ui, rect);
    }
    let entered = focused && ui.input(|input| input.key_pressed(Key::Enter));
    row.on_hover_cursor(CursorIcon::PointingHand).clicked() || entered
}

impl InRoom {
    // The picker over every monitor; body takes its answer when it comes.
    fn pick_region(&mut self, ctx: &Context) -> Option<Step> {
        let list = match monitors::list() {
            Ok(list) => list,
            Err(why) => return Some(Step::Log(why)),
        };
        let ctx = ctx.clone();
        match region::pick(list, move || ctx.request_repaint()) {
            Ok(answer) => {
                self.region_pick = Some(answer);
                None
            }
            Err(err) => Some(Step::Log(format!(
                "could not open the region picker: {err}"
            ))),
        }
    }
}

// What the block under the title row asks for while this PC shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharePress {
    Resume,
    Sound(bool),
}

// A click aimed at the share list, or at the rows under it, can land in the
// block as it appears in the list's place, some 80 ms after the share is
// picked on a host and a round trip more on a client, or as a pause or a
// resume moves it, and a click meant for a game can land there as the panel
// comes back from minimized; a click aimed at Share PC sound while on, to
// turn it off, can land just after its capture failed and turned it off.
// Resume and turning the switch on wait this long after the block appears
// or moves, after the panel comes back from minimized, and after a new share
// takes the block over, as Allow does (ALLOW_AFTER). Turning the switch on
// also waits after it goes off, by itself or by hand, which moves nothing
// above it, so Resume does not. The list's room stays until the block has
// stood this long.
const BLOCK_AFTER: Duration = Duration::from_millis(500);

// When the block last appeared, moved or changed hands, which Resume waits
// after, and when what a press on the switch means last changed, which the
// switch waits after.
#[derive(Debug, Default)]
struct BlockWait {
    shown: Option<(Showing, Instant, Instant)>,
}

// How much longer the block waits since it was placed, for Resume and the
// list's room, and the switch since its last change; None once over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Waits {
    placed: Option<Duration>,
    switch: Option<Duration>,
}

// What the block shows that moves the switch or changes what a press on it
// means: the share it is for (none once Stop sharing took it), whether that
// share is paused by hand, which moves the switch, and whether the switch is
// on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Showing {
    share: Option<u32>,
    held: bool,
    on: bool,
}

impl BlockWait {
    fn out_of_sight(&mut self) {
        self.shown = None;
    }

    // Every frame in a room. Turning the switch moves nothing above it, so
    // only the switch's wait starts again then.
    fn follow(&mut self, share: &ShareView, now: Instant) -> Waits {
        let Some(running) = &share.running else {
            self.shown = None;
            return Waits::default();
        };
        let number = match share.own {
            OwnShare::Sharing { number, .. } => Some(number),
            _ => None,
        };
        let showing = Showing {
            share: number,
            held: running.held,
            on: share.own_sound.on,
        };
        let (placed, switched) = match self.shown {
            Some((shown, placed, switched)) if shown == showing => (placed, switched),
            Some((shown, placed, _))
                if (shown.share, shown.held) == (showing.share, showing.held) =>
            {
                (placed, now)
            }
            _ => (now, now),
        };
        self.shown = Some((showing, placed, switched));
        let left = |since: Instant| {
            BLOCK_AFTER
                .checked_sub(now.saturating_duration_since(since))
                .filter(|left| !left.is_zero())
        };
        Waits {
            placed: left(placed),
            switch: left(switched),
        }
    }
}

// The share list's room, kept once the list goes from under the pointer:
// what was under it would move up where a second click aimed at the list
// lands, on a host the invite's lifetime toggle.
#[derive(Debug, Default)]
struct ListRoom {
    // How tall the list was the last frame it showed.
    drawn: f32,
    // The room kept, and since when.
    held: Option<(f32, Instant)>,
    // The height the list had when it refilled in place, and since when.
    refilled: Option<(f32, Instant)>,
}

impl ListRoom {
    // A pick of a window that has closed, or the share key on an open list,
    // refills the list where it is, shorter by the windows gone, which moves
    // what is under it up under a click aimed at the list as a list that
    // goes would.
    fn refilled(&mut self, now: Instant) {
        self.refilled = Some((self.drawn, now));
    }

    // While the list shows: the height it keeps since it refilled, and how
    // long that has left.
    fn refill_room(&mut self, now: Instant) -> Option<(f32, Duration)> {
        let (room, since) = self.refilled?;
        let left = BLOCK_AFTER
            .checked_sub(now.saturating_duration_since(since))
            .filter(|left| !left.is_zero());
        if left.is_none() {
            self.refilled = None;
        }
        left.map(|left| (room, left))
    }

    // A list on screen again ends any room kept for the last one.
    fn measured(&mut self, height: f32) {
        self.drawn = height;
        self.held = None;
    }

    // Every frame the list is closed: the first after it showed keeps its
    // room, however it went, unless Esc or Share again cleared it.
    fn give_way(&mut self, now: Instant) {
        if self.drawn > 0.0 {
            self.held = Some((self.drawn, now));
        }
        self.drawn = 0.0;
        self.refilled = None;
    }

    // Esc and Share again are aimed at the keyboard and the title row, not
    // at the list, so nothing under it needs to stay.
    fn clear(&mut self) {
        self.drawn = 0.0;
        self.refilled = None;
    }

    // Every frame. The room kept and how long its least wait has left; None
    // once that is over and either no share came of the list or this PC's
    // share runs with its block still for BLOCK_AFTER. The least wait is for
    // a share refused at once, which would otherwise give the room away
    // before a double click's second press.
    fn follow(
        &mut self,
        share: &ShareView,
        placed: bool,
        now: Instant,
    ) -> Option<(f32, Option<Duration>)> {
        let (room, since) = self.held?;
        let left = BLOCK_AFTER
            .checked_sub(now.saturating_duration_since(since))
            .filter(|left| !left.is_zero());
        let over = !own_share_on(&share.own) && share.running.is_none();
        let taken =
            matches!(share.own, OwnShare::Sharing { .. }) && share.running.is_some() && placed;
        if left.is_none() && (over || taken) {
            self.held = None;
            return None;
        }
        Some((room, left))
    }
}

// The rest of the list's room under what was drawn since `top`: in panel
// tone, so it reads as one block with the share's, with nothing in it to
// press, and add_space so no widget or id changes after it.
fn keep_room(ui: &mut Ui, top: f32, room: f32) {
    let rest = room - (ui.cursor().top() - top);
    if rest > 0.0 {
        let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), rest));
        ui.painter().rect_filled(rect, 0.0, PANEL);
        ui.add_space(rest);
    }
}

// While this PC's share runs, under the title row as the monitor list is.
// Paused by hand: the paused sentence and Resume, which is secondary, since
// Stop sharing stays the room's one primary, and waits as the switch does: a
// stray click on it sends the screen again after a pause made on purpose.
// Then Share PC sound, off at the start of every share, with the line under
// it. `waits` says when each takes a press; turning the switch off never
// waits.
fn share_block(ui: &mut Ui, held: bool, own: &OwnSound, waits: Waits) -> Option<SharePress> {
    controls::region(ui, |ui| {
        let mut press = None;
        if held {
            // In the room the longer wording takes, so turning the switch
            // under it does not move the switch from under the pointer.
            let room = [messages::SHARE_PAUSED, messages::SHARE_PAUSED_WITH_SOUND]
                .map(|words| controls::text_height(ui, words, theme::body()))
                .into_iter()
                .fold(0.0, f32::max);
            ui.scope(|ui| {
                ui.set_min_height(room);
                controls::text(ui, paused_sentence(own.on), theme::body(), CHALK);
            });
            ui.add_space(STEP);
            let resume = Button::new("Resume").enabled(waits.placed.is_none());
            if controls::buttons(ui, &[resume], CONTROL_HEIGHT).is_some() {
                press = Some(SharePress::Resume);
            }
            ui.add_space(FIELD_GAP);
        }
        let (line, color) = sound_line(own);
        let id = ui.id().with("share pc sound");
        let enabled = own.on || (own.unavailable.is_none() && waits.switch.is_none());
        if controls::checkbox_row(ui, id, own.on, messages::SHARE_PC_SOUND, enabled, &line) {
            press = Some(SharePress::Sound(!own.on));
        }
        ui.add_space(HALF_STEP);
        controls::prose(ui, &line, theme::caption(), color);
        press
    })
}

fn paused_sentence(sound_on: bool) -> &'static str {
    if sound_on {
        messages::SHARE_PAUSED_WITH_SOUND
    } else {
        messages::SHARE_PAUSED
    }
}

// The line under Share PC sound: why it cannot go on, what happened to its
// capture while it waits to open again, or what it sends.
fn sound_line(own: &OwnSound) -> (String, Color32) {
    if let Some(why) = own.unavailable {
        return (why.sentence().to_owned(), ASH);
    }
    match &own.lost {
        Some(lost) if own.on => (messages::sentence(lost), WARN),
        _ => (messages::SHARE_PC_SOUND_NOTE.to_owned(), ASH),
    }
}

// 8 px of window tone between a block in panel tone and the first row, as
// under the last one, so the focus ring around a button on that row stays in
// the list's tone instead of reaching into the block's.
fn list_top(ui: &mut Ui, block: bool) {
    if block {
        ui.add_space(STEP);
    }
}

// Everyone else in the order they joined, which is the order the room gives,
// and your own row last on every panel, so it and its Mute and Deafen are in
// the same place whether you host or join.
fn in_order(people: &[Person]) -> impl Iterator<Item = &Person> {
    let others = people.iter().filter(|person| !person.is_you);
    others.chain(people.iter().filter(|person| person.is_you))
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
    // microphone or the speakers are not running. The list is in the
    // window's tone, 32 px a row, with nothing between rows but their own
    // height. True when it ended on a row, with 8 px under it.
    fn people(&mut self, ui: &mut Ui, view: &View) -> bool {
        let client_link = (view.role == Role::Client).then_some(view.strip.state);
        let gone = matches!(client_link, Some(LinkState::Lost | LinkState::Closed));
        let stale = gone || client_link == Some(LinkState::Reconnecting);
        let offered = self.control_offered;
        // Everyone else's row has a menu: their volume, first.
        let with_menu: Vec<[u8; 32]> = view
            .people
            .iter()
            .filter(|person| !person.is_you)
            .map(|person| person.key)
            .collect();
        self.menu.keep_only(&with_menu);
        // Whether the list ended on a row's own edge, with nothing under it.
        let mut row_last = false;
        for person in in_order(&view.people) {
            row_last = true;
            let same_name = view
                .people
                .iter()
                .filter(|other| other.name == person.name)
                .count()
                > 1;
            let dim = gone || person.reconnecting;
            let state = control::state_word(view, person, offered);
            let volume = (!person.is_you)
                .then(|| messages::volume_word(self.volumes.get(&person.key)))
                .flatten();
            let word = match (state, volume) {
                (Some(state), Some(volume)) => Some(format!("{state}, {volume}")),
                (state, volume) => volume.or(state.map(str::to_owned)),
            };
            let word = word.as_deref();
            if person.is_you {
                self.your_row(ui, &view.voice, person, word, dim, ends_here(view));
                let paused = paused_line(self.keys);
                let problem = voice_problem(&view.voice);
                if paused.is_some() || problem.is_some() {
                    row_last = false;
                    controls::gutter(ui, 0.0, STEP, |ui| {
                        if let Some(paused) = paused {
                            controls::prose(ui, paused, theme::caption(), WARN);
                        }
                        if let Some(problem) = problem {
                            controls::prose(ui, problem, theme::caption(), BAD);
                        }
                    });
                }
                continue;
            }
            let rect = row_rect(ui, ROW_HEIGHT);
            let items = control::menu_items(view, person, offered);
            // Under the row's own buttons, which take their clicks first.
            let row = ui.interact(rect, ui.id().with(("row", person.key)), Sense::click());
            self.menu.follow_row(ui, &row, person.key, &person.name);
            let rtt = person.rtt_ms.filter(|_| !dim).map(strip::round_trip);
            let rtt_width = rtt.as_deref().map_or(0.0, |text| {
                controls::text_width(ui, text, theme::mono_caption())
            });
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
                |ui| row_name(ui, person, Slot::of(person, None), word, same_name, dim),
                // From the right edge: the round trip keeps its column
                // whether or not the row has buttons.
                |ui| {
                    if let Some(text) = &rtt {
                        let color = if stale {
                            ASH
                        } else {
                            theme::level_color(person.rtt_level)
                        };
                        controls::one_line(ui, text, theme::mono_caption(), color);
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
            let before = ui.cursor().top();
            let was = self.volumes.get(&person.key);
            let mut percent = u32::from(was);
            let volume = Some((&mut percent, person.name.as_str()));
            let watched = view.share.watching && current.is_some_and(|c| c.key == person.key);
            // Their share's sound while this PC watches it, whether or not
            // it has any, so the slider is there to set before the sound
            // comes and nothing appears under the focus when it does.
            let sound_was = self.sound_volumes.get(&person.key);
            let mut sound_percent = u32::from(sound_was);
            let sound = watched.then_some((&mut sound_percent, SoundWord::of(&view.share)));
            let mut cap = view.share.cap;
            let asked = watched.then_some((&mut cap, view.share.cap_unheard));
            if let Some(MenuItem::EndControl) =
                self.menu.show(ui, person.key, volume, sound, asked, &items)
            {
                self.room.end_control();
            }
            if cap != view.share.cap {
                self.room.cap_watch(cap);
            }
            let percent = u8::try_from(percent).unwrap_or(MOST_VOLUME);
            if percent != was {
                self.volumes.set(person.key, percent);
                self.room.set_volume(person.key, percent);
            }
            let sound_percent = u8::try_from(sound_percent).unwrap_or(MOST_VOLUME);
            if sound_percent != sound_was {
                self.sound_volumes.set(person.key, sound_percent);
                self.room.set_sound_volume(person.key, sound_percent);
            }
            // An open menu ends with space of its own under its button.
            if ui.cursor().top() > before {
                row_last = false;
            }
        }
        // Written once the menu closes, not at each step of a drag. Each
        // keeps what went wrong with it, which the line below says.
        if !self.menu.is_open() {
            self.volumes.save();
            self.sound_volumes.save();
        }
        let problems: Vec<String> = [&self.volumes, &self.sound_volumes]
            .into_iter()
            .filter_map(Volumes::problem)
            .map(messages::volumes_error)
            .collect();
        if !problems.is_empty() {
            row_last = false;
            let error = problems.join(" ");
            controls::gutter(ui, 0.0, STEP, |ui| {
                controls::prose(ui, error.as_str(), theme::caption(), BAD);
            });
        }
        // 8 px of window tone under the last row, so the focus ring around a
        // button on it stays in the list's tone instead of reaching into the
        // chat's.
        if row_last {
            ui.add_space(STEP);
        }
        row_last
    }

    // Your row is 32 px, with Hold to talk in push-to-talk mode
    // while the hotkeys cannot do it, then Mute and Deafen. Once the room
    // has `ended` there is no voice left to send or hear: Hold to talk goes,
    // and Mute and Deafen stay in place disabled, in ash with no fill.
    fn your_row(
        &mut self,
        ui: &mut Ui,
        voice: &Voice,
        person: &Person,
        word: Option<&str>,
        dim: bool,
        ended: bool,
    ) {
        let rect = row_rect(ui, CONTROL_HEIGHT);
        let pointer_down = ui.input(|input| input.pointer.any_down());
        let hold = !ended && hold_shown(voice, self.keys, self.hold.stays(pointer_down));
        let buttons = your_buttons(voice, self.hold.held, hold);
        // Hold to talk keeps its size when it reads Talking, so Mute and
        // Deafen do not move under the mouse.
        let hold_width = [HOLD, TALKING_WORD]
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
            |ui| row_name(ui, person, Slot::of(person, Some(voice)), word, false, dim),
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
                        let mute = i == usize::from(hold);
                        // A toggle that is on keeps the control fill, so a
                        // muted microphone shows without reading the word.
                        let on = if mute { voice.muted } else { voice.deafened };
                        if button.on(on).enabled(!ended).show(ui).clicked() && !ended {
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
const TALKING_WORD: &str = "Talking";

// The words on your row's buttons, in order, and their colour: Hold to talk
// when `hold` says it shows, which reads Talking in amber while held and
// voice goes out, then Mute or Unmute, then Deafen or Undeafen. Held while
// muted, deafened or with no microphone, nothing goes out, and the button
// says so by not changing.
fn your_buttons(voice: &Voice, held: bool, hold: bool) -> Vec<(&'static str, Color32)> {
    let mut buttons = Vec::with_capacity(3);
    if hold {
        buttons.push(if held && voice.sending {
            (TALKING_WORD, AMBER)
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

// The ring slot, 16 px at the left of every row and 8 px before the name,
// kept on every row so names never move. Talking does not touch the name:
// Medium is wider than Regular, and a name that grew each time its owner
// spoke would shove what follows it back and forth.
const SLOT: f32 = 16.0;
const DOT: f32 = 4.0;
// 14 px across on the outside, 2 px thick, 3 px clear of the dot.
const RING_RADIUS: f32 = 6.0;
const RING_STROKE: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Quiet,
    Talking,
    Muted,
    Deafened,
}

impl Slot {
    // Only your own mute and deafen are known on this panel, so `voice` is
    // there for your row alone. Deafened mutes too and says more, and a
    // muted row never shows the ring.
    fn of(person: &Person, voice: Option<&Voice>) -> Slot {
        match voice {
            Some(voice) if voice.deafened => Slot::Deafened,
            Some(voice) if voice.muted => Slot::Muted,
            _ if person.talking => Slot::Talking,
            _ => Slot::Quiet,
        }
    }
}

fn paint_slot(ui: &mut Ui, slot: Slot, dim: bool) {
    let glyph = match slot {
        Slot::Muted => Some(theme::icons::regular::MICROPHONE_SLASH),
        Slot::Deafened => Some(theme::icons::regular::EAR_SLASH),
        Slot::Quiet | Slot::Talking => None,
    };
    if let Some(glyph) = glyph {
        controls::icon(ui, glyph, ASH);
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(SLOT, SLOT), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let center = rect.center().round_to_pixels(ui.pixels_per_point());
    let painter = ui.painter();
    if slot == Slot::Talking && !dim {
        painter.circle_filled(center, DOT / 2.0, TALKING);
        painter.circle_stroke(center, RING_RADIUS, Stroke::new(RING_STROKE, TALKING));
    } else {
        let color = if dim { ASH } else { QUIET_DOT };
        painter.circle_filled(center, DOT / 2.0, color);
    }
}

fn row_name(
    ui: &mut Ui,
    person: &Person,
    slot: Slot,
    word: Option<&str>,
    same_name: bool,
    dim: bool,
) {
    paint_slot(ui, slot, dim);
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
        reserve += controls::text_width(ui, &person.fingerprint, theme::mono_caption()) + gap;
    }
    if let Some(word) = word {
        reserve += controls::text_width(ui, word, theme::caption()) + gap;
    }
    let name_width = (ui.available_width() - reserve).max(0.0);
    let baseline = ui
        .scope(|ui| {
            ui.set_max_width(name_width);
            let color = if dim { ASH } else { CHALK };
            controls::one_line_on(ui, name, theme::name(), color, None).1
        })
        .inner;
    // On the name's baseline: centred in the row, the smaller text would sit
    // a pixel above it.
    if fingerprint {
        let print = theme::mono_caption();
        controls::one_line_on(ui, &person.fingerprint, print, ASH, Some(baseline));
    }
    if let Some(word) = word {
        controls::one_line_on(ui, word, theme::caption(), ASH, Some(baseline));
    }
}

// A full-width row of the Ui it is in: inside the gutter on a page, edge to
// edge for list rows.
fn row_rect(ui: &mut Ui, height: f32) -> Rect {
    ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover())
        .0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Role::{Primary, Secondary};
    use room::ReplyRefused;
    use room::view::{
        ControlView, CurrentShare, NoSound, Numbers, Party, Paused, Refusal, RouterState,
        RunningShare, Strip,
    };

    // The line under Share PC sound says what it sends, why it cannot go
    // on, or, while it is on, what happened to its capture, in warn.
    #[test]
    fn the_line_under_share_pc_sound() {
        let off = OwnSound::default();
        assert_eq!(
            sound_line(&off),
            (String::from(messages::SHARE_PC_SOUND_NOTE), ASH)
        );
        let old_host = OwnSound {
            unavailable: Some(NoSound::OldHost),
            ..OwnSound::default()
        };
        assert_eq!(
            sound_line(&old_host),
            (String::from(NoSound::OldHost.sentence()), ASH)
        );
        let lost = OwnSound {
            on: true,
            lost: Some(String::from("the sound device stopped; Booth tries again")),
            ..OwnSound::default()
        };
        assert_eq!(
            sound_line(&lost),
            (
                String::from("The sound device stopped. Booth tries again."),
                WARN
            )
        );
        assert_eq!(paused_sentence(true), messages::SHARE_PAUSED_WITH_SOUND);
        assert_eq!(paused_sentence(false), messages::SHARE_PAUSED);
    }

    // The block's waits both over, or both just begun.
    fn ready(ready: bool) -> Waits {
        let left = (!ready).then_some(BLOCK_AFTER);
        Waits {
            placed: left,
            switch: left,
        }
    }

    // While you share, a click on Share PC sound, a screen reader's click, or
    // Space on it with no other key while its ring shows, asks for the other
    // state. Enter, Space held with keys such as the panel's own
    // Ctrl+Shift+Space, Space as it repeats, Space after a click took the ring
    // away, and Space on Resume above it do nothing to it. With the host too
    // old it is disabled, says so to a screen reader, and a click asks for
    // nothing; so is it, while off, for a moment after it appears or moves.
    // A press is aimed at the switch as the frame before it drew it: one
    // during which it was disabled at any moment from then on, or turned by
    // itself, is no click when it is let go.
    #[test]
    fn share_pc_sound_turns_on_and_off() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::{Event, accesskit, pos2};
        // As the app draws a frame: the ring follows the keyboard.
        fn draw(ui: &mut Ui, held: bool, own: &OwnSound, done: bool) -> Option<SharePress> {
            controls::note_focus_source(ui.ctx());
            share_block(ui, held, own, ready(done))
        }
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let off = OwnSound::default();
        let on = OwnSound {
            on: true,
            ..OwnSound::default()
        };
        let (_, output) = frame(&ctx, Vec::new(), |ui| draw(ui, false, &off, true));
        let nodes = output.accesskit_update.expect("a tree").nodes;
        let (switch, bounds) = nodes
            .iter()
            .find(|(_, node)| node.label() == Some(messages::SHARE_PC_SOUND))
            .and_then(|(id, node)| Some((*id, node.bounds()?)))
            .expect("the switch, placed");
        let row = pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        // Each click, as in the app, after a frame that showed the switch.
        let click = |own: &OwnSound, ready: bool| {
            frame(&ctx, Vec::new(), |ui| draw(ui, false, own, ready));
            frame(&ctx, press(row), |ui| draw(ui, false, own, ready));
            frame(&ctx, release(row), |ui| draw(ui, false, own, ready))
        };
        let reader = |own: &OwnSound| {
            let request = accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: switch,
                data: None,
            };
            let events = vec![Event::AccessKitActionRequest(request)];
            frame(&ctx, events, |ui| draw(ui, false, own, true)).0
        };
        assert_eq!(click(&off, true).0, Some(SharePress::Sound(true)));
        assert_eq!(click(&off, false).0, None, "still waiting");
        // The frame before the press, the press, a frame while it is held,
        // and the release, each with what the block shows then.
        let slow_click = |shown: [(&OwnSound, bool); 4]| {
            let [seen, first, then, last] = shown;
            frame(&ctx, Vec::new(), |ui| draw(ui, false, seen.0, seen.1));
            frame(&ctx, press(row), |ui| draw(ui, false, first.0, first.1));
            frame(&ctx, Vec::new(), |ui| draw(ui, false, then.0, then.1));
            frame(&ctx, release(row), |ui| draw(ui, false, last.0, last.1)).0
        };
        assert_eq!(
            slow_click([(&off, true), (&off, true), (&off, true), (&off, true)]),
            Some(SharePress::Sound(true))
        );
        assert_eq!(
            slow_click([(&off, false), (&off, false), (&off, true), (&off, true)]),
            None,
            "pressed while waiting, let go after"
        );
        assert_eq!(
            slow_click([(&off, true), (&off, true), (&off, false), (&off, true)]),
            None,
            "waiting a moment while held"
        );
        assert_eq!(
            slow_click([(&on, true), (&on, true), (&off, true), (&off, true)]),
            None,
            "turned off by itself while held"
        );
        assert_eq!(
            slow_click([(&on, true), (&off, true), (&off, true), (&off, true)]),
            None,
            "turned off by itself as it was pressed"
        );
        assert_eq!(
            slow_click([(&off, false), (&off, true), (&off, true), (&off, true)]),
            None,
            "aimed at it while it waited"
        );
        assert_eq!(
            slow_click([(&off, true), (&off, true), (&off, true), (&off, false)]),
            None,
            "let go while it waits"
        );
        // The press and the release in one frame, after a frame that showed
        // the switch.
        let quick_click = |seen: (&OwnSound, bool), at: (&OwnSound, bool)| {
            frame(&ctx, Vec::new(), |ui| draw(ui, false, seen.0, seen.1));
            let events = [press(row), release(row)].concat();
            frame(&ctx, events, |ui| draw(ui, false, at.0, at.1)).0
        };
        assert_eq!(
            quick_click((&off, true), (&off, true)),
            Some(SharePress::Sound(true))
        );
        assert_eq!(
            quick_click((&off, false), (&off, true)),
            None,
            "in one frame, as the wait ends"
        );
        assert_eq!(
            quick_click((&on, true), (&off, true)),
            None,
            "in one frame, as it turned off by itself"
        );
        assert_eq!(
            quick_click((&off, true), (&off, false)),
            None,
            "in one frame, while it waits"
        );
        assert_eq!(
            click(&on, false).0,
            Some(SharePress::Sound(false)),
            "off never waits"
        );
        assert_eq!(
            reader(&off),
            Some(SharePress::Sound(true)),
            "a screen reader"
        );

        let key = |key, modifiers, repeat| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers,
        };
        let plain = Modifiers::NONE;
        let held = Modifiers::CTRL | Modifiers::SHIFT;
        // Each on a panel of its own, where Tab reaches the block's first
        // stop: the switch, or Resume while paused. Then one frame for each
        // list of events. egui marks a key repeating itself, when it comes
        // again before it was let go.
        let pressed = |paused: bool, frames: Vec<Vec<Event>>| {
            let ctx = Context::default();
            theme::apply(&ctx);
            frame(&ctx, vec![key(Key::Tab, plain, false)], |ui| {
                draw(ui, paused, &on, true)
            });
            frames
                .into_iter()
                .map(|events| frame(&ctx, events, |ui| draw(ui, paused, &on, true)).0)
                .collect::<Vec<_>>()
        };
        let space = || vec![key(Key::Space, plain, false)];
        assert_eq!(
            pressed(false, vec![vec![key(Key::Enter, plain, false)]]),
            [None],
            "Enter"
        );
        assert_eq!(
            pressed(false, vec![vec![key(Key::Space, held, false)]]),
            [None],
            "Ctrl+Shift+Space"
        );
        assert_eq!(
            pressed(false, vec![space(), space()]),
            [Some(SharePress::Sound(false)), None],
            "once, not as it repeats"
        );
        assert_eq!(
            pressed(false, vec![press(row), release(row), space()]),
            [None, Some(SharePress::Sound(false)), None],
            "no ring after a click"
        );
        assert_eq!(
            pressed(true, vec![space()]),
            [Some(SharePress::Resume)],
            "Space on Resume"
        );

        let old_host = OwnSound {
            unavailable: Some(NoSound::OldHost),
            ..OwnSound::default()
        };
        let (asked, output) = click(&old_host, true);
        assert_eq!(asked, None);
        assert_eq!(reader(&old_host), None, "a screen reader, disabled");
        let nodes = output.accesskit_update.expect("a tree").nodes;
        let (_, node) = nodes
            .iter()
            .find(|(_, node)| node.label() == Some(messages::SHARE_PC_SOUND))
            .expect("the switch");
        assert!(node.is_disabled());
        assert_eq!(node.description(), Some(NoSound::OldHost.sentence()));
    }

    // Paused, turning Share PC sound changes the sentence above it. At no
    // window width does that move the switch.
    #[test]
    fn share_pc_sound_stays_put_when_turned_while_paused() {
        use crate::controls::tests::frame_at;
        let top = |width: f32, on: bool| {
            let ctx = Context::default();
            theme::apply(&ctx);
            ctx.enable_accesskit();
            let own = OwnSound {
                on,
                ..OwnSound::default()
            };
            let size = vec2(width, 640.0);
            let (_, output) = frame_at(&ctx, size, Vec::new(), |ui| {
                share_block(ui, true, &own, ready(true))
            });
            let nodes = output.accesskit_update.expect("a tree").nodes;
            nodes
                .iter()
                .find(|(_, node)| node.label() == Some(messages::SHARE_PC_SOUND))
                .and_then(|(_, node)| node.bounds())
                .expect("the switch, placed")
                .y0
        };
        for width in (320..=720).step_by(10) {
            let width = width as f32;
            assert_eq!(top(width, false), top(width, true), "{width} px");
        }
    }

    // The block waits BLOCK_AFTER from when it appears, and again when a
    // pause or a resume moves it, when another share takes it over with no
    // frame between, when it comes back after it went or after the panel was
    // minimized, and while it stays a moment after Stop sharing: Resume and
    // the switch both wait then. Going on or off, also while paused, starts
    // only the switch's wait again: it moves nothing above the switch, and
    // only turning it on waits for that.
    #[test]
    fn the_block_waits_after_it_appears_or_moves() {
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let sharing = |number, held| ShareView {
            own: OwnShare::Sharing { number, fps: 60 },
            running: Some(RunningShare {
                software: false,
                paused: None,
                held,
            }),
            ..ShareView::default()
        };
        let on = |share: ShareView| ShareView {
            own_sound: OwnSound {
                on: true,
                ..OwnSound::default()
            },
            ..share
        };
        let none = ShareView::default();
        let both = |left| Waits {
            placed: Some(left),
            switch: Some(left),
        };
        let fresh = both(BLOCK_AFTER);
        let switched = Waits {
            placed: None,
            switch: Some(BLOCK_AFTER),
        };
        let over = Waits::default();
        let mut wait = BlockWait::default();
        assert_eq!(wait.follow(&none, t), over);
        assert_eq!(wait.follow(&sharing(1, false), t), fresh);
        assert_eq!(
            wait.follow(&sharing(1, false), at(300)),
            both(Duration::from_millis(200))
        );
        assert_eq!(wait.follow(&sharing(1, false), at(500)), over);
        assert_eq!(wait.follow(&sharing(1, true), at(900)), fresh, "paused");
        assert_eq!(wait.follow(&sharing(1, true), at(1_400)), over);
        assert_eq!(wait.follow(&sharing(1, false), at(1_500)), fresh, "resumed");
        assert_eq!(
            wait.follow(&sharing(2, false), at(2_100)),
            fresh,
            "another share"
        );
        assert_eq!(wait.follow(&none, at(2_700)), over);
        assert_eq!(
            wait.follow(&sharing(2, false), at(2_800)),
            fresh,
            "back after it went"
        );
        assert_eq!(wait.follow(&sharing(2, false), at(3_300)), over);
        wait.out_of_sight();
        assert_eq!(
            wait.follow(&sharing(2, false), at(3_400)),
            fresh,
            "back from minimized"
        );
        assert_eq!(
            wait.follow(&on(sharing(2, false)), at(4_000)),
            switched,
            "gone on"
        );
        assert_eq!(wait.follow(&on(sharing(2, false)), at(4_500)), over);
        assert_eq!(
            wait.follow(&sharing(2, false), at(4_600)),
            switched,
            "gone off"
        );
        assert_eq!(wait.follow(&sharing(2, true), at(5_000)), fresh, "paused");
        assert_eq!(
            wait.follow(&on(sharing(2, true)), at(5_600)),
            switched,
            "turned on while paused"
        );
        let stopping = ShareView {
            own: OwnShare::Off,
            ..on(sharing(2, true))
        };
        assert_eq!(wait.follow(&stopping, at(6_200)), fresh, "Stop sharing");
    }

    // Resume, on a share paused by hand, shows disabled and takes no press
    // while the block waits: no click, nor one pressed then and let go
    // after, nor one held while the block waited again, nor Space or a
    // screen reader's click. Once ready, a click, Space after Tab or a
    // screen reader's click still resumes, also while the switch alone
    // still waits, and the switch then takes no press.
    #[test]
    fn resume_waits_as_the_switch_does() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::{Event, PlatformOutput, accesskit, pos2};
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let off = &OwnSound::default();
        let draw = |waits: Waits| move |ui: &mut Ui| share_block(ui, true, off, waits);
        let resume = |output: PlatformOutput| {
            let nodes = output.accesskit_update.expect("a tree").nodes;
            nodes
                .into_iter()
                .find(|(_, node)| node.label() == Some("Resume"))
                .expect("Resume")
        };
        let (wait, go) = (ready(false), ready(true));
        let (_, output) = frame(&ctx, Vec::new(), draw(wait));
        let (id, node) = resume(output);
        assert!(node.is_disabled(), "disabled while it waits");
        let bounds = node.bounds().expect("Resume, placed");
        let at = pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        let (_, output) = frame(&ctx, Vec::new(), draw(go));
        assert!(!resume(output).1.is_disabled(), "enabled once ready");
        // The frame before the press, the press, a frame while it is held,
        // and the release, each with the waits then.
        let click_at = |point, shown: [Waits; 4]| {
            let [seen, first, then, last] = shown;
            frame(&ctx, Vec::new(), draw(seen));
            frame(&ctx, press(point), draw(first));
            frame(&ctx, Vec::new(), draw(then));
            frame(&ctx, release(point), draw(last)).0
        };
        let slow_click = |shown: [Waits; 4]| click_at(at, shown);
        assert_eq!(slow_click([go; 4]), Some(SharePress::Resume), "ready");
        assert_eq!(slow_click([wait; 4]), None, "while it waits");
        assert_eq!(
            slow_click([wait, wait, go, go]),
            None,
            "pressed while it waited, let go after"
        );
        assert_eq!(
            slow_click([go, go, wait, go]),
            None,
            "waiting again while held"
        );
        frame(&ctx, Vec::new(), draw(wait));
        let events = [press(at), release(at)].concat();
        assert_eq!(
            frame(&ctx, events, draw(go)).0,
            None,
            "in one frame, as the wait ends"
        );
        let switch_only = Waits {
            placed: None,
            switch: Some(BLOCK_AFTER),
        };
        assert_eq!(
            slow_click([switch_only; 4]),
            Some(SharePress::Resume),
            "only the switch waits"
        );
        let (_, output) = frame(&ctx, Vec::new(), draw(switch_only));
        let nodes = output.accesskit_update.expect("a tree").nodes;
        let (_, switch) = nodes
            .into_iter()
            .find(|(_, node)| node.label() == Some(messages::SHARE_PC_SOUND))
            .expect("the switch");
        assert!(switch.is_disabled(), "the switch, while only it waits");
        let bounds = switch.bounds().expect("the switch, placed");
        let on_switch = pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        assert_eq!(
            click_at(on_switch, [switch_only; 4]),
            None,
            "the switch, while only it waits"
        );
        assert_eq!(
            click_at(on_switch, [go; 4]),
            Some(SharePress::Sound(true)),
            "the switch, once ready"
        );
        let reader = |waits: Waits| {
            let request = accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: id,
                data: None,
            };
            let events = vec![Event::AccessKitActionRequest(request)];
            frame(&ctx, events, draw(waits)).0
        };
        assert_eq!(reader(go), Some(SharePress::Resume), "a screen reader");
        assert_eq!(reader(wait), None, "a screen reader, while it waits");
        let key = |key| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        // Each on a panel of its own, where Tab reaches Resume, the block's
        // first stop while paused.
        let space = |waits: Waits| {
            let ctx = Context::default();
            theme::apply(&ctx);
            frame(&ctx, vec![key(Key::Tab)], draw(waits));
            frame(&ctx, vec![key(Key::Space)], draw(waits)).0
        };
        assert_eq!(space(go), Some(SharePress::Resume), "Space after Tab");
        assert_eq!(space(wait), None, "Space while it waits");
    }

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
                punched: false,
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

    // Open while the room runs; kept in its shape, taking nothing, while the
    // host is lost and may come back; gone once the room has ended, when
    // Hold to talk goes too.
    #[test]
    fn the_composer_goes_with_the_room() {
        assert_eq!(composer(&in_room()), Composer::Open);
        let lost = View {
            notice: Some(Notice::LostHost),
            strip: Strip {
                state: LinkState::Lost,
                ..Strip::default()
            },
            ..in_room()
        };
        assert_eq!(composer(&lost), Composer::Off);
        let closed = View {
            notice: Some(Notice::RoomClosed),
            strip: Strip {
                state: LinkState::Closed,
                ..Strip::default()
            },
            ..in_room()
        };
        assert!(ends_here(&closed));
        assert_eq!(composer(&closed), Composer::Gone);
        let host = View {
            role: Role::Host,
            ..in_room()
        };
        assert_eq!(composer(&host), Composer::Open);
    }

    // Your own row is last on every panel, the host's included, and the rest
    // keep the order the room gave them.
    #[test]
    fn your_row_is_last() {
        let names = |people: &[Person]| -> Vec<String> {
            in_order(people).map(|person| person.name.clone()).collect()
        };
        let host = [
            person("Shadi", true),
            person("Mara", false),
            person("Jonas", false),
        ];
        assert_eq!(names(&host), ["Mara", "Jonas", "Shadi"]);
        let client = [
            person("Mara", false),
            person("Tom", true),
            person("Ines", false),
        ];
        assert_eq!(names(&client), ["Mara", "Ines", "Tom"]);
    }

    // The way out is always the title row's last verb: Cancel while joining,
    // Leave or Close room while the room runs, and Leave alone once it has
    // ended, on the host too.
    #[test]
    fn the_way_out_is_in_the_title_row() {
        use crate::theme::Role::Destructive;
        let joining = View {
            people: Vec::new(),
            reply: None,
            ..in_room()
        };
        assert_eq!(way_out(&joining), ("Cancel", Secondary));
        let still = View {
            notice: Some(Notice::StillTrying),
            ..joining.clone()
        };
        assert_eq!(way_out(&still), ("Cancel", Secondary));
        let refused = View {
            notice: Some(Notice::InviteExpired),
            ..joining
        };
        assert_eq!(way_out(&refused), ("Leave", Destructive));
        assert_eq!(way_out(&in_room()), ("Leave", Destructive));
        let closed = View {
            notice: Some(Notice::RoomClosed),
            strip: Strip {
                state: LinkState::Closed,
                ..Strip::default()
            },
            ..in_room()
        };
        assert_eq!(way_out(&closed), ("Leave", Destructive));
        assert!(share_verb(&closed, ON).is_none(), "Leave stands alone");
        let host = View {
            role: Role::Host,
            ..in_room()
        };
        assert_eq!(way_out(&host), ("Close room", Destructive));
        let failed = View {
            notice: Some(Notice::SocketFailed),
            ..host
        };
        assert_eq!(way_out(&failed), ("Leave", Destructive));
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
        assert_eq!(paste_line(None, true, None, 41000), cannot_help);
        assert_eq!(
            paste_line(None, true, Some(&PasteState::Sent), 41000),
            cannot_help
        );
        let too_soon = PasteState::Refused(ReplyRefused::TooSoon);
        assert_eq!(paste_line(None, true, Some(&too_soon), 41000), cannot_help);
        // Something that is not a code at all is still answered.
        let typed = "This is an invite, not a reply code. Paste it in Join.";
        assert_eq!(
            paste_line(Some(typed), true, None, 41000),
            Some((typed.to_owned(), BAD))
        );
    }

    #[test]
    fn paste_line_last_paste() {
        assert_eq!(paste_line(None, false, None, 41000), None);
        assert_eq!(
            paste_line(None, false, Some(&PasteState::Sent), 41000),
            Some((String::from(messages::PASTE_SENT), ASH))
        );
        assert_eq!(
            paste_line(None, false, Some(&PasteState::Joined), 41000),
            None
        );
        let too_soon = PasteState::Refused(ReplyRefused::TooSoon);
        let (_, color) = paste_line(None, false, Some(&too_soon), 41000).unwrap();
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
    // button is back, and the pause is shown under your row.
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
    fn the_ring_slot_says_talking_muted_or_deafened() {
        let quiet = person("Mara", false);
        let talking = Person {
            talking: true,
            ..person("Mara", false)
        };
        assert_eq!(Slot::of(&quiet, None), Slot::Quiet);
        assert_eq!(Slot::of(&talking, None), Slot::Talking);
        // Your own row knows its mute and deafen; a muted row never shows
        // the ring, and deafened, which mutes too, says the more.
        let you = Person {
            talking: true,
            ..person("Tom", true)
        };
        let muted = Voice {
            muted: true,
            ..Voice::default()
        };
        let deafened = Voice {
            muted: true,
            deafened: true,
            ..Voice::default()
        };
        assert_eq!(Slot::of(&you, Some(&Voice::default())), Slot::Talking);
        assert_eq!(Slot::of(&you, Some(&muted)), Slot::Muted);
        assert_eq!(Slot::of(&you, Some(&deafened)), Slot::Deafened);
    }

    // A microphone that cannot open is shown under your row as a message,
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

    fn verb(view: &View) -> Option<(&'static str, theme::Role, Option<String>, bool)> {
        let words = share_verb(view, ON)?.words();
        Some((words.text, words.role, words.label, words.enabled))
    }

    // Share, Stop sharing as the room's one primary while sharing, and
    // Share in ash that does nothing while someone else shares, whose
    // accessible name says who. Asked and not granted yet, it says so.
    #[test]
    fn the_title_row_says_what_share_does_now() {
        let idle = live(ShareView::default());
        assert_eq!(verb(&idle), Some(("Share", Secondary, None, true)));
        let sharing = live(ShareView {
            own: OwnShare::Sharing {
                number: 7,
                fps: 120,
            },
            current: Some(mara_shares(true)),
            ..ShareView::default()
        });
        assert_eq!(verb(&sharing), Some(("Stop sharing", Primary, None, true)));
        let asking = live(ShareView {
            own: OwnShare::Asking { fps: 120 },
            ..ShareView::default()
        });
        assert_eq!(verb(&asking), Some(("Starting", Secondary, None, false)));
        let busy = live(ShareView {
            current: Some(mara_shares(false)),
            ..ShareView::default()
        });
        let spoken = Some(String::from("Share. Mara is sharing. One share at a time."));
        assert_eq!(
            verb(&busy),
            Some(("Share", Secondary, spoken.clone(), false))
        );
        // Someone else's share wins over an ask the host will refuse.
        let refused = live(ShareView {
            own: OwnShare::Asking { fps: 60 },
            current: Some(mara_shares(false)),
            ..ShareView::default()
        });
        assert_eq!(verb(&refused), Some(("Share", Secondary, spoken, false)));
        // After a refusal, Share is Share again.
        let after = live(ShareView {
            own: OwnShare::Refused(Refusal::TooSoon),
            ..ShareView::default()
        });
        assert_eq!(verb(&after), Some(("Share", Secondary, None, true)));
    }

    // Stop control takes Share's place, primary like Stop sharing, and stays
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
            Some(("Stop control", Primary, None, true))
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
                held: false,
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
            (sharing.clone(), capture(Some(Paused::SecureDesktop)), true),
            // Paused by hand: resuming would show them in its first picture.
            (
                sharing,
                capture(None).map(|running| RunningShare {
                    held: true,
                    ..running
                }),
                true,
            ),
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

    fn window(hwnd: isize, title: &str) -> WindowPick {
        WindowPick {
            id: room::WindowId { hwnd, process: 1 },
            title: title.to_string(),
            quiet: String::from("notepad"),
            spoken: format!("Share the window {title}"),
        }
    }

    // Region, after the monitors, opens the picker.
    #[test]
    fn one_click_in_the_monitor_list_picks_that_monitor() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::{Context, pos2};
        let ctx = Context::default();
        theme::apply(&ctx);
        let picks = picks();
        let (second, _) = frame(&ctx, Vec::new(), |ui| {
            assert_eq!(share_list(ui, &picks, &[]), None);
            let first = Button::new(&picks[0].text).width(ui);
            let gap = ui.spacing().item_spacing.x;
            pos2(SIDE + first + gap + 10.0, SIDE + CONTROL_HEIGHT / 2.0)
        });
        frame(&ctx, press(second), |ui| share_list(ui, &picks, &[]));
        let (pressed, _) = frame(&ctx, release(second), |ui| share_list(ui, &picks, &[]));
        assert_eq!(pressed, Some(RowPick::Monitor(1)));
        let first = pos2(SIDE + 10.0, SIDE + CONTROL_HEIGHT / 2.0);
        frame(&ctx, press(first), |ui| share_list(ui, &picks, &[]));
        let (pressed, _) = frame(&ctx, release(first), |ui| share_list(ui, &picks, &[]));
        assert_eq!(pressed, Some(RowPick::Monitor(0)));
        // Alone in the row, so it is where the first monitor was.
        frame(&ctx, press(first), |ui| share_list(ui, &[], &[]));
        let (pressed, _) = frame(&ctx, release(first), |ui| share_list(ui, &[], &[]));
        assert_eq!(pressed, Some(RowPick::Region));
    }

    // A window's row, under the monitors, shares that window in one click.
    #[test]
    fn one_click_on_a_window_row_picks_that_window() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::{Context, pos2};
        let ctx = Context::default();
        theme::apply(&ctx);
        let windows = [window(1, "Notes"), window(2, "Game")];
        let (second, _) = frame(&ctx, Vec::new(), |ui| {
            assert_eq!(share_list(ui, &[], &windows), None);
            let below = SIDE + CONTROL_HEIGHT + ui.spacing().item_spacing.y;
            pos2(SIDE + 10.0, below + ROW_HEIGHT * 1.5)
        });
        frame(&ctx, press(second), |ui| share_list(ui, &[], &windows));
        let (pressed, _) = frame(&ctx, release(second), |ui| share_list(ui, &[], &windows));
        assert_eq!(pressed, Some(RowPick::Window(1)));
    }

    // The list's room stays from the frame the list goes, however it went,
    // until BLOCK_AFTER has passed and either no share of this PC's came of
    // it or that share runs with its block in place for BLOCK_AFTER: through
    // an ask, a grant before the capture opens and the block's own wait, and
    // past a share refused at once. A list on screen again ends it, and Esc
    // or Share again keeps none.
    #[test]
    fn the_list_s_room_stays_until_the_block_can_take_it() {
        // A frame with the list closed, in body's order.
        fn closed(
            list: &mut ListRoom,
            share: &ShareView,
            placed: bool,
            now: Instant,
        ) -> Option<(f32, Option<Duration>)> {
            list.give_way(now);
            list.follow(share, placed, now)
        }
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let room = 160.0;
        let kept = |left: Option<u64>| Some((room, left.map(Duration::from_millis)));
        let off = ShareView::default();
        let asking = ShareView {
            own: OwnShare::Asking { fps: 120 },
            ..ShareView::default()
        };
        let granted = ShareView {
            own: OwnShare::Sharing {
                number: 7,
                fps: 120,
            },
            ..ShareView::default()
        };
        let block = Some(RunningShare {
            software: false,
            paused: None,
            held: false,
        });
        let running = ShareView {
            running: block.clone(),
            ..granted.clone()
        };
        let mut list = ListRoom::default();
        assert_eq!(closed(&mut list, &off, true, t), None, "never measured");

        // A host's pick: Off still in the pick's own frame, granted at once,
        // the block 80 ms on and in place 500 ms after.
        list.measured(room);
        assert_eq!(closed(&mut list, &off, true, t), kept(Some(500)));
        assert_eq!(
            closed(&mut list, &granted, true, at(16)),
            kept(Some(484)),
            "granted, not running"
        );
        assert_eq!(closed(&mut list, &running, false, at(80)), kept(Some(420)));
        assert_eq!(
            closed(&mut list, &running, false, at(579)),
            kept(None),
            "the block still waits"
        );
        assert_eq!(closed(&mut list, &running, true, at(580)), None);
        assert_eq!(closed(&mut list, &running, true, at(600)), None);

        // A client's pick, asking for a round trip or much longer.
        list.measured(room);
        assert_eq!(closed(&mut list, &off, true, t), kept(Some(500)));
        assert_eq!(closed(&mut list, &asking, true, at(16)), kept(Some(484)));
        assert_eq!(
            closed(&mut list, &asking, true, at(2_000)),
            kept(None),
            "asking at 2 s"
        );

        // Refused at once, as a client asking while reconnecting is: the
        // least wait counts from the frame the list went, not from any frame
        // after.
        let refused = ShareView {
            own: OwnShare::Refused(Refusal::NotLive),
            ..ShareView::default()
        };
        list.measured(room);
        assert_eq!(closed(&mut list, &off, true, t), kept(Some(500)));
        assert_eq!(closed(&mut list, &refused, true, at(16)), kept(Some(484)));
        assert_eq!(closed(&mut list, &refused, true, at(300)), kept(Some(200)));
        assert_eq!(closed(&mut list, &refused, true, at(499)), kept(Some(1)));
        assert_eq!(closed(&mut list, &refused, true, at(500)), None);

        // Region, before the picker is up, or Share taken away by someone
        // else's share: no share of this PC's comes of it.
        let theirs = ShareView {
            current: Some(mara_shares(false)),
            ..ShareView::default()
        };
        for share in [&off, &theirs] {
            list.measured(room);
            assert_eq!(closed(&mut list, share, true, t), kept(Some(500)));
            assert_eq!(closed(&mut list, share, true, at(499)), kept(Some(1)));
            assert_eq!(closed(&mut list, share, true, at(500)), None);
        }

        // Stopped, and asked again from the list while the old share's
        // thread still closes: its block stands in place, but it is not this
        // share's.
        let closing = ShareView {
            running: block,
            ..asking.clone()
        };
        list.measured(room);
        assert_eq!(closed(&mut list, &closing, true, t), kept(Some(500)));
        assert_eq!(
            closed(&mut list, &closing, true, at(600)),
            kept(None),
            "the old share's block"
        );

        list.measured(room);
        assert_eq!(closed(&mut list, &off, true, t), kept(Some(500)));
        list.measured(room);
        assert_eq!(list.follow(&off, true, at(100)), None, "the list again");
        list.clear();
        assert_eq!(
            closed(&mut list, &off, true, at(200)),
            None,
            "Esc or Share again"
        );
    }

    // At 360 px with two monitors the invite's lifetime toggle lies under the
    // second monitor's button, and under a window row once the share's block
    // stands alone above it. After a pick, a second click at either place
    // presses nothing until the room goes, and the toggle stays where the
    // list left it, then moves once, up to where the block alone puts it: on
    // a host, granted at once with the block 80 ms on, and on a client that
    // asks for 200 ms first.
    #[test]
    fn a_second_click_where_the_list_was_presses_nothing() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::{Event, PlatformOutput, pos2};
        // What body keeps between frames, as far as the list and the block
        // go.
        #[derive(Default)]
        struct Panel {
            open: bool,
            list: ListRoom,
            wait: BlockWait,
        }
        const TOGGLE: &str = "single use, 10 min";
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let picks = picks();
        let windows = [window(1, "Notes"), window(2, "Game"), window(3, "Mail")];
        // One frame in body's order: the list while open, measured, closed
        // by a pick; given way once closed; the block; the room; then the
        // invite's first row, as the host's body has it under them.
        let draw = |panel: &mut Panel, events: Vec<Event>, share: &ShareView, now: Instant| {
            frame(&ctx, events, |ui| {
                let top = ui.cursor().top();
                let mut picked = None;
                if panel.open {
                    picked = share_list(ui, &picks, &windows);
                    panel.list.measured(ui.cursor().top() - top);
                    panel.open = picked.is_none();
                }
                if !panel.open {
                    panel.list.give_way(now);
                }
                let waits = panel.wait.follow(share, now);
                let pressed = share
                    .running
                    .as_ref()
                    .and_then(|running| share_block(ui, running.held, &share.own_sound, waits));
                if let Some((room, _)) = panel.list.follow(share, waits.placed.is_none(), now) {
                    keep_room(ui, top, room);
                }
                let toggled = controls::region(ui, |ui| lifetime_row(ui, false));
                (picked, pressed, toggled)
            })
        };
        let place = |output: &PlatformOutput, label: &str| {
            let bounds = output
                .accesskit_update
                .as_ref()
                .expect("accesskit is on")
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .and_then(|(_, node)| node.bounds())
                .unwrap_or_else(|| panic!("nothing called {label}"));
            Rect::from_min_max(
                pos2(bounds.x0 as f32, bounds.y0 as f32),
                pos2(bounds.x1 as f32, bounds.y1 as f32),
            )
        };
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let off = ShareView::default();
        let asking = ShareView {
            own: OwnShare::Asking { fps: 120 },
            ..ShareView::default()
        };
        let granted = ShareView {
            own: OwnShare::Sharing {
                number: 7,
                fps: 120,
            },
            ..ShareView::default()
        };
        let running = ShareView {
            running: Some(RunningShare {
                software: false,
                paused: None,
                held: false,
            }),
            ..granted.clone()
        };

        // Nothing above: the toggle lies under the second monitor's button,
        // where a second click on that button lands once the list goes.
        let (_, output) = draw(&mut Panel::default(), Vec::new(), &off, t);
        let bare = place(&output, TOGGLE);
        let listed = &mut Panel {
            open: true,
            ..Panel::default()
        };
        let (_, output) = draw(listed, Vec::new(), &off, t);
        let second = place(&output, &picks[1].spoken);
        let under_list = place(&output, TOGGLE).top();
        let rows: Vec<Rect> = windows
            .iter()
            .map(|window| place(&output, &window.spoken))
            .collect();
        assert!(bare.intersects(second), "{bare:?} {second:?}");
        // The block alone, in place: the toggle under a window row.
        let settled = &mut Panel::default();
        draw(settled, Vec::new(), &running, t);
        let (_, output) = draw(settled, Vec::new(), &running, at(500));
        let under_block = place(&output, TOGGLE);
        let row = rows
            .iter()
            .find(|row| row.intersects(under_block))
            .expect("the toggle under a window row");
        let aims = [
            bare.intersect(second).center(),
            under_block.intersect(*row).center(),
        ];

        // Each view from the time it starts, when the second clicks come, and
        // when the room goes.
        let host = [(0, &off), (16, &granted), (80, &running)];
        let client = [(0, &off), (16, &asking), (200, &granted), (280, &running)];
        let runs = [
            (&host[..], &[16, 80, 300, 579][..], 580),
            (&client[..], &[16, 200, 280, 500, 779][..], 780),
        ];
        for (views, clicks, gone) in runs {
            let view = |ms: u64| {
                let mut started = views.iter().rev().filter(|(from, _)| ms >= *from);
                let (_, view) = started.next().expect("each run has a view from 0 ms");
                *view
            };
            let panel = &mut Panel {
                open: true,
                ..Panel::default()
            };
            let (_, output) = draw(panel, Vec::new(), &off, t);
            let pick = place(&output, &picks[1].spoken).center();
            draw(panel, press(pick), &off, t);
            let ((picked, _, _), output) = draw(panel, release(pick), &off, t);
            assert_eq!(picked, Some(RowPick::Monitor(1)));
            assert_eq!(place(&output, TOGGLE).top(), under_list);
            for &ms in clicks {
                for aim in aims {
                    for events in [Vec::new(), press(aim), release(aim)] {
                        let (did, output) = draw(panel, events, view(ms), at(ms));
                        assert_eq!(did, (None, None, false), "{ms} ms at {aim:?}");
                        assert_eq!(place(&output, TOGGLE).top(), under_list, "{ms} ms");
                    }
                }
            }
            let (_, output) = draw(panel, Vec::new(), view(gone), at(gone));
            assert_eq!(place(&output, TOGGLE).top(), under_block.top(), "{gone} ms");
        }
    }

    // A list that refilled keeps the height it had until BLOCK_AFTER has
    // gone; a list that goes, or is closed by Esc or Share again, keeps
    // nothing of it for the next one.
    #[test]
    fn a_refilled_list_keeps_its_height_a_while() {
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let kept = |left: u64| Some((240.0, Duration::from_millis(left)));
        let mut list = ListRoom::default();
        assert_eq!(list.refill_room(t), None, "never refilled");
        list.measured(240.0);
        list.refilled(t);
        assert_eq!(list.refill_room(at(16)), kept(484));
        list.measured(240.0);
        assert_eq!(list.refill_room(at(499)), kept(1));
        assert_eq!(list.refill_room(at(500)), None);
        assert_eq!(list.refill_room(at(16)), None, "over for good");

        list.measured(240.0);
        list.refilled(t);
        list.give_way(at(16));
        list.measured(160.0);
        assert_eq!(list.refill_room(at(32)), None, "the list went");
        list.refilled(t);
        list.clear();
        assert_eq!(list.refill_room(at(32)), None, "Esc or Share again");
    }

    // A pick of a window that has closed refills the list in place, here two
    // windows shorter, which would bring the invite's lifetime toggle up
    // under the row just clicked. The list keeps its height until
    // BLOCK_AFTER has gone, so a second click there presses nothing, and the
    // toggle then moves up once.
    #[test]
    fn a_second_click_on_a_list_that_refilled_presses_nothing() {
        use crate::controls::tests::{frame, press, release};
        use eframe::egui::{Event, PlatformOutput, pos2};
        const TOGGLE: &str = "single use, 10 min";
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let picks = picks();
        let all = [window(1, "Notes"), window(2, "Game"), window(3, "Mail")];
        let left = [window(1, "Notes")];
        // One frame in body's order while the list stays open, then the
        // invite's first row.
        let draw = |list: &mut ListRoom, windows: &[WindowPick], events: Vec<Event>, now| {
            frame(&ctx, events, |ui| {
                let top = ui.cursor().top();
                let picked = share_list(ui, &picks, windows);
                if let Some((room, _)) = list.refill_room(now) {
                    keep_room(ui, top, room);
                }
                list.measured(ui.cursor().top() - top);
                let toggled = controls::region(ui, |ui| lifetime_row(ui, false));
                (picked, toggled)
            })
        };
        let place = |output: &PlatformOutput, label: &str| {
            let bounds = output
                .accesskit_update
                .as_ref()
                .expect("accesskit is on")
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .and_then(|(_, node)| node.bounds())
                .unwrap_or_else(|| panic!("nothing called {label}"));
            Rect::from_min_max(
                pos2(bounds.x0 as f32, bounds.y0 as f32),
                pos2(bounds.x1 as f32, bounds.y1 as f32),
            )
        };
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);

        // Without the height kept, the toggle would be under Mail's row.
        let (_, output) = draw(&mut ListRoom::default(), &left, Vec::new(), t);
        let unkept = place(&output, TOGGLE);
        let list = &mut ListRoom::default();
        let (_, output) = draw(list, &all, Vec::new(), t);
        let kept = place(&output, TOGGLE);
        let mail = place(&output, &all[2].spoken);
        assert!(unkept.intersects(mail), "{unkept:?} {mail:?}");
        let aim = unkept.intersect(mail).center();

        // The first click, on Mail's row: its window has closed, so body
        // refills the list.
        draw(list, &all, press(aim), t);
        let ((picked, _), _) = draw(list, &all, release(aim), t);
        assert_eq!(picked, Some(RowPick::Window(2)));
        list.refilled(t);
        for (ms, events) in [
            (16, Vec::new()),
            (100, press(aim)),
            (116, release(aim)),
            (499, Vec::new()),
        ] {
            let ((_, toggled), output) = draw(list, &left, events, at(ms));
            assert!(!toggled, "a second click at {ms} ms");
            assert_eq!(place(&output, TOGGLE), kept, "{ms} ms");
        }
        let (_, output) = draw(list, &left, Vec::new(), at(500));
        assert_eq!(place(&output, TOGGLE), unkept, "500 ms");
    }
}
