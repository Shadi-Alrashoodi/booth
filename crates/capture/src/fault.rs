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
