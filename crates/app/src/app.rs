use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::text_edit::TextEditState;
use eframe::egui::{
    self, CentralPanel, Context, Frame, Key, KeyboardShortcut, Modifiers, RawInput, Ui, UiBuilder,
    ViewportCommand,
};
use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
use input::{Action, Chord, Event};
use invite::Invite;
use keys::Identity;
use net::firewall::FirewallState;
use room::view::Role;
use room::{
    Candidates, Config, DamagedList, Devices, Injector, KnownDevices, KnownError, List,
    ListProblem, Lookup, LossKnob, Manual, Notify, Room, TalkMode, Timers, VideoConfig,
    VoiceConfig,
};
use voice::audio::Direction;
use zeroize::Zeroize;

use crate::backlog::Backlog;
use crate::firewall::Firewall;
use crate::hotkeys::{self, Hotkeys, ShareKey, ShareStep, Target};
use crate::remote::{Flags, Guard};
use crate::running::{self, Lock};
use crate::screens::control::PanicKey;
use crate::screens::firewall::{self as firewall_screen, Answer, Ask};
use crate::screens::room::{InRoom, Step, own_share_on};
use crate::screens::settings::{self as settings_screen, Draft, Press, Refused};
use crate::screens::start::{self, Choice, Start};
use crate::screens::stats;
use crate::settings::{DEFAULT_PORT, Settings};
use crate::sound::SettingsAudio;
use crate::strip::{self, Sweep};
use crate::theme::{self, INK};
use crate::tray::{self, Tray};
use crate::update::{self, Update};
use crate::{Args, controls, messages, monitors, win};

const STATS_KEY: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::I);

// Relative times in the panel (session age) move once a second even when the
// network has nothing new to say.
const IDLE_REPAINT: Duration = Duration::from_secs(1);
// The start-up check has no way to wake the panel, so the panel looks.
const CHECK_POLL: Duration = Duration::from_millis(15);
// The level meter is redrawn about 30 times a second while it shows; the
// level itself is kept by the capture thread and read at each redraw.
const METER_REPAINT: Duration = Duration::from_millis(33);

pub struct App {
    setup: Result<Setup, String>,
    screen: Screen,
    firewall: Firewall,
    backlog: Backlog,
    stats_open: bool,
    sweep: Sweep,
    scrolling: bool,
    notify: Notify,
    // After Not now on the firewall screen, the line the start screen shows
    // for the rest of the run. Which one depends on what the screen said.
    not_now: Option<&'static str>,
    // Known lists found damaged and put aside during this run, each said
    // once on the start screen for the rest of it.
    damaged: Vec<DamagedList>,
    // Known lists left where they were because they could not be used, one
    // start screen line each, until a later read of that list works.
    unusable: Vec<(List, String)>,
    hotkeys: Hotkeys,
    target: Target,
    // The panic key's cut and whether this PC is controlled, shared with the
    // hotkey thread and the injector's guard.
    flags: Arc<Flags>,
    // Remote control ships once the injector does, and it is not written yet.
    // Until a room here gets one, the panel shows none of it: no Control, no
    // request block, no Stop control or End control, no control words or
    // stats lines, no settings lock and no control states in the tray, and it
    // declines any request that reaches it anyway. Read once, since the
    // injector a room gets cannot come or go while Booth runs.
    control_offered: bool,
    share_key: ShareKey,
    // Shown by a hotkey on the last pass, and brought to the front on this
    // one, once Windows has it on screen.
    raise: bool,
    // Set on the viewer's thread when its strip was clicked, which brings the
    // panel forward with the stats panel open.
    viewer_strip: Arc<AtomicBool>,
    // None for a panel that can open no room, or when Windows would not
    // make one; the log says which.
    tray: Option<Tray>,
    // Set on the tray's thread when the icon was clicked.
    tray_clicked: Arc<AtomicBool>,
    // The update check and its download, if the setting is on.
    update: Update,
}

pub struct Setup {
    // Never read: holding the handle is the point (running.rs).
    _held: Option<File>,
    identity: Arc<Identity>,
    config: Config,
    // As saved, for the next room to start with; a room already open keeps
    // what it started with.
    settings: Settings,
    // --port, which wins over the setting for this run: it is how two
    // copies share one PC for testing.
    port_given: Option<u16>,
}

impl Setup {
    pub fn port(&self) -> u16 {
        self.config.port
    }

    fn apply_settings(&mut self) {
        self.config.port = self.port_given.unwrap_or(self.settings.port());
        // The room puts in a plain word if Windows would not say.
        self.config.name = match self.settings.name() {
            Some(name) => name.to_owned(),
            None => win::user_name().unwrap_or_default(),
        };
        self.config.stun_servers = self.settings.stun_servers();
        self.config.voice = VoiceConfig {
            input: self.settings.device(Direction::Input),
            output: self.settings.device(Direction::Output),
            talk: self.settings.talk_mode(),
            constant_rate: self.settings.constant_rate(),
            devices: Devices::Windows,
        };
        self.config.video_upload_kbps = self.settings.upload_mbits() * 1000;
        self.config.video.vsync = self.settings.vsync();
    }
}

