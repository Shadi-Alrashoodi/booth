use std::cell::RefCell;
use std::fs::File;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui::text_edit::TextEditState;
use eframe::egui::{
    CentralPanel, Context, Frame, Key, KeyboardShortcut, Modifiers, Ui, UiBuilder, ViewportCommand,
};
use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
use input::{Action, Chord, Event, PanelKeys};
use invite::{CodeError, Invite};
use keys::Identity;
use net::firewall::FirewallState;
use room::view::{Role, ShareState};
use room::{
    Candidates, Config, DamagedList, Devices, Injector, KnownDevices, KnownError, List,
    ListProblem, Lookup, LossKnob, Manual, Notify, Room, TalkMode, Timers, VideoConfig,
    VoiceConfig,
};
use voice::audio::Direction;
use zeroize::{Zeroize, Zeroizing};

use crate::adapter::{self, CardWatch};
use crate::backlog::Backlog;
use crate::firewall::Firewall;
use crate::hotkeys::{self, Hotkeys, ShareKey, ShareStep, Target};
use crate::remote::{Flags, Guard};
use crate::running::{self, Lock};
use crate::screens::control::PanicKey;
use crate::screens::firewall::{self as firewall_screen, Answer, Ask};
use crate::screens::room::{InRoom, Step, own_share_on, share_offered};
use crate::screens::settings::{self as settings_screen, Draft, Press, Refused};
use crate::screens::start::{self, Choice, LinkPage, LinkWait, Start, Waiting};
use crate::screens::stats;
use crate::settings::{DEFAULT_PORT, GaveWay, Settings};
use crate::sound::SettingsAudio;
use crate::strip::{self, Sweep};
use crate::theme::{self, WINDOW};
use crate::tray::{self, Tray};
use crate::update::{self, Update};
use crate::volumes::{Kind, Volumes};
use crate::{Args, controls, messages, monitors, win, winver};

const STATS_KEY: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::I);

// Relative times in the panel (session age) move once a second even when the
// network has nothing new to say.
const IDLE_REPAINT: Duration = Duration::from_secs(1);
// The start-up check has no way to wake the panel, so the panel looks.
const CHECK_POLL: Duration = Duration::from_millis(15);
// The level meter is redrawn about 30 times a second while it shows; the
// level itself is kept by the capture thread and read at each redraw.
pub(crate) const METER_REPAINT: Duration = Duration::from_millis(33);

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
    // for the rest of the run. Which one depends on what the screen showed.
    not_now: Option<&'static str>,
    // Known lists found damaged and put aside during this run, each shown on
    // the start screen for the rest of the run.
    damaged: Vec<DamagedList>,
    // Known lists left where they were because they could not be used, one
    // start screen line each, until a later read of that list works.
    unusable: Vec<(List, String)>,
    hotkeys: Hotkeys,
    target: Target,
    // The panic key's cut and whether this PC is controlled, shared with the
    // hotkey thread and the injector's guard.
    flags: Arc<Flags>,
    // Remote control stays hidden and refused until the injector exists. Read
    // once, since a room's injector cannot come or go while Booth runs.
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
    // A link for the panel, from the tray's thread or from this start's own
    // command line, and whether one came since the panel last looked.
    inbox: Arc<Mutex<Option<Zeroizing<Vec<u8>>>>>,
    link_arrived: Arc<AtomicBool>,
    // Set from just before a room is asked for until it is left, so the
    // tray turns away a link that comes then instead of keeping it.
    in_room: Arc<AtomicBool>,
    link: Option<Waiting>,
    // The update check and its download, if the setting is on.
    update: Update,
    // The window's scale the icons were last loaded for, and whether it had
    // the focus, as last seen, so the icons and the title's colour change
    // only when these do.
    scale: Option<f32>,
    focused: Option<bool>,
    // Shared with the message hook (main.rs), which runs between passes on
    // this thread.
    panel_keys: Rc<RefCell<PanelKeys>>,
    // Only while the panel draws on the CPU. On a card nothing is looked at.
    card_watch: Option<CardWatch>,
}

