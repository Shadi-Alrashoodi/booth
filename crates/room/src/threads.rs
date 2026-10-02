// The two threads a room runs: one blocked on the socket, one blocked on the
// command channel until the next deadline. Both work under one lock on the
// room's state and call notify only after letting go of it. Others never
// take the lock and hand what they find to the timer thread over the command
// channel: one looks up the STUN server names, on both sides, one asks the
// host's router for a port (mapper.rs), one looks up the address name when a
// side asks for it (names.rs), and Windows' own worker thread says when an
// address on this PC changed (AddressHook). The known list goes the other
// way: the two threads hand a copy to the saver thread, which writes it
// (saver.rs). Remote control adds one more that never takes the lock, the
// controller's send thread (remote/send.rs), and the app's injector, which
// the receive and timer threads call with what was decided under the lock
// once they let go of it (remote.rs, Gate). A panel call that decided some
// wakes the timer thread for them instead of making them itself.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use invite::ReplyCode;
use net::dns::DnsError;
use net::pace::{self, Signal, Timer, Woken};
use net::watch::AddressWatch;

use crate::chat::{self, ChatRefused};
use crate::client::Client;
use crate::config::{VideoConfig, VideoSource};
use crate::host::{Host, Listing};
use crate::known::{Save, Turn};
use crate::log::{Log, Tally, Writer, list, log};
use crate::mapper::{Mapper, Report, Target};
use crate::names::{self, Outcome, Request};
use crate::peer::Clock;
use crate::remote::{self, Controls, Gate};
use crate::reply::{ReplyAccepted, ReplyRefused};
use crate::saver::{SaveHandle, Saver};
use crate::screen::{self, Sharing, Watching};
use crate::socket::Socket;
use crate::talk::{self, Timing, VoiceConfig};
use crate::view::View;
use crate::{Notify, Soonest};

// A full Ethernet payload, well above net::MIN_RECV_BUFFER.
const RECEIVE_BUFFER: usize = 2048;
// How long leave() waits for the router to confirm the port mapping is
// gone. The mapper thread finishes a slower deletion after leave returns.
const DELETE_WAIT: Duration = Duration::from_millis(150);
// How long leave() waits for the microphone and the speakers to close. A
// Bluetooth headset can take far longer, over 30 s once, and closes on its
// own thread after leave returns, holding only the device, which a next
// room on the same one waits for (talk/devices.rs).
const DEVICES_WAIT: Duration = Duration::from_millis(100);
// More servers make no answer surer. Each name is looked up before the
// first STUN round can settle, and every address it has is asked each
// round, so a list pasted from the web would slow both.
const MAX_STUN_SERVERS: usize = 4;

pub(crate) enum Side {
    Host(Box<Host>),
    Client(Box<Client>),
}

impl Side {
    fn on_packet(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        match self {
            Side::Host(host) => host.on_packet(packet, from, now, socket),
            Side::Client(client) => client.on_packet(packet, from, now, socket),
        }
    }

