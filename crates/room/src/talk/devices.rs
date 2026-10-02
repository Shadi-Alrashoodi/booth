// The microphone and the speakers of a room, opened and closed on a thread
// of their own. Opening a Bluetooth headset can take a second while it
// changes profile, and letting go of one far longer, over 30 s once; neither
// the panel nor the room's threads wait for that. Each stream closes on its
// own thread, so a slow close holds up no order here either. Orders are
// taken one at a time, and a microphone opens only once the last one has let
// go, so a room never has two on the device at once.
//
// A room can be left while its streams are still closing. They close on
// their own threads, holding nothing of the room's (talk::Shared::let_go).
// A next room that would open the same device waits on its own devices
// thread until they have ended (earlier_closed), so a device never has two
// rooms' streams on it either: the new open starts once the old close is
// over, whatever the headset's driver does in between.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, never, select};
use voice::audio::fake::Fake;
use voice::audio::{
    AudioError, Capture, Choice, Direction, Event, Render, StreamInfo, StreamThread,
};

use super::ear::Speaker;
use super::mouth::Mouth;
use super::{Devices, Shared, Status, VoiceConfig, lock};
use crate::log::{Log, log, yes_no};
use crate::peer::Clock;

// Every stream a room in this process opened that has not ended yet, closing
// ones included.
static OPEN: Mutex<Vec<Opened>> = Mutex::new(Vec::new());
// Numbers the rooms, so a room never waits for its own streams.
static NEXT_ROOM: AtomicU64 = AtomicU64::new(0);
// How often a room waiting for the last room's device asks again which one
// it would open: Windows' default can move to another device meanwhile.
const LOOK_AGAIN: Duration = Duration::from_millis(250);

struct Opened {
    room: u64,
    device: Which,
    thread: StreamThread,
}

// A device as a room asks for it: Windows' own or a fake, which side, and
// the choice from settings.
#[derive(Clone)]
struct Which {
    fake: Option<Fake>,
    direction: Direction,
    choice: Choice,
}

impl Which {
    fn of(config: &VoiceConfig, direction: Direction) -> Which {
        let fake = match (&config.devices, direction) {
            (Devices::Windows, _) => None,
            (Devices::Fake { microphone, .. }, Direction::Input) => Some(microphone.clone()),
            (Devices::Fake { speakers, .. }, Direction::Output) => Some(speakers.clone()),
        };
        let choice = match direction {
            Direction::Input => config.input.clone(),
            Direction::Output => config.output.clone(),
        };
        Which {
            fake,
            direction,
            choice,
        }
    }

    // Windows' devices or the same fake, on the same side.
    fn beside(&self, other: &Which) -> bool {
        let source = match (&self.fake, &other.fake) {
            (None, None) => true,
            (Some(one), Some(other)) => one.same_as(other),
            _ => false,
        };
        source && self.direction == other.direction
    }

    // The id an open would get now: the one chosen in settings, or Windows'
    // default as it is at this moment. None when there is no default or
    // Windows would not say which it is.
    fn id_now(&self) -> Option<String> {
        match &self.choice {
            Choice::Device(id) => Some(id.clone()),
            Choice::Default => match &self.fake {
                Some(fake) => fake.default_id(),
                None => voice::audio::default_id(self.direction).ok().flatten(),
            },
        }
    }

    fn word(&self) -> &'static str {
        match self.direction {
            Direction::Input => "microphone",
            Direction::Output => "speakers",
        }
    }

    fn status<'a>(&self, shared: &'a Shared) -> &'a Mutex<Status> {
        match self.direction {
            Direction::Input => &shared.microphone,
            Direction::Output => &shared.speakers,
        }
    }

    // After a wait that opens nothing, so the line under your row does not
    // say the device is waiting when it is not.
    fn not_waiting(&self, shared: &Shared) {
        let mut status = lock(self.status(shared));
        if matches!(status.error, Some(AudioError::StillClosing { .. })) {
            status.error = None;
            drop(status);
            shared.changed();
        }
    }
}

fn note_open(room: u64, device: &Which, thread: StreamThread) {
    let mut open = lock(&OPEN);
    open.retain(|opened| opened.thread.running());
    open.push(Opened {
        room,
        device: device.clone(),
        thread,
    });
}

