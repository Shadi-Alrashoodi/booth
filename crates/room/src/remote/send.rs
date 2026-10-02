// The controller's way out (thread "room control send"): what the app's
// capture hands to Controls is sealed and sent from the caller's own thread
// with no lock of the room's, as the sharer's Outbox does video. Key and
// button events go at once, with any motion waiting in front of them, so the
// order holds. Mouse moves and the wheel go at once too unless a packet left
// less than MOVE_EVERY ago; then they add up and this thread sends them when
// MOVE_EVERY is up. While this PC controls, it also sends the held state
// every STATE_EVERY, so the other PC can let go of anything a lost packet
// left down.
//
// Why coalesce: a gaming mouse reports up to 8000 times a second, each one a
// packet of about 150 bytes on the wire and a decrypt and a SendInput on the
// other PC. At one packet per 2 ms that is 500 at most, 0.6 Mbit/s, and a
// move waits 2 ms at the most, under half a frame at 240 fps; the first one
// after a pause waits for nothing.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use channels::Channel;
use net::pace::{self, Signal, Timer};
use session::Sealer;
use zeroize::Zeroize;

use super::wire::{self, MAX_EVENTS, MAX_INPUT};
use super::{Held, InputEvent, ScanCode, wipe};
use crate::log::{Log, log};
use crate::peer::Clock;
use crate::socket::Socket;

pub const MOVE_EVERY: Duration = Duration::from_millis(2);
pub const STATE_EVERY: Duration = Duration::from_millis(100);

// Where a controller's packets go while it controls: the host, or on the
// host the sharer's link, with the prefix that side takes.
pub(crate) struct Link {
    pub sealer: Sealer,
    pub to: SocketAddr,
    pub kind: u8,
    pub slot: u8,
}

// Where the controller's mouse comes from while its viewer captures: points
// from the viewer's own window (absolute mode, the desktop), or the input
// crate's raw motion (relative mode, a game).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Aim {
    Points,
    Motion,
}

// Who hands an event over: a test playing the capture by hand, the app's
// feed of keys and raw mouse, or the viewer's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum From {
    Anyone,
    Feed,
    Viewer,
}

struct Out {
    link: Option<Link>,
    // None while the viewer captures nothing: the feed and the viewer are
    // then refused whatever they hand over.
    aim: Option<Aim>,
    seq: u32,
    held: Held,
    // Not sent yet: motion adding up, or what did not fit the last packet.
    waiting: Vec<InputEvent>,
    // When the oldest of them was captured, on the ping clock.
    oldest_us: Option<u64>,
    last_sent: Option<Instant>,
    plain: Vec<u8>,
    sealed: Vec<u8>,
}

// Hands the controller's input to the room. The app's capture calls it,
// from any thread, only while this PC controls and its viewer is in front.
pub struct Controls {
    clock: Clock,
    socket: Option<Arc<Socket>>,
    out: Mutex<Out>,
    wake: Signal,
    closing: AtomicBool,
    packets: AtomicU64,
}

impl Controls {
    pub(crate) fn new(clock: Clock, socket: Option<Arc<Socket>>, wake: Signal) -> Arc<Controls> {
        Arc::new(Controls {
            clock,
            socket,
            out: Mutex::new(Out {
                link: None,
                aim: None,
                seq: 0,
                held: Held::default(),
                waiting: Vec::with_capacity(MAX_EVENTS),
                oldest_us: None,
                last_sent: None,
                plain: Vec::with_capacity(1 + MAX_INPUT),
                sealed: Vec::with_capacity(1 + MAX_INPUT + session::DATA_OVERHEAD),
            }),
            wake,
            closing: AtomicBool::new(false),
            packets: AtomicU64::new(0),
        })
    }

    // This PC controls someone's share now, as the room knows it. Nothing
    // is sent otherwise, whatever the app hands over.
    pub fn controlling(&self) -> bool {
        lock(&self.out).link.is_some()
    }

    // The app's capture: keys and raw mouse from the input crate's feed,
    // oldest first, captured at `at`. They go only while the viewer
    // captures (set_capture), and the raw mouse only in relative mode; in
    // absolute mode the mouse goes as the viewer's points.
    pub fn captured(&self, events: &[InputEvent], at: Instant) {
        self.take_in(events, at, From::Feed);
    }

    // The viewer's points, clicks and wheel in absolute mode.
    pub(crate) fn pointed(&self, events: &[InputEvent], at: Instant) {
        self.take_in(events, at, From::Viewer);
    }

    // For tests that play the capture by hand: whatever it is handed goes
    // while this PC controls, the viewer or not.
    #[doc(hidden)]
    pub fn send(&self, events: &[InputEvent], at: Instant) {
        self.take_in(events, at, From::Anyone);
    }

