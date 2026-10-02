use std::time::{Duration, Instant};

use stats::{LOST_AFTER, LinkStats, TRACE_LEN, TraceSample};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn rtt(n: u64) -> TraceSample {
    TraceSample::Rtt(n as f32)
}

// Pings every 100 ms, like the rate while media flows. Each one is answered
// right away with its own round trip, or not at all when it is None.
fn run(stats: &mut LinkStats, t0: Instant, first_seq: u32, answers: &[Option<u64>]) -> Instant {
    let mut t = t0;
    for (i, answer) in answers.iter().enumerate() {
        let seq = first_seq.wrapping_add(i as u32);
        t = t0 + ms(100 * i as u64);
        stats.ping_sent(seq, t);
        if let Some(r) = answer {
            stats.pong_received(seq, ms(*r), t + ms(*r));
        }
    }
    t
}

#[test]
fn nothing_measured_yet() {
    let s = LinkStats::new().snapshot();
    assert_eq!(s.rtt_ms, None);
    assert_eq!(s.rtt_avg_ms, None);
    assert_eq!(s.rtt_p95_ms, None);
    assert_eq!(s.loss_pct, None);
    assert_eq!(s.jitter_ms, None);
    assert_eq!(s.inbound_loss_pct, None);
    assert!(s.trace.is_empty());
}

#[test]
fn answered_ping_shows_its_round_trip() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(7, t0);
    // The peer took 1 ms to answer. The panel leaves that out; the time
    // handed back for the retransmit timer keeps it, as an ack would.
    assert_eq!(stats.pong_received(7, ms(9), t0 + ms(10)), Some(ms(10)));
    let s = stats.snapshot();
    assert_eq!(s.rtt_ms, Some(9.0));
    assert_eq!(s.trace, vec![rtt(9)]);
    assert_eq!((s.pings_sent, s.pongs_received, s.lost), (1, 1, 0));
    assert_eq!(s.loss_pct, Some(0.0));
}

#[test]
fn trace_keeps_sequence_order_with_late_pongs_and_expiry() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(1, t0);
    stats.ping_sent(2, t0 + ms(100));
    stats.ping_sent(3, t0 + ms(200));

    stats.pong_received(3, ms(20), t0 + ms(220));
    let s = stats.snapshot();
    assert!(s.trace.is_empty(), "ping 3 must wait for 1 and 2");
    assert_eq!(s.rtt_ms, Some(20.0));

    stats.pong_received(1, ms(30), t0 + ms(230));
    let s = stats.snapshot();
    assert_eq!(s.trace, vec![rtt(30)]);
    assert_eq!(
        s.rtt_ms,
        Some(20.0),
        "a late pong must not replace a newer one"
    );

    stats.tick(t0 + ms(100) + LOST_AFTER - ms(1));
    assert_eq!(stats.snapshot().trace.len(), 1);

    stats.tick(t0 + ms(100) + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(s.trace, vec![rtt(30), TraceSample::Lost, rtt(20)]);
    assert_eq!(s.lost, 1);

    assert_eq!(stats.pong_received(2, ms(40), t0 + ms(2500)), None);
    let s = stats.snapshot();
    assert_eq!(s.trace, vec![rtt(30), TraceSample::Lost, rtt(20)]);
    assert_eq!((s.pongs_received, s.lost), (2, 1));
}

#[test]
fn pong_arriving_at_lost_after_is_lost_even_without_a_tick() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(1, t0);
    assert_eq!(stats.pong_received(1, ms(1900), t0 + LOST_AFTER), None);
    let s = stats.snapshot();
    assert_eq!(s.trace, vec![TraceSample::Lost]);
    assert_eq!((s.pongs_received, s.lost), (0, 1));
    assert_eq!(s.rtt_ms, None);
}

#[test]
fn unknown_and_repeated_pongs_are_ignored() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(1, t0);
    assert_eq!(stats.pong_received(99, ms(5), t0 + ms(5)), None);
    assert_eq!(stats.pong_received(1, ms(8), t0 + ms(8)), Some(ms(8)));
    assert_eq!(stats.pong_received(1, ms(3), t0 + ms(9)), None);
    let s = stats.snapshot();
    assert_eq!(s.trace, vec![rtt(8)]);
    assert_eq!(s.pongs_received, 1);
    assert_eq!(s.rtt_ms, Some(8.0));
}

#[test]
fn round_trip_never_exceeds_what_our_clock_saw() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(1, t0);
    assert_eq!(stats.pong_received(1, ms(500), t0 + ms(12)), Some(ms(12)));
    assert_eq!(stats.snapshot().rtt_ms, Some(12.0));
}

#[test]
fn repeated_seq_on_send_is_not_a_second_ping() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(4, t0);
    stats.ping_sent(4, t0 + ms(1));
    stats.pong_received(4, ms(3), t0 + ms(4));
    let s = stats.snapshot();
    assert_eq!(s.pings_sent, 1);
    assert_eq!(s.trace, vec![rtt(3)]);
}

#[test]
fn loss_is_over_the_last_hundred_finalized_pings() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let dropped = [2, 9, 10, 64, 109];
    let answers: Vec<Option<u64>> = (0..110)
        .map(|i| if dropped.contains(&i) { None } else { Some(5) })
        .collect();
    let last = run(&mut stats, t0, 0, &answers);
    stats.tick(last + LOST_AFTER);

    let s = stats.snapshot();
    assert_eq!(s.trace.len(), 110);
    assert_eq!(s.lost, 5);
    // 10, 64 and 109 fall inside pings 10 to 109; 2 and 9 are older.
    assert_eq!(s.loss_pct, Some(3.0));
    for i in dropped {
        assert_eq!(s.trace[i], TraceSample::Lost, "gap for ping {i}");
    }
}

