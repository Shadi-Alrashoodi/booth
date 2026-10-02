// Voice in a room. Five threads touch it, and only one of them ever waits on
// the room's state lock:
//
// - The capture thread (mouth.rs) cuts the microphone into Opus frames,
//   decides whether to send, encodes, and seals each packet itself with the
//   session's Sealer. What it needs from the room, the socket and the links
//   to send on, it reads from Shared: one uncontended lock to clone the
//   Route, and atomics. The room swaps the Route when a session or an
//   address changes, and Leave takes it back, socket and all.
// - The receive thread, under the state lock as for every packet, checks a
//   voice packet (wire.rs), hands it on if this PC is the host, and pushes
//   it into that talker's jitter buffer. Pushing copies at most 128 bytes.
// - The render thread (ear.rs) mixes every talker's buffer. Each buffer has
//   a lock of its own, held for one pull, which decodes one frame (two when
//   the buffer drops one to shrink). The receive thread holds the same lock
//   only for a push.
// - The timer thread, under the state lock, sends the loss reports, reads
//   the numbers and switches redundancy and the frame size (adapt.rs).
// - The devices thread (devices.rs) opens and closes the microphone and the
//   speakers, so the panel never waits for a Bluetooth headset. It and the
//   audio threads may outlive the room while a headset is slow to let go,
//   holding only the devices, which a next room on the same one waits for
//   (devices.rs).
//
// The share cue (cue.rs) is asked for by whichever thread starts or ends
// this PC's share, and made by the render thread.

mod adapt;
mod cue;
mod detect;
mod devices;
mod ear;
mod mouth;
mod wire;

use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use session::Sealer;
use stats::StreamStats;
use voice::audio::fake::Fake;
use voice::audio::{AudioError, Choice, Microphone, Render, StreamInfo};
use voice::codec::Mode;

use crate::chat;
use crate::config::Timers;
use crate::control::{LossPermille, PERMILLE, Periods};
use crate::log::{Log, log};
use crate::peer::{Clock, MEDIA_FLOWS_FOR};
use crate::socket::Socket;
use crate::view::{Buffer, MouthToEar, Numbers, Spread, Voice as VoiceView, VoiceLoss};

pub(crate) use cue::Cue;
pub(crate) use devices::Streams;
pub(crate) use ear::{Ear, Speaker, ToSpeaker};
pub(crate) use mouth::HOST_SLOT;
pub(crate) use wire::{Frame, MAX_VOICE, read_relayed, read_spoken};

// JitterStats counts loss over the last 2 s, so a talker not heard for that
// long has nothing new to report.
pub(crate) const HEARD_LATELY: Duration = Duration::from_secs(2);
// How long an ear the mixer may still hold is kept after its talker left, so
// the last reference, and the decoder with it, goes on this thread and not
// the render thread.
const RETIRED_FOR: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TalkMode {
    // The default, since there is no echo cancellation: an open mic next to
    // speakers echoes.
    #[default]
    PushToTalk,
    OpenMic,
}

// How the room opens its microphone and speakers: Windows' own, or the fake
// devices in memory, so no test ever opens a real microphone or plays a
// sound.
#[derive(Clone)]
pub enum Devices {
    Windows,
    Fake { microphone: Fake, speakers: Fake },
}

impl fmt::Debug for Devices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Devices::Windows => "Windows",
            Devices::Fake { .. } => "Fake",
        })
    }
}

#[derive(Clone, Debug)]
pub struct VoiceConfig {
    pub input: Choice,
    pub output: Choice,
    pub talk: TalkMode,
    // Padded packets at a constant rate, so their sizes say nothing about the
    // speech. On by default; each person can turn it off for their own
    // stream.
    pub constant_rate: bool,
    pub devices: Devices,
}

impl Default for VoiceConfig {
    fn default() -> VoiceConfig {
        VoiceConfig {
            input: Choice::Default,
            output: Choice::Default,
            talk: TalkMode::PushToTalk,
            constant_rate: true,
            devices: Devices::Windows,
        }
    }
}

