// While a share runs, Windows' timer ticks every millisecond instead of
// every 15.6, so the frame the fps cap held, which waits in capture's
// AcquireNextFrame timeout, goes out when its slot comes and not up to 15 ms
// later. Since Windows 10 2004 the setting is this process's alone; some
// extra battery on a laptop is the cost, and only while sharing.

use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};

const PERIOD_MS: u32 = 1;

// Raises taken and not given back yet, in this process.
static HELD: AtomicU32 = AtomicU32::new(0);

// Holds the 1 ms timer until dropped: on every way out of a share, errors
// and panics included, the drop puts it back.
pub struct FineTimer {
    raised: bool,
}

impl FineTimer {
    // A refusal costs only the held frame's timing, so the share goes on
    // without it and the caller says why.
    pub fn raise() -> Result<FineTimer, String> {
        // SAFETY: a plain call with a constant argument.
        let answer = unsafe { timeBeginPeriod(PERIOD_MS) };
        if answer != TIMERR_NOERROR {
            return Err(format!(
                "could not raise the timer resolution to {PERIOD_MS} ms: timeBeginPeriod answered {answer}; a still screen's last frame can go out up to 15 ms late"
            ));
        }
        HELD.fetch_add(1, Ordering::AcqRel);
        Ok(FineTimer { raised: true })
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        if std::mem::take(&mut self.raised) {
            // SAFETY: the same period as the timeBeginPeriod that succeeded
            // for this value, given back once.
            unsafe { timeEndPeriod(PERIOD_MS) };
            HELD.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

// How many FineTimers this process holds now: what tests check to see that
// every raise was given back.
pub fn fine_timers_held() -> u32 {
    HELD.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Other tests in this binary do not share, so the count is this test's.
    #[test]
    fn every_raise_is_given_back_even_through_a_panic() {
        let before = fine_timers_held();
        let timer = FineTimer::raise().expect("Windows takes a 1 ms period");
        assert_eq!(fine_timers_held(), before + 1);
        let panicked = std::panic::catch_unwind(|| {
            let _inner = FineTimer::raise().expect("a second raise");
            panic!("on purpose, to show the drop runs on the way out");
        });
        assert!(panicked.is_err());
        assert_eq!(fine_timers_held(), before + 1);
        drop(timer);
        assert_eq!(fine_timers_held(), before);
    }
}
