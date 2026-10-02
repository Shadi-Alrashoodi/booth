// Remote control of a shared screen. Whoever is being controlled approves
// each session: the host when it shares, the sharing client otherwise, and
// the host can end any.
// Approval lives only in this memory, nowhere on disk: it dies with the
// share, with the session (a rekey keeps it, a fresh handshake does not),
// with the app, and with every end in ControlEnd.
//
// Three threads touch it:
//
// - The controller's capture, the app's, hands events to Controls, which
//   seals and sends them without the room's lock (send.rs).
// - The receive thread, under the state lock, checks an input packet; the
//   host passes it on to the sharer, and the PC being controlled copies its
//   events out. The app's injector is called only once the lock is let go,
//   on the same thread, through the Gate, which carries the starts, the
//   cutoffs and the ends too, in the order the room decided them.
// - The timer thread, under the state lock, runs the cutoff when no packet
//   came for CUTOFF, and tells the controller when an administrator window
//   pauses control. It makes the Gate's calls after its passes, those the
//   panel's own calls decided included, and works out the numbers.
//
// Only those two, both at the highest priority, call the injector while the
// room runs: a panel thread that did would take over the receive thread's
// input whenever it was making calls, and wait for a time slice with it.
//
// One privacy rule holds throughout: no key, character or position is
// logged, counted by value or kept once it is sent or injected. Debug shows
// what kind of event, never which key or where, and the buffers they pass
// through are wiped after use.

mod capture;
mod send;
pub(crate) mod wire;

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use crate::peer::Clock;
use crate::talk::Cue;
use crate::view::{ControlNumbers, ControlRequest, InputDrops, Latency, Party};

pub(crate) use capture::ViewerInput;
pub use capture::{Capture, mouse_report};
pub use send::{Controls, MOVE_EVERY, STATE_EVERY};
pub(crate) use send::{Link as ControlLink, start as start_sender};
pub use wire::MAX_EVENTS;

// No input packet for this long while controlled lets go of everything the
// injector holds; control goes on and the next packet picks up from its
// held state. The controller sends one at least every STATE_EVERY, so this
// is five lost in a row, or a controller that stopped.
pub const CUTOFF: Duration = Duration::from_millis(500);

// The host's limit on each friend's input packets. A controller sends a
// packet per key or button event, mouse moves and the wheel at most every
// MOVE_EVERY, and one with its held state every STATE_EVERY: about 510 a
// second with the mouse moving all the time. Twice that, and a burst of a
// fifth of a second, which covers a clump after a hiccup on the way.
pub(crate) const INPUT_PER_SECOND: f64 = 1000.0;
pub(crate) const INPUT_BURST: f64 = 200.0;
// Asks for control: each one puts a request in front of someone.
pub(crate) const ASKS_PER_SECOND: f64 = 1.0;
pub(crate) const ASK_BURST: f64 = 4.0;
// Input the controller sent just before an end reaches the host after it,
// on a path of its own, and is let go quietly for this long; so does what
// the host passed on before it heard of an end the sharer made.
pub(crate) const ENDED_GRACE: Duration = Duration::from_secs(1);

// Input that waited this long after it arrived, because the injector was
// still busy with what came before it, goes to the injector with its held
// state and without its events: after a stall, a burst of what the
// controller did three frames ago at 60 fps moves the mouse where they no
// longer aim. A lost packet is no worse, since the next one's held state
// lets go of whatever is up. A placeholder until real runs say otherwise.
pub const STALE: Duration = Duration::from_millis(50);
// Past this many input packets waiting, the oldest goes, as if stale. They
// pile up only while one thread is stuck in an injector call and the other
// takes packets, and this keeps that to about 100 KB. The host's
// INPUT_BURST, so a burst the host let through at once is never cut here.
const MOST_WAITING: usize = INPUT_BURST as usize;

// The numbers are over the last 10 s, the same window as mouth to ear and
// capture to display, worked out at most this often off the state lock.
const WINDOW: Duration = Duration::from_secs(10);
const REFRESH_EVERY: Duration = Duration::from_millis(250);
// 10 s of packets at the rate above, twice over.
const MOST_SAMPLES: usize = 10_000;

// A key by where it sits: its scan code set 1 make code, and whether the
// keyboard sends E0 before it, as Raw Input gives it and SendInput takes it.
// Right Ctrl is 0x1D with e0. Make codes 0 and 0xFF are no key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanCode {
    pub code: u8,
    pub e0: bool,
}

impl ScanCode {
    pub(crate) fn possible(code: u8) -> bool {
        code != 0 && code != 0xFF
    }

    fn bit(self) -> usize {
        usize::from(self.code) + if self.e0 { 256 } else { 0 }
    }
}

impl fmt::Debug for ScanCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ScanCode")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    // X1 and X2: back and forward in a browser.
    Back,
    Forward,
}

impl Button {
    pub const ALL: [Button; 5] = [
        Button::Left,
        Button::Right,
        Button::Middle,
        Button::Back,
        Button::Forward,
    ];

    pub(crate) fn index(self) -> u8 {
        match self {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
            Button::Back => 3,
            Button::Forward => 4,
        }
    }

    pub(crate) fn from_index(index: u8) -> Option<Button> {
        Button::ALL.get(usize::from(index)).copied()
    }
}

// One thing the controller did, in the order it was done.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Key { key: ScanCode, down: bool },
    // Raw mouse counts, as Raw Input gives them: relative mode, for games.
    Move { dx: i32, dy: i32 },
    // A point on the shared picture, 0 to 65535 across and down: absolute
    // mode, for the desktop. The PC being controlled maps it onto the
    // shared monitor (Injection::area).
    At { x: u16, y: u16 },
    Button { button: Button, down: bool },
    // In Windows' units, 120 a notch; up and right are positive.
    Wheel { delta: i32 },
    HWheel { delta: i32 },
}

impl InputEvent {
    // What a wiped buffer holds: nothing of anyone's input.
    const BLANK: InputEvent = InputEvent::Move { dx: 0, dy: 0 };
}

impl fmt::Debug for InputEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InputEvent::Key { .. } => "Key",
            InputEvent::Move { .. } => "Move",
            InputEvent::At { .. } => "At",
            InputEvent::Button { .. } => "Button",
            InputEvent::Wheel { .. } => "Wheel",
            InputEvent::HWheel { .. } => "HWheel",
        })
    }
}

// Overwrites what `events` held, then empties it; the capacity stays for
// the next packet.
pub(crate) fn wipe(events: &mut Vec<InputEvent>) {
    events.fill(InputEvent::BLANK);
    events.clear();
}

// Everything the controller holds down: every input packet carries it, so
// a lost packet never leaves a key down on the other PC.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    keys: [u64; 8],
    buttons: u8,
}

