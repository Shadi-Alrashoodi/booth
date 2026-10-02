use std::time::Duration;

// RFC 6298's G, the least margin above the smoothed round trip. On a calm
// link RTTVAR decays toward zero and the timeout would sit right on the
// average. The room's waits run on the 15.6 ms Windows tick, so a little more
// than one tick is the smallest margin worth having.
const GRANULARITY: Duration = Duration::from_millis(20);
pub(crate) const MIN_TIMEOUT: Duration = Duration::from_millis(20);
pub(crate) const MAX_TIMEOUT: Duration = Duration::from_secs(1);

/// The reliable channel's retransmit timeout, estimated from ping round trips
/// as in RFC 6298 section 2. Feed it pings only, timed on our own clock from
/// sending the ping to its pong arriving, so the peer's time to answer is in
/// the sample the way it is in an ack. Each pong answers exactly one ping, so
/// no sample is ambiguous the way the ack for a retransmitted message is, and
/// Karn's rule has nothing to exclude.
#[derive(Debug, Clone, Copy, Default)]
pub struct RttEstimator {
    // The smoothed round trip and its mean deviation, SRTT and RTTVAR.
    smoothed: Option<(Duration, Duration)>,
}

impl RttEstimator {
    pub fn new() -> RttEstimator {
        RttEstimator::default()
    }

    pub fn sample(&mut self, rtt: Duration) {
        self.smoothed = Some(match self.smoothed {
            None => (rtt, rtt / 2),
            Some((srtt, rttvar)) => {
                // The deviation is measured against the old SRTT.
                let rttvar = rttvar.saturating_mul(3) / 4 + srtt.abs_diff(rtt) / 4;
                let srtt = srtt.saturating_mul(7) / 8 + rtt / 8;
                (srtt, rttvar)
            }
        });
    }

    pub fn retransmit_timeout(&self) -> Option<Duration> {
        let (srtt, rttvar) = self.smoothed?;
        let margin = GRANULARITY.max(rttvar.saturating_mul(4));
        Some(srtt.saturating_add(margin).clamp(MIN_TIMEOUT, MAX_TIMEOUT))
    }

    // For a path change: round trips on the old path say nothing about the
    // new one.
    pub fn clear(&mut self) {
        self.smoothed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    #[test]
    fn follows_rfc_6298_by_hand() {
        let mut estimator = RttEstimator::new();
        assert_eq!(estimator.retransmit_timeout(), None);

        // SRTT 16, RTTVAR 8: 16 + 4 * 8.
        estimator.sample(ms(16));
        assert_eq!(estimator.retransmit_timeout(), Some(ms(48)));
        // RTTVAR 3/4 * 8 + 1/4 * |16 - 32| = 10, SRTT 7/8 * 16 + 1/8 * 32 = 18.
        estimator.sample(ms(32));
        assert_eq!(estimator.retransmit_timeout(), Some(ms(58)));
        // RTTVAR 7.5 + 86 / 4 = 29, SRTT 15.75 + 13 = 28.75.
        estimator.sample(ms(104));
        assert_eq!(estimator.retransmit_timeout(), Some(us(144_750)));
        // RTTVAR 21.75 + 12.75 / 4 = 24.9375, SRTT 25.15625 + 2 = 27.15625.
        estimator.sample(ms(16));
        assert_eq!(
            estimator.retransmit_timeout(),
            Some(Duration::from_nanos(126_906_250))
        );
    }

    #[test]
    fn a_steady_round_trip_keeps_the_granularity_as_margin() {
        let mut estimator = RttEstimator::new();
        for _ in 0..50 {
            estimator.sample(ms(30));
        }
        // RTTVAR has decayed to nanoseconds, far under G.
        assert_eq!(estimator.retransmit_timeout(), Some(ms(50)));
    }

    #[test]
    fn stays_between_twenty_ms_and_a_second() {
        let mut estimator = RttEstimator::new();
        estimator.sample(Duration::ZERO);
        assert_eq!(estimator.retransmit_timeout(), Some(ms(20)));

        let mut estimator = RttEstimator::new();
        estimator.sample(ms(600));
        assert_eq!(estimator.retransmit_timeout(), Some(ms(1000)));

        // Sums that overflow would panic.
        let mut estimator = RttEstimator::new();
        estimator.sample(Duration::from_micros(i64::MAX as u64));
        estimator.sample(Duration::MAX);
        assert_eq!(estimator.retransmit_timeout(), Some(ms(1000)));
    }

    #[test]
    fn clear_forgets_the_old_path() {
        let mut estimator = RttEstimator::new();
        estimator.sample(ms(16));
        estimator.clear();
        assert_eq!(estimator.retransmit_timeout(), None);
        estimator.sample(ms(80));
        assert_eq!(estimator.retransmit_timeout(), Some(ms(240)));
    }
}
