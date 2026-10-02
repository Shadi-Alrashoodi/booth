// Screen sharing in a room. One share at a time (one viewer window, one
// upload budget), granted by the host, and video goes only to people who
// pressed Watch. Four threads touch it, and only the room's own two ever
// take the room's state lock:
//
// - The sharer's thread (sharer.rs, and the share crate's pacer under it)
//   seals each video and pointer packet itself through an Outbox, with the
//   session's Sealer, to the links in Sharing's outlet list. The room swaps
//   that list when a session, an address, the share or its watchers change,
//   as it does for voice. Sharing's signal wakes it when a share of this
//   PC's starts, and when someone starts watching one that nobody watched,
//   which captures nothing meanwhile; while it runs it reads the answers
//   and facts before each frame.
// - The receive thread, under the state lock as for every packet: the host
//   checks a sharer's video packet, turns its prefix around in place and
//   seals it once per watcher; a watcher checks what the host passed on and
//   puts it in the open viewer's inbox, which never blocks it: a full inbox
//   drops and counts. With no video threads (VideoSource::Hooks) it waits
//   in Watching's own inbox for a test to take.
// - The viewer's thread (viewer.rs) decodes and shows what its inbox has,
//   and hands recover requests, IDR asks and its loss back through
//   Watching.
// - The timer thread, under the state lock, sends what the two hooks queue
//   (shapes, recover requests, IDR asks, loss) as control messages, the
//   reports that go once a second, and acts on what the two video threads
//   report: a share that could not start ends, a viewer closed stops
//   watching.

mod sharer;
mod viewer;
pub(crate) mod wire;

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use channels::Channel;
use channels::reliable::MAX_QUEUED;
use net::pace::Signal;
use share::rate::RoundTrip;
use share::{Knob, MonitorId};
use stats::{STREAM_WINDOW, StreamLoss, StreamStats};

use crate::config::LossKnob;
use crate::control::{Facts, MAX_RECOVER_SPAN, PERMILLE};
use crate::limit::Bucket;
use crate::peer::{Clock, MEDIA_FLOWS_FOR};
use crate::remote::{Area, Controls, Gate, Injector, Remote};
use crate::socket::Socket;
use crate::talk::{Cue, HOST_SLOT, OnLink, Outlet};
use crate::view::{Numbers, Paused, RunningShare, SharingNumbers, WatchingNumbers};

pub(crate) use sharer::start as start_sharer;
pub(crate) use viewer::{control_for, link_numbers, start as start_viewer};

pub use wire::{
    INTERNET_DATAGRAM, LAN_DATAGRAM, MAX_SHAPE_BYTES, MAX_SHAPE_SIDE, Pointer, SHAPE_CHUNK, Shape,
    ShapeError, ShapeKind,
};

use wire::{Assembler, MAX_VIDEO, PREFIX, RELAYED, SENT};

// The host's limits on each friend: anyone in the room may be a stranger
// with a leaked invite, or a friend whose PC runs someone else's program.
//
// Video: 80 Mbit/s, the most the upload setting takes, at 120 fps, the most
// capture takes, is 73 data shards of 1152 bytes a frame and 37 parity at
// the 50 percent ceiling: 13 200 packets a second, and 13 080 at 60 fps. A
// watcher's reports can add IDRs worth up to a quarter of the setting on
// top (share's floor between IDRs) and hold the parity at its ceiling, so a
// sharer may send 100 Mbit/s without breaking a rule: 91 data shards and 46
// parity a frame, 16 440 packets a second, and at most 16 519 at any rate
// up to 120 fps with frames of mixed sizes. A watcher looping Watch and
// Stop watching also gets an IDR every 500 ms (IDR_ASK_EVERY) that the
// floor does not hold. At 6.5 frames' worth, the most NVIDIA's encoders
// were measured at, that is 707 packets at 120 fps and 1412 at 60, for
// 17 854 and 19 144 a second in all, and at most 19 765 from 50 fps up. The
// limit is 20 000, so with IDRs that size no one watcher can make the host
// cut the picture for everyone.
// Bigger IDRs, or several friends looping at once, can still go over: the
// host's drops then reach the other watchers as loss, and the sharer backs
// off 20 percent a step until it fits. One step makes room for even the
// largest IDR there can be, twice a second (19 408 at most).
// The burst is the largest frame the packet format carries, 2048 data
// shards with 1024 parity, the worst IDR there can be.
pub(crate) const VIDEO_PER_SECOND: f64 = 20_000.0;
pub(crate) const VIDEO_BURST: f64 = 3072.0;
// A pointer moves at most once a frame of a 240 Hz monitor; twice that,
// with half a second of it as the burst.
pub(crate) const POINTER_PER_SECOND: f64 = 480.0;
pub(crate) const POINTER_BURST: f64 = 120.0;
// Shapes are relayed to every watcher over the control channel. A pointer
// changes shape a few times a second at most, and a common one is 4 to 9 KB,
// so 64 KB a second, with one of the largest and a common one as the burst.
pub(crate) const SHAPE_BYTES_PER_SECOND: f64 = 65_536.0;
pub(crate) const SHAPE_BYTES_BURST: f64 = (MAX_SHAPE_BYTES + 32 * 1024) as f64;
// A sharer holds its own shapes to a little less: 7/8 of the rate and 16 KB
// less burst. The control channel can deliver two shapes' first chunks
// closer together than they were sent, after a resend, and 16 KB is a
// quarter second of the host's rate, so the host never drops a shape a
// sharer sent within its own budget. A shape over it waits, and a newer one
// replaces it.
pub(crate) const SHAPE_SEND_PER_SECOND: f64 = SHAPE_BYTES_PER_SECOND * 7.0 / 8.0;
pub(crate) const SHAPE_SEND_BURST: f64 = (MAX_SHAPE_BYTES + 16 * 1024) as f64;
// A shape goes on a control channel only while the whole of it leaves half
// the channel's queue free, so what comes behind it always finds room; a
// shape that does not fit tries again this much later.
const SHAPE_QUEUE_ROOM: usize = MAX_QUEUED / 2;
const SHAPE_RETRY: Duration = Duration::from_millis(50);
// Asks to share: each one refused is answered and each one granted changes
// the roster for everyone.
pub(crate) const SHARE_ASKS_PER_SECOND: f64 = 1.0;
pub(crate) const SHARE_ASK_BURST: f64 = 4.0;
// Watch and Stop watching: each start asks the sharer for an IDR, which the
// IDR gap below holds to one per watcher, and each changes the facts the
// sharer hears, which FACTS_EVERY holds back. Past this a press still takes
// effect, but writes no log line and waits for the next view.
pub(crate) const WATCHES_PER_SECOND: f64 = 2.0;
pub(crate) const WATCH_BURST: f64 = 6.0;
// A viewer gathers the frames it drops for 20 ms into one request, and asks
// once more for each frame that would not decode: 50 a second and the odd
// failed decode, twice over.
pub(crate) const RECOVERS_PER_SECOND: f64 = 100.0;
pub(crate) const RECOVER_BURST: f64 = 50.0;
// A viewer's loss goes once a second, and its first nonzero loss at once.
pub(crate) const LOSSES_PER_SECOND: f64 = 4.0;
pub(crate) const LOSS_BURST: f64 = 4.0;
// An IDR is about six frames' worth at 15 Mbit/s, so each watcher gets at
// most one every half second, its Watch included: a friend asking in a loop
// costs the sharer two IDRs a second, not one per frame.
// The time is kept by key, so leaving and joining again does not reset it.
pub(crate) const IDR_ASK_EVERY: Duration = Duration::from_millis(500);
// Watch and Stop watching past their limit still take effect, so the host
// and the watcher never disagree about who watches. What they tell the
// sharer, which changes its encoder's rate, goes at most this often; the
// first change in a while goes at once.
pub(crate) const FACTS_EVERY: Duration = Duration::from_millis(250);

// What reaches Watching's inbox and waits there for the viewer's thread, as
// a socket's receive buffer would hold it: about 4.8 MB, half a second at
// 80 Mbit/s. Past it packets are dropped and counted.
const MAX_WAITING: usize = 4096;
const MAX_SPARE: usize = 256;
// Shapes finished and not taken yet. The newest win.
const MAX_SHAPES_WAITING: usize = 4;
// Starts, ends and frame rates not taken yet, for a viewer thread that stopped
// taking: the newest are kept.
const MAX_EVENTS_WAITING: usize = 64;
// Recover requests not taken yet, for a sharer's thread that stalled. The
// host already merges what several watchers send, so a stream that loses
// frames often has a few waiting at most; past this the oldest go, and the
// viewer's IDR ask is what brings the picture back then.
const MAX_RECOVERS_WAITING: usize = 64;

// Frames of the far end's share whose packets are still coming, for the
// strip's loss: a frame's last packets can arrive after the next frame's
// first. A frame is counted when a newer one pushes it out, or once it has
// waited this long.
const OPEN_FRAMES: usize = 4;
const OPEN_FOR: Duration = Duration::from_millis(250);
// Frames counted over STREAM_WINDOW, 2 s at 240 frames a second twice over.
// This only bounds a flood.
const MAX_COUNTED: usize = 1024;

// This PC's own share, for the panel and the sharer's thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OwnShare {
    #[default]
    Off,
    // A client asked the host and waits for the answer. Nothing is
    // captured before it.
    Asking {
        fps: u8,
    },
    Sharing {
        number: u32,
        fps: u8,
    },
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    // Someone else shares.
    Busy { name: String },
    // Asked again too soon after the last ask.
    TooSoon,
    // The host was lost or the room closed before it answered.
    NotLive,
}