// Where the capture thread sends from and to. The voice threads reach the
// room's socket only through this, and Leave swaps in an empty one, so the
// socket closes with the room even while a device on those threads takes
// seconds to close.
#[derive(Default)]
pub(crate) struct Route {
    socket: Option<Arc<Socket>>,
    outlets: Vec<Outlet>,
}

// One per link, with the session's sealer and the address media goes to.
pub(crate) struct Outlet {
    pub sealer: Sealer,
    pub to: SocketAddr,
    pub sent: Arc<Sent>,
}

// Voice sent on one link from the capture thread, which the link's own
// counters under the state lock do not see.
#[derive(Default)]
pub(crate) struct Sent {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl Sent {
    pub(crate) fn add_to(&self, numbers: &mut Numbers) {
        numbers.packets_sent += self.packets.load(Ordering::Relaxed);
        numbers.bytes_sent += self.bytes.load(Ordering::Relaxed);
    }

    pub(crate) fn count(&self, len: usize) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
    }
}

// A device's state for the view: what it opened, or why it is not running.
#[derive(Default)]
struct Status {
    info: Option<StreamInfo>,
    error: Option<AudioError>,
}

// What the render thread needs to time what it plays: where the callback
// under way starts, in samples since the stream began and on the ping clock,
// and how long from being written to leaving the speaker.
#[derive(Default)]
pub(crate) struct RenderNow {
    start_sample: AtomicU64,
    start_us: AtomicU64,
    latency_us: AtomicU64,
    // The render period in samples, for the jitter buffers' minimum depth.
    period: AtomicU32,
}

// The last thousand or so callback times, written by an audio thread
// without waiting and read by the view.
pub(crate) struct Timing {
    micros: [AtomicU32; 1024],
    next: AtomicUsize,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            micros: [const { AtomicU32::new(0) }; 1024],
            next: AtomicUsize::new(0),
        }
    }
}

impl Timing {
    pub(crate) fn record(&self, took: Duration) {
        let at = self.next.fetch_add(1, Ordering::Relaxed) % self.micros.len();
        let micros = took.as_micros().clamp(1, u128::from(u32::MAX)) as u32;
        self.micros[at].store(micros, Ordering::Relaxed);
    }

    pub(crate) fn spread(&self) -> Option<Spread> {
        let mut times: Vec<u32> = self
            .micros
            .iter()
            .map(|micros| micros.load(Ordering::Relaxed))
            .filter(|&micros| micros > 0)
            .collect();
        if times.is_empty() {
            return None;
        }
        times.sort_unstable();
        let at = |share: f32| times[((times.len() - 1) as f32 * share).round() as usize];
        Some(Spread {
            median_ms: at(0.5) as f32 / 1000.0,
            p99_ms: at(0.99) as f32 / 1000.0,
            count: times.len(),
        })
    }
}

