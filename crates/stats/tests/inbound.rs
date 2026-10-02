use stats::LinkStats;

// The peer's clock and ours are unrelated; only differences may matter.
const PEER_CLOCK: u64 = 7_000_000_000;
const OUR_CLOCK: u64 = 123_456_789;

fn feed(stats: &mut LinkStats, pings: &[(u32, u64, u64)]) {
    for (seq, sent, arrived) in pings {
        stats.peer_ping_received(*seq, PEER_CLOCK + sent, OUR_CLOCK + arrived);
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn jitter_needs_two_pings() {
    let mut stats = LinkStats::new();
    feed(&mut stats, &[(0, 0, 1000)]);
    assert_eq!(stats.snapshot().jitter_ms, None);
    feed(&mut stats, &[(1, 100_000, 101_000)]);
    assert_eq!(stats.snapshot().jitter_ms, Some(0.0));
}

#[test]
fn jitter_is_zero_for_regular_arrivals() {
    let mut stats = LinkStats::new();
    let pings: Vec<(u32, u64, u64)> = (0..50u32)
        .map(|i| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 4_000))
        .collect();
    feed(&mut stats, &pings);
    assert_eq!(stats.snapshot().jitter_ms, Some(0.0));
}

#[test]
fn uneven_sending_with_steady_transit_is_not_jitter() {
    let mut stats = LinkStats::new();
    let sends = [0u64, 90_000, 230_000, 300_000, 1_300_000, 1_310_000];
    let pings: Vec<(u32, u64, u64)> = sends
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u32, *s, s + 2_500))
        .collect();
    feed(&mut stats, &pings);
    assert_eq!(stats.snapshot().jitter_ms, Some(0.0));
}

#[test]
fn jitter_matches_hand_computed_rfc3550() {
    let mut stats = LinkStats::new();
    // Transit times 1000, 1000, 3000, 1000, 1500 us.
    // D = 0, 2000, -2000, 500
    // J = 0
    //   -> 0 + (2000 - 0) / 16          = 125
    //   -> 125 + (2000 - 125) / 16      = 242.1875
    //   -> 242.1875 + (500 - 242.1875) / 16 = 258.30078125 us
    feed(
        &mut stats,
        &[
            (0, 0, 1_000),
            (1, 100_000, 101_000),
            (2, 200_000, 203_000),
            (3, 300_000, 301_000),
            (4, 400_000, 401_500),
        ],
    );
    let j = stats.snapshot().jitter_ms.unwrap();
    assert!(close(j, 0.258_300_78), "jitter {j}");
}

#[test]
fn jitter_uses_arrival_order() {
    let mut stats = LinkStats::new();
    // Ping 2 overtakes ping 1, which spends 1 s longer in transit. Taken in
    // arrival order, 0 then 2 gives D = 0, and 2 then 1 gives
    // (1_101_000 - 201_000) - (100_000 - 200_000) = 1_000_000 us,
    // so J = 1_000_000 / 16 us = 62.5 ms.
    feed(
        &mut stats,
        &[
            (0, 0, 1_000),
            (2, 200_000, 201_000),
            (1, 100_000, 1_101_000),
        ],
    );
    let s = stats.snapshot();
    assert_eq!(s.jitter_ms, Some(62.5));
    assert_eq!(s.inbound_loss_pct, Some(0.0));
}

#[test]
fn inbound_loss_from_gaps() {
    let mut stats = LinkStats::new();
    // 5 and 17 end up 94 and 82 behind the newest, past the low 64 bits.
    let pings: Vec<(u32, u64, u64)> = (0..100u32)
        .filter(|i| *i != 5 && *i != 17)
        .map(|i| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 1_000))
        .collect();
    feed(&mut stats, &pings);
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(2.0));
}