    fn on_timer(&mut self, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.on_timer(now, socket),
            Side::Client(client) => client.on_timer(now, socket),
        }
    }

    fn new_invite(&mut self, multi_use: bool, now: Instant) -> bool {
        match self {
            Side::Host(host) => host.new_invite(multi_use, now),
            Side::Client(_) => false,
        }
    }

    fn new_code(&mut self, now: Instant) -> bool {
        match self {
            Side::Host(_) => false,
            Side::Client(client) => client.new_code(now),
        }
    }

    fn accept_reply(
        &mut self,
        code: &ReplyCode,
        now: Instant,
    ) -> Result<ReplyAccepted, ReplyRefused> {
        match self {
            Side::Host(host) => host.accept_reply(code, now),
            Side::Client(_) => Err(ReplyRefused::Closed),
        }
    }

    fn say(&mut self, text: String, now: Instant, socket: &Socket) -> Result<(), ChatRefused> {
        match self {
            Side::Host(host) => host.say(text, now, socket),
            Side::Client(client) => client.say(text, now, socket),
        }
    }

    fn stun_found(&mut self, servers: Vec<SocketAddr>, socket: &Socket) {
        match self {
            Side::Host(host) => host.stun_found(servers, socket),
            Side::Client(client) => client.stun_found(servers, socket),
        }
    }

    fn stun_resolved(&mut self, now: Instant) -> bool {
        match self {
            Side::Host(host) => host.stun_resolved(now),
            Side::Client(client) => client.stun_resolved(now),
        }
    }

    fn port_mapping(&mut self, report: Report, now: Instant) -> bool {
        match self {
            Side::Host(host) => host.port_mapping(report, now),
            Side::Client(_) => false,
        }
    }

    fn name_wanted(&mut self) -> Option<Request> {
        match self {
            Side::Host(host) => host.name_wanted(),
            Side::Client(client) => client.name_wanted(),
        }
    }

    fn name_found(&mut self, outcome: Outcome, now: Instant) -> bool {
        match self {
            Side::Host(host) => host.name_found(outcome),
            Side::Client(client) => client.name_found(outcome, now),
        }
    }

    // `listing` is this PC's address list read again, on a host that lists
    // its own.
    fn address_changed(
        &mut self,
        now: Instant,
        socket: &Socket,
        listing: Option<io::Result<Listing>>,
    ) -> bool {
        match self {
            Side::Host(host) => host.address_changed(now, socket, listing),
            Side::Client(client) => client.address_changed(now, socket),
        }
    }

    fn socket_failed(&mut self) {
        match self {
            Side::Host(host) => host.socket_failed(),
            Side::Client(client) => client.socket_failed(),
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        match self {
            Side::Host(host) => host.next_deadline(),
            Side::Client(client) => client.next_deadline(),
        }
    }

    fn view(&self, now: Instant, socket: &Socket) -> View {
        let oversized = socket.oversized_drops();
        match self {
            Side::Host(host) => host.view(now, oversized),
            Side::Client(client) => client.view(now, oversized),
        }
    }

    fn leave(&mut self, now: Instant, socket: &Socket) {
        match self {
            Side::Host(host) => host.leave(now, socket),
            Side::Client(client) => client.leave(now, socket),
        }
    }

    fn take_save(&mut self, now: Instant) -> Option<Save> {
        match self {
            Side::Host(host) => host.take_save(now),
            Side::Client(client) => client.take_save(now),
        }
    }

    fn take_last_save(&mut self) -> Option<Save> {
        match self {
            Side::Host(host) => host.take_last_save(),
            Side::Client(client) => client.take_last_save(),
        }
    }

    fn save_due(&self) -> Option<Instant> {
        match self {
            Side::Host(host) => host.save_due(),
            Side::Client(client) => client.save_due(),
        }
    }

    fn publish_voice(&mut self) {
        match self {
            Side::Host(host) => host.publish_voice(),
            Side::Client(client) => client.publish_voice(),
        }
    }

    fn publish_share(&mut self) {
        match self {
            Side::Host(host) => host.publish_share(),
            Side::Client(client) => client.publish_share(),
        }
    }

    fn share(&mut self, fps: u8, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.share(fps, now, socket),
            Side::Client(client) => client.share(fps, now, socket),
        }
    }

    fn stop_sharing(&mut self, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.stop_sharing(now, socket),
            Side::Client(client) => client.stop_sharing(now, socket),
        }
    }

    fn watch(&mut self, share: u32, on: bool, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.watch(share, on, now, socket),
            Side::Client(client) => client.watch(share, on, now, socket),
        }
    }

    fn screen_work(&mut self, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.screen_work(now, socket),
            Side::Client(client) => client.screen_work(now, socket),
        }
    }

    fn screen_hooks(&self) -> (Arc<Sharing>, Arc<Watching>) {
        match self {
            Side::Host(host) => host.screen_hooks(),
            Side::Client(client) => client.screen_hooks(),
        }
    }

    fn remote_hooks(&self) -> (Arc<Gate>, Arc<Controls>) {
        let remote = match self {
            Side::Host(host) => host.remote(),
            Side::Client(client) => client.remote(),
        };
        (Arc::clone(&remote.gate), Arc::clone(&remote.controls))
    }

    fn ask_control(&mut self, share: u32, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.ask_control(share, now, socket),
            Side::Client(client) => client.ask_control(share, now, socket),
        }
    }

    fn answer_control(&mut self, number: u32, allow: bool, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.answer_control(number, allow, now, socket),
            Side::Client(client) => client.answer_control(number, allow, now, socket),
        }
    }

    fn stop_control(&mut self, panic: bool, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.stop_control(panic, now, socket),
            Side::Client(client) => client.stop_control(panic, now, socket),
        }
    }

    fn end_control(&mut self, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.end_control(now, socket),
            Side::Client(_) => false,
        }
    }

    fn control_work(&mut self, now: Instant, socket: &Socket) -> bool {
        match self {
            Side::Host(host) => host.control_work(now, socket),
            Side::Client(client) => client.control_work(now, socket),
        }
    }

    fn publish_control(&mut self) {
        match self {
            Side::Host(host) => host.publish_control(),
            Side::Client(client) => client.publish_control(),
        }
    }
}

enum Command {
    NewInvite(bool),
    NewCode,
    Mapping(Report),
    // One STUN server name resolved.
    StunFound(Vec<SocketAddr>),
    // All of them have.
    StunResolved,
    // The address name was looked up.
    Name(Outcome),
    // Windows says an address on this PC changed.
    AddressChanged,
    // The address list, read again on a thread of its own.
    Relisted(io::Result<Listing>),
    // The receive thread moved a deadline earlier than the one being waited
    // for, or a panel call left calls for the injector (remote.rs, Gate).
    Wake,
    // Something the view shows about voice changed on an audio thread: this
    // PC started or stopped sending, or a device opened or failed.
    Voice,
    // A sharer's thread left a shape to send or sent the first video after
    // a pause, or a viewer's thread left something for the sharer.
    Screen,
    // The injector found an administrator window came to the front or went.
    Control,
    Stop,
}

