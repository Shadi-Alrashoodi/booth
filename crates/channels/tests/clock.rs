use channels::ping::OFFSET_SAMPLES;
use channels::{ClockSample, OffsetEstimator, clock_sample};

fn sample(rtt_us: i64, offset_us: i64) -> ClockSample {
    ClockSample { rtt_us, offset_us }
}

#[test]
fn same_clock_symmetric_path() {
    // 500 us each way, peer holds the ping for 100 us.
    assert_eq!(
        clock_sample(1_000, 1_500, 1_600, 2_100),
        Some(sample(1_000, 0))
    );
}

#[test]
fn asymmetric_path_puts_half_the_difference_in_the_offset() {
    // 1000 us out, 3000 us back, clocks equal: the estimate is off by
    // (1000 - 3000) / 2.
    assert_eq!(
        clock_sample(0, 1_000, 1_000, 4_000),
        Some(sample(4_000, -1_000))
    );
}

#[test]
fn peer_clock_far_ahead() {
    // Peer is 5e12 us (about 58 days) ahead. 3000 us each way, held 200 us.
    // t2 = 1_000_000 + 3_000 + 5_000_000_000_000
    // t4 = 1_000_000 + 3_000 + 200 + 3_000
    let t1 = 1_000_000;
    let t2 = 5_000_001_003_000;
    let t3 = 5_000_001_003_200;
    let t4 = 1_006_200;
    assert_eq!(
        clock_sample(t1, t2, t3, t4),
        Some(sample(6_000, 5_000_000_000_000))
    );
}

#[test]
fn peer_clock_far_behind() {
    // We have been up 10^10 us, the peer only about a second. 2500 us each
    // way, held 100 us.
    // t2 = 10_000_000_000 + 2_500 - 9_999_000_000
    // offset = ((1_002_500 - 10_000_000_000) + (1_002_600 - 10_000_005_100)) / 2
    let t1 = 10_000_000_000;
    let t2 = 1_002_500;
    let t3 = 1_002_600;
    let t4 = 10_000_005_100;
    assert_eq!(
        clock_sample(t1, t2, t3, t4),
        Some(sample(5_000, -9_999_000_000))
    );
}

#[test]
fn peer_clock_across_the_wrap() {
    // The peer's clock sits 300 us behind ours, just below 2^64 while ours
    // is just above zero. 50 us each way, no hold.
    let t1 = 100;
    let t2 = u64::MAX - 149;
    let t3 = t2;
    let t4 = 200;
    assert_eq!(clock_sample(t1, t2, t3, t4), Some(sample(100, -300)));

    // And the other way round: ours about to wrap, the peer's already did.
    let t1 = u64::MAX - 99;
    let t2 = 250;
    let t3 = 260;
    let t4 = t1.wrapping_add(110);
    assert_eq!(clock_sample(t1, t2, t3, t4), Some(sample(100, 300)));
}

#[test]
fn impossible_samples_are_rejected() {
    // Pong arrives before the ping left.
    assert_eq!(clock_sample(2_000, 5, 6, 1_000), None);
    // Peer says it sent the pong before the ping arrived.
    assert_eq!(clock_sample(1_000, 600, 500, 2_000), None);
    // Peer says it held the ping longer than the whole round trip.
    assert_eq!(clock_sample(1_000, 0, 1_001, 2_000), None);
    assert_eq!(clock_sample(0, 0, u64::MAX, 10), None);
}

#[test]
fn zero_round_trip_is_accepted() {
    assert_eq!(clock_sample(10, 10, 20, 20), Some(sample(0, 0)));
}

#[test]
fn estimator_starts_empty() {
    assert_eq!(OffsetEstimator::new().best(), None);
}

#[test]
fn estimator_picks_the_shortest_round_trip() {
    let mut estimator = OffsetEstimator::new();
    estimator.push(sample(9_000, 4_000));
    estimator.push(sample(1_200, 150));
    estimator.push(sample(30_000, -12_000));
    estimator.push(sample(1_900, 700));
    assert_eq!(estimator.best(), Some(sample(1_200, 150)));
}

#[test]
fn estimator_prefers_the_newest_on_a_tie() {
    let mut estimator = OffsetEstimator::new();
    estimator.push(sample(1_000, 10));
    estimator.push(sample(1_000, 20));
    estimator.push(sample(5_000, 30));
    assert_eq!(estimator.best(), Some(sample(1_000, 20)));
}

#[test]
fn estimator_forgets_samples_older_than_the_last_32() {
    let mut estimator = OffsetEstimator::new();
    estimator.push(sample(100, 1));
    for i in 0..OFFSET_SAMPLES as i64 - 1 {
        estimator.push(sample(2_000 + i, 2));
    }
    assert_eq!(estimator.best(), Some(sample(100, 1)));
    estimator.push(sample(2_500, 3));
    assert_eq!(estimator.best(), Some(sample(2_000, 2)));
}

#[test]
fn estimator_clear_drops_everything() {
    let mut estimator = OffsetEstimator::new();
    estimator.push(sample(100, 1));
    estimator.clear();
    assert_eq!(estimator.best(), None);
    estimator.push(sample(900, 5));
    assert_eq!(estimator.best(), Some(sample(900, 5)));
}