// Everything the room, the panel and the audio threads share, behind one
// Arc. The panel and the room write the switches; the audio threads read
// them once a frame.
pub(crate) struct Shared {
    talk: TalkMode,
    constant_rate: bool,
    // A host sends its own voice as Relayed from its own slot, since
    // clients take only that kind.
    host: bool,
    held: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    // The host closed the room, or the socket failed: nobody is left to
    // hear, so the microphone closes and Windows stops showing it in use.
    over: AtomicBool,
    sending: AtomicBool,
    redundancy: AtomicBool,
    repair: AtomicBool,
    expected_loss: AtomicU8,
    // Kept across microphone restarts, so a listener's buffer never sees
    // the numbering go backwards within a room.
    next_seq: AtomicU16,
    frames_sent: AtomicU64,
    // The capture time of the last frame sent, on the ping clock, 0 before
    // the first. It goes to every link at once, so it keeps them all
    // pinging at the media rate.
    last_sent_us: AtomicU64,
    route: Mutex<Arc<Route>>,
    // Leave took the route back: what the audio threads do from here on
    // reaches nothing of the room's.
    left: AtomicBool,
    microphone: Mutex<Status>,
    speakers: Mutex<Status>,
    render: Mutex<Option<Render>>,
    render_now: RenderNow,
    capture_times: Timing,
    render_times: Timing,
    // The share cue waiting for the render thread (cue::pack), 0 for none,
    // and the control cue.
    cue: AtomicU64,
    control_cue: AtomicU64,
    changed: OnceLock<Box<dyn Fn() + Send + Sync>>,
    // Asks the devices thread to open or close the microphone to match.
    follow: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Shared {
    pub(crate) fn new(
        config: &VoiceConfig,
        socket: Option<Arc<Socket>>,
        host: bool,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            talk: config.talk,
            constant_rate: config.constant_rate,
            host,
            held: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            over: AtomicBool::new(false),
            sending: AtomicBool::new(false),
            redundancy: AtomicBool::new(false),
            repair: AtomicBool::new(false),
            expected_loss: AtomicU8::new(0),
            next_seq: AtomicU16::new(random_seq()),
            frames_sent: AtomicU64::new(0),
            last_sent_us: AtomicU64::new(0),
            route: Mutex::new(Arc::new(Route {
                socket,
                outlets: Vec::new(),
            })),
            left: AtomicBool::new(false),
            microphone: Mutex::default(),
            speakers: Mutex::default(),
            render: Mutex::new(None),
            render_now: RenderNow::default(),
            capture_times: Timing::default(),
            render_times: Timing::default(),
            cue: AtomicU64::new(0),
            control_cue: AtomicU64::new(0),
            changed: OnceLock::new(),
            follow: OnceLock::new(),
        })
    }

    // Called from an audio thread when what the view shows changed.
    pub(crate) fn on_change(&self, changed: impl Fn() + Send + Sync + 'static) {
        let _ = self.changed.set(Box::new(changed));
    }

    fn changed(&self) {
        if self.left.load(Ordering::Relaxed) {
            return;
        }
        if let Some(changed) = self.changed.get() {
            changed();
        }
    }

    // Leave, before anything else: the capture thread has nothing to send
    // on from here, and lets go of the socket once the frame under way, if
    // any, is out, so the socket closes with the room whatever the devices
    // thread is waiting for.
    pub(crate) fn let_go(&self) {
        self.left.store(true, Ordering::Relaxed);
        *lock(&self.route) = Arc::default();
    }

    // Read by the devices thread only, never per frame: a mute or unmute
    // queued before Leave opens nothing after it.
    pub(crate) fn left(&self) -> bool {
        self.left.load(Ordering::Relaxed)
    }

    // `at_us` is now on the ping clock. A newer cue replaces one the render
    // thread has not taken yet.
    pub(crate) fn cue(&self, cue: Cue, at_us: u64) {
        self.cue.store(cue::pack(cue, at_us), Ordering::Relaxed);
    }

    // On the render thread, once a period.
    fn take_cue(&self, now_us: u64) -> Option<Cue> {
        cue::unpack(self.cue.swap(0, Ordering::Relaxed), now_us)
    }

    // The control cue, in a slot of its own, so neither cue takes the
    // other's place.
    pub(crate) fn control_cue(&self, cue: Cue, at_us: u64) {
        self.control_cue
            .store(cue::pack(cue, at_us), Ordering::Relaxed);
    }

    fn take_control_cue(&self, now_us: u64) -> Option<Cue> {
        cue::unpack(self.control_cue.swap(0, Ordering::Relaxed), now_us)
    }

    pub(crate) fn set_held(&self, held: bool) {
        self.held.store(held, Ordering::Relaxed);
    }

    // Both return whether the microphone should run now.
    pub(crate) fn set_muted(&self, muted: bool) -> bool {
        self.muted.store(muted, Ordering::Relaxed);
        // Unmute undeafens too: with the speakers off nobody would know
        // their voice was going out.
        if !muted {
            self.deafened.store(false, Ordering::Relaxed);
        }
        self.microphone_wanted()
    }

    pub(crate) fn set_deafened(&self, deafened: bool) -> bool {
        self.deafened.store(deafened, Ordering::Relaxed);
        self.microphone_wanted()
    }

    pub(crate) fn microphone_wanted(&self) -> bool {
        !self.muted.load(Ordering::Relaxed)
            && !self.deafened.load(Ordering::Relaxed)
            && !self.over.load(Ordering::Relaxed)
    }

    pub(crate) fn on_follow(&self, follow: impl Fn() + Send + Sync + 'static) {
        let _ = self.follow.set(Box::new(follow));
    }

    pub(crate) fn room_over(&self) {
        if !self.over.swap(true, Ordering::Relaxed)
            && let Some(follow) = self.follow.get()
        {
            follow();
        }
    }

    pub(crate) fn over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }

    pub(crate) fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub(crate) fn deafened(&self) -> bool {
        self.deafened.load(Ordering::Relaxed)
    }

    pub(crate) fn sending(&self) -> bool {
        self.sending.load(Ordering::Relaxed)
    }

    fn route(&self) -> Arc<Route> {
        Arc::clone(&lock(&self.route))
    }

    pub(crate) fn view(&self) -> VoiceView {
        let wanted = self.microphone_wanted();
        VoiceView {
            mode: self.talk,
            // Deafened is muted too, so the row reads Unmute then, and
            // Unmute undeafens as well.
            muted: self.muted() || self.deafened(),
            deafened: self.deafened(),
            sending: self.sending(),
            microphone: lock(&self.microphone).error.clone().filter(|_| wanted),
            speakers: lock(&self.speakers).error.clone(),
        }
    }

    // This PC's side of the audio numbers, for the view and for the far side.
    pub(crate) fn periods(&self) -> Periods {
        let period = |status: &Mutex<Status>| lock(status).info.as_ref().map(|info| info.period);
        // To a tenth of a millisecond: the queued part moves a little with
        // every write, and the far side is told only when this changes.
        let latency = lock(&self.render)
            .as_ref()
            .and_then(Render::latency)
            .map(|latency| Duration::from_micros((latency.as_micros() as u64 + 50) / 100 * 100));
        if let Some(latency) = latency {
            self.render_now
                .latency_us
                .store(latency.as_micros() as u64, Ordering::Relaxed);
        }
        Periods {
            input: period(&self.microphone),
            output: period(&self.speakers),
            render_latency: latency,
        }
    }

    fn microphone(&self) -> Option<Microphone> {
        lock(&self.microphone)
            .info
            .as_ref()
            .map(|info| Microphone::new(&info.engine, info.hands_free))
    }
}