// The timer thread waits on an event and the high-resolution timer together,
// not on the channel (see Waiting), so every command also sets the event.
#[derive(Clone)]
struct Commands {
    sender: Sender<Command>,
    wake: Arc<Signal>,
}

impl Commands {
    // A timer thread that has ended takes nothing more, and nothing is lost
    // by that.
    fn send(&self, command: Command) {
        if self.sender.send(command).is_ok() {
            self.wake.set();
        }
    }
}

struct State {
    side: Side,
    // What the timer thread is waiting for, so the receive thread knows when
    // a new deadline needs it woken.
    planned: Option<Instant>,
}

struct Shared {
    state: Mutex<State>,
    view: Mutex<View>,
    stop: AtomicBool,
    socket: Arc<Socket>,
    notify: Notify,
    commands: Commands,
    saves: SaveHandle,
    // An AddressChanged is on its way to the timer thread.
    address_pending: Arc<AtomicBool>,
    // A host that lists its own addresses reads them again on an address
    // change, on a thread of its own: while adapters come and go Windows
    // can take tens of milliseconds, and the timer thread sends the pings.
    relist: bool,
    log: Log,
    // How late the timer thread woke for its deadlines.
    late: Timing,
    // The viewer's strip shows the view's link numbers.
    watching: Arc<Watching>,
    // The receive and timer threads make the injector calls decided under
    // the state lock once they let go of it.
    gate: Arc<Gate>,
}

// Hands a Windows address change notification to the room. It only posts a
// command, so any thread may call it, Windows' own worker thread included,
// and a burst of calls while one is on its way posts nothing more.
#[derive(Clone)]
pub struct AddressHook {
    commands: Commands,
    pending: Arc<AtomicBool>,
}

impl AddressHook {
    pub fn changed(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.commands.send(Command::AddressChanged);
        }
    }
}

// Called with the state lock held, so a view built earlier on one thread can
// never replace a view built later on the other. The lock order is state,
// then view, everywhere; Room::view takes only the view lock.
fn store_view(shared: &Shared, state: &State, now: Instant) {
    let mut view = state.side.view(now, &shared.socket);
    view.numbers.timer_late = shared.late.spread();
    shared.watching.link(now, || screen::link_numbers(&view));
    shared.watching.control(screen::control_for(&view));
    *lock(&shared.view) = view;
}

// After each step the side takes, with the state lock held. A changed known
// list fills the saver's one slot, and the writing happens there; a changed
// session or address reaches the capture thread.
fn after_step(shared: &Shared, state: &mut State, now: Instant) {
    if let Some(save) = state.side.take_save(now) {
        shared.saves.save(save);
    }
    state.side.publish_voice();
    state.side.publish_share();
    state.side.publish_control();
}

// The side's own next deadline, or a change to the known list held back
// (known::SAVE_GAP), even on a side that has closed and waits for nothing
// else.
fn next_deadline(state: &State) -> Option<Instant> {
    let mut soonest = Soonest(state.side.next_deadline());
    soonest.add(state.side.save_due());
    soonest.0
}

pub(crate) struct Threads {
    shared: Arc<Shared>,
    voice: Arc<talk::Shared>,
    sharing: Arc<Sharing>,
    watching: Arc<Watching>,
    controls: Arc<Controls>,
    // The controller's send thread (remote/send.rs).
    control_send: Option<JoinHandle<()>>,
    streams: Option<talk::Streams>,
    // The share's and the viewer's threads, when the room runs them.
    video: Vec<JoinHandle<()>>,
    receive: Option<JoinHandle<()>>,
    timer: Option<JoinHandle<()>>,
    mapper: Option<Mapper>,
    watch: Option<AddressWatch>,
    saver: Saver,
    writer: Option<Writer>,
}

// Where the room's threads send what they find, and the known list the room
// writes back.
pub(crate) struct Outlets {
    pub notify: Notify,
    pub log: Log,
    pub writer: Option<Writer>,
    pub turn: Turn,
}

// The room's voice: its settings, what its audio threads share, the render
// thread's mixer, and the clock voice is stamped with.
pub(crate) struct VoiceStart {
    pub config: VoiceConfig,
    pub shared: Arc<talk::Shared>,
    pub speaker: talk::Speaker,
    pub clock: Clock,
}

