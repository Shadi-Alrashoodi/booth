use std::ops::Range;
use std::time::{Duration, Instant};

use stats::{STREAM_WINDOW, StreamLoss, StreamStats};

// The sender's clock and ours are unrelated; only differences may matter.
const SENDER_CLOCK: u64 = 9_000_000_000;
const OUR_CLOCK: u64 = 55_555;

// The 5 ms voice frames numbered `frames`, counted on from `first`, each
// `transit` us on the way, with the ones in `lost` never arriving.
fn steady(
    stats: &mut StreamStats,
    start: Instant,
    first: u16,
    frames: Range<u16>,
    lost: &[u16],
    transit: impl Fn(u16) -> u64,
) {
    for i in frames {
        if lost.contains(&i) {
            continue;
        }
        let sent = u64::from(i) * 5_000;
        let arrived = sent + transit(i);
        stats.record(
            first.wrapping_add(i),
            Some(SENDER_CLOCK + sent),
            OUR_CLOCK + arrived,
            start + Duration::from_micros(arrived),
        );
    }
}

fn at_ms(start: Instant, ms: u64) -> Instant {
    start + Duration::from_millis(ms)
}

#[test]
fn a_steady_stream_has_no_jitter_and_no_loss() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    assert_eq!(stats.loss(start), None);
    assert_eq!(stats.jitter_ms(), None);
    steady(&mut stats, start, 100, 0..200, &[], |_| 3_000);
    assert_eq!(stats.jitter_ms(), Some(0.0));
    assert_eq!(
        stats.loss(at_ms(start, 1_000)),
        Some(StreamLoss {
            lost: 0,
            expected: 200
        })
    );
}

#[test]
fn every_tenth_frame_missing_reads_ten_percent() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    let lost: Vec<u16> = (5..400).step_by(10).collect();
    steady(&mut stats, start, 65_000, 0..400, &lost, |_| 1_000);
    let loss = stats.loss(at_ms(start, 2_000)).unwrap();
    assert_eq!(loss.lost, 40);
    assert_eq!(loss.percent(), Some(10.0));
}

#[test]
fn only_the_last_2_s_count() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    // Every other frame lost for a second, then half a second clean.
    let lost: Vec<u16> = (0..200).filter(|i| i % 2 == 1).collect();
    steady(&mut stats, start, 0, 0..300, &lost, |_| 1_000);
    let loss = stats.loss(at_ms(start, 1_500)).unwrap();
    assert_eq!((loss.lost, loss.expected), (100, 300));
    // 2 s clean after that second. The first frame after it still carries
    // the gap before it until it is 2 s old itself.
    steady(&mut stats, start, 0, 300..600, &[], |_| 1_000);
    assert_eq!(stats.loss(at_ms(start, 3_000)).unwrap().lost, 1);
    let end = at_ms(start, 3_005);
    assert_eq!(stats.loss(end).unwrap().percent(), Some(0.0));
    assert_eq!(stats.loss(end + STREAM_WINDOW), None);
}

#[test]
fn a_late_frame_fills_its_gap_and_a_second_copy_counts_once() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    let order = [0u16, 1, 4, 2, 3, 3, 5, 5, 6];
    for (i, &seq) in order.iter().enumerate() {
        let at = start + Duration::from_millis(5 * i as u64);
        stats.record(seq, None, OUR_CLOCK, at);
    }
    let loss = stats.loss(at_ms(start, 100)).unwrap();
    assert_eq!(
        loss,
        StreamLoss {
            lost: 0,
            expected: 7
        }
    );
    // Nothing had a send time, so there is no jitter to tell.
    assert_eq!(stats.jitter_ms(), None);
}

#[test]
fn jitter_follows_rfc_3550_in_arrival_order() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    // Transit 1000, 1000, 3000, 1000 and 1500 us. As in the ping stats'
    // own test: D = 0, 2000, -2000, 500, so J = 258.30078125 us.
    let transit = [1_000, 1_000, 3_000, 1_000, 1_500];
    steady(&mut stats, start, 7, 0..5, &[], |i| transit[usize::from(i)]);
    let jitter = stats.jitter_ms().unwrap();
    assert!((jitter - 0.258_300_78).abs() < 1e-5, "{jitter}");
}

#[test]
fn sender_counting_again_is_followed() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 1_000, 0..100, &[], |_| 1_000);
    // Far ahead: a new room on the sender's side.
    let later = at_ms(start, 600);
    stats.record(30_000, None, OUR_CLOCK, later);
    stats.record(30_001, None, OUR_CLOCK, later);
    // Far behind, twice in a row: the same, and a gap after it is loss.
    stats.record(20_000, None, OUR_CLOCK, later);
    stats.record(20_001, None, OUR_CLOCK, later);
    stats.record(20_003, None, OUR_CLOCK, later);
    assert_eq!(
        stats.loss(later),
        Some(StreamLoss {
            lost: 1,
            expected: 105
        })
    );
    // One stray from far behind is not followed, and not counted.
    stats.record(10_000, None, OUR_CLOCK, later);
    stats.record(20_004, None, OUR_CLOCK, later);
    assert_eq!(
        stats.loss(later),
        Some(StreamLoss {
            lost: 1,
            expected: 106
        })
    );
}

#[test]
fn streams_add_up() {
    let one = StreamLoss {
        lost: 3,
        expected: 100,
    };
    let two = StreamLoss {
        lost: 1,
        expected: 300,
    };
    assert_eq!((one + two).percent(), Some(1.0));
    assert_eq!(StreamLoss::default().percent(), None);
}
