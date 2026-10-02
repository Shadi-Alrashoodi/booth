// The hotkey thread's logic on made-up Raw Input events. Nothing here makes
// a window, registers for input, installs a hook or sends a key.

use std::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::bindings::Action;
use crate::feed::{Captured, Mouse};
use crate::key::Key;

type Heard = Arc<Mutex<Vec<Event>>>;

struct Rig {
    run: Loop,
    heard: Heard,
    commands: Sender<Command>,
    remote: Remote,
    feed: Feed,
    // The window in front, as the hotkey thread would ask Windows for it.
    front: Arc<AtomicIsize>,
    cuts: Arc<AtomicUsize>,
}

fn rig(elevated_self: bool) -> Rig {
    let heard = Heard::default();
    let cuts = Arc::new(AtomicUsize::new(0));
    let (commands, waiting) = mpsc::channel();
    let (sender, events) = mpsc::sync_channel(FEED_ROOM);
    let remote = Remote::new();
    let front = Arc::new(AtomicIsize::new(VIEWER));
    let sink = Sink::new(sender, remote.shared(), {
        let front = Arc::clone(&front);
        Arc::new(move || front.load(Relaxed))
    });
    let handler = Box::new({
        let heard = Arc::clone(&heard);
        move |event| heard.lock().unwrap().push(event)
    });
    let panic = Box::new({
        let cuts = Arc::clone(&cuts);
        move || {
            cuts.fetch_add(1, Relaxed);
        }
    });
    let run = Loop::new(
        Tracker::new(Bindings::default()),
        handler,
        panic,
        waiting,
        elevated_self,
        remote.shared(),
        sink,
    );
    Rig {
        run,
        heard,
        commands,
        feed: Feed::new(events, remote.shared()),
        remote,
        front,
        cuts,
    }
}

impl Rig {
    fn key(&mut self, raw: RawKey) {
        self.run.raw(raw, 0, false);
    }

    fn keys(&mut self, device: usize, steps: &[(Key, bool)]) {
        for (key, down) in steps {
            let raw = if *down {
                RawKey::press(*key, device)
            } else {
                RawKey::release(*key, device)
            };
            self.key(raw);
        }
    }

    // What the viewer reads, without the times.
    fn fed(&mut self) -> Vec<Fed> {
        std::iter::from_fn(|| self.feed.next(Duration::ZERO).ok())
            .map(|event| match event {
                Captured::Key { key, down, .. } => Fed::Key(key, down),
                Captured::Mouse { mouse, .. } => Fed::Mouse(mouse),
                Captured::Missed(count) => Fed::Missed(count),
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Fed {
    Key(Key, bool),
    Mouse(Mouse),
    Missed(u32),
}

fn take(heard: &Heard) -> Vec<Event> {
    std::mem::take(&mut *heard.lock().unwrap())
}

const PRESSED: Event = Event::Pressed(Action::PushToTalk);
const RELEASED: Event = Event::Released(Action::PushToTalk);
const PANIC: Event = Event::Pressed(Action::Panic);
const KEYBOARD: usize = 1;
const MOUSE: usize = 2;
const VIEWER: isize = 0x0004_0A12;
const BROWSER: isize = 0x0003_0C44;
const DOWN: bool = true;
const UP: bool = false;
const W: Key = Key::from_scan(0, 0x11);
const ALT_F4: [(Key, bool); 2] = [(Key::LEFT_ALT, DOWN), (Key::F4, DOWN)];

fn chord(key: Key) -> [(Key, bool); 6] {
    [
        (Key::LEFT_CTRL, DOWN),
        (Key::LEFT_SHIFT, DOWN),
        (key, DOWN),
        (key, UP),
        (Key::LEFT_SHIFT, UP),
        (Key::LEFT_CTRL, UP),
    ]
}

fn sent(key: Key, down: bool) -> Fed {
    Fed::Key(key, down)
}

#[test]
fn an_administrator_window_in_front_pauses_and_lets_go() {
    let mut rig = rig(false);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 900, false);
    rig.run.front(Ok(Elevation::Unreadable), 1_000);
    assert_eq!(
        take(&rig.heard),
        [PRESSED, RELEASED, Event::Paused(Elevation::Unreadable)]
    );
    assert!(!rig.run.watching(), "nothing held, so the watch stops");
    assert_eq!(rig.remote.paused(), Some(Elevation::Unreadable));
    rig.run.front(Ok(Elevation::Normal), 2_000);
    assert_eq!(take(&rig.heard), [Event::Resumed]);
    assert_eq!(rig.remote.paused(), None);
}

// The injector reads the verdict before every event instead of asking
// Windows, so it follows the hotkeys' own pause, whichever way it ends.
#[test]
fn the_pause_is_there_for_the_injector_to_read() {
    let mut rig = rig(false);
    assert_eq!(rig.remote.paused(), None);
    rig.run.front(Ok(Elevation::Elevated), 1_000);
    assert_eq!(rig.remote.paused(), Some(Elevation::Elevated));
    rig.run.watch(Some(Ok(Elevation::Unreadable)), 1_500);
    assert_eq!(rig.remote.paused(), Some(Elevation::Unreadable));
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_600, false);
    assert_eq!(rig.remote.paused(), None, "keys still arrive");
    rig.run
        .front(Err(io::Error::other("the window went")), 2_000);
    assert_eq!(rig.remote.paused(), None);

    let mut rig = self::rig(true);
    rig.run.front(Ok(Elevation::Elevated), 1_000);
    assert_eq!(rig.remote.paused(), None, "an elevated Booth reaches it");
}

#[test]
fn a_check_that_failed_changes_nothing() {
    let mut rig = rig(false);
    rig.run
        .front(Err(io::Error::other("the window went")), 1_000);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_010, false);
    assert_eq!(take(&rig.heard), [PRESSED]);
}

#[test]
fn an_elevated_booth_never_pauses() {
    let mut rig = rig(true);
    rig.run.front(Ok(Elevation::Elevated), 1_000);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_010, false);
    assert_eq!(take(&rig.heard), [PRESSED]);
}