impl Threads {
    // `router` is where a host asks for its port to be mapped, if anywhere.
    pub(crate) fn start(
        mut side: Side,
        socket: Arc<Socket>,
        stun_servers: Vec<String>,
        router: Option<Target>,
        outlets: Outlets,
        voice: VoiceStart,
        video: VideoConfig,
    ) -> io::Result<Threads> {
        let Outlets {
            notify,
            log,
            writer,
            turn,
        } = outlets;
        let view = side.view(Instant::now(), &socket);
        let port = socket.local_port();
        let saver = Saver::start(turn, log.clone())?;
        let stun_servers = first_stun_servers(stun_servers, &log);
        let (sender, inbox) = crossbeam_channel::unbounded();
        let commands = Commands {
            sender,
            wake: Arc::new(Signal::new()?),
        };
        let timer = Timer::new()?;
        if let Some(note) = timer.note() {
            log!(log, "room timers: {note}");
        }
        let mapper = match router {
            Some(target) => {
                let commands = commands.clone();
                let report = move |report| {
                    commands.send(Command::Mapping(report));
                };
                Some(Mapper::start(target, port, log.clone(), report)?)
            }
            None => None,
        };
        if let (Side::Host(host), Some(mapper)) = (&mut side, &mapper) {
            host.renew_mapping_with(mapper.renewer());
            host.remap_with(mapper.remapper());
        }
        let relist = matches!(&side, Side::Host(host) if host.lists_addresses());
        let (sharing, watching) = side.screen_hooks();
        let (gate, controls) = side.remote_hooks();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                side,
                planned: None,
            }),
            view: Mutex::new(view),
            stop: AtomicBool::new(false),
            socket,
            notify,
            commands,
            saves: saver.handle(),
            address_pending: Arc::new(AtomicBool::new(false)),
            relist,
            log,
            late: Timing::default(),
            watching: Arc::clone(&watching),
            gate: Arc::clone(&gate),
        });
        let receive = thread::Builder::new().name("room receive".into()).spawn({
            let shared = Arc::clone(&shared);
            move || receive(&shared)
        })?;
        let commands = shared.commands.clone();
        voice.shared.on_change(move || {
            commands.send(Command::Voice);
        });
        let commands = shared.commands.clone();
        sharing.on_work(move || commands.send(Command::Screen));
        let (speakers, clock) = (Arc::clone(&voice.shared), voice.clock);
        sharing.on_cue(move |cue| speakers.cue(cue, clock.micros(Instant::now())));
        let commands = shared.commands.clone();
        watching.on_work(move || commands.send(Command::Screen));
        let commands = shared.commands.clone();
        gate.on_work(move || commands.send(Command::Control));
        let speakers = Arc::clone(&voice.shared);
        gate.on_cue(move |cue| speakers.control_cue(cue, clock.micros(Instant::now())));
        let control_send = remote::start_sender(Arc::clone(&controls), shared.log.clone())?;
        let streams = talk::Streams::start(
            voice.config,
            Arc::clone(&voice.shared),
            voice.speaker,
            voice.clock,
            shared.log.clone(),
        )?;
        let mut threads = Threads {
            shared: Arc::clone(&shared),
            voice: voice.shared,
            sharing,
            watching,
            controls,
            control_send: Some(control_send),
            streams: Some(streams),
            video: Vec::new(),
            receive: Some(receive),
            timer: None,
            mapper,
            watch: None,
            saver,
            writer,
        };
        let timer = thread::Builder::new().name("room timers".into()).spawn({
            let shared = Arc::clone(&shared);
            move || timers(&shared, &inbox, timer)
        })?;
        threads.timer = Some(timer);
        if video.source != VideoSource::Hooks {
            let log = shared.log.clone();
            let sharer = screen::start_sharer(
                Arc::clone(&threads.sharing),
                video.source.clone(),
                log.clone(),
            )?;
            threads.video.push(sharer);
            let viewer = screen::start_viewer(
                Arc::clone(&threads.watching),
                video,
                Arc::clone(&threads.controls),
                log,
            )?;
            threads.video.push(viewer);
        }
        if stun_servers.is_empty() {
            shared.commands.send(Command::StunResolved);
        } else {
            let weak = Arc::downgrade(&shared);
            thread::Builder::new()
                .name("room stun lookup".into())
                .spawn(move || look_up_stun(&weak, &stun_servers))?;
        }
        Ok(threads)
    }

    pub(crate) fn view(&self) -> View {
        lock(&self.shared.view).clone()
    }

    pub(crate) fn share(&self, fps: u8, monitor: Option<share::MonitorId>) {
        if self.sharing.number().is_none() {
            self.sharing.set_monitor(monitor);
        }
        self.with_side(|side, now, socket| side.share(fps, now, socket));
    }

    pub(crate) fn stop_sharing(&self) {
        self.with_side(|side, now, socket| side.stop_sharing(now, socket));
    }

    pub(crate) fn watch(&self, share: u32, on: bool) {
        self.with_side(|side, now, socket| side.watch(share, on, now, socket));
    }

    pub(crate) fn sharing(&self) -> Arc<Sharing> {
        Arc::clone(&self.sharing)
    }

    pub(crate) fn ask_control(&self, share: u32) {
        self.with_side(|side, now, socket| side.ask_control(share, now, socket));
    }

    pub(crate) fn answer_control(&self, number: u32, allow: bool) {
        self.with_side(|side, now, socket| side.answer_control(number, allow, now, socket));
    }

    pub(crate) fn stop_control(&self, panic: bool) {
        self.with_side(|side, now, socket| side.stop_control(panic, now, socket));
    }

    pub(crate) fn end_control(&self) {
        self.with_side(|side, now, socket| side.end_control(now, socket));
    }

    pub(crate) fn controls(&self) -> Arc<Controls> {
        Arc::clone(&self.controls)
    }

    pub(crate) fn watching(&self) -> Arc<Watching> {
        Arc::clone(&self.watching)
    }

    pub(crate) fn new_invite(&self, multi_use: bool) {
        self.shared.commands.send(Command::NewInvite(multi_use));
    }

    pub(crate) fn new_code(&self) {
        self.shared.commands.send(Command::NewCode);
    }

    pub(crate) fn address_hook(&self) -> AddressHook {
        AddressHook {
            commands: self.shared.commands.clone(),
            pending: Arc::clone(&self.shared.address_pending),
        }
    }

    // A room without it still notices a new outside address, at the next
    // STUN round or when every friend goes quiet at once, so a watch Windows
    // refuses is logged and nothing more.
    pub(crate) fn watch_addresses(&mut self) {
        let hook = self.address_hook();
        match net::watch::start(move || hook.changed()) {
            Ok(watch) => {
                self.watch = Some(watch);
                log!(
                    self.shared.log,
                    "watching this pc's addresses, a change asks stun at once"
                );
            }
            Err(err) => log!(
                self.shared.log,
                "{err}; a new outside address is noticed at the next stun round instead"
            ),
        }
    }

    // Answered at once, so the panel can say why a paste was refused. The
    // lock is held only for the checks; the punches go out on the timer
    // thread, which is woken for the first round.
    pub(crate) fn accept_reply(&self, code: &ReplyCode) -> Result<ReplyAccepted, ReplyRefused> {
        self.with_side(|side, now, _| side.accept_reply(code, now))
    }

    // The panel's Hold to talk. Read by the capture thread at its next frame.
    pub(crate) fn talk(&self, held: bool) {
        self.voice.set_held(held);
    }

    pub(crate) fn mute(&self, muted: bool) {
        let microphone = self.voice.set_muted(muted);
        let what = if muted { "muted" } else { "unmuted" };
        self.voice_changed(what, microphone);
    }

    // Deafened, this PC also stops sending; see talk::Shared::set_muted.
    pub(crate) fn deafen(&self, deafened: bool) {
        let microphone = self.voice.set_deafened(deafened);
        let what = if deafened { "deafened" } else { "undeafened" };
        self.voice_changed(what, microphone);
    }

    fn voice_changed(&self, what: &str, microphone: bool) {
        let then = if microphone {
            "the microphone opens"
        } else {
            "the microphone closes"
        };
        log!(self.shared.log, "voice: {what}, {then}");
        if let Some(streams) = &self.streams {
            streams.follow();
        }
        self.shared.commands.send(Command::Voice);
    }

    // The text is cleaned on the panel's thread, before the lock. What is
    // left goes out from here, and the next view has it.
    pub(crate) fn say(&self, text: &str) -> Result<(), ChatRefused> {
        let text = chat::clean_text(text)?;
        self.with_side(|side, now, socket| side.say(text, now, socket))
    }

    // A call from the panel's thread: the side does its part under the lock,
    // the view follows, and the timer thread is woken when that moved the
    // next deadline earlier, as a retransmit timer does, or left calls for
    // the injector. This thread is at normal priority and never makes them
    // itself: while it did, the receive thread would leave its input to it.
    // The panic key does not wait for that, since the app cuts the injector
    // itself first.
    fn with_side<R>(&self, act: impl FnOnce(&mut Side, Instant, &Socket) -> R) -> R {
        let now = Instant::now();
        let (result, wake) = {
            let mut state = lock(&self.shared.state);
            let result = act(&mut state.side, now, &self.shared.socket);
            after_step(&self.shared, &mut state, now);
            let next = next_deadline(&state);
            let wake = next.is_some_and(|at| state.planned.is_none_or(|planned| at < planned));
            if wake {
                state.planned = next;
            }
            store_view(&self.shared, &state, now);
            (result, wake)
        };
        if wake || self.shared.gate.pending() {
            self.shared.commands.send(Command::Wake);
        }
        (self.shared.notify)();
        result
    }

    pub(crate) fn stop(&mut self) {
        if self.receive.is_none() && self.timer.is_none() {
            return;
        }
        let started = Instant::now();
        // First, so no voice goes out while the room closes, and so the
        // socket closes with the room even while a device is slow to.
        self.voice.let_go();
        if let Some(mut streams) = self.streams.take()
            && !streams.stop(DEVICES_WAIT)
        {
            log!(
                self.shared.log,
                "voice: leave waits no longer than {} ms for the microphone and speakers, they close on their own",
                DEVICES_WAIT.as_millis()
            );
        }
        // The share and the viewer too: nothing is captured or shown once
        // the room starts to close. Each ends within a frame, or 100 ms on
        // a still screen.
        self.sharing.close();
        self.watching.close();
        for thread in self.video.drain(..) {
            let _ = thread.join();
        }
        // First, so the router is asked while everything else shuts down.
        if let Some(mapper) = &self.mapper {
            mapper.close();
        }
        // Windows waits for a notification it is handing over, and all that
        // does is post a command, so this is quick. Nothing more comes in
        // while the room shuts down.
        drop(self.watch.take());
        {
            let mut state = lock(&self.shared.state);
            state.side.leave(Instant::now(), &self.shared.socket);
            state.side.publish_voice();
            state.side.publish_share();
            state.side.publish_control();
        }
        // Leaving ended any control of this PC: the injector lets go of what
        // it holds before the room is gone, on the timer thread now, or on
        // this one below once the room's threads have stopped.
        self.shared.commands.send(Command::Wake);
        self.controls.close();
        if let Some(thread) = self.control_send.take() {
            let _ = thread.join();
        }
        self.shared.stop.store(true, Ordering::Release);
        let woke = self.shared.socket.wake().is_ok();
        self.shared.commands.send(Command::Stop);
        if let Some(timer) = self.timer.take() {
            let _ = timer.join();
        }
        // Without the wake it stays in recv_from until some packet arrives.
        // It exits then, and leave() must not wait for that.
        if let Some(receive) = self.receive.take()
            && woke
        {
            let _ = receive.join();
        }
        self.shared.gate.run();
        // Leaving marks everyone in the room as seen now, and the last list
        // goes out whenever the one before it did.
        if let Some(save) = lock(&self.shared.state).side.take_last_save() {
            self.shared.saves.save(save);
        }
        self.saver.finish();
        let mapper_done = self
            .mapper
            .as_mut()
            .is_none_or(|mapper| mapper.wait(started + DELETE_WAIT));
        if !mapper_done {
            log!(
                self.shared.log,
                "port mapping: leave waits no longer than {} ms, the mapping thread finishes on its own",
                DELETE_WAIT.as_millis()
            );
        }
        if let Some(mut writer) = self.writer.take() {
            match &self.mapper {
                Some(mapper) if !mapper_done => mapper.hand_over(writer),
                _ => writer.stop(),
            }
        }
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.stop();
    }
}