impl Held {
    // Bits for the make codes no key has, with and without E0.
    const NO_KEY: [usize; 4] = [0, 0xFF, 256, 256 + 0xFF];

    pub fn key(&self, key: ScanCode) -> bool {
        self.bit(key.bit())
    }

    pub fn button(&self, button: Button) -> bool {
        self.buttons & (1 << button.index()) != 0
    }

    pub fn keys(&self) -> impl Iterator<Item = ScanCode> + '_ {
        (0..512).filter(|&bit| self.bit(bit)).map(|bit| ScanCode {
            code: (bit % 256) as u8,
            e0: bit >= 256,
        })
    }

    pub fn buttons(&self) -> impl Iterator<Item = Button> + '_ {
        Button::ALL
            .into_iter()
            .filter(|&button| self.button(button))
    }

    pub fn is_empty(&self) -> bool {
        self.buttons == 0 && self.keys.iter().all(|&word| word == 0)
    }

    pub fn set_key(&mut self, key: ScanCode, down: bool) {
        if !ScanCode::possible(key.code) {
            return;
        }
        let bit = key.bit();
        if down {
            self.keys[bit / 64] |= 1 << (bit % 64);
        } else {
            self.keys[bit / 64] &= !(1 << (bit % 64));
        }
    }

    pub fn set_button(&mut self, button: Button, down: bool) {
        if down {
            self.buttons |= 1 << button.index();
        } else {
            self.buttons &= !(1 << button.index());
        }
    }

    // Down or up, as the event says.
    pub fn apply(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key { key, down } => self.set_key(key, down),
            InputEvent::Button { button, down } => self.set_button(button, down),
            _ => {}
        }
    }

    fn bit(&self, bit: usize) -> bool {
        self.keys[bit / 64] & (1 << (bit % 64)) != 0
    }
}

impl fmt::Debug for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: u32 = self.keys.iter().map(|word| word.count_ones()).sum();
        f.debug_struct("Held")
            .field("keys", &keys)
            .field("buttons", &self.buttons.count_ones())
            .finish()
    }
}

// The shared monitor on this PC's desktop, in physical pixels and upright,
// as capture has it: the picture covers all of it, so At points map onto it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

// Why control ended. Each end, from either side, reaches the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlEnd {
    // The controller let go: Stop control, its release key, or it stopped
    // watching.
    Released,
    // The person controlled pressed Stop control.
    Stopped,
    // The person controlled pressed the panic key.
    Panic,
    // The host's End control.
    EndedByHost,
    ShareEnded,
    // Either PC's session with the host ended: it left, was lost, or came
    // back through a new handshake.
    SessionLost,
    // This PC's room closed.
    Closed,
}

// A control session of this PC starts: the owner allowed `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub name: String,
    // None while the share is not of a monitor (the test pattern).
    pub area: Option<Area>,
}

// One input packet for this PC, from the controller its owner allowed.
#[derive(Debug)]
pub struct Injection<'a> {
    // Oldest first, as the controller did them.
    pub events: &'a [InputEvent],
    // Everything the controller holds after them: anything the injector
    // holds that is not in it goes up.
    pub held: &'a Held,
    pub area: Option<Area>,
}

// What the injector did with one packet: the events that went to SendInput
// and the ones it dropped, by why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Injected {
    pub sent: u32,
    // The block list: the panic chord, this PC's own hotkeys, Win+L, and
    // anything aimed at Booth's own windows.
    pub blocked: u32,
    // Key events past the rate limit, dropped and never queued.
    pub over_rate: u32,
    // This PC's owner touched their own keyboard or mouse lately.
    pub local: u32,
    // After the panic key.
    pub cut: u32,
    // While an administrator window is in front.
    pub paused: u32,
    // An administrator window is in front now: the controller's viewer
    // says control is paused.
    pub admin: bool,
}

// What the app gives the room to put a controller's input on this PC. The
// room calls it with none of its locks held, one call at a time, in the
// order it decided them: on its receive thread or its timer thread, both at
// the highest priority, and once they have stopped, on the thread leaving
// the room. Each call must return quickly. Nothing reaches `inject` between
// an `ended` and the next `started`.
pub trait Injector: Send + Sync {
    fn started(&self, started: &Started);
    fn inject(&self, input: &Injection<'_>) -> Injected;
    // CUTOFF passed without a packet: let go of everything held. Control
    // goes on.
    fn cut_off(&self);
    // Let go of everything held; nothing more comes until the next start.
    fn ended(&self, why: ControlEnd);
}

// Calls for the injector, decided under the state lock and made after it.
// Input goes in a box of its own, kept for the next packet, so the queue
// holds only where it is and nothing of it stays behind in the queue's
// slots once it is wiped.
enum Call {
    Started { number: u32, started: Started },
    Input(Box<Batch>),
    CutOff { number: u32 },
    Ended { number: u32, why: ControlEnd },
}

struct Batch {
    number: u32,
    events: Vec<InputEvent>,
    held: Held,
    area: Option<Area>,
    // On this PC's ping clock, and whether it is only about right. None
    // when no clock offset reached it.
    captured: Option<(u64, bool)>,
    received: Instant,
}

impl Batch {
    fn wipe(&mut self) {
        wipe(&mut self.events);
        self.held = Held::default();
    }
}

// The boxes go back and forth between `spare` and `calls` whole, so a batch
// is never copied, and only a box's pointer sits in either list.
#[derive(Default)]
#[allow(clippy::vec_box)]
struct Queue {
    calls: VecDeque<Call>,
    spare: Vec<Box<Batch>>,
    // How many of the calls are input.
    inputs: usize,
}

// Only the thread making calls holds it.
#[derive(Default)]
struct Running {
    // The control the injector was told started, until it is told it ended.
    live: Option<u32>,
}

// The way from the room to the app's injector.
pub(crate) struct Gate {
    injector: Option<Arc<dyn Injector>>,
    clock: Clock,
    queue: Mutex<Queue>,
    // Calls wait in the queue. Every packet's thread looks, and this keeps
    // the look to one load when nothing waits, which is nearly always.
    waiting: AtomicBool,
    running: Mutex<Running>,
    stats: Mutex<Stats>,
    // Events dropped for waiting past STALE, or behind MOST_WAITING others.
    stale: AtomicU64,
    // Only the timer thread's refresh uses them: the latencies are worked
    // out here, off the stats lock.
    sums: Mutex<[Vec<f32>; 3]>,
    // The newest Injected said an administrator window is in front.
    admin: AtomicBool,
    // Wakes the timer thread to tell the controller about it.
    work: OnceLock<Box<dyn Fn() + Send + Sync>>,
    // Plays the control cue in this PC's own speakers.
    cue: OnceLock<Box<dyn Fn(Cue) + Send + Sync>>,
}

impl Gate {
    pub(crate) fn new(injector: Option<Arc<dyn Injector>>, clock: Clock) -> Arc<Gate> {
        Arc::new(Gate {
            injector,
            clock,
            queue: Mutex::default(),
            waiting: AtomicBool::new(false),
            running: Mutex::default(),
            stats: Mutex::new(Stats::default()),
            stale: AtomicU64::new(0),
            sums: Mutex::default(),
            admin: AtomicBool::new(false),
            work: OnceLock::new(),
            cue: OnceLock::new(),
        })
    }