// The watch found a desktop this process cannot open: a locked screen or
// the administrator prompt. Push to talk is let go there and then.
#[test]
fn the_watch_lets_go_when_keys_stop_arriving() {
    let mut rig = rig(false);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_000, false);
    rig.run.watch(Some(Ok(Elevation::Normal)), 1_500);
    assert_eq!(take(&rig.heard), [PRESSED]);
    rig.run.watch(None, 2_000);
    assert_eq!(take(&rig.heard), [RELEASED, Event::Lost]);
    rig.run.watch(None, 2_500);
    assert_eq!(take(&rig.heard), [], "said once");
    // An administrator window the hook missed is caught by the watch.
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 3_000, false);
    rig.run.watch(Some(Ok(Elevation::Elevated)), 3_500);
    assert_eq!(
        take(&rig.heard),
        [PRESSED, RELEASED, Event::Paused(Elevation::Elevated)]
    );
}

// A game started as another user: its rights cannot be read from here, so
// it is taken for elevated, but Windows still hands its keys over. The
// first press after the pause began proves it, and counts.
#[test]
fn a_key_that_still_arrives_ends_a_wrong_pause() {
    let mut rig = rig(false);
    rig.run.front(Ok(Elevation::Unreadable), 1_000);
    assert_eq!(take(&rig.heard), [Event::Paused(Elevation::Unreadable)]);
    // Pressed before the window came forward and read after it, as a posted
    // message can overtake input.
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 990, false);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_000, false);
    // A break is no proof: it may belong to a key held from before.
    rig.run
        .raw(RawKey::release(Key::RIGHT_CTRL, KEYBOARD), 1_010, false);
    assert_eq!(take(&rig.heard), []);
    assert!(rig.run.tracker.paused());

    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_020, false);
    assert_eq!(
        take(&rig.heard),
        [Event::KeysArrive, Event::Resumed, PRESSED]
    );
    // The watch, running while the key is down, leaves that window be.
    rig.run.watch(Some(Ok(Elevation::Unreadable)), 1_520);
    assert_eq!(take(&rig.heard), []);
    rig.run
        .raw(RawKey::release(Key::RIGHT_CTRL, KEYBOARD), 1_600, false);
    assert_eq!(take(&rig.heard), [RELEASED]);

    // The next window in front is judged afresh.
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_900, false);
    rig.run.front(Ok(Elevation::Elevated), 2_000);
    assert_eq!(
        take(&rig.heard),
        [PRESSED, RELEASED, Event::Paused(Elevation::Elevated)]
    );
}

