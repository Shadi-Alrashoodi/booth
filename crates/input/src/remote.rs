// What remote control switches on the hotkey thread, and what it reads back
// from it. The switches are atomics the thread reads at every event, so a
// switch takes effect with the next key; the wake only matters for what the
// thread has to ask Windows for (the mouse registration, the Windows keys
// hook).

use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::tracker::Elevation;
use crate::win;

// The sending switches share one atomic with the sending period, so the
// release key and the viewer switching sending on at the same moment cannot
// leave it on, and an event is stamped with the period it was decided in.
const SENDING: u32 = 1;
const WINDOWS_KEYS: u32 = 2;
// The release key was pressed while sending: sending stays off until the
// viewer has switched it off itself, which it does when control ends.
const RELEASED: u32 = 4;
const SWITCHES: u32 = SENDING | WINDOWS_KEYS | RELEASED;
// The period counts up in the bits above the switches, wrapping.
const PERIOD: u32 = 1 << 8;

// Any number of clones, on any thread; they all share one hotkey thread.
#[derive(Clone)]
pub struct Remote {
    shared: Arc<Shared>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoteNumbers {
    // Events the viewer lost: the feed was full, or they had waited longer
    // than FEED_LATE by the time the viewer read them.
    pub feed_dropped: u64,
    // Times Windows took the Windows keys hook away because this thread
    // answered too late, and Booth put it back.
    pub hooks_lost: u32,
}

pub(crate) struct Shared {
    controlled: AtomicBool,
    state: AtomicU32,
    // The viewer's window, which must be in front for anything to be sent.
    viewer: AtomicIsize,
    // Every event offered to the feed takes the next number, sent or not,
    // so the viewer can tell where some went missing.
    sequence: AtomicU32,
    // The last physical key or mouse event while controlled, as microseconds
    // after `epoch` plus one, so that zero is none.
    touched: AtomicU64,
    epoch: Instant,
    // 0 when the hotkeys run, else what paused them: 1 elevated, 2
    // unreadable.
    paused: AtomicU8,
    mouse: AtomicBool,
    hooked: AtomicBool,
    dropped: AtomicU64,
    hooks_lost: AtomicU32,
    // The hotkey window while it lives. Set and cleared by its own thread;
    // a switch from another thread posts to it only under this lock, so it
    // never posts to a window that is gone.
    window: Mutex<Option<isize>>,
}

impl Remote {
    pub(crate) fn new() -> Remote {
        Remote {
            shared: Arc::new(Shared {
                controlled: AtomicBool::new(false),
                state: AtomicU32::new(0),
                viewer: AtomicIsize::new(0),
                sequence: AtomicU32::new(0),
                touched: AtomicU64::new(0),
                epoch: Instant::now(),
                paused: AtomicU8::new(0),
                mouse: AtomicBool::new(false),
                hooked: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                hooks_lost: AtomicU32::new(0),
                window: Mutex::new(None),
            }),
        }
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    // This PC is being controlled. Turn it on before the injector starts
    // and off after it stops. While on, injected keys set off none of this
    // PC's hotkeys, and the owner's own keyboard and mouse are timed for
    // last_physical_input; nothing else about them is kept.
    pub fn set_controlled(&self, on: bool) {
        self.shared.controlled.store(on, Relaxed);
        self.shared.forget_touch();
        self.wake();
    }

    pub fn controlled(&self) -> bool {
        self.shared.controlled()
    }

    // The viewer is controlling. `viewer` is its own top-level window: keys
    // and mouse go to the feed from the next event, and only while that
    // window is in front, which the hotkey thread checks at every event. So
    // a viewer that hangs, or misses losing the focus, still sends nothing
    // typed into another window. A start begins a new period, and the feed
    // drops whatever an earlier one left in it. After the release key this
    // does nothing until the viewer has called stop_sending.
    pub fn start_sending(&self, viewer: isize) {
        let before = self.shared.viewer.swap(viewer, SeqCst);
        let _ = self.shared.state.fetch_update(SeqCst, SeqCst, |now| {
            if now & RELEASED != 0 {
                None
            } else if now & SENDING != 0 && before == viewer {
                Some(now)
            } else {
                Some((now | SENDING).wrapping_add(PERIOD))
            }
        });
        self.wake();
    }

    // The Windows keys stop with it, so a later start in a window does not
    // find them still on from fullscreen.
    pub fn stop_sending(&self) {
        self.shared.state.fetch_and(!SWITCHES, SeqCst);
        self.wake();
    }

    pub fn sending(&self) -> bool {
        self.shared.sending_period().is_some()
    }

    // The viewer is fullscreen, focused and controlling: the Windows keys
    // go to the feed too, through a keyboard hook that exists only while
    // this and sending are both on.
    pub fn set_windows_keys(&self, on: bool) {
        if on {
            self.shared.state.fetch_or(WINDOWS_KEYS, SeqCst);
        } else {
            self.shared.state.fetch_and(!WINDOWS_KEYS, SeqCst);
        }
        self.wake();
    }

    // When this PC's owner last touched their own keyboard or mouse while
    // controlled. None before the first touch, and after control ends.
    pub fn last_physical_input(&self) -> Option<Instant> {
        match self.shared.touched.load(Relaxed) {
            0 => None,
            micros => self
                .shared
                .epoch
                .checked_add(Duration::from_micros(micros - 1)),
        }
    }

    // Some while an administrator window is in front, from the moment the
    // hotkeys send Event::Paused to Event::Resumed. Windows drops input
    // injected into that window, so the injector pauses instead of sending
    // into nothing. Asking Windows takes several system calls; this is one
    // atomic read, made fresh by the hotkey thread whenever another window
    // comes to the front.
    pub fn paused(&self) -> Option<Elevation> {
        match self.shared.paused.load(Relaxed) {
            1 => Some(Elevation::Elevated),
            2 => Some(Elevation::Unreadable),
            _ => None,
        }
    }

    // The hotkey window holds this process's mouse registration, which the
    // hotkey thread takes while controlled or sending. Asked of Windows each
    // time, since another registration in this process takes it silently.
    // False then means the owner's mouse cannot pause the controller, and
    // the viewer gets no raw mouse.
    pub fn mouse_heard(&self) -> bool {
        if !self.shared.mouse.load(Relaxed) {
            return false;
        }
        let window = self
            .shared
            .window
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        window.is_some_and(win::mouse_registered_to)
    }

    // The Windows keys hook is in place, as far as Booth can tell.
    pub fn windows_keys_hooked(&self) -> bool {
        self.shared.hooked.load(Relaxed)
    }

    pub fn numbers(&self) -> RemoteNumbers {
        RemoteNumbers {
            feed_dropped: self.shared.dropped.load(Relaxed),
            hooks_lost: self.shared.hooks_lost.load(Relaxed),
        }
    }

    fn wake(&self) {
        let window = self
            .shared
            .window
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(window) = *window {
            win::wake_for_switches(window);
        }
    }
}

impl Shared {
    pub(crate) fn controlled(&self) -> bool {
        self.controlled.load(Relaxed)
    }

