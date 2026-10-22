use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::error::CaptureError;

// A waitable timer that wakes within a fraction of a millisecond; a plain
// one, or a sleep, wakes on the system tick, up to 15.6 ms late.
pub(crate) struct Timer(HANDLE);

// SAFETY: a timer handle may be waited on and set from any thread.
unsafe impl Send for Timer {}

impl Timer {
    pub(crate) fn new() -> Result<Timer, CaptureError> {
        // SAFETY: no name and no security attributes; the handle is checked
        // and closed on drop.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|err| CaptureError::windows("make a high resolution timer", &err))?;
        Ok(Timer(handle))
    }

    // Sets the timer to fire at `deadline`, for a caller that waits on it
    // with something else; false when Windows refused.
    pub(crate) fn set(&self, deadline: Instant) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        // Negative means relative, in 100 ns units.
        let due = -((left.as_nanos() / 100).max(1) as i64);
        // SAFETY: the handle is a live timer owned by self, `due` is a live
        // i64, and no completion routine is passed.
        unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) }.is_ok()
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    pub(crate) fn wait_until(&self, deadline: Instant) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        if !self.set(deadline) {
            std::thread::sleep(left);
            return;
        }
        // SAFETY: as above; the timer fires once, so the wait ends.
        unsafe {
            WaitForSingleObject(self.0, INFINITE);
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by self and closed only here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