// The stamps are milliseconds since boot in a u32, which wraps after 49.7
// days.
#[test]
fn tick_count_wrap() {
    let mut rig = rig(false);
    rig.run.front(Ok(Elevation::Unreadable), u32::MAX - 5);
    take(&rig.heard);
    rig.run.raw(
        RawKey::press(Key::RIGHT_CTRL, KEYBOARD),
        u32::MAX - 10,
        false,
    );
    assert_eq!(take(&rig.heard), []);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 4, false);
    assert_eq!(
        take(&rig.heard),
        [Event::KeysArrive, Event::Resumed, PRESSED]
    );
}

// A keyboard unplugged with push to talk down sends no break, and the watch
// sees nothing wrong with the window in front.
#[test]
fn a_keyboard_that_goes_away_lets_go_and_stops_the_watch() {
    let mut rig = rig(false);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_000, false);
    rig.run.watch(Some(Ok(Elevation::Normal)), 1_500);
    assert_eq!(take(&rig.heard), [PRESSED]);
    rig.run.device_gone(KEYBOARD);
    assert_eq!(take(&rig.heard), [RELEASED]);
    assert!(!rig.run.watching());
}

#[test]
fn commands_apply_in_order() {
    let mut rig = rig(false);
    rig.run
        .raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD), 1_000, false);
    rig.commands.send(Command::Capture(true)).unwrap();
    rig.commands.send(Command::Capture(false)).unwrap();
    rig.commands
        .send(Command::Bindings(Bindings::default()))
        .unwrap();
    rig.run.commands();
    assert_eq!(take(&rig.heard), [PRESSED, RELEASED]);
}

// The cut runs before the handler hears of the press: the app's handler
// takes locks, the cut must not wait for them.
#[test]
fn the_panic_key_cuts_before_the_handler_hears_it() {
    let mut rig = rig(false);
    let cuts = Arc::clone(&rig.cuts);
    let seen_by_handler = Arc::new(AtomicUsize::new(usize::MAX));
    rig.run.handler = Box::new({
        let seen = Arc::clone(&seen_by_handler);
        move |event| {
            if event == PANIC {
                seen.store(cuts.load(Relaxed), Relaxed);
            }
        }
    });
    rig.keys(KEYBOARD, &chord(Key::END));
    assert_eq!(rig.cuts.load(Relaxed), 1);
    assert_eq!(seen_by_handler.load(Relaxed), 1, "cut already made");
}

#[test]
fn an_injected_panic_chord_cuts_nothing() {
    let mut rig = rig(false);
    rig.keys(INJECTED, &chord(Key::END));
    rig.remote.set_controlled(true);
    rig.keys(INJECTED, &chord(Key::END));
    // Injected modifiers under a physical End.
    rig.keys(INJECTED, &[(Key::LEFT_CTRL, DOWN), (Key::LEFT_SHIFT, DOWN)]);
    rig.keys(KEYBOARD, &[(Key::END, DOWN), (Key::END, UP)]);
    assert_eq!(rig.cuts.load(Relaxed), 0);
    assert!(!take(&rig.heard).contains(&PANIC));
}

