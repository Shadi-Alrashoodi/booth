#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Good,
    Warn,
    Bad,
}

/// Where the strip changes colour.
///
/// Round trip and jitter: good below the first number, warn up to and
/// including the second, bad above it. Loss: good only at exactly zero, warn
/// below `loss_warn_below_pct`, bad from there up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub rtt_good_below_ms: f32,
    pub rtt_warn_up_to_ms: f32,
    pub jitter_good_below_ms: f32,
    pub jitter_warn_up_to_ms: f32,
    pub loss_warn_below_pct: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            rtt_good_below_ms: 40.0,
            rtt_warn_up_to_ms: 100.0,
            jitter_good_below_ms: 5.0,
            jitter_warn_up_to_ms: 15.0,
            loss_warn_below_pct: 2.0,
        }
    }
}

impl Thresholds {
    pub fn rtt_level(&self, ms: f32) -> Level {
        two_step(ms, self.rtt_good_below_ms, self.rtt_warn_up_to_ms)
    }

    pub fn jitter_level(&self, ms: f32) -> Level {
        two_step(ms, self.jitter_good_below_ms, self.jitter_warn_up_to_ms)
    }

    pub fn loss_level(&self, pct: f32) -> Level {
        if pct <= 0.0 {
            Level::Good
        } else if pct < self.loss_warn_below_pct {
            Level::Warn
        } else {
            Level::Bad
        }
    }
}

// A NaN fails both comparisons and lands on Bad, which is the colour that
// makes someone look at a number that should never be there.
fn two_step(value: f32, good_below: f32, warn_up_to: f32) -> Level {
    if value < good_below {
        Level::Good
    } else if value <= warn_up_to {
        Level::Warn
    } else {
        Level::Bad
    }
}