    // From the viewer, through the room: capture runs with its mouse from
    // `aim`, or no longer. A control that is not running has no capture to
    // start. Whether capture ran before, and whether it runs now.
    pub(crate) fn set_capture(&self, aim: Option<Aim>) -> (bool, bool) {
        let mut out = lock(&self.out);
        let aim = aim.filter(|_| out.link.is_some());
        let before = std::mem::replace(&mut out.aim, aim);
        (before.is_some(), aim.is_some())
    }

    fn take_in(&self, events: &[InputEvent], at: Instant, from: From) {
        let now = Instant::now();
        let at_us = self.clock.micros(at);
        let mut out = lock(&self.out);
        if out.link.is_none() {
            return;
        }
        let mut urgent = false;
        for event in events {
            // Raw Input reports a keyboard overrun as make code 0xFF. It is no
            // key, and the host would refuse the whole packet around it,
            // clicks and key-ups too.
            if let InputEvent::Key { key, .. } = event
                && !ScanCode::possible(key.code)
            {
                continue;
            }
            if !admits(from, out.aim, &out.held, event) {
                continue;
            }
            if out.waiting.len() >= MAX_EVENTS {
                self.flush(&mut out, now);
            }
            match *event {
                InputEvent::Key { .. } | InputEvent::Button { .. } => {
                    out.held.apply(event);
                    out.waiting.push(*event);
                    urgent = true;
                }
                motion => add_motion(&mut out.waiting, motion),
            }
            out.oldest_us = Some(out.oldest_us.map_or(at_us, |oldest| oldest.min(at_us)));
        }
        if out.waiting.is_empty() {
            out.oldest_us = None;
            return;
        }
        let due = urgent
            || out
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= MOVE_EVERY);
        if due {
            self.flush(&mut out, now);
        } else {
            drop(out);
            self.wake.set();
        }
    }

    // The capture stopped: the viewer lost the focus, or control is ending.
    // Nothing is held any more, and a packet says so now.
    pub fn let_go(&self) {
        let mut out = lock(&self.out);
        if out.link.is_none() {
            return;
        }
        out.held = Held::default();
        self.flush(&mut out, Instant::now());
    }

    pub(crate) fn packets_sent(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }

    // From the room, under the state lock. A control that starts starts
    // with nothing held; a rekey only changes the link.
    pub(crate) fn set_link(&self, link: Option<Link>) {
        let mut out = lock(&self.out);
        let starts = out.link.is_none() && link.is_some();
        if link.is_none() || starts {
            out.held = Held::default();
            out.aim = None;
            wipe(&mut out.waiting);
            out.oldest_us = None;
            out.last_sent = None;
        }
        out.link = link;
        drop(out);
        self.wake.set();
    }

    pub(crate) fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.wake.set();
    }

    // The thread's turn: motion that waited MOVE_EVERY, or the held state
    // when nothing went for STATE_EVERY. When to look again, if ever.
    fn tick(&self, now: Instant) -> Option<Instant> {
        let mut out = lock(&self.out);
        out.link.as_ref()?;
        let every = |out: &Out| {
            if out.waiting.is_empty() {
                STATE_EVERY
            } else {
                MOVE_EVERY
            }
        };
        let due = out
            .last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= every(&out));
        if due {
            self.flush(&mut out, now);
        }
        Some(out.last_sent.unwrap_or(now) + every(&out))
    }

    // One packet: what waits, up to MAX_EVENTS, and the held state. A
    // failed seal or send is a dead link; the room hears of that from the
    // silence, as with every other packet.
    fn flush(&self, out: &mut Out, now: Instant) {
        let Out {
            link,
            aim: _,
            seq,
            held,
            waiting,
            oldest_us,
            last_sent,
            plain,
            sealed,
        } = out;
        let Some(link) = link.as_ref() else {
            return;
        };
        let count = waiting.len().min(MAX_EVENTS);
        let captured = oldest_us.unwrap_or_else(|| self.clock.micros(now));
        plain.clear();
        plain.push(Channel::Input as u8);
        wire::write_input(
            link.kind,
            link.slot,
            *seq,
            captured,
            held,
            &waiting[..count],
            plain,
        );
        waiting[..count].fill(InputEvent::BLANK);
        waiting.drain(..count);
        *oldest_us = (!waiting.is_empty()).then_some(captured);
        *seq = seq.wrapping_add(1);
        *last_sent = Some(now);
        let sealed_ok = link.sealer.seal(plain, sealed).is_ok();
        plain.zeroize();
        if !sealed_ok {
            return;
        }
        if let Some(socket) = &self.socket
            && socket.send_to(sealed, link.to).is_ok()
        {
            self.packets.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// Whether an event from `from` goes while the viewer captures with `aim`.
// Keys come from the feed; the mouse from where the aim says, so a click is
// never sent twice, once from each. A button held comes up from either, so
// a switch of aim in the middle of a click never leaves it down.
fn admits(from: From, aim: Option<Aim>, held: &Held, event: &InputEvent) -> bool {
    if from == From::Anyone {
        return true;
    }
    let Some(aim) = aim else {
        return false;
    };
    let mouse_from = match aim {
        Aim::Points => From::Viewer,
        Aim::Motion => From::Feed,
    };
    match *event {
        InputEvent::Key { .. } => from == From::Feed,
        InputEvent::Button {
            button,
            down: false,
        } if held.button(button) => true,
        InputEvent::At { .. } => from == From::Viewer && aim == Aim::Points,
        InputEvent::Move { .. } => from == From::Feed && aim == Aim::Motion,
        InputEvent::Button { .. } | InputEvent::Wheel { .. } | InputEvent::HWheel { .. } => {
            from == mouse_from
        }
    }
}

// Motion adds up while it waits: moves into the move before them, a newer
// point replaces the one before it, turns of the wheel add up. Each stays
// within what one event on the wire carries, and nothing that adds up to no
// motion at all is sent.
fn add_motion(waiting: &mut Vec<InputEvent>, event: InputEvent) {
    let fits = |value: i32| i16::try_from(value).is_ok();
    match (waiting.last_mut(), event) {
        (Some(InputEvent::Move { dx, dy }), InputEvent::Move { dx: x, dy: y })
            if fits(*dx + x) && fits(*dy + y) =>
        {
            *dx += x;
            *dy += y;
            if *dx == 0 && *dy == 0 {
                waiting.pop();
            }
        }
        (Some(InputEvent::At { x, y }), InputEvent::At { x: to_x, y: to_y }) => {
            *x = to_x;
            *y = to_y;
        }
        (Some(InputEvent::Wheel { delta }), InputEvent::Wheel { delta: more })
        | (Some(InputEvent::HWheel { delta }), InputEvent::HWheel { delta: more })
            if fits(*delta + more) =>
        {
            *delta += more;
            if *delta == 0 {
                waiting.pop();
            }
        }
        (_, InputEvent::Move { dx, dy }) => {
            let (mut dx, mut dy) = (dx, dy);
            while dx != 0 || dy != 0 {
                let (x, y) = (step(dx), step(dy));
                waiting.push(InputEvent::Move { dx: x, dy: y });
                dx -= x;
                dy -= y;
            }
        }
        (_, InputEvent::Wheel { delta }) => split_wheel(waiting, delta, false),
        (_, InputEvent::HWheel { delta }) => split_wheel(waiting, delta, true),
        (_, other) => waiting.push(other),
    }
}

fn step(value: i32) -> i32 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX))
}

