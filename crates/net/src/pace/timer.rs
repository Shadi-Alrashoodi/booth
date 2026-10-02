// The default Windows timer ticks every 15.6 ms, so a thread that sleeps or
// waits with a timeout can wake up to a whole 120 fps frame late, and more.
// A high-resolution waitable timer (Windows 10 1803 and later) wakes within
// a fraction of a millisecond without raising the timer rate for the whole
// system the way timeBeginPeriod does.

use std::fmt;
use std::io;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    SetEvent, SetWaitableTimerEx, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
};

// A synchronization timer: a wait that it ends also resets it, so a timer
// that fired while nobody waited costs one early wake-up at most.
pub struct Timer {
    handle: HANDLE,
    high_resolution: bool,
    note: Option<String>,
}

impl Timer {
    pub fn new() -> io::Result<Timer> {
        match create_timer(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION) {
            Ok(handle) => Ok(Timer {
                handle,
                high_resolution: true,
                note: None,
            }),
            Err(high) => Timer::normal(Some(high)),
        }
    }

    // What an older Windows gets, and what the tests use to check the
    // fallback on a Windows that has the better one.
    pub(crate) fn normal(why: Option<io::Error>) -> io::Result<Timer> {
        let handle = create_timer(0).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("could not create a waitable timer: {err}"),
            )
        })?;
        let why = why.map_or_else(|| String::from("not asked for"), |err| err.to_string());
        Ok(Timer {
            handle,
            high_resolution: false,
            note: Some(format!(
                "no high resolution timer ({why}; it needs Windows 10 1803 or later): \
                 waits can end up to 16 ms late"
            )),
        })
    }

    pub fn high_resolution(&self) -> bool {
        self.high_resolution
    }

    // Set only when this is the fallback timer, for the caller to log once.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    // Replaces any time set before.
    #[allow(unsafe_code)]
    pub fn set(&self, after: Duration) -> io::Result<()> {
        // Negative means relative, in 100 ns units. Rounded up so the timer
        // never fires before the moment asked for, and never zero, which
        // Windows would take as an absolute time long past.
        let ticks = after.as_nanos().div_ceil(100).clamp(1, i64::MAX as u128) as i64;
        let due = -ticks;
        // SAFETY: `handle` is a live timer owned by self, `due` is a local
        // that outlives the call, and there is no completion routine, wake
        // context or tolerable delay.
        let ok =
            unsafe { SetWaitableTimerEx(self.handle, &due, 0, None, ptr::null(), ptr::null(), 0) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!("could not set the waitable timer: {err}"),
            ));
        }
        Ok(())
    }

    pub fn set_at(&self, at: Instant) -> io::Result<()> {
        self.set(at.saturating_duration_since(Instant::now()))
    }

    #[allow(unsafe_code)]
    pub fn wait(&self) -> io::Result<()> {
        // SAFETY: `handle` is a live timer owned by self.
        match unsafe { WaitForSingleObject(self.handle, INFINITE) } {
            WAIT_OBJECT_0 => Ok(()),
            _ => Err(wait_error("the waitable timer")),
        }
    }
}

impl Drop for Timer {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: created in create_timer and closed only here.
        unsafe { CloseHandle(self.handle) };
    }
}

impl fmt::Debug for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timer")
            .field("high_resolution", &self.high_resolution)
            .finish()
    }
}

// SAFETY: a timer handle may be set, waited on and closed from any thread.
#[allow(unsafe_code)]
unsafe impl Send for Timer {}
// SAFETY: set and wait are safe to call on one handle from several threads
// at once; Windows serializes them.
#[allow(unsafe_code)]
unsafe impl Sync for Timer {}

#[allow(unsafe_code)]
fn create_timer(flags: u32) -> io::Result<HANDLE> {
    // SAFETY: no security attributes and no name; the handle is closed in
    // Timer's drop.
    let handle =
        unsafe { CreateWaitableTimerExW(ptr::null(), ptr::null(), flags, TIMER_ALL_ACCESS) };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

// An auto-reset event: set from any thread, it wakes one wait and resets.
pub struct Signal {
    handle: HANDLE,
}

impl Signal {
    #[allow(unsafe_code)]
    pub fn new() -> io::Result<Signal> {
        // SAFETY: no security attributes and no name, auto-reset, not set;
        // the handle is closed in drop.
        let handle = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if handle.is_null() {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!("could not create an event: {err}"),
            ));
        }
        Ok(Signal { handle })
    }

    // Fails only if the handle is gone, which drop alone does.
    #[allow(unsafe_code)]
    pub fn set(&self) {
        // SAFETY: `handle` is a live event owned by self.
        unsafe { SetEvent(self.handle) };
    }
}

impl Drop for Signal {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: created in Signal::new and closed only here.
        unsafe { CloseHandle(self.handle) };
    }
}

impl fmt::Debug for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signal")
    }
}

// SAFETY: an event handle may be set, waited on and closed from any thread.
#[allow(unsafe_code)]
unsafe impl Send for Signal {}
// SAFETY: SetEvent and waits on one handle from several threads at once are
// what events are for.
#[allow(unsafe_code)]
unsafe impl Sync for Signal {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woken {
    Signal,
    Timer,
}

// Blocks until the signal is set or the timer fires, whichever is first.
// Without a timer it waits for the signal alone. When both are ready the
// signal wins, and the timer stays set for the next wait.
#[allow(unsafe_code)]
pub fn wait(signal: &Signal, timer: Option<&Timer>) -> io::Result<Woken> {
    let handles = [
        signal.handle,
        timer.map_or(ptr::null_mut(), |timer| timer.handle),
    ];
    let count = if timer.is_some() { 2 } else { 1 };
    // SAFETY: the first `count` handles are live, owned by the borrowed
    // signal and timer, which outlive the call.
    let woken = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, INFINITE) };
    match woken {
        WAIT_OBJECT_0 => Ok(Woken::Signal),
        w if w == WAIT_OBJECT_0 + 1 && timer.is_some() => Ok(Woken::Timer),
        _ => Err(wait_error("an event and a timer")),
    }
}

fn wait_error(what: &str) -> io::Error {
    let err = io::Error::last_os_error();
    io::Error::new(err.kind(), format!("could not wait on {what}: {err}"))
}