impl Refusal {
    // What the panel shows, as a system line in the chat.
    pub fn sentence(&self) -> String {
        match self {
            Refusal::Busy { name } => format!("{name} is sharing. One share at a time."),
            Refusal::TooSoon => String::from(
                "Could not start sharing: asked too often. Wait a second and try again.",
            ),
            Refusal::NotLive => String::from(
                "Could not start sharing: the host cannot be reached. Try again once it is back.",
            ),
        }
    }
}

// What the host says the sharer's encoder and send thread need: the rate for
// the bitrate rule, the paths for the pacer and the datagram size, and
// whether the share may go in HEVC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareFacts {
    // Everyone watching, and those of them on an internet path.
    pub watchers: u8,
    pub internet: u8,
    // The rate the encoder may use: this PC's own upload setting, or the
    // host's setting divided by the internet watchers when that is less.
    pub rate_kbps: u32,
    // Some watcher is on an internet path: spread each frame's packets.
    pub spread: bool,
    // Every link the share goes over is on the LAN: 1400-byte datagrams.
    pub lan: bool,
    // Every watcher decodes HEVC, so the share may go in it; true while
    // nobody watches.
    pub hevc: bool,
}

impl ShareFacts {
    // What Packetizer::new takes: a datagram less the session's overhead,
    // the channel byte and the room's prefix.
    pub fn payload(&self) -> usize {
        payload(if self.lan {
            LAN_DATAGRAM
        } else {
            INTERNET_DATAGRAM
        })
    }
}

fn payload(datagram: usize) -> usize {
    datagram - session::DATA_OVERHEAD - 1 - PREFIX
}

// What came back for the share under way, oldest first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Answer {
    // Frames `first` to `last`, wrapping, were lost for some watcher.
    Recover { first: u32, last: u32 },
    // A watcher has nothing it can decode after frame `seen`, or started
    // watching (None): the next frame should be an IDR.
    Idr { seen: Option<u32> },
    // The worst shard loss any watcher had over the last 2 s, in percent,
    // for each frame's parity (channels::video::parity_for_loss).
    Loss(Option<f32>),
}

// Where the sharer's threads send from, and what comes back to them.
pub struct Sharing {
    clock: Clock,
    // A host's packets go straight to the watchers, as Relayed from its own
    // slot; a client's go to the host as Sent.
    host: bool,
    socket: Option<Arc<Socket>>,
    upload_kbps: u32,
    state: Mutex<Own>,
    told: Mutex<Told>,
    // Set when an answer, new facts or a new state waits.
    wake: Signal,
    outlets: Mutex<Arc<[Outlet]>>,
    // Bumped whenever the outlets change, so an Outbox takes the list again
    // only then.
    generation: AtomicU64,
    // The newest shape waiting to go out over the control channel.
    shape: Mutex<Option<(u32, Shape)>>,
    next_shape: AtomicU32,
    next_pointer: AtomicU32,
    video_sent: AtomicU64,
    // When the last video packet left, on the ping clock, 0 before the
    // first. It keeps every link pinging at the media rate.
    last_sent_us: AtomicU64,
    // The newest frame that went out, NO_FRAME before the first: a recover
    // request can only be about a frame a watcher could have had.
    newest_frame: AtomicU64,
    // Asks the timer thread to send what waits.
    work: OnceLock<Box<dyn Fn() + Send + Sync>>,
    // The monitor Room::share asked for, which the share's thread opens.
    // None is the primary one.
    monitor: Mutex<Option<MonitorId>>,
    // The room is closing: the share's thread ends.
    closing: AtomicBool,
    // What the share's thread has for the room, taken on the timer thread.
    news: Mutex<Vec<ShareNews>>,
    running: Mutex<Option<RunningShare>>,
    numbers: Mutex<Option<SharingNumbers>>,
    // New numbers or news since the view was last made.
    fresh: AtomicBool,
    // The round trip on the share's links, for the rate's backoff, until
    // the share's thread takes it. The room measures it once a second on
    // its own clock, and a reading read twice would count twice.
    round_trip: Mutex<Option<RoundTrip>>,
    // Plays the share cue in this PC's own speakers.
    cue: OnceLock<Box<dyn Fn(Cue) + Send + Sync>>,
    // The shared monitor on this PC's desktop, while a share of one runs:
    // where a controller's points land, and where the indicator goes.
    area: Mutex<Option<Area>>,
}

// This PC's own share, and the one the rising cue played for until the
// falling one plays. One lock for both, so a share the room ends while its
// thread is still opening it gets both cues or neither. The cues are asked
// for under it too: the speakers keep only the newest, and a cue asked for
// after the lock was let go could land after a later one and leave a share
// that ended sounding started.
#[derive(Default)]
struct Own {
    share: OwnShare,
    cued: Option<u32>,
}

// What the host told the share's thread: the facts, and the answers since
// the thread last took them. One lock for both, so a new watcher's IDR ask
// and the facts that count it are read together (control::Facts::idr).
#[derive(Default)]
struct Told {
    facts: Option<ShareFacts>,
    answers: Vec<Answer>,
}

const NO_FRAME: u64 = u64::MAX;

// What the share's thread tells the room about the share `share`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ShareNews {
    // It could not start, or stopped on an error. `ran` says which. The room
    // ends the share and says why.
    Failed { share: u32, ran: bool, why: String },
    // The frame rate it runs at now, stepped down or fitted to the software
    // encoder, for the watchers' reassemblers.
    Fps { share: u32, fps: u8 },
    // It runs on Windows' software encoder, at up to 1080p60, from the
    // start or from when its GPU encoder failed, with the sentence for why
    // that the panel shows.
    Software { share: u32, sentence: &'static str },
    Paused { share: u32, paused: Paused },
}

impl Sharing {
    fn new(
        clock: Clock,
        host: bool,
        socket: Option<Arc<Socket>>,
        upload_kbps: u32,
        wake: Signal,
    ) -> Arc<Sharing> {
        Arc::new(Sharing {
            clock,
            host,
            socket,
            upload_kbps,
            state: Mutex::default(),
            told: Mutex::default(),
            wake,
            outlets: Mutex::new(Arc::new([])),
            generation: AtomicU64::new(0),
            shape: Mutex::default(),
            next_shape: AtomicU32::new(1),
            next_pointer: AtomicU32::new(0),
            video_sent: AtomicU64::new(0),
            last_sent_us: AtomicU64::new(0),
            newest_frame: AtomicU64::new(NO_FRAME),
            work: OnceLock::new(),
            monitor: Mutex::default(),
            closing: AtomicBool::new(false),
            news: Mutex::default(),
            running: Mutex::default(),
            numbers: Mutex::default(),
            fresh: AtomicBool::new(false),
            round_trip: Mutex::default(),
            cue: OnceLock::new(),
            area: Mutex::default(),
        })
    }

    pub(crate) fn on_work(&self, work: impl Fn() + Send + Sync + 'static) {
        let _ = self.work.set(Box::new(work));
    }

    pub(crate) fn on_cue(&self, cue: impl Fn(Cue) + Send + Sync + 'static) {
        let _ = self.cue.set(Box::new(cue));
    }

    // Called with the state lock held, so it must take no lock of the
    // room's: in the room it only stores the cue for the render thread.
    fn play(&self, cue: Cue) {
        if let Some(play) = self.cue.get() {
            play(cue);
        }
    }

    fn wake_room(&self) {
        if let Some(work) = self.work.get() {
            work();
        }
    }

    pub fn state(&self) -> OwnShare {
        lock(&self.state).share.clone()
    }

    // Set whenever an answer, new facts or a new state waits. The share's
    // thread waits on it for a share of this PC's to start, and while one
    // runs with nobody watching; otherwise it takes what waits before each
    // frame, and capture wakes it at least every 100 ms on a still screen.
    pub fn signal(&self) -> &Signal {
        &self.wake
    }

    // None until the share is granted and, on a client, until the host has
    // said how it is watched.
    pub fn facts(&self) -> Option<ShareFacts> {
        lock(&self.told).facts
    }

    // What came back since the last call, for the share under way.
    pub fn take_answers(&self) -> Vec<Answer> {
        std::mem::take(&mut lock(&self.told).answers)
    }

    // Both at once: the answers since the last call, and the facts as they
    // are with them.
    pub fn take_answers_and_facts(&self) -> (Vec<Answer>, Option<ShareFacts>) {
        let mut told = lock(&self.told);
        (std::mem::take(&mut told.answers), told.facts)
    }

    fn answers_waiting(&self) -> bool {
        !lock(&self.told).answers.is_empty()
    }

    // One per thread that sends: each has buffers of its own.
    pub fn outbox(self: &Arc<Sharing>) -> Outbox {
        Outbox {
            sharing: Arc::clone(self),
            outlets: Arc::new([]),
            generation: u64::MAX,
            plain: Vec::with_capacity(1 + MAX_VIDEO),
            sealed: Vec::with_capacity(1 + MAX_VIDEO + session::DATA_OVERHEAD),
        }
    }