fn random_seq() -> u16 {
    let mut bytes = [0u8; 2];
    let _ = getrandom::fill(&mut bytes);
    u16::from_le_bytes(bytes)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// One talker as the room sees them.
struct Talker {
    key: [u8; 32],
    ear: Arc<Mutex<Ear>>,
    last_packet: Instant,
    // Their last packet said it was the last.
    ended: bool,
    // What their packets say about the way here, for the strip when they
    // are at the other end of the link (on_link).
    stream: StreamStats,
}

// What the voice or video of the person at the other end of a link says
// about it over the last 2 s. No jitter while no send time can be read yet:
// for voice, before the clock offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OnLink {
    pub jitter_ms: Option<f32>,
    pub loss_pct: f32,
}

// The room's side of voice, owned by the host or the client under the state
// lock: the talkers this PC hears, whether they are talking, the loss
// reports, and the links the capture thread sends on.
pub(crate) struct Talk {
    shared: Arc<Shared>,
    clock: Clock,
    timers: Timers,
    log: Log,
    talkers: Vec<Talker>,
    retired: Vec<(Arc<Mutex<Ear>>, Instant)>,
    speaker: Sender<ToSpeaker>,
    adapt: adapt::Adapt,
    // What listeners last said of this PC's voice, as the room last put it
    // together.
    own_loss: Option<VoiceLoss>,
    next_report: Instant,
    // Which sessions and addresses the capture thread has now.
    published: Vec<(u32, SocketAddr)>,
    // The talker heard most recently, for the numbers.
    latest: Option<[u8; 32]>,
    dropped: u64,
}

