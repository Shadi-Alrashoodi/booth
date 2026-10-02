// The controller's side: the keys and mouse the viewer sends to the PC it
// controls. The process has one keyboard registration, the hotkey window's,
// so the keys come from the hotkey thread, and so does the mouse while
// sending. Events are handed on as they come and never kept.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use crate::bindings::Action;
use crate::key::Key;
use crate::remote::Shared;
use crate::tracker::{Event, INJECTED, RawKey};

// A little over 100 ms of an 8000 Hz mouse. The viewer takes events as they
// come; this far behind it has stalled, and events are dropped rather than
// kept for it.
pub(crate) const FEED_ROOM: usize = 1024;

// Input this old when the viewer reads it waited behind a viewer that
// stalled. Sent now it would land on the other PC late and in a burst, maybe
// in another window than the one it was meant for, so it is dropped the way
// a late frame is. Well past the 6 to 24 ms a thread at normal priority can
// wait for the CPU behind a game.
pub(crate) const FEED_LATE: Duration = Duration::from_millis(100);

const MOUSE_MOVE_ABSOLUTE: u16 = 1;
const RI_MOUSE_WHEEL: u16 = 0x400;
const RI_MOUSE_HWHEEL: u16 = 0x800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Captured {
    // Key::scan() is the make code with its E0 or E1 prefix.
    Key { key: Key, down: bool, at: Instant },
    Mouse { mouse: Mouse, at: Instant },
    // This many events were lost just before the next one: the feed was
    // full, or they had waited longer than FEED_LATE. What the viewer thinks
    // is held may be wrong.
    Missed(u32),
}