#[test]
fn inbound_loss_counts_only_the_last_hundred() {
    let mut stats = LinkStats::new();
    // The window is 10 to 109: 9 is the newest gap outside it, 10 the oldest
    // inside.
    let pings: Vec<(u32, u64, u64)> = (0..110u32)
        .filter(|i| ![1, 4, 9, 10].contains(i))
        .map(|i| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 1_000))
        .collect();
    feed(&mut stats, &pings);
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(1.0));
}

#[test]
fn reordered_ping_fills_its_gap() {
    let mut stats = LinkStats::new();
    feed(
        &mut stats,
        &[(0, 0, 1_000), (1, 100_000, 101_000), (3, 300_000, 301_000)],
    );
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(25.0));
    feed(&mut stats, &[(2, 200_000, 302_000)]);
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(0.0));
}

#[test]
fn duplicates_change_nothing() {
    let mut stats = LinkStats::new();
    feed(
        &mut stats,
        &[(0, 0, 1_000), (1, 100_000, 101_000), (2, 200_000, 201_000)],
    );
    let before = stats.snapshot();
    // Same sequence numbers again, with timestamps that would be huge jitter
    // if they were used.
    feed(&mut stats, &[(1, 0, 9_000_000), (2, 5, 1), (0, 0, 0)]);
    let after = stats.snapshot();
    assert_eq!(after.jitter_ms, before.jitter_ms);
    assert_eq!(after.inbound_loss_pct, Some(0.0));
}

#[test]
fn duplicates_far_back_are_still_recognised() {
    let mut stats = LinkStats::new();
    let steady = |i: u32| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 1_000);
    let pings: Vec<(u32, u64, u64)> = (0..200).map(steady).collect();
    feed(&mut stats, &pings);
    // 127 and 126 behind the newest, the far end of what is remembered. Taken
    // as far behind instead, the pair would read as the peer counting again
    // and the wild timestamps would reach the jitter.
    feed(&mut stats, &[(72, 0, 9_000_000), (73, 5, 1)]);
    let s = stats.snapshot();
    assert_eq!(s.jitter_ms, Some(0.0));
    assert_eq!(s.inbound_loss_pct, Some(0.0));
}

#[test]
fn joining_mid_stream_is_not_loss() {
    let mut stats = LinkStats::new();
    feed(
        &mut stats,
        &[
            (1000, 0, 1_000),
            (1001, 100_000, 101_000),
            (1002, 200_000, 201_000),
        ],
    );
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(0.0));
}

#[test]
fn first_ping_arriving_late_is_counted() {
    let mut stats = LinkStats::new();
    feed(&mut stats, &[(11, 100_000, 101_000), (10, 0, 102_000)]);
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(0.0));
    feed(&mut stats, &[(13, 300_000, 301_000)]);
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(25.0));
}

#[test]
fn peer_sequence_wraps() {
    let mut stats = LinkStats::new();
    let first = u32::MAX - 14;
    let pings: Vec<(u32, u64, u64)> = (0..100u32)
        .map(|i| (first.wrapping_add(i), u64::from(i) * 100_000))
        .filter(|(seq, _)| *seq != u32::MAX && *seq != 0)
        .map(|(seq, sent)| (seq, sent, sent + 1_000))
        .collect();
    feed(&mut stats, &pings);
    let s = stats.snapshot();
    assert_eq!(s.inbound_loss_pct, Some(2.0));
    assert_eq!(s.jitter_ms, Some(0.0));

    // u32::MAX shows up late, after the wrap, and fills its gap.
    feed(&mut stats, &[(u32::MAX, 1_400_000, 9_902_000)]);
    assert_eq!(stats.snapshot().inbound_loss_pct, Some(1.0));
}

#[test]
fn far_jump_in_sequence_is_all_loss_not_a_crash() {
    let mut stats = LinkStats::new();
    feed(
        &mut stats,
        &[(0, 0, 1_000), (0x7fff_ffff, 100_000, 101_000)],
    );
    let loss = stats.snapshot().inbound_loss_pct.unwrap();
    assert!(close(loss, 99.0), "loss {loss}");
    // Half the sequence space away reads as behind, and is dropped.
    feed(
        &mut stats,
        &[(0x7fff_ffffu32.wrapping_add(0x8000_0000), 0, 0)],
    );
    assert!(close(stats.snapshot().inbound_loss_pct.unwrap(), loss));
}