// Another room's stream that may be on the device `device` would open now:
// one on the same id, or one whose device is not known yet because it is
// still opening, or when Windows does not say which its default is. By id,
// so a default that moved to other speakers does not wait for the old
// ones, and a headset chosen in settings waits for the same headset opened
// as Windows' default.
fn in_the_way(room: u64, device: &Which) -> Option<StreamThread> {
    let beside: Vec<StreamThread> = {
        let mut open = lock(&OPEN);
        open.retain(|opened| opened.thread.running());
        open.iter()
            .filter(|opened| opened.room != room && opened.device.beside(device))
            .map(|opened| opened.thread.clone())
            .collect()
    };
    if beside.is_empty() {
        return None;
    }
    // Only now, so a room with nothing closing never asks Windows.
    let id = device.id_now();
    beside
        .into_iter()
        .find(|thread| match (thread.device(), &id) {
            (Some(on), Some(id)) => on == *id,
            _ => true,
        })
}

// Waits until no other room has a stream left on the device `device` would
// open, taking orders meanwhile. False when the room asked to stop. Only
// this thread waits: the room is up and its network runs, and the panel
// says under your row why that device has no sound. The line stays until
// the device opens, or fails to, which replaces it (on_event); a caller
// that opens nothing after the wait takes it away (not_waiting).
fn earlier_closed(
    room: u64,
    device: &Which,
    shared: &Shared,
    orders: &Receiver<Order>,
    log: &Log,
) -> bool {
    let Some(mut first) = in_the_way(room, device) else {
        return true;
    };
    lock(device.status(shared)).error = Some(AudioError::StillClosing {
        direction: device.direction,
        default: device.choice == Choice::Default,
    });
    shared.changed();
    let started = Instant::now();
    log!(
        log,
        "voice: {}",
        match device.direction {
            Direction::Input =>
                "the last room's microphone is still closing, it opens here once it has let go",
            Direction::Output =>
                "the last room's speakers are still closing, they open here once they have let go",
        }
    );
    loop {
        select! {
            recv(first.ended()) -> _ => {}
            recv(orders) -> order => match order {
                // Mute and deafen are read again once it has let go.
                Ok(Order::Follow) => {}
                Ok(Order::Stop) | Err(_) => return false,
            },
            default(LOOK_AGAIN) => {}
        }
        match in_the_way(room, device) {
            Some(next) => first = next,
            None => break,
        }
    }
    log!(
        log,
        "voice: waited {} ms for the last room's {}",
        started.elapsed().as_millis(),
        device.word()
    );
    true
}

// Waits for a stream's thread to end, taking orders meanwhile. False when
// the room asked to stop first.
fn wait_ended(thread: &StreamThread, orders: &Receiver<Order>) -> bool {
    loop {
        select! {
            // Nothing is ever sent on it: it only disconnects.
            recv(thread.ended()) -> _ => return true,
            recv(orders) -> order => match order {
                // Mute and deafen are read again once it has let go.
                Ok(Order::Follow) => {}
                Ok(Order::Stop) | Err(_) => return false,
            },
        }
    }
}

enum Order {
    // Open or close the microphone to match mute and deafen as they are now.
    Follow,
    Stop,
}

pub(crate) struct Streams {
    orders: Sender<Order>,
    thread: Option<JoinHandle<()>>,
    // Disconnects when the thread has closed both streams.
    done: Receiver<()>,
}

impl Streams {
    pub(crate) fn start(
        config: VoiceConfig,
        shared: Arc<Shared>,
        speaker: Speaker,
        clock: Clock,
        log: Log,
    ) -> io::Result<Streams> {
        let (orders, taken) = crossbeam_channel::unbounded();
        let (finished, done) = crossbeam_channel::bounded::<()>(0);
        let room_over = orders.clone();
        shared.on_follow(move || {
            let _ = room_over.send(Order::Follow);
        });
        let thread = thread::Builder::new()
            .name(String::from("voice devices"))
            .spawn(move || {
                let run = Run {
                    config: &config,
                    shared: &shared,
                    clock,
                    log: &log,
                    orders: &taken,
                    room: NEXT_ROOM.fetch_add(1, Ordering::Relaxed),
                };
                run.run(speaker);
                drop(finished);
            })?;
        Ok(Streams {
            orders,
            thread: Some(thread),
            done,
        })
    }

