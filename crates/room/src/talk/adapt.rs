// What the loss this PC's listeners hear does to how it sends. Fed once a
// second with the worst scattered loss any listener reported over the last
// 2 s: frames lost one or two in a row, the only kind the copy and Opus's
// repair data can bring back. No report means nobody heard this PC lately,
// which says nothing about the path, so nothing changes then.

use std::time::{Duration, Instant};

use voice::codec::Mode;

use crate::config::Timers;

// libopus 1.6 sends no repair data at 32 kbit/s below about 6 percent
// (voice::codec), so the 10 ms mode starts where it can pay off...
const REPAIR_AT_PERCENT: f32 = 5.0;
// ...and stops once the path is close to clean.
const REPAIR_OFF_UNDER_PERCENT: f32 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Switch {
    RedundancyOn { loss: f32 },
    RedundancyOff { clean_for: Duration },
    RepairOn { loss: f32, for_at_least: Duration },
    RepairOff { loss: f32, under_for: Duration },
}

pub(crate) struct Adapt {
    redundancy: bool,
    mode: Mode,
    // What Opus is told to expect, from the last report.
    expected_loss: u8,
    // Since when each report has been zero, at 5 percent or more, and under
    // 1 percent.
    clean_since: Option<Instant>,
    high_since: Option<Instant>,
    low_since: Option<Instant>,
}

impl Adapt {
    pub(crate) fn new() -> Adapt {
        Adapt {
            redundancy: false,
            mode: Mode::LowDelay,
            expected_loss: 0,
            clean_since: None,
            high_since: None,
            low_since: None,
        }
    }

    pub(crate) fn redundancy(&self) -> bool {
        self.redundancy
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn expected_loss(&self) -> u8 {
        self.expected_loss
    }

    // At most two switches come of one report: redundancy on and the 10 ms
    // mode on together, or both off.
    pub(crate) fn report(&mut self, loss: f32, now: Instant, timers: &Timers) -> Vec<Switch> {
        let loss = if loss.is_finite() {
            loss.clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.expected_loss = loss.ceil() as u8;
        let mut switches = Vec::new();

        if loss > 0.0 {
            self.clean_since = None;
            if !self.redundancy {
                self.redundancy = true;
                switches.push(Switch::RedundancyOn { loss });
            }
        } else {
            let since = *self.clean_since.get_or_insert(now);
            let clean_for = now.saturating_duration_since(since);
            if self.redundancy && clean_for >= timers.redundancy_off_after {
                self.redundancy = false;
                switches.push(Switch::RedundancyOff { clean_for });
            }
        }

        if loss >= REPAIR_AT_PERCENT {
            let since = *self.high_since.get_or_insert(now);
            let for_at_least = now.saturating_duration_since(since);
            if self.mode == Mode::LowDelay && for_at_least >= timers.repair_after {
                self.mode = Mode::Repair;
                switches.push(Switch::RepairOn { loss, for_at_least });
            }
        } else {
            self.high_since = None;
        }

        if loss < REPAIR_OFF_UNDER_PERCENT {
            let since = *self.low_since.get_or_insert(now);
            let under_for = now.saturating_duration_since(since);
            if self.mode == Mode::Repair && under_for >= timers.repair_off_after {
                self.mode = Mode::LowDelay;
                switches.push(Switch::RepairOff { loss, under_for });
            }
        } else {
            self.low_since = None;
        }
        switches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    // Reports once a second, as the room sends them.
    fn feed(adapt: &mut Adapt, start: Instant, from: u64, losses: &[f32]) -> Vec<(u64, Switch)> {
        let timers = Timers::default();
        let mut out = Vec::new();
        for (i, &loss) in losses.iter().enumerate() {
            let at = from + i as u64;
            for switch in adapt.report(loss, start + SECOND * at as u32, &timers) {
                out.push((at, switch));
            }
        }
        out
    }

    #[test]
    fn redundancy_on_and_off() {
        let start = Instant::now();
        let mut adapt = Adapt::new();
        assert!(feed(&mut adapt, start, 0, &[0.0; 40]).is_empty());
        assert!(!adapt.redundancy());

        let on = feed(&mut adapt, start, 40, &[0.5]);
        assert_eq!(on, [(40, Switch::RedundancyOn { loss: 0.5 })]);
        assert!(adapt.redundancy());
        assert_eq!(adapt.expected_loss(), 1);

        // A single lossy report inside the 30 s starts the count again.
        let mut losses = vec![0.0; 20];
        losses.push(2.0);
        losses.extend([0.0; 31]);
        let off = feed(&mut adapt, start, 41, &losses);
        assert_eq!(
            off,
            [(
                92,
                Switch::RedundancyOff {
                    clean_for: 30 * SECOND
                }
            )]
        );
        assert_eq!(adapt.mode(), Mode::LowDelay);
    }

    #[test]
    fn repair_mode_on_and_off() {
        let start = Instant::now();
        let mut adapt = Adapt::new();
        // Two reports apart are not 2 s of it.
        let got = feed(&mut adapt, start, 0, &[6.0, 3.0, 6.0, 6.0]);
        assert_eq!(got, [(0, Switch::RedundancyOn { loss: 6.0 })]);
        assert_eq!(adapt.mode(), Mode::LowDelay);
        let got = feed(&mut adapt, start, 4, &[10.0]);
        assert_eq!(
            got,
            [(
                4,
                Switch::RepairOn {
                    loss: 10.0,
                    for_at_least: 2 * SECOND
                }
            )]
        );
        assert_eq!(adapt.mode(), Mode::Repair);

        // 1 percent or more holds it; under 1 for 30 s ends it. Redundancy
        // needs its own 30 s at zero.
        let mut losses = vec![0.5; 10];
        losses.push(1.0);
        losses.extend([0.5; 30]);
        losses.extend([0.0; 31]);
        let got = feed(&mut adapt, start, 5, &losses);
        assert_eq!(
            got,
            [
                (
                    46,
                    Switch::RepairOff {
                        loss: 0.0,
                        under_for: 30 * SECOND
                    }
                ),
                (
                    76,
                    Switch::RedundancyOff {
                        clean_for: 30 * SECOND
                    }
                ),
            ]
        );
        assert_eq!((adapt.mode(), adapt.redundancy()), (Mode::LowDelay, false));
    }

    #[test]
    fn nonsense_loss_clamped() {
        let start = Instant::now();
        let mut adapt = Adapt::new();
        let timers = Timers::default();
        assert!(adapt.report(f32::NAN, start, &timers).is_empty());
        assert!(adapt.report(-4.0, start, &timers).is_empty());
        adapt.report(1e9, start, &timers);
        assert_eq!(adapt.expected_loss(), 100);
        assert!(adapt.redundancy());
    }
}
