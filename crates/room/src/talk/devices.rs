// The microphone and the speakers of a room, opened and closed on a thread
// of their own. Opening a Bluetooth headset can take a second while it
// changes profile, and letting go of one far longer, over 30 s once; neither
// the panel nor the room's threads wait for that. Each stream closes on its
// own thread, so a slow close holds up no order here either. Orders are
// taken one at a time, and a stream opens only once the last one on the same
// device has let go, so a room never has two on a device at once.
//
// A room can be left while its streams are still closing. They close on
// their own threads, holding nothing of the room's (talk::Shared::let_go).
// A next room that would open the same device waits on its own devices
// thread until they have ended (earlier_closed), so a device never has two
// rooms' streams on it either: the new open starts once the old close is
// over, whatever the headset's driver does in between.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, after, never, select};
use voice::audio::fake::Fake;
use voice::audio::{
    AudioError, Capture, Choice, Direction, Event, Render, StreamInfo, StreamThread,
};
use voice::process::Processor;

use super::ear::Speaker;
use super::mouth::Mouth;
use super::{Devices, Shared, Status, VoiceConfig, lock};
use crate::log::{Log, log, yes_no};
use crate::peer::Clock;

// Every stream a room in this process opened that has not ended yet, closing
// ones included.
static OPEN: Mutex<Vec<Opened>> = Mutex::new(Vec::new());
// Numbers the rooms, so a wait can say whose device is still closing.
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