    // Checked here, then sent over the control channel by the timer thread.
    // A newer shape replaces one still waiting. The id is what the pointer
    // updates name.
    pub fn shape(&self, shape: Shape) -> Result<u32, ShapeError> {
        shape.check()?;
        let id = loop {
            let id = self.next_shape.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                break id;
            }
        };
        *lock(&self.shape) = Some((id, shape));
        self.wake_room();
        Ok(id)
    }

    // Microseconds on this PC's ping clock, which FrameFacts' capture and
    // encode times are on.
    pub fn micros(&self, at: Instant) -> u64 {
        self.clock.micros(at)
    }

    pub fn video_sent(&self) -> u64 {
        self.video_sent.load(Ordering::Relaxed)
    }

    // The falling cue plays here when the share it rose for ends: at once,
    // not when its thread notices at its next frame.
    pub(crate) fn set_state(&self, state: OwnShare) {
        let mut own = lock(&self.state);
        let ended = !matches!(state, OwnShare::Sharing { .. });
        let new_share = match (&own.share, &state) {
            (OwnShare::Sharing { number: was, .. }, OwnShare::Sharing { number, .. }) => {
                was != number
            }
            _ => true,
        };
        if own.cued.is_some() && (ended || new_share) {
            own.cued = None;
            self.play(Cue::Falling);
        }
        own.share = state;
        drop(own);
        if ended || new_share {
            *lock(&self.told) = Told::default();
            self.newest_frame.store(NO_FRAME, Ordering::Relaxed);
        }
        if ended {
            *lock(&self.shape) = None;
        }
        self.wake.set();
    }

    pub(crate) fn number(&self) -> Option<u32> {
        match lock(&self.state).share {
            OwnShare::Sharing { number, .. } => Some(number),
            _ => None,
        }
    }

    // The share's thread, or a test that plays it by hand: the capture and
    // the encoder of share `share` are open, and the rising cue plays,
    // unless the room ended that share while they opened.
    pub fn opened(&self, share: u32, running: RunningShare) {
        {
            let mut own = lock(&self.state);
            let ours = matches!(own.share, OwnShare::Sharing { number, .. } if number == share);
            if ours && own.cued != Some(share) {
                own.cued = Some(share);
                self.play(Cue::Rising);
            }
        }
        self.set_running(Some(running));
    }

    // They are closed again, whichever way the share ended. The falling cue
    // plays unless the room's end of the share played it already: here it
    // is for a share that stopped on an error while the room still had it.
    pub fn closed(&self) {
        {
            let mut own = lock(&self.state);
            if own.cued.take().is_some() {
                self.play(Cue::Falling);
            }
        }
        self.set_area(None);
        self.set_running(None);
    }

    // The share's thread, or a test that plays it by hand: the monitor it
    // captures, once it chose one, and None when it stops.
    pub fn set_area(&self, area: Option<Area>) {
        *lock(&self.area) = area;
    }

    pub(crate) fn area(&self) -> Option<Area> {
        *lock(&self.area)
    }

    // From the host's Facts. Its cap is the host's share of the rule, zero
    // with no internet watcher. A new watcher's IDR ask in them goes in with
    // them, under the same lock.
    pub(crate) fn set_facts(&self, facts: &Facts) {
        let cap_kbps = (facts.cap_kbps > 0).then_some(facts.cap_kbps);
        let rate_kbps = cap_kbps.map_or(self.upload_kbps, |cap| cap.min(self.upload_kbps));
        let mut told = lock(&self.told);
        told.facts = Some(ShareFacts {
            watchers: facts.watchers,
            internet: facts.internet,
            rate_kbps,
            spread: facts.internet > 0,
            lan: facts.lan,
            hevc: facts.hevc,
        });
        if facts.idr {
            add_answer(&mut told.answers, Answer::Idr { seen: None });
        }
        drop(told);
        self.wake.set();
    }

    pub(crate) fn answer(&self, answer: Answer) {
        add_answer(&mut lock(&self.told).answers, answer);
        self.wake.set();
    }

    pub(crate) fn take_shape_if(&self, fits: impl FnOnce(&Shape) -> bool) -> Option<(u32, Shape)> {
        let mut waiting = lock(&self.shape);
        if waiting.as_ref().is_some_and(|(_, shape)| fits(shape)) {
            return waiting.take();
        }
        None
    }

    // The newest frame this PC's share sent, if any went out yet.
    pub(crate) fn newest_frame(&self) -> Option<u32> {
        match self.newest_frame.load(Ordering::Relaxed) {
            NO_FRAME => None,
            frame => Some(frame as u32),
        }
    }

    // This PC's video went out lately.
    pub(crate) fn sent_lately(&self, now: Instant) -> bool {
        let last = self.last_sent_us.load(Ordering::Relaxed);
        last != 0
            && self.clock.micros(now).saturating_sub(last) < MEDIA_FLOWS_FOR.as_micros() as u64
    }

    fn set_outlets(&self, outlets: Vec<Outlet>) {
        *lock(&self.outlets) = outlets.into();
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn set_monitor(&self, monitor: Option<MonitorId>) {
        *lock(&self.monitor) = monitor;
    }

    // The share's thread ends at its next frame, or within 100 ms on a
    // still screen.
    pub(crate) fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.wake.set();
    }

    pub(crate) fn take_news(&self) -> Vec<ShareNews> {
        std::mem::take(&mut *lock(&self.news))
    }

    // True once for each batch of new numbers or news: the view needs
    // making again.
    pub(crate) fn take_fresh(&self) -> bool {
        self.fresh.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn running(&self) -> Option<RunningShare> {
        lock(&self.running).clone()
    }

    pub(crate) fn set_round_trip(&self, round_trip: Option<RoundTrip>) {
        *lock(&self.round_trip) = round_trip;
    }

    fn monitor(&self) -> Option<MonitorId> {
        lock(&self.monitor).clone()
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn tell(&self, news: ShareNews) {
        lock(&self.news).push(news);
        self.freshened();
    }

    fn set_running(&self, running: Option<RunningShare>) {
        *lock(&self.running) = running;
        self.freshened();
    }

    fn set_numbers(&self, numbers: Option<SharingNumbers>) {
        *lock(&self.numbers) = numbers;
        self.freshened();
    }

    fn freshened(&self) {
        self.fresh.store(true, Ordering::Release);
        self.wake_room();
    }

    fn take_round_trip(&self) -> Option<RoundTrip> {
        lock(&self.round_trip).take()
    }

    // Everything this PC's socket sent so far, headers included.
    fn upload_bytes(&self) -> Option<u64> {
        self.socket.as_ref().map(|socket| socket.sent_bytes())
    }

    // The ping clock, as the share crate writes frame times in it.
    fn share_clock(&self) -> share::Clock {
        let epoch = Instant::now();
        share::Clock {
            epoch,
            at_epoch: self.clock.micros(epoch),
        }
    }
}

// A sharer's thread's way out: seals each packet for every link in the
// outlet list and sends it, with buffers of its own, so nothing allocates
// once it runs.
pub struct Outbox {
    sharing: Arc<Sharing>,
    outlets: Arc<[Outlet]>,
    generation: u64,
    plain: Vec<u8>,
    sealed: Vec<u8>,
}

impl Outbox {
    // A channels::video packet. Returns how many links it went to: none
    // before the share is granted, and none while nobody watches the host's.
    pub fn video(&mut self, packet: &[u8]) -> usize {
        if PREFIX + packet.len() > MAX_VIDEO {
            return 0;
        }
        let (kind, slot) = self.prefix();
        self.plain.clear();
        self.plain.push(Channel::Video as u8);
        wire::write_video_prefix(kind, slot, &mut self.plain);
        self.plain.extend_from_slice(packet);
        // Before it leaves, so a watcher's recover request about it can
        // never arrive first.
        if let Ok(read) = channels::video::read_packet(packet) {
            let _ = self.sharing.newest_frame.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |was| {
                    (was == NO_FRAME || is_after(read.frame, was as u32))
                        .then_some(u64::from(read.frame))
                },
            );
        }
        let sent = self.send();
        if sent > 0 {
            let sharing = &self.sharing;
            sharing.video_sent.fetch_add(1, Ordering::Relaxed);
            let now_us = sharing.clock.micros(Instant::now());
            let before = sharing.last_sent_us.swap(now_us, Ordering::Relaxed);
            // The first video after a pause brings the pings to the media
            // rate at once, not at the next ping planned at the idle rate, up
            // to a second away.
            if before == 0 || now_us.saturating_sub(before) >= MEDIA_FLOWS_FOR.as_micros() as u64 {
                sharing.wake_room();
            }
        }
        sent
    }

    // Where the pointer is now; its sequence number is given here.
    pub fn pointer(&mut self, x: i32, y: i32, visible: bool, shape: u32) -> usize {
        let pointer = Pointer {
            seq: self.sharing.next_pointer.fetch_add(1, Ordering::Relaxed),
            x,
            y,
            visible,
            shape,
        };
        let (kind, slot) = self.prefix();
        self.plain.clear();
        self.plain.push(Channel::Cursor as u8);
        wire::write_pointer(kind, slot, &pointer, &mut self.plain);
        self.send()
    }

    fn prefix(&self) -> (u8, u8) {
        if self.sharing.host {
            (RELAYED, HOST_SLOT)
        } else {
            (SENT, 0)
        }
    }

    fn send(&mut self) -> usize {
        let generation = self.sharing.generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.outlets = Arc::clone(&lock(&self.sharing.outlets));
            self.generation = generation;
        }
        let Some(socket) = &self.sharing.socket else {
            return 0;
        };
        let mut sent = 0;
        for outlet in self.outlets.iter() {
            if outlet.sealer.seal(&self.plain, &mut self.sealed).is_err() {
                continue;
            }
            // A failed send is a dead route; the room hears about that from
            // the silence, as with every other packet.
            if socket.send_to(&self.sealed, outlet.to).is_ok() {
                outlet.sent.count(self.sealed.len());
                sent += 1;
            }
        }
        sent
    }
}

// A packet the host passed on from the share being watched. Its Debug shows
// the length, not the bytes: they are someone's screen, which booth.log
// never holds.
pub struct Video {
    pub share: u32,
    // A channels::video packet, for the share's Reassembler.
    pub packet: Vec<u8>,
}