fn receive(shared: &Shared) {
    // A remote controller's input is injected on this thread (remote.rs).
    // At normal priority, with a game keeping every CPU busy, it would wait
    // for a time slice after each packet arrives: the timer thread did, 6 to
    // 24 ms at the 99th percentile (tests/timer.rs), against the 5 ms the
    // host side of control has in all. It blocks on the socket between
    // packets and never spins.
    if let Err(err) = pace::raise_priority() {
        log!(
            shared.log,
            "room receive: {err}; a controller's input can wait several milliseconds while every cpu is busy"
        );
    }
    let mut buf = vec![0u8; RECEIVE_BUFFER];
    // net drops these inside recv_from, where the sender is not known, so
    // there is no source to hold to a few lines a minute. Anyone can send
    // them, and a line per drop would push the rest of the log out.
    let mut oversized = Tally::default();
    loop {
        let got = shared.socket.recv_from(&mut buf);
        if shared.stop.load(Ordering::Acquire) {
            if shared.log.is_on()
                && let Some(grown) = oversized.rest(shared.socket.oversized_drops())
            {
                log!(
                    shared.log,
                    "{grown} datagrams too big for the {RECEIVE_BUFFER} byte receive buffer dropped since the last line about them"
                );
            }
            return;
        }
        let now = Instant::now();
        if shared.log.is_on() {
            let total = shared.socket.oversized_drops();
            if let Some(grown) = oversized.due(total, now) {
                log!(
                    shared.log,
                    "{grown} datagrams too big for the {RECEIVE_BUFFER} byte receive buffer dropped, {total} so far; written at most once a minute"
                );
            }
        }
        // net already retries the errors a UDP socket gets from the network.
        // What is left means this PC's socket stopped working, and a room
        // that says so beats one that goes on pinging peers it cannot hear.
        let (len, from) = match got {
            Ok(got) => got,
            Err(err) => {
                log!(
                    shared.log,
                    "the socket stopped receiving: {err}; the room has stopped, leave, then host or join again"
                );
                {
                    let mut state = lock(&shared.state);
                    state.side.socket_failed();
                    store_view(shared, &state, now);
                }
                shared.gate.run();
                (shared.notify)();
                return;
            }
        };
        let Some(packet) = buf.get(..len) else {
            continue;
        };
        let (changed, wake) = {
            let mut state = lock(&shared.state);
            let changed = state.side.on_packet(packet, from, now, &shared.socket);
            after_step(shared, &mut state, now);
            let next = next_deadline(&state);
            let wake = next.is_some_and(|at| state.planned.is_none_or(|planned| at < planned));
            if wake {
                state.planned = next;
            }
            if changed {
                store_view(shared, &state, now);
            }
            (changed, wake)
        };
        // Input for this PC's injector goes now, on this thread: no other
        // packet or timer pass waits behind SendInput under the lock, and no
        // thread hop is added.
        shared.gate.run();
        if wake {
            shared.commands.send(Command::Wake);
        }
        if changed {
            (shared.notify)();
        }
    }
}