    pub(crate) fn on_work(&self, work: impl Fn() + Send + Sync + 'static) {
        let _ = self.work.set(Box::new(work));
    }

    pub(crate) fn on_cue(&self, cue: impl Fn(Cue) + Send + Sync + 'static) {
        let _ = self.cue.set(Box::new(cue));
    }

    // Under the state lock: it only stores the cue for the render thread.
    fn play(&self, cue: Cue) {
        if let Some(play) = self.cue.get() {
            play(cue);
        }
    }

    // Whether this PC can be controlled at all: without an injector every
    // request is refused on the spot.
    pub(crate) fn can_inject(&self) -> bool {
        self.injector.is_some()
    }

    // Under the state lock. The receive and timer threads run the calls
    // once they let go of it; any other thread wakes the timer thread for
    // them (`pending`).
    fn push(&self, call: Call) {
        let mut queue = lock(&self.queue);
        if matches!(call, Call::Input(_)) {
            if queue.inputs >= MOST_WAITING {
                self.drop_oldest_input(&mut queue);
            }
            queue.inputs += 1;
        }
        queue.calls.push_back(call);
        self.waiting.store(true, Ordering::Release);
    }

    fn drop_oldest_input(&self, queue: &mut Queue) {
        let Some(at) = queue
            .calls
            .iter()
            .position(|call| matches!(call, Call::Input(_)))
        else {
            return;
        };
        if let Some(Call::Input(mut oldest)) = queue.calls.remove(at) {
            self.stale
                .fetch_add(oldest.events.len() as u64, Ordering::Relaxed);
            oldest.wipe();
            queue.spare.push(oldest);
            queue.inputs -= 1;
        }
    }

    // Calls wait for a thread to make them.
    pub(crate) fn pending(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    fn batch(&self) -> Box<Batch> {
        lock(&self.queue).spare.pop().unwrap_or_else(|| {
            Box::new(Batch {
                number: 0,
                events: Vec::with_capacity(MAX_EVENTS),
                held: Held::default(),
                area: None,
                captured: None,
                received: Instant::now(),
            })
        })
    }

    pub(crate) fn admin(&self) -> bool {
        self.admin.load(Ordering::Acquire)
    }

    // After the state lock is let go, on the receive or the timer thread. A
    // thread that finds the other one making calls leaves its own to it:
    // that one takes what waits before it lets go, and at the same priority.
    pub(crate) fn run(&self) {
        if !self.waiting.load(Ordering::Acquire) {
            return;
        }
        loop {
            {
                let mut running = match self.running.try_lock() {
                    Ok(running) => running,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => return,
                };
                while let Some(call) = self.next() {
                    self.call(&mut running, call);
                }
            }
            if lock(&self.queue).calls.is_empty() {
                return;
            }
        }
    }

    fn next(&self) -> Option<Call> {
        let mut queue = lock(&self.queue);
        let call = queue.calls.pop_front();
        if matches!(call, Some(Call::Input(_))) {
            queue.inputs -= 1;
        }
        if queue.calls.is_empty() {
            self.waiting.store(false, Ordering::Release);
        }
        call
    }

    fn call(&self, running: &mut Running, call: Call) {
        match call {
            Call::Started { number, started } => {
                running.live = Some(number);
                lock(&self.stats).started();
                if let Some(injector) = &self.injector {
                    injector.started(&started);
                }
            }
            Call::Input(mut batch) => {
                if running.live == Some(batch.number) {
                    self.inject(&mut batch);
                }
                batch.wipe();
                lock(&self.queue).spare.push(batch);
            }
            Call::CutOff { number } => {
                if running.live == Some(number)
                    && let Some(injector) = &self.injector
                {
                    injector.cut_off();
                }
            }
            Call::Ended { number, why } => {
                if running.live == Some(number) {
                    running.live = None;
                    if let Some(injector) = &self.injector {
                        injector.ended(why);
                    }
                }
                if self.admin.swap(false, Ordering::AcqRel) {
                    self.wake();
                }
            }
        }
    }

    fn inject(&self, batch: &mut Batch) {
        let Some(injector) = &self.injector else {
            return;
        };
        let called = Instant::now();
        if called.saturating_duration_since(batch.received) > STALE {
            self.stale
                .fetch_add(batch.events.len() as u64, Ordering::Relaxed);
            wipe(&mut batch.events);
        }
        let injected = injector.inject(&Injection {
            events: &batch.events,
            held: &batch.held,
            area: batch.area,
        });
        let returned = Instant::now();
        let admin = injected.admin;
        {
            let mut stats = lock(&self.stats);
            stats.add(&injected);
            if !batch.events.is_empty() {
                stats.receive_to_inject.record(
                    called,
                    ms(called.saturating_duration_since(batch.received)),
                    false,
                );
                stats.call.record(
                    called,
                    ms(returned.saturating_duration_since(called)),
                    false,
                );
                if let Some((captured_us, about)) = batch.captured {
                    let took = self.clock.micros(called).wrapping_sub(captured_us) as i64;
                    // A clock offset a little off can make a fast packet look
                    // as if it came before it left; that reads as zero. One
                    // past a second is no packet this side took in time.
                    if took < 1_000_000 {
                        stats
                            .capture_to_inject
                            .record(called, took.max(0) as f32 / 1000.0, about);
                    }
                }
            }
        }
        if self.admin.swap(admin, Ordering::AcqRel) != admin {
            self.wake();
        }
    }

    fn wake(&self) {
        if let Some(work) = self.work.get() {
            work();
        }
    }

    // On the timer thread once it let go of the state lock, at most every
    // REFRESH_EVERY: the latencies over the last WINDOW. The samples are
    // copied out under the stats lock and worked out after it, so neither
    // the receive thread recording the next one nor a view being built
    // waits for the sums.
    pub(crate) fn refresh(&self, now: Instant) {
        let mut sums = lock(&self.sums);
        let (session, about) = {
            let mut stats = lock(&self.stats);
            if stats
                .refreshed
                .is_some_and(|at| now.saturating_duration_since(at) < REFRESH_EVERY)
            {
                return;
            }
            stats.refreshed = Some(now);
            let mut about = [false; 3];
            for ((window, values), about) in stats
                .windows()
                .into_iter()
                .zip(sums.iter_mut())
                .zip(&mut about)
            {
                *about = window.recent(now, values);
            }
            (stats.session, about)
        };
        let mut latest = [None; 3];
        for ((values, about), latest) in sums.iter_mut().zip(about).zip(&mut latest) {
            *latest = spread(values).map(|(median_ms, p95_ms)| Latency {
                median_ms,
                p95_ms,
                about,
            });
        }
        let mut stats = lock(&self.stats);
        // Worked out across a start: those were the last session's.
        if stats.session != session {
            return;
        }
        for (window, latest) in stats.windows().into_iter().zip(latest) {
            window.latest = latest;
        }
    }

    fn fill(&self, numbers: &mut ControlNumbers) {
        lock(&self.stats).fill(numbers);
        numbers.dropped.stale += self.stale.load(Ordering::Relaxed);
    }
}

// Latencies over the last WINDOW, worked out at most every REFRESH_EVERY.
#[derive(Default)]
struct Window {
    samples: VecDeque<(Instant, f32, bool)>,
    latest: Option<Latency>,
}

impl Window {
    fn record(&mut self, at: Instant, ms: f32, about: bool) {
        if self.samples.len() >= MOST_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((at, ms, about));
    }