#[test]
fn loss_before_a_hundred_pings_uses_what_there_is() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let mut answers = vec![Some(5); 50];
    answers[3] = None;
    let last = run(&mut stats, t0, 0, &answers);
    stats.tick(last + LOST_AFTER);
    assert_eq!(stats.snapshot().loss_pct, Some(2.0));
}

#[test]
fn trace_is_capped() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let answers: Vec<Option<u64>> = (1..=200).map(Some).collect();
    run(&mut stats, t0, 0, &answers);
    let s = stats.snapshot();
    assert_eq!(s.trace.len(), TRACE_LEN);
    assert_eq!(s.trace.first(), Some(&rtt(81)));
    assert_eq!(s.trace.last(), Some(&rtt(200)));
    assert_eq!(s.rtt_min_ms, Some(81.0));
    assert_eq!(s.rtt_max_ms, Some(200.0));
    assert_eq!(s.pings_sent, 200);
}

#[test]
fn summary_on_known_data() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    // 100 down to 1 so the answers are not already sorted.
    let answers: Vec<Option<u64>> = (1..=100).rev().map(Some).collect();
    run(&mut stats, t0, 0, &answers);
    let s = stats.snapshot();
    assert_eq!(s.rtt_min_ms, Some(1.0));
    assert_eq!(s.rtt_max_ms, Some(100.0));
    assert_eq!(s.rtt_avg_ms, Some(50.5));
    assert_eq!(s.rtt_p95_ms, Some(95.0));
    assert_eq!(s.rtt_ms, Some(1.0));
}

#[test]
fn summary_skips_lost_pings() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let answers = [Some(10), None, Some(20), None, Some(30), Some(40)];
    let last = run(&mut stats, t0, 0, &answers);
    stats.tick(last + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(s.rtt_avg_ms, Some(25.0));
    assert_eq!(s.rtt_min_ms, Some(10.0));
    assert_eq!(s.rtt_max_ms, Some(40.0));
    // Nearest rank of 4 samples: ceil(3.8) = 4th.
    assert_eq!(s.rtt_p95_ms, Some(40.0));
}

#[test]
fn p95_of_twenty_is_the_nineteenth() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let answers: Vec<Option<u64>> = (1..=20).map(|n| Some(n * 2)).collect();
    run(&mut stats, t0, 0, &answers);
    assert_eq!(stats.snapshot().rtt_p95_ms, Some(38.0));
}

#[test]
fn summary_is_empty_when_every_ping_was_lost() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let last = run(&mut stats, t0, 0, &[None, None]);
    stats.tick(last + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(s.trace, vec![TraceSample::Lost, TraceSample::Lost]);
    assert_eq!(s.loss_pct, Some(100.0));
    assert_eq!(s.rtt_avg_ms, None);
    assert_eq!(s.rtt_p95_ms, None);
}

#[test]
fn our_sequence_wraps() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let first = u32::MAX - 2;
    let seqs: Vec<u32> = (0..6).map(|i| first.wrapping_add(i)).collect();
    assert_eq!(seqs, [u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1, 2]);
    for (i, seq) in seqs.iter().enumerate() {
        stats.ping_sent(*seq, t0 + ms(100 * i as u64));
    }
    // Answered newest first, and seq 0 never.
    let at = t0 + ms(600);
    for (i, seq) in seqs.iter().enumerate().rev() {
        if *seq != 0 {
            stats.pong_received(*seq, ms(i as u64 + 1), at);
        }
    }
    assert_eq!(stats.snapshot().trace, vec![rtt(1), rtt(2), rtt(3)]);

    stats.tick(t0 + ms(300) + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(
        s.trace,
        vec![rtt(1), rtt(2), rtt(3), TraceSample::Lost, rtt(5), rtt(6)]
    );
    assert_eq!(s.rtt_ms, Some(6.0));
}

#[test]
fn round_trip_goes_blank_when_newer_pings_go_unanswered() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    stats.ping_sent(0, t0);
    stats.pong_received(0, ms(12), t0 + ms(12));
    // From here the peer's pings may still reach us, but our pings or its
    // answers no longer get through.
    for i in 1..=20u32 {
        stats.ping_sent(i, t0 + ms(100 * u64::from(i)));
    }

    stats.tick(t0 + ms(100) + LOST_AFTER - ms(1));
    assert_eq!(stats.snapshot().rtt_ms, Some(12.0));

    stats.tick(t0 + ms(100) + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(s.rtt_ms, None);
    assert_eq!(s.trace, vec![rtt(12), TraceSample::Lost]);
    assert_eq!(s.rtt_avg_ms, Some(12.0));

    // The first answer after that brings the number back, and the pings
    // still expiring behind it do not take it away again.
    stats.ping_sent(21, t0 + ms(2100));
    stats.pong_received(21, ms(15), t0 + ms(2115));
    assert_eq!(stats.snapshot().rtt_ms, Some(15.0));
    stats.tick(t0 + ms(2100) + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(s.rtt_ms, Some(15.0));
    assert_eq!(s.lost, 20);
}

#[test]
fn a_lost_ping_between_answered_ones_keeps_the_round_trip() {
    let mut stats = LinkStats::new();
    let t0 = Instant::now();
    let last = run(&mut stats, t0, 0, &[Some(10), None, Some(11), Some(12)]);
    stats.tick(last + LOST_AFTER);
    let s = stats.snapshot();
    assert_eq!(s.lost, 1);
    assert_eq!(s.rtt_ms, Some(12.0));
}