// The panic key has to stop injection within 10 ms of the key going down,
// even while the controller floods modifier key-ups. This is the hotkey
// thread's share of it: from the moment the key-down is queued for the
// thread to the cut, with 1,000 injected Ctrl and Shift key-ups a second
// queued along with it, on a thread raised as the real one is.
#[test]
fn panic_key_under_a_flood_of_key_ups() {
    const TICKS: u32 = 1_000;
    const CHORD_EVERY: u32 = 25;
    let epoch = Instant::now();
    let cut_at = Arc::new(AtomicU64::new(0));
    let mut rig = rig(false);
    rig.remote.set_controlled(true);
    rig.run.panic = Box::new({
        let cut_at = Arc::clone(&cut_at);
        move || cut_at.store(epoch.elapsed().as_nanos() as u64, Relaxed)
    });
    let mut run = rig.run;
    let (queue, queued) = mpsc::channel::<(RawKey, u32, Option<Instant>)>();
    let thread = thread::spawn(move || {
        let _ = win::raise_priority();
        let mut took = Vec::new();
        for (raw, tick, stamp) in queued {
            run.raw(raw, tick, false);
            if let Some(stamp) = stamp {
                let cut = cut_at.swap(0, Relaxed);
                let from = stamp.duration_since(epoch).as_nanos() as u64;
                took.push((cut > 0).then(|| Duration::from_nanos(cut.saturating_sub(from))));
            }
        }
        took
    });

    let start = Instant::now();
    let send = |raw, tick, stamp| queue.send((raw, tick, stamp)).unwrap();
    for tick in 0..TICKS {
        let due = start + Duration::from_millis(u64::from(tick));
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        let flood = if tick % 2 == 0 {
            Key::LEFT_CTRL
        } else {
            Key::LEFT_SHIFT
        };
        send(RawKey::release(flood, INJECTED), tick, None);
        match tick % CHORD_EVERY {
            0 => send(RawKey::press(Key::LEFT_CTRL, KEYBOARD), tick, None),
            3 => send(RawKey::press(Key::LEFT_SHIFT, KEYBOARD), tick, None),
            8 => send(
                RawKey::press(Key::END, KEYBOARD),
                tick,
                Some(Instant::now()),
            ),
            12 => {
                send(RawKey::release(Key::END, KEYBOARD), tick, None);
                send(RawKey::release(Key::LEFT_SHIFT, KEYBOARD), tick, None);
                send(RawKey::release(Key::LEFT_CTRL, KEYBOARD), tick, None);
            }
            _ => {}
        }
    }
    drop(queue);
    let took = thread.join().unwrap();

    let chords = (TICKS / CHORD_EVERY) as usize;
    assert_eq!(took.len(), chords);
    let mut took: Vec<Duration> = took
        .into_iter()
        .map(|took| took.expect("every physical chord cut, flood or not"))
        .collect();
    took.sort();
    let median = took[took.len() / 2];
    let worst = took[took.len() - 1];
    eprintln!(
        "panic key, queued to cut, {chords} chords under 1000 injected key-ups a second: median {} us, worst {} us",
        median.as_micros(),
        worst.as_micros()
    );
    assert!(median < Duration::from_millis(1), "median {median:?}");
    assert!(worst < Duration::from_millis(10), "worst {worst:?}");
}

// While controlled, the injector's keys press nothing here even if its block
// list missed them; otherwise injected keys work as usual, for macro tools.
#[test]
fn while_controlled_injected_keys_press_no_hotkey() {
    let mut rig = rig(false);
    rig.keys(INJECTED, &chord(Key::M));
    assert_eq!(take(&rig.heard), [Event::Pressed(Action::Mute)]);
    rig.remote.set_controlled(true);
    rig.keys(INJECTED, &chord(Key::M));
    rig.keys(INJECTED, &[(Key::RIGHT_CTRL, DOWN), (Key::RIGHT_CTRL, UP)]);
    assert_eq!(take(&rig.heard), []);
    rig.keys(KEYBOARD, &chord(Key::M));
    assert_eq!(take(&rig.heard), [Event::Pressed(Action::Mute)]);
    rig.remote.set_controlled(false);
    rig.keys(INJECTED, &[(Key::RIGHT_CTRL, DOWN), (Key::RIGHT_CTRL, UP)]);
    assert_eq!(take(&rig.heard), [PRESSED, RELEASED]);
}

#[test]
fn owners_own_input_is_only_a_time() {
    let mut rig = rig(false);
    rig.keys(KEYBOARD, &[(W, DOWN), (W, UP)]);
    assert_eq!(rig.remote.last_physical_input(), None, "not controlled");

    rig.remote.set_controlled(true);
    rig.keys(INJECTED, &[(W, DOWN), (W, UP)]);
    rig.run.mouse(RawMouse {
        x: 5,
        device: INJECTED,
        ..RawMouse::default()
    });
    assert_eq!(rig.remote.last_physical_input(), None, "injected");

    let before = Instant::now();
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    let touched = rig.remote.last_physical_input().expect("a physical key");
    assert!(touched >= before - Duration::from_millis(1) && touched <= Instant::now());

    thread::sleep(Duration::from_millis(5));
    rig.run.mouse(RawMouse {
        y: -3,
        device: MOUSE,
        ..RawMouse::default()
    });
    let moved = rig.remote.last_physical_input().expect("a physical mouse");
    assert!(moved > touched);
    assert!(rig.fed().is_empty(), "nothing goes anywhere");

    rig.remote.set_controlled(false);
    assert_eq!(rig.remote.last_physical_input(), None, "gone with control");
    rig.run.mouse(RawMouse {
        y: -3,
        device: MOUSE,
        ..RawMouse::default()
    });
    assert_eq!(rig.remote.last_physical_input(), None);
}