impl fmt::Debug for Video {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Video")
            .field("share", &self.share)
            .field("packet", &self.packet.len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    // This PC watches a share now: a new stream, with a reassembler of its
    // own, at this frame rate.
    Started { share: u32, fps: u8, name: String },
    // The sharer's frame rate changed.
    Fps { share: u32, fps: u8 },
    // The share ended, or this PC stopped watching it: the viewer closes.
    Ended { share: u32 },
}

// What the viewer's thread hands back. The share is named, so what is meant
// for a share that has ended goes nowhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Back {
    Recover { share: u32, first: u32, last: u32 },
    Idr { share: u32, seen: u32 },
    // Shard loss over the last 2 s in percent, None before any measure.
    Loss { share: u32, loss: Option<f32> },
}

#[derive(Default, Debug)]
pub struct Batch {
    pub video: VecDeque<Video>,
    // The newest pointer update; an older one not taken yet is gone.
    pub pointer: Option<(u32, Pointer)>,
    pub shapes: Vec<(u32, u32, Shape)>,
    pub events: Vec<WatchEvent>,
}

#[derive(Default)]
struct Inbox {
    video: VecDeque<Video>,
    spare: Vec<Vec<u8>>,
    pointer: Option<(u32, Pointer)>,
    shapes: Vec<(u32, u32, Shape)>,
    events: Vec<WatchEvent>,
}

// Where a watcher's viewer thread takes what the host passed on, and hands
// back what the sharer should hear.
pub struct Watching {
    clock: Clock,
    inbox: Mutex<Inbox>,
    wake: Signal,
    overflow: AtomicU64,
    backs: Mutex<Vec<Back>>,
    // The share watched now, the sharer's ping clock minus this PC's, and
    // whether either clock offset behind it was taken over a jittery link.
    offset: Mutex<Option<(u32, i64, bool)>>,
    work: OnceLock<Box<dyn Fn() + Send + Sync>>,
    // The room runs a viewer's thread: what arrives goes straight into the
    // viewer's own inbox, and nothing waits in this one. Off in tests that
    // play the viewer by hand.
    threads: bool,
    knob: Option<LossKnob>,
    // The viewer of the share watched now, from its Started on.
    attached: Mutex<Option<Attached>>,
    closing: AtomicBool,
    news: Mutex<Vec<WatchNews>>,
    numbers: Mutex<Option<WatchingNumbers>>,
    fresh: AtomicBool,
    strip_click: Mutex<Option<StripClick>>,
    // The viewer's window is open.
    showing: AtomicBool,
    // Whether this PC's viewer decodes HEVC, which the host hears with
    // Watch: None until the viewer's thread has asked the GPU's driver,
    // which it does as the room opens and again when a viewer opens, and
    // taken as no meanwhile. `hevc_changed` is set when the answer changed
    // while this PC may be watching, so Watch goes again.
    hevc: Mutex<Option<bool>>,
    hevc_changed: AtomicBool,
    // Whether this PC controls the share watched, as the viewer was last
    // told, and a new viewer is told at its start.
    control: Mutex<Option<share::Control>>,
}

// What the panel gives the room to be called when someone clicks the
// viewer's strip, which brings the panel forward with the stats panel open.
// Called on the viewer's thread.
pub type StripClick = Arc<dyn Fn() + Send + Sync>;

// The viewer's side of the share watched now.
struct Attached {
    share: u32,
    inbox: Arc<share::Inbox>,
    knob: Option<Knob>,
    knob_dropped: u64,
    // Shapes lately received, newest last, and the one the viewer draws.
    shapes: VecDeque<(u32, Shape)>,
    drawn: u32,
    scale: f32,
    pointer: Option<Pointer>,
    // The clock offset the viewer has, and when it last had link numbers.
    offset: Option<i64>,
    link_at: Option<Instant>,
    // The share's frame rate, and the one-way time to its sharer when that
    // is not on the LAN: the levels of the viewer's numbers follow both.
    fps: u8,
    one_way_ms: Option<f32>,
}

// What the viewer's thread tells the room about the share `share`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WatchNews {
    // The viewer could not open, or stopped on an error; the room stops
    // watching and says why.
    Failed {
        share: u32,
        name: String,
        why: String,
    },
    // The person closed the viewer's window: the same as Stop watching.
    Closed {
        share: u32,
    },
}

// Shapes a viewer keeps by id: the one drawn now, and a few that came
// before the pointer update naming them.
const SHAPES_KEPT: usize = 4;
// How often the room hands the viewer its link numbers for the strip.
const LINK_EVERY: Duration = Duration::from_millis(250);

impl Watching {
    // `hevc` false says this PC's viewer takes no HEVC whatever its GPU
    // does (VideoConfig::hevc). A test that plays the viewer by hand says
    // what it decodes through it alone.
    fn new(
        clock: Clock,
        wake: Signal,
        threads: bool,
        knob: Option<LossKnob>,
        hevc: bool,
    ) -> Arc<Watching> {
        Arc::new(Watching {
            clock,
            inbox: Mutex::default(),
            wake,
            overflow: AtomicU64::new(0),
            backs: Mutex::default(),
            offset: Mutex::default(),
            work: OnceLock::new(),
            threads,
            knob,
            attached: Mutex::default(),
            closing: AtomicBool::new(false),
            news: Mutex::default(),
            numbers: Mutex::default(),
            fresh: AtomicBool::new(false),
            strip_click: Mutex::default(),
            showing: AtomicBool::new(false),
            hevc: Mutex::new((!threads || !hevc).then_some(hevc)),
            hevc_changed: AtomicBool::new(false),
            control: Mutex::default(),
        })
    }

    // After every change of the view, under the state lock: the viewer
    // hears only of a change, and a start or end of control wakes it, so
    // its capture stops within a moment of control ending.
    pub(crate) fn control(&self, control: Option<share::Control>) {
        // Let go before the viewer's lock: a start takes the two the other
        // way round.
        if std::mem::replace(&mut *lock(&self.control), control) == control {
            return;
        }
        if let Some(attached) = lock(&self.attached).as_ref() {
            attached.inbox.set_control(control);
        }
    }

    // What the host is told this PC's viewer decodes.
    pub(crate) fn takes_hevc(&self) -> bool {
        lock(&self.hevc).unwrap_or(false)
    }

    // From the viewer's thread: what the GPU's driver said, or that a
    // stream of HEVC was refused.
    fn set_takes_hevc(&self, takes: bool) {
        let was = lock(&self.hevc).replace(takes);
        if was.unwrap_or(false) != takes {
            self.hevc_changed.store(true, Ordering::Release);
            self.freshened();
        }
    }