#[test]
fn clocks_wrapping_is_not_jitter() {
    let mut stats = LinkStats::new();
    // Steady 2 ms transit, 100 ms apart. The peer's clock wraps after the
    // second ping and ours after the fourth.
    let peer_start = u64::MAX - 150_000;
    let our_start = u64::MAX - 350_000;
    for i in 0..6u32 {
        let step = u64::from(i) * 100_000;
        stats.peer_ping_received(
            i,
            peer_start.wrapping_add(step),
            our_start.wrapping_add(step + 2_000),
        );
    }
    assert_eq!(stats.snapshot().jitter_ms, Some(0.0));
}

#[test]
fn one_wild_peer_timestamp_is_capped_and_fades() {
    let mut stats = LinkStats::new();
    let steady = |i: u32| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 1_000);
    feed(&mut stats, &[steady(0), steady(1)]);
    stats.peer_ping_received(2, 0, OUR_CLOCK + 201_000);
    feed(&mut stats, &[steady(3)]);
    // Both pairs around the wild ping swing by far more than LOST_AFTER, so
    // each adds the 2 s cap: 2_000_000 / 16, then 125_000 + 1_875_000 / 16.
    assert_eq!(stats.snapshot().jitter_ms, Some(242.1875));

    let pings: Vec<(u32, u64, u64)> = (4..104).map(steady).collect();
    feed(&mut stats, &pings);
    let j = stats.snapshot().jitter_ms.unwrap();
    assert!(j < 1.0, "jitter {j}");
}

#[test]
fn peer_restarting_its_count_is_followed() {
    let mut stats = LinkStats::new();
    let old: Vec<(u32, u64, u64)> = (0..200u32)
        .map(|i| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 1_000))
        .collect();
    feed(&mut stats, &old);

    // The peer's app restarts: its count starts again at 0 and its clock
    // from a new epoch. 1 and 20 are lost on the way.
    let new_epoch = 900_000_000_000;
    let arrived_from = 30_000_000;
    for i in (0..30u32).filter(|i| *i != 1 && *i != 20) {
        let step = u64::from(i) * 100_000;
        stats.peer_ping_received(i, new_epoch + step, OUR_CLOCK + arrived_from + step + 3_000);
    }
    // Fewer pings than the loss window, so counting any of the old run would
    // show here.
    let s = stats.snapshot();
    assert!(close(s.inbound_loss_pct.unwrap(), 200.0 / 30.0));
    assert_eq!(s.jitter_ms, Some(0.0), "the two epochs were compared");

    // 16 ms more transit on the next one moves the jitter by 16 / 16 ms.
    stats.peer_ping_received(
        30,
        new_epoch + 3_000_000,
        OUR_CLOCK + arrived_from + 3_000_000 + 19_000,
    );
    assert_eq!(stats.snapshot().jitter_ms, Some(1.0));
}

#[test]
fn late_stragglers_from_far_back_do_not_restart_the_count() {
    let mut stats = LinkStats::new();
    let steady = |i: u32| (i, u64::from(i) * 100_000, u64::from(i) * 100_000 + 1_000);
    let pings: Vec<(u32, u64, u64)> = (0..200).map(steady).collect();
    feed(&mut stats, &pings);

    // Two old pings turn up very late with a current one between them, so
    // they never arrive back to back.
    feed(
        &mut stats,
        &[steady(50), steady(200), steady(51), steady(201)],
    );
    let s = stats.snapshot();
    assert_eq!(s.inbound_loss_pct, Some(0.0));
    assert_eq!(s.jitter_ms, Some(0.0));
}