    // Forgets what is older than WINDOW and copies the rest into `values`;
    // true when any of them is only about right.
    fn recent(&mut self, now: Instant, values: &mut Vec<f32>) -> bool {
        while self
            .samples
            .front()
            .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) >= WINDOW)
        {
            self.samples.pop_front();
        }
        values.clear();
        values.extend(self.samples.iter().map(|(_, ms, _)| *ms));
        self.samples.iter().any(|(_, _, about)| *about)
    }
}

// The median and the 95th percentile by nearest rank, as share::spread has
// them, from two selections in place: a sort of 10,000 samples allocates and
// takes several times as long.
fn spread(values: &mut [f32]) -> Option<(f32, f32)> {
    let len = values.len();
    if len == 0 {
        return None;
    }
    let rank = |percent: usize| (len * percent).div_ceil(100) - 1;
    let (median_at, p95_at) = (rank(50), rank(95));
    let (below, p95, _) = values.select_nth_unstable_by(p95_at, f32::total_cmp);
    let p95 = *p95;
    let median = if median_at == p95_at {
        p95
    } else {
        *below.select_nth_unstable_by(median_at, f32::total_cmp).1
    };
    Some((median, p95))
}

#[derive(Default)]
struct Stats {
    capture_to_inject: Window,
    receive_to_inject: Window,
    call: Window,
    injected: u64,
    dropped: InputDrops,
    refreshed: Option<Instant>,
    // Counts the starts, so a refresh worked out across one is let go.
    session: u64,
}

impl Stats {
    // A new session's numbers start over.
    fn started(&mut self) {
        self.capture_to_inject = Window::default();
        self.receive_to_inject = Window::default();
        self.call = Window::default();
        self.refreshed = None;
        self.session += 1;
    }

    fn windows(&mut self) -> [&mut Window; 3] {
        [
            &mut self.capture_to_inject,
            &mut self.receive_to_inject,
            &mut self.call,
        ]
    }

    fn add(&mut self, injected: &Injected) {
        self.injected += u64::from(injected.sent);
        let dropped = &mut self.dropped;
        dropped.blocked += u64::from(injected.blocked);
        dropped.over_rate += u64::from(injected.over_rate);
        dropped.local += u64::from(injected.local);
        dropped.cut += u64::from(injected.cut);
        dropped.paused += u64::from(injected.paused);
    }

    fn fill(&self, numbers: &mut ControlNumbers) {
        numbers.capture_to_inject = self.capture_to_inject.latest;
        numbers.receive_to_inject = self.receive_to_inject.latest;
        numbers.inject_call = self.call.latest;
        numbers.injected += self.injected;
        let dropped = &mut numbers.dropped;
        dropped.blocked += self.dropped.blocked;
        dropped.over_rate += self.dropped.over_rate;
        dropped.local += self.dropped.local;
        dropped.cut += self.dropped.cut;
        dropped.paused += self.dropped.paused;
    }
}

fn ms(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

// This PC's share as the one controlled: asked, then allowed.
#[derive(Clone, Debug)]
pub(crate) struct Here {
    // The host's number for this control.
    pub number: u32,
    pub share: u32,
    pub controller: [u8; 32],
    // Where the host's relayed input says it comes from.
    pub slot: u8,
    pub name: String,
    pub allowed: bool,
    // The newest input packet taken, and when.
    newest: Option<u32>,
    last_input: Option<Instant>,
    // The cutoff ran since the last packet.
    cut: bool,
    // What the controller was last told about an administrator window.
    pub told_admin: bool,
}

// This PC as the controller: asked, then allowed.
#[derive(Clone, Debug)]
pub(crate) struct There {
    pub share: u32,
    // This PC's own number for the ask, which the host's answer names.
    pub ask: u32,
    pub sharer: [u8; 32],
    pub name: String,
    pub allowed: bool,
    // An administrator window is in front on the sharer's PC.
    pub paused: bool,
}

// What the receive thread knows of an input packet besides what it says:
// the share it is for, the slot the host says it came from, its capture
// time on this PC's clock already, the shared monitor, and when it came.
pub(crate) struct Arrival {
    pub share: u32,
    pub slot: u8,
    pub captured: Option<(u64, bool)>,
    pub area: Option<Area>,
    pub now: Instant,
}

// Why an input packet goes no further on the PC controlled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotTaken {
    // Nothing of this share is allowed, or not from that slot.
    NotAllowed,
    // Older than one already taken.
    Late,
    // From the control that ended here within ENDED_GRACE: sent or passed
    // on before the other side heard of the end.
    JustEnded,
}

// The control of this PC that ended last, until when its input is let go
// quietly.
#[derive(Clone, Copy, Debug)]
struct Ended {
    share: u32,
    slot: u8,
    until: Instant,
}

// The room's side of control that both roles keep, under the state lock.
pub(crate) struct Remote {
    pub gate: Arc<Gate>,
    pub controls: Arc<Controls>,
    here: Option<Here>,
    there: Option<There>,
    ended: Option<Ended>,
    next_ask: u32,
    // Counted here: packets older than one taken, packets over the host's
    // limit, and cutoffs.
    late: u64,
    over_rate: u64,
    cutoffs: u64,
    // Input is what the link carries, not what the view shows, so its
    // numbers wait for the next view.
    published: Option<(u32, SocketAddr, u8)>,
}

impl Remote {
    pub(crate) fn new(gate: Arc<Gate>, controls: Arc<Controls>) -> Remote {
        Remote {
            gate,
            controls,
            here: None,
            there: None,
            ended: None,
            next_ask: 1,
            late: 0,
            over_rate: 0,
            cutoffs: 0,
            published: None,
        }
    }

    pub(crate) fn here(&self) -> Option<&Here> {
        self.here.as_ref()
    }

