// The Windows part. The hidden window's thread is started and stopped for
// real: a test build registers it without the background flag
// (BACKGROUND), and the window is never in front, so nothing typed while
// this runs ever reaches it. That is the only test in the binary that
// registers for input, since a process has one keyboard registration and
// two would take it from each other. The Windows keys hook's procedure is
// driven with made-up events; no hook is installed.

use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use windows_sys::Win32::System::Threading::{
    GR_USEROBJECTS, GetCurrentProcess, GetGuiResources, GetProcessHandleCount,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP,
};

use super::Hooked;
use crate::bindings::Bindings;
use crate::feed::{Captured, FEED_ROOM, Feed, Sink};
use crate::grab::Grab;
use crate::hotkeys::Hotkeys;
use crate::key::Key;
use crate::remote::Remote;

const VIEWER: isize = 0x0004_0A12;
const BROWSER: isize = 0x0003_0C44;
const E: Key = Key::from_scan(0, 0x12);

#[test]
fn the_hotkey_thread_starts_and_stops_cleanly_many_times() {
    let token = Arc::new(());
    let start = || {
        let held = Arc::clone(&token);
        Hotkeys::start(
            Bindings::default(),
            move |_| {
                let _ = &held;
            },
            || {},
        )
        .expect("the hotkey thread starts")
    };
    // Whatever Windows sets up once for the whole process is set up here.
    for _ in 0..5 {
        drop(start());
    }
    let before = counts();

    for round in 0..100 {
        let hotkeys = start();
        assert!(
            hotkeys.listening(),
            "round {round}: keyboard registered, without the background flag"
        );
        hotkeys.capture(true);
        hotkeys.set_bindings(Bindings::default());
        hotkeys.capture(false);
        drop(hotkeys);
        assert_eq!(Arc::strong_count(&token), 1, "round {round}: thread ended");
    }

    let after = counts();
    // A thread, a window and a hook leaked per round would show as 100 or
    // more; the slack is for Windows' own worker threads coming and going.
    assert!(
        after.0 < before.0 + 30,
        "kernel handles went from {} to {} over 100 starts",
        before.0,
        after.0
    );
    assert!(
        after.1 < before.1 + 10,
        "user objects went from {} to {} over 100 starts",
        before.1,
        after.1
    );
}

// Kernel handles, and user objects (windows, hooks).
fn counts() -> (u32, u32) {
    let mut handles = 0;
    // SAFETY: the pseudo handle for this process needs no closing, and
    // `handles` is a live u32 for the call to fill.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) };
    assert_ne!(ok, 0, "could not read this process's handle count");
    // SAFETY: as above; the count is the return value.
    let objects = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };
    (handles, objects)
}

// Only while fullscreen and sending, and only while the viewer is in
// front, does the hook take a new press and send it. A key it took keeps
// going the same way to its release, but nothing reaches the feed while
// another window is in front.
#[test]
fn hook_takes_keys_only_for_a_viewer_in_front() {
    let remote = Remote::new();
    let front = Arc::new(AtomicIsize::new(VIEWER));
    let (sender, events) = mpsc::sync_channel(FEED_ROOM);
    let sink = Sink::new(sender, remote.shared(), {
        let front = Arc::clone(&front);
        Arc::new(move || front.load(Relaxed))
    });
    let mut feed = Feed::new(events, remote.shared());
    let mut hooked = Hooked {
        grab: Grab::new(Bindings::default(), std::iter::empty()),
        sink,
    };
    let win = |down| made_up(0x5B, 0x5B, LLKHF_EXTENDED, down);
    let e = |down| made_up(0x45, 0x12, 0, down);

    assert!(!hooked.event(&win(true)), "not sending");
    hooked.event(&win(false));
    remote.start_sending(VIEWER);
    assert!(!hooked.event(&win(true)), "not fullscreen");
    hooked.event(&win(false));
    remote.set_windows_keys(true);
    assert!(
        !hooked.event(&made_up(0x5B, 0x5B, LLKHF_EXTENDED | LLKHF_INJECTED, true)),
        "injected"
    );
    assert!(hooked.event(&win(true)));
    assert!(hooked.event(&e(true)));

    front.store(BROWSER, Relaxed);
    assert!(hooked.event(&e(false)), "its press was taken");
    assert!(!hooked.event(&e(true)), "a new press stays here");
    assert!(!hooked.event(&e(false)));
    assert!(hooked.event(&win(false)), "its press was taken");
    assert_eq!(fed(&mut feed), [(Key::LEFT_WIN, true), (E, true)]);
}

fn made_up(vk: u32, scan: u32, flags: u32, down: bool) -> KBDLLHOOKSTRUCT {
    KBDLLHOOKSTRUCT {
        vkCode: vk,
        scanCode: scan,
        flags: if down { flags } else { flags | LLKHF_UP },
        ..KBDLLHOOKSTRUCT::default()
    }
}

fn fed(feed: &mut Feed) -> Vec<(Key, bool)> {
    std::iter::from_fn(|| feed.next(Duration::ZERO).ok())
        .filter_map(|event| match event {
            Captured::Key { key, down, .. } => Some((key, down)),
            _ => None,
        })
        .collect()
}