#[test]
fn nothing_is_fed_until_sending_is_on() {
    let mut rig = rig(false);
    rig.keys(KEYBOARD, &[(W, DOWN), (W, UP)]);
    rig.run.mouse(RawMouse {
        x: 4,
        device: MOUSE,
        ..RawMouse::default()
    });
    assert_eq!(rig.fed(), []);

    rig.remote.start_sending(VIEWER);
    let before = Instant::now();
    rig.keys(KEYBOARD, &[(W, DOWN), (W, DOWN), (W, UP)]);
    assert_eq!(rig.fed(), [sent(W, DOWN), sent(W, DOWN), sent(W, UP)]);

    // Each event carries when this thread read it, for the controller's
    // capture time.
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    let at = rig.feed.next(Duration::ZERO).unwrap().at().unwrap();
    assert!(at >= before && at <= Instant::now());

    rig.remote.stop_sending();
    rig.keys(KEYBOARD, &[(W, DOWN), (W, UP)]);
    assert_eq!(rig.fed(), [], "stopped with the next key");
}

// Keys are read for sending only while the viewer's window is in front. A
// viewer that hangs, or misses losing the focus, cannot send what is typed
// into the window that took it.
#[test]
fn nothing_is_fed_while_another_window_is_in_front() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    rig.front.store(BROWSER, Relaxed);
    rig.keys(KEYBOARD, &[(W, UP), (Key::M, DOWN), (Key::M, UP)]);
    rig.run.mouse(RawMouse {
        x: 4,
        device: MOUSE,
        ..RawMouse::default()
    });
    assert_eq!(rig.fed(), [sent(W, DOWN)]);
    assert!(rig.remote.sending(), "the viewer's switch is its own");
    rig.front.store(VIEWER, Relaxed);
    rig.keys(KEYBOARD, &[(Key::M, DOWN)]);
    assert_eq!(rig.fed(), [sent(Key::M, DOWN)]);

    // Nothing in front at all, between two windows.
    rig.front.store(0, Relaxed);
    rig.keys(KEYBOARD, &[(Key::M, UP)]);
    assert_eq!(rig.fed(), []);
}

// The release key, mute, deafen, show panel, the stats panel key and F11
// stay on the controller's PC, repeats and releases included. The modifiers
// pressed on the way to them have gone already, which is harmless. Push to
// talk and the share key go and act here too.
#[test]
fn the_feed_keeps_the_local_keys_and_passes_the_rest() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    for key in [Key::M, Key::D, Key::SPACE, Key::I] {
        rig.keys(KEYBOARD, &chord(key));
        assert_eq!(
            rig.fed(),
            [
                sent(Key::LEFT_CTRL, DOWN),
                sent(Key::LEFT_SHIFT, DOWN),
                sent(Key::LEFT_SHIFT, UP),
                sent(Key::LEFT_CTRL, UP),
            ],
            "{key}"
        );
    }
    rig.keys(
        KEYBOARD,
        &[(Key::F11, DOWN), (Key::F11, DOWN), (Key::F11, UP)],
    );
    rig.keys(KEYBOARD, &[(Key::ESC, DOWN), (Key::ESC, UP)]);
    assert_eq!(rig.fed(), [sent(Key::ESC, DOWN), sent(Key::ESC, UP)]);

    rig.keys(KEYBOARD, &[(Key::RIGHT_CTRL, DOWN), (Key::RIGHT_CTRL, UP)]);
    rig.keys(KEYBOARD, &chord(Key::S));
    assert_eq!(rig.fed().len(), 8);
    assert!(take(&rig.heard).contains(&PRESSED), "push to talk here too");

    // The made-up Shift around an arrow under Num Lock is no key.
    rig.key(RawKey {
        make_code: 0x2A,
        flags: 2,
        vkey: 0x10,
        device: KEYBOARD,
    });
    assert_eq!(rig.fed(), []);
}