    pub(crate) fn follow(&self) {
        let _ = self.orders.send(Order::Follow);
    }

    // Waits up to `within` for both streams to close, and says whether they
    // did; a slower device is left to close on its own thread, which ends
    // by itself.
    pub(crate) fn stop(&mut self, within: Duration) -> bool {
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let _ = self.orders.send(Order::Stop);
        if let Err(RecvTimeoutError::Disconnected) = self.done.recv_timeout(within) {
            let _ = thread.join();
            return true;
        }
        false
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
    }
}

// The devices thread.
struct Run<'a> {
    config: &'a VoiceConfig,
    shared: &'a Arc<Shared>,
    clock: Clock,
    log: &'a Log,
    orders: &'a Receiver<Order>,
    room: u64,
}

// The room's microphone: the one open, or else the last one, still closing
// after a mute, with what the log says once it has.
#[derive(Default)]
struct Microphone {
    open: Option<Capture>,
    closing: Option<(StreamThread, &'static str)>,
}

impl Run<'_> {
    fn run(&self, speaker: Speaker) {
        let (config, shared, log) = (self.config, self.shared, self.log);
        log!(
            log,
            "voice: {}, constant rate {}, input {}, output {}",
            match config.talk {
                super::TalkMode::PushToTalk => "push to talk",
                super::TalkMode::OpenMic => "open mic",
            },
            if config.constant_rate { "on" } else { "off" },
            config.input.id().unwrap_or("windows default"),
            config.output.id().unwrap_or("windows default")
        );
        let speakers = Which::of(config, Direction::Output);
        if !earlier_closed(self.room, &speakers, shared, self.orders, log) || shared.left() {
            return;
        }
        match open_speakers(config, shared, speaker, log) {
            Ok(render) => {
                note_open(self.room, &speakers, render.thread());
                *lock(&shared.render) = Some(render);
            }
            Err(err) => {
                log!(log, "voice: could not start the speakers: {err}");
                lock(&shared.speakers).error = Some(err);
                shared.changed();
            }
        }
        let mut microphone = Microphone::default();
        let mut going = self.follow(&mut microphone);
        while going {
            let closed = match &microphone.closing {
                Some((thread, _)) => thread.ended().clone(),
                None => never(),
            };
            select! {
                recv(self.orders) -> order => match order {
                    Ok(Order::Follow) => going = self.follow(&mut microphone),
                    Ok(Order::Stop) | Err(_) => break,
                },
                recv(closed) -> _ => {
                    if let Some((_, line)) = microphone.closing.take() {
                        log!(log, "{line}");
                    }
                }
            }
        }
        // Both asked at once, so a microphone slow to let go does not keep
        // the speakers open too. Each closes on its own thread.
        let open = microphone.open.take().map(Capture::let_go);
        let render = lock(&shared.render).take();
        let speakers = render.map(Render::let_go);
        if let Some((thread, line)) = microphone.closing.take() {
            let _ = thread.ended().recv();
            log!(log, "{line}");
        }
        if let Some(thread) = open {
            let _ = thread.ended().recv();
            log!(log, "voice: microphone closed, the room closed");
        }
        if let Some(thread) = speakers {
            let _ = thread.ended().recv();
            log!(log, "voice: speakers closed");
        }
    }

