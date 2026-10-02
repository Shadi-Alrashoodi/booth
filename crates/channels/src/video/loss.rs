// The parity follows the loss the viewer measures, counted from shards over
// the last 2 s. Each frame says how many shards it had, so a frame that
// arrived in part says exactly what it lost. A frame that never arrived at
// all says nothing, so it counts as the average frame of the window, all
// lost.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::time::{Duration, Instant};

use super::packetize::parity_count;

pub const LOSS_WINDOW: Duration = Duration::from_secs(2);

// A frame always has at least one data and one parity shard.
const FEWEST_SHARDS: u32 = 2;

// 2 s of 240 frames a second fit twice. This only bounds a flood.
const MAX_ENTRIES: usize = 1024;

pub const PARITY_DEFAULT: u32 = 20;
// A floor of 10 gave a frame of up to ten data shards a single parity shard,
// and at 5 percent loss dropped 4 frames in 100.
pub const PARITY_FLOOR: u32 = 20;
pub const PARITY_CEILING: u32 = 50;

// The parity the stats panel shows for this loss, as a percentage of a
// frame's data shards: twice the loss over the last 2 s, 20 to 50, and 20
// before anything has been measured. Each frame's own is parity_for_loss's,
// which gives more to a frame this rounds badly for, and can give less to a
// frame of 71 data shards or more at over 10 percent loss: the model needs
// less than twice the loss once a frame has that many packets.
pub fn parity_percent(loss_percent: Option<f32>) -> u32 {
    match loss_percent {
        Some(loss) if loss.is_finite() => {
            ((loss.max(0.0) * 2.0).ceil() as u32).clamp(PARITY_FLOOR, PARITY_CEILING)
        }
        _ => PARITY_DEFAULT,
    }
}

// What a share is held to: at 5 percent loss, under 1 frame in 100 is lost
// for good.
const MOST_LOST_FOR_GOOD: f64 = 0.01;