impl Captured {
    // When the hotkey thread read it from Windows: the controller's capture
    // time.
    pub fn at(&self) -> Option<Instant> {
        match *self {
            Captured::Key { at, .. } | Captured::Mouse { at, .. } => Some(at),
            Captured::Missed(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mouse {
    // In the mouse's own counts, before Windows' pointer speed.
    pub dx: i32,
    pub dy: i32,
    // Mouse::LEFT and the others, pressed and let go in this report.
    pub pressed: u8,
    pub released: u8,
    // 120 a notch: positive is away from the user, and right on the tilt
    // wheel.
    pub wheel: i16,
    pub hwheel: i16,
}

impl Mouse {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 4;
    pub const BACK: u8 = 8;
    pub const FORWARD: u8 = 16;
}

// One mouse event as Raw Input reports it: RAWMOUSE's fields and the device
// handle from its header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawMouse {
    pub(crate) flags: u16,
    pub(crate) buttons: u16,
    pub(crate) data: u16,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) device: usize,
}

impl RawMouse {
    // None when there is nothing in it to send.
    pub(crate) fn mouse(&self) -> Option<Mouse> {
        // Pens, touch screens and remote desktop report where the pointer
        // is, not how far it moved. Relative mode is for mice.
        let (dx, dy) = if self.flags & MOUSE_MOVE_ABSOLUTE != 0 {
            (0, 0)
        } else {
            (self.x, self.y)
        };
        let mut mouse = Mouse {
            dx,
            dy,
            ..Mouse::default()
        };
        // Raw Input's button flags are down and up pairs, left button first.
        let order = [
            Mouse::LEFT,
            Mouse::RIGHT,
            Mouse::MIDDLE,
            Mouse::BACK,
            Mouse::FORWARD,
        ];
        for (i, button) in order.into_iter().enumerate() {
            if self.buttons & (1 << (2 * i)) != 0 {
                mouse.pressed |= button;
            }
            if self.buttons & (2 << (2 * i)) != 0 {
                mouse.released |= button;
            }
        }
        // Both wheels share the one data field, so a report has one or the
        // other.
        let turn = self.data as i16;
        if self.buttons & RI_MOUSE_WHEEL != 0 {
            mouse.wheel = turn;
        } else if self.buttons & RI_MOUSE_HWHEEL != 0 {
            mouse.hwheel = turn;
        }
        (mouse != Mouse::default()).then_some(mouse)
    }
}

// On its way to the viewer: the event, the sending period it was decided
// in, and its number.
pub(crate) type Queued = (Captured, u32, u32);

// Which window is in front: GetForegroundWindow, or a stand-in in tests.
pub(crate) type Front = Arc<dyn Fn() -> isize + Send + Sync>;

// The sending end, for the Raw Input side and the Windows keys hook alike.
#[derive(Clone)]
pub(crate) struct Sink {
    sender: SyncSender<Queued>,
    shared: Arc<Shared>,
    front: Front,
}

impl Sink {
    pub(crate) fn new(sender: SyncSender<Queued>, shared: Arc<Shared>, front: Front) -> Sink {
        Sink {
            sender,
            shared,
            front,
        }
    }

    // The period to send in: Some while sending and the viewer's own window
    // is in front.
    pub(crate) fn open(&self) -> Option<u32> {
        self.shared
            .sending_period()
            .filter(|_| self.viewer_in_front())
    }

    // As open, while the Windows keys go to the feed too.
    pub(crate) fn windows_keys(&self) -> Option<u32> {
        if !self.shared.windows_keys() {
            return None;
        }
        self.open()
    }

    // Never waits: the hotkey thread also answers push to talk, the panic
    // key and, while it is in, the hook every key on this PC waits for.
    pub(crate) fn send(&self, event: Captured, period: u32) {
        let sequence = self.shared.next_sequence();
        if self.sender.try_send((event, period, sequence)).is_err() {
            self.shared.dropped(1);
        }
    }

    // A viewer that never said which window is its own has none in front.
    fn viewer_in_front(&self) -> bool {
        let viewer = self.shared.viewer();
        viewer != 0 && (self.front)() == viewer
    }
}

// The hotkey thread's end: what of each event goes to the feed.
pub(crate) struct Tap {
    sink: Sink,
    // Keys whose press stayed on this PC, so their repeats and their
    // release stay too.
    kept: Vec<Key>,
    // The sending period `kept` belongs to.
    period: Option<u32>,
}

impl Tap {
    pub(crate) fn new(sink: Sink) -> Tap {
        Tap {
            sink,
            kept: Vec::new(),
            period: None,
        }
    }

    pub(crate) fn sink(&self) -> Sink {
        self.sink.clone()
    }

    // `fired` is what the hotkeys made of this event, and `hook_took` says
    // the Windows keys hook sent it already.
    pub(crate) fn key(&mut self, raw: RawKey, fired: &[Event], hook_took: bool) {
        let Some(period) = self.follow() else {
            return;
        };
        if self.injected_by_its_controller(raw.device) {
            return;
        }
        let Some(key) = raw.key() else {
            return;
        };
        let down = raw.down();
        if let Some(at) = self.kept.iter().position(|kept| *kept == key) {
            if !down {
                self.kept.remove(at);
            }
            return;
        }
        let local = key == Key::F11
            || fired
                .iter()
                .any(|event| matches!(event, Event::Pressed(action) if stays_local(*action)));
        if down && local {
            self.kept.push(key);
            return;
        }
        if !hook_took {
            let at = Instant::now();
            self.sink.send(Captured::Key { key, down, at }, period);
        }
    }

    pub(crate) fn mouse(&mut self, raw: RawMouse) {
        let Some(period) = self.follow() else {
            return;
        };
        if self.injected_by_its_controller(raw.device) {
            return;
        }
        if let Some(mouse) = raw.mouse() {
            let at = Instant::now();
            self.sink.send(Captured::Mouse { mouse, at }, period);
        }
    }

    // The period to send in, starting clean each time one begins.
    fn follow(&mut self) -> Option<u32> {
        let period = self.sink.shared.sending_period();
        if period != self.period {
            self.period = period;
            self.kept.clear();
        }
        period.filter(|_| self.sink.viewer_in_front())
    }

    // While this PC is controlled itself, injected input is that
    // controller's, and it does not reach through this PC to the next one.
    fn injected_by_its_controller(&self, device: usize) -> bool {
        device == INJECTED && self.sink.shared.controlled()
    }
}

// The viewer's end of the feed, from Hotkeys::take_feed.
pub struct Feed {
    events: Receiver<Queued>,
    shared: Arc<Shared>,
    // The period and number of the last event read.
    last: Option<(u32, u32)>,
    missed: u32,
    // Held back while Missed goes first, with its period.
    waiting: Option<(Captured, u32)>,
}

impl Feed {
    pub(crate) fn new(events: Receiver<Queued>, shared: Arc<Shared>) -> Feed {
        Feed {
            events,
            shared,
            last: None,
            missed: 0,
            waiting: None,
        }
    }

    // The next event of the sending period now running, waiting up to
    // `wait` for one. What an earlier period left, and what is left once
    // sending stopped, is dropped without a word: it was meant for a
    // session that is over. Events lost to a full feed, or dropped here for
    // being older than FEED_LATE, come back as one Missed ahead of the next
    // event, or on its own when nothing follows. Disconnected once the
    // hotkeys have stopped.
    pub fn next(&mut self, wait: Duration) -> Result<Captured, RecvTimeoutError> {
        let deadline = Instant::now().checked_add(wait);
        let running = self.shared.sending_period();
        if self.last.is_some_and(|(period, _)| Some(period) != running) {
            self.last = None;
            self.missed = 0;
        }
        loop {
            if let Some((event, period)) = self.waiting.take() {
                if self.shared.sending_period() == Some(period) {
                    return Ok(event);
                }
                continue;
            }
            let (event, period, number) = if self.missed > 0 {
                match self.events.try_recv() {
                    Ok(queued) => queued,
                    Err(_) => return Ok(self.take_missed()),
                }
            } else {
                let left = deadline.map_or(Duration::MAX, |deadline| {
                    deadline.saturating_duration_since(Instant::now())
                });
                self.events.recv_timeout(left)?
            };
            if self.shared.sending_period() != Some(period) {
                continue;
            }
            match self.last {
                Some((last_period, last)) if last_period == period => {
                    let gap = number.wrapping_sub(last).wrapping_sub(1);
                    self.missed = self.missed.saturating_add(gap);
                }
                _ => self.missed = 0,
            }
            self.last = Some((period, number));
            if event.at().is_some_and(|at| at.elapsed() > FEED_LATE) {
                self.missed = self.missed.saturating_add(1);
                self.shared.dropped(1);
                continue;
            }
            if self.missed > 0 {
                self.waiting = Some((event, period));
                return Ok(self.take_missed());
            }
            return Ok(event);
        }
    }

    fn take_missed(&mut self) -> Captured {
        Captured::Missed(std::mem::take(&mut self.missed))
    }
}

// The keys the viewer handles itself while controlling. F11 is the viewer's
// too. Push to talk and the share key are not among them and go to the
// sharer as well.
fn stays_local(action: Action) -> bool {
    matches!(
        action,
        Action::Panic | Action::Mute | Action::Deafen | Action::ShowPanel | Action::StatsPanel
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
    use std::sync::mpsc;

    use super::*;
    use crate::remote::Remote;

    const VIEWER: isize = 0x0004_0A12;
    const DOWN: bool = true;
    const W: Key = Key::from_scan(0, 0x11);

    struct Rig {
        remote: Remote,
        front: Arc<AtomicIsize>,
        sink: Sink,
        feed: Feed,
    }

    fn rig() -> Rig {
        let remote = Remote::new();
        let front = Arc::new(AtomicIsize::new(VIEWER));
        let (sender, events) = mpsc::sync_channel(FEED_ROOM);
        let sink = Sink::new(sender, remote.shared(), {
            let front = Arc::clone(&front);
            Arc::new(move || front.load(Relaxed))
        });
        let feed = Feed::new(events, remote.shared());
        Rig {
            remote,
            front,
            sink,
            feed,
        }
    }

    impl Rig {
        fn send(&self, key: Key, at: Instant) {
            let period = self.sink.open().expect("sending, viewer in front");
            self.sink.send(
                Captured::Key {
                    key,
                    down: DOWN,
                    at,
                },
                period,
            );
        }

        fn read(&mut self) -> Vec<Captured> {
            std::iter::from_fn(|| self.feed.next(Duration::ZERO).ok()).collect()
        }
    }

    fn key(key: Key, at: Instant) -> Captured {
        Captured::Key {
            key,
            down: DOWN,
            at,
        }
    }

    fn ago(millis: u64) -> Instant {
        Instant::now()
            .checked_sub(Duration::from_millis(millis))
            .expect("the clock runs from before")
    }

    #[test]
    fn open_only_while_sending_to_a_viewer_in_front() {
        let rig = rig();
        assert_eq!(rig.sink.open(), None, "not sending");
        rig.remote.start_sending(VIEWER);
        assert!(rig.sink.open().is_some());
        rig.front.store(0x0009_0B00, Relaxed);
        assert_eq!(rig.sink.open(), None, "another window in front");
        rig.front.store(VIEWER, Relaxed);
        assert_eq!(rig.sink.windows_keys(), None, "not fullscreen");
        rig.remote.set_windows_keys(true);
        assert!(rig.sink.windows_keys().is_some());
        rig.remote.stop_sending();
        assert_eq!(rig.sink.open(), None);

        let rig = self::rig();
        rig.front.store(0, Relaxed);
        rig.remote.start_sending(0);
        assert_eq!(rig.sink.open(), None, "no window named, none in front");
    }

    // The viewer stopped reading when control ended. What it left must not
    // reach the next session, which may be with another PC.
    #[test]
    fn what_an_earlier_period_left_is_dropped() {
        let mut rig = rig();
        rig.remote.start_sending(VIEWER);
        let now = Instant::now();
        rig.send(Key::M, now);
        rig.remote.stop_sending();
        rig.remote.start_sending(VIEWER);
        rig.send(W, now);
        assert_eq!(rig.read(), [key(W, now)]);

        rig.send(Key::D, now);
        rig.remote.stop_sending();
        assert_eq!(rig.read(), [], "left once sending stopped");
        assert_eq!(
            rig.feed.next(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        );
        assert_eq!(rig.remote.numbers().feed_dropped, 0, "none of it was lost");
    }

    // Saying it again while it is on changes nothing; a new window does.
    #[test]
    fn a_new_period_starts_only_with_a_new_start() {
        let mut rig = rig();
        rig.remote.start_sending(VIEWER);
        let now = Instant::now();
        rig.send(Key::M, now);
        rig.remote.start_sending(VIEWER);
        assert_eq!(rig.read(), [key(Key::M, now)]);
        rig.send(Key::D, now);
        rig.front.store(VIEWER + 4, Relaxed);
        rig.remote.start_sending(VIEWER + 4);
        assert_eq!(rig.read(), []);
    }

    #[test]
    fn late_input_is_dropped_and_the_viewer_told() {
        let mut rig = rig();
        rig.remote.start_sending(VIEWER);
        let late = ago(FEED_LATE.as_millis() as u64 + 50);
        let now = Instant::now();
        rig.send(Key::M, late);
        rig.send(Key::D, late);
        rig.send(W, now);
        assert_eq!(rig.read(), [Captured::Missed(2), key(W, now)]);
        rig.send(Key::M, late);
        assert_eq!(rig.read(), [Captured::Missed(1)], "said with nothing after");
        assert_eq!(rig.remote.numbers().feed_dropped, 3);
    }

    #[test]
    fn a_full_feed_is_told_where_it_lost_events() {
        let mut rig = rig();
        rig.remote.start_sending(VIEWER);
        let now = Instant::now();
        for _ in 0..FEED_ROOM + 3 {
            rig.send(Key::M, now);
        }
        assert_eq!(rig.read().len(), FEED_ROOM);
        rig.send(W, now);
        assert_eq!(rig.read(), [Captured::Missed(3), key(W, now)]);
        assert_eq!(rig.remote.numbers().feed_dropped, 3);
    }

    #[test]
    fn the_feed_ends_with_the_hotkeys() {
        let rig = rig();
        let Rig { sink, mut feed, .. } = rig;
        drop(sink);
        assert_eq!(
            feed.next(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected)
        );
    }
}