// The release key is the panic key on the controller's side: it lets go
// of sending and the Windows keys at once, on this thread, so a viewer that
// hangs cannot keep them.
#[test]
fn the_release_key_stops_the_feed_there_and_then() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    rig.remote.set_windows_keys(true);
    assert_eq!(rig.run.wants(), (true, true));
    rig.keys(
        KEYBOARD,
        &[
            (Key::LEFT_CTRL, DOWN),
            (Key::LEFT_SHIFT, DOWN),
            (Key::END, DOWN),
        ],
    );
    assert!(!rig.remote.sending());
    assert_eq!(rig.fed(), [], "the feed drops what sending left");
    rig.keys(
        KEYBOARD,
        &[(Key::END, UP), (Key::LEFT_SHIFT, UP), (Key::LEFT_CTRL, UP)],
    );
    assert_eq!(rig.run.wants(), (false, false));
    assert_eq!(rig.cuts.load(Relaxed), 1);
    assert_eq!(take(&rig.heard), [PANIC]);

    // A viewer that has not heard yet cannot switch it back on; once it
    // has switched it off itself, as it does when control ends, it can.
    rig.remote.start_sending(VIEWER);
    rig.keys(KEYBOARD, &[(W, DOWN), (W, UP)]);
    assert_eq!(rig.fed(), []);
    rig.remote.stop_sending();
    rig.remote.start_sending(VIEWER);
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    assert_eq!(rig.fed(), [sent(W, DOWN)]);
}

// The same chord at any other time, as the panic key on a PC being
// controlled or as "select to the end" in an editor, leaves the next
// session free to start.
#[test]
fn the_panic_chord_while_nothing_is_sent_blocks_nothing() {
    let mut rig = rig(false);
    rig.remote.set_windows_keys(true);
    rig.keys(KEYBOARD, &chord(Key::END));
    assert_eq!(rig.cuts.load(Relaxed), 1);
    rig.remote.start_sending(VIEWER);
    assert!(rig.remote.sending());
    assert_eq!(rig.run.wants(), (true, false), "the Windows keys went");
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    assert_eq!(rig.fed(), [sent(W, DOWN)]);
}

// A switch left on from a fullscreen session must not bring the hook back
// in a window.
#[test]
fn stopping_lets_go_of_the_windows_keys_too() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    rig.remote.set_windows_keys(true);
    assert_eq!(rig.run.wants(), (true, true));
    rig.remote.stop_sending();
    rig.remote.start_sending(VIEWER);
    assert_eq!(rig.run.wants(), (true, false));
}

#[test]
fn a_key_the_windows_keys_hook_sent_is_not_sent_twice() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    rig.run.raw(RawKey::press(Key::LEFT_WIN, KEYBOARD), 0, true);
    rig.run
        .raw(RawKey::release(Key::LEFT_WIN, KEYBOARD), 0, true);
    assert_eq!(rig.fed(), []);
    rig.keys(KEYBOARD, &ALT_F4);
    assert_eq!(
        rig.fed(),
        [sent(Key::LEFT_ALT, DOWN), sent(Key::F4, DOWN)],
        "sent by Raw Input when the hook did not take it"
    );
}

// A PC can be controlled and control a third one at once. Its own
// controller's input arrives injected, and stays on this PC.
#[test]
fn injected_input_while_controlled() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    rig.keys(INJECTED, &[(W, DOWN)]);
    rig.run.mouse(RawMouse {
        x: 1,
        device: INJECTED,
        ..RawMouse::default()
    });
    assert_eq!(rig.fed().len(), 2, "a macro tool's keys still work");
    rig.remote.set_controlled(true);
    rig.keys(INJECTED, &[(W, UP)]);
    rig.run.mouse(RawMouse {
        x: 1,
        device: INJECTED,
        ..RawMouse::default()
    });
    assert_eq!(rig.fed(), []);
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    assert_eq!(rig.fed(), [sent(W, DOWN)]);
}