    // The period running while sending is on.
    pub(crate) fn sending_period(&self) -> Option<u32> {
        let state = self.state.load(SeqCst);
        (state & SENDING != 0).then_some(state / PERIOD)
    }

    pub(crate) fn windows_keys(&self) -> bool {
        self.state.load(SeqCst) & (SENDING | WINDOWS_KEYS) == SENDING | WINDOWS_KEYS
    }

    pub(crate) fn viewer(&self) -> isize {
        self.viewer.load(SeqCst)
    }

    // The release key on the controller's side: whatever the viewer is
    // doing, this PC's keys are its own again from here. The latch is set
    // only when there was something to let go of. The same chord pressed at
    // any other time, as the controlled PC's own panic key or as "select to
    // the end" in an editor, must not block the next session.
    pub(crate) fn release(&self) {
        let _ = self.state.fetch_update(SeqCst, SeqCst, |now| {
            if now & (SENDING | RELEASED) != 0 {
                Some((now & !SWITCHES) | RELEASED)
            } else {
                Some(now & !WINDOWS_KEYS)
            }
        });
    }

    pub(crate) fn next_sequence(&self) -> u32 {
        self.sequence.fetch_add(1, Relaxed)
    }

    pub(crate) fn touch(&self) {
        let micros = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.touched.store(micros.saturating_add(1), Relaxed);
    }

    pub(crate) fn forget_touch(&self) {
        self.touched.store(0, Relaxed);
    }

    pub(crate) fn set_paused(&self, why: Option<Elevation>) {
        let code = match why {
            None | Some(Elevation::Normal) => 0,
            Some(Elevation::Elevated) => 1,
            Some(Elevation::Unreadable) => 2,
        };
        self.paused.store(code, Relaxed);
    }

    pub(crate) fn set_mouse(&self, held: bool) {
        self.mouse.store(held, Relaxed);
    }

    pub(crate) fn set_hooked(&self, hooked: bool) {
        self.hooked.store(hooked, Relaxed);
    }

    pub(crate) fn hook_lost(&self) {
        self.hooks_lost.fetch_add(1, Relaxed);
    }

    pub(crate) fn dropped(&self, count: u64) {
        self.dropped.fetch_add(count, Relaxed);
    }

    pub(crate) fn set_window(&self, window: Option<isize>) {
        *self.window.lock().unwrap_or_else(PoisonError::into_inner) = window;
    }
}