    // The request numbered `number`, while it is the one on show and not
    // answered yet. Allow answers only it: the one on show can change
    // between the owner seeing it and the click.
    pub(crate) fn request(&self, number: u32) -> Option<&Here> {
        self.here
            .as_ref()
            .filter(|here| here.number == number && !here.allowed)
    }

    pub(crate) fn there(&self) -> Option<&There> {
        self.there.as_ref()
    }

    // Someone asks to control this PC's share `share`. A request already on
    // show gives way: the host has only one at a time.
    pub(crate) fn asked(
        &mut self,
        number: u32,
        share: u32,
        controller: [u8; 32],
        slot: u8,
        name: String,
        now: Instant,
    ) {
        // The host ends one before it asks again, so this is a host that
        // did not; control ends all the same.
        if let Some(here) = self.here.take()
            && here.allowed
        {
            self.stopped_injecting(&here, ControlEnd::EndedByHost, now);
        }
        self.here = Some(Here {
            number,
            share,
            controller,
            slot,
            name,
            allowed: false,
            newest: None,
            last_input: None,
            cut: false,
            told_admin: false,
        });
    }

    // This PC's owner pressed Allow. The injector hears of it before any
    // input, and the cue plays in this PC's own speakers.
    pub(crate) fn allow(&mut self, area: Option<Area>) -> Option<&Here> {
        let here = self.here.as_mut().filter(|here| !here.allowed)?;
        here.allowed = true;
        let (number, name) = (here.number, here.name.clone());
        self.gate.push(Call::Started {
            number,
            started: Started { name, area },
        });
        self.gate.play(Cue::Rising);
        self.here.as_ref()
    }

    // Control of this PC is over, however it ended, or the request is gone.
    pub(crate) fn end_here(&mut self, why: ControlEnd, now: Instant) -> Option<Here> {
        let here = self.here.take()?;
        if here.allowed {
            self.stopped_injecting(&here, why, now);
        }
        Some(here)
    }

    fn stopped_injecting(&mut self, here: &Here, why: ControlEnd, now: Instant) {
        self.gate.push(Call::Ended {
            number: here.number,
            why,
        });
        self.gate.play(Cue::Falling);
        self.ended = Some(Ended {
            share: here.share,
            slot: here.slot,
            until: now + ENDED_GRACE,
        });
    }

    // Input from `slot` for `share` (None: this PC shares nothing now) is
    // from the control that just ended here.
    pub(crate) fn just_ended(&self, share: Option<u32>, slot: u8, now: Instant) -> bool {
        self.ended.is_some_and(|ended| {
            share.is_none_or(|share| share == ended.share)
                && slot == ended.slot
                && now < ended.until
        })
    }

    // This PC asks to control `share`; the number goes with the ask.
    pub(crate) fn ask(&mut self, share: u32, sharer: [u8; 32], name: String) -> u32 {
        let ask = self.next_ask;
        self.next_ask = self.next_ask.wrapping_add(1).max(1);
        self.there = Some(There {
            share,
            ask,
            sharer,
            name,
            allowed: false,
            paused: false,
        });
        ask
    }

    // The sharer allowed this PC's ask `ask`.
    pub(crate) fn allowed(&mut self, share: u32, ask: u32) -> Option<&There> {
        let there = self
            .there
            .as_mut()
            .filter(|there| there.share == share && there.ask == ask && !there.allowed)?;
        there.allowed = true;
        self.gate.play(Cue::Rising);
        self.there.as_ref()
    }

    // This PC's control, or its ask, is over. The cue plays only for
    // control that had started.
    pub(crate) fn end_there(&mut self) -> Option<There> {
        let there = self.there.take()?;
        if there.allowed {
            self.gate.play(Cue::Falling);
        }
        Some(there)
    }

    pub(crate) fn set_paused(&mut self, share: u32, ask: u32, paused: bool) -> bool {
        match self
            .there
            .as_mut()
            .filter(|there| there.share == share && there.ask == ask && there.allowed)
        {
            Some(there) if there.paused != paused => {
                there.paused = paused;
                true
            }
            _ => false,
        }
    }

    // An input packet, read into `events`, which it takes, leaving an empty
    // buffer in their place.
    pub(crate) fn input(
        &mut self,
        arrival: Arrival,
        head: &wire::Head,
        events: &mut Vec<InputEvent>,
    ) -> Result<(), NotTaken> {
        let Arrival {
            share,
            slot,
            captured,
            area,
            now,
        } = arrival;
        let Some(here) = self
            .here
            .as_mut()
            .filter(|here| here.allowed && here.share == share && here.slot == slot)
        else {
            wipe(events);
            if self.just_ended(Some(share), slot, now) {
                return Err(NotTaken::JustEnded);
            }
            return Err(NotTaken::NotAllowed);
        };
        if here
            .newest
            .is_some_and(|newest| head.seq.wrapping_sub(newest).wrapping_sub(1) >= 1 << 31)
        {
            wipe(events);
            self.late += 1;
            return Err(NotTaken::Late);
        }
        here.newest = Some(head.seq);
        here.last_input = Some(now);
        here.cut = false;
        let mut batch = self.gate.batch();
        batch.number = here.number;
        std::mem::swap(&mut batch.events, events);
        batch.held = head.held;
        batch.area = area;
        batch.captured = captured;
        batch.received = now;
        self.gate.push(Call::Input(batch));
        Ok(())
    }

    pub(crate) fn over_rate(&mut self) {
        self.over_rate += 1;
    }

    // Every timer pass: CUTOFF without a packet lets go of what is held.
    // True when it ran.
    pub(crate) fn step(&mut self, now: Instant) -> bool {
        let Some(here) = self.here.as_mut().filter(|here| here.allowed && !here.cut) else {
            return false;
        };
        let Some(last) = here.last_input else {
            return false;
        };
        if now.saturating_duration_since(last) < CUTOFF {
            return false;
        }
        here.cut = true;
        let number = here.number;
        self.gate.push(Call::CutOff { number });
        self.cutoffs += 1;
        true
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        let here = self
            .here
            .as_ref()
            .filter(|here| here.allowed && !here.cut)?;
        here.last_input.map(|last| last + CUTOFF)
    }

    // What the controller should hear now about an administrator window,
    // when that changed since it last heard.
    pub(crate) fn admin_news(&mut self) -> Option<(u32, u32, bool)> {
        let admin = self.gate.admin();
        let here = self
            .here
            .as_mut()
            .filter(|here| here.allowed && here.told_admin != admin)?;
        here.told_admin = admin;
        Some((here.share, here.number, admin))
    }