pub struct Setup {
    // Never read: holding the handle is the point (running.rs).
    _held: Option<File>,
    identity: Arc<Identity>,
    config: Config,
    // As saved, for the next room to start with. A room already open takes
    // only what a Save over it applies.
    settings: Settings,
    // --port, which wins over the setting for this run: it is how two
    // copies share one PC for testing.
    port_given: Option<u16>,
    // Keys that went back to their defaults as settings.txt was read, and a
    // new action moved off its default.
    gave_way: Vec<GaveWay>,
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
            sensitivity_db: self.settings.sensitivity_db(),
            echo_cancellation: self.settings.echo_cancellation() && winver::cancels_echo(),
            process_loopback: winver::shares_pc_sound(),
            noise_suppression: self.settings.noise_suppression(),
            automatic_volume: self.settings.automatic_volume(),
            cues: self.settings.sound_cues(),
            #[cfg(feature = "memstats")]
            devices: crate::memstats::devices().unwrap_or(Devices::Windows),
            #[cfg(not(feature = "memstats"))]
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
        panel_keys: Rc<RefCell<PanelKeys>>,
        link: Option<Zeroizing<String>>,
    ) -> App {
        theme::apply(&cc.egui_ctx);
        if let Ok(handle) = cc.window_handle()
            && let RawWindowHandle::Win32(handle) = handle.as_raw()
        {
            win::caption_colours(handle.hwnd.get());
            win::set_window_icons(handle.hwnd.get());
        }
        let ctx = cc.egui_ctx.clone();
        let drawn_on = cc
            .wgpu_render_state
            .as_ref()
            .map(|state| state.adapter.get_info());
        if let Some(info) = &drawn_on {
            backlog.add(adapter::start_line(info));
        }
        let card_watch = drawn_on
            .filter(adapter::on_cpu)
            .map(|_| CardWatch::start(Instant::now()));
        // Here and not in main: the window toolkit registers for keyboard
        // Raw Input when its event loop starts, and the last registration in
        // a process is the one that counts.
        let target = Target::default();
        let flags = Arc::new(Flags::default());
        let hotkeys = start_hotkeys(&setup, &target, &ctx, &flags, &mut backlog);
        let control_offered = room_injector(&hotkeys, &flags).is_some();
        let tray_clicked = Arc::new(AtomicBool::new(false));
        let link_arrived = Arc::new(AtomicBool::new(link.is_some()));
        let inbox = Arc::new(Mutex::new(
            link.map(|text| Zeroizing::new(text.as_bytes().to_vec())),
        ));
        let in_room = Arc::new(AtomicBool::new(false));
        let tray = start_tray(
            &setup,
            &tray_clicked,
            &ctx,
            link_receiver(
                Arc::clone(&inbox),
                Arc::clone(&link_arrived),
                Arc::clone(&in_room),
                Arc::new({
                    let ctx = ctx.clone();
                    move || ctx.request_repaint()
                }),
            ),
            &mut backlog,
        );
        let mut app = App {
            setup,
            screen: Screen::Checking,
            firewall,
            backlog,
            stats_open: false,
            sweep: Sweep::default(),
            scrolling: adapter::scrolls(win::animations_on(), card_watch.is_some()),
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
            inbox,
            link_arrived,
            in_room,
            link: None,
            update: Update::off(),
            scale: None,
            focused: None,
            panel_keys,
            card_watch,
        };
        // The start screen reads the known hosts each time it opens. The
        // devices are read only in settings and when a room opens, so a list
        // of them that cannot be used is looked for once here, to be shown
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
            // The Store updates its own copy.
            if setup.settings.check_for_new_versions() && !cfg!(feature = "store") {
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
            // The Store package brings its own rule, so its copy only reads
            // the firewall, for the log and the stats.
            Screen::Checking => match self.firewall.checked(&mut self.backlog) {
                Some(
                    state @ (FirewallState::Blocked(_)
                    | FirewallState::Missing(_)
                    | FirewallState::BlockingAll(_)),
                ) if !cfg!(feature = "store") => {
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

    fn host(&mut self, ctx: &Context) {
        self.clear_link();
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
        let opened = opening(&self.in_room, || {
            Room::host(
                config,
                Arc::clone(&setup.identity),
                room_name,
                Arc::clone(&self.notify),
            )
        });
        match opened {
            Ok(room) => {
                forget_paste(start, ctx);
                self.enter(room);
            }
            Err(err) => start.host_error = Some(messages::room_error(&err)),
        }
    }

    fn join(&mut self, ctx: &Context) {
        let Screen::Start(start) = &mut self.screen else {
            return;
        };
        let invite = match Invite::decode(start.paste.trim()) {
            Ok(invite) => invite,
            Err(err) => {
                start.join_error = Some(messages::code_error(&err));
                return;
            }
        };
        self.join_invite(invite, ctx);
    }

    fn join_invite(&mut self, invite: Invite, ctx: &Context) {
        let injector = self.injector();
        let (Ok(setup), Screen::Start(start)) = (&self.setup, &mut self.screen) else {
            return;
        };
        if let Some(log) = &setup.config.log {
            self.backlog.write_log(log, "client");
        }
        let opened = opening(&self.in_room, || {
            Room::join(
                room_config(&setup.config, injector),
                Arc::clone(&setup.identity),
                invite,
                Arc::clone(&self.notify),
            )
        });
        match opened {
            Ok(room) => {
                forget_paste(start, ctx);
                self.enter(room);
            }
            Err(err) => {
                let line = messages::room_error(&err);
                match &mut self.link {
                    Some(Waiting::Shown { error, dead, .. }) => {
                        // No later press gets past these, so the page has
                        // nothing left to keep a newer link out for.
                        *dead = matches!(
                            err,
                            room::RoomError::OwnInvite | room::RoomError::BadHostKey
                        );
                        *error = Some(line);
                    }
                    _ => start.join_error = Some(line),
                }
            }
        }
    }

    fn enter(&mut self, room: Room) {
        self.in_room.store(true, Ordering::Release);
        self.clear_link();
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
        // Set again when Settings is saved in the room, and the viewer's
        // windows read it as they draw.
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
        // Before anyone is heard, so each is heard at their own volume
        // from the first word, and their share's sound from its first frame.
        let load = |kind| match &self.setup {
            Ok(setup) => Volumes::load(&setup.config.data_dir, kind),
            Err(_) => Volumes::default(),
        };
        let volumes = load(Kind::Voice);
        for (key, percent) in volumes.each() {
            room.set_volume(key, percent);
        }
        let sound_volumes = load(Kind::PcSound);
        for (key, percent) in sound_volumes.each() {
            room.set_sound_volume(key, percent);
        }
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
            [volumes, sound_volumes],
            self.setup
                .as_ref()
                .is_ok_and(|setup| setup.settings.invite_links()),
        )));
        self.sweep = Sweep::default();
    }

    fn leave(&mut self, ctx: &Context) {
        let Screen::Room(in_room) = &mut self.screen else {
            return;
        };
        let start = Start {
            room_name: std::mem::take(&mut in_room.typed_room_name),
            ..Start::default()
        };
        in_room.close_settings();
        in_room.forget_paste(ctx);
        // The room writes its known list as it leaves, so the list read for
        // the start screen after it is the one it left.
        if let Screen::Room(in_room) = std::mem::replace(&mut self.screen, Screen::Checking) {
            self.target.leave();
            hotkeys::leave(in_room.room);
        }
        self.hotkeys.capture(false);
        self.share_key = ShareKey::default();
        self.in_room.store(false, Ordering::Release);
        self.screen = self.start(start);
        self.stats_open = false;
        self.sweep = Sweep::default();
    }

    // A known host joined again with the secret it gave, and no invite.
    fn rejoin(&mut self, key: [u8; 32], ctx: &Context) {
        self.clear_link();
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
        let opened = opening(&self.in_room, || {
            Room::rejoin(
                room_config(&setup.config, injector),
                Arc::clone(&setup.identity),
                known.clone(),
                Arc::clone(&self.notify),
            )
        });
        match opened {
            Ok(room) => {
                forget_paste(start, ctx);
                self.enter(room);
            }
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
                start.known_error = None;
                self.screen = self.start(start);
            }
            Err(err) => {
                self.backlog.add(format!("known hosts: {err}"));
                start.address_error = Some(messages::list_failed(&err));
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
                start.known_error = None;
                self.screen = self.start(start);
            }
            Err(err) => {
                self.backlog.add(format!("known hosts: {err}"));
                start.known_error = Some(messages::list_failed(&err));
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
                // A damaged list is shown on the start screen; anything else
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

    fn open_room_settings(&mut self, ctx: &Context) {
        let (Ok(setup), Screen::Room(in_room)) = (&self.setup, &mut self.screen) else {
            return;
        };
        self.stats_open = false;
        if in_room.settings_draft_mut().is_some() {
            return;
        }
        let mut draft = Draft::new(
            &setup.settings,
            KnownDevices::default(),
            win::user_name().unwrap_or_default(),
        );
        draft.hotkeys_off = self.hotkeys.failed().map(messages::hotkeys_off);
        let audio = SettingsAudio::in_room(ctx, &draft.input);
        in_room.open_settings(draft, audio);
        self.backlog
            .add(String::from("settings: opened over the room"));
    }

    fn save_room_settings(&mut self) {
        let (Ok(setup), Screen::Room(in_room)) = (&mut self.setup, &mut self.screen) else {
            return;
        };
        let Some(draft) = in_room.settings_draft_mut() else {
            return;
        };
        let before = setup.settings.clone();
        draft.stop_waiting();
        let saved = save_draft(setup, draft, &mut self.backlog, self.flags.controlled());
        if !saved {
            draft.show_reason = true;
        }
        if !setup.settings.invite_links() {
            self.link = None;
            self.inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
        }
        let after = setup.settings.hotkeys();
        if after != before.hotkeys() && self.hotkeys.set_bindings(after) {
            self.backlog.add(hotkeys::bindings_line(&after));
        }
        if setup.settings != before {
            let now = &setup.settings;
            if voice_settings_differ(now, &before) {
                in_room.room.set_voice(setup.config.voice.clone());
            }
            if now.sound_cues() != before.sound_cues() {
                in_room.room.set_cues(now.sound_cues());
            }
            viewer::hide_strip_in_fullscreen(now.hide_strip());
            in_room.set_invite_links(now.invite_links());
            in_room.set_panic_words(now.hotkeys().chord(Action::Panic).to_string());
            in_room.set_devices([now.device(Direction::Input), now.device(Direction::Output)]);
        }
        self.hotkeys.capture(false);
        if saved {
            self.close_room_settings();
        }
    }

    fn close_room_settings(&mut self) {
        if let Screen::Room(in_room) = &mut self.screen {
            in_room.close_settings();
            self.hotkeys.capture(false);
            self.backlog
                .add(String::from("settings: closed over the room"));
        }
    }

    fn settings_draft_mut(&mut self) -> Option<&mut Draft> {
        match &mut self.screen {
            Screen::Settings { draft, .. } => Some(draft),
            Screen::Room(in_room) => in_room.settings_draft_mut(),
            _ => None,
        }
    }

    // A refused field or a failed write keeps the screen open with the
    // reason under it, and what was typed stays for another try. Settings
    // that went in before a list failed count now: a later Save finds them
    // already there, and Cancel does not look.
    fn save_settings(&mut self) {
        let (Ok(setup), Screen::Settings { draft, start, .. }) =
            (&mut self.setup, &mut self.screen)
        else {
            return;
        };
        let before = setup.settings.clone();
        draft.stop_waiting();
        let saved = save_draft(setup, draft, &mut self.backlog, self.flags.controlled());
        if setup.settings != before {
            start.settings_changed();
        }
        if !setup.settings.invite_links() {
            self.link = None;
            self.inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
        }
        let after = setup.settings.hotkeys();
        if after != before.hotkeys() && self.hotkeys.set_bindings(after) {
            self.backlog.add(hotkeys::bindings_line(&after));
        }
        if !saved {
            draft.show_reason = true;
            return;
        }
        self.close_settings();
    }

    fn close_settings(&mut self) {
        let Screen::Settings { start, .. } = &mut self.screen else {
            return;
        };
        let mut start = std::mem::take(start);
        // A row left open brings its Save and Forget back under the bar's two
        // buttons, and the second click of a double click, where they act at
        // once.
        start.opened_at = Some(Instant::now());
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
            && let Some(draft) = self.settings_draft_mut()
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
                Event::Pressed(Action::Share) => self.share_key(at, ctx),
                Event::Pressed(Action::ShowPanel) => self.show_or_hide(ctx),
                Event::Pressed(Action::StatsPanel) => self.stats_key(ctx),
                Event::Chord(chord) => self.chord(chord),
                Event::Cancelled => {
                    if let Some(draft) = self.settings_draft_mut() {
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
    fn share_key(&mut self, at: Instant, ctx: &Context) {
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
        if in_room.picking() {
            return;
        }
        // After a region or window share the key opens the list instead, in
        // later rooms and runs too, so a press never shows more than the
        // person last chose.
        if setup.settings.share_key_opens_list() {
            self.open_list_by_key(monitors::list(), ctx);
            return;
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

    // The panel comes forward only for a list that stays. With Share not on
    // offer the next frame would close it, so the room says why in the chat
    // instead, which starts nothing even if the other share has just ended.
    fn open_list_by_key(&mut self, list: Result<Vec<room::Monitor>, String>, ctx: &Context) {
        let Screen::Room(in_room) = &mut self.screen else {
            return;
        };
        if !share_offered(&in_room.room.view(), self.control_offered) {
            in_room.room.refuse_share();
            return;
        }
        match in_room.open_share_list(list) {
            Ok(()) => self.show_panel(ctx),
            Err(err) => {
                in_room.list_failed();
                self.backlog.add(format!("share key: {err}"));
            }
        }
    }

    fn remember_monitor(&mut self, device: &str) {
        if let Ok(setup) = &mut self.setup {
            remember_monitor(setup, device, self.flags.controlled(), &mut self.backlog);
        }
    }

    fn remember_list_next(&mut self) {
        if let Ok(setup) = &mut self.setup {
            remember_list_next(setup, &mut self.backlog);
        }
    }

    fn take_step(&mut self, step: Option<Step>, ctx: &Context) {
        match step {
            Some(Step::Leave) => self.leave(ctx),
            Some(Step::OpenSettings) => self.open_room_settings(ctx),
            Some(Step::SaveSettings) => self.save_room_settings(),
            Some(Step::CloseSettings) => self.close_room_settings(),
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
            Some(Step::ListNext) => self.remember_list_next(),
            Some(Step::Log(line)) => self.backlog.add(line),
            None => {}
        }
    }

    fn chord(&mut self, chord: Chord) {
        if let Some(draft) = self.settings_draft_mut() {
            draft.take_chord(chord);
        }
    }

    // The hotkeys read keys for settings exactly while a row waits for one.
    fn follow_capture(&mut self) {
        if let Screen::Room(in_room) = &mut self.screen
            && !in_room.settings_on_top(self.stats_open)
        {
            in_room.stop_settings_wait();
        }
        let waiting = self
            .settings_draft_mut()
            .is_some_and(|draft| draft.waiting.is_some());
        self.hotkeys.capture(waiting);
    }

    // The message hook decides between passes what egui gets of each key, so
    // after each pass it learns what the panel is doing.
    fn follow_panel_keys(&self, ctx: &Context) {
        let Ok(mut keys) = self.panel_keys.try_borrow_mut() else {
            return;
        };
        keys.set_bindings(self.hotkeys.bindings());
        keys.set_capture(self.hotkeys.capturing());
        keys.set_typing(ctx.text_edit_focused());
        keys.set_focused(ctx.input(|input| input.viewport().focused != Some(false)));
    }

    fn logging(&self) -> bool {
        self.setup
            .as_ref()
            .is_ok_and(|setup| setup.config.log.is_some())
    }

    fn clear_link(&mut self) {
        self.link = None;
        // Before the take: a link the tray stores after it keeps the flag on.
        self.link_arrived.store(false, Ordering::Release);
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn follow_link(&mut self, hwnd: Option<isize>) {
        if !self.link_arrived.swap(false, Ordering::AcqRel) {
            return;
        }
        let bytes = self
            .inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(bytes) = bytes else {
            return;
        };
        let refused = match &self.setup {
            Err(_) => Some("the panel cannot open a room"),
            Ok(setup) if !setup.settings.invite_links() => Some("invite links are off"),
            Ok(_) if matches!(self.screen, Screen::Room(_)) => Some("a room is open"),
            Ok(_) => None,
        };
        if let Some(reason) = refused {
            if self.logging() {
                self.backlog.add(format!("invite link: dropped, {reason}"));
            }
            return;
        }
        // A newer link must not swap the invite under a click on Join, but a
        // page that can no longer join anything gives way to it.
        if let Some(Waiting::Shown {
            invite,
            wait,
            dead: false,
            ..
        }) = &mut self.link
            && !invite.is_expired(start::unix_now())
        {
            wait.restart();
        } else {
            let decoded = std::str::from_utf8(&bytes).ok().map(Invite::decode);
            let waiting = match decoded {
                Some(Ok(invite)) if !invite.is_expired(start::unix_now()) => Waiting::Shown {
                    invite,
                    wait: LinkWait::default(),
                    error: None,
                    dead: false,
                },
                Some(Ok(_)) => Waiting::Refused(messages::code_error(&CodeError::Expired)),
                None | Some(Err(CodeError::Empty | CodeError::NotACode)) => {
                    Waiting::Refused(String::from(messages::LINK_NOT_BOOTH))
                }
                Some(Err(err)) => Waiting::Refused(messages::code_error(&err)),
            };
            if self.logging() {
                self.backlog.add(match &waiting {
                    Waiting::Shown { invite, .. } => format!(
                        "invite link: on show, host {}",
                        keys::fingerprint(&invite.host_key)
                    ),
                    Waiting::Refused(reason) => format!("invite link: refused, {reason}"),
                });
            }
            self.link = Some(waiting);
        }
        if let Some(hwnd) = hwnd {
            win::take_foreground(hwnd);
        }
    }

    // The title goes to ash while another window has the focus, as the
    // caption buttons dim, and the icons are loaded again at the sizes a
    // new DPI asks for. The scale App::new loaded them at is not known yet
    // there, so the first one seen only counts as the starting point.
    fn follow_window(&mut self, ctx: &Context, frame: &eframe::Frame) {
        let (scale, focused) = ctx.input(|input| {
            let viewport = input.viewport();
            (viewport.native_pixels_per_point, viewport.focused)
        });
        let Some(hwnd) = window(frame) else {
            return;
        };
        let before = std::mem::replace(&mut self.scale, scale);
        if before.is_some() && scale.is_some() && before != scale {
            win::set_window_icons(hwnd);
        }
        if let Some(focused) = focused
            && self.focused != Some(focused)
        {
            self.focused = Some(focused);
            win::caption_text(hwnd, focused);
        }
    }

    // What the update check or its download reported since the last pass. The
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

// What in_room was as the last room on this thread was asked for, so the
// tests see each way into a room come through opening.
#[cfg(test)]
thread_local! {
    static ASKED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn opening(
    in_room: &AtomicBool,
    open: impl FnOnce() -> Result<Room, room::RoomError>,
) -> Result<Room, room::RoomError> {
    in_room.store(true, Ordering::Release);
    #[cfg(test)]
    ASKED.with(|asked| asked.set(Some(in_room.load(Ordering::Acquire))));
    let opened = open();
    if opened.is_err() {
        in_room.store(false, Ordering::Release);
    }
    opened
}

// Once a room opens, by Join or not, an invite in the paste field should not
// sit there, or in the field's undo history where Ctrl+Z after Leave would
// bring it back, for the next person at this PC to copy.
fn forget_paste(start: &mut Start, ctx: &Context) {
    start.paste.zeroize();
    if let Some(id) = start.paste_id {
        ctx.data_mut(|data| data.remove::<TextEditState>(id));
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

// Save builds on the saved settings, so a monitor picked while Settings
// is open survives Save. A write that fails keeps it for this run and
// says so in the log. While this PC is controlled it is a sharing setting
// like the others and takes no change, whoever clicked, so if a window or a
// region was picked last the key still opens the list.
fn remember_monitor(setup: &mut Setup, device: &str, controlled: bool, backlog: &mut Backlog) {
    if setup.settings.share_monitor() == Some(device) && !setup.settings.share_key_opens_list() {
        return;
    }
    if controlled {
        backlog.add(format!(
            "settings: the share key keeps its monitor while this pc is controlled; {device} was not remembered"
        ));
        return;
    }
    setup.settings.set_share_monitor(Some(device));
    setup.settings.set_share_key_opens_list(false);
    match setup.settings.save(&setup.config.data_dir) {
        Ok(()) => backlog.add(format!("settings: the share key takes {device} now")),
        Err(err) => backlog.add(format!(
            "settings: {err}; the share key takes {device} until Booth closes"
        )),
    }
}

// Kept the same way when a window or a region is picked. Being controlled does
// not stop this one: it can only make the key show less. Each pick writes it
// again, since a file left without it gives a whole monitor after a restart.
fn remember_list_next(setup: &mut Setup, backlog: &mut Backlog) {
    let new = !setup.settings.share_key_opens_list();
    setup.settings.set_share_key_opens_list(true);
    match setup.settings.save(&setup.config.data_dir) {
        Ok(()) if new => backlog.add(String::from("settings: the share key opens the list now")),
        Ok(()) => {}
        Err(err) => backlog.add(format!(
            "settings: {err}; the share key opens the list until Booth closes"
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
    link: impl Fn(&[u8]) -> isize + Send + 'static,
    backlog: &mut Backlog,
) -> Option<Tray> {
    if setup.is_err() {
        return None;
    }
    let clicked = Arc::clone(clicked);
    let ctx = ctx.clone();
    match Tray::start(
        move || {
            clicked.store(true, Ordering::Release);
            ctx.request_repaint();
        },
        link,
    ) {
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

pub(crate) fn link_receiver(
    inbox: Arc<Mutex<Option<Zeroizing<Vec<u8>>>>>,
    arrived: Arc<AtomicBool>,
    in_room: Arc<AtomicBool>,
    notify: Notify,
) -> impl Fn(&[u8]) -> isize + Send + 'static {
    move |bytes| {
        if in_room.load(Ordering::Acquire) {
            return 2;
        }
        let bytes = Zeroizing::new(bytes.to_vec());
        let mut slot = inbox.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(bytes);
        }
        drop(slot);
        arrived.store(true, Ordering::Release);
        notify();
        1
    }
}

// What a running room has to hear about after a Save over it.
fn voice_settings_differ(now: &Settings, before: &Settings) -> bool {
    now.device(Direction::Input) != before.device(Direction::Input)
        || now.device(Direction::Output) != before.device(Direction::Output)
        || now.talk_mode() != before.talk_mode()
        || now.constant_rate() != before.constant_rate()
        || now.sensitivity_db() != before.sensitivity_db()
        || now.echo_cancellation() != before.echo_cancellation()
        || now.noise_suppression() != before.noise_suppression()
        || now.automatic_volume() != before.automatic_volume()
}

// True when all of it is on disk. While this PC is controlled a change to
// the panic key, the other hotkeys, the sharing settings, the voice devices
// or how you talk is refused whole, and the draft stays for a Save once
// control has ended. The lock is here, in what Save does, so no click on the
// settings screen can get past it, the controller's included.
fn save_draft(
    setup: &mut Setup,
    draft: &mut Draft,
    backlog: &mut Backlog,
    controlled: bool,
) -> bool {
    let mut next = match draft.settings(&setup.settings) {
        Ok(next) => next,
        Err(refused) => {
            draft.refused = refused;
            return false;
        }
    };
    draft.locked = controlled;
    if controlled && next.locked_part_differs(&setup.settings) {
        backlog.add(String::from(
            "settings: not saved, this pc is controlled and the change touches the hotkeys, the sharing settings, the voice devices or how you talk",
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
    let links = on_off(settings.invite_links());
    let link_wait = settings.invite_link_wait_secs();
    format!(
        "settings: saved, name {name}, port {port}, {stun} stun servers, address name {address}, input {input}, output {output}, {talk}, constant-rate voice {rate}, video upload {upload} mbit/s, vsync in the viewer {vsync}, strip hidden in fullscreen {hide}, check for new versions {versions}, invite links {links}, join waits after a link {link_wait} s"
    )
}

impl eframe::App for App {
    // Runs while the window is minimized too, where ui does not.
    fn logic(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        #[cfg(feature = "memstats")]
        {
            crate::memstats::egui(ctx);
            self.backlog.gauge();
            ::stats::gauge!("app.damaged", self.damaged);
            ::stats::gauge!("app.unusable", self.unusable);
        }
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
        self.follow_link(window(frame));
        if (!matches!(self.screen, Screen::Start(_)) || !start::link_focused(ctx))
            && let Some(waiting) = &mut self.link
        {
            waiting.restart();
        }
        self.follow_window(ctx, frame);
        self.follow_update();
        self.follow_hotkeys(ctx);
        // Here, since a request waiting over a game is when it matters, and
        // the panel is minimized then. So can it be when the region picker
        // answers, which starts a share with no frame to see it.
        let mut step = None;
        let view = match &mut self.screen {
            Screen::Room(in_room) => {
                step = in_room.follow_region_pick();
                let view = in_room.room.view();
                self.share_key.follow(own_share_on(&view.share.own));
                in_room.decline_unshown(&view, self.control_offered);
                if minimized(ctx) {
                    in_room.out_of_sight();
                }
                Some(view)
            }
            // A minimized window shows no meter, so its microphone is closed
            // until the window is back and ui runs again.
            Screen::Settings { draft, audio, .. } => {
                if minimized(ctx) {
                    let echo = draft.echo_cancellation && winver::cancels_echo();
                    audio.follow(&draft.input, &draft.output, echo, false);
                }
                None
            }
            Screen::Checking | Screen::Firewall(_) | Screen::Start(_) => None,
        };
        if let Some(tray) = &mut self.tray {
            tray.show(tray::look(view.as_ref(), self.control_offered));
        }
        self.take_step(step, ctx);
        self.follow_panel_keys(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        controls::note_focus_source(ui.ctx());
        self.follow_firewall(ui.ctx());
        if let Some(watch) = &mut self.card_watch {
            let now = Instant::now();
            if let Some(line) = watch.look(now) {
                self.backlog.add(line);
            }
            if let Some(left) = watch.next_look(now) {
                ui.request_repaint_after(left);
            }
        }
        let on_cpu = self
            .card_watch
            .as_ref()
            .filter(|watch| watch.card_came())
            .map(|_| messages::PANEL_ON_CPU);
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
            self.scrolling = adapter::scrolls(win::animations_on(), self.card_watch.is_some());
            self.sweep.update(&view.strip.trace);
            ui.request_repaint_after(IDLE_REPAINT);
            // The global stats key hears this press too while it can, and
            // one press must not toggle twice.
            if ui.input_mut(|input| input.consume_shortcut(&STATS_KEY))
                && !self.hotkeys.state().live()
            {
                self.stats_open = !self.stats_open;
            }
            if let Some(problem) = &view.list_problem {
                self.note(problem);
            }
        }

        let mut step = None;
        let mut choice = None;
        let mut answer = None;
        let mut press = None;
        let mut strip_clicked = false;
        CentralPanel::default()
            .frame(Frame::new().fill(WINDOW))
            .show(ui, |ui| {
                // The screen is laid out before the strip, not in a bottom
                // panel, so Tab reaches the strip last as it reads. Settings
                // outside a room has none, so its bar meets the window edge.
                let whole = ui.max_rect();
                let strip_height = if matches!(self.screen, Screen::Settings { .. }) {
                    0.0
                } else {
                    strip::HEIGHT
                };
                let (body, bottom) = whole.split_top_bottom_at_y(whole.bottom() - strip_height);
                ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
                    ui.set_clip_rect(body);
                    match (&mut self.screen, &view) {
                        (Screen::Room(in_room), Some(view)) => {
                            let keys = self.hotkeys.state();
                            let offered = self.control_offered;
                            step = in_room.show(ui, view, self.stats_open, keys, offered, on_cpu);
                        }
                        (Screen::Start(start), _) => {
                            let blocked = self.setup.as_ref().err().map(String::as_str);
                            // Gone once a prompt left open behind Not now let Booth in.
                            let allowed =
                                matches!(self.firewall.state(), Some(FirewallState::Allowed(_)));
                            let hint = self.not_now.filter(|_| !allowed);
                            let lines =
                                start_lines(hint, &self.setup, &self.damaged, &self.unusable);
                            let update = self.update.shown();
                            let page = self.link.as_mut().map(|waiting| LinkPage {
                                waiting,
                                delay: Duration::from_secs(
                                    self.setup.as_ref().map_or(1, |setup| {
                                        u64::from(setup.settings.invite_link_wait_secs())
                                    }),
                                ),
                                open_mic: self.setup.as_ref().is_ok_and(|setup| {
                                    setup.settings.talk_mode() == TalkMode::OpenMic
                                }),
                            });
                            choice = start::show(
                                ui,
                                start,
                                blocked,
                                on_cpu,
                                &lines,
                                update.as_ref(),
                                page,
                            );
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
                            // ui runs only for a window on screen: minimized,
                            // only logic does.
                            let echo = draft.echo_cancellation && winver::cancels_echo();
                            audio.follow(&draft.input, &draft.output, echo, true);
                            press = settings_screen::show(ui, draft, audio);
                            if audio.level().is_some() {
                                ui.request_repaint_after(METER_REPAINT);
                            }
                        }
                        // Usually a frame or two at most, so no words.
                        (Screen::Checking, _) => {
                            controls::title_row(ui, controls::Lead::Empty, &[]);
                        }
                        (Screen::Room(_), None) => {}
                    }
                });
                // No line over the strip: it is window tone, and the chat
                // above it panel tone.
                if strip_height > 0.0 {
                    ui.scope_builder(UiBuilder::new().max_rect(bottom), |ui| {
                        ui.set_clip_rect(bottom);
                        let shown = view.as_ref().map(|view| &view.strip);
                        let cap_set = view.as_ref().is_some_and(|view| view.share.cap_set());
                        let watched = view
                            .as_ref()
                            .map_or(ShareState::Live, |view| view.share.watched());
                        strip_clicked =
                            strip::show(ui, shown, cap_set, watched, &self.sweep, self.scrolling)
                                .clicked();
                    });
                }
            });
        if self.stats_open && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.stats_open = false;
        }
        if strip_clicked {
            self.stats_open = !self.stats_open;
        }

        if let Some(answer) = answer {
            self.answer(answer, ui.ctx(), window(frame));
        }
        match choice {
            Some(Choice::Host) => self.host(ui.ctx()),
            Some(Choice::Join) => self.join(ui.ctx()),
            Some(Choice::JoinLink) => {
                if let Some(Waiting::Shown { invite, .. }) = &self.link {
                    self.join_invite(invite.clone(), ui.ctx());
                }
            }
            Some(Choice::DismissLink) => self.clear_link(),
            Some(Choice::Rejoin(key)) => self.rejoin(key, ui.ctx()),
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
        // Before a Leave on this frame drops the room screen, and its lines
        // with it.
        if let Screen::Room(in_room) = &mut self.screen {
            for line in in_room.take_log() {
                self.backlog.add(line);
            }
        }
        self.take_step(step, ui.ctx());
        // Change or Save pressed on this frame starts or ends a wait.
        self.follow_capture();
        self.follow_panel_keys(ui.ctx());
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
    // A panel that can open no room, with the message that says why.
    Blocked(String),
    // No panel and nothing to say: the copy already open took the start or
    // the link, links are off, or the run did not come from the window in use.
    Quiet,
    // No panel: a box says why the link was not opened.
    Said(&'static str),
}

// What the start screen says for this run. A key that gave way is said until
// Settings has moved it or the other key from where reading the file left them:
// after that the line's "back on" or "is on", or its clash, is not true.
fn start_lines(
    hint: Option<&'static str>,
    setup: &Result<Setup, String>,
    damaged: &[DamagedList],
    unusable: &[(List, String)],
) -> Vec<String> {
    let mut lines: Vec<String> = hint.map(String::from).into_iter().collect();
    if let Ok(setup) = setup {
        if setup.settings.unread() {
            lines.push(String::from(messages::SETTINGS_UNREAD));
        }
        let hotkeys = setup.settings.hotkeys();
        lines.extend(
            setup
                .gave_way
                .iter()
                .filter(|gave| {
                    hotkeys.chord(gave.action) == gave.now
                        && hotkeys.chord(gave.with) == gave.with_now
                })
                .map(messages::key_gave_way),
        );
    }
    lines.extend(damaged.iter().map(messages::damaged));
    lines.extend(unusable.iter().map(|(_, line)| line.clone()));
    lines
}

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
        Lock::BroughtForward => return Err(NotStarted::Quiet),
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
    #[cfg(feature = "memstats")]
    crate::memstats::start(&data_dir);
    Ok(setup_from(args, data_dir, held, identity, backlog))
}

pub fn load_link(link: &str, backlog: &mut Backlog) -> Result<Setup, NotStarted> {
    let shown = |err: keys::KeyError| NotStarted::Blocked(messages::sentence(&err.to_string()));
    let data_dir = keys::data_dir(None).map_err(shown)?;
    load_link_in(data_dir, link, backlog)
}

fn load_link_in(data_dir: PathBuf, link: &str, backlog: &mut Backlog) -> Result<Setup, NotStarted> {
    let (settings, _, _) = Settings::load(&data_dir);
    if settings.unread() || !settings.invite_links() || link.is_empty() {
        return Err(NotStarted::Quiet);
    }
    let may = win::may_take_foreground();
    let said = |line| {
        if may {
            NotStarted::Said(line)
        } else {
            NotStarted::Quiet
        }
    };
    let held = match running::hand_over(&data_dir, running::WAIT, link) {
        running::HandOver::Held(file) => Some(file),
        running::HandOver::Unheld(err) => {
            backlog.add(format!(
                "could not hold {}: {err}; a second copy on this data folder would not be stopped",
                data_dir.join(running::LOCK_FILE).display()
            ));
            None
        }
        running::HandOver::Handed => return Err(NotStarted::Quiet),
        running::HandOver::InRoom => return Err(said(messages::LINK_IN_ROOM)),
        running::HandOver::Older(pid) => {
            if may {
                running::bring_forward(pid);
            }
            return Err(said(messages::LINK_OLDER));
        }
        running::HandOver::NoAnswer => return Err(said(messages::LINK_NO_ANSWER)),
    };
    let identity = Identity::load_or_create(&data_dir)
        .map_err(|err| NotStarted::Blocked(messages::sentence(&err.to_string())))?;
    let args = Args {
        profile: None,
        port: None,
        log: false,
        loopback: None,
        video_loss: None,
    };
    #[cfg(feature = "memstats")]
    crate::memstats::start(&data_dir);
    Ok(setup_from(&args, data_dir, held, identity, backlog))
}

// A settings line that could not be used goes to the log and costs nothing
// else, but for a key that gave way, which the start screen says too. Apart
// from load, which takes the real profile's lock, for the tests.
fn setup_from(
    args: &Args,
    data_dir: PathBuf,
    held: Option<File>,
    identity: Identity,
    backlog: &mut Backlog,
) -> Setup {
    let (settings, problems, gave_way) = Settings::load(&data_dir);
    for problem in problems {
        backlog.add(problem);
    }
    let mut setup = Setup {
        _held: held,
        identity: Arc::new(identity),
        settings,
        port_given: args.port,
        gave_way,
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
            version: invite::VERSION,
        },
    };
    if let Some(knob) = setup.config.video.loss {
        backlog.add(format!(
            "--video-loss: dropping {}% of the video packets that arrive for a share this pc watches, seed {}",
            knob.percent, knob.seed
        ));
    }
    setup.apply_settings();
    setup
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::thread;

    use eframe::egui::{Pos2, RawInput, Rect, ViewportId, ViewportInfo, vec2};
    use voice::audio::fake::{Fake, Setup as FakeSetup};

    use crate::screens::room::tests as room_tests;
    use crate::{firewall, sound};

    fn setup_in(dir: &Path) -> Setup {
        Setup {
            _held: None,
            identity: Arc::new(Identity::generate()),
            settings: Settings::default(),
            port_given: None,
            gave_way: Vec::new(),
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
                version: invite::VERSION,
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

    fn test_app(setup: Result<Setup, String>, screen: Screen) -> App {
        App {
            setup,
            screen,
            firewall: firewall::tests::quiet(),
            backlog: Backlog::default(),
            stats_open: false,
            sweep: Sweep::default(),
            scrolling: false,
            notify: Arc::new(|| {}),
            not_now: None,
            damaged: Vec::new(),
            unusable: Vec::new(),
            hotkeys: Hotkeys::off(),
            target: Target::default(),
            flags: Arc::default(),
            control_offered: false,
            share_key: ShareKey::default(),
            raise: false,
            viewer_strip: Arc::default(),
            tray: None,
            tray_clicked: Arc::default(),
            inbox: Arc::default(),
            link_arrived: Arc::default(),
            in_room: Arc::default(),
            link: None,
            update: Update::off(),
            scale: None,
            focused: None,
            panel_keys: Rc::default(),
            card_watch: None,
        }
    }

    fn link_invite() -> Invite {
        Invite::new(
            *Identity::generate().public(),
            Vec::new(),
            invite::Mapping::Easy,
            false,
            false,
            false,
            None,
            false,
            start::unix_now(),
        )
        .unwrap()
    }

    fn arrive(app: &mut App, bytes: &[u8]) {
        let receiver = link_receiver(
            Arc::clone(&app.inbox),
            Arc::clone(&app.link_arrived),
            Arc::clone(&app.in_room),
            Arc::clone(&app.notify),
        );
        assert_eq!(receiver(bytes), 1);
        app.follow_link(None);
        assert!(app.inbox.lock().unwrap().is_none());
    }

    #[test]
    fn links_wait_on_the_screens_before_start() {
        let dir = fresh_dir("link-screens");
        let code = Zeroizing::new(invite::link(&link_invite().encode()));
        for screen in [
            Screen::Start(Start::default()),
            Screen::Checking,
            Screen::Firewall(Ask {
                blocked: false,
                standard_user: false,
                blocking_all: true,
            }),
        ] {
            let mut app = test_app(Ok(setup_in(&dir)), screen);
            arrive(&mut app, code.as_bytes());
            assert!(matches!(app.link, Some(Waiting::Shown { .. })));
            app.screen = Screen::Start(Start::default());
            assert!(matches!(app.link, Some(Waiting::Shown { .. })));
        }
        let mut app = app_in_settings(&dir, Start::default(), |_| {});
        arrive(&mut app, code.as_bytes());
        app.close_settings();
        assert!(matches!(app.screen, Screen::Start(_)));
        assert!(matches!(app.link, Some(Waiting::Shown { .. })));
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn links_are_dropped_when_the_panel_cannot_take_them() {
        let dir = fresh_dir("link-dropped");
        let code = Zeroizing::new(invite::link(&link_invite().encode()));
        let mut blocked = test_app(
            Err(String::from("The key could not be read.")),
            Screen::Start(Start::default()),
        );
        arrive(&mut blocked, code.as_bytes());
        assert!(blocked.link.is_none());
        let mut setup = setup_in(&dir);
        setup.settings.set_invite_links(false);
        let mut off = test_app(Ok(setup), Screen::Start(Start::default()));
        arrive(&mut off, code.as_bytes());
        assert!(off.link.is_none());
        let mut room = app_in_room(&dir);
        assert!(room.in_room.load(Ordering::Acquire));
        // An arrival taken just before the room opened is dropped by the panel too.
        *room.inbox.lock().unwrap() = Some(Zeroizing::new(code.as_bytes().to_vec()));
        room.link_arrived.store(true, Ordering::Release);
        room.follow_link(None);
        assert!(room.link.is_none());
        assert!(room.inbox.lock().unwrap().is_none());
        for app in [&blocked, &off, &room] {
            assert!(app.backlog.is_empty());
        }
        room.leave(&Context::default());
        assert!(!room.in_room.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_link_keeps_the_first_and_restarts_it() {
        let dir = fresh_dir("link-second");
        let first = link_invite();
        // A Join refused over a port in use can work at the next press, so
        // the page is still live.
        let held = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut setup = setup_in(&dir);
        setup.config.port = held.local_addr().unwrap().port();
        let mut app = test_app(Ok(setup), Screen::Start(Start::default()));
        arrive(&mut app, invite::link(&first.encode()).as_bytes());
        app.join_invite(first.clone(), &Context::default());
        assert!(matches!(
            &app.link,
            Some(Waiting::Shown { error: Some(_), .. })
        ));
        if let Some(Waiting::Shown { wait, .. }) = &mut app.link {
            // The page has already waited before another click on the link.
            wait.follow(
                true,
                Instant::now() - Duration::from_secs(2),
                Duration::from_secs(1),
            );
        }
        arrive(&mut app, invite::link(&link_invite().encode()).as_bytes());
        let Some(Waiting::Shown {
            invite,
            wait,
            error,
            ..
        }) = &mut app.link
        else {
            panic!("the first page stays");
        };
        assert_eq!(invite, &first);
        assert!(error.is_some());
        assert_eq!(
            wait.follow(true, Instant::now(), Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_link_replaces_a_refusal() {
        let dir = fresh_dir("link-after-refusal");
        let newer = link_invite();
        let mut app = test_app(Ok(setup_in(&dir)), Screen::Start(Start::default()));
        arrive(&mut app, b"not an invite");
        assert!(matches!(&app.link, Some(Waiting::Refused(_))));
        arrive(&mut app, invite::link(&newer.encode()).as_bytes());
        let Some(Waiting::Shown { invite, error, .. }) = &app.link else {
            panic!("the newer link is shown");
        };
        assert_eq!(invite, &newer);
        assert!(error.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_link_replaces_an_expired_invite() {
        let dir = fresh_dir("link-after-expiry");
        let newer = link_invite();
        let mut app = test_app(Ok(setup_in(&dir)), Screen::Start(Start::default()));
        arrive(&mut app, invite::link(&link_invite().encode()).as_bytes());
        let Some(Waiting::Shown { invite, .. }) = &mut app.link else {
            panic!("the first link is shown");
        };
        // The invite ran out while on show, which turns Join off and sets no
        // error.
        invite.expires_at = start::unix_now();
        arrive(&mut app, invite::link(&newer.encode()).as_bytes());
        let Some(Waiting::Shown { invite, error, .. }) = &app.link else {
            panic!("the newer link is shown");
        };
        assert_eq!(invite, &newer);
        assert!(error.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_link_replaces_a_page_that_cannot_join() {
        let dir = fresh_dir("link-after-own");
        let newer = link_invite();
        let mut app = test_app(Ok(setup_in(&dir)), Screen::Start(Start::default()));
        let mut own = link_invite();
        own.host_key = *app.setup.as_ref().unwrap().identity.public();
        arrive(&mut app, invite::link(&own.encode()).as_bytes());
        app.join_invite(own, &Context::default());
        assert!(
            matches!(&app.link, Some(Waiting::Shown { error: Some(error), .. }) if error == &messages::room_error(&room::RoomError::OwnInvite))
        );
        arrive(&mut app, invite::link(&newer.encode()).as_bytes());
        let Some(Waiting::Shown { invite, error, .. }) = &app.link else {
            panic!("the newer link is shown");
        };
        assert_eq!(invite, &newer);
        assert!(error.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_links_are_refused_without_logging_their_text() {
        let dir = fresh_dir("link-refused");
        let mut setup = setup_in(&dir);
        setup.config.log = Some(dir.join("booth.log"));
        let mut app = test_app(Ok(setup), Screen::Start(Start::default()));
        for (bytes, expected) in [
            (vec![0xff], String::from(messages::LINK_NOT_BOOTH)),
            (Vec::new(), String::from(messages::LINK_NOT_BOOTH)),
            (
                b"booth1-abc".to_vec(),
                messages::code_error(&CodeError::Damaged),
            ),
        ] {
            app.clear_link();
            arrive(&mut app, &bytes);
            assert!(matches!(&app.link, Some(Waiting::Refused(reason)) if reason == &expected));
        }
        app.clear_link();
        let mut expired = link_invite();
        expired.expires_at = start::unix_now();
        arrive(&mut app, invite::link(&expired.encode()).as_bytes());
        assert!(
            matches!(&app.link, Some(Waiting::Refused(reason)) if reason == &messages::code_error(&CodeError::Expired))
        );
        app.clear_link();
        arrive(&mut app, invite::link(&link_invite().encode()).as_bytes());
        assert!(
            app.backlog
                .texts()
                .iter()
                .any(|line| line.starts_with("invite link: on show, host "))
        );
        assert!(
            app.backlog
                .texts()
                .iter()
                .all(|line| !line.contains("booth1-"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_links_off_drops_the_waiting_invite() {
        let dir = fresh_dir("link-save-off");
        let mut app = app_in_settings(&dir, Start::default(), |draft| draft.invite_links = false);
        arrive(&mut app, invite::link(&link_invite().encode()).as_bytes());
        assert!(app.link.is_some());
        app.save_settings();
        assert!(app.link.is_none());
        assert!(app.inbox.lock().unwrap().is_none());
        assert!(!app.setup.as_ref().unwrap().settings.invite_links());
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_a_room_refuses_links_until_the_call_ends() {
        let in_room = Arc::new(AtomicBool::new(false));
        let receiver = link_receiver(
            Arc::default(),
            Arc::default(),
            Arc::clone(&in_room),
            Arc::new(|| {}),
        );
        assert_eq!(receiver(&[1]), 1);
        let opened = opening(&in_room, || {
            assert!(in_room.load(Ordering::Acquire));
            assert_eq!(receiver(&[2]), 2);
            Err(room::RoomError::OwnInvite)
        });
        assert!(opened.is_err());
        assert!(!in_room.load(Ordering::Acquire));
        let dir = fresh_dir("link-opening");
        let room = opening(&in_room, || {
            assert!(in_room.load(Ordering::Acquire));
            Ok(room_tests::test_room(&dir))
        })
        .unwrap();
        assert!(in_room.load(Ordering::Acquire));
        room.leave();
        let held = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut setup = setup_in(&dir);
        setup.config.port = held.local_addr().unwrap().port();
        let mut app = test_app(Ok(setup), Screen::Start(Start::default()));
        app.host(&Context::default());
        assert!(!app.in_room.load(Ordering::Acquire));
        assert!(start_of(&app).host_error.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    // Host, Join on a link's page and Join on a known host each ask for
    // their room with links already refused, and take them again when it
    // fails: here the port is held.
    #[test]
    fn every_way_into_a_room_refuses_links_first() {
        let dir = fresh_dir("link-asked");
        let host_dir = dir.join("host");
        std::fs::create_dir_all(&host_dir).unwrap();
        let host = room_tests::test_room(&host_dir);
        room_tests::test_friend(&host, &dir).leave();
        host.leave();
        let known = Room::known_hosts(&dir).unwrap();
        let key = *known[0].host_key();
        let held = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut setup = setup_in(&dir);
        setup.config.port = held.local_addr().unwrap().port();
        let mut app = test_app(
            Ok(setup),
            Screen::Start(Start {
                known,
                ..Start::default()
            }),
        );
        let mut asked = Vec::new();
        app.host(&Context::default());
        asked.push(ASKED.take());
        app.join_invite(link_invite(), &Context::default());
        asked.push(ASKED.take());
        app.rejoin(key, &Context::default());
        asked.push(ASKED.take());
        assert_eq!(asked, [Some(true); 3]);
        assert!(!app.in_room.load(Ordering::Acquire));
        let start = start_of(&app);
        assert!(start.host_error.is_some());
        assert!(start.join_error.is_some());
        assert!(start.known_error.is_some());
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    // The flag goes off before the inbox is emptied. The other way round, a
    // link the tray stored between the two would wait there unflagged, and
    // the next link would show it in its own place.
    #[test]
    fn clearing_a_link_turns_the_flag_off_before_the_take() {
        let mut app = test_app(Err(String::new()), Screen::Checking);
        app.link_arrived.store(true, Ordering::Release);
        let inbox = Arc::clone(&app.inbox);
        let arrived = Arc::clone(&app.link_arrived);
        let (locked, held) = std::sync::mpsc::channel();
        let tray = thread::spawn(move || {
            let slot = inbox.lock().unwrap();
            locked.send(()).unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            while arrived.load(Ordering::Acquire) && Instant::now() < until {
                thread::yield_now();
            }
            let off = !arrived.load(Ordering::Acquire);
            drop(slot);
            off
        });
        held.recv().unwrap();
        app.clear_link();
        assert!(
            tray.join().unwrap(),
            "the flag was still on while the inbox was taken"
        );
    }

    #[test]
    fn link_wait_restarts_when_the_page_cannot_be_seen() {
        let dir = fresh_dir("link-unseen");
        for (start, focus, minimized) in [
            (false, Some(true), false),
            (true, None, false),
            (true, Some(false), false),
            (true, Some(true), true),
        ] {
            let screen = if start {
                Screen::Start(Start::default())
            } else {
                Screen::Checking
            };
            let mut app = test_app(Ok(setup_in(&dir)), screen);
            arrive(&mut app, invite::link(&link_invite().encode()).as_bytes());
            let at = Instant::now();
            if let Some(Waiting::Shown { wait, .. }) = &mut app.link {
                wait.follow(true, at - Duration::from_secs(2), Duration::from_secs(1));
                assert!(wait.follow(true, at, Duration::from_secs(1)).is_none());
            }
            let ctx = Context::default();
            let mut input = with_window(minimized);
            input.viewports.get_mut(&ViewportId::ROOT).unwrap().focused = focus;
            let _ = ctx.run_logic(&input, |ctx| {
                eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest())
            });
            let Some(Waiting::Shown { wait, .. }) = &mut app.link else {
                panic!("the page waits");
            };
            assert_eq!(
                wait.follow(true, at + Duration::from_secs(2), Duration::from_secs(1)),
                Some(Duration::from_secs(1))
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_join_failure_keeps_the_page_and_success_wipes_the_paste() {
        let dir = fresh_dir("link-join");
        let mut setup = setup_in(&dir);
        setup.config.port = 0;
        setup.config.candidates = Candidates::Fixed(Vec::new());
        setup.config.video.source = room::VideoSource::Hooks;
        let fake = Fake::new(FakeSetup::default(), &[("a", "Test sound")], Some("a"));
        setup.config.voice.devices = Devices::Fake {
            microphone: fake.clone(),
            speakers: fake.clone(),
            pc_sound: fake,
        };
        let id = eframe::egui::Id::new("link join paste");
        let ctx = Context::default();
        TextEditState::default().store(&ctx, id);
        let mut app = test_app(
            Ok(setup),
            Screen::Start(Start {
                paste: String::from("booth1-private"),
                paste_id: Some(id),
                ..Start::default()
            }),
        );
        let invite = link_invite();
        arrive(&mut app, invite::link(&invite.encode()).as_bytes());
        let mut own = invite.clone();
        own.host_key = *app.setup.as_ref().unwrap().identity.public();
        app.join_invite(own, &ctx);
        assert!(!app.in_room.load(Ordering::Acquire));
        assert!(
            matches!(&app.link, Some(Waiting::Shown { error: Some(error), .. }) if error == &messages::room_error(&room::RoomError::OwnInvite))
        );
        assert_eq!(start_of(&app).paste, "booth1-private");
        let mut expired = invite.clone();
        expired.expires_at = start::unix_now();
        app.join_invite(expired, &ctx);
        assert!(
            matches!(&app.link, Some(Waiting::Shown { error: Some(error), .. }) if error == &messages::room_error(&room::RoomError::InviteExpired))
        );
        app.join_invite(invite, &ctx);
        assert!(matches!(app.screen, Screen::Room(_)));
        assert!(app.link.is_none());
        assert!(TextEditState::load(&ctx, id).is_none());
        app.leave(&ctx);
        assert!(start_of(&app).paste.is_empty());
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    // Host and Join on a known host take no invite from the paste field, but
    // one pasted there and not used goes as Join's does, field and undo
    // history, or Ctrl+Z after Leave would bring it back.
    #[test]
    fn host_and_rejoin_wipe_an_unused_paste() {
        let dir = fresh_dir("paste-unused");
        let host_dir = dir.join("host");
        std::fs::create_dir_all(&host_dir).unwrap();
        let host = room_tests::test_room(&host_dir);
        room_tests::test_friend(&host, &dir).leave();
        host.leave();
        let known = Room::known_hosts(&dir).unwrap();
        let key = *known[0].host_key();
        let mut setup = setup_in(&dir);
        setup.config.port = 0;
        setup.config.candidates = Candidates::Fixed(Vec::new());
        setup.config.video.source = room::VideoSource::Hooks;
        let fake = Fake::new(FakeSetup::default(), &[("a", "Test sound")], Some("a"));
        setup.config.voice.devices = Devices::Fake {
            microphone: fake.clone(),
            speakers: fake.clone(),
            pc_sound: fake,
        };
        let mut app = test_app(
            Ok(setup),
            Screen::Start(Start {
                known,
                ..Start::default()
            }),
        );
        let id = eframe::egui::Id::new("unused paste");
        let ctx = Context::default();
        let paste = |app: &mut App| {
            TextEditState::default().store(&ctx, id);
            let Screen::Start(start) = &mut app.screen else {
                panic!("no start screen");
            };
            start.paste = String::from("booth1-private");
            start.paste_id = Some(id);
        };
        paste(&mut app);
        app.host(&ctx);
        assert!(matches!(app.screen, Screen::Room(_)), "Host");
        assert!(TextEditState::load(&ctx, id).is_none(), "Host");
        app.leave(&ctx);
        paste(&mut app);
        app.rejoin(key, &ctx);
        assert!(matches!(app.screen, Screen::Room(_)), "Join");
        assert!(TextEditState::load(&ctx, id).is_none(), "Join");
        app.leave(&ctx);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    // A code left in the room's field goes with the room, field and undo
    // history: the field is the same one in the next room, where Ctrl+Z
    // would bring it back.
    #[test]
    fn leave_wipes_a_code_left_in_the_room() {
        let dir = fresh_dir("code-left");
        let mut app = app_in_room(&dir);
        let ctx = Context::default();
        theme::apply(&ctx);
        let id = room_tests::code_left(in_room(&mut app), &ctx, "booth1-r-q8w2e4");
        assert!(TextEditState::load(&ctx, id).is_some());
        app.leave(&ctx);
        assert!(TextEditState::load(&ctx, id).is_none());
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_cold_start_respects_settings_before_loading_a_key() {
        let dir = fresh_dir("link-cold");
        std::fs::write(dir.join(crate::settings::FILE), b"invite_links = off\r\n").unwrap();
        assert!(matches!(
            load_link_in(dir.clone(), "booth://join/test", &mut Backlog::default()),
            Err(NotStarted::Quiet)
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(dir.join(crate::settings::FILE)).unwrap();
        std::fs::create_dir(dir.join(crate::settings::FILE)).unwrap();
        assert!(matches!(
            load_link_in(dir.clone(), "booth://join/test", &mut Backlog::default()),
            Err(NotStarted::Quiet)
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir(dir.join(crate::settings::FILE)).unwrap();
        assert!(matches!(
            load_link_in(dir.clone(), "", &mut Backlog::default()),
            Err(NotStarted::Quiet)
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let setup = load_link_in(dir.clone(), "booth://join/test", &mut Backlog::default())
            .ok()
            .expect("a cold link opens the default panel");
        assert!(setup._held.is_some());
        assert!(setup.settings.invite_links());
        drop(setup);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn fresh_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("booth-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // In a room that shares nothing by itself, so no monitor or encoder opens.
    fn app_in_room(dir: &Path) -> App {
        let mut app = test_app(Ok(setup_in(dir)), Screen::Checking);
        app.enter(room_tests::test_room(dir));
        app
    }

    fn in_room(app: &mut App) -> &mut InRoom {
        match &mut app.screen {
            Screen::Room(in_room) => in_room,
            _ => panic!("not in a room"),
        }
    }

    fn share_goes(app: &mut App, on: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while own_share_on(&in_room(app).room.view().share.own) != on {
            assert!(Instant::now() < deadline, "the share never went {on}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Two presses of the share key within a second.
    fn press_share_key_twice(app: &mut App, ctx: &Context) {
        let at = Instant::now();
        app.share_key(at, ctx);
        app.share_key(at + Duration::from_millis(100), ctx);
    }

    fn close_room(mut app: App) {
        app.leave(&Context::default());
    }

    fn with_window(minimized: bool) -> RawInput {
        let mut input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(360.0, 640.0))),
            ..RawInput::default()
        };
        input.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo {
                minimized: Some(minimized),
                ..ViewportInfo::default()
            },
        );
        input
    }

    // Minimized, eframe calls logic and not ui. The microphone and Hear
    // yourself stop there, and ui opens the microphone again with the window
    // back. Hear yourself stays off until it is turned on again.
    #[test]
    fn settings_audio_stops_while_the_window_is_minimized() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let fake = Fake::new(FakeSetup::default(), &[("a", "Headset A")], Some("a"));
        let draft = Draft::new(&Settings::default(), KnownDevices::default(), String::new());
        let mut audio = sound::tests::fake_audio(&ctx, &fake, &draft.input);
        sound::tests::fake_hear(&mut audio, &fake, &draft.output);
        let mut app = test_app(
            Err(String::new()),
            Screen::Settings {
                draft: Box::new(draft),
                start: Start::default(),
                audio: Box::new(audio),
            },
        );
        let hearing = |app: &App| match &app.screen {
            Screen::Settings { audio, .. } => audio.hearing(),
            _ => panic!("the screen changed"),
        };
        let mut frame = eframe::Frame::_new_kittest();
        sound::tests::wait_until("both streams open", || fake.record().at_once == 2);

        // On screen, logic leaves them alone.
        let _ = ctx.run_logic(&with_window(false), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        assert!(hearing(&app));
        assert_eq!(fake.record().at_once, 2);

        let _ = ctx.run_logic(&with_window(true), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        assert!(!hearing(&app));
        sound::tests::wait_until("both streams close", || fake.record().at_once == 0);

        // Further passes while minimized open nothing.
        let opens = fake.record().opens.len();
        let _ = ctx.run_logic(&with_window(true), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.record().opens.len(), opens);

        let _ = ctx.run_logic(&with_window(false), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        ctx.run_ui(with_window(false), |ui| {
            eframe::App::ui(&mut app, ui, &mut frame);
        })
        .drop_without_applying_deltas();
        sound::tests::wait_until("the microphone opens", || {
            fake.record().opens.len() == opens + 1
        });
        assert_eq!(fake.record().at_once, 1);
        assert!(!hearing(&app));
    }

    // On the settings screen with fake sound, as the start screen left it.
    fn app_in_settings(dir: &Path, start: Start, change: impl FnOnce(&mut Draft)) -> App {
        let setup = setup_in(dir);
        let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
        change(&mut draft);
        let fake = Fake::new(FakeSetup::default(), &[("a", "Headset A")], Some("a"));
        let audio = sound::tests::fake_audio(&Context::default(), &fake, &draft.input);
        test_app(
            Ok(setup),
            Screen::Settings {
                draft: Box::new(draft),
                start,
                audio: Box::new(audio),
            },
        )
    }

    fn start_of(app: &App) -> &Start {
        match &app.screen {
            Screen::Start(start) | Screen::Settings { start, .. } => start,
            _ => panic!("no start screen"),
        }
    }

    // Forget works before Settings opens, and takes no click straight after
    // it closes, when a second click on Save or Cancel can land there.
    #[test]
    fn a_second_click_on_leaving_settings_forgets_nothing() {
        use crate::controls::tests::{frame, press, release};
        let dir = fresh_dir("forget-after-settings");
        let host_dir = dir.join("host");
        std::fs::create_dir_all(&host_dir).unwrap();
        let host = room_tests::test_room(&host_dir);
        room_tests::test_friend(&host, &dir).leave();
        host.leave();
        let known = Room::known_hosts(&dir).unwrap();
        let key = *known[0].host_key();
        let mut start = Start {
            known,
            opened: Some(key),
            ..Start::default()
        };
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let draw = |start: &mut Start, events| {
            frame(&ctx, events, |ui| {
                start::show(ui, start, None, None, &[], None, None)
            })
        };
        draw(&mut start, Vec::new());
        let (_, output) = draw(&mut start, Vec::new());
        let forget = start::tests::places(&output, "Forget")[0].center();
        draw(&mut start, press(forget));
        let (choice, _) = draw(&mut start, release(forget));
        assert!(matches!(choice, Some(Choice::Forget(k)) if k == key));

        let mut app = app_in_settings(&dir, start, |_| {});
        let closed = Instant::now();
        app.close_settings();
        let Screen::Start(start) = &mut app.screen else {
            panic!("no start screen");
        };
        assert_eq!(start.opened, Some(key));
        assert!(start.opened_at >= Some(closed));
        draw(start, press(forget));
        let (choice, _) = draw(start, release(forget));
        // These frames are real time, and a slow PC may take them past the wait.
        assert!(choice.is_none() || closed.elapsed() >= Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn no_card() -> Result<Vec<capture::Adapter>, capture::CaptureError> {
        Ok(Vec::new())
    }

    fn a_card() -> Result<Vec<capture::Adapter>, capture::CaptureError> {
        Ok(vec![capture::Adapter {
            description: String::from("NVIDIA GeForce RTX 4070 Ti SUPER"),
            vendor_id: 0x10de,
            device_id: 0x2705,
            luid: 9,
        }])
    }

    // Whether a frame of the whole panel shows the CPU line, and how long it
    // asks to wait before the next.
    fn cpu_frame(app: &mut App, ctx: &Context) -> (bool, Duration) {
        let mut frame = eframe::Frame::_new_kittest();
        let mut output = ctx.run_ui(with_window(false), |ui| {
            eframe::App::ui(app, ui, &mut frame);
        });
        let delay = output.viewport_output[&ViewportId::ROOT].repaint_delay;
        let update = std::mem::take(&mut output.platform_output)
            .accesskit_update
            .expect("accesskit is on");
        output.drop_without_applying_deltas();
        let shown = update
            .nodes
            .iter()
            .any(|(_, node)| [node.label(), node.value()].contains(&Some(messages::PANEL_ON_CPU)));
        (shown, delay)
    }

    fn listed_line(app: &App) -> bool {
        app.backlog
            .texts()
            .iter()
            .any(|text| text.starts_with("panel: NVIDIA GeForce RTX 4070 Ti SUPER is listed now"))
    }

    // On a card the start screen says nothing and asks for no frames. On the
    // CPU it says so from the frame whose look finds a card, a card listed
    // already at the first look included, and until then it asks for a frame
    // by the next look, since nothing else wakes the start screen.
    #[test]
    fn the_start_screen_says_when_a_card_came_for_a_panel_on_the_cpu() {
        let dir = fresh_dir("on-cpu");
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let mut app = test_app(Ok(setup_in(&dir)), Screen::Start(Start::default()));
        // The first frames of a context ask for more by themselves.
        cpu_frame(&mut app, &ctx);
        cpu_frame(&mut app, &ctx);
        assert_eq!(
            cpu_frame(&mut app, &ctx),
            (false, Duration::MAX),
            "on a card"
        );

        app.card_watch = Some(CardWatch::listing(no_card, Instant::now()));
        let (shown, delay) = cpu_frame(&mut app, &ctx);
        assert!(!shown, "before a card came");
        assert!(
            delay > Duration::ZERO && delay <= adapter::LOOK_EVERY,
            "{delay:?}"
        );
        assert!(!listed_line(&app));

        app.card_watch = Some(CardWatch::listing(a_card, Instant::now()));
        assert_eq!(
            cpu_frame(&mut app, &ctx),
            (true, Duration::MAX),
            "after a card came"
        );
        assert!(listed_line(&app), "{:?}", app.backlog.texts());
        assert_eq!(
            cpu_frame(&mut app, &ctx),
            (true, Duration::MAX),
            "on the next frame"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // In a room the line goes under your row once a card came, and on the CPU
    // the trace stands still whatever Windows animations are.
    #[test]
    fn the_room_says_when_a_card_came_for_a_panel_on_the_cpu() {
        let dir = fresh_dir("on-cpu-room");
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let mut app = app_in_room(&dir);
        assert!(!cpu_frame(&mut app, &ctx).0, "on a card");
        assert_eq!(app.scrolling, win::animations_on());

        app.card_watch = Some(CardWatch::listing(no_card, Instant::now()));
        assert!(!cpu_frame(&mut app, &ctx).0, "before a card came");
        assert!(!app.scrolling);

        app.card_watch = Some(CardWatch::listing(a_card, Instant::now()));
        assert!(cpu_frame(&mut app, &ctx).0, "after a card came");
        assert!(listed_line(&app), "{:?}", app.backlog.texts());
        assert!(!app.scrolling);
        close_room(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The settings go in and the list fails after them. Neither Save again
    // nor Cancel finds a change after that, so the new key has to reach the
    // hotkeys here, or the old one goes on working until Booth restarts.
    #[test]
    fn a_save_that_fails_on_a_list_still_moves_the_hotkeys() {
        let f9: Chord = "F9".parse().unwrap();
        let mute = |app: &App| app.hotkeys.last_given().chord(Action::Mute);
        for cancel in [false, true] {
            let dir = fresh_dir(&format!("hotkeys-list-{cancel}"));
            let mut app = app_in_settings(&dir, Start::default(), |draft| {
                draft.hotkeys.set(Action::Mute, f9);
                draft.removed.push([7; 32]);
            });
            std::fs::create_dir(dir.join("devices.bin")).unwrap();
            app.save_settings();
            assert!(matches!(app.screen, Screen::Settings { .. }));
            assert_eq!(mute(&app), f9, "after the failed save");
            let moved = hotkeys::bindings_line(&app.hotkeys.last_given());
            assert!(app.backlog.texts().contains(&moved.as_str()));

            std::fs::remove_dir(dir.join("devices.bin")).unwrap();
            if cancel {
                app.close_settings();
            } else {
                app.save_settings();
            }
            assert!(matches!(app.screen, Screen::Start(_)), "{cancel}");
            assert_eq!(mute(&app), f9, "{cancel}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // Refused, or settings.txt not written: the hotkeys stay on what the
    // settings still say, and the screen brings up why.
    #[test]
    fn a_save_that_changed_nothing_leaves_the_hotkeys() {
        let f9: Chord = "F9".parse().unwrap();
        let dir = fresh_dir("hotkeys-refused");
        let mut app = app_in_settings(&dir, Start::default(), |draft| {
            draft.hotkeys.set(Action::Mute, f9);
            draft.port = String::from("80");
        });
        let reason_due = |app: &App| match &app.screen {
            Screen::Settings { draft, .. } => draft.show_reason,
            _ => panic!("the screen closed"),
        };
        assert!(!reason_due(&app));
        app.save_settings();
        assert!(reason_due(&app), "a refused field");
        assert_eq!(app.hotkeys.last_given(), input::Bindings::default());

        let Screen::Settings { draft, .. } = &mut app.screen else {
            panic!("the screen closed");
        };
        draft.show_reason = false;
        draft.port = DEFAULT_PORT.to_string();
        std::fs::create_dir(dir.join("settings.txt")).unwrap();
        app.save_settings();
        assert!(reason_due(&app), "settings.txt not written");
        assert_eq!(app.hotkeys.last_given(), input::Bindings::default());
        std::fs::remove_dir(dir.join("settings.txt")).unwrap();

        let Screen::Settings { draft, .. } = &mut app.screen else {
            panic!("the screen closed");
        };
        draft.show_reason = false;
        app.flags = Arc::new(Flags::controlled_for_tests());
        app.save_settings();
        assert!(reason_due(&app), "this pc is controlled");
        assert_eq!(app.hotkeys.last_given(), input::Bindings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn with_errors() -> Start {
        Start {
            host_error: Some(String::from("host")),
            join_error: Some(String::from("join")),
            known_error: Some(String::from("known")),
            ..Start::default()
        }
    }

    // "Change the port in settings", with the old port in the sentence, goes
    // once the settings have changed, a Save that failed on a list after them
    // included. Save with no change and Cancel leave the sentence true.
    #[test]
    fn a_change_in_settings_ends_the_start_screen_s_errors() {
        let gone = |app: &App| {
            let start = start_of(app);
            [&start.host_error, &start.join_error, &start.known_error].map(Option::is_none)
        };
        let dir = fresh_dir("start-errors");

        let mut app = app_in_settings(&dir, with_errors(), |_| {});
        app.save_settings();
        assert!(matches!(app.screen, Screen::Start(_)));
        assert_eq!(gone(&app), [false; 3], "nothing changed");

        let port = |draft: &mut Draft| draft.port = String::from("41010");
        let mut app = app_in_settings(&dir, with_errors(), port);
        app.close_settings();
        assert_eq!(gone(&app), [false; 3], "cancelled");

        let mut app = app_in_settings(&dir, with_errors(), |draft| {
            draft.port = String::from("80");
        });
        app.save_settings();
        assert_eq!(gone(&app), [false; 3], "refused");

        let mut app = app_in_settings(&dir, with_errors(), port);
        std::fs::create_dir(dir.join("settings.txt")).unwrap();
        app.save_settings();
        assert_eq!(gone(&app), [false; 3], "not written");
        std::fs::remove_dir(dir.join("settings.txt")).unwrap();

        let mut app = app_in_settings(&dir, with_errors(), port);
        app.save_settings();
        assert!(matches!(app.screen, Screen::Start(_)));
        assert_eq!(gone(&app), [true; 3], "saved");
        std::fs::remove_file(dir.join("settings.txt")).unwrap();

        let mut app = app_in_settings(&dir, with_errors(), |draft| {
            port(draft);
            draft.removed.push([7; 32]);
        });
        std::fs::create_dir(dir.join("devices.bin")).unwrap();
        app.save_settings();
        assert!(matches!(app.screen, Screen::Settings { .. }));
        app.close_settings();
        assert_eq!(gone(&app), [true; 3], "saved, then the list failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The red line under the list is about the last rejoin or Forget, so a
    // Forget that fails and then works, or an address saved, ends it.
    #[test]
    fn a_known_host_step_that_works_ends_the_last_error() {
        let host_dir = fresh_dir("known-error-host");
        let dir = fresh_dir("known-error-friend");
        let host = room_tests::test_room(&host_dir);
        room_tests::test_friend(&host, &dir).leave();
        host.leave();
        let known = Room::known_hosts(&dir).unwrap();
        let key = *known[0].host_key();
        let list = dir.join("hosts.bin");
        let aside = dir.join("hosts.bin.aside");

        let mut app = test_app(
            Ok(setup_in(&dir)),
            Screen::Start(Start {
                known,
                known_error: Some(String::from("the last rejoin")),
                ..Start::default()
            }),
        );
        app.save_address(key);
        assert_eq!(start_of(&app).known_error, None, "an address saved");

        std::fs::rename(&list, &aside).unwrap();
        std::fs::create_dir(&list).unwrap();
        app.forget(key);
        assert!(start_of(&app).known_error.is_some());
        assert_eq!(start_of(&app).forgot, None);

        std::fs::remove_dir(&list).unwrap();
        std::fs::rename(&aside, &list).unwrap();
        app.forget(key);
        assert!(start_of(&app).forgot.is_some());
        assert_eq!(start_of(&app).known_error, None, "forgotten");
        assert!(start_of(&app).known.is_empty());
        let _ = std::fs::remove_dir_all(&host_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // While this PC is controlled, Save refuses a change to the panic key,
    // the other hotkeys, the sharing settings, the voice devices or how you
    // talk, writes nothing, and keeps the draft for a Save once control has
    // ended.
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
        let other = voice::audio::Choice::Device(String::from("other"));
        refused("talk", &|draft| draft.talk = TalkMode::OpenMic);
        refused("input", &|draft| draft.input = other.clone());
        refused("output", &|draft| draft.output = other.clone());
        refused("sensitivity", &|draft| {
            draft.sensitivity_db = *room::SENSITIVITY_DB.start();
        });
        refused("constant rate", &|draft| draft.constant_rate = false);
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

    #[test]
    fn room_settings_save_only_applies_what_reached_disk() {
        let dir = fresh_dir("room-settings-save");
        let ctx = Context::default();
        let mut app = app_in_room(&dir);
        let room = Arc::clone(&in_room(&mut app).room);
        std::fs::write(dir.join("devices.bin"), b"unreadable").unwrap();
        app.open_room_settings(&ctx);
        let draft = app.settings_draft_mut().unwrap();
        assert!(draft.devices.is_empty() && draft.list_error.is_none());
        assert_eq!(
            std::fs::read(dir.join("devices.bin")).unwrap(),
            b"unreadable"
        );
        draft.talk = TalkMode::OpenMic;
        draft.invite_links = false;
        let output = voice::audio::Choice::Device(String::from("other"));
        draft.output = output.clone();
        let panic: Chord = "F9".parse().unwrap();
        draft.hotkeys.set(Action::Panic, panic);
        draft.change(Action::Mute);
        app.follow_capture();
        assert!(app.hotkeys.capturing());
        std::fs::create_dir(dir.join("settings.txt")).unwrap();
        app.save_room_settings();
        assert_eq!(room.view().voice.mode, TalkMode::PushToTalk);
        assert_eq!(app.hotkeys.last_given(), input::Bindings::default());
        assert!(app.settings_draft_mut().unwrap().show_reason);
        assert!(!app.hotkeys.capturing());
        assert!(app.setup.as_ref().unwrap().settings.invite_links());
        let (_, links, devices) = room_tests::room_settings_values(in_room(&mut app));
        assert!(links);
        assert_eq!(devices[1], voice::audio::Choice::Default);
        std::fs::remove_dir(dir.join("settings.txt")).unwrap();
        app.remember_monitor(r"\\.\DISPLAY2");
        app.save_room_settings();
        assert!(app.settings_draft_mut().is_none());
        let until = Instant::now() + Duration::from_secs(3);
        while room.view().voice.mode != TalkMode::OpenMic {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(Arc::ptr_eq(&room, &in_room(&mut app).room));
        assert_eq!(app.hotkeys.last_given().chord(Action::Panic), panic);
        let (words, links, devices) = room_tests::room_settings_values(in_room(&mut app));
        assert_eq!(words, "F9");
        assert!(!links);
        assert_eq!(devices[1], output);
        let saved = Settings::load(&dir).0;
        assert_eq!(saved.share_monitor(), Some(r"\\.\DISPLAY2"));
        assert_eq!(saved.talk_mode(), TalkMode::OpenMic);
        assert!(!saved.invite_links());
        drop(room);
        close_room(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_voice_setting_counts_as_a_voice_change() {
        let before = Settings::default();
        let other = voice::audio::Choice::Device(String::from("other"));
        let differs = |change: &dyn Fn(&mut Settings)| {
            let mut now = before.clone();
            change(&mut now);
            voice_settings_differ(&now, &before)
        };
        assert!(differs(&|now| now.set_device(Direction::Input, &other)));
        assert!(differs(&|now| now.set_device(Direction::Output, &other)));
        assert!(differs(&|now| now.set_talk_mode(TalkMode::OpenMic)));
        assert!(differs(&|now| now.set_constant_rate(false)));
        assert!(differs(
            &|now| now.set_sensitivity_db(*room::SENSITIVITY_DB.start())
        ));
        assert!(differs(&|now| now.set_echo_cancellation(false)));
        assert!(differs(&|now| now.set_noise_suppression(true)));
        assert!(differs(&|now| now.set_automatic_volume(false)));
        assert!(!differs(&|now| now.set_name("Tom")));
    }

    #[test]
    fn room_settings_from_the_gear_open_no_microphone() {
        let dir = fresh_dir("room-settings-no-mic");
        let mut app = app_in_room(&dir);
        app.open_room_settings(&Context::default());
        assert!(!room_tests::settings_have_a_microphone(in_room(&mut app)));
        close_room(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn room_settings_apply_sound_cues_on_save() {
        let dir = fresh_dir("room-settings-cues");
        let ctx = Context::default();
        let mut app = app_in_room(&dir);
        let room = Arc::clone(&in_room(&mut app).room);
        let before = room.sound_cues();
        app.open_room_settings(&ctx);
        let draft = app.settings_draft_mut().unwrap();
        draft.cues.own_mute = false;
        draft.cues.volume = 60;
        let wanted = draft.cues;
        std::fs::create_dir(dir.join("settings.txt")).unwrap();
        app.save_room_settings();
        assert_eq!(room.sound_cues(), before, "the write failed");
        std::fs::remove_dir(dir.join("settings.txt")).unwrap();
        app.save_room_settings();
        assert!(app.settings_draft_mut().is_none());
        assert_eq!(room.sound_cues(), wanted);
        assert_eq!(Settings::load(&dir).0.sound_cues(), wanted);
        drop(room);
        close_room(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn room_settings_keep_a_failed_device_choice() {
        let dir = fresh_dir("room-settings-device");
        let ctx = Context::default();
        let mut app = app_in_room(&dir);
        app.open_room_settings(&ctx);
        app.settings_draft_mut().unwrap().input =
            voice::audio::Choice::Device(String::from("missing"));
        let before = Instant::now();
        app.save_room_settings();
        assert!(before.elapsed() < Duration::from_millis(500));
        assert!(app.settings_draft_mut().is_none());
        let room = Arc::clone(&in_room(&mut app).room);
        let until = Instant::now() + Duration::from_secs(3);
        while room.view().voice.microphone.is_none() {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            room.view().voice.microphone,
            Some(voice::audio::AudioError::NotConnected { .. })
        ));
        assert_eq!(
            Settings::load(&dir).0.device(Direction::Input).id(),
            Some("missing")
        );
        app.open_room_settings(&ctx);
        assert_eq!(
            app.settings_draft_mut().unwrap().input.id(),
            Some("missing")
        );
        app.close_room_settings();
        drop(room);
        close_room(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn room_settings_cancel_cover_and_leave_end_capture() {
        let dir = fresh_dir("room-settings-cover");
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        let mut app = app_in_room(&dir);
        let mut frame = eframe::Frame::_new_kittest();
        let draw = |app: &mut App, events| {
            let mut input = with_window(false);
            input.events = events;
            let mut output = ctx.run_ui(input, |ui| {
                eframe::App::ui(app, ui, &mut eframe::Frame::_new_kittest());
            });
            let tree = std::mem::take(&mut output.platform_output)
                .accesskit_update
                .unwrap();
            output.drop_without_applying_deltas();
            tree
        };
        app.open_room_settings(&ctx);
        app.settings_draft_mut().unwrap().name = String::from("draft");
        let tree = draw(&mut app, Vec::new());
        assert!(
            tree.nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Stats"))
        );
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some(messages::MESSAGE_HINT))
        );
        app.settings_draft_mut().unwrap().change(Action::Mute);
        app.follow_capture();
        assert!(app.hotkeys.capturing());
        app.stats_open = true;
        app.follow_capture();
        assert!(!app.hotkeys.capturing());
        assert!(app.settings_draft_mut().unwrap().waiting.is_none());
        room_tests::cover_settings(in_room(&mut app));
        let escape = || {
            vec![eframe::egui::Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }]
        };
        let tree = draw(&mut app, escape());
        assert!(!in_room(&mut app).share_list_open());
        assert!(app.stats_open);
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Save"))
        );
        draw(&mut app, escape());
        assert!(!app.stats_open);
        assert_eq!(app.settings_draft_mut().unwrap().name, "draft");
        draw(&mut app, escape());
        assert!(app.settings_draft_mut().is_none());
        assert!(!dir.join("settings.txt").exists());
        app.open_room_settings(&ctx);
        app.settings_draft_mut().unwrap().change(Action::Mute);
        app.follow_capture();
        assert!(app.hotkeys.capturing());
        app.leave(&ctx);
        assert!(!app.hotkeys.capturing());
        let _ = ctx.run_logic(&with_window(false), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame)
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn room_settings_stay_refused_while_controlled() {
        let dir = fresh_dir("room-settings-controlled");
        let mut app = app_in_room(&dir);
        app.flags = Arc::new(Flags::controlled_for_tests());
        app.open_room_settings(&Context::default());
        let draft = app.settings_draft_mut().unwrap();
        draft.talk = TalkMode::OpenMic;
        draft.hotkeys.set(Action::Panic, "F9".parse().unwrap());
        app.save_room_settings();
        assert!(app.settings_draft_mut().unwrap().locked);
        assert_eq!(
            in_room(&mut app).room.view().voice.mode,
            TalkMode::PushToTalk
        );
        assert!(!dir.join("settings.txt").exists());
        close_room(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    // Pressing Save again, as the line says, once the program has let go of
    // the file: the save goes through and what this run never read stays.
    #[test]
    fn save_after_a_failed_read_keeps_the_file_s_lines() {
        use std::fs;
        let dir = fresh_dir("failed-read");
        let path = dir.join("settings.txt");
        fs::create_dir(&path).unwrap();
        let mut setup = setup_in(&dir);
        (setup.settings, _, setup.gave_way) = Settings::load(&dir);
        fs::remove_dir(&path).unwrap();
        fs::write(&path, "name = Mara\r\nport = 41010\r\n").unwrap();
        let mut backlog = Backlog::default();
        let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
        draft.vsync = true;
        assert!(save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "name = Mara\r\nport = 41010\r\nvsync_in_viewer = on\r\n"
        );

        // A monitor picked in Share's list saves with no Save pressed.
        remember_monitor(&mut setup, r"\\.\DISPLAY2", false, &mut backlog);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "name = Mara\r\nport = 41010\r\nvsync_in_viewer = on\r\nshare_monitor = \\\\.\\DISPLAY2\r\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // Turned on, saved, then turned off and saved: the line goes, or the next
    // start would have what the panel says is off, the mic open or the update
    // check going. Share's list does the same with no Save pressed.
    #[test]
    fn set_back_after_a_failed_read_the_line_goes() {
        use std::fs;
        let dir = fresh_dir("failed-read-set-back");
        let path = dir.join("settings.txt");
        fs::create_dir(&path).unwrap();
        let mut setup = setup_in(&dir);
        (setup.settings, _, setup.gave_way) = Settings::load(&dir);
        fs::remove_dir(&path).unwrap();
        fs::write(&path, "name = Mara\r\n").unwrap();
        let mut backlog = Backlog::default();
        let mut draft = Draft::new(&setup.settings, KnownDevices::default(), String::new());
        draft.talk = TalkMode::OpenMic;
        draft.check_for_new_versions = true;
        assert!(save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "name = Mara\r\ntalk_mode = open_mic\r\ncheck_for_new_versions = on\r\n"
        );

        draft.talk = TalkMode::PushToTalk;
        draft.check_for_new_versions = false;
        assert!(save_draft(&mut setup, &mut draft, &mut backlog, false));
        assert_eq!(fs::read_to_string(&path).unwrap(), "name = Mara\r\n");

        remember_list_next(&mut setup, &mut backlog);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "name = Mara\r\nshare_key_opens_list = on\r\n"
        );
        remember_monitor(&mut setup, r"\\.\DISPLAY2", false, &mut backlog);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "name = Mara\r\nshare_monitor = \\\\.\\DISPLAY2\r\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // What load hands over, with no profile's lock: the line for a key that
    // gave way, and the one for a file that could not be read at all.
    #[test]
    fn the_start_screen_has_what_reading_the_file_found() {
        use std::fs;
        let args = Args {
            profile: None,
            port: None,
            log: false,
            loopback: None,
            video_loss: None,
        };
        let build = |dir: &Path| {
            let mut backlog = Backlog::default();
            Ok(setup_from(
                &args,
                dir.to_path_buf(),
                None,
                Identity::generate(),
                &mut backlog,
            ))
        };
        let dir = fresh_dir("handed-over");
        let path = dir.join("settings.txt");
        assert!(start_lines(None, &build(&dir), &[], &[]).is_empty());

        fs::write(&path, "hotkey_push_to_talk = F9\r\n").unwrap();
        assert_eq!(
            start_lines(None, &build(&dir), &[], &[]),
            [
                "Pausing a share is on Ctrl+Shift+F10 rather than Ctrl+Shift+F9, which clashed with F9 for push to talk. Change either in Settings."
            ]
        );

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(
            start_lines(Some("first"), &build(&dir), &[], &[]),
            ["first", messages::SETTINGS_UNREAD]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // Saved before pause sharing had Ctrl+Shift+F9: push to talk keeps F9, and
    // pausing moves to Ctrl+Shift+F10. The line stays for the run while both
    // keys are where reading the file left them, and goes once either has been
    // moved, as it offers.
    #[test]
    fn the_start_screen_says_which_key_gave_way() {
        use std::fs;
        let dir = fresh_dir("gave-way");
        fs::write(dir.join("settings.txt"), "hotkey_push_to_talk = F9\r\n").unwrap();
        let mut setup = setup_in(&dir);
        (setup.settings, _, setup.gave_way) = Settings::load(&dir);
        let read = setup.settings.hotkeys();
        let mut setup = Ok(setup);
        let line = "Pausing a share is on Ctrl+Shift+F10 rather than Ctrl+Shift+F9, which clashed with F9 for push to talk. Change either in Settings.";
        assert_eq!(start_lines(None, &setup, &[], &[]), [line]);
        assert_eq!(
            start_lines(
                Some("first"),
                &setup,
                &[],
                &[(List::Hosts, String::from("last"))]
            ),
            ["first", line, "last"]
        );

        let Ok(moved) = &mut setup else {
            panic!("the setup is there");
        };
        let mut hotkeys = read;
        hotkeys.set(input::Action::PushToTalk, "F8".parse().unwrap());
        moved.settings.set_hotkeys(hotkeys);
        assert!(start_lines(None, &setup, &[], &[]).is_empty());

        // Moving the other one ends the line as well.
        let Ok(moved) = &mut setup else {
            panic!("the setup is there");
        };
        let mut hotkeys = read;
        hotkeys.set(input::Action::PauseShare, "F8".parse().unwrap());
        moved.settings.set_hotkeys(hotkeys);
        assert!(start_lines(None, &setup, &[], &[]).is_empty());

        // Two moved onto one key both went back, and each line is said, with
        // nothing left clashing by then.
        fs::write(
            dir.join("settings.txt"),
            "hotkey_mute = F8\r\nhotkey_deafen = F8\r\n",
        )
        .unwrap();
        let Ok(both) = &mut setup else {
            panic!("the setup is there");
        };
        (both.settings, _, both.gave_way) = Settings::load(&dir);
        assert_eq!(
            start_lines(None, &setup, &[], &[]),
            [
                "Mute was on F8, the same key as deafen. It is back on Ctrl+Shift+M. Change either in Settings.",
                "Deafen was on F8, the same key as mute. It is back on Ctrl+Shift+D. Change either in Settings."
            ]
        );
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
        let (saved, _, _) = Settings::load(&dir);
        assert_eq!(saved.share_monitor(), Some(r"\\.\DISPLAY2"));
        let _ = fs::remove_dir_all(&dir);
    }

    // The list stays what the share key opens after a window or a region is
    // picked, in the next room and the next run, until a monitor is picked.
    #[test]
    fn a_window_or_region_stays_the_choice_between_rooms() {
        let dir = fresh_dir("list-next");
        let ctx = Context::default();
        let mut app = app_in_room(&dir);
        app.take_step(Some(Step::ListNext), &Context::default());
        assert!(Settings::load(&dir).0.share_key_opens_list());

        app.leave(&ctx);
        app.enter(room_tests::test_room(&dir));
        press_share_key_twice(&mut app, &ctx);
        thread::sleep(Duration::from_millis(100));
        let own = in_room(&mut app).room.view().share.own;
        assert!(!own_share_on(&own), "{own:?}");
        let listed = monitors::list().is_ok_and(|list| !list.is_empty());
        assert_eq!(app.raise, listed, "the panel comes forward with the list");
        assert_eq!(room_tests::list_open(in_room(&mut app)), listed);

        // A monitor picked ends it, the same monitor as before included.
        app.take_step(
            Some(Step::Remember(String::from(r"\\.\DISPLAY2"))),
            &Context::default(),
        );
        assert!(!Settings::load(&dir).0.share_key_opens_list());
        app.take_step(Some(Step::ListNext), &Context::default());
        app.take_step(
            Some(Step::Remember(String::from(r"\\.\DISPLAY2"))),
            &Context::default(),
        );
        assert!(!Settings::load(&dir).0.share_key_opens_list());
        press_share_key_twice(&mut app, &ctx);
        share_goes(&mut app, true);

        close_room(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // While controlled a monitor picked changes nothing, so the key does not
    // go back to a whole monitor. A window or a region is kept, as it only
    // makes the key show less.
    #[test]
    fn while_controlled_the_share_key_does_not_widen() {
        let dir = fresh_dir("list-controlled");
        let mut setup = setup_in(&dir);
        let mut backlog = Backlog::default();
        remember_list_next(&mut setup, &mut backlog);
        assert!(setup.settings.share_key_opens_list());
        let saved = std::fs::read(dir.join("settings.txt")).unwrap();
        remember_monitor(&mut setup, r"\\.\DISPLAY2", true, &mut backlog);
        assert!(setup.settings.share_key_opens_list());
        assert_eq!(setup.settings.share_monitor(), None);
        assert_eq!(std::fs::read(dir.join("settings.txt")).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A file left without the flag gives a whole monitor after a restart, so
    // a write that failed is tried again at the next pick. The log hears of a
    // pick only when it changed something or could not be kept.
    #[test]
    fn the_next_pick_writes_a_list_next_that_could_not_be_written() {
        let dir = fresh_dir("list-write");
        let not_a_folder = dir.join("a file");
        std::fs::write(&not_a_folder, b"").unwrap();
        let mut setup = setup_in(&not_a_folder);
        let mut backlog = Backlog::default();
        remember_list_next(&mut setup, &mut backlog);
        assert!(setup.settings.share_key_opens_list());
        assert_eq!(backlog.texts().len(), 1);
        assert!(backlog.texts()[0].ends_with("until Booth closes"));

        setup.config.data_dir = dir.clone();
        remember_list_next(&mut setup, &mut backlog);
        assert!(Settings::load(&dir).0.share_key_opens_list());
        assert_eq!(backlog.texts().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // With Share not on offer the list would not stay, so the panel does not
    // come forward. While a friend shares, the room gives its one line.
    #[test]
    fn the_share_key_shows_no_list_while_a_friend_shares() {
        let dir = fresh_dir("list-busy");
        let friend_dir = fresh_dir("list-busy-friend");
        let ctx = Context::default();
        let mut app = app_in_room(&dir);
        app.take_step(Some(Step::ListNext), &Context::default());
        let friend = room_tests::test_friend(&in_room(&mut app).room, &friend_dir);
        friend.share(30, None);
        let room = Arc::clone(&in_room(&mut app).room);
        let deadline = Instant::now() + Duration::from_secs(5);
        while room.view().share.current.is_none() {
            assert!(Instant::now() < deadline, "the friend's share never showed");
            thread::sleep(Duration::from_millis(5));
        }

        press_share_key_twice(&mut app, &ctx);
        let view = room.view();
        assert!(!app.raise);
        assert!(!room_tests::list_open(in_room(&mut app)));
        assert!(!own_share_on(&view.share.own), "{:?}", view.share.own);
        let line = "Bo is sharing. One share at a time.";
        assert!(view.chat.iter().any(|said| said.text == line));

        friend.leave();
        drop(room);
        close_room(app);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&friend_dir);
    }

    // No list to show: the panel stays where it is, a line on it says so for
    // when the person looks, and the log says why.
    #[test]
    fn the_share_key_brings_the_panel_only_for_a_list() {
        let dir = fresh_dir("list-none");
        let ctx = Context::default();
        let mut app = app_in_room(&dir);
        let shown = |app: &mut App| room_tests::shows_line(in_room(app), messages::LIST_FAILED);
        assert!(!shown(&mut app));

        app.open_list_by_key(Err(String::from("no list")), &ctx);
        assert!(app.backlog.texts().contains(&"share key: no list"));
        assert!(shown(&mut app));
        app.open_list_by_key(Ok(Vec::new()), &ctx);
        assert!(
            app.backlog
                .texts()
                .contains(&"share key: no monitor is attached")
        );
        assert!(!app.raise);
        assert!(!room_tests::list_open(in_room(&mut app)));

        let monitor = monitors::tests::monitor(r"\\.\DISPLAY1", 0, true);
        app.open_list_by_key(Ok(vec![monitor]), &ctx);
        assert!(app.raise);
        assert!(room_tests::list_open(in_room(&mut app)));
        assert!(!shown(&mut app));

        // An empty list says so the same way.
        in_room(&mut app).close_monitors();
        app.open_list_by_key(Ok(Vec::new()), &ctx);
        assert!(shown(&mut app));

        close_room(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // While the picker is up the key starts nothing and opens nothing, the
    // list after a window or a region included. One press still stops a share.
    #[test]
    fn the_share_key_starts_nothing_while_the_picker_is_up() {
        let dir = fresh_dir("key-picking");
        let ctx = Context::default();
        for list_next in [false, true] {
            let mut app = app_in_room(&dir);
            if list_next {
                app.take_step(Some(Step::ListNext), &Context::default());
            }
            let answer = room_tests::picker_open(in_room(&mut app));
            press_share_key_twice(&mut app, &ctx);
            thread::sleep(Duration::from_millis(100));
            let own = in_room(&mut app).room.view().share.own;
            assert!(!own_share_on(&own), "{list_next}: {own:?}");
            assert!(!room_tests::list_open(in_room(&mut app)), "{list_next}");
            assert!(!app.raise, "{list_next}");
            drop(answer);
            close_room(app);
        }
        let mut app = app_in_room(&dir);
        in_room(&mut app).room.share(30, None);
        share_goes(&mut app, true);
        let answer = room_tests::picker_open(in_room(&mut app));
        app.share_key(Instant::now(), &ctx);
        share_goes(&mut app, false);
        drop(answer);
        close_room(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The mouse let go with the panel minimized starts the share there, with
    // no frame run, and the share key's count follows it.
    #[test]
    fn a_region_picked_while_minimized_shares_at_once() {
        let dir = fresh_dir("region-minimized");
        let ctx = Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = app_in_room(&dir);
        let answer = room_tests::picker_open(in_room(&mut app));

        let at = Instant::now();
        app.share_key(at, &ctx);
        let _ = ctx.run_logic(&with_window(true), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        assert!(in_room(&mut app).picking());
        let picked = crate::region::Picked {
            monitor: room::MonitorId {
                device_name: String::from(r"\\.\DISPLAY1"),
                adapter_luid: 0,
            },
            refresh_hz: 60.0,
            region: share::Region {
                left: 0,
                top: 0,
                width: 256,
                height: 256,
            },
            drawn_on: (1920, 1080),
        };
        answer.send(Ok(Some(picked))).unwrap();
        let _ = ctx.run_logic(&with_window(true), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        assert!(!in_room(&mut app).picking());
        share_goes(&mut app, true);
        assert!(Settings::load(&dir).0.share_key_opens_list());
        let _ = ctx.run_logic(&with_window(true), |ctx| {
            eframe::App::logic(&mut app, ctx, &mut frame);
        });
        // A share seen running ended the first press, which can no longer
        // start anything.
        assert_eq!(app.share_key.press(at, false), None);

        close_room(app);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn saved_sound_cues_reach_the_next_room() {
        let dir = fresh_dir("sound-cues-next-room");
        let mut setup = setup_in(&dir);
        let wanted = room::SoundCues {
            volume: 60,
            own_mute: false,
            ..room::SoundCues::default()
        };
        setup.settings.set_sound_cues(wanted);
        setup.apply_settings();
        assert_eq!(setup.config.voice.cues, wanted);
        let _ = std::fs::remove_dir_all(dir);
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