fn split_wheel(waiting: &mut Vec<InputEvent>, delta: i32, horizontal: bool) {
    let mut delta = delta;
    while delta != 0 {
        let part = step(delta);
        waiting.push(if horizontal {
            InputEvent::HWheel { delta: part }
        } else {
            InputEvent::Wheel { delta: part }
        });
        delta -= part;
    }
}

pub(crate) fn start(controls: Arc<Controls>, log: Log) -> io::Result<JoinHandle<()>> {
    let timer = Timer::new()?;
    thread::Builder::new()
        .name("room control send".into())
        .spawn(move || run(&controls, &timer, &log))
}

fn run(controls: &Controls, timer: &Timer, log: &Log) {
    // A game can keep every core busy, and at normal priority this thread
    // would wait for a time slice after its timer fires, 6 to 24 ms at the
    // 99th percentile by tests/timer.rs, where it has 2 to hand motion on.
    if let Err(err) = pace::raise_priority() {
        log!(
            log,
            "control send: {err}; mouse moves can go several milliseconds late while every cpu is busy"
        );
    }
    let mut failed = false;
    loop {
        if controls.closing.load(Ordering::Acquire) {
            return;
        }
        let next = controls.tick(Instant::now());
        let waited = match next {
            Some(at) => timer
                .set_at(at)
                .and_then(|()| pace::wait(&controls.wake, Some(timer))),
            None => pace::wait(&controls.wake, None),
        };
        if let Err(err) = waited {
            if !std::mem::replace(&mut failed, true) {
                log!(
                    log,
                    "control send: {err}; looking every {} ms instead",
                    MOVE_EVERY.as_millis()
                );
            }
            thread::sleep(MOVE_EVERY);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::remote::{Button, ScanCode};

    fn moved(dx: i32, dy: i32) -> InputEvent {
        InputEvent::Move { dx, dy }
    }

    #[test]
    fn motion_adds_up() {
        let mut waiting = Vec::new();
        add_motion(&mut waiting, moved(3, -2));
        add_motion(&mut waiting, moved(4, 1));
        assert_eq!(waiting, [moved(7, -1)]);
        add_motion(&mut waiting, InputEvent::At { x: 1, y: 2 });
        add_motion(&mut waiting, InputEvent::At { x: 5, y: 6 });
        add_motion(&mut waiting, InputEvent::Wheel { delta: 120 });
        add_motion(&mut waiting, InputEvent::Wheel { delta: 240 });
        assert_eq!(
            waiting,
            [
                moved(7, -1),
                InputEvent::At { x: 5, y: 6 },
                InputEvent::Wheel { delta: 360 }
            ]
        );
        // Back where it started: nothing to send.
        add_motion(&mut waiting, InputEvent::Wheel { delta: -360 });
        assert_eq!(waiting.len(), 2);
        let mut waiting = vec![moved(1, 1)];
        add_motion(&mut waiting, moved(-1, -1));
        assert!(waiting.is_empty());
        // A flick larger than one event carries.
        add_motion(&mut waiting, moved(70_000, -5));
        assert_eq!(
            waiting,
            [moved(32_767, -5), moved(32_767, 0), moved(4_466, 0)]
        );
        let mut waiting = vec![moved(32_000, 0)];
        add_motion(&mut waiting, moved(1_000, 0));
        assert_eq!(waiting, [moved(32_000, 0), moved(1_000, 0)]);
    }

    // A made-up link: sealed with a real session's keys and sent nowhere,
    // since the Controls has no socket.
    fn link() -> Link {
        link_and_reader().0
    }

    // With the other end of its session, which opens what it sealed.
    pub(crate) fn link_and_reader() -> (Link, session::Session) {
        let (session, reader) = crate::testing::session_pair();
        let link = Link {
            sealer: session.sealer().expect("a confirmed session"),
            to: "127.0.0.1:9".parse().expect("an address"),
            kind: wire::SENT,
            slot: 0,
        };
        (link, reader)
    }

    fn controls() -> Arc<Controls> {
        let now = Instant::now();
        Controls::new(Clock::new(now), None, Signal::new().expect("an event"))
    }

    const A: ScanCode = ScanCode {
        code: 0x1E,
        e0: false,
    };

    #[test]
    fn keys_at_once_moves_wait() {
        let controls = controls();
        let now = Instant::now();
        controls.send(&[InputEvent::Key { key: A, down: true }], now);
        assert_eq!(controls.lock_seq(), 0, "nothing goes before control starts");
        controls.set_link(Some(link()));
        assert!(controls.controlling());
        controls.send(&[moved(1, 0)], now);
        assert_eq!(controls.lock_seq(), 1, "the first move goes at once");
        controls.send(&[moved(1, 0)], now);
        controls.send(&[moved(2, 0)], now);
        assert_eq!(controls.lock_seq(), 1, "the next ones wait");
        assert_eq!(lock(&controls.out).waiting, [moved(3, 0)]);
        controls.send(
            &[InputEvent::Button {
                button: Button::Left,
                down: true,
            }],
            now,
        );
        assert_eq!(
            controls.lock_seq(),
            2,
            "a click goes at once, the move with it"
        );
        assert!(lock(&controls.out).waiting.is_empty());
        assert!(lock(&controls.out).held.button(Button::Left));
        // The thread's turn, MOVE_EVERY later, sends what waited.
        controls.send(&[moved(5, 5)], now);
        let later = Instant::now() + MOVE_EVERY;
        let next = controls.tick(later).expect("a next look");
        assert_eq!(controls.lock_seq(), 3);
        assert_eq!(next, later + STATE_EVERY);
        // Nothing for STATE_EVERY: the held state goes alone.
        controls.tick(later + STATE_EVERY);
        assert_eq!(controls.lock_seq(), 4);
        controls.let_go();
        assert_eq!(controls.lock_seq(), 5);
        assert!(lock(&controls.out).held.is_empty());
        controls.set_link(None);
        assert!(!controls.controlling());
        assert_eq!(controls.tick(later), None);
    }

    #[test]
    fn burst_spans_packets() {
        let controls = controls();
        controls.set_link(Some(link()));
        let events: Vec<InputEvent> = (0..MAX_EVENTS + 5)
            .map(|i| InputEvent::Key {
                key: ScanCode {
                    code: 1 + i as u8,
                    e0: false,
                },
                down: true,
            })
            .collect();
        controls.send(&events, Instant::now());
        assert_eq!(controls.lock_seq(), 2);
        assert!(lock(&controls.out).waiting.is_empty());
        assert_eq!(lock(&controls.out).held.keys().count(), MAX_EVENTS + 5);
    }

    // A keyboard overrun among real keys: it goes nowhere, and the host is
    // not handed a packet it would refuse whole, with the click beside it.
    #[test]
    fn overrun_key_left_out() {
        let controls = controls();
        let (link, mut reader) = link_and_reader();
        controls.set_link(Some(link));
        let overrun = |e0| InputEvent::Key {
            key: ScanCode { code: 0xFF, e0 },
            down: true,
        };
        let click = InputEvent::Button {
            button: Button::Left,
            down: true,
        };
        let zero = InputEvent::Key {
            key: ScanCode { code: 0, e0: false },
            down: true,
        };
        let a = InputEvent::Key { key: A, down: true };
        controls.send(
            &[overrun(false), click, zero, a, overrun(true)],
            Instant::now(),
        );
        assert_eq!(controls.lock_seq(), 1);
        let sealed = lock(&controls.out).sealed.clone();
        let mut plain = Vec::new();
        reader.decrypt(&sealed, &mut plain).expect("it opens");
        assert_eq!(plain[0], Channel::Input as u8);
        let mut events = Vec::new();
        let head =
            wire::read_input(&plain[1..], wire::SENT, &mut events).expect("the host takes it");
        assert_eq!(events, [click, a]);
        assert_eq!(head.held.keys().collect::<Vec<_>>(), [A]);
        assert!(head.held.button(Button::Left));
        // Nothing but an overrun: nothing to send.
        controls.send(&[overrun(false)], Instant::now());
        assert_eq!(controls.lock_seq(), 1);
    }

    impl Controls {
        fn lock_seq(&self) -> u32 {
            lock(&self.out).seq
        }
    }

    // Packets sent so far.
    pub(crate) fn sent(controls: &Controls) -> u32 {
        controls.lock_seq()
    }

    // The last packet sent, as the host reads it: its events and the held
    // state after them.
    pub(crate) fn last(
        controls: &Controls,
        reader: &mut session::Session,
    ) -> (Vec<InputEvent>, Held) {
        let sealed = lock(&controls.out).sealed.clone();
        let mut plain = Vec::new();
        reader.decrypt(&sealed, &mut plain).expect("it opens");
        assert_eq!(plain[0], Channel::Input as u8);
        let mut events = Vec::new();
        let head =
            wire::read_input(&plain[1..], wire::SENT, &mut events).expect("the host takes it");
        (events, head.held)
    }

    // What the capture's aim lets through, from the feed and the viewer.
    #[test]
    fn admits_by_aim() {
        let none = Held::default();
        let mut left_held = Held::default();
        left_held.set_button(Button::Left, true);
        let key = InputEvent::Key { key: A, down: true };
        let at = InputEvent::At { x: 5, y: 5 };
        let moved = moved(1, 1);
        let click = InputEvent::Button {
            button: Button::Left,
            down: true,
        };
        let release = InputEvent::Button {
            button: Button::Left,
            down: false,
        };
        let wheel = InputEvent::Wheel { delta: 120 };
        for event in [key, at, moved, click, wheel] {
            assert!(admits(From::Anyone, None, &none, &event), "{event:?}");
            assert!(!admits(From::Feed, None, &none, &event), "{event:?}");
            assert!(!admits(From::Viewer, None, &none, &event), "{event:?}");
        }
        let points = Some(Aim::Points);
        let motion = Some(Aim::Motion);
        let feed = |aim, event| admits(From::Feed, aim, &none, &event);
        let viewer = |aim, event| admits(From::Viewer, aim, &none, &event);
        assert!(feed(points, key) && feed(motion, key));
        assert!(!viewer(points, key) && !viewer(motion, key));
        assert!(viewer(points, at) && !feed(points, at) && !viewer(motion, at));
        assert!(feed(motion, moved) && !feed(points, moved) && !viewer(motion, moved));
        for event in [click, wheel] {
            assert!(viewer(points, event) && !feed(points, event), "{event:?}");
            assert!(feed(motion, event) && !viewer(motion, event), "{event:?}");
        }
        // A button that is down comes up from either side.
        assert!(!feed(points, release));
        assert!(admits(From::Feed, points, &left_held, &release));
        assert!(admits(From::Viewer, motion, &left_held, &release));
    }
}