    // Where this PC's controller packets go now, or None while it controls
    // nothing: `link` is (the session's remote index, the address, the
    // prefix's slot) and `make` builds the link when that changed.
    pub(crate) fn publish(
        &mut self,
        link: Option<(u32, SocketAddr, u8)>,
        make: impl FnOnce() -> Option<ControlLink>,
    ) {
        let link = link.filter(|_| self.there.as_ref().is_some_and(|there| there.allowed));
        if link == self.published {
            return;
        }
        self.published = link;
        self.controls
            .set_link(if link.is_some() { make() } else { None });
    }

    pub(crate) fn fill(&self, numbers: &mut ControlNumbers) {
        numbers.cutoffs = self.cutoffs;
        numbers.dropped.late = self.late;
        numbers.dropped.host_over_rate = self.over_rate;
        numbers.packets_sent = self.controls.packets_sent();
        self.gate.fill(numbers);
    }

    // The request block and the rows: who asks and who controls, from this
    // PC's side. `controller` is who controls the share now as the room
    // knows it.
    pub(crate) fn view(&self, controller: Option<[u8; 32]>) -> crate::view::ControlView {
        let party = |key: [u8; 32], name: &str| Party {
            key,
            name: name.to_owned(),
        };
        let here = self.here.as_ref();
        let there = self.there.as_ref();
        crate::view::ControlView {
            controller,
            asked_by: here
                .filter(|here| !here.allowed)
                .map(|here| ControlRequest {
                    number: here.number,
                    key: here.controller,
                    name: here.name.clone(),
                }),
            controlled_by: here
                .filter(|here| here.allowed)
                .map(|here| party(here.controller, &here.name)),
            admin_here: here.is_some_and(|here| here.allowed && here.told_admin),
            asking: there
                .filter(|there| !there.allowed)
                .map(|there| there.share),
            controlling: there
                .filter(|there| there.allowed)
                .map(|there| party(there.sharer, &there.name)),
            paused: there.is_some_and(|there| there.allowed && there.paused),
        }
    }
}

// Where a person sits in a control session, for the system lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seat {
    Controlled,
    Controller,
    // The host, when it is neither.
    Host,
}

pub(crate) fn started_line(seat: Seat, controller: &str, sharer: &str) -> String {
    match seat {
        Seat::Controlled => format!("{controller} is controlling this PC."),
        Seat::Controller => format!("You are controlling {sharer}'s PC."),
        Seat::Host => format!("{controller} is controlling {sharer}'s PC."),
    }
}

pub(crate) fn ended_line(why: ControlEnd, seat: Seat, controller: &str, sharer: &str) -> String {
    let why_text = match (why, seat) {
        (ControlEnd::Released, Seat::Controlled) => {
            return format!("{controller} stopped controlling this PC.");
        }
        (ControlEnd::Released, Seat::Controller) => {
            return format!("You stopped controlling {sharer}'s PC.");
        }
        (ControlEnd::Released, Seat::Host) => format!("{controller} let go"),
        (ControlEnd::Stopped, Seat::Controlled) => String::from("you stopped it"),
        (ControlEnd::Stopped, _) => format!("{sharer} stopped it"),
        (ControlEnd::Panic, _) => String::from("the panic key"),
        (ControlEnd::EndedByHost, Seat::Host) => String::from("you ended it"),
        (ControlEnd::EndedByHost, _) => String::from("the host ended it"),
        (ControlEnd::ShareEnded, _) => String::from("the share ended"),
        (ControlEnd::SessionLost, _) => String::from("the connection was lost"),
        (ControlEnd::Closed, _) => String::from("the room closed"),
    };
    match seat {
        Seat::Host => format!("Control of {sharer}'s PC ended: {why_text}."),
        _ => format!("Control ended: {why_text}."),
    }
}

// The answers a controller can get, and what its panel says to each.
pub(crate) fn not_allowed_line(sharer: &str) -> String {
    format!("{sharer} did not allow control.")
}

// The one who asked first may control already or still wait for the
// answer; either way they are first.
pub(crate) fn busy_line(first: &str) -> String {
    format!("Could not ask for control: {first} asked first. One controller at a time.")
}

pub(crate) const TOO_SOON_LINE: &str =
    "Could not ask for control: asked too often. Wait a second and try again.";

