// A shared monitor that left the desktop: turned off, unplugged, or asleep
// with the PC, which brings it back only some seconds after the wake.
// Capture stays paused and looks for it once a second, and gives up at the
// first look that still misses it LOOKS looks after the first one, about
// 30 s. Looks are counted rather than time, since an Instant on Windows goes
// on through sleep: a minute asleep would use up the whole wait before the
// monitor could come back.

use std::time::{Duration, Instant};

pub(crate) const LOOK_EVERY: Duration = Duration::from_secs(1);
pub(crate) const LOOKS: u32 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Missed {
    Wait,
    GiveUp,
}

// Looks in a row that missed the monitor, and when the last one did.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Missing {
    misses: u32,
    last_miss: Option<Instant>,
}

impl Missing {
    // At once, unless the last look missed it less than LOOK_EVERY ago.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.last_miss
            .is_none_or(|at| now.saturating_duration_since(at) >= LOOK_EVERY)
    }

    pub(crate) fn missed(&mut self, now: Instant) -> Missed {
        self.misses += 1;
        self.last_miss = Some(now);
        if self.misses > LOOKS {
            Missed::GiveUp
        } else {
            Missed::Wait
        }
    }

    // Listed again. When it leaves again, the wait starts over.
    pub(crate) fn found(&mut self) {
        *self = Missing::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The share's thread asks every 100 ms while paused; only an ask that
    // is due looks at the list of monitors.
    #[test]
    fn a_monitor_that_stays_away_is_given_up_after_30_s() {
        let start = Instant::now();
        let mut missing = Missing::default();
        let mut at = start;
        let mut looks = 0;
        loop {
            if missing.due(at) {
                looks += 1;
                if missing.missed(at) == Missed::GiveUp {
                    break;
                }
            }
            at += Duration::from_millis(100);
            assert!(at < start + Duration::from_secs(60), "never gave up");
        }
        assert_eq!(looks, LOOKS + 1);
        assert_eq!(at - start, LOOK_EVERY * LOOKS);
    }

    // The monitor turned off as the PC went to sleep, and a minute asleep
    // moved the Instant on by a minute. That is one look, not the wait.
    #[test]
    fn time_asleep_does_not_use_up_the_wait() {
        let start = Instant::now();
        let mut missing = Missing::default();
        assert_eq!(missing.missed(start), Missed::Wait);
        let woke = start + Duration::from_secs(60);
        for second in 0..8u32 {
            let at = woke + LOOK_EVERY * second;
            assert!(missing.due(at), "{second} s after the wake");
            assert_eq!(
                missing.missed(at),
                Missed::Wait,
                "{second} s after the wake"
            );
        }
        missing.found();
        assert!(missing.due(woke + Duration::from_millis(7100)));
    }

    // Back within the wait, then gone again: a whole wait from there.
    #[test]
    fn a_monitor_back_in_time_starts_the_next_wait_over() {
        let start = Instant::now();
        let mut missing = Missing::default();
        missing.missed(start);
        assert!(!missing.due(start + Duration::from_millis(999)));
        for look in 1..LOOKS {
            assert_eq!(missing.missed(start + LOOK_EVERY * look), Missed::Wait);
        }
        missing.found();
        let again = start + Duration::from_secs(40);
        for look in 0..=LOOKS {
            let verdict = missing.missed(again + LOOK_EVERY * look);
            assert_eq!(verdict == Missed::GiveUp, look == LOOKS, "look {look}");
        }
    }
}