    // True once after what takes_hevc says changed.
    pub(crate) fn take_hevc_changed(&self) -> bool {
        self.hevc_changed.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn on_work(&self, work: impl Fn() + Send + Sync + 'static) {
        let _ = self.work.set(Box::new(work));
    }

    // Set whenever something came in. The viewer's thread waits on it for a
    // watch to start; a test playing the viewer, for packets.
    pub fn signal(&self) -> &Signal {
        &self.wake
    }

    // Swaps what waits into `batch`. The packet buffers of the batch before
    // go back to be filled again.
    pub fn take(&self, batch: &mut Batch) {
        let mut inbox = lock(&self.inbox);
        for video in batch.video.drain(..) {
            if inbox.spare.len() < MAX_SPARE {
                let mut buffer = video.packet;
                buffer.clear();
                inbox.spare.push(buffer);
            }
        }
        std::mem::swap(&mut inbox.video, &mut batch.video);
        batch.pointer = inbox.pointer.take();
        batch.shapes.clear();
        std::mem::swap(&mut inbox.shapes, &mut batch.shapes);
        batch.events.clear();
        std::mem::swap(&mut inbox.events, &mut batch.events);
    }

    // The room takes everything waiting each time it is woken, so one wake
    // per batch is enough.
    pub fn back(&self, back: Back) {
        let mut backs = lock(&self.backs);
        let first = backs.is_empty();
        backs.push(back);
        drop(backs);
        if first && let Some(work) = self.work.get() {
            work();
        }
    }

    // A time on the sharer's ping clock, a frame's capture time, on this
    // PC's, and whether it is only about right. None before the offsets
    // are known, or for another share.
    pub fn to_here(&self, share: u32, at_us: u64) -> Option<(u64, bool)> {
        let (watched, offset, about) = (*lock(&self.offset))?;
        (watched == share).then(|| (at_us.wrapping_sub(offset as u64), about))
    }

    // Microseconds on this PC's ping clock.
    pub fn micros(&self, at: Instant) -> u64 {
        self.clock.micros(at)
    }

    // Packets dropped because the inbox was full: this one's, or the
    // viewer's own while one is open.
    pub fn overflow(&self) -> u64 {
        let viewer = lock(&self.attached)
            .as_ref()
            .map_or(0, |attached| attached.inbox.overflow());
        self.overflow.load(Ordering::Relaxed) + viewer
    }

    pub(crate) fn video(&self, share: u32, packet: &[u8]) {
        if self.threads {
            if let Some(attached) = lock(&self.attached)
                .as_mut()
                .filter(|attached| attached.share == share)
            {
                if attached.knob.as_mut().is_some_and(Knob::drops) {
                    attached.knob_dropped += 1;
                } else {
                    attached.inbox.packet(packet);
                }
            }
            return;
        }
        let mut inbox = lock(&self.inbox);
        if inbox.video.len() >= MAX_WAITING {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut buffer = inbox.spare.pop().unwrap_or_default();
        buffer.extend_from_slice(packet);
        inbox.video.push_back(Video {
            share,
            packet: buffer,
        });
        // The viewer takes everything each time it wakes, so one wake per
        // batch is enough.
        let first = inbox.video.len() == 1;
        drop(inbox);
        if first {
            self.wake.set();
        }
    }

    // A newer update replaces one not taken yet; an older one is dropped.
    pub(crate) fn pointer(&self, share: u32, pointer: Pointer) {
        if self.threads {
            if let Some(attached) = lock(&self.attached)
                .as_mut()
                .filter(|attached| attached.share == share)
            {
                attached.pointer_in(pointer);
            }
            return;
        }
        let mut inbox = lock(&self.inbox);
        let newer = inbox.pointer.is_none_or(|(waiting, older)| {
            waiting != share || pointer.seq.wrapping_sub(older.seq) < 1 << 31
        });
        if newer {
            inbox.pointer = Some((share, pointer));
            drop(inbox);
            self.wake.set();
        }
    }

    pub(crate) fn shape(&self, share: u32, id: u32, shape: Shape) {
        if self.threads {
            if let Some(attached) = lock(&self.attached)
                .as_mut()
                .filter(|attached| attached.share == share)
            {
                attached.shape_in(id, shape);
            }
            return;
        }
        let mut inbox = lock(&self.inbox);
        if inbox.shapes.len() >= MAX_SHAPES_WAITING {
            inbox.shapes.remove(0);
        }
        inbox.shapes.push((share, id, shape));
        drop(inbox);
        self.wake.set();
    }

    pub(crate) fn event(&self, event: WatchEvent) {
        if self.threads {
            self.viewer_event(&event);
        }
        let mut inbox = lock(&self.inbox);
        if let WatchEvent::Ended { share } = event {
            // What waits for a share that ended goes nowhere now.
            let mut dropped = Vec::new();
            inbox.video.retain_mut(|video| {
                let keep = video.share != share;
                if !keep {
                    dropped.push(std::mem::take(&mut video.packet));
                }
                keep
            });
            for mut buffer in dropped {
                if inbox.spare.len() < MAX_SPARE {
                    buffer.clear();
                    inbox.spare.push(buffer);
                }
            }
            if inbox.pointer.is_some_and(|(waiting, _)| waiting == share) {
                inbox.pointer = None;
            }
            inbox.shapes.retain(|(waiting, _, _)| *waiting != share);
            *lock(&self.offset) = None;
        }
        if inbox.events.len() >= MAX_EVENTS_WAITING {
            inbox.events.remove(0);
        }
        inbox.events.push(event);
        drop(inbox);
        self.wake.set();
    }

    pub(crate) fn take_backs(&self) -> Vec<Back> {
        std::mem::take(&mut *lock(&self.backs))
    }

    pub(crate) fn set_offset(&self, offset: Option<(u32, i64, bool)>) {
        *lock(&self.offset) = offset;
        let Some((share, offset_us, _)) = offset else {
            return;
        };
        if let Some(attached) = lock(&self.attached)
            .as_mut()
            .filter(|attached| attached.share == share && attached.offset != Some(offset_us))
        {
            attached.offset = Some(offset_us);
            attached
                .inbox
                .set_clock(sharer_clock(self.clock, offset_us));
        }
    }

    // The viewer's side of a start, an end or a new frame rate. The inbox is
    // made the moment watching starts, before the viewer's window opens, so
    // the IDR asked for at the start never arrives to nobody.
    fn viewer_event(&self, event: &WatchEvent) {
        let mut attached = lock(&self.attached);
        match *event {
            WatchEvent::Started { share, fps, .. } => {
                if let Some(old) = attached.take() {
                    old.inbox.stop();
                }
                // Making an event is all Inbox::new does; if Windows refuses
                // even that, the viewer's thread finds no inbox and says so.
                if let Ok(inbox) = share::Inbox::new() {
                    let offset = lock(&self.offset)
                        .filter(|(watched, _, _)| *watched == share)
                        .map(|(_, offset_us, _)| offset_us);
                    if let Some(offset_us) = offset {
                        inbox.set_clock(sharer_clock(self.clock, offset_us));
                    }
                    if let Some(control) = *lock(&self.control) {
                        inbox.set_control(Some(control));
                    }
                    *attached = Some(Attached {
                        share,
                        inbox: Arc::new(inbox),
                        knob: self
                            .knob
                            .filter(|knob| knob.percent > 0.0)
                            .map(|knob| Knob::new(knob.percent, knob.seed)),
                        knob_dropped: 0,
                        shapes: VecDeque::new(),
                        drawn: 0,
                        scale: 1.0,
                        pointer: None,
                        offset,
                        link_at: None,
                        fps,
                        one_way_ms: None,
                    });
                }
            }
            WatchEvent::Ended { share } => {
                if attached.as_ref().is_some_and(|a| a.share == share)
                    && let Some(old) = attached.take()
                {
                    old.inbox.stop();
                }
                drop(attached);
                self.set_numbers(None);
            }
            WatchEvent::Fps { share, fps } => {
                if let Some(attached) = attached.as_mut().filter(|a| a.share == share) {
                    attached.fps = fps;
                    attached.inbox.set_fps(u32::from(fps));
                }
            }
        }
    }

    // The viewer's inbox for `share`, from its Started until its Ended.
    pub(crate) fn inbox_for(&self, share: u32) -> Option<Arc<share::Inbox>> {
        lock(&self.attached)
            .as_ref()
            .filter(|attached| attached.share == share)
            .map(|attached| Arc::clone(&attached.inbox))
    }

    // Starts and ends for the viewer's thread, oldest first.
    pub(crate) fn take_events(&self) -> Vec<WatchEvent> {
        std::mem::take(&mut lock(&self.inbox).events)
    }

    // The sharer's clock as this PC reads it, and whether it is only about
    // right: None before the offsets are known, or for another share.
    pub(crate) fn sharer_clock(&self, share: u32) -> Option<(share::Clock, bool)> {
        let (watched, offset_us, about) = (*lock(&self.offset))?;
        (watched == share).then(|| (sharer_clock(self.clock, offset_us), about))
    }

    // The strip's link numbers for the viewer, at most every LINK_EVERY.
    pub(crate) fn link(&self, now: Instant, link: impl FnOnce() -> share::LinkNumbers) {
        if let Some(attached) = lock(&self.attached).as_mut()
            && attached
                .link_at
                .is_none_or(|at| now.saturating_duration_since(at) >= LINK_EVERY)
        {
            attached.link_at = Some(now);
            let link = link();
            attached.one_way_ms = link.one_way_ms;
            attached.inbox.set_link(link);
        }
    }

    // The frame rate of `share` and the one-way time to its sharer, while
    // this PC watches it.
    pub(crate) fn pace(&self, share: u32) -> Option<(u8, Option<f32>)> {
        lock(&self.attached)
            .as_ref()
            .filter(|attached| attached.share == share)
            .map(|attached| (attached.fps, attached.one_way_ms))
    }

    // Packets the loss knob dropped for `share` so far.
    pub(crate) fn knob_dropped(&self, share: u32) -> u64 {
        lock(&self.attached)
            .as_ref()
            .filter(|attached| attached.share == share)
            .map_or(0, |attached| attached.knob_dropped)
    }

    pub(crate) fn knob(&self) -> Option<LossKnob> {
        self.knob.filter(|knob| knob.percent > 0.0)
    }

    // The viewer's thread ends once its viewer, if one is open, has closed.
    pub(crate) fn close(&self) {
        self.closing.store(true, Ordering::Release);
        if let Some(attached) = lock(&self.attached).take() {
            attached.inbox.stop();
        }
        self.wake.set();
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn showing(&self) -> bool {
        self.showing.load(Ordering::Acquire)
    }

    fn set_showing(&self, showing: bool) {
        self.showing.store(showing, Ordering::Release);
        self.freshened();
    }

    // As if the person closed the viewer's window.
    pub(crate) fn close_viewer(&self) {
        if let Some(attached) = lock(&self.attached).as_ref() {
            attached.inbox.close();
        }
    }

    pub(crate) fn on_strip_click(&self, click: StripClick) {
        *lock(&self.strip_click) = Some(click);
    }

    fn strip_clicked(&self) {
        let click = lock(&self.strip_click).clone();
        if let Some(click) = click {
            click();
        }
    }

    pub(crate) fn take_news(&self) -> Vec<WatchNews> {
        std::mem::take(&mut *lock(&self.news))
    }

    pub(crate) fn take_fresh(&self) -> bool {
        self.fresh.swap(false, Ordering::AcqRel)
    }

    fn tell(&self, news: WatchNews) {
        lock(&self.news).push(news);
        self.freshened();
    }

    fn set_numbers(&self, numbers: Option<WatchingNumbers>) {
        *lock(&self.numbers) = numbers;
        self.freshened();
    }

    fn freshened(&self) {
        self.fresh.store(true, Ordering::Release);
        if let Some(work) = self.work.get() {
            work();
        }
    }
}

// This PC's ping clock moved by the offset to the sharer's: the sharer's
// clock as read here.
fn sharer_clock(clock: Clock, offset_us: i64) -> share::Clock {
    let epoch = Instant::now();
    share::Clock {
        epoch,
        at_epoch: clock.micros(epoch).wrapping_add(offset_us as u64),
    }
}

impl Attached {
    fn pointer_in(&mut self, pointer: Pointer) {
        if self
            .pointer
            .is_some_and(|older| pointer.seq.wrapping_sub(older.seq) >= 1 << 31)
        {
            return;
        }
        self.pointer = Some(pointer);
        self.hand_pointer();
    }

    fn shape_in(&mut self, id: u32, shape: Shape) {
        if self.shapes.len() >= SHAPES_KEPT {
            self.shapes.pop_front();
        }
        self.shapes.push_back((id, shape));
        // A shape that came after the update naming it goes out now.
        if self.pointer.is_some_and(|pointer| pointer.shape == id) {
            self.hand_pointer();
        }
    }

    // The pointer as capture had it, with its shape the first time the
    // viewer can draw it, as capture sends a shape only when it changes.
    fn hand_pointer(&mut self) {
        let Some(pointer) = self.pointer else {
            return;
        };
        let mut shape = None;
        if pointer.shape != self.drawn
            && let Some((_, named)) = self
                .shapes
                .iter()
                .rev()
                .find(|(id, _)| *id == pointer.shape)
        {
            self.drawn = pointer.shape;
            self.scale = f32::from(named.scale_milli) / 1000.0;
            shape = Some(cursor_shape(named));
        }
        self.inbox.cursor(share::CursorUpdate {
            x: pointer.x,
            y: pointer.y,
            visible: pointer.visible,
            scale: self.scale,
            shape,
        });
    }
}

fn cursor_shape(shape: &Shape) -> share::CursorShape {
    share::CursorShape {
        kind: match shape.kind {
            ShapeKind::Monochrome => share::CursorKind::Monochrome,
            ShapeKind::Color => share::CursorKind::Color,
            ShapeKind::MaskedColor => share::CursorKind::MaskedColor,
        },
        width: u32::from(shape.width),
        height: u32::from(shape.height),
        pitch: u32::from(shape.pitch),
        hotspot_x: i32::from(shape.hotspot_x),
        hotspot_y: i32::from(shape.hotspot_y),
        bytes: shape.bytes.clone(),
    }
}

// What the room opens with: the socket the sharer's threads send on (none in
// tests that drive a side by hand), this PC's upload setting, and the events
// the viewer's thread and the sharer's thread wait on, made where a failure
// can still stop the room from opening.
pub(crate) struct ScreenSetup {
    pub socket: Option<Arc<Socket>>,
    pub upload_kbps: u32,
    pub wake: Signal,
    pub answered: Signal,
    // The room runs the share's and the viewer's threads; off when a test
    // plays them by hand.
    pub threads: bool,
    pub knob: Option<LossKnob>,
    // VideoConfig::hevc.
    pub hevc: bool,
    // Remote control: what puts a controller's input on this PC, none
    // where it cannot be controlled, and the event the controller's send
    // thread waits on.
    pub injector: Option<Arc<dyn Injector>>,
    pub control_wake: Signal,
}

// The room's side of sharing that both roles keep, under the state lock.
pub(crate) struct Screen {
    pub sharing: Arc<Sharing>,
    pub watching: Arc<Watching>,
    // Remote control of the share, either way.
    pub remote: Remote,
    // Which sessions and addresses the sharer's outlets have now.
    published: Vec<(u32, SocketAddr)>,
    // The share this PC watches.
    pub watched: Option<u32>,
    shapes: Assembler,
    // What this PC's own shapes may still send, and when the one waiting
    // is to be tried again.
    shape_budget: Bucket,
    shape_due: Option<Instant>,
    // The video of the share whose sharer is at the far end of a link, for
    // the strip.
    far: Option<Far>,
    // Host: video, pointers and shapes over a friend's limits; watcher: none.
    pub dropped: u64,
    // Host: video packets passed on to watchers.
    pub relayed: u64,
    // The newest problem with this PC's share or viewer, as the chat said
    // it, until the next Share or Watch.
    pub problem: Option<String>,
}

struct Far {
    sharer: [u8; 32],
    share: u32,
    // Jitter from each frame's first packet, whose encode time it carries.
    jitter: StreamStats,
    loss: PacketLoss,
}

impl Screen {
    pub(crate) fn new(clock: Clock, host: bool, setup: ScreenSetup, now: Instant) -> Screen {
        let controls = Controls::new(clock, setup.socket.clone(), setup.control_wake);
        Screen {
            remote: Remote::new(Gate::new(setup.injector, clock), controls),
            sharing: Sharing::new(clock, host, setup.socket, setup.upload_kbps, setup.answered),
            watching: Watching::new(clock, setup.wake, setup.threads, setup.knob, setup.hevc),
            published: Vec::new(),
            watched: None,
            shapes: Assembler::default(),
            shape_budget: Bucket::full(now, SHAPE_SEND_BURST),
            shape_due: None,
            far: None,
            dropped: 0,
            relayed: 0,
            problem: None,
        }
    }

    // This PC's newest shape, when its budget has the bytes and every
    // control channel it goes on has room for its chunks; `queued` is what
    // waits on the fullest of them now. Otherwise the shape waits, and
    // shape_due says when to try again.
    pub(crate) fn shape_out(&mut self, now: Instant, queued: usize) -> Option<(u32, Shape)> {
        let (budget, due) = (&mut self.shape_budget, &mut self.shape_due);
        *due = None;
        self.sharing.take_shape_if(|shape| {
            let bytes = shape.bytes.len();
            if queued + wire::chunk_count(bytes) > SHAPE_QUEUE_ROOM {
                *due = Some(now + SHAPE_RETRY);
                return false;
            }
            let bytes = bytes as f64;
            let wait = budget.wait_for(now, bytes, SHAPE_SEND_PER_SECOND, SHAPE_SEND_BURST);
            if !wait.is_zero() {
                *due = Some(now + wait);
                return false;
            }
            budget.take_many(now, bytes, SHAPE_SEND_PER_SECOND, SHAPE_SEND_BURST)
        })
    }

    // Only while this PC shares: a time left from a share that ended would
    // be a deadline in the past on every timer pass.
    pub(crate) fn shape_due(&self) -> Option<Instant> {
        self.shape_due.filter(|_| self.sharing.number().is_some())
    }

    // Hands the sharer's thread a new list of links when the sessions,
    // addresses or watchers changed. Called after every step the room takes,
    // so it compares without allocating and builds sealers only on a change.
    pub(crate) fn publish(
        &mut self,
        links: impl Iterator<Item = (u32, SocketAddr)> + Clone,
        outlets: impl FnOnce() -> Vec<Outlet>,
    ) {
        if links.clone().eq(self.published.iter().copied()) {
            return;
        }
        self.published = links.collect();
        self.sharing.set_outlets(outlets());
    }

    // A video packet for the share this PC watches, from `sharer`. It feeds
    // the strip's numbers when the sharer is at the far end of this PC's
    // link: the host's own share on a client, any client's on the host.
    pub(crate) fn video_in(
        &mut self,
        sharer: [u8; 32],
        share: u32,
        packet: &channels::video::Packet<'_>,
        far_end: bool,
        arrived_us: u64,
        now: Instant,
    ) {
        if !far_end {
            return;
        }
        let far = match &mut self.far {
            Some(far) if far.sharer == sharer && far.share == share => far,
            far => far.insert(Far {
                sharer,
                share,
                jitter: StreamStats::new(),
                loss: PacketLoss::default(),
            }),
        };
        far.loss.packet(packet.frame, packet.total(), now);
        if packet.index == 0 {
            // Frame numbers stand in for sequence numbers: 16 bits wrap
            // every nine minutes at 120 fps, which StreamStats follows. Its
            // loss is not used: a frame the sharer's own pacer let go would
            // count as lost there.
            far.jitter.record(
                packet.frame as u16,
                wire::encoded_at(packet),
                arrived_us,
                now,
            );
        }
    }

    // The strip's numbers from the video `far_end` shares, while it flows.
    pub(crate) fn on_link(&self, far_end: &[u8; 32], now: Instant) -> Option<OnLink> {
        let far = self.far.as_ref().filter(|far| far.sharer == *far_end)?;
        Some(OnLink {
            jitter_ms: far.jitter.jitter_ms(),
            loss_pct: far.loss.loss(now)?.percent()?,
        })
    }

    pub(crate) fn fill(&self, numbers: &mut Numbers) {
        numbers.video_sent = self.sharing.video_sent();
        numbers.video_relayed = self.relayed;
        numbers.video_dropped = self.dropped;
        numbers.video_overflow = self.watching.overflow();
        numbers.share_rate_kbps = self
            .sharing
            .facts()
            .filter(|_| self.sharing.number().is_some())
            .map(|facts| facts.rate_kbps);
        numbers.sharing = lock(&self.sharing.numbers).clone();
        numbers.watching = lock(&self.watching.numbers).clone();
        self.remote.fill(&mut numbers.control);
    }

    pub(crate) fn shape_in(&mut self, chunk: wire::ShapeChunk) -> Option<(u32, u32, Shape)> {
        self.shapes.push(chunk)
    }

    // The watched share is over, or this PC stopped watching it.
    pub(crate) fn stop_watching(&mut self) {
        if let Some(share) = self.watched.take() {
            self.watching.event(WatchEvent::Ended { share });
            self.shapes = Assembler::default();
        }
    }
}

// The strip's loss from video: the packets missing from the frames that
// arrived, over STREAM_WINDOW. A frame none of whose packets came is not
// counted: that is far more often the sharer's own pacer letting a late
// frame go than the network taking every packet of it, and the strip shows
// what the network loses.
#[derive(Default)]
struct PacketLoss {
    open: VecDeque<OpenFrame>,
    counted: VecDeque<Counted>,
}

struct OpenFrame {
    frame: u32,
    expected: u32,
    arrived: u32,
    since: Instant,
}

struct Counted {
    at: Instant,
    frame: u32,
    lost: u32,
    expected: u32,
}

impl PacketLoss {
    // `total` is the frame's data and parity packets, as each of its
    // packets says.
    fn packet(&mut self, frame: u32, total: usize, now: Instant) {
        if let Some(open) = self.open.iter_mut().find(|open| open.frame == frame) {
            open.arrived = open.arrived.saturating_add(1);
            return;
        }
        // Late for a frame already counted: one fewer lost.
        if let Some(counted) = self.counted.iter_mut().rev().find(|c| c.frame == frame) {
            counted.lost = counted.lost.saturating_sub(1);
            return;
        }
        self.open.push_back(OpenFrame {
            frame,
            expected: u32::try_from(total).unwrap_or(u32::MAX),
            arrived: 1,
            since: now,
        });
        while self.open.len() > OPEN_FRAMES
            || self
                .open
                .front()
                .is_some_and(|open| now.saturating_duration_since(open.since) >= OPEN_FOR)
        {
            let Some(open) = self.open.pop_front() else {
                break;
            };
            self.count(open, now);
        }
    }

    fn count(&mut self, open: OpenFrame, now: Instant) {
        while self.counted.len() >= MAX_COUNTED
            || self
                .counted
                .front()
                .is_some_and(|c| now.saturating_duration_since(c.at) >= STREAM_WINDOW)
        {
            self.counted.pop_front();
        }
        self.counted.push_back(Counted {
            at: now,
            frame: open.frame,
            lost: open.expected.saturating_sub(open.arrived),
            expected: open.expected,
        });
    }

    // None when no frame was counted in the window: video does not flow.
    fn loss(&self, now: Instant) -> Option<StreamLoss> {
        let (lost, expected) = self
            .counted
            .iter()
            .filter(|c| now.saturating_duration_since(c.at) < STREAM_WINDOW)
            .fold((0u32, 0u32), |(lost, expected), c| {
                (
                    lost.saturating_add(c.lost),
                    expected.saturating_add(c.expected),
                )
            });
        (expected > 0).then_some(StreamLoss { lost, expected })
    }
}

// Sharing problems are system lines in warn that say what happened and what
// to do. What the share crate reports is worded that way already, as a
// clause.
pub(crate) fn share_failed(why: &str, ran: bool) -> String {
    if ran {
        sentence("Sharing stopped", why)
    } else {
        sentence("Could not start sharing", why)
    }
}

pub(crate) fn watch_failed(name: &str, why: &str) -> String {
    sentence(&format!("Could not show {name}'s screen"), why)
}

pub(crate) const SOFTWARE_ENCODER: &str =
    "No GPU encoder on this PC would open. Sharing will use the software encoder at up to 1080p60.";

pub(crate) fn paused_sentence(paused: Paused) -> &'static str {
    match paused {
        Paused::SecureDesktop => {
            "Sharing is paused while Windows shows a secure screen: a UAC prompt, the lock screen or Ctrl+Alt+Del. It goes on by itself when that closes."
        }
        Paused::Taken => {
            "Sharing is paused: another program is capturing this monitor. Close it and sharing goes on."
        }
        Paused::Disconnected => {
            "Sharing is paused while this Windows session is disconnected. It goes on when you are back."
        }
        Paused::Changing => {
            "Sharing is paused while Windows changes the display. It goes on by itself."
        }
    }
}

fn sentence(what: &str, why: &str) -> String {
    let why = why.trim().trim_end_matches('.');
    format!("{what}: {why}.")
}

// Frame `a` comes after frame `b`, the numbers wrapping.
pub(crate) fn is_after(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 1 << 31
}

// One IDR answers two asks. A new watcher's, with no frame, needs one
// whatever came before; otherwise the ask about the newer frame is the one
// an IDR must come after.
fn either_idr(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    let (a, b) = (a?, b?);
    Some(if is_after(b, a) { b } else { a })
}

// Answers waiting for the share's thread are held to a few however long it
// does not take them: only the newest loss counts, one IDR answers every
// ask, and past MAX_RECOVERS_WAITING the oldest recover request goes.
fn add_answer(answers: &mut Vec<Answer>, answer: Answer) {
    match answer {
        Answer::Loss(_) => answers.retain(|waiting| !matches!(waiting, Answer::Loss(_))),
        Answer::Idr { seen } => {
            let waiting = answers.iter_mut().find_map(|waiting| match waiting {
                Answer::Idr { seen: asked } => Some(asked),
                _ => None,
            });
            if let Some(asked) = waiting {
                *asked = either_idr(*asked, seen);
                return;
            }
        }
        Answer::Recover { .. } => {
            let recover = |waiting: &Answer| matches!(waiting, Answer::Recover { .. });
            if answers.iter().filter(|waiting| recover(waiting)).count() >= MAX_RECOVERS_WAITING
                && let Some(oldest) = answers.iter().position(recover)
            {
                answers.remove(oldest);
            }
        }
    }
    answers.push(answer);
}

// A viewer's recover request as the control channel carries it: at most
// MAX_RECOVER_SPAN frames, from the first. The reassembler can gather a
// longer one after an outage, and the sharer looks at no more than the
// first 64 of a range anyway.
pub(crate) fn recover_span(first: u32, last: u32) -> (u32, u32) {
    if last.wrapping_sub(first) >= MAX_RECOVER_SPAN {
        (first, first.wrapping_add(MAX_RECOVER_SPAN - 1))
    } else {
        (first, last)
    }
}

// Loss in percent as the control messages carry it, in tenths of a percent,
// and back.
pub(crate) fn permille(percent: Option<f32>) -> Option<u16> {
    let percent = percent.filter(|percent| percent.is_finite())?;
    Some((percent * 10.0).round().clamp(0.0, f32::from(PERMILLE)) as u16)
}

pub(crate) fn percent(permille: u16) -> f32 {
    f32::from(permille.min(PERMILLE)) / 10.0
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Message;
    use crate::testing::quiet_screen;
    use net::pace::{Timer, Woken};

    fn screen(now: Instant) -> Screen {
        Screen::new(Clock::new(now), false, quiet_screen(), now)
    }

    fn color(side: u16) -> Shape {
        Shape {
            kind: ShapeKind::Color,
            width: side,
            height: side,
            pitch: side * 4,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_milli: 1000,
            bytes: vec![9; usize::from(side) * usize::from(side) * 4],
        }
    }

    // xorshift64, seeded, so a failure shows up the same way every run.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    // A minute of a sharer whose pointer changes 25 times a second, largest
    // shapes among them, sent under its own budget, and a host that charges
    // each shape when its first chunk arrives: in order, but each up to a
    // quarter second late, so two can arrive closer than they left. The
    // host never has to drop one, and the sharer still gets most of what
    // its budget allows.
    #[test]
    fn sharer_shapes_fit_host_budget() {
        let start = Instant::now();
        let mut screen = screen(start);
        let mut host = Bucket::full(start, SHAPE_BYTES_BURST);
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        let mut arrived = start;
        let (mut shapes, mut bytes) = (0usize, 0usize);
        for step in 0..6000u64 {
            let now = start + Duration::from_millis(step * 10);
            if random.next().is_multiple_of(4) {
                let side = [256, 256, 128, 64, 32][(random.next() % 5) as usize];
                screen
                    .sharing
                    .shape(color(side))
                    .expect("a shape Booth sends");
            }
            let Some((_, shape)) = screen.shape_out(now, 0) else {
                continue;
            };
            arrived = arrived.max(now + Duration::from_millis(random.next() % 250));
            let len = shape.bytes.len();
            assert!(
                host.take_many(
                    arrived,
                    len as f64,
                    SHAPE_BYTES_PER_SECOND,
                    SHAPE_BYTES_BURST
                ),
                "shape {shapes}, {len} bytes, dropped by the host"
            );
            shapes += 1;
            bytes += len;
        }
        let allowed = SHAPE_SEND_BURST + SHAPE_SEND_PER_SECOND * 60.0;
        println!(
            "{shapes} shapes, {bytes} bytes in a minute, of {allowed:.0} the budget allows; the host took every one"
        );
        assert!(bytes as f64 > allowed * 0.75, "{bytes} of {allowed}");
    }

    #[test]
    fn shape_past_budget_waits() {
        let start = Instant::now();
        let mut screen = screen(start);
        screen
            .sharing
            .set_state(OwnShare::Sharing { number: 1, fps: 60 });
        let first = screen.sharing.shape(color(256)).unwrap();
        assert_eq!(screen.shape_out(start, 0).map(|(id, _)| id), Some(first));
        assert_eq!(screen.shape_due(), None);

        screen.sharing.shape(color(256)).unwrap();
        assert!(screen.shape_out(start, 0).is_none());
        let due = screen.shape_due().expect("a time to try again");
        // 16 KB left of the burst, 240 KB more at 56 KB a second.
        let wait = due - start;
        assert!(
            wait > Duration::from_millis(4250) && wait < Duration::from_millis(4300),
            "{wait:?}"
        );
        let before = due - Duration::from_millis(10);
        assert!(screen.shape_out(before, 0).is_none());
        let third = screen.sharing.shape(color(32)).unwrap();
        assert_eq!(screen.shape_out(before, 0).map(|(id, _)| id), Some(third));

        // A control channel without room for all of it holds a shape back
        // as well, and it is tried again soon.
        let fourth = screen.sharing.shape(color(32)).unwrap();
        let later = due + Duration::from_secs(10);
        let chunks = wire::chunk_count(32 * 32 * 4);
        assert!(
            screen
                .shape_out(later, SHAPE_QUEUE_ROOM - chunks + 1)
                .is_none()
        );
        assert_eq!(screen.shape_due(), Some(later + SHAPE_RETRY));
        assert_eq!(
            screen
                .shape_out(later, SHAPE_QUEUE_ROOM - chunks)
                .map(|(id, _)| id),
            Some(fourth)
        );
        assert_eq!(screen.shape_due(), None);

        // A share that ends takes the shape waiting with it, and leaves no
        // time to try again.
        for waits in [false, true] {
            screen.sharing.shape(color(256)).unwrap();
            assert_eq!(screen.shape_out(later, 0).is_none(), waits);
        }
        assert!(screen.shape_due().is_some());
        screen.sharing.set_state(OwnShare::Off);
        assert_eq!(screen.shape_due(), None);
        screen
            .sharing
            .set_state(OwnShare::Sharing { number: 2, fps: 60 });
        assert!(
            screen
                .shape_out(later + Duration::from_secs(60), 0)
                .is_none()
        );
    }

    // The sharer's thread may stall while answers keep coming: only the
    // newest loss is kept, one IDR ask stands for all, and the recover
    // requests are held to the newest MAX_RECOVERS_WAITING.
    #[test]
    fn stalled_sharer_answers_stay_few() {
        let screen = screen(Instant::now());
        let sharing = &screen.sharing;
        for n in 0..1000u32 {
            sharing.answer(Answer::Recover { first: n, last: n });
            sharing.answer(Answer::Loss(Some(n as f32 / 10.0)));
            sharing.answer(Answer::Idr { seen: Some(n) });
        }
        let answers = sharing.take_answers();
        assert_eq!(answers.len(), MAX_RECOVERS_WAITING + 2);
        let losses: Vec<&Answer> = answers
            .iter()
            .filter(|answer| matches!(answer, Answer::Loss(_)))
            .collect();
        assert_eq!(losses, [&Answer::Loss(Some(99.9))]);
        assert!(answers.contains(&Answer::Idr { seen: Some(999) }));
        let recovered: Vec<u32> = answers
            .iter()
            .filter_map(|answer| match answer {
                Answer::Recover { first, .. } => Some(*first),
                _ => None,
            })
            .collect();
        assert_eq!(
            recovered,
            (1000 - MAX_RECOVERS_WAITING as u32..1000).collect::<Vec<_>>()
        );

        // A new watcher's ask, with no frame, needs an IDR whatever else
        // waits; otherwise the newest frame, across a wrap too.
        for seen in [Some(5), None, Some(9)] {
            sharing.answer(Answer::Idr { seen });
        }
        assert_eq!(sharing.take_answers(), [Answer::Idr { seen: None }]);
        for seen in [u32::MAX - 1, 2, u32::MAX] {
            sharing.answer(Answer::Idr { seen: Some(seen) });
        }
        assert_eq!(sharing.take_answers(), [Answer::Idr { seen: Some(2) }]);

        // And the sharer's thread is woken for them.
        let timer = Timer::new().expect("a timer");
        timer
            .set_at(Instant::now() + Duration::from_millis(50))
            .expect("set the timer");
        sharing.answer(Answer::Loss(None));
        assert!(matches!(
            net::pace::wait(sharing.signal(), Some(&timer)),
            Ok(Woken::Signal)
        ));
    }

    // A new watcher's IDR ask comes in the facts that count it, and the
    // share's thread takes the two together: facts that change the codec
    // never reach it a frame ahead of the ask. The ask joins one waiting.
    #[test]
    fn idr_ask_taken_with_facts() {
        let screen = screen(Instant::now());
        let sharing = &screen.sharing;
        let facts = |watchers, hevc, idr| Facts {
            share: 1,
            watchers,
            internet: 0,
            cap_kbps: 0,
            lan: true,
            hevc,
            idr,
        };
        sharing.set_facts(&facts(1, true, true));
        sharing.answer(Answer::Idr { seen: Some(40) });
        sharing.set_facts(&facts(2, false, true));
        let (answers, told) = sharing.take_answers_and_facts();
        assert_eq!(answers, [Answer::Idr { seen: None }]);
        let told = told.expect("facts");
        assert_eq!((told.watchers, told.hevc), (2, false));

        // Facts without an ask bring none, and stay for the next look.
        sharing.set_facts(&facts(1, true, false));
        let (answers, told) = sharing.take_answers_and_facts();
        assert!(answers.is_empty());
        assert_eq!(told.map(|told| told.hevc), Some(true));
        assert_eq!(sharing.facts().map(|told| told.watchers), Some(1));
    }

    // Frames of ten packets: two the sharer's pacer let go never came, one
    // lost two packets on the way, and one packet of another came so late
    // that its frame was counted already.
    #[test]
    fn strip_loss_counts_network_only() {
        let start = Instant::now();
        let mut loss = PacketLoss::default();
        let mut at = start;
        for frame in 0..20u32 {
            at += Duration::from_millis(8);
            if frame == 3 || frame == 4 {
                continue;
            }
            let lost: &[u16] = match frame {
                6 => &[2, 9],
                7 => &[5],
                _ => &[],
            };
            for index in 0..10u16 {
                if !lost.contains(&index) {
                    loss.packet(frame, 10, at);
                }
            }
            if frame == 12 {
                loss.packet(7, 10, at);
            }
        }
        // The newest four are still open: 0 to 2 and 5 to 15 are counted.
        assert_eq!(
            loss.loss(at),
            Some(StreamLoss {
                lost: 2,
                expected: 140
            })
        );
        // A frame waits no longer than OPEN_FOR for the rest of it.
        loss.packet(20, 10, at + OPEN_FOR);
        assert_eq!(loss.loss(at + OPEN_FOR).map(|l| l.expected), Some(180));
        // Video that stopped flowing has no loss.
        assert_eq!(loss.loss(at + OPEN_FOR + STREAM_WINDOW), None);
    }

    #[test]
    fn recover_span_fits_message() {
        let most = MAX_RECOVER_SPAN - 1;
        let wrapped = u32::MAX - 5;
        for ((first, last), want) in [
            ((10, 12), (10, 12)),
            ((10, 10 + most), (10, 10 + most)),
            ((10, 10 + most + 1), (10, 10 + most)),
            ((10, 9), (10, 10 + most)),
            ((wrapped, 3000), (wrapped, wrapped.wrapping_add(most))),
        ] {
            let (first, last) = recover_span(first, last);
            assert_eq!((first, last), want);
            let message = Message::Recover {
                share: 1,
                first,
                last,
            };
            assert!(matches!(
                Message::decode(&message.encode()),
                Some(Message::Recover { share: 1, first: f, last: l }) if (f, l) == (first, last)
            ));
        }
    }

    #[test]
    fn debug_hides_screen_bytes() {
        let video = Video {
            share: 3,
            packet: vec![0xAB; 1161],
        };
        let shape = color(2);
        let chunk = wire::chunks(3, 4, &shape).next().expect("a chunk");
        let batch = Batch {
            video: VecDeque::from([Video {
                share: 3,
                packet: vec![0xAB; 5],
            }]),
            shapes: vec![(3, 4, shape.clone())],
            ..Batch::default()
        };
        for shown in [
            format!("{video:?}"),
            format!("{shape:?}"),
            format!("{chunk:?}"),
            format!("{batch:?}"),
        ] {
            assert!(!shown.contains("171") && !shown.contains("9, 9"), "{shown}");
        }
        assert_eq!(format!("{video:?}"), "Video { share: 3, packet: 1161 }");
        assert!(format!("{shape:?}").contains("bytes: 16"));
    }

    // Every way a share of this PC's begins and ends: the rising cue once
    // its capture and encoder open, the falling one once however it ends,
    // and neither for a share that never opened or that the room ended
    // while it was opening.
    #[test]
    fn share_cues() {
        let screen = screen(Instant::now());
        let sharing = &screen.sharing;
        let played = Arc::new(Mutex::new(Vec::new()));
        sharing.on_cue({
            let played = Arc::clone(&played);
            move |cue| lock(&played).push(cue)
        });
        let running = || RunningShare {
            software: false,
            paused: None,
        };
        let take = || std::mem::take(&mut *lock(&played));
        let sharing_as = |number| OwnShare::Sharing { number, fps: 60 };

        // Granted, opened, a new frame rate, stopped, and its thread closing
        // after.
        sharing.set_state(sharing_as(1));
        assert!(take().is_empty(), "a grant alone plays nothing");
        sharing.opened(1, running());
        assert_eq!(take(), [Cue::Rising]);
        sharing.set_state(OwnShare::Sharing {
            number: 1,
            fps: 120,
        });
        sharing.set_state(OwnShare::Off);
        sharing.closed();
        assert_eq!(take(), [Cue::Falling]);

        // Could not start: the thread closes without having opened.
        sharing.set_state(sharing_as(2));
        sharing.closed();
        sharing.set_state(OwnShare::Off);
        assert!(take().is_empty());

        // Ended by the room while its thread was still opening it.
        sharing.set_state(sharing_as(3));
        sharing.set_state(OwnShare::Off);
        sharing.opened(3, running());
        sharing.closed();
        assert!(take().is_empty());

        // Stopped on an error while running: the thread's closing plays the
        // falling cue, and the room ending the share after it plays nothing.
        sharing.set_state(sharing_as(4));
        sharing.opened(4, running());
        sharing.closed();
        sharing.set_state(OwnShare::Off);
        assert_eq!(take(), [Cue::Rising, Cue::Falling]);

        // A new share in place of the one running.
        sharing.set_state(sharing_as(5));
        sharing.opened(5, running());
        sharing.set_state(sharing_as(6));
        sharing.closed();
        sharing.opened(6, running());
        assert_eq!(take(), [Cue::Rising, Cue::Falling, Cue::Rising]);
    }
}