// Only a build that gives the room no injector gets here; the request is
// declined.
pub(crate) const NO_INJECTOR_LINE: &str =
    "Could not allow control: this Booth cannot put input on this PC. The request was declined.";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    // What a fake injector heard, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Heard {
        Started(String),
        Input(Vec<InputEvent>, Held),
        CutOff,
        Ended(ControlEnd),
    }

    #[derive(Default)]
    struct Fake {
        heard: Mutex<Vec<Heard>>,
        admin: AtomicBool,
    }

    impl Injector for Fake {
        fn started(&self, started: &Started) {
            lock(&self.heard).push(Heard::Started(started.name.clone()));
        }

        fn inject(&self, input: &Injection<'_>) -> Injected {
            lock(&self.heard).push(Heard::Input(input.events.to_vec(), *input.held));
            Injected {
                sent: input.events.len() as u32,
                admin: self.admin.load(Ordering::Relaxed),
                ..Injected::default()
            }
        }

        fn cut_off(&self) {
            lock(&self.heard).push(Heard::CutOff);
        }

        fn ended(&self, why: ControlEnd) {
            lock(&self.heard).push(Heard::Ended(why));
        }
    }

    fn remote(fake: &Arc<Fake>, now: Instant) -> Remote {
        let clock = Clock::new(now);
        let injector: Arc<dyn Injector> = Arc::clone(fake) as Arc<dyn Injector>;
        let gate = Gate::new(Some(injector), clock);
        let wake = net::pace::Signal::new().expect("an event");
        Remote::new(gate, Controls::new(clock, None, wake))
    }

    fn head(seq: u32, held: Held) -> wire::Head {
        wire::Head {
            slot: 3,
            seq,
            captured: None,
            about: false,
            held,
        }
    }

    const A: ScanCode = ScanCode {
        code: 0x1E,
        e0: false,
    };

    fn press(key: ScanCode) -> InputEvent {
        InputEvent::Key { key, down: true }
    }

    fn heard(fake: &Fake) -> Vec<Heard> {
        std::mem::take(&mut *lock(&fake.heard))
    }

    // A packet numbered `seq` for share `share` from `slot`, as the receive
    // thread hands it over.
    fn feed(
        remote: &mut Remote,
        (share, slot, seq): (u32, u8, u32),
        held: Held,
        events: &mut Vec<InputEvent>,
        now: Instant,
    ) -> Result<(), NotTaken> {
        let arrival = Arrival {
            share,
            slot,
            captured: None,
            area: None,
            now,
        };
        remote.input(arrival, &head(seq, held), events)
    }

    #[test]
    fn injector_only_while_allowed() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        let mut events = vec![press(A)];
        remote.asked(7, 1, [9; 32], 3, String::from("Mara"), now);
        assert_eq!(
            feed(&mut remote, (1, 3, 0), Held::default(), &mut events, now),
            Err(NotTaken::NotAllowed)
        );
        assert!(events.is_empty(), "a refused packet's events are wiped");
        remote.allow(None).expect("allowed");
        let mut held = Held::default();
        held.set_key(A, true);
        let mut events = vec![press(A)];
        // From another slot, or for another share.
        for (share, slot) in [(1, 4), (2, 3)] {
            let mut copy = events.clone();
            assert_eq!(
                feed(&mut remote, (share, slot, 1), held, &mut copy, now),
                Err(NotTaken::NotAllowed)
            );
        }
        feed(&mut remote, (1, 3, 1), held, &mut events, now).expect("taken");
        assert!(events.is_empty(), "the events went to the gate");
        // Not newer than the one taken.
        let mut again = vec![press(A)];
        assert_eq!(
            feed(&mut remote, (1, 3, 1), held, &mut again, now),
            Err(NotTaken::Late)
        );
        remote.end_here(ControlEnd::Panic, now).expect("ended");
        // Let go quietly for a moment after the end, and not taken either.
        let mut after = vec![press(A)];
        assert_eq!(
            feed(&mut remote, (1, 3, 2), held, &mut after, now),
            Err(NotTaken::JustEnded)
        );
        let mut later = vec![press(A)];
        assert_eq!(
            feed(&mut remote, (1, 3, 3), held, &mut later, now + ENDED_GRACE),
            Err(NotTaken::NotAllowed)
        );
        assert!(after.is_empty() && later.is_empty());
        remote.gate.run();
        assert_eq!(
            heard(&fake),
            [
                Heard::Started(String::from("Mara")),
                Heard::Input(vec![press(A)], held),
                Heard::Ended(ControlEnd::Panic),
            ]
        );
    }

    // Input decided before an end but made after it, as when the timer
    // thread's end runs its calls first, never reaches the injector: calls go
    // in the order the room decided them, and none after the end.
    #[test]
    fn calls_in_order_none_after_end() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        let mut events = vec![press(A)];
        feed(&mut remote, (1, 3, 1), Held::default(), &mut events, now).expect("taken");
        remote.end_here(ControlEnd::Stopped, now);
        // A second session, and input from the first one's number decided
        // late: the gate drops it.
        remote.asked(2, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        remote.gate.push(Call::Input(Box::new(Batch {
            number: 1,
            events: vec![press(A)],
            held: Held::default(),
            area: None,
            captured: None,
            received: now,
        })));
        remote.gate.run();
        assert_eq!(
            heard(&fake),
            [
                Heard::Started(String::from("Mara")),
                Heard::Input(vec![press(A)], Held::default()),
                Heard::Ended(ControlEnd::Stopped),
                Heard::Started(String::from("Mara")),
            ]
        );
    }

    #[test]
    fn cutoff_once_per_silence() {
        let start = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, start);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), start);
        remote.allow(None);
        assert_eq!(remote.deadline(), None);
        assert!(!remote.step(start + CUTOFF * 2));
        let mut held = Held::default();
        held.set_key(A, true);
        let mut events = vec![press(A)];
        feed(&mut remote, (1, 3, 1), held, &mut events, start).expect("taken");
        assert_eq!(remote.deadline(), Some(start + CUTOFF));
        assert!(!remote.step(start + CUTOFF - Duration::from_millis(1)));
        assert!(remote.step(start + CUTOFF));
        assert!(!remote.step(start + CUTOFF * 3), "once per silence");
        assert_eq!(remote.deadline(), None);
        let later = start + CUTOFF * 4;
        feed(&mut remote, (1, 3, 2), held, &mut Vec::new(), later).expect("taken");
        assert!(remote.step(later + CUTOFF));
        remote.gate.run();
        let cuts = heard(&fake)
            .into_iter()
            .filter(|heard| *heard == Heard::CutOff)
            .count();
        assert_eq!(cuts, 2);
        let mut numbers = ControlNumbers::default();
        remote.fill(&mut numbers);
        assert_eq!(numbers.cutoffs, 2);
    }

    // The sequence wraps: newer is within half the range ahead.
    #[test]
    fn a_packet_is_newer_across_the_wrap() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        let take = |remote: &mut Remote, seq: u32| {
            feed(remote, (1, 3, seq), Held::default(), &mut Vec::new(), now)
        };
        assert!(take(&mut remote, u32::MAX - 1).is_ok());
        assert!(take(&mut remote, u32::MAX).is_ok());
        assert!(take(&mut remote, 0).is_ok());
        assert_eq!(take(&mut remote, u32::MAX), Err(NotTaken::Late));
        assert!(take(&mut remote, 5).is_ok());
        assert_eq!(take(&mut remote, 5), Err(NotTaken::Late));
    }

    #[test]
    fn admin_window_news_once() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 4, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        fake.admin.store(true, Ordering::Relaxed);
        feed(
            &mut remote,
            (4, 3, 1),
            Held::default(),
            &mut Vec::new(),
            now,
        )
        .expect("taken");
        remote.gate.run();
        assert_eq!(remote.admin_news(), Some((4, 1, true)));
        assert_eq!(remote.admin_news(), None);
        fake.admin.store(false, Ordering::Relaxed);
        feed(
            &mut remote,
            (4, 3, 2),
            Held::default(),
            &mut Vec::new(),
            now,
        )
        .expect("taken");
        remote.gate.run();
        assert_eq!(remote.admin_news(), Some((4, 1, false)));
    }

    #[test]
    fn held_keys_and_buttons() {
        let mut held = Held::default();
        assert!(held.is_empty());
        let right_ctrl = ScanCode {
            code: 0x1D,
            e0: true,
        };
        held.apply(&press(right_ctrl));
        held.apply(&InputEvent::Button {
            button: Button::Middle,
            down: true,
        });
        held.set_key(ScanCode { code: 0, e0: false }, true);
        held.set_key(
            ScanCode {
                code: 0xFF,
                e0: true,
            },
            true,
        );
        assert_eq!(held.keys().collect::<Vec<_>>(), [right_ctrl]);
        assert_eq!(held.buttons().collect::<Vec<_>>(), [Button::Middle]);
        assert!(!held.key(ScanCode {
            code: 0x1D,
            e0: false
        }));
        held.apply(&InputEvent::Key {
            key: right_ctrl,
            down: false,
        });
        held.set_button(Button::Middle, false);
        assert!(held.is_empty());
    }

    // The request on show can change between the owner reading it and the
    // click: an answer names its number, and a number that is not the one
    // on show, or one already allowed, answers nothing.
    #[test]
    fn an_answer_is_for_the_request_it_names() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(7, 1, [9; 32], 3, String::from("Mara"), now);
        let shown = remote.view(None).asked_by.expect("on show");
        assert_eq!((shown.number, shown.name.as_str()), (7, "Mara"));
        remote.end_here(ControlEnd::Released, now);
        remote.asked(8, 1, [8; 32], 4, String::from("Tom"), now);
        assert!(remote.request(7).is_none());
        assert_eq!(
            remote.request(8).map(|here| here.name.as_str()),
            Some("Tom")
        );
        remote.allow(None);
        assert!(remote.request(8).is_none(), "answered already");
    }

    // Input that waited past STALE for the injector goes with its held
    // state and none of its events, and is counted; the start and the end
    // around it are made as always.
    #[test]
    fn stale_input_presses_nothing() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        let mut held = Held::default();
        held.set_key(A, true);
        let long_ago = now.checked_sub(STALE * 2).expect("a time before now");
        let mut events = vec![press(A), InputEvent::Move { dx: 3, dy: 0 }];
        feed(&mut remote, (1, 3, 1), held, &mut events, long_ago).expect("taken");
        remote.end_here(ControlEnd::Stopped, now);
        remote.gate.run();
        assert_eq!(
            heard(&fake),
            [
                Heard::Started(String::from("Mara")),
                Heard::Input(Vec::new(), held),
                Heard::Ended(ControlEnd::Stopped),
            ]
        );
        let mut numbers = ControlNumbers::default();
        remote.fill(&mut numbers);
        assert_eq!(numbers.dropped.stale, 2);
        assert_eq!(numbers.injected, 0);
    }

    // While the injector is stuck, input waits for it MOST_WAITING packets
    // deep at most, and the oldest go first.
    #[test]
    fn stuck_injector_queue_cap() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        let extra = 5;
        for seq in 1..=(MOST_WAITING + extra) as u32 {
            let mut events = vec![InputEvent::Move {
                dx: seq as i32,
                dy: 0,
            }];
            feed(
                &mut remote,
                (1, 3, seq),
                Held::default(),
                &mut events,
                Instant::now(),
            )
            .expect("taken");
        }
        remote.gate.run();
        let moves: Vec<Vec<InputEvent>> = heard(&fake)
            .into_iter()
            .filter_map(|heard| match heard {
                Heard::Input(events, _) => Some(events),
                _ => None,
            })
            .collect();
        assert_eq!(moves.len(), MOST_WAITING);
        // On a slow run some may have gone stale too, and lost their events.
        let oldest_left = moves
            .iter()
            .flatten()
            .map(|event| match event {
                InputEvent::Move { dx, .. } => *dx,
                _ => 0,
            })
            .min();
        assert!(
            oldest_left.is_none_or(|dx| dx > extra as i32),
            "{oldest_left:?}"
        );
        let mut numbers = ControlNumbers::default();
        remote.fill(&mut numbers);
        assert!(numbers.dropped.stale >= extra as u64, "{numbers:?}");
    }

    // What the controller sent, or the host passed on, before it heard of
    // an end reaches this PC after it: let go quietly for ENDED_GRACE, from
    // that share and slot only.
    #[test]
    fn input_after_end_dropped_quietly() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        remote.end_here(ControlEnd::Panic, now);
        let soon = now + Duration::from_millis(10);
        let take = |remote: &mut Remote, (share, slot): (u32, u8), at: Instant| {
            let mut events = vec![press(A)];
            feed(remote, (share, slot, 9), Held::default(), &mut events, at)
        };
        assert_eq!(take(&mut remote, (1, 3), soon), Err(NotTaken::JustEnded));
        assert_eq!(take(&mut remote, (1, 4), soon), Err(NotTaken::NotAllowed));
        assert_eq!(take(&mut remote, (2, 3), soon), Err(NotTaken::NotAllowed));
        assert!(
            remote.just_ended(None, 3, soon),
            "once the share is over too"
        );
        assert_eq!(
            take(&mut remote, (1, 3), now + ENDED_GRACE),
            Err(NotTaken::NotAllowed)
        );
        // A request that was never allowed ended nothing.
        remote.asked(2, 1, [7; 32], 5, String::from("Tom"), now);
        remote.end_here(ControlEnd::Stopped, now);
        assert_eq!(take(&mut remote, (1, 5), soon), Err(NotTaken::NotAllowed));
        remote.gate.run();
        assert!(
            !heard(&fake)
                .iter()
                .any(|heard| matches!(heard, Heard::Input(..))),
            "nothing injected"
        );
    }

    // The latencies are worked out by the timer thread's refresh, never on
    // the way to the injector, and a new session's start clears them.
    #[test]
    fn latencies_from_refresh() {
        let now = Instant::now();
        let fake = Arc::new(Fake::default());
        let mut remote = remote(&fake, now);
        remote.asked(1, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        let mut events = vec![press(A)];
        feed(
            &mut remote,
            (1, 3, 1),
            Held::default(),
            &mut events,
            Instant::now(),
        )
        .expect("taken");
        remote.gate.run();
        let numbers = |remote: &Remote| {
            let mut numbers = ControlNumbers::default();
            remote.fill(&mut numbers);
            numbers
        };
        assert_eq!(numbers(&remote).receive_to_inject, None);
        remote.gate.refresh(Instant::now());
        assert!(numbers(&remote).receive_to_inject.is_some());
        assert!(numbers(&remote).inject_call.is_some());
        remote.end_here(ControlEnd::Stopped, now);
        remote.asked(2, 1, [9; 32], 3, String::from("Mara"), now);
        remote.allow(None);
        remote.gate.run();
        assert_eq!(numbers(&remote).receive_to_inject, None);
    }

    proptest::proptest! {
        #[test]
        fn spread_matches_share(
            values in proptest::collection::vec(-5.0f32..1000.0, 0..400),
        ) {
            proptest::prop_assert_eq!(
                spread(&mut values.clone()),
                share::spread(&mut values.clone())
            );
        }
    }

    #[test]
    fn every_end_has_its_line_from_each_seat() {
        let ends = [
            ControlEnd::Released,
            ControlEnd::Stopped,
            ControlEnd::Panic,
            ControlEnd::EndedByHost,
            ControlEnd::ShareEnded,
            ControlEnd::SessionLost,
            ControlEnd::Closed,
        ];
        for why in ends {
            for seat in [Seat::Controlled, Seat::Controller, Seat::Host] {
                let line = ended_line(why, seat, "Mara", "Ines");
                assert!(line.ends_with('.') && !line.contains('!'), "{line}");
                assert!(!line.contains('\u{2014}'), "{line}");
            }
        }
        assert_eq!(
            ended_line(ControlEnd::Panic, Seat::Controlled, "Mara", "Ines"),
            "Control ended: the panic key."
        );
        assert_eq!(
            started_line(Seat::Controlled, "Mara", "Ines"),
            "Mara is controlling this PC."
        );
        assert_eq!(
            ended_line(ControlEnd::Stopped, Seat::Controller, "Mara", "Ines"),
            "Control ended: Ines stopped it."
        );
    }
}
