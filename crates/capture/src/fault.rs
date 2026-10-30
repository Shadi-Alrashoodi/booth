//! Faults a test can put into capture, built with the `fault` feature only,
//! which room's tests turn on and a release build never has. A test that
//! goes on using the desktop cannot bring up the lock screen, so it says
//! what a look at the desktop finds instead.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::PauseReason;

static DESKTOP: Mutex<Option<(Option<PauseReason>, Duration)>> = Mutex::new(None);

fn desktop() -> MutexGuard<'static, Option<(Option<PauseReason>, Duration)>> {
    DESKTOP.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every look at the desktop takes `takes` and finds `away` until clear():
/// None for this session's own desktop.
pub fn set_desktop(away: Option<PauseReason>, takes: Duration) {
    *desktop() = Some((away, takes));
}

/// Looks at the desktop ask Windows again.
pub fn clear() {
    *desktop() = None;
}

pub(crate) fn desktop_said() -> Option<(Option<PauseReason>, Duration)> {
    desktop().clone()
}

static HOLD_AFTER_MINIMIZED: Mutex<Vec<(isize, Duration)>> = Mutex::new(Vec::new());

/// The next pass of `window`'s capture that finds it minimized sleeps
/// `hold` right after IsIconic says so, once, so a restore can land between
/// that look and the rest of the pass. Only that window's: capture's window
/// tests run side by side, and others minimize, or hold, a window of their
/// own.
pub fn set_hold_after_minimized(window: isize, hold: Duration) {
    let mut holds = HOLD_AFTER_MINIMIZED
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    holds.retain(|&(set_for, _)| set_for != window);
    holds.push((window, hold));
}

pub(crate) fn hold_after_minimized(window: isize) -> Option<Duration> {
    let mut holds = HOLD_AFTER_MINIMIZED
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let at = holds.iter().position(|&(set_for, _)| set_for == window)?;
    Some(holds.swap_remove(at).1)
}