// How the timer thread waits for its next command or deadline. Waiting on
// the channel with a timeout would run on the default 15.6 ms Windows timer
// and wake 9 ms late at the median; the high-resolution timer wakes within
// about half a millisecond.
struct Waiting {
    timer: Timer,
    // The event or the timer failed once and the log said so; the channel's
    // own timed wait stands in for them from then on.
    failed: bool,
}

impl Waiting {
    // None once the room stops taking commands.
    fn next(
        &mut self,
        shared: &Shared,
        inbox: &Receiver<Command>,
        planned: Option<Instant>,
        now: Instant,
    ) -> Option<Command> {
        // What came during the pass goes first, without a wait.
        match inbox.try_recv() {
            Ok(command) => return Some(command),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        // A deadline already behind us would turn this into a busy loop; one
        // millisecond is far below anything the room waits for.
        let target = planned.map(|at| at.max(now + Duration::from_millis(1)));
        if !self.failed {
            let set = target.map_or(Ok(()), |target| self.timer.set_at(target));
            let timer = target.map(|_| &self.timer);
            match set.and_then(|()| pace::wait(&shared.commands.wake, timer)) {
                Ok(Woken::Signal) => {
                    return match inbox.try_recv() {
                        Ok(command) => Some(command),
                        Err(TryRecvError::Disconnected) => None,
                        // The event was left set by a command taken above.
                        Err(TryRecvError::Empty) => Some(Command::Wake),
                    };
                }
                Ok(Woken::Timer) => {
                    if let Some(target) = target {
                        shared
                            .late
                            .record(Instant::now().saturating_duration_since(target));
                    }
                    return Some(Command::Wake);
                }
                Err(err) => {
                    self.failed = true;
                    log!(
                        shared.log,
                        "room timers: {err}; waiting on the default windows timer from now on, up to 16 ms late"
                    );
                }
            }
        }
        let got = match target {
            Some(target) => inbox.recv_deadline(target),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(target) = target {
                    shared
                        .late
                        .record(Instant::now().saturating_duration_since(target));
                }
                Some(Command::Wake)
            }
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

fn timers(shared: &Arc<Shared>, inbox: &Receiver<Command>, timer: Timer) {
    // With every CPU busy, at normal priority this thread woke 6 to 24 ms
    // late at the 99th percentile, waiting for a CPU after its timer fired
    // (tests/timer.rs), and the pings and recover requests went that late.
    if let Err(err) = pace::raise_priority() {
        log!(
            shared.log,
            "room timers: {err}; they can wake several milliseconds late while every cpu is busy"
        );
    }
    let mut waiting = Waiting {
        timer,
        failed: false,
    };
    let mut changed_by_command = true;
    // A thread reading the address list is out, and another change came in
    // meanwhile.
    let mut relisting = false;
    let mut relist_again = false;
    loop {
        let now = Instant::now();
        let (changed, planned, name) = {
            let mut state = lock(&shared.state);
            let changed = state.side.on_timer(now, &shared.socket) || changed_by_command;
            // Commands change the state too, and each one is followed by a
            // pass through here.
            after_step(shared, &mut state, now);
            let name = state.side.name_wanted();
            // The view says it is being looked up.
            let changed = changed || name.is_some();
            let planned = next_deadline(&state);
            state.planned = planned;
            if changed {
                store_view(shared, &state, now);
            }
            (changed, planned, name)
        };
        shared.gate.run();
        // Off the state lock, and off the receive thread, which injects.
        shared.gate.refresh(now);
        if changed {
            (shared.notify)();
        }
        if let Some(request) = name {
            look_up_name(shared, request);
        }

        let Some(command) = waiting.next(shared, inbox, planned, now) else {
            return;
        };
        changed_by_command = false;
        match command {
            Command::Stop => return,
            Command::NewInvite(multi_use) => {
                changed_by_command = lock(&shared.state)
                    .side
                    .new_invite(multi_use, Instant::now());
            }
            Command::NewCode => {
                changed_by_command = lock(&shared.state).side.new_code(Instant::now());
            }
            Command::Mapping(report) => {
                changed_by_command = lock(&shared.state)
                    .side
                    .port_mapping(report, Instant::now());
            }
            // Its questions go out now, so the answers come in while the
            // next name is still being looked up.
            Command::StunFound(servers) => {
                lock(&shared.state).side.stun_found(servers, &shared.socket);
            }
            Command::StunResolved => {
                changed_by_command = lock(&shared.state).side.stun_resolved(Instant::now());
            }
            Command::Name(outcome) => {
                changed_by_command = lock(&shared.state).side.name_found(outcome, Instant::now());
            }
            Command::AddressChanged => {
                // Cleared first, so a change reported while this one is
                // handled still gets its own pass.
                shared.address_pending.store(false, Ordering::Release);
                if !shared.relist {
                    changed_by_command = lock(&shared.state).side.address_changed(
                        Instant::now(),
                        &shared.socket,
                        None,
                    );
                } else if relisting {
                    // The list being read may be from before this change.
                    relist_again = true;
                } else {
                    relisting = relist(shared);
                    if !relisting {
                        changed_by_command = lock(&shared.state).side.address_changed(
                            Instant::now(),
                            &shared.socket,
                            None,
                        );
                    }
                }
            }
            Command::Relisted(listing) => {
                relisting = std::mem::take(&mut relist_again) && relist(shared);
                changed_by_command = lock(&shared.state).side.address_changed(
                    Instant::now(),
                    &shared.socket,
                    Some(listing),
                );
            }
            Command::Voice => changed_by_command = true,
            Command::Screen => {
                changed_by_command = lock(&shared.state)
                    .side
                    .screen_work(Instant::now(), &shared.socket);
            }
            Command::Control => {
                changed_by_command = lock(&shared.state)
                    .side
                    .control_work(Instant::now(), &shared.socket);
            }
            Command::Wake => {}
        }
        shared.gate.run();
    }
}

fn first_stun_servers(mut names: Vec<String>, log: &Log) -> Vec<String> {
    if names.len() > MAX_STUN_SERVERS {
        log!(
            log,
            "stun servers: the first {MAX_STUN_SERVERS} of {} are used, the rest are left out: {}",
            names.len(),
            names[MAX_STUN_SERVERS..].join(", ")
        );
        names.truncate(MAX_STUN_SERVERS);
    }
    names
}

// Name lookups block with no timeout, so they run on a thread of their own
// that holds only a Weak while one runs: leave() does not wait for them, the
// socket still closes at once, and the timer thread goes on meanwhile, so a
// slow lookup neither holds up what the mapper found nor stretches the
// first invite's wait.
fn look_up_stun(weak: &Weak<Shared>, names: &[String]) {
    for name in names {
        let found = net::stun::resolve(name);
        let Some(shared) = weak.upgrade() else {
            return;
        };
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        if found.is_empty() {
            log!(
                shared.log,
                "stun server {name} did not resolve to any address, left out"
            );
        } else {
            log!(shared.log, "stun server {name} is {}", list(&found));
        }
        shared.commands.send(Command::StunFound(found));
    }
    if let Some(shared) = weak.upgrade() {
        shared.commands.send(Command::StunResolved);
    }
}

// The address list and the route to the internet, read on a thread of its
// own that holds only a Weak, like the lookups. False when it could not
// start, and the change goes on without the list.
fn relist(shared: &Arc<Shared>) -> bool {
    let weak = Arc::downgrade(shared);
    let spawned = thread::Builder::new()
        .name("room address list".into())
        .spawn(move || {
            let listing = net::addrs::local_addresses().map(|addrs| Listing {
                router: net::addrs::mapping_gateway(&addrs),
                addrs,
            });
            let Some(shared) = weak.upgrade() else {
                return;
            };
            if !shared.stop.load(Ordering::Acquire) {
                shared.commands.send(Command::Relisted(listing));
            }
        });
    if let Err(err) = &spawned {
        log!(
            shared.log,
            "could not start a thread to list this pc's addresses again: {err}"
        );
    }
    spawned.is_ok()
}

// The same rules as the STUN lookup: a thread of its own that holds only a
// Weak, so leave() never waits on a nameserver.
fn look_up_name(shared: &Arc<Shared>, request: Request) {
    let weak = Arc::downgrade(shared);
    let log = shared.log.clone();
    let name = request.name.clone();
    let spawned = thread::Builder::new()
        .name("room name lookup".into())
        .spawn(move || {
            let outcome = names::look_up(request, &log);
            let Some(shared) = weak.upgrade() else {
                return;
            };
            if !shared.stop.load(Ordering::Acquire) {
                shared.commands.send(Command::Name(outcome));
            }
        });
    if let Err(err) = spawned {
        log!(
            shared.log,
            "address name: could not start a thread to look up {name}: {err}"
        );
        shared.commands.send(Command::Name(Outcome {
            servers: None,
            result: Err(DnsError::Unanswered(name)),
        }));
    }
}

// A panic on one thread leaves the state as it was at the panic. Carrying on
// with it beats taking the other thread down too.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_stun_list_cut() {
        let names: Vec<String> = (1..=7)
            .map(|n| format!("stun{n}.example.org:3478"))
            .collect();
        let (log, captured) = Log::capture(8);
        let kept = first_stun_servers(names.clone(), &log);
        assert_eq!(kept, names[..MAX_STUN_SERVERS]);
        assert_eq!(
            captured.lines(),
            [
                "stun servers: the first 4 of 7 are used, the rest are left out: stun5.example.org:3478, stun6.example.org:3478, stun7.example.org:3478"
            ]
        );

        let (log, captured) = Log::capture(8);
        let two = names[..2].to_vec();
        assert_eq!(first_stun_servers(two.clone(), &log), two);
        assert!(captured.lines().is_empty());
    }
}