// Parity shards for a frame of `data` data shards at the viewers' loss: the
// fewest that keep its chance of being lost for good under 1 in 100, never
// fewer than the floor's share of the data and never more than the ceiling's.
// The floor alone gave a frame of 3 to 5 data shards one parity shard, which
// 5 percent loss beats 1.4 to 3.3 times in 100. Before any report, the
// default's share.
pub fn parity_for_loss(data: u16, loss_percent: Option<f32>) -> u16 {
    let Some(loss) = loss_percent.filter(|loss| loss.is_finite()) else {
        return parity_count(data, PARITY_DEFAULT);
    };
    let (least, most) = (
        parity_count(data, PARITY_FLOOR),
        parity_count(data, PARITY_CEILING),
    );
    if least >= most {
        return most;
    }
    let loss = f64::from(loss.clamp(0.0, 100.0)) / 100.0;
    // Any viewer can report any loss, so none may make the send path sum
    // for long. Once a frame's packets times the loss reach one past its
    // parity, the median count lost is past the parity: the frame is lost at
    // least half the time, with nothing to sum. Every sum left then starts
    // past the average count lost, where the terms only shrink and
    // lost_for_good stops early.
    let too_few = |parity: u16| {
        (f64::from(data) + f64::from(parity)) * loss >= f64::from(parity) + 1.0
            || lost_for_good(data, parity, loss) >= MOST_LOST_FOR_GOOD
    };
    if !too_few(least) {
        return least;
    }
    // The chance only falls as parity is added, so halve the way to the
    // fewest that does, or to the ceiling when none below it does.
    let (mut low, mut high) = (least, most);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if too_few(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    high
}

// The chance that a frame of `data` data and `parity` parity shards is lost
// for good, each of its packets lost on its own with chance `loss` (0 to 1):
// more of them lost than it has parity shards, since any `data` of them
// rebuild it. Summed in logarithms, because a big frame's terms are smaller
// than an f64 holds.
pub fn lost_for_good(data: u16, parity: u16, loss: f64) -> f64 {
    if data == 0 || loss.is_nan() || loss <= 0.0 {
        return 0.0;
    }
    if loss >= 1.0 {
        return 1.0;
    }
    let n = f64::from(data) + f64::from(parity);
    let (lost, kept) = (loss.ln(), (-loss).ln_1p());
    let mut k = f64::from(parity) + 1.0;
    let mut log_term =
        ln_factorial(n) - ln_factorial(k) - ln_factorial(n - k) + k * lost + (n - k) * kept;
    let mut sum = 0.0;
    while k <= n {
        let term = log_term.exp();
        sum += term;
        // Past the likeliest count the terms only shrink.
        if k >= n * loss && term <= sum * f64::EPSILON {
            break;
        }
        log_term += ((n - k) / (k + 1.0)).ln() + lost - kept;
        k += 1.0;
    }
    sum.min(1.0)
}

// Stirling's series for the log of the gamma function at n + 1, good to
// 1e-12 from 10 up; below that it steps up.
fn ln_factorial(n: f64) -> f64 {
    let mut x = n + 1.0;
    let mut below = 0.0;
    while x < 10.0 {
        below += x.ln();
        x += 1.0;
    }
    let x2 = x * x;
    let series = (1.0 / 12.0 - (1.0 / 360.0 - (1.0 / 1260.0 - 1.0 / (1680.0 * x2)) / x2) / x2) / x;
    (x - 0.5) * x.ln() - x + 0.5 * (2.0 * PI).ln() + series - below
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoLoss {
    pub lost: u32,
    pub expected: u32,
}

impl VideoLoss {
    pub fn percent(&self) -> Option<f32> {
        (self.expected > 0)
            .then(|| self.lost.min(self.expected) as f32 * 100.0 / self.expected as f32)
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    at: Instant,
    // Frames with packets, and the shards they said they had.
    seen: u32,
    seen_expected: u32,
    // Frames never seen, and the shards counted for them.
    unseen_expected: u32,
    received: u32,
}

#[derive(Debug, Default)]
pub(crate) struct LossWindow {
    entries: VecDeque<Entry>,
}

impl LossWindow {
    pub(crate) fn frame(&mut self, now: Instant, expected: u32, received: u32) {
        self.add(Entry {
            at: now,
            seen: 1,
            seen_expected: expected,
            unseen_expected: 0,
            received: received.min(expected),
        });
    }

    // Frames that never showed a packet. Returns the shards counted for them.
    pub(crate) fn unseen(&mut self, now: Instant, frames: u32) -> u32 {
        self.forget(now);
        let (seen, expected) = self
            .entries
            .iter()
            .fold((0u64, 0u64), |(seen, expected), entry| {
                (
                    seen + u64::from(entry.seen),
                    expected + u64::from(entry.seen_expected),
                )
            });
        let each = if seen == 0 {
            FEWEST_SHARDS
        } else {
            (expected.div_ceil(seen) as u32).max(FEWEST_SHARDS)
        };
        let shards = each.saturating_mul(frames);
        self.add(Entry {
            at: now,
            seen: 0,
            seen_expected: 0,
            unseen_expected: shards,
            received: 0,
        });
        shards
    }

    // A shard that came after its frame was out or dropped. It counts with
    // the newest entry, which is at most a few frames off.
    pub(crate) fn late_shard(&mut self, now: Instant) {
        match self.entries.back_mut() {
            Some(entry) => entry.received = entry.received.saturating_add(1),
            None => self.add(Entry {
                at: now,
                seen: 0,
                seen_expected: 0,
                unseen_expected: 0,
                received: 1,
            }),
        }
    }

    pub(crate) fn loss(&self, now: Instant) -> VideoLoss {
        let (expected, received) = self
            .entries
            .iter()
            .filter(|entry| now.saturating_duration_since(entry.at) < LOSS_WINDOW)
            .fold((0u32, 0u32), |(expected, received), entry| {
                (
                    expected
                        .saturating_add(entry.seen_expected)
                        .saturating_add(entry.unseen_expected),
                    received.saturating_add(entry.received),
                )
            });
        VideoLoss {
            lost: expected.saturating_sub(received),
            expected,
        }
    }

    fn add(&mut self, entry: Entry) {
        self.forget(entry.at);
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    fn forget(&mut self, now: Instant) {
        while let Some(entry) = self.entries.front() {
            if now.saturating_duration_since(entry.at) < LOSS_WINDOW {
                break;
            }
            self.entries.pop_front();
        }
    }
}