impl Talk {
    pub(crate) fn new(
        shared: Arc<Shared>,
        speaker: Sender<ToSpeaker>,
        clock: Clock,
        timers: Timers,
        log: Log,
        now: Instant,
    ) -> Talk {
        Talk {
            shared,
            clock,
            timers,
            log,
            talkers: Vec::new(),
            retired: Vec::new(),
            speaker,
            adapt: adapt::Adapt::new(),
            own_loss: None,
            next_report: now + timers.voice_report_every,
            published: Vec::new(),
            latest: None,
            dropped: 0,
        }
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    // Hands the capture thread a new list of links when the sessions or
    // addresses changed. Called after everything the room does under its
    // lock, so it compares the session each link sends on and where, and
    // makes the sealers only when those changed.
    pub(crate) fn publish(
        &mut self,
        links: impl Iterator<Item = (u32, SocketAddr)> + Clone,
        outlets: impl FnOnce() -> Vec<Outlet>,
    ) {
        // Compared as they come, since this runs for every packet and a
        // list made each time would allocate as often.
        if links.clone().eq(self.published.iter().copied()) {
            return;
        }
        self.published = links.collect();
        let outlets = outlets();
        // The socket is read and the route swapped under one lock, so a
        // route Leave took back never gets its socket again.
        let mut route = lock(&self.shared.route);
        let socket = route.socket.clone();
        *route = Arc::new(Route { socket, outlets });
    }

    // A frame from `key`, checked. `captured` is its capture time on this
    // PC's ping clock, when known. True when that changed who is talking.
    pub(crate) fn hear(
        &mut self,
        key: [u8; 32],
        frame: &Frame,
        captured: Option<u64>,
        about: bool,
        now: Instant,
    ) -> bool {
        let at = match self.talkers.iter().position(|talker| talker.key == key) {
            Some(at) => at,
            None => match Ear::new() {
                Ok(ear) => {
                    let ear = Arc::new(Mutex::new(ear));
                    let _ = self.speaker.send(ToSpeaker::Add(key, Arc::clone(&ear)));
                    self.talkers.push(Talker {
                        key,
                        ear,
                        last_packet: now,
                        ended: true,
                        stream: StreamStats::new(),
                    });
                    self.talkers.len() - 1
                }
                Err(err) => {
                    log!(
                        self.log,
                        "voice: could not make a decoder for {}: {err}",
                        keys::fingerprint(&key)
                    );
                    return false;
                }
            },
        };
        let talker = &mut self.talkers[at];
        let was_talking = talker.is_talking(now, self.timers.talking_for);
        talker
            .stream
            .record(frame.seq, captured, self.clock.micros(now), now);
        if !lock(&talker.ear).push(frame, captured, about) {
            self.dropped += 1;
        }
        talker.last_packet = now;
        talker.ended = frame.last;
        self.latest = Some(key);
        was_talking != talker.is_talking(now, self.timers.talking_for)
    }

    pub(crate) fn dropped(&mut self) {
        self.dropped += 1;
    }

    // Someone left: their buffer leaves the mixer.
    pub(crate) fn forget(&mut self, key: &[u8; 32], now: Instant) {
        if let Some(at) = self.talkers.iter().position(|talker| talker.key == *key) {
            let talker = self.talkers.swap_remove(at);
            let _ = self.speaker.send(ToSpeaker::Remove(talker.key));
            self.retired.push((talker.ear, now + RETIRED_FOR));
        }
    }

    pub(crate) fn keep_only(&mut self, keys: &[[u8; 32]], now: Instant) {
        let gone: Vec<[u8; 32]> = self
            .talkers
            .iter()
            .map(|talker| talker.key)
            .filter(|key| !keys.contains(key))
            .collect();
        for key in gone {
            self.forget(&key, now);
        }
    }

    pub(crate) fn talking(&self, key: &[u8; 32], now: Instant) -> bool {
        self.talkers
            .iter()
            .any(|talker| talker.key == *key && talker.is_talking(now, self.timers.talking_for))
    }

    // `far_end` is the person at the other end of the link: on the host the
    // friend, on a client the host. A client hears its friends through the
    // host too, but their way to the host is theirs and not this PC's, and
    // the strip shows this PC's own path. What was lost of their voice still
    // shows under Voice loss in the stats panel.
    pub(crate) fn on_link(&self, far_end: &[u8; 32], now: Instant) -> Option<OnLink> {
        let talker = self.talkers.iter().find(|talker| talker.key == *far_end)?;
        Some(OnLink {
            jitter_ms: talker.stream.jitter_ms(),
            loss_pct: talker.stream.loss(now)?.percent()?,
        })
    }

    // This PC's own voice went out lately, to every link.
    pub(crate) fn sent_lately(&self, now: Instant) -> bool {
        let last = self.shared.last_sent_us.load(Ordering::Relaxed);
        let since = self.clock.micros(now).saturating_sub(last);
        self.shared.sending() || since < MEDIA_FLOWS_FOR.as_micros() as u64
    }

    // `reporting`: someone is there to report to or hear reports from.
    pub(crate) fn next_deadline(&self, reporting: bool) -> Option<Instant> {
        let mut soonest = crate::Soonest(reporting.then_some(self.next_report));
        for talker in &self.talkers {
            if !talker.ended {
                soonest.add(Some(talker.last_packet + self.timers.talking_for));
            }
        }
        soonest.add(self.retired.iter().map(|(_, until)| *until).min());
        soonest.0
    }

    // True when someone stopped talking. The caller sends the reports when
    // report_due says so.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        self.retired.retain(|(_, until)| now < *until);
        let talking_for = self.timers.talking_for;
        let mut changed = false;
        for talker in &mut self.talkers {
            if !talker.ended && now >= talker.last_packet + talking_for {
                talker.ended = true;
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn report_due(&mut self, now: Instant) -> bool {
        if now < self.next_report {
            return false;
        }
        self.next_report = now + self.timers.voice_report_every;
        true
    }

    // What this PC lost of each talker heard over the last 2 s.
    pub(crate) fn losses(&self, now: Instant) -> Vec<([u8; 32], VoiceLoss)> {
        self.talkers
            .iter()
            .filter(|talker| now.saturating_duration_since(talker.last_packet) < HEARD_LATELY)
            .map(|talker| (talker.key, lock(&talker.ear).loss()))
            .collect()
    }

    // The worst loss any listener reported for this PC's own voice. Only the
    // scattered part switches anything: an outage is no reason to send more
    // or wait longer once it is over.
    pub(crate) fn listeners_lost(&mut self, loss: VoiceLoss, now: Instant) {
        self.own_loss = Some(loss);
        for switch in self.adapt.report(loss.scattered_pct, now, &self.timers) {
            match switch {
                adapt::Switch::RedundancyOn { loss } => log!(
                    self.log,
                    "voice: redundancy on, a listener lost {loss:.1} percent one or two frames at a time over the last 2 s"
                ),
                adapt::Switch::RedundancyOff { clean_for } => log!(
                    self.log,
                    "voice: redundancy off, no scattered loss reported for {:.0} s",
                    clean_for.as_secs_f32()
                ),
                adapt::Switch::RepairOn { loss, for_at_least } => log!(
                    self.log,
                    "voice: 10 ms frames with opus repair data, a listener lost {loss:.1} percent one or two frames at a time, 5 or more for {:.0} s",
                    for_at_least.as_secs_f32()
                ),
                adapt::Switch::RepairOff { loss, under_for } => log!(
                    self.log,
                    "voice: back to 5 ms frames, {loss:.1} percent scattered loss, under 1 for {:.0} s",
                    under_for.as_secs_f32()
                ),
            }
        }
        let shared = &self.shared;
        shared
            .redundancy
            .store(self.adapt.redundancy(), Ordering::Relaxed);
        shared
            .repair
            .store(self.adapt.mode() == Mode::Repair, Ordering::Relaxed);
        shared
            .expected_loss
            .store(self.adapt.expected_loss(), Ordering::Relaxed);
    }

    pub(crate) fn periods(&self) -> Periods {
        self.shared.periods()
    }

    // The numbers the stats panel shows about voice. `name` gives a key's
    // name; `loss` a talker's loss as the stats should show it, which on the
    // host is the worst any listener lost.
    pub(crate) fn fill(
        &self,
        numbers: &mut Numbers,
        name: impl Fn(&[u8; 32]) -> Option<String>,
        loss: impl Fn(&[u8; 32], VoiceLoss) -> VoiceLoss,
        far: Option<Periods>,
        now: Instant,
    ) {
        let own = self.shared.periods();
        numbers.audio_in_ms = own.input.map(millis);
        numbers.audio_out_ms = own.output.map(millis);
        numbers.render_latency_ms = own.render_latency.map(millis);
        numbers.microphone = self.shared.microphone();
        if let Some(far) = far {
            numbers.far_audio_in_ms = far.input.map(millis);
            numbers.far_audio_out_ms = far.output.map(millis);
            numbers.far_render_latency_ms = far.render_latency.map(millis);
        }
        let repair = self.shared.repair.load(Ordering::Relaxed);
        numbers.send_frame_ms = if repair { 10 } else { 5 };
        numbers.send_repair_copy = !repair && self.shared.redundancy.load(Ordering::Relaxed);
        numbers.own_voice_loss = self.own_loss;
        numbers.voice_sent = self.shared.frames_sent.load(Ordering::Relaxed);
        numbers.voice_dropped = self.dropped;
        numbers.capture_callback = self.shared.capture_times.spread();
        numbers.render_callback = self.shared.render_times.spread();
        numbers.voice_loss = self
            .losses(now)
            .into_iter()
            .filter_map(|(key, lost)| Some((name(&key)?, loss(&key, lost))))
            .collect();

        let Some(key) = self.latest else {
            return;
        };
        let Some(talker) = self.talkers.iter().find(|talker| talker.key == key) else {
            return;
        };
        let Some(who) = name(&key) else {
            return;
        };
        // Made before the lock, so the render thread never waits on an
        // allocation, and sorted after it.
        let mut recent = Vec::with_capacity(ear::SAMPLES_KEPT);
        let (stats, heard) = {
            let ear = lock(&talker.ear);
            (ear.stats(), ear.heard(self.clock.micros(now), &mut recent))
        };
        numbers.buffer = Some(Buffer {
            name: who.clone(),
            ms: stats.depth_ms,
            frames: stats.depth_frames,
        });
        numbers.mouth_to_ear = heard.map(|heard| MouthToEar {
            name: who,
            ..ear::mouth_to_ear(heard, &mut recent)
        });
    }
}

impl Talker {
    fn is_talking(&self, now: Instant, talking_for: Duration) -> bool {
        !self.ended && now < self.last_packet + talking_for
    }
}

// A loss in percent as the control messages carry it, and back.
pub(crate) fn to_wire(loss: VoiceLoss) -> LossPermille {
    let permille = |percent: f32| (percent * 10.0).round().clamp(0.0, f32::from(PERMILLE)) as u16;
    let all = permille(loss.all_pct);
    LossPermille {
        all,
        scattered: permille(loss.scattered_pct).min(all),
    }
}

pub(crate) fn from_wire(lost: LossPermille) -> VoiceLoss {
    let percent = |permille: u16| f32::from(permille.min(PERMILLE)) / 10.0;
    VoiceLoss {
        all_pct: percent(lost.all),
        scattered_pct: percent(lost.scattered),
    }
}

// Several listeners' word on one talker: the worst of each number, which may
// come from different listeners.
pub(crate) fn worse(a: VoiceLoss, b: VoiceLoss) -> VoiceLoss {
    VoiceLoss {
        all_pct: a.all_pct.max(b.all_pct),
        scattered_pct: a.scattered_pct.max(b.scattered_pct),
    }
}

// A capture time on a peer's ping clock, on this PC's, and whether it is
// only about right: chat delivery's rules.
pub(crate) fn our_time(at: u64, offset_us: Option<i64>, jittery: bool) -> (Option<u64>, bool) {
    let Some(offset) = offset_us else {
        return (None, false);
    };
    (Some(chat::to_our_clock(at, offset)), jittery)
}

fn millis(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deafen also mutes, so the row reads Unmute and Undeafen, and Unmute
    // gives back both.
    #[test]
    fn deafen_mutes_and_unmute_undeafens() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        assert!(!shared.set_deafened(true));
        let view = shared.view();
        assert!(view.muted && view.deafened);
        assert!(shared.set_muted(false));
        let view = shared.view();
        assert!(!view.muted && !view.deafened);

        // Muted before deafening, still muted after undeafening.
        shared.set_muted(true);
        shared.set_deafened(true);
        assert!(!shared.set_deafened(false));
        let view = shared.view();
        assert!(view.muted && !view.deafened);
    }

    // An outage is loss in a long run, which neither the repair copy nor the
    // 10 ms mode brings back. Reported for 3 s at 20 percent, it switches
    // nothing; a mix with 6 percent scattered turns both on, on that part
    // alone.
    #[test]
    fn only_scattered_loss_switches() {
        let start = Instant::now();
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let (speaker, _orders) = crossbeam_channel::unbounded();
        let (log, captured) = Log::capture(16);
        let clock = Clock::new(start);
        let mut talk = Talk::new(shared, speaker, clock, Timers::default(), log, start);
        let at = |s: u64| start + Duration::from_secs(s);
        let loss = |all_pct, scattered_pct| VoiceLoss {
            all_pct,
            scattered_pct,
        };
        for s in 0..=3 {
            talk.listeners_lost(loss(20.0, 0.0), at(s));
        }
        let switched = |talk: &Talk| {
            let shared = talk.shared();
            (
                shared.redundancy.load(Ordering::Relaxed),
                shared.repair.load(Ordering::Relaxed),
            )
        };
        assert_eq!(switched(&talk), (false, false));
        assert!(captured.lines().is_empty());

        for s in 4..=6 {
            talk.listeners_lost(loss(26.0, 6.0), at(s));
        }
        assert_eq!(switched(&talk), (true, true));
        assert_eq!(talk.shared().expected_loss.load(Ordering::Relaxed), 6);
        assert_eq!(
            captured.lines(),
            [
                "voice: redundancy on, a listener lost 6.0 percent one or two frames at a time over the last 2 s",
                "voice: 10 ms frames with opus repair data, a listener lost 6.0 percent one or two frames at a time, 5 or more for 2 s",
            ]
        );
    }

    #[test]
    fn loss_on_the_wire() {
        let wire = to_wire(VoiceLoss {
            all_pct: 12.34,
            scattered_pct: 12.36,
        });
        assert_eq!(
            wire,
            LossPermille {
                all: 123,
                scattered: 123
            }
        );
        let back = from_wire(LossPermille {
            all: 55,
            scattered: 5,
        });
        assert_eq!(
            back,
            VoiceLoss {
                all_pct: 5.5,
                scattered_pct: 0.5
            }
        );
        let worst = worse(
            VoiceLoss {
                all_pct: 30.0,
                scattered_pct: 0.0,
            },
            back,
        );
        assert_eq!(
            worst,
            VoiceLoss {
                all_pct: 30.0,
                scattered_pct: 0.5
            }
        );
    }

    #[test]
    fn room_over_closes_microphone() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let asked = Arc::new(AtomicUsize::new(0));
        shared.on_follow({
            let asked = Arc::clone(&asked);
            move || {
                asked.fetch_add(1, Ordering::Relaxed);
            }
        });
        assert!(shared.microphone_wanted());
        shared.room_over();
        shared.room_over();
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        assert!(!shared.microphone_wanted());
        assert!(!shared.set_muted(false), "Unmute cannot open it again");
    }
}