    // False when the room was left, or asked to stop while the microphone
    // waited for the last one to let go.
    fn follow(&self, microphone: &mut Microphone) -> bool {
        let shared = self.shared;
        let log = self.log;
        if shared.left() {
            return false;
        }
        if !shared.microphone_wanted() {
            if let Some(open) = microphone.open.take() {
                let line = if shared.over() {
                    "voice: microphone closed, nobody is left to hear it"
                } else {
                    "voice: microphone closed"
                };
                // On its own thread, so a headset slow to let go keeps
                // neither an unmute nor Leave waiting behind it here.
                microphone.closing = Some((open.let_go(), line));
                *lock(&shared.microphone) = Status::default();
                shared.changed();
            }
            return true;
        }
        if microphone.open.is_some() {
            return true;
        }
        if let Some((thread, line)) = microphone.closing.take() {
            if thread.running() {
                log!(
                    log,
                    "voice: the microphone is still closing, it opens again once it has let go"
                );
            }
            if !wait_ended(&thread, self.orders) {
                microphone.closing = Some((thread, line));
                return false;
            }
            log!(log, "{line}");
        }
        let which = Which::of(self.config, Direction::Input);
        if !earlier_closed(self.room, &which, shared, self.orders, log) {
            return false;
        }
        // Mute, deafen or Leave may have come meanwhile.
        if shared.left() {
            return false;
        }
        if !shared.microphone_wanted() {
            which.not_waiting(shared);
            log!(
                log,
                "voice: {}",
                if shared.over() {
                    "nobody is left to hear it, the microphone stays closed"
                } else {
                    "muted meanwhile, the microphone stays closed"
                }
            );
            return true;
        }
        let mouth = match Mouth::new(Arc::clone(shared), self.clock) {
            Ok(mouth) => mouth,
            Err(err) => {
                log!(
                    log,
                    "voice: could not make the opus encoder, nothing is sent: {err}"
                );
                which.not_waiting(shared);
                return true;
            }
        };
        match open_microphone(self.config, shared, mouth, log) {
            Ok(open) => {
                note_open(self.room, &which, open.thread());
                microphone.open = Some(open);
            }
            Err(err) => {
                log!(log, "voice: could not start the microphone: {err}");
                lock(&shared.microphone).error = Some(err);
                shared.changed();
            }
        }
        true
    }
}

fn open_microphone(
    config: &VoiceConfig,
    shared: &Arc<Shared>,
    mut mouth: Mouth,
    log: &Log,
) -> Result<Capture, AudioError> {
    let audio = move |samples: &[f32], at: Instant| mouth.hear(samples, at);
    let events = on_event(Arc::clone(shared), log.clone(), Side::Microphone);
    let choice = config.input.clone();
    match &config.devices {
        Devices::Windows => Capture::start(choice, audio, events),
        Devices::Fake { microphone, .. } => {
            Capture::start_with(microphone.device(), choice, audio, events)
        }
    }
}

fn open_speakers(
    config: &VoiceConfig,
    shared: &Arc<Shared>,
    mut speaker: Speaker,
    log: &Log,
) -> Result<Render, AudioError> {
    let fill = move |out: &mut [f32]| speaker.fill(out);
    let events = on_event(Arc::clone(shared), log.clone(), Side::Speakers);
    let choice = config.output.clone();
    match &config.devices {
        Devices::Windows => Render::start(choice, fill, events),
        Devices::Fake { speakers, .. } => {
            Render::start_with(speakers.device(), choice, fill, events)
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Microphone,
    Speakers,
}

impl Side {
    fn word(self) -> &'static str {
        match self {
            Side::Microphone => "microphone",
            Side::Speakers => "speakers",
        }
    }
}

// On the stream's own thread: what the view shows and the log says.
fn on_event(shared: Arc<Shared>, log: Log, side: Side) -> impl FnMut(Event) + Send + 'static {
    move |event| {
        let status = match side {
            Side::Microphone => &shared.microphone,
            Side::Speakers => &shared.speakers,
        };
        match event {
            Event::Opened(info) => {
                log!(log, "voice: {} {}", side.word(), opened(&info));
                if let Side::Speakers = side {
                    shared
                        .render_now
                        .period
                        .store(info.period_frames, Ordering::Relaxed);
                }
                *lock(status) = Status {
                    info: Some(info),
                    error: None,
                };
            }
            Event::Lost(err) => {
                log!(
                    log,
                    "voice: {} lost, opened again in a moment: {err}",
                    side.word()
                );
                *lock(status) = Status {
                    info: None,
                    error: Some(err),
                };
            }
            Event::Gone(err) | Event::Failed(err) => {
                log!(log, "voice: {} stopped: {err}", side.word());
                *lock(status) = Status {
                    info: None,
                    error: Some(err),
                };
            }
        }
        shared.changed();
    }
}

fn opened(info: &StreamInfo) -> String {
    format!(
        "opened: {}, period {:.2} ms ({} frames), engine {} Hz, resampled by windows {}, bluetooth hands-free {}, small period {}, pro audio {}",
        crate::log::quoted(&info.device),
        info.period_ms(),
        info.period_frames,
        info.engine.rate,
        yes_no(info.resampled),
        yes_no(info.hands_free),
        yes_no(info.small_period),
        yes_no(info.pro_audio)
    )
}
