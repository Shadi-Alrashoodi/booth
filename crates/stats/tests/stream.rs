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

    // Arriving as 0, 2, 1: D = 0, then 1000000 us for 1 against 2, the one
    // before it in arrival, so J = 62.5 ms.
    let mut stats = StreamStats::new();
    for (seq, sent, arrived) in [
        (0, 0, 1_000),
        (2, 200_000, 201_000),
        (1, 100_000, 1_101_000),
    ] {
        stats.record(
            seq,
            Some(SENDER_CLOCK + sent),
            OUR_CLOCK + arrived,
            start + Duration::from_micros(arrived),
        );
    }
    assert_eq!(stats.jitter_ms(), Some(62.5));
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
fn far_late_frames_fill_gap() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..300, &[100, 101], |_| 1_000);
    // 1 s late, far behind the newest, and their gap still in the window.
    let late = at_ms(start, 1_498);
    stats.record(100, None, OUR_CLOCK, late);
    stats.record(101, None, OUR_CLOCK, late);
    steady(&mut stats, start, 0, 300..310, &[], |_| 1_000);
    assert_eq!(
        stats.loss(at_ms(start, 1_600)),
        Some(StreamLoss {
            lost: 0,
            expected: 310
        })
    );
}

#[test]
fn run_after_stale_pair() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..600, &[100, 101], |_| 1_000);
    // Over 2 s late, after their gap aged out, so placement restarts at
    // them. The run going on is not loss.
    let late = at_ms(start, 2_998);
    stats.record(100, None, OUR_CLOCK, late);
    stats.record(101, None, OUR_CLOCK, late);
    steady(&mut stats, start, 0, 600..700, &[], |_| 1_000);
    let end = at_ms(start, 3_500);
    let loss = stats.loss(end);
    assert_eq!(loss.unwrap().lost, 0);
    // What arrived before the pair is still known.
    stats.record(598, None, OUR_CLOCK, end);
    assert_eq!(stats.loss(end), loss);

    // Nor when 598 comes late or 599 twice right after the pair, or one
    // more stale packet comes from below it.
    for (lost, then) in [
        (&[100, 101, 598][..], &[100, 101, 598][..]),
        (&[100, 101], &[100, 101, 599]),
        (&[100, 110, 111], &[110, 111, 100]),
    ] {
        let mut stats = StreamStats::new();
        steady(&mut stats, start, 0, 0..600, lost, |_| 1_000);
        for &seq in then {
            stats.record(seq, None, OUR_CLOCK, late);
        }
        steady(&mut stats, start, 0, 600..700, &[], |_| 1_000);
        assert_eq!(stats.loss(end).unwrap().lost, 0, "{then:?}");
    }

    // Nor when the talker pauses for 2.5 s after the pair.
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..600, &[100, 101], |_| 1_000);
    stats.record(100, None, OUR_CLOCK, late);
    stats.record(101, None, OUR_CLOCK, late);
    steady(&mut stats, at_ms(start, 2_500), 0, 600..700, &[], |_| 1_000);
    assert_eq!(stats.loss(at_ms(start, 6_000)).unwrap().lost, 0);

    // A gap counted before the pair still counts.
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..600, &[100, 101, 500], |_| 1_000);
    stats.record(100, None, OUR_CLOCK, late);
    stats.record(101, None, OUR_CLOCK, late);
    steady(&mut stats, start, 0, 600..700, &[], |_| 1_000);
    assert_eq!(stats.loss(end).unwrap().lost, 1);
}

#[test]
fn stale_pairs_then_gap() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..600, &[20, 21, 160, 161], |_| 1_000);
    // Two stale pairs, the second far behind the first, then the run goes
    // on with 600 to 602 lost.
    let late = at_ms(start, 2_998);
    for seq in [160, 161, 20, 21] {
        stats.record(seq, None, OUR_CLOCK, late);
    }
    steady(&mut stats, start, 0, 603..700, &[], |_| 1_000);
    assert_eq!(stats.loss(at_ms(start, 3_500)).unwrap().lost, 3);
}

#[test]
fn stale_pair_after_restart() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    // Counting again 10100 lower, then a stale pair of the new run 3 s on.
    steady(&mut stats, start, 30_000, 0..100, &[], |_| 1_000);
    steady(&mut stats, start, 19_900, 100..700, &[200, 201], |_| 1_000);
    let late = at_ms(start, 3_500);
    stats.record(20_100, None, OUR_CLOCK, late);
    stats.record(20_101, None, OUR_CLOCK, late);
    steady(&mut stats, start, 19_900, 700..800, &[], |_| 1_000);
    assert_eq!(stats.loss(at_ms(start, 4_000)).unwrap().lost, 0);

    // Counting again twice, 2.5 s apart: the second jump is too far ahead
    // of the newest to be loss.
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 30_000, 0..100, &[], |_| 1_000);
    steady(&mut stats, start, 19_900, 100..600, &[], |_| 1_000);
    steady(&mut stats, start, 30_000, 600..700, &[], |_| 1_000);
    assert_eq!(stats.loss(at_ms(start, 3_500)).unwrap().lost, 0);
}

#[test]
fn restart_past_old_top() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..600, &[], |_| 1_000);
    // Counting again 129 lower, past the old top, then 611 to 749 lost.
    let first = 470u16.wrapping_sub(600);
    let lost: Vec<u16> = (741..880).collect();
    steady(&mut stats, start, first, 600..881, &lost, |_| 1_000);
    assert_eq!(
        stats.loss(at_ms(start, 4_500)),
        Some(StreamLoss {
            lost: 139,
            expected: 380
        })
    );
}

#[test]
fn gap_before_first_frame() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    for seq in [5, 2, 4] {
        stats.record(seq, None, OUR_CLOCK, start);
    }
    assert_eq!(
        stats.loss(start),
        Some(StreamLoss {
            lost: 1,
            expected: 4
        })
    );
    stats.record(3, None, OUR_CLOCK, start);
    assert_eq!(
        stats.loss(start),
        Some(StreamLoss {
            lost: 0,
            expected: 4
        })
    );
}

#[test]
fn gap_before_counting_again() {
    let start = Instant::now();
    let mut stats = StreamStats::new();
    steady(&mut stats, start, 0, 0..100, &[], |_| 1_000);
    let later = at_ms(start, 600);
    // A jump far ahead, then one from just before it: 4998 and 4999 lost.
    stats.record(5_000, None, OUR_CLOCK, later);
    stats.record(4_997, None, OUR_CLOCK, later);
    assert_eq!(stats.loss(later).unwrap().lost, 2);
    // A restart from far behind, then one from before the pair: 2999 lost.
    stats.record(3_000, None, OUR_CLOCK, later);
    stats.record(3_001, None, OUR_CLOCK, later);
    stats.record(2_998, None, OUR_CLOCK, later);
    assert_eq!(stats.loss(later).unwrap().lost, 3);
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