enum Screen {
    // The firewall check has not answered yet.
    Checking,
    Firewall(Ask),
    Start(Start),
    // The start screen is kept as it was, typed text and all, for Save or
    // Cancel to go back to. The audio part holds the device lists and the
    // microphone for the meter open, and closes them when the screen goes.
    Settings {
        draft: Box<Draft>,
        start: Start,
        audio: Box<SettingsAudio>,
    },
    Room(Box<InRoom>),
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        setup: Result<Setup, String>,
        firewall: Firewall,
        mut backlog: Backlog,
    ) -> App {
        theme::apply(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        // Here and not in main: the window toolkit registers for keyboard
        // Raw Input when its event loop starts, and the last registration in
        // a process is the one that counts.
        let target = Target::default();
        let flags = Arc::new(Flags::default());
        let hotkeys = start_hotkeys(&setup, &target, &ctx, &flags, &mut backlog);
        let control_offered = room_injector(&hotkeys, &flags).is_some();
        let tray_clicked = Arc::new(AtomicBool::new(false));
        let tray = start_tray(&setup, &tray_clicked, &ctx, &mut backlog);
        let mut app = App {
            setup,
            screen: Screen::Checking,
            firewall,
            backlog,
            stats_open: false,
            sweep: Sweep::default(),
            scrolling: win::animations_on(),
            notify: Arc::new(move || ctx.request_repaint()),
            not_now: None,
            damaged: Vec::new(),
            unusable: Vec::new(),
            hotkeys,
            target,
            flags,
            control_offered,
            share_key: ShareKey::default(),
            raise: false,
            viewer_strip: Arc::default(),
            tray,
            tray_clicked,
            update: Update::off(),
        };
        // The start screen reads the known hosts each time it opens. The
        // devices are read only in settings and when a room opens, so a list
        // of them that cannot be used is looked for once here, to be said
        // with the hosts before anyone hosts with it.
        if let Ok(setup) = &app.setup
            && let Err(err) = Room::known_devices(&setup.config.data_dir)
        {
            app.list_problem(&err);
        }
        // At start and only here: the check never runs from inside a room,
        // and once a day at most (update/mod.rs).
        if let Ok(setup) = &app.setup {
            let dir = setup.config.data_dir.clone();
            if let Some(line) = update::tidy(&dir) {
                app.backlog.add(line);
            }
            if setup.settings.check_for_new_versions() {
                app.update = Update::start(dir, Arc::clone(&app.notify));
            }
        }
        app
    }

    // The start screen, with the known hosts as they are on disk now.
    fn start(&mut self, mut start: Start) -> Screen {
        start.known = match &self.setup {
            Ok(setup) => match Room::known_hosts(&setup.config.data_dir) {
                Ok(hosts) => {
                    self.list_read(List::Hosts);
                    hosts
                }
                Err(err) => {
                    self.list_problem(&err);
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Screen::Start(start)
    }

    fn list_problem(&mut self, err: &KnownError) {
        self.backlog.add(format!("known lists: {err}"));
        if let Some(problem) = err.problem() {
            self.note(&problem);
        }
    }

    fn note(&mut self, problem: &ListProblem) {
        match problem {
            ListProblem::Damaged(damaged) => {
                if !self.damaged.contains(damaged) {
                    self.damaged.push(damaged.clone());
                }
            }
            ListProblem::Unusable { list, why } => {
                let line = messages::unusable(why);
                if !self.unusable.contains(&(*list, line.clone())) {
                    self.unusable.retain(|(was, _)| was != list);
                    self.unusable.push((*list, line));
                }
            }
        }
    }

    fn list_read(&mut self, list: List) {
        self.unusable.retain(|(was, _)| *was != list);
    }

    // Moves on from the check once it answers, and from the firewall screen
    // once the prompt and the helper are done. A prompt left open by Not now
    // can end on any screen, and the log and the stats still get its answer.
    fn follow_firewall(&mut self, ctx: &Context) {
        let ended = self.firewall.prompt_ended(&mut self.backlog);
        match &self.screen {
            // No room can start, so an administrator prompt would be for
            // nothing; the start screen says why.
            Screen::Checking if self.setup.is_err() => {
                self.screen = self.start(Start::default());
            }
            Screen::Checking => match self.firewall.checked(&mut self.backlog) {
                Some(
                    state @ (FirewallState::Blocked(_)
                    | FirewallState::Missing(_)
                    | FirewallState::BlockingAll(_)),
                ) => {
                    self.screen = Screen::Firewall(ask(state));
                }
                Some(_) => self.screen = self.start(Start::default()),
                None => ctx.request_repaint_after(CHECK_POLL),
            },
            Screen::Firewall(_) => {
                if let Some(note) = ended {
                    self.screen = self.start(Start {
                        note,
                        ..Start::default()
                    });
                }
            }
            Screen::Start(_) | Screen::Settings { .. } | Screen::Room(_) => {}
        }
    }

    fn answer(&mut self, answer: Answer, ctx: &Context, owner: Option<isize>) {
        match answer {
            Answer::Allow => {
                if let Err(note) = self.firewall.allow(ctx, owner, &mut self.backlog) {
                    self.screen = self.start(Start {
                        note: Some(note),
                        ..Start::default()
                    });
                }
                // The buttons were drawn before the press was known; the
                // waiting line replaces them on the next frame.
                ctx.request_repaint();
            }
            Answer::NotNow => {
                self.firewall.not_now(&mut self.backlog);
                if let Screen::Firewall(ask) = &self.screen {
                    self.not_now = Some(not_now_line(ask));
                }
                self.screen = self.start(Start::default());
            }
            Answer::Continue => {
                self.backlog.add(String::from(
                    "firewall: continue, windows blocks all incoming connections",
                ));
                self.screen = self.start(Start::default());
            }
        }
    }

    fn injector(&self) -> Option<Arc<dyn Injector>> {
        room_injector(&self.hotkeys, &self.flags)
    }

    fn host(&mut self) {
        let injector = self.injector();
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        if let Some(log) = &setup.config.log {
            self.backlog.write_log(log, "host");
        }
        let room_name = start.room_name.trim().to_owned();
        let config = Config {
            address_name: setup.settings.address_name().map(str::to_owned),
            ..room_config(&setup.config, injector)
        };
        match Room::host(
            config,
            Arc::clone(&setup.identity),
            room_name,
            Arc::clone(&self.notify),
        ) {
            Ok(room) => self.enter(room),
            Err(err) => start.host_error = Some(messages::room_error(&err)),
        }
    }

    fn join(&mut self, ctx: &Context) {
        let injector = self.injector();
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        let invite = match Invite::decode(start.paste.trim()) {
            Ok(invite) => invite,
            Err(err) => {
                start.join_error = Some(messages::code_error(&err));
                return;
            }
        };
        if let Some(log) = &setup.config.log {
            self.backlog.write_log(log, "client");
        }
        match Room::join(
            room_config(&setup.config, injector),
            Arc::clone(&setup.identity),
            invite,
            Arc::clone(&self.notify),
        ) {
            Ok(room) => {
                // The invite's secret has done its job; it should not sit in
                // the field, or in the field's undo history where Ctrl+Z
                // after Leave would bring it back, for the next person at
                // this PC to copy.
                start.paste.zeroize();
                if let Some(id) = start.paste_id {
                    ctx.data_mut(|data| data.remove::<TextEditState>(id));
                }
                self.enter(room);
            }
            Err(err) => start.join_error = Some(messages::room_error(&err)),
        }
    }

    fn enter(&mut self, room: Room) {
        if let Some(line) = self.update.room_opened() {
            self.backlog.add(line);
        }
        let room_name = match &self.screen {
            Screen::Start(start) => start.room_name.clone(),
            Screen::Checking | Screen::Firewall(_) | Screen::Settings { .. } | Screen::Room(_) => {
                String::new()
            }
        };
        let log_file = self
            .setup
            .as_ref()
            .ok()
            .and_then(|setup| setup.config.log.clone());
        let firewall = stats::firewall(self.firewall.state());
        let devices = match &self.setup {
            Ok(setup) => [
                setup.settings.device(Direction::Input),
                setup.settings.device(Direction::Output),
            ],
            Err(_) => Default::default(),
        };
        // A click on the last room's viewer that came as it closed is not
        // about this one.
        self.viewer_strip.store(false, Ordering::Release);
        let clicked = Arc::clone(&self.viewer_strip);
        let notify = Arc::clone(&self.notify);
        room.on_strip_click(Arc::new(move || {
            clicked.store(true, Ordering::Release);
            notify();
        }));
        // Settings apply to the next room, and the viewer's windows read
        // this one as they draw.
        if let Ok(setup) = &self.setup {
            viewer::hide_strip_in_fullscreen(setup.settings.hide_strip());
        }
        let chord = match &self.setup {
            Ok(setup) => setup.settings.hotkeys().chord(Action::Panic),
            Err(_) => Action::Panic.default_chord(),
        };
        let panic = PanicKey {
            words: chord.to_string(),
            flags: Arc::clone(&self.flags),
        };
        let room = Arc::new(room);
        self.target.enter(Arc::clone(&room));
        self.screen = Screen::Room(Box::new(InRoom::new(
            room,
            room_name,
            log_file,
            firewall,
            devices,
            self.target.clone(),
            panic,
        )));
        self.sweep = Sweep::default();
    }

    fn leave(&mut self) {
        let Screen::Room(in_room) = &mut self.screen else {
            return;
        };
        let start = Start {
            room_name: std::mem::take(&mut in_room.typed_room_name),
            ..Start::default()
        };
        // The room writes its known list as it leaves, so the list read for
        // the start screen after it is the one it left.
        if let Screen::Room(in_room) = std::mem::replace(&mut self.screen, Screen::Checking) {
            self.target.leave();
            hotkeys::leave(in_room.room);
        }
        self.share_key = ShareKey::default();
        self.screen = self.start(start);
        self.stats_open = false;
        self.sweep = Sweep::default();
    }

    // A known host joined again with the secret it gave, and no invite.
    fn rejoin(&mut self, key: [u8; 32]) {
        let injector = self.injector();
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        let Some(known) = start.known.iter().find(|host| *host.host_key() == key) else {
            return;
        };
        if let Some(log) = &setup.config.log {
            self.backlog.write_log(log, "client");
        }
        match Room::rejoin(
            room_config(&setup.config, injector),
            Arc::clone(&setup.identity),
            known.clone(),
            Arc::clone(&self.notify),
        ) {
            Ok(room) => self.enter(room),
            Err(err) => start.known_error = Some(messages::room_error(&err)),
        }
    }

    fn save_address(&mut self, key: [u8; 32]) {
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        let Ok(manual) = Manual::parse(&start.address) else {
            start.address_error = Some(String::from(messages::MANUAL_REFUSED));
            return;
        };
        let dir = setup.config.data_dir.clone();
        let saved = Room::set_manual(&dir, &key, manual.clone());
        match saved {
            Ok(()) => {
                self.backlog.add(match &manual {
                    Some(manual) => {
                        format!("known host {}: typed in {manual}", keys::fingerprint(&key))
                    }
                    None => format!(
                        "known host {}: the address typed in is cleared",
                        keys::fingerprint(&key)
                    ),
                });
                let mut start = std::mem::take(start);
                start.opened = None;
                self.screen = self.start(start);
            }
            Err(err) => {
                self.backlog.add(format!("known hosts: {err}"));
                start.address_error = Some(messages::sentence(&err.to_string()));
                self.list_problem(&err);
            }
        }
    }

    // At once, with the secret and no second step: a new invite brings the
    // host back.
    fn forget(&mut self, key: [u8; 32]) {
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        let Some(room) = start
            .known
            .iter()
            .find(|host| *host.host_key() == key)
            .map(|host| host.room_name().to_owned())
        else {
            return;
        };
        let dir = setup.config.data_dir.clone();
        match Room::forget_host(&dir, &key) {
            Ok(()) => {
                self.backlog
                    .add(format!("known host {}: forgotten", keys::fingerprint(&key)));
                let mut start = std::mem::take(start);
                start.opened = None;
                start.forgot = Some(messages::forgot(&room));
                self.screen = self.start(start);
            }
            Err(err) => {
                self.backlog.add(format!("known hosts: {err}"));
                start.known_error = Some(messages::sentence(&err.to_string()));
                self.list_problem(&err);
            }
        }
    }

    fn open_settings(&mut self, ctx: &Context) {
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        let start = std::mem::take(start);
        let (known, problem) = match Room::known_devices(&setup.config.data_dir) {
            Ok(known) => (known, None),
            Err(err) => (KnownDevices::default(), Some(err)),
        };
        let user_name = win::user_name().unwrap_or_default();
        let mut draft = Draft::new(&setup.settings, known, user_name);
        draft.hotkeys_off = self.hotkeys.failed().map(messages::hotkeys_off);
        match problem {
            None => self.list_read(List::Devices),
            Some(err) => {
                // A damaged list is said on the start screen; anything else
                // here too, where Remove and Unblock would have been.
                if err.damaged().is_none() {
                    draft.list_error = Some(messages::list_error(&err));
                }
                self.list_problem(&err);
            }
        }
        let audio = SettingsAudio::open(ctx, &draft.input);
        self.backlog.add(String::from(
            "settings: device lists and the microphone for the meter opened",
        ));
        self.screen = Screen::Settings {
            draft: Box::new(draft),
            start,
            audio: Box::new(audio),
        };
    }

    // A refused field or a failed write keeps the screen open with the
    // reason under it, and what was typed stays for another try.
    fn save_settings(&mut self) {
        let (Ok(setup), Screen::Settings { draft, .. }) = (&mut self.setup, &mut self.screen)
        else {
            return;
        };
        let before = setup.settings.hotkeys();
        draft.stop_waiting();
        if !save_draft(setup, draft, &mut self.backlog, self.flags.controlled()) {
            return;
        }
        let after = setup.settings.hotkeys();
        if after != before && self.hotkeys.set_bindings(after) {
            self.backlog.add(hotkeys::bindings_line(&after));
        }
        self.close_settings();
    }

    fn close_settings(&mut self) {
        let Screen::Settings { start, .. } = &mut self.screen else {
            return;
        };
        let start = std::mem::take(start);
        self.hotkeys.capture(false);
        // Replacing the screen drops its audio part: the device watch stops,
        // and the meter's stream is stopped on a thread of its own, so a
        // headset still opening does not hold up the start screen.
        self.backlog.add(String::from(
            "settings: device lists and the microphone for the meter closed",
        ));
        self.screen = self.start(start);
    }

    // What the hotkey thread heard since the last pass. Push to talk, mute
    // and deafen already happened there; the rest is the panel's to do.
    fn follow_hotkeys(&mut self, ctx: &Context) {
        // Raw Input hears the keys of every window, and one typed in another
        // is not meant for the row that waits.
        if ctx.input(|input| input.viewport().focused == Some(false))
            && let Screen::Settings { draft, .. } = &mut self.screen
        {
            draft.stop_waiting();
        }
        let in_room = matches!(self.screen, Screen::Room(_));
        for (event, at) in self.hotkeys.heard() {
            if self.logging()
                && let Some(line) = hotkeys::log_line(&event, in_room)
            {
                self.backlog.add(line);
            }
            match event {
                Event::Pressed(Action::Share) => self.share_key(at),
                Event::Pressed(Action::ShowPanel) => self.show_or_hide(ctx),
                Event::Pressed(Action::StatsPanel) => self.stats_key(ctx),
                Event::Chord(chord) => self.chord(chord),
                Event::Cancelled => {
                    if let Screen::Settings { draft, .. } = &mut self.screen {
                        draft.stop_waiting();
                    }
                }
                _ => {}
            }
        }
        self.follow_capture();
        self.flush_log();
    }

    // Minimized while it is on screen, shown and brought forward otherwise.
    // Hiding means minimizing: the taskbar button stays, so the panel can
    // always be found again, even while an administrator window keeps the
    // hotkey from working, and the tray icon brings it back too. A panel
    // covered by the game counts as on screen: the second press shows it,
    // which is simpler to learn than a guess at what is covered.
    fn show_or_hide(&mut self, ctx: &Context) {
        if minimized(ctx) {
            self.show_panel(ctx);
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
    }

    fn show_panel(&mut self, ctx: &Context) {
        if minimized(ctx) {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        }
        self.raise = true;
        ctx.request_repaint();
    }

    // The stats panel opens in the panel window, which comes forward with it
    // open when it was out of sight.
    fn stats_key(&mut self, ctx: &Context) {
        let away = minimized(ctx);
        if away {
            self.show_panel(ctx);
        }
        if matches!(self.screen, Screen::Room(_)) {
            self.stats_open = away || !self.stats_open;
        }
    }

    // One press of the share key stops a share, and two within a second
    // start one on the monitor picked last in Share's list.
    // While someone else shares the room refuses the second press with its
    // one line, "Ines is sharing. One share at a time."
    fn share_key(&mut self, at: Instant) {
        let (Ok(setup), Screen::Room(in_room)) = (&self.setup, &mut self.screen) else {
            return;
        };
        let sharing = own_share_on(&in_room.room.view().share.own);
        match self.share_key.press(at, sharing) {
            Some(ShareStep::Start) => {}
            Some(ShareStep::Stop) => {
                in_room.room.stop_sharing();
                return;
            }
            None => return,
        }
        in_room.close_monitors();
        let (fps, monitor) = match monitors::list() {
            Ok(list) => monitors::remembered(&list, setup.settings.share_monitor())
                .map_or((monitors::MOST_FPS, None), |monitor| {
                    (monitors::fps(monitor.refresh_hz), Some(monitor.id.clone()))
                }),
            // The room tries the primary monitor, and says why in the chat
            // if that cannot open either.
            Err(err) => {
                self.backlog.add(format!("share key: {err}"));
                (monitors::MOST_FPS, None)
            }
        };
        in_room.room.share(fps, monitor);
    }

    fn remember_monitor(&mut self, device: &str) {
        if let Ok(setup) = &mut self.setup {
            remember_monitor(setup, device, self.flags.controlled(), &mut self.backlog);
        }
    }

    fn chord(&mut self, chord: Chord) {
        if let Screen::Settings { draft, .. } = &mut self.screen {
            draft.take_chord(chord);
        }
    }

    // The hotkeys read keys for settings exactly while a row waits for one.
    fn follow_capture(&mut self) {
        let waiting =
            matches!(&self.screen, Screen::Settings { draft, .. } if draft.waiting.is_some());
        self.hotkeys.capture(waiting);
    }

    fn logging(&self) -> bool {
        self.setup
            .as_ref()
            .is_ok_and(|setup| setup.config.log.is_some())
    }

    // What the update check or its download said since the last pass. The
    // lines wait in the backlog like any other, but outside a room nothing
    // else would write them, and someone who ran --log to see why a check
    // failed may never open one.
    fn follow_update(&mut self) {
        let lines = self.update.follow();
        if lines.is_empty() {
            return;
        }
        for line in lines {
            self.backlog.add(line);
        }
        if matches!(self.screen, Screen::Room(_)) {
            return;
        }
        if let Ok(setup) = &self.setup
            && let Some(log) = &setup.config.log
        {
            self.backlog.write_log(log, "panel");
        }
    }

    // In a room the log is open, so what the panel noted goes in now rather
    // than at the next room.
    fn flush_log(&mut self) {
        let (Ok(setup), Screen::Room(in_room)) = (&self.setup, &self.screen) else {
            return;
        };
        let Some(log) = &setup.config.log else {
            return;
        };
        if self.backlog.is_empty() {
            return;
        }
        let role = match in_room.room.view().role {
            Role::Host => "host",
            Role::Client => "client",
        };
        self.backlog.write_log(log, role);
    }
}

// A blocked panel can open no room, so there is nothing for the hotkeys to
// do, and in a second copy turned away they would answer the first copy's
// keys: its show key would hide this one where nothing shows it again.
fn start_hotkeys(
    setup: &Result<Setup, String>,
    target: &Target,
    ctx: &Context,
    flags: &Arc<Flags>,
    backlog: &mut Backlog,
) -> Hotkeys {
    let Ok(setup) = setup else {
        return Hotkeys::off();
    };
    let bindings = setup.settings.hotkeys();
    let hotkeys = Hotkeys::start(bindings, target.clone(), ctx.clone(), Arc::clone(flags));
    backlog.add(match hotkeys.failed() {
        Some(err) => format!("hotkeys: could not start: {err}"),
        None => hotkeys::bindings_line(&bindings),
    });
    hotkeys
}

// Kept for the share key the moment it is picked, from inside a room, where
// settings cannot be opened. A write that fails keeps it for this run and
// says so in the log. While this PC is controlled it is a sharing setting
// like the others and takes no change, whoever clicked.
fn remember_monitor(setup: &mut Setup, device: &str, controlled: bool, backlog: &mut Backlog) {
    if setup.settings.share_monitor() == Some(device) {
        return;
    }
    if controlled {
        backlog.add(format!(
            "settings: the share key keeps its monitor while this pc is controlled; {device} was not remembered"
        ));
        return;
    }
    setup.settings.set_share_monitor(Some(device));
    match setup.settings.save(&setup.config.data_dir) {
        Ok(()) => backlog.add(format!("settings: the share key takes {device} now")),
        Err(err) => backlog.add(format!(
            "settings: {err}; the share key takes {device} until Booth closes"
        )),
    }
}

// The injector that calls SendInput belongs to the input crate's inject
// part, which is not written yet. Until it is, no room gets one: the room
// refuses a request for the host's own share, and the panel declines one for
// a client's share without showing it.
fn system_injector() -> Option<Arc<dyn Injector>> {
    None
}

// One place for both, so the panel's switch cannot offer control on a PC
// whose rooms could only decline it: once system_injector gives one, a PC
// whose hotkeys did not start still gets none.
fn room_injector(hotkeys: &Hotkeys, flags: &Arc<Flags>) -> Option<Arc<dyn Injector>> {
    guarded(hotkeys.remote(), flags, system_injector())
}

// What the next room is given to put a controller's input on this PC,
// behind the guard. Without hotkeys there is no panic key, so nobody may
// control this PC at all.
fn guarded(
    hotkeys: Option<input::Remote>,
    flags: &Arc<Flags>,
    inner: Option<Arc<dyn Injector>>,
) -> Option<Arc<dyn Injector>> {
    let hotkeys = hotkeys?;
    let inner = inner?;
    Some(Arc::new(Guard::new(
        inner,
        Arc::clone(flags),
        Some(hotkeys),
    )))
}

fn room_config(config: &Config, injector: Option<Arc<dyn Injector>>) -> Config {
    Config {
        video: VideoConfig {
            injector,
            ..config.video.clone()
        },
        ..config.clone()
    }
}

// A blocked panel is a second copy turned away, or one that can open no
// room: a second icon would only confuse. Without a tray the panel still
// works, and the log says why.
fn start_tray(
    setup: &Result<Setup, String>,
    clicked: &Arc<AtomicBool>,
    ctx: &Context,
    backlog: &mut Backlog,
) -> Option<Tray> {
    if setup.is_err() {
        return None;
    }
    let clicked = Arc::clone(clicked);
    let ctx = ctx.clone();
    match Tray::start(move || {
        clicked.store(true, Ordering::Release);
        ctx.request_repaint();
    }) {
        Ok(tray) => {
            if !tray.on_taskbar() {
                backlog.add(String::from(
                    "tray: windows did not take the icon, which goes on when explorer starts again",
                ));
            }
            Some(tray)
        }
        Err(err) => {
            backlog.add(format!("tray: {err}; the panel goes on without its icon"));
            None
        }
    }
}

// True when all of it is on disk. While this PC is controlled a change to
// the panic key, the other hotkeys or the sharing settings is refused
// whole, and the draft stays for a Save once control has ended. The lock is
// here, in what Save does, so no click on the settings screen can get past
// it, the controller's included.
fn save_draft(
    setup: &mut Setup,
    draft: &mut Draft,
    backlog: &mut Backlog,
    controlled: bool,
) -> bool {
    let next = match draft.settings(&setup.settings) {
        Ok(next) => next,
        Err(refused) => {
            draft.refused = refused;
            return false;
        }
    };
    draft.locked = controlled;
    if controlled && next.locked_part_differs(&setup.settings) {
        backlog.add(String::from(
            "settings: not saved, this pc is controlled and the change touches the hotkeys or the sharing settings",
        ));
        return false;
    }
    draft.refused = Refused::default();
    draft.error = None;
    draft.list_error = None;
    let dir = setup.config.data_dir.clone();
    // settings.txt first: if it cannot be written nothing has changed yet,
    // and Cancel still undoes Remove and Unblock.
    if next != setup.settings {
        if let Err(err) = next.save(&dir) {
            backlog.add(format!("settings: {err}"));
            draft.error = Some(messages::settings_error(&err));
            return false;
        }
        backlog.add(saved_line(&next));
        setup.settings = next;
        setup.apply_settings();
    }
    // Each one leaves the draft once it is on disk, so a failure part way
    // keeps only what is still to do, for Save again or Cancel.
    while let Some(&key) = draft.removed.first() {
        if let Err(err) = Room::remove_device(&dir, &key) {
            backlog.add(format!("settings: {err}"));
            draft.list_error = Some(messages::list_error(&err));
            return false;
        }
        backlog.add(format!(
            "settings: known device {} removed",
            keys::fingerprint(&key)
        ));
        draft.removed.remove(0);
    }
    while let Some(&key) = draft.unblocked.first() {
        if let Err(err) = Room::unblock(&dir, &key) {
            backlog.add(format!("settings: {err}"));
            draft.list_error = Some(messages::list_error(&err));
            return false;
        }
        backlog.add(format!("settings: {} unblocked", keys::fingerprint(&key)));
        draft.unblocked.remove(0);
    }
    true
}

fn saved_line(settings: &Settings) -> String {
    let name = settings
        .name()
        .map_or_else(|| String::from("the windows user name"), String::from);
    let port = settings.port();
    let stun = settings.stun_servers().len();
    let address = settings.address_name().unwrap_or("none");
    let device = |direction| {
        let choice = settings.device(direction);
        choice.id().unwrap_or("windows default").to_owned()
    };
    let (input, output) = (device(Direction::Input), device(Direction::Output));
    let talk = match settings.talk_mode() {
        TalkMode::PushToTalk => "push to talk",
        TalkMode::OpenMic => "open mic",
    };
    let on_off = |on: bool| if on { "on" } else { "off" };
    let rate = on_off(settings.constant_rate());
    let upload = settings.upload_mbits();
    let vsync = on_off(settings.vsync());
    let hide = on_off(settings.hide_strip());
    let versions = on_off(settings.check_for_new_versions());
    format!(
        "settings: saved, name {name}, port {port}, {stun} stun servers, address name {address}, input {input}, output {output}, {talk}, constant-rate voice {rate}, video upload {upload} mbit/s, vsync in the viewer {vsync}, strip hidden in fullscreen {hide}, check for new versions {versions}"
    )
}

impl eframe::App for App {
    // Runs while the window is minimized too, where ui does not.
    fn logic(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        // Shown on the last pass, the window is on screen now and can come
        // forward.
        if std::mem::take(&mut self.raise)
            && let Some(hwnd) = window(frame)
        {
            win::bring_forward(hwnd);
        }
        if self.viewer_strip.swap(false, Ordering::AcqRel) && matches!(self.screen, Screen::Room(_))
        {
            self.stats_open = true;
            self.show_panel(ctx);
        }
        if self.tray_clicked.swap(false, Ordering::AcqRel) {
            self.show_panel(ctx);
        }
        self.follow_update();
        self.follow_hotkeys(ctx);
        // Here, since a request waiting over a game is when it matters, and
        // the panel is minimized then.
        let view = match &mut self.screen {
            Screen::Room(in_room) => {
                let view = in_room.room.view();
                in_room.decline_unshown(&view, self.control_offered);
                Some(view)
            }
            Screen::Checking | Screen::Firewall(_) | Screen::Start(_) | Screen::Settings { .. } => {
                None
            }
        };
        if let Some(tray) = &mut self.tray {
            tray.show(tray::look(view.as_ref(), self.control_offered));
        }
    }

    // While settings waits for a new key the key belongs to the hotkeys,
    // which read it themselves. The panel must not act on it as well: Space
    // pressing the focused button, Tab moving on, Esc closing something.
    fn raw_input_hook(&mut self, _ctx: &Context, raw_input: &mut RawInput) {
        if self.hotkeys.capturing() {
            raw_input.events.retain(|event| {
                !matches!(
                    event,
                    egui::Event::Key { .. }
                        | egui::Event::Text(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                        | egui::Event::Paste(_)
                )
            });
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        controls::note_focus_source(ui.ctx());
        self.follow_firewall(ui.ctx());
        let view = match &self.screen {
            Screen::Room(in_room) => Some(in_room.room.view()),
            Screen::Checking | Screen::Firewall(_) | Screen::Start(_) | Screen::Settings { .. } => {
                None
            }
        };
        if let Some(view) = &view {
            // Read again every frame, which in a room is at least once a
            // second, so turning Windows animations off takes effect while
            // the trace is running. It is one short call into Windows.
            self.scrolling = win::animations_on();
            self.sweep.update(&view.strip.trace);
            ui.request_repaint_after(IDLE_REPAINT);
            // The global stats key hears this press too while it can, and
            // one press must not toggle twice.
            if ui.input_mut(|input| input.consume_shortcut(&STATS_KEY))
                && !self.hotkeys.state().live()
            {
                self.stats_open = !self.stats_open;
            }
            if self.stats_open
                && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
            {
                self.stats_open = false;
            }
            if let Some(problem) = &view.list_problem {
                self.note(problem);
            }
            // Here and not in logic: apart from the key, only the buttons
            // start a share, and they can be clicked only while ui runs.
            self.share_key.follow(own_share_on(&view.share.own));
        }

        let mut step = None;
        let mut choice = None;
        let mut answer = None;
        let mut press = None;
        let mut strip_clicked = false;
        CentralPanel::default()
            .frame(Frame::new().fill(INK))
            .show(ui, |ui| {
                // The screen is laid out before the strip, not in a bottom
                // panel, so Tab reaches the strip last as it reads.
                let whole = ui.max_rect();
                let (body, bottom) = whole.split_top_bottom_at_y(whole.bottom() - strip::HEIGHT);
                ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
                    ui.set_clip_rect(body);
                    match (&mut self.screen, &view) {
                        (Screen::Room(in_room), Some(view)) => {
                            let keys = self.hotkeys.state();
                            let offered = self.control_offered;
                            step = in_room.show(ui, view, self.stats_open, keys, offered);
                        }
                        (Screen::Start(start), _) => {
                            let blocked = self.setup.as_ref().err().map(String::as_str);
                            // Gone once a prompt left open behind Not now let Booth in.
                            let allowed =
                                matches!(self.firewall.state(), Some(FirewallState::Allowed(_)));
                            let hint = self.not_now.filter(|_| !allowed);
                            let mut lines: Vec<String> =
                                hint.map(String::from).into_iter().collect();
                            lines.extend(self.damaged.iter().map(messages::damaged));
                            lines.extend(self.unusable.iter().map(|(_, line)| line.clone()));
                            let update = self.update.shown();
                            choice = start::show(ui, start, blocked, &lines, update.as_ref());
                        }
                        (Screen::Firewall(ask), _) => {
                            answer = firewall_screen::show(ui, ask, self.firewall.waiting());
                        }
                        (Screen::Settings { draft, audio, .. }, _) => {
                            // The line follows the switch. The lock itself,
                            // in save_draft and the hotkeys, follows the
                            // flag alone, which only the injector's guard
                            // sets, so without an injector neither is on.
                            draft.locked = self.control_offered && self.flags.controlled();
                            if draft.locked {
                                draft.stop_waiting();
                            }
                            // A minimized window shows no meter, so its
                            // microphone is closed until the window is back.
                            let showing =
                                ui.input(|input| input.viewport().minimized != Some(true));
                            audio.follow(&draft.input, showing);
                            press = settings_screen::show(ui, draft, audio);
                            if audio.level().is_some() {
                                ui.request_repaint_after(METER_REPAINT);
                            }
                        }
                        // Usually a frame or two at most, so no words.
                        (Screen::Checking, _) => {
                            controls::title_row(ui, "Booth", &[]);
                        }
                        (Screen::Room(_), None) => {}
                    }
                });
                ui.scope_builder(UiBuilder::new().max_rect(bottom), |ui| {
                    controls::hairline(ui, bottom.top() + 1.0);
                    let shown = view.as_ref().map(|view| &view.strip);
                    strip_clicked = strip::show(ui, shown, &self.sweep, self.scrolling).clicked();
                });
            });
        if strip_clicked {
            self.stats_open = !self.stats_open;
        }

        if let Some(answer) = answer {
            self.answer(answer, ui.ctx(), window(frame));
        }
        match choice {
            Some(Choice::Host) => self.host(),
            Some(Choice::Join) => self.join(ui.ctx()),
            Some(Choice::Rejoin(key)) => self.rejoin(key),
            Some(Choice::SaveAddress(key)) => {
                self.save_address(key);
                ui.request_repaint();
            }
            Some(Choice::Forget(key)) => {
                self.forget(key);
                ui.request_repaint();
            }
            Some(Choice::Download) => {
                if let Ok(setup) = &self.setup {
                    let dir = setup.config.data_dir.clone();
                    self.update.download(dir, Arc::clone(&self.notify));
                }
                ui.request_repaint();
            }
            Some(Choice::Settings) => {
                self.open_settings(ui.ctx());
                // This frame was laid out for the start screen, and nothing
                // else would ask for the next one.
                ui.request_repaint();
            }
            None => {}
        }
        if let Some(press) = press {
            match press {
                Press::Save => self.save_settings(),
                Press::Cancel => self.close_settings(),
            }
            ui.request_repaint();
        }
        match step {
            Some(Step::Leave) => self.leave(),
            Some(Step::NewInvite { multi_use }) => {
                if let Screen::Room(in_room) = &self.screen {
                    in_room.room.new_invite(multi_use);
                }
            }
            Some(Step::NewCode) => {
                if let Screen::Room(in_room) = &self.screen {
                    in_room.room.new_code();
                }
            }
            Some(Step::Remember(device)) => self.remember_monitor(&device),
            None => {}
        }
        // Change or Save pressed on this frame starts or ends a wait.
        self.follow_capture();
    }
}

// Windows asks by itself, with its own prompt, only when no rule exists and
// the account can say yes. A standard user's answer makes a Block rule
// whatever they pick, so for them that comes first.
fn not_now_line(ask: &Ask) -> &'static str {
    if ask.standard_user {
        messages::FIREWALL_NOT_NOW_STANDARD_USER
    } else if ask.blocked {
        messages::FIREWALL_NOT_NOW_BLOCKED
    } else {
        messages::FIREWALL_MAY_ASK
    }
}

fn ask(state: &FirewallState) -> Ask {
    test_wording(Ask {
        blocked: matches!(state, FirewallState::Blocked(_)),
        standard_user: win::rights() == win::Rights::Standard,
        blocking_all: matches!(state, FirewallState::BlockingAll(_)),
    })
}

// Debug builds only, to see the screen in wordings this PC would not get:
// BOOTH_TEST_FIREWALL=standard-user,blocked,blocking-all.
#[cfg(debug_assertions)]
fn test_wording(mut ask: Ask) -> Ask {
    let asked = std::env::var("BOOTH_TEST_FIREWALL").unwrap_or_default();
    for word in asked.split(',') {
        match word.trim() {
            "standard-user" => ask.standard_user = true,
            "blocked" => ask.blocked = true,
            "blocking-all" => ask.blocking_all = true,
            _ => {}
        }
    }
    ask
}

#[cfg(not(debug_assertions))]
fn test_wording(ask: Ask) -> Ask {
    ask
}

fn minimized(ctx: &Context) -> bool {
    ctx.input(|input| input.viewport().minimized == Some(true))
}

// The panel's own window, which the administrator prompt belongs to.
fn window(frame: &eframe::Frame) -> Option<isize> {
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

pub enum NotStarted {
    // A panel that can open no room, with the sentence that says why.
    Blocked(String),
    // No panel: the copy already open on this profile showed its own.
    BroughtForward,
}

// A settings line that could not be used goes to the log and costs nothing
// else.
pub fn load(args: &Args, backlog: &mut Backlog) -> Result<Setup, NotStarted> {
    let shown = |err: keys::KeyError| NotStarted::Blocked(messages::sentence(&err.to_string()));
    let data_dir = keys::data_dir(args.profile.as_deref()).map_err(shown)?;
    // A panel already open would run on without this run's options, and
    // nothing would say so.
    let lock = if args.for_this_run() {
        running::hold(&data_dir)
    } else {
        running::take(&data_dir, running::WAIT)
    };
    let held = match lock {
        Lock::Held(file) => Some(file),
        Lock::BroughtForward => return Err(NotStarted::BroughtForward),
        Lock::Open => return Err(NotStarted::Blocked(String::from(messages::ALREADY_OPEN))),
        Lock::Unheld(err) => {
            backlog.add(format!(
                "could not hold {}: {err}; a second copy on this data folder would not be stopped",
                data_dir.join(running::LOCK_FILE).display()
            ));
            None
        }
    };
    let identity = Identity::load_or_create(&data_dir).map_err(shown)?;
    let (settings, problems) = Settings::load(&data_dir);
    for problem in problems {
        backlog.add(problem);
    }
    let mut setup = Setup {
        _held: held,
        identity: Arc::new(identity),
        settings,
        port_given: args.port,
        config: Config {
            log: args.log.then(|| data_dir.join("booth.log")),
            data_dir,
            // The next three come from the settings, just below.
            port: DEFAULT_PORT,
            name: String::new(),
            stun_servers: Vec::new(),
            candidates: Candidates::Discover,
            punch_loopback: false,
            timers: Timers::default(),
            // A host takes it from the settings as they are when the room
            // opens; a client from the invite.
            address_name: None,
            lookup: Lookup::default(),
            watch_addresses: true,
            // From the settings, just below.
            voice: VoiceConfig::default(),
            video_upload_kbps: room::DEFAULT_VIDEO_UPLOAD_KBPS,
            video: VideoConfig {
                loss: args.video_loss.map(|percent| LossKnob {
                    percent,
                    seed: share::fresh_seed(),
                }),
                ..VideoConfig::default()
            },
        },
    };
    if let Some(knob) = setup.config.video.loss {
        backlog.add(format!(
            "--video-loss: dropping {}% of the video packets that arrive for a share this pc watches, seed {}",
            knob.percent, knob.seed
        ));
    }
    setup.apply_settings();
    Ok(setup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn setup_in(dir: &Path) -> Setup {
        Setup {
            _held: None,
            identity: Arc::new(Identity::generate()),
            settings: Settings::default(),
            port_given: None,
            config: Config {
                log: None,
                data_dir: dir.to_path_buf(),
                port: DEFAULT_PORT,
                name: String::new(),
                stun_servers: Vec::new(),
                candidates: Candidates::Discover,
                punch_loopback: false,
                timers: Timers::default(),
                address_name: None,
                lookup: Lookup::default(),
                watch_addresses: false,
                voice: VoiceConfig::default(),
                video_upload_kbps: room::DEFAULT_VIDEO_UPLOAD_KBPS,
                video: VideoConfig::default(),
            },
        }
    }

    // Remove and Unblock take effect on Save, and Cancel undoes them. A
    // settings.txt that cannot be written leaves them undone too.
    #[test]
    fn remove_and_unblock_wait_for_settings_txt() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("booth-app-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut setup = setup_in(&dir);
        let mut backlog = Backlog::default();
        let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
        draft.port = String::from("41010");
        draft.removed.push([7; 32]);
        draft.unblocked.push([8; 32]);

        // Windows will not open a folder as a file, which stands in for
        // settings.txt held open by another program.
        fs::create_dir(dir.join("settings.txt")).unwrap();
        assert!(!save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert!(draft.error.is_some());
        assert_eq!(draft.list_error, None);
        assert_eq!(draft.removed, [[7; 32]]);
        assert_eq!(draft.unblocked, [[8; 32]]);
        assert_eq!(setup.settings.port(), DEFAULT_PORT);
        let texts = backlog.texts();
        assert!(
            texts
                .iter()
                .all(|text| !text.ends_with("removed") && !text.ends_with("unblocked")),
            "{texts:?}"
        );

        // The settings go in, then the known devices cannot be read. What
        // is left stays in the draft, and the line is not the settings'.
        fs::remove_dir(dir.join("settings.txt")).unwrap();
        fs::create_dir(dir.join("devices.bin")).unwrap();
        assert!(!save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert_eq!(setup.settings.port(), 41010);
        assert_eq!(draft.error, None);
        let line = draft.list_error.clone().expect("a line with the devices");
        assert!(line.ends_with("then start Booth again."), "{line}");
        assert_eq!(draft.removed, [[7; 32]]);

        // Save again once it can be read, and the rest goes through.
        fs::remove_dir(dir.join("devices.bin")).unwrap();
        assert!(save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert!(draft.removed.is_empty() && draft.unblocked.is_empty());
        assert_eq!(draft.list_error, None);
        let texts = backlog.texts();
        assert!(
            texts.iter().any(|text| text.ends_with(" removed")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.ends_with(" unblocked")),
            "{texts:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // While this PC is controlled, Save refuses a change to the panic key,
    // the other hotkeys or the sharing settings, writes nothing, and keeps
    // the draft for a Save once control has ended.
    #[test]
    fn save_while_controlled() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("booth-app-locked-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut setup = setup_in(&dir);
        let mut backlog = Backlog::default();
        let f9: Chord = "F9".parse().unwrap();
        let mut refused = |what: &str, change: &dyn Fn(&mut Draft)| {
            let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
            change(&mut draft);
            let before = draft.hotkeys;
            assert!(
                !save_draft(&mut setup, &mut draft, &mut backlog, true),
                "{what}"
            );
            assert!(draft.locked, "{what}");
            assert_eq!(draft.hotkeys, before, "{what}: the draft stays");
            assert_eq!(setup.settings, Settings::default(), "{what}");
            assert!(!dir.join("settings.txt").exists(), "{what}");
        };
        refused("panic key", &|draft| draft.hotkeys.set(Action::Panic, f9));
        refused("mute", &|draft| draft.hotkeys.set(Action::Mute, f9));
        refused("upload", &|draft| draft.upload_mbits = 40);
        refused("vsync", &|draft| draft.vsync = true);
        refused("hide strip", &|draft| draft.hide_strip = true);
        // Nothing else is locked.
        let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
        draft.name = String::from("Tom");
        assert!(save_draft(&mut setup, &mut draft, &mut backlog, true));
        assert_eq!(setup.settings.name(), Some("Tom"));
        // Once control has ended the same change goes through.
        let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
        draft.vsync = true;
        assert!(save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert!(setup.settings.vsync());
        let _ = fs::remove_dir_all(&dir);
    }

    // The monitor picked in Share's list is a sharing setting too.
    #[test]
    fn while_controlled_the_share_key_keeps_its_monitor() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("booth-app-monitor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut setup = setup_in(&dir);
        let mut backlog = Backlog::default();
        remember_monitor(&mut setup, r"\\.\DISPLAY2", true, &mut backlog);
        assert_eq!(setup.settings.share_monitor(), None);
        assert!(!dir.join("settings.txt").exists());
        remember_monitor(&mut setup, r"\\.\DISPLAY2", false, &mut backlog);
        assert_eq!(setup.settings.share_monitor(), Some(r"\\.\DISPLAY2"));
        let (saved, _) = Settings::load(&dir);
        assert_eq!(saved.share_monitor(), Some(r"\\.\DISPLAY2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panel_without_hotkeys_gives_the_room_no_injector() {
        struct Never;
        impl Injector for Never {
            fn started(&self, _: &room::Started) {}
            fn inject(&self, _: &room::Injection<'_>) -> room::Injected {
                room::Injected::default()
            }
            fn cut_off(&self) {}
            fn ended(&self, _: room::ControlEnd) {}
        }
        let hotkeys = Hotkeys::off();
        assert!(hotkeys.remote().is_none());
        let flags = Arc::new(Flags::default());
        let inner: Arc<dyn Injector> = Arc::new(Never);
        let given = guarded(hotkeys.remote(), &flags, Some(inner));
        assert!(given.is_none());
        let config = room_config(&setup_in(&std::env::temp_dir()).config, given);
        assert!(config.video.injector.is_none());
    }

    // Settings apply to the next room, which takes the upload and vsync from
    // its Config.
    #[test]
    fn the_sharing_settings_reach_the_next_room() {
        let dir = std::env::temp_dir().join(format!("booth-app-sharing-{}", std::process::id()));
        let mut setup = setup_in(&dir);
        setup.apply_settings();
        assert_eq!(
            setup.config.video_upload_kbps,
            room::DEFAULT_VIDEO_UPLOAD_KBPS
        );
        assert!(!setup.config.video.vsync);
        setup.settings.set_upload_mbits(40);
        setup.settings.set_vsync(true);
        setup.apply_settings();
        assert_eq!(setup.config.video_upload_kbps, 40_000);
        assert!(setup.config.video.vsync);
        assert_eq!(setup.config.video.loss, None);
    }

    #[test]
    fn a_blocked_panel_starts_no_hotkeys() {
        let mut backlog = Backlog::default();
        let setup = Err(String::from(messages::ALREADY_OPEN));
        let hotkeys = start_hotkeys(
            &setup,
            &Target::default(),
            &Context::default(),
            &Arc::default(),
            &mut backlog,
        );
        assert!(!hotkeys.state().running);
        assert_eq!(hotkeys.failed(), None);
        assert!(backlog.is_empty(), "{:?}", backlog.texts());
    }

    #[test]
    fn line_after_not_now() {
        let ask = |blocked, standard_user| Ask {
            blocked,
            standard_user,
            blocking_all: false,
        };
        assert_eq!(not_now_line(&ask(false, false)), messages::FIREWALL_MAY_ASK);
        assert_eq!(
            not_now_line(&ask(true, false)),
            messages::FIREWALL_NOT_NOW_BLOCKED
        );
        assert_eq!(
            not_now_line(&ask(false, true)),
            messages::FIREWALL_NOT_NOW_STANDARD_USER
        );
        assert_eq!(
            not_now_line(&ask(true, true)),
            messages::FIREWALL_NOT_NOW_STANDARD_USER
        );
    }
}