#[test]
fn the_mouse_is_fed_as_moves_buttons_and_wheels() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    let events = [
        RawMouse {
            x: -7,
            y: 12,
            device: MOUSE,
            ..RawMouse::default()
        },
        // Left down, right up, back down, forward up.
        RawMouse {
            buttons: 0x0001 | 0x0008 | 0x0040 | 0x0200,
            device: MOUSE,
            ..RawMouse::default()
        },
        RawMouse {
            buttons: 0x0400,
            data: (-120i16) as u16,
            device: MOUSE,
            ..RawMouse::default()
        },
        RawMouse {
            buttons: 0x0800,
            data: 240,
            device: MOUSE,
            ..RawMouse::default()
        },
        // A pen: where it is, not how far it went; its button still counts.
        RawMouse {
            flags: 1,
            buttons: 0x0010,
            x: 30_000,
            y: 20_000,
            device: MOUSE,
            ..RawMouse::default()
        },
        RawMouse {
            flags: 1,
            x: 30_000,
            y: 20_000,
            device: MOUSE,
            ..RawMouse::default()
        },
        RawMouse {
            device: MOUSE,
            ..RawMouse::default()
        },
    ];
    for raw in events {
        rig.run.mouse(raw);
    }
    let mouse = Fed::Mouse;
    assert_eq!(
        rig.fed(),
        [
            mouse(Mouse {
                dx: -7,
                dy: 12,
                ..Mouse::default()
            }),
            mouse(Mouse {
                pressed: Mouse::LEFT | Mouse::BACK,
                released: Mouse::RIGHT | Mouse::FORWARD,
                ..Mouse::default()
            }),
            mouse(Mouse {
                wheel: -120,
                ..Mouse::default()
            }),
            mouse(Mouse {
                hwheel: 240,
                ..Mouse::default()
            }),
            mouse(Mouse {
                pressed: Mouse::MIDDLE,
                ..Mouse::default()
            }),
        ]
    );
}

// A viewer that stalls loses events rather than holding up this thread, and
// is told where, so it can let go of what it thinks is held.
#[test]
fn a_reader_that_falls_behind_loses_events_and_is_told() {
    let mut rig = rig(false);
    rig.remote.start_sending(VIEWER);
    let moved = RawMouse {
        x: 1,
        device: MOUSE,
        ..RawMouse::default()
    };
    for _ in 0..FEED_ROOM + 5 {
        rig.run.mouse(moved);
    }
    assert_eq!(rig.fed().len(), FEED_ROOM);
    rig.keys(KEYBOARD, &[(W, DOWN)]);
    assert_eq!(rig.fed(), [Fed::Missed(5), sent(W, DOWN)]);
    assert_eq!(rig.remote.numbers().feed_dropped, 5);
    rig.keys(KEYBOARD, &[(W, UP)]);
    assert_eq!(rig.fed(), [sent(W, UP)]);
}

#[test]
fn what_the_window_asks_windows_for() {
    let mut rig = rig(false);
    assert_eq!(rig.run.wants(), (false, false));
    rig.remote.set_windows_keys(true);
    assert_eq!(rig.run.wants(), (false, false), "not without sending");
    rig.remote.start_sending(VIEWER);
    assert_eq!(rig.run.wants(), (true, true));
    rig.remote.set_windows_keys(false);
    assert_eq!(rig.run.wants(), (true, false));
    rig.remote.stop_sending();
    rig.remote.set_controlled(true);
    assert_eq!(rig.run.wants(), (true, false));
    rig.remote.set_controlled(false);
    assert_eq!(rig.run.wants(), (false, false));
}

// The hook goes in with what physical keyboards hold then, and with the
// bindings, so a Tab pressed with an Alt held from before is Windows' own.
#[test]
fn the_hook_starts_from_what_physical_keyboards_hold() {
    let mut rig = rig(false);
    rig.keys(KEYBOARD, &[(Key::LEFT_ALT, DOWN)]);
    rig.keys(INJECTED, &[(Key::LEFT_WIN, DOWN)]);
    let (mut grab, _) = rig.run.grab();
    assert!(
        !grab.hook(Some(W), DOWN, false, true),
        "the injected Win is not held"
    );
    assert!(grab.hook(Some(Key::TAB), DOWN, false, true));
}