pub(super) fn gauge() {
    let open = lock(&OPEN);
    stats::gauge!("room.devices.open", *open);
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

// A stream that may be on the device `device` would open now:
// one on the same id, or one whose device is not known yet because it is
// still opening, or when Windows does not say which its default is. By id,
// so a default that moved to other speakers does not wait for the old
// ones, and a headset chosen in settings waits for the same headset opened
// as Windows' default.
fn in_the_way(device: &Which) -> Option<(u64, StreamThread)> {
    let beside: Vec<(u64, StreamThread)> = {
        let mut open = lock(&OPEN);
        open.retain(|opened| opened.thread.running());
        open.iter()
            .filter(|opened| opened.device.beside(device))
            .map(|opened| (opened.room, opened.thread.clone()))
            .collect()
    };
    if beside.is_empty() {
        return None;
    }
    // Only now, so a room with nothing closing never asks Windows.
    let id = device.id_now();
    beside
        .into_iter()
        .find(|(_, thread)| match (thread.device(), &id) {
            (Some(on), Some(id)) => on == *id,
            _ => true,
        })
}

// A wait for an earlier stream on the device a room would open, and whether
// that stream is this room's own.
struct Wait {
    thread: StreamThread,
    started: Instant,
    own: bool,
}

// Whether no earlier stream is left on the device `device` would open, with
// `wait` kept up to date. It never blocks: the devices thread waits in one
// select, so a wait on one device holds up neither the other nor an order,
// and the room's network runs meanwhile. The panel says under your row why
// that device has no sound, unless the stream is this room's own and no new
// choice waits for it, as for an unmute while the microphone of the mute
// still closes; a new choice made during that wait puts the line up then,
// and so does a wait that moves to the last room's stream. The line stays
// until the device opens, or fails to, which replaces it (on_event); a
// caller that opens nothing after the wait takes it away (not_waiting).
fn earlier_closed(
    room: u64,
    device: &Which,
    switched: bool,
    wait: &mut Option<Wait>,
    shared: &Shared,
    log: &Log,
) -> bool {
    let Some((owner, thread)) = in_the_way(device) else {
        if let Some(done) = wait.take() {
            log!(
                log,
                "voice: waited {} ms for {} {}",
                done.started.elapsed().as_millis(),
                if done.own {
                    "this room's last"
                } else {
                    "the last room's"
                },
                device.word()
            );
        }
        return true;
    };
    let own = owner == room;
    if !own || switched {
        let line = AudioError::StillClosing {
            direction: device.direction,
            default: device.choice == Choice::Default,
            switched: own,
        };
        let mut status = lock(device.status(shared));
        if status.error.as_ref() != Some(&line) {
            status.error = Some(line);
            drop(status);
            shared.changed();
        }
    }
    if let Some(wait) = wait {
        wait.thread = thread;
        wait.own = own;
        return false;
    }
    log!(
        log,
        "voice: {}",
        match (device.direction, own) {
            (Direction::Input, true) =>
                "the microphone is still closing, it opens again once it has let go",
            (Direction::Output, true) =>
                "the speakers are still closing, they open again once they have let go",
            (Direction::Input, false) =>
                "the last room's microphone is still closing, it opens here once it has let go",
            (Direction::Output, false) =>
                "the last room's speakers are still closing, they open here once they have let go",
        }
    );
    *wait = Some(Wait {
        thread,
        started: Instant::now(),
        own,
    });
    false
}

enum Order {
    // Open or close the microphone to match mute and deafen as they are now.
    Follow,
    Voice(VoiceConfig),
    Stop,
}

pub(crate) struct Streams {
    orders: Sender<Order>,
    devices: Devices,
    process_loopback: bool,
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
        let devices = config.devices.clone();
        let process_loopback = config.process_loopback;
        let thread = thread::Builder::new()
            .name(String::from("voice devices"))
            .spawn(move || {
                let run = Run {
                    config,
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
            devices,
            process_loopback,
            thread: Some(thread),
            done,
        })
    }

    pub(crate) fn set_voice(&self, mut config: VoiceConfig) {
        config.devices = self.devices.clone();
        config.process_loopback = self.process_loopback;
        let _ = self.orders.send(Order::Voice(config));
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
    config: VoiceConfig,
    shared: &'a Arc<Shared>,
    clock: Clock,
    log: &'a Log,
    orders: &'a Receiver<Order>,
    room: u64,
}

// The room's microphone: the one open, or the wait before the next one
// opens, and whether that one is a new choice. `closing` has every stream of
// the room still closing, microphones let go at a mute or a new choice and
// speakers at a new choice, each with what the log says once it has.
#[derive(Default)]
struct Microphone {
    open: Option<Capture>,
    wait: Option<Wait>,
    switched: bool,
    closing: Vec<(StreamThread, &'static str)>,
}

impl Run<'_> {
    fn run(mut self, speaker: Speaker) {
        let shared = self.shared;
        let log = self.log;
        log!(
            log,
            "voice: {}, constant rate {}, input {}, output {}",
            match self.config.talk {
                super::TalkMode::PushToTalk => "push to talk",
                super::TalkMode::OpenMic => "open mic",
            },
            if self.config.constant_rate {
                "on"
            } else {
                "off"
            },
            self.config.input.id().unwrap_or("windows default"),
            self.config.output.id().unwrap_or("windows default")
        );
        // While no render pulls the talkers through, their orders are taken
        // here instead, so none piles up for speakers that never come.
        let unheard = speaker.orders();
        let speaker = Arc::new(Mutex::new(speaker));
        let mut retired = Arc::new(AtomicBool::new(false));
        let mut microphone = Microphone::default();
        let mut speakers_wait = None;
        let mut speakers_switched = false;
        let mut next = Order::Follow;
        loop {
            match next {
                Order::Stop => break,
                Order::Follow => {}
                Order::Voice(config) => {
                    let output = config.output != self.config.output;
                    let input = config.input != self.config.input;
                    let reopen = input
                        || config.echo_cancellation != self.config.echo_cancellation
                        || (output && config.echo_cancellation);
                    if input {
                        log!(
                            log,
                            "voice: input now {}",
                            config.input.id().unwrap_or("windows default")
                        );
                    }
                    if output {
                        log!(
                            log,
                            "voice: output now {}",
                            config.output.id().unwrap_or("windows default")
                        );
                    }
                    if reopen {
                        microphone.switched = true;
                        self.close_microphone(&mut microphone);
                    }
                    if output {
                        speakers_switched = true;
                        speakers_wait = None;
                        retired.store(true, Ordering::Release);
                        if let Some(render) = lock(&shared.render).take() {
                            microphone
                                .closing
                                .push((render.let_go(), "voice: speakers closed"));
                        }
                        *lock(&shared.speakers) = Status::default();
                        retired = Arc::new(AtomicBool::new(false));
                    }
                    self.config = config;
                }
            }
            if shared.left() {
                break;
            }
            microphone.closing.retain(|(thread, line)| {
                if thread.running() {
                    true
                } else {
                    log!(log, "{line}");
                    false
                }
            });
            if lock(&shared.render).is_none() {
                let which = Which::of(&self.config, Direction::Output);
                if earlier_closed(
                    self.room,
                    &which,
                    speakers_switched,
                    &mut speakers_wait,
                    shared,
                    log,
                ) {
                    speakers_switched = false;
                    match open_speakers(
                        &self.config,
                        shared,
                        Arc::clone(&speaker),
                        Arc::clone(&retired),
                        log,
                    ) {
                        Ok(render) => {
                            note_open(self.room, &which, render.thread());
                            *lock(&shared.render) = Some(render);
                        }
                        Err(err) => {
                            log!(log, "voice: could not start the speakers: {err}");
                            lock(&shared.speakers).error = Some(err);
                            shared.changed();
                        }
                    }
                }
            }
            self.follow(&mut microphone);
            let ended = |wait: &Option<Wait>| {
                wait.as_ref()
                    .map(|wait| wait.thread.ended().clone())
                    .unwrap_or_else(never)
            };
            let closed = microphone
                .closing
                .first()
                .map(|(thread, _)| thread.ended().clone())
                .unwrap_or_else(never);
            let (speakers_free, microphone_free) = (ended(&speakers_wait), ended(&microphone.wait));
            let look_again = if speakers_wait.is_some() || microphone.wait.is_some() {
                after(LOOK_AGAIN)
            } else {
                never()
            };
            let mut playing = lock(&shared.render)
                .as_ref()
                .map(Render::thread)
                .filter(StreamThread::running);
            next = loop {
                let (stops, pending) = match &playing {
                    Some(thread) => (thread.ended().clone(), never()),
                    None => (never(), unheard.clone()),
                };
                select! {
                    recv(self.orders) -> order => break order.unwrap_or(Order::Stop),
                    recv(closed) -> _ => break Order::Follow,
                    recv(speakers_free) -> _ => break Order::Follow,
                    recv(microphone_free) -> _ => break Order::Follow,
                    recv(look_again) -> _ => break Order::Follow,
                    recv(stops) -> _ => playing = None,
                    recv(pending) -> order => {
                        if let Ok(order) = order {
                            lock(&speaker).take(order);
                        }
                    }
                }
            };
        }
        retired.store(true, Ordering::Release);
        self.close_microphone(&mut microphone);
        if let Some(render) = lock(&shared.render).take() {
            microphone
                .closing
                .push((render.let_go(), "voice: speakers closed"));
        }
        for (thread, line) in microphone.closing {
            let _ = thread.ended().recv();
            log!(log, "{line}");
        }
    }

    fn close_microphone(&self, microphone: &mut Microphone) {
        if let Some(open) = microphone.open.take() {
            // In this order, and the Mouth checks again after it marks a
            // spell, so one the old microphone starts as it goes cannot leave
            // your row reading as sending.
            self.shared
                .capture_generation
                .fetch_add(1, Ordering::SeqCst);
            self.shared.sending.store(false, Ordering::SeqCst);
            let line = if self.shared.left() {
                "voice: microphone closed, the room closed"
            } else if self.shared.over() {
                "voice: microphone closed, nobody is left to hear it"
            } else {
                "voice: microphone closed"
            };
            microphone.closing.push((open.let_go(), line));
            *lock(&self.shared.microphone) = Status::default();
            *lock(&self.shared.level) = None;
            self.shared.changed();
        }
    }

    fn follow(&self, microphone: &mut Microphone) {
        let shared = self.shared;
        let log = self.log;
        let wanted = shared.microphone_wanted();
        if !wanted {
            self.close_microphone(microphone);
        }
        // A wait under way goes on while muted, so an unmute after it opens
        // at once; a muted microphone starts none.
        if microphone.open.is_some() || (!wanted && microphone.wait.is_none()) {
            return;
        }
        let which = Which::of(&self.config, Direction::Input);
        if !earlier_closed(
            self.room,
            &which,
            microphone.switched,
            &mut microphone.wait,
            shared,
            log,
        ) {
            return;
        }
        if !wanted {
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
            return;
        }
        microphone.switched = false;
        let mouth = match Mouth::new(Arc::clone(shared), self.clock) {
            Ok(mouth) => mouth,
            Err(err) => {
                log!(
                    log,
                    "voice: could not make the opus encoder, nothing is sent: {err}"
                );
                which.not_waiting(shared);
                return;
            }
        };
        match open_microphone(&self.config, shared, mouth, log) {
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
    }
}

fn open_microphone(
    config: &VoiceConfig,
    shared: &Arc<Shared>,
    mut mouth: Mouth,
    log: &Log,
) -> Result<Capture, AudioError> {
    let mut processor = Processor::new(Arc::clone(&shared.switches));
    let level = processor.level();
    *lock(&shared.level) = Some((Arc::clone(&level), config.input.clone()));
    // Set at each open: a microphone opened again after a gap, or another
    // one once Windows' default moved, starts afresh.
    let fresh = Arc::new(AtomicBool::new(false));
    let opened = Arc::clone(&fresh);
    let audio = move |samples: &[f32], at: Instant| {
        if fresh.swap(false, Ordering::Relaxed) {
            processor.restart();
            mouth.restart();
        }
        let (samples, at) = processor.run(samples, at);
        mouth.hear(samples, at);
    };
    let mut said = on_event(Arc::clone(shared), log.clone(), Side::Microphone, None);
    let away = Arc::clone(shared);
    let generation = shared.capture_generation.load(Ordering::Relaxed);
    // Where Windows already suppresses noise or evens out the volume on
    // this microphone, Booth's own stage stands down rather than do it
    // twice. Opened comes before the first packet, on the same thread.
    let events = move |event: Event| {
        if generation != away.capture_generation.load(Ordering::Relaxed) {
            return;
        }
        level.clear();
        if matches!(event, Event::Opened(_)) {
            opened.store(true, Ordering::Relaxed);
        }
        said(event);
    };
    let choice = config.input.clone();
    match &config.devices {
        Devices::Windows if config.echo_cancellation => {
            let reference = config.output.id().map(str::to_owned);
            Capture::start_cancelling_echo(choice, reference, audio, events)
        }
        Devices::Windows => Capture::start(choice, audio, events),
        Devices::Fake { microphone, .. } => {
            Capture::start_with(microphone.device(), choice, audio, events)
        }
    }
}

fn open_speakers(
    config: &VoiceConfig,
    shared: &Arc<Shared>,
    speaker: Arc<Mutex<Speaker>>,
    retired: Arc<AtomicBool>,
    log: &Log,
) -> Result<Render, AudioError> {
    let fill = speaker_audio(speaker, Arc::clone(&retired));
    let events = on_event(
        Arc::clone(shared),
        log.clone(),
        Side::Speakers,
        Some(retired),
    );
    let choice = config.output.clone();
    match &config.devices {
        Devices::Windows => Render::start_stereo(choice, fill, events),
        Devices::Fake { speakers, .. } => {
            Render::start_stereo_with(speakers.device(), choice, fill, events)
        }
    }
}

fn speaker_audio(
    speaker: Arc<Mutex<Speaker>>,
    retired: Arc<AtomicBool>,
) -> impl FnMut(&mut [f32]) + Send + 'static {
    move |out| {
        if retired.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }
        let mut speaker = lock(&speaker);
        if retired.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }
        speaker.fill(out);
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
fn on_event(
    shared: Arc<Shared>,
    log: Log,
    side: Side,
    retired: Option<Arc<AtomicBool>>,
) -> impl FnMut(Event) + Send + 'static {
    let generation = shared.capture_generation.load(Ordering::Relaxed);
    move |event| {
        let status = match side {
            Side::Microphone => &shared.microphone,
            Side::Speakers => &shared.speakers,
        };
        let mut status = lock(status);
        if retired
            .as_ref()
            .is_some_and(|retired| retired.load(Ordering::Acquire))
            || (matches!(side, Side::Microphone)
                && generation != shared.capture_generation.load(Ordering::Relaxed))
        {
            return;
        }
        // No packet ends a spell while the microphone is away. Retired
        // streams must leave the current microphone's spell alone.
        if matches!(side, Side::Microphone) && !matches!(event, Event::Opened(_)) {
            shared.sending.store(false, Ordering::Relaxed);
        }
        match event {
            Event::Opened(info) => {
                log!(log, "voice: {} {}", side.word(), opened(&info));
                if let Side::Speakers = side {
                    shared
                        .render_now
                        .period
                        .store(info.period_frames, Ordering::Relaxed);
                }
                if let Side::Microphone = side {
                    shared.follow_effects(Some(&info));
                }
                *status = Status {
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
                *status = Status {
                    info: None,
                    error: Some(err),
                };
            }
            Event::Gone(err) => {
                log!(
                    log,
                    "voice: {} not open, opened again once Windows gives it: {err}",
                    side.word()
                );
                *status = Status {
                    info: None,
                    error: Some(err),
                };
            }
            Event::Failed(err) => {
                log!(log, "voice: {} stopped: {err}", side.word());
                *status = Status {
                    info: None,
                    error: Some(err),
                };
            }
        }
        drop(status);
        shared.changed();
    }
}

fn opened(info: &StreamInfo) -> String {
    let refused = info
        .refused
        .map(|refused| {
            format!(
                ", opened on a second try: the first, from an engine at {} Hz, was refused with 0x{:08X}",
                refused.engine.rate, refused.code
            )
        })
        .unwrap_or_default();
    let processing = match info.direction {
        Direction::Input => format!(
            ", echo cancellation {}, windows noise suppression {}, windows gain control {}",
            info.echo,
            yes_no(info.effects.noise_suppression),
            yes_no(info.effects.automatic_gain)
        ),
        Direction::Output => String::new(),
    };
    format!(
        "opened: {}, period {:.2} ms ({} frames), engine {} Hz, resampled by windows {}, bluetooth hands-free {}, small period {}, pro audio {}, stream ({}), stream latency {:.2} ms{processing}{refused}",
        crate::log::quoted(&info.device),
        info.period_ms(),
        info.period_frames,
        info.engine.rate,
        yes_no(info.resampled),
        yes_no(info.hands_free),
        yes_no(info.small_period),
        yes_no(info.pro_audio),
        info.format,
        info.stream_latency.as_secs_f64() * 1000.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_processing_uses_the_switches_and_windows_effects() {
        let fake = Fake::new(
            voice::audio::fake::Setup::default(),
            &[("mic", "Test microphone")],
            Some("mic"),
        );
        let mut config = VoiceConfig {
            automatic_volume: false,
            devices: Devices::Fake {
                microphone: fake.clone(),
                speakers: fake.clone(),
                pc_sound: fake.clone(),
            },
            ..VoiceConfig::default()
        };
        let shared = Shared::new(&config, None, false);
        let clock = Clock::new(Instant::now());
        let mouth = Mouth::new(Arc::clone(&shared), clock).unwrap();
        let capture = open_microphone(&config, &shared, mouth, &Log::off()).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        let mut info = loop {
            if let Some(mic) = shared.microphone_now() {
                break mic.info;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        };
        let mut events = on_event(Arc::clone(&shared), Log::off(), Side::Microphone, None);
        let mut processor = Processor::new(Arc::clone(&shared.switches));
        let samples = [0.01; 480];
        let at = Instant::now();
        config.noise_suppression = true;
        config.automatic_volume = true;
        info.effects = voice::audio::Effects {
            noise_suppression: true,
            automatic_gain: true,
            ..Default::default()
        };
        events(Event::Opened(info.clone()));
        shared.set_voice(&config);
        assert_eq!(processor.run(&samples, at).0, samples);
        info.effects = voice::audio::Effects::default();
        events(Event::Opened(info.clone()));
        config.automatic_volume = false;
        shared.set_voice(&config);
        assert_ne!(processor.run(&samples, at).0, samples);
        config.noise_suppression = false;
        shared.set_voice(&config);
        assert_eq!(processor.run(&samples, at).0, samples);
        config.automatic_volume = true;
        shared.set_voice(&config);
        for _ in 0..100 {
            processor.run(&[0.0001; 480], at);
        }
        for _ in 0..100 {
            processor.run(&samples, at);
        }
        assert!(processor.gain_db() > 0.0);
        config.automatic_volume = false;
        shared.set_voice(&config);
        assert_eq!(processor.run(&samples, at).0, samples);
        assert_eq!(fake.record().starts, 1);
        capture.stop();
    }

    #[test]
    fn retired_speakers_never_pull_from_the_mixer() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (_orders, taken) = crossbeam_channel::unbounded();
        let speaker = Arc::new(Mutex::new(Speaker::new(taken, Arc::clone(&shared), clock)));
        let retired = Arc::new(AtomicBool::new(false));
        let mut old = speaker_audio(Arc::clone(&speaker), Arc::clone(&retired));
        let mut current = speaker_audio(speaker, Arc::new(AtomicBool::new(false)));
        retired.store(true, Ordering::Release);
        shared.cue(super::super::Cue::Rising, clock.micros(Instant::now()));
        let pending = shared.cue.load(Ordering::Relaxed);
        assert_ne!(pending, 0);
        let mut out = [1.0; 480];
        old(&mut out);
        assert!(out.iter().all(|sample| *sample == 0.0));
        assert_eq!(shared.cue.load(Ordering::Relaxed), pending);
        current(&mut out);
        assert_eq!(shared.cue.load(Ordering::Relaxed), 0);
    }

    // Each talker's next frame is placed by a count of samples that the
    // render keeps, so new speakers go on counting from the last.
    #[test]
    fn new_speakers_count_on_from_the_last() {
        let fake = Fake::new(
            voice::audio::fake::Setup::default(),
            &[("spk", "Test speakers")],
            Some("spk"),
        );
        let config = VoiceConfig {
            devices: Devices::Fake {
                microphone: fake.clone(),
                speakers: fake.clone(),
                pc_sound: fake.clone(),
            },
            ..VoiceConfig::default()
        };
        let shared = Shared::new(&config, None, false);
        let clock = Clock::new(Instant::now());
        let (_orders, taken) = crossbeam_channel::unbounded();
        let speaker = Arc::new(Mutex::new(Speaker::new(taken, Arc::clone(&shared), clock)));
        let played = || shared.render_now.start_sample.load(Ordering::Relaxed);
        let until = Instant::now() + Duration::from_secs(3);
        let retired = Arc::new(AtomicBool::new(false));
        let first = open_speakers(
            &config,
            &shared,
            Arc::clone(&speaker),
            Arc::clone(&retired),
            &Log::off(),
        )
        .unwrap();
        while played() < 4800 {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        retired.store(true, Ordering::Release);
        let _ = first.let_go().ended().recv();
        *lock(&shared.speakers) = Status::default();
        let before = played();
        let second = open_speakers(
            &config,
            &shared,
            speaker,
            Arc::new(AtomicBool::new(false)),
            &Log::off(),
        )
        .unwrap();
        while lock(&shared.speakers).info.is_none() {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        // A callback after the open, which is where a count started again
        // would show.
        let opened = played();
        while played() == opened {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(played() > before, "{} after {before}", played());
        second.stop();
    }

    // Speakers that will not run leave the talkers' orders to this thread,
    // so they never pile up and refuse the next talker.
    #[test]
    fn orders_for_speakers_that_failed_are_taken_all_the_same() {
        let microphone = Fake::new(
            voice::audio::fake::Setup::default(),
            &[("mic", "Test microphone")],
            Some("mic"),
        );
        let speakers = Fake::new(
            voice::audio::fake::Setup {
                rate: 44_100,
                ..Default::default()
            },
            &[("spk", "Test speakers")],
            Some("spk"),
        );
        let config = VoiceConfig {
            devices: Devices::Fake {
                microphone: microphone.clone(),
                speakers,
                pc_sound: microphone,
            },
            ..VoiceConfig::default()
        };
        let shared = Shared::new(&config, None, false);
        let clock = Clock::new(Instant::now());
        let (orders, taken) = crossbeam_channel::unbounded();
        let speaker = Speaker::new(taken, Arc::clone(&shared), clock);
        let mut streams =
            Streams::start(config, Arc::clone(&shared), speaker, clock, Log::off()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while !matches!(lock(&shared.speakers).error, Some(AudioError::Rate { .. })) {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        for key in 0..4 {
            shared.ears_waiting.fetch_add(1, Ordering::Relaxed);
            let ear = Arc::new(Mutex::new(super::super::ear::Ear::new().unwrap()));
            let add = super::super::ear::ToSpeaker::Add([key; 32], ear, 1.0);
            orders.send(add).unwrap();
        }
        while !orders.is_empty() || shared.ears_waiting.load(Ordering::Relaxed) != 0 {
            assert!(Instant::now() < until, "the orders still wait");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(streams.stop(Duration::from_secs(1)));
    }
}
