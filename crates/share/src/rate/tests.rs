use super::*;

const INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 120);

fn at(start: Instant, n: u64) -> Instant {
    start + Duration::from_secs(n)
}

fn bytes(kbps: u32) -> u64 {
    u64::from(kbps) * 1000 / 8
}

// A round trip of `recent_ms` over a floor of 18 on a steady link, where
// the margin is its least, ROUND_TRIP_RISE_MS.
fn rtt(recent_ms: f32) -> Option<RoundTrip> {
    Some(RoundTrip {
        recent_ms,
        floor_ms: 18.0,
        spread_ms: 1.0,
    })
}

// A round trip past the margin but not twice it: a queue beside frames lost
// past the parity, which they count as one only next to (SHARD_FRAMES is
// the other sign).
fn queue_beside() -> Option<RoundTrip> {
    rtt(36.0)
}

// A clean second of a share sending `kbps` of video and parity.
fn sending(kbps: u32) -> Second {
    Second {
        sent: 120,
        lost: 0,
        bytes: bytes(kbps),
        round_trip: rtt(20.0),
        unanswered_ms: None,
        shard_loss: None,
        encode_ms: Some(2.5),
        interval: INTERVAL,
        internet: false,
    }
}

// Seconds of `second` from `from`, one a second, with what each decided.
fn run(rate: &mut Rate, start: Instant, from: u64, seconds: u64, second: Second) -> Vec<Decision> {
    (from..from + seconds)
        .map(|n| rate.second(at(start, n), &second))
        .collect()
}

fn rates(decisions: &[Decision]) -> Vec<Option<u32>> {
    decisions
        .iter()
        .map(|decision| decision.rate_kbps)
        .collect()
}

#[test]
fn loss_near_the_rate_cuts_a_fifth() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let lossy = Second {
        lost: 10,
        round_trip: queue_beside(),
        ..sending(14_000)
    };
    let decisions = run(&mut rate, start, 0, 5, lossy);
    // Seconds 0, 2 and 4 cut; 1 and 3 fall inside the gap.
    assert_eq!(
        rates(&decisions),
        [Some(12_000), None, Some(9_600), None, Some(7_680)]
    );
    assert_eq!(decisions[0].backoff, Some(Sign::Loss));
    assert_eq!(rate.backoffs(), 3);
}

// Five frames in 100 lost past the parity while sending near the rate, with
// the round trip past its margin, is a queue; one frame in a window is not,
// nor is a frame a second, nor one frame of a still screen's few.
#[test]
fn five_in_100_lost_backs_off() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let five = Second {
        lost: 6,
        round_trip: queue_beside(),
        ..sending(14_000)
    };
    let first = rate.second(start, &five);
    assert_eq!(
        (first.rate_kbps, first.backoff),
        (Some(12_000), Some(Sign::Loss))
    );

    let mut rate = Rate::new(15_000);
    for n in 0..60 {
        let lost = u32::from(n % LOSS_SECONDS as u64 == 2);
        let second = Second {
            lost,
            ..sending(14_000)
        };
        assert_eq!(rate.second(at(start, n), &second), Decision::default());
    }
    // A frame every second is 5 in 600, under 4 in 100.
    run(
        &mut rate,
        start,
        60,
        20,
        Second {
            lost: 1,
            ..sending(14_000)
        },
    );
    assert_eq!(rate.rate_kbps(), 15_000);
    assert_eq!(rate.judged().lost, 5);
    // Nor one frame of a still screen's few, a third of them as that is.
    let still = Second {
        sent: 3,
        ..sending(14_000)
    };
    rate.second(at(start, 80), &Second { lost: 1, ..still });
    run(&mut rate, start, 81, 10, still);
    assert_eq!(rate.backoffs(), 0);
}

// A real queue: the share sends at its rate and the round trip climbs past
// the margin and stays. One risen second is not yet a queue, unless it is
// past twice the margin.
#[test]
fn a_risen_round_trip_backs_off() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let queued = |recent_ms| Second {
        round_trip: rtt(recent_ms),
        ..sending(14_000)
    };
    // 22 and 27 ms over the floor: past the margin of 15, not twice it.
    assert_eq!(
        rate.second(at(start, 0), &queued(40.0)),
        Decision::default()
    );
    assert_eq!(
        rate.second(at(start, 1), &queued(20.0)),
        Decision::default()
    );
    assert_eq!(
        rate.second(at(start, 2), &queued(40.0)),
        Decision::default()
    );
    let cut = rate.second(at(start, 3), &queued(45.0));
    assert_eq!(
        (cut.rate_kbps, cut.backoff),
        (Some(12_000), Some(Sign::RoundTrip))
    );
    assert_eq!(rate.judged().risen_seconds, 2);
    // The count starts again at a backoff, so the queue gets as long to
    // drain as a rise takes to count.
    assert_eq!(rate.second(at(start, 4), &queued(45.0)).rate_kbps, None);
    assert_eq!(
        rate.second(at(start, 5), &queued(45.0)).rate_kbps,
        Some(9_600)
    );
    // 42 ms over the floor counts the first second.
    let mut rate = Rate::new(15_000);
    let cut = rate.second(at(start, 0), &queued(60.0));
    assert_eq!(
        (cut.rate_kbps, cut.backoff),
        (Some(12_000), Some(Sign::RoundTrip))
    );
    assert_eq!(rate.judged().risen_seconds, 1);
    assert_eq!(rate.judged().far_seconds, 1);
    // No round trip measured is no sign.
    let mut rate = Rate::new(15_000);
    let unmeasured = Second {
        round_trip: None,
        ..sending(14_000)
    };
    run(&mut rate, start, 0, 10, unmeasured);
    assert_eq!(rate.rate_kbps(), 15_000);
}

// The room measures the round trip on its own clock and the share takes
// each reading once: a second without one neither counts a risen second
// again nor starts the count over.
#[test]
fn no_new_round_trip_keeps_the_count() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let risen = Second {
        round_trip: rtt(40.0),
        ..sending(14_000)
    };
    let nothing_new = Second {
        round_trip: None,
        ..sending(14_000)
    };
    assert_eq!(rate.second(at(start, 0), &risen), Decision::default());
    for n in 1..3 {
        assert_eq!(rate.second(at(start, n), &nothing_new), Decision::default());
        assert_eq!(rate.judged().risen_seconds, 1);
    }
    let cut = rate.second(at(start, 3), &risen);
    assert_eq!(
        (cut.rate_kbps, cut.backoff),
        (Some(12_000), Some(Sign::RoundTrip))
    );
    assert_eq!(rate.judged().risen_seconds, 2);
}

// The room gives the worst of the watchers' links, which can be another
// link from one second to the next. Going from one with a high floor to one
// with a low floor, both risen, is no queue draining.
#[test]
fn a_rise_on_another_watchers_link_is_no_queue_draining() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let watcher = |recent_ms, floor_ms| Second {
        round_trip: Some(RoundTrip {
            recent_ms,
            floor_ms,
            spread_ms: 1.0,
        }),
        ..sending(14_000)
    };
    assert_eq!(
        rate.second(at(start, 0), &watcher(102.0, 80.0)),
        Decision::default()
    );
    let cut = rate.second(at(start, 1), &watcher(44.0, 20.0));
    assert_eq!(cut.backoff, Some(Sign::RoundTrip));
}

#[test]
fn the_round_trip_margin_grows_with_the_links_own_spread() {
    let wifi = |recent_ms| RoundTrip {
        recent_ms,
        floor_ms: 4.0,
        spread_ms: 6.0,
    };
    assert_eq!(wifi(20.0).margin_ms(), 24.0);
    assert!(!wifi(27.0).risen(), "23 over the floor is this link's own");
    assert!(wifi(28.5).risen());
    let steady = RoundTrip {
        recent_ms: 19.5,
        floor_ms: 4.0,
        spread_ms: 0.5,
    };
    assert_eq!(steady.margin_ms(), ROUND_TRIP_RISE_MS, "never under 15");
    assert!(steady.risen());
    // 30 s that were mostly the share's own queue.
    let swamped = RoundTrip {
        spread_ms: 200.0,
        ..wifi(210.0)
    };
    assert_eq!(swamped.margin_ms(), MOST_MARGIN_MS, "never past 50");
    assert!(swamped.risen());

    // So a share at its rate on such a link goes on, where a steady link
    // with the same round trips backs off.
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let jittery = Second {
        round_trip: Some(wifi(26.0)),
        ..sending(14_000)
    };
    run(&mut rate, start, 0, 30, jittery);
    assert_eq!(rate.backoffs(), 0);
    let steady = Second {
        round_trip: Some(RoundTrip {
            spread_ms: 1.0,
            ..wifi(26.0)
        }),
        ..jittery
    };
    run(&mut rate, start, 30, 2, steady);
    assert_eq!(rate.backoffs(), 1);
}

// Below half the rate a share cannot fill a queue unless the link carries
// less than half the rate: loss and delay short of the gate are the link's
// own and never cut for. The room's log says so once when it starts, and
// counts the seconds on its 10 s line. Here 10 frames in 100 lost and a
// round trip 22 ms over the floor, past the margin but not twice it.
#[test]
fn signs_far_under_the_rate_are_let_pass() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let light = Second {
        lost: 12,
        round_trip: rtt(40.0),
        ..sending(7_400)
    };
    let mut lines = Vec::new();
    for n in 0..6 {
        let decision = rate.second(at(start, n), &light);
        assert_eq!(
            (decision.rate_kbps, decision.let_pass),
            (None, Some(Sign::Loss))
        );
        lines.push(rate.line(&decision));
    }
    assert!(
        lines[0]
            .as_deref()
            .is_some_and(|line| line.contains(", not near the rate, loss let pass; sent 7400")),
        "{lines:?}"
    );
    assert!(lines[1..].iter().all(Option::is_none), "{lines:?}");
    assert_eq!(rate.take_let_pass(), 6);
    assert_eq!(rate.take_let_pass(), 0);
    assert!(!rate.judged().near);
    assert_eq!(rate.backoffs(), 0);
    // Coming near the rate, what was lost below it no longer counts: one
    // lost frame now is 1 in 120.
    let near = Second {
        lost: 1,
        round_trip: rtt(20.0),
        ..sending(7_600)
    };
    assert_eq!(rate.second(at(start, 6), &near), Decision::default());
    assert_eq!((rate.judged().lost, rate.judged().frames), (1, 120));
    assert_eq!(rate.judged().sent_kbps, 7_500, "averaged over 2 s");
    assert!(rate.judged().near);

    // A let pass that stops and comes back is said again.
    let mut rate = Rate::new(15_000);
    let queued = Second {
        round_trip: rtt(80.0),
        ..sending(1_000)
    };
    let clear = Second {
        round_trip: rtt(20.0),
        ..sending(1_000)
    };
    let said: Vec<bool> = [queued, queued, clear, queued]
        .iter()
        .zip(0..)
        .map(|(second, n)| {
            let decision = rate.second(at(start, n), second);
            rate.line(&decision).is_some()
        })
        .collect();
    assert_eq!(said, [true, false, false, true]);
}

// While the share sends under half its rate, 20 frames in 100 lost over a
// full loss window and in 2 of its seconds, or a round trip past twice the
// margin for 5 s in a row, backs off all the same, to 0.8 times what was
// sent. Just short of either is let pass, as the link's own.
#[test]
fn the_gate_cuts_from_what_was_sent() {
    let start = Instant::now();
    let light = Second {
        sent: 100,
        round_trip: queue_beside(),
        ..sending(7_200)
    };
    // 19 in 100 is let pass however long it lasts.
    let mut rate = Rate::new(15_000);
    let decisions = run(&mut rate, start, 0, 20, Second { lost: 19, ..light });
    assert!(
        decisions.iter().all(|d| d.let_pass == Some(Sign::Loss)),
        "{decisions:?}"
    );
    assert_eq!((rate.judged().lost, rate.judged().frames), (95, 500));
    // 20 in 100 waits for the window to be full: 100 of 500 on the fifth.
    let mut rate = Rate::new(15_000);
    let decisions = run(&mut rate, start, 0, 5, Second { lost: 20, ..light });
    assert!(
        decisions[..4]
            .iter()
            .all(|d| d.let_pass == Some(Sign::Loss))
    );
    assert_eq!(
        (decisions[4].rate_kbps, decisions[4].backoff),
        (Some(5_760), Some(Sign::HeavyLoss))
    );
    assert_eq!(rate.backoffs(), 1);
    // The log says which rule fired.
    assert_eq!(
        rate.line(&decisions[4]).as_deref(),
        Some(
            "rate 5760 kbit/s of 15000 allowed, backed off for 20 or more frames in 100 lost in 2 s or more of 5 though not near the rate, from what was sent; sent 7200 kbit/s, 48 percent of 15000; 100 of 500 frames lost over 5 s, 5 s of it at 5 frames and 20 in 100 or more; round trip 36.0 ms against a floor of 18.0 and a margin of 15.0, risen 5 s"
        )
    );
    // Heavy seconds need a heavy window too: 25 frames of 100 every other
    // second is 2 or 3 heavy seconds in any 5, and 15 in 100 at most.
    let mut rate = Rate::new(15_000);
    let decisions: Vec<Decision> = (0..20)
        .map(|n| {
            let lost = if n % 2 == 1 { 25 } else { 0 };
            rate.second(at(start, n), &Second { lost, ..light })
        })
        .collect();
    assert!(
        decisions[1..]
            .iter()
            .all(|d| d.let_pass == Some(Sign::Loss)),
        "{decisions:?}"
    );
    let judged = rate.judged();
    assert_eq!(
        (judged.lost, judged.frames, judged.heavy_seconds),
        (75, 500, 3)
    );
    // Under 25 frames a second, a second is heavy on its own only with 5
    // frames lost: 4 of 10 every second never is, and is left to the round
    // trip, which the queue holds past twice the margin. The window alone
    // turned heavy at 7.
    let mut rate = Rate::new(15_000);
    let slow = Second {
        sent: 10,
        ..sending(1_500)
    };
    run(&mut rate, start, 0, 5, slow);
    let queued = Second {
        lost: 4,
        round_trip: rtt(60.0),
        ..slow
    };
    let judged: Vec<(u64, Option<Sign>, bool, usize)> = (5..10)
        .map(|n| {
            let decision = rate.second(at(start, n), &queued);
            let judged = rate.judged();
            (
                n,
                decision.backoff,
                heavy_window(judged),
                judged.heavy_seconds,
            )
        })
        .collect();
    assert_eq!(
        judged,
        [
            (5, None, false, 0),
            (6, None, false, 0),
            (7, None, true, 0),
            (8, None, true, 0),
            (9, Some(Sign::FarRoundTrip), true, 0),
        ]
    );

    // 30 ms over the floor is twice the margin, not past it.
    let mut rate = Rate::new(15_000);
    let twice = Second {
        round_trip: rtt(48.0),
        ..light
    };
    let decisions = run(&mut rate, start, 0, 20, twice);
    assert!(
        decisions
            .iter()
            .skip(1)
            .all(|d| d.let_pass == Some(Sign::RoundTrip)),
        "{decisions:?}"
    );
    assert_eq!(rate.judged().far_seconds, 0);
    // 31 over it for 4 s, a second with no new round trip, which leaves the
    // count as it was, then a fifth.
    let mut rate = Rate::new(15_000);
    let far = Second {
        round_trip: rtt(49.0),
        ..light
    };
    let unmeasured = Second {
        round_trip: None,
        ..light
    };
    let decisions: Vec<Decision> = [far, far, far, far, unmeasured, far]
        .iter()
        .zip(0..)
        .map(|(second, n)| rate.second(at(start, n), second))
        .collect();
    assert!(decisions[..5].iter().all(|d| d.backoff.is_none()));
    assert_eq!(
        (decisions[5].rate_kbps, decisions[5].backoff),
        (Some(5_760), Some(Sign::FarRoundTrip))
    );
    assert_eq!(
        rate.line(&decisions[5]).as_deref(),
        Some(
            "rate 5760 kbit/s of 15000 allowed, backed off for a round trip past 2 times the margin for 5 s though not near the rate, from what was sent; sent 7200 kbit/s, 48 percent of 15000; 0 of 500 frames lost over 5 s; round trip 49.0 ms against a floor of 18.0 and a margin of 15.0, risen 5 s, past 2 times the margin for 5 s"
        )
    );
    // A second back under twice the margin starts the count again.
    let mut rate = Rate::new(15_000);
    let broken = [far, far, far, far, twice, far, far, far, far];
    let decisions: Vec<Decision> = broken
        .iter()
        .zip(0..)
        .map(|(second, n)| rate.second(at(start, n), second))
        .collect();
    assert!(decisions.iter().all(|d| d.backoff.is_none()));
    assert_eq!(rate.judged().far_seconds, 4);

    // Never below the floor: 0.8 of 800 kbit/s sent is 640.
    let mut rate = Rate::new(15_000);
    let scarce = Second {
        lost: 50,
        round_trip: queue_beside(),
        ..sending(800)
    };
    let decisions = run(&mut rate, start, 0, 5, scarce);
    assert_eq!(decisions[4].rate_kbps, Some(RATE_FLOOR_KBPS));
    assert_eq!(decisions[4].backoff, Some(Sign::HeavyLoss));
}

// The heavy loss as it stood before HEAVY_SECONDS: 20 frames in 100 over a
// full window, however its seconds shared them.
fn heavy_window(judged: &Judged) -> bool {
    judged.loss_seconds == LOSS_SECONDS && heavy_loss(judged.frames, judged.lost)
}

// An outage of 1 or 2 s on a lightly moving screen, 60 frames a second at
// 1.2 Mbit/s of 15 as in the first test over the internet: no round trip
// while it lasts, then the first frame after it brings the report of every
// frame it covered, in one second of the window. That is 20 and 40 frames
// in 100 of the window, which through the gate cut the rate to the floor;
// heavy in that one second only, and with no queue beside it, it is no sign
// at all. So on a still screen's 3 frames a second, where the IDR ask after
// the report comes the second after it, one frame, which is a third of that
// second's.
#[test]
fn an_outage_is_let_pass() {
    let start = Instant::now();
    for (sent, kbps, outage, asked) in [(60, 1_200, 1, 0), (60, 1_200, 2, 0), (3, 100, 2, 1)] {
        let still = Second {
            sent,
            ..sending(kbps)
        };
        let dark = Second {
            round_trip: None,
            ..still
        };
        let mut seconds = vec![still; LOSS_SECONDS];
        seconds.extend(std::iter::repeat_n(dark, outage));
        seconds.push(Second {
            lost: sent * outage as u32,
            ..still
        });
        seconds.push(Second {
            lost: asked,
            ..still
        });
        seconds.extend([still; LOSS_SECONDS]);
        let mut rate = Rate::new(15_000);
        let mut heavy_before = 0;
        let mut lines = Vec::new();
        for (second, n) in seconds.iter().zip(0..) {
            let decision = rate.second(at(start, n), second);
            let judged = *rate.judged();
            let case = format!("{outage} s at {sent} frames a second, second {n}: {judged}");
            assert_eq!(decision.backoff, None, "{case}");
            assert_eq!(decision.let_pass, None, "{case}");
            if heavy_window(&judged) {
                heavy_before += 1;
                assert_eq!(judged.heavy_seconds, 1, "{case}");
            }
            lines.extend(rate.line(&decision));
        }
        // The rule before HEAVY_SECONDS backed off in any second the report
        // stayed in the window.
        assert_eq!(heavy_before, LOSS_SECONDS, "{outage} s at {sent}");
        assert_eq!(rate.rate_kbps(), 15_000);
        assert!(lines.is_empty(), "{lines:?}");
    }
}

// What a share allowed 15 whose picture makes 7.2 of video and parity, and
// about its rate once the rate is under that, did over a link of
// `link_kbps` each second, in a simple link model: a router queue of 200 ms,
// what arrives past it dropped, and a frame that loses a packet lost for
// good at three times the bytes' share. The watcher's report comes a second
// later and, as Lost does, says nothing of frames sent before a backoff; the
// round trip is 18 ms plus the queue.
struct OverLink {
    // The second, the sign and the rate after.
    backoffs: Vec<(u64, Sign, u32)>,
    sent_kbps: Vec<f64>,
    lost_frames: Vec<u32>,
    // The first second with a heavy window, where the rule before
    // HEAVY_SECONDS backed off.
    first_heavy_window: Option<u64>,
}

fn over_a_link(seconds: u64, link_kbps: impl Fn(u64) -> f64) -> OverLink {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let mut queued_kbit = 0.0f64;
    let mut reported = 0;
    let mut over_link = OverLink {
        backoffs: Vec::new(),
        sent_kbps: Vec::new(),
        lost_frames: Vec::new(),
        first_heavy_window: None,
    };
    for n in 0..seconds {
        let link = link_kbps(n);
        let kbps = 7_200.0f64.min(f64::from(rate.rate_kbps()) * 1.02);
        queued_kbit = (queued_kbit + kbps - link).max(0.0);
        let over = (queued_kbit - link * 0.2).max(0.0);
        queued_kbit -= over;
        let lost = ((over / kbps * 3.0).min(1.0) * 120.0).ceil() as u32;
        let second = Second {
            lost: reported,
            round_trip: rtt(18.0 + (queued_kbit / link * 1000.0) as f32),
            ..sending(kbps as u32)
        };
        let decision = rate.second(at(start, n), &second);
        if over_link.first_heavy_window.is_none() && heavy_window(rate.judged()) {
            over_link.first_heavy_window = Some(n);
        }
        if let Some(sign) = decision.backoff {
            over_link.backoffs.push((n, sign, rate.rate_kbps()));
        }
        reported = if decision.backoff.is_some() { 0 } else { lost };
        over_link.sent_kbps.push(kbps);
        over_link.lost_frames.push(lost);
    }
    over_link
}

// The hole the gate closes: a link of 5 Mbit/s from the start. Before the
// gate this lost 91 percent of its frames with no backoff. Now the fifth
// second backs off through the gate, from what was sent, and then the rule
// for a share near its rate settles it under the link.
#[test]
fn a_slow_link_from_the_start() {
    const LINK_KBPS: f64 = 5_000.0;
    let OverLink {
        backoffs,
        sent_kbps,
        lost_frames,
        first_heavy_window,
    } = over_a_link(90, |_| LINK_KBPS);
    println!("backoffs: {backoffs:?}");
    assert_eq!(backoffs[0], (4, Sign::HeavyLoss, 5_760));
    assert_eq!(first_heavy_window, Some(4));
    assert!(
        backoffs[1..]
            .iter()
            .all(|(_, sign, _)| !sign.through_gate()),
        "{backoffs:?}"
    );
    // Over the last minute: what was sent averages under the link, now and
    // then a careful step past it and back, and no frame is lost.
    let last = &sent_kbps[30..];
    let mean = last.iter().sum::<f64>() / last.len() as f64;
    assert!(mean < LINK_KBPS, "{mean:.0} kbit/s sent: {last:?}");
    assert!(mean > LINK_KBPS * 0.8, "{mean:.0} kbit/s sent: {last:?}");
    assert_eq!(lost_frames[30..].iter().sum::<u32>(), 0, "{lost_frames:?}");
    let lost: u32 = lost_frames.iter().sum();
    println!(
        "{lost} of {} frames lost, {:.1} percent; mean sent over the last minute {mean:.0} kbit/s",
        90 * 120,
        f64::from(lost) * 100.0 / (90.0 * 120.0)
    );
}

// A queue loses frames every second it stands, so it backs off at its
// second heavy second. Here the link falls ten seconds into the share, as
// when someone else in the house starts an upload. To 5 Mbit/s, the first
// report, a second later, names half of that second's frames, 10 in 100 of
// the window, and the next takes it past 20, heavy in both seconds: where
// the window first turns heavy, as before HEAVY_SECONDS. To 4, the first
// report names every frame of that second, 20 in 100 of the window by
// itself, which the rule before HEAVY_SECONDS backed off for at once; heavy
// in one second only, as an outage's report is, it waits for the next.
#[test]
fn a_queue_mid_share() {
    for (link_kbps, first_heavy_window, backoff_at) in [(5_000.0, 12, 12), (4_000.0, 11, 12)] {
        let over_link = over_a_link(30, |n| if n < 10 { 20_000.0 } else { link_kbps });
        assert_eq!(
            over_link.first_heavy_window,
            Some(first_heavy_window),
            "{link_kbps}"
        );
        assert_eq!(
            over_link.backoffs[0],
            (backoff_at, Sign::HeavyLoss, 5_760),
            "{link_kbps}: {:?}",
            over_link.backoffs
        );
    }
}

// One second of the first share over the internet, on 2026-09-29: frames
// sent, frames reported lost, the median round trip, and upload in kbit/s,
// which is video with its parity plus voice and control, so more than the
// rule counts.
type Logged = (u32, u32, f32, u32);

// Test 1, share 4 at 15 Mbit/s, host log 10:06:15 to 10:07:02, from the
// second someone watched (3) to the last rate line (50). Where the log has
// a second (its rate lines at 6, 9, 16, 18, 21, 26, 32, 36, 40, 42, 47 and
// 50, and the 10 s lines), its numbers; between them, frames and upload
// drawn straight between the 10 s lines, the round trip at a value the old
// rule did not cut for, and the 15 recover requests where the 10 s lines'
// counts put them. The floor was 5.2 ms, then 3.6.
const TEST_1: [Logged; 48] = [
    (15, 0, 17.0, 2000),
    (58, 0, 17.0, 1300),
    (60, 1, 17.0, 1250),
    (62, 0, 23.2, 1250),
    (60, 0, 17.0, 1220),
    (61, 2, 17.0, 1220),
    (63, 1, 14.4, 1210),
    (60, 0, 17.0, 1207),
    (60, 0, 17.0, 1220),
    (61, 1, 17.0, 1240),
    (60, 0, 17.0, 1260),
    (59, 0, 16.5, 1280),
    (60, 1, 16.5, 1300),
    (60, 0, 24.2, 1320),
    (52, 0, 16.5, 1340),
    (46, 1, 21.2, 1350),
    (48, 0, 16.5, 1360),
    (49, 0, 16.5, 1372),
    (55, 0, 20.6, 1340),
    (54, 0, 16.5, 1310),
    (53, 0, 16.5, 1280),
    (52, 1, 16.5, 1250),
    (51, 0, 16.5, 1220),
    (50, 1, 21.6, 1190),
    (51, 0, 16.5, 1160),
    (52, 0, 16.5, 1130),
    (52, 1, 16.5, 1110),
    (53, 0, 16.5, 1094),
    (53, 0, 16.5, 1080),
    (53, 0, 27.2, 1070),
    (53, 0, 16.5, 1060),
    (52, 1, 16.5, 1050),
    (52, 0, 16.5, 1040),
    (52, 1, 23.3, 1030),
    (52, 0, 16.5, 1020),
    (53, 0, 16.5, 1010),
    (53, 0, 16.5, 1000),
    (53, 0, 21.4, 991),
    (53, 0, 16.5, 985),
    (53, 0, 19.3, 980),
    (54, 0, 16.5, 975),
    (54, 1, 16.5, 970),
    (54, 0, 16.5, 960),
    (54, 0, 16.5, 950),
    (54, 1, 23.2, 945),
    (55, 0, 16.5, 940),
    (55, 1, 16.5, 935),
    (56, 0, 19.1, 926),
];

// Test 2, share 1 at 30 Mbit/s, host log 10:07:49 to 10:08:38, from the
// second someone watched (4) to the last rate line (53), filled the same
// way: rate lines at 9, 11, 16, 21, 26, 31, 36, 43, 48 and 53; 31
// invalidations and one recover request covered already. Busy from about
// 15 to 37, with up to 14 Mbit/s of video. The floor was 5.3 ms, then 4.2,
// then 3.7.
const TEST_2: [Logged; 50] = [
    (10, 0, 11.0, 2500),
    (70, 1, 11.0, 2400),
    (69, 0, 11.0, 2250),
    (68, 1, 11.0, 2250),
    (70, 1, 11.0, 2250),
    (70, 3, 10.8, 2230),
    (68, 2, 11.0, 2213),
    (60, 1, 10.2, 2200),
    (66, 0, 11.0, 2400),
    (72, 1, 11.0, 3000),
    (78, 0, 11.5, 4000),
    (83, 0, 12.0, 5500),
    (87, 1, 10.3, 7000),
    (92, 1, 12.5, 8500),
    (96, 0, 13.0, 10_000),
    (100, 1, 13.5, 11_000),
    (104, 1, 14.0, 12_157),
    (119, 1, 15.3, 13_500),
    (112, 0, 14.0, 14_500),
    (110, 1, 14.0, 15_500),
    (112, 0, 14.0, 16_500),
    (108, 1, 14.0, 17_000),
    (100, 0, 15.1, 17_500),
    (110, 1, 14.0, 18_000),
    (112, 1, 14.0, 18_500),
    (114, 0, 14.0, 18_700),
    (115, 1, 14.0, 18_925),
    (110, 0, 13.0, 18_800),
    (108, 1, 13.5, 18_500),
    (105, 0, 13.5, 17_500),
    (100, 1, 13.5, 16_000),
    (95, 0, 13.5, 14_000),
    (88, 0, 13.5, 11_000),
    (80, 1, 12.5, 7000),
    (75, 1, 12.0, 4000),
    (72, 0, 11.5, 2600),
    (71, 1, 11.5, 2423),
    (70, 1, 12.0, 2420),
    (69, 0, 12.0, 2420),
    (68, 0, 20.2, 2415),
    (68, 1, 12.0, 2415),
    (69, 0, 12.0, 2410),
    (69, 0, 12.0, 2410),
    (68, 2, 12.0, 2410),
    (68, 1, 13.4, 2410),
    (70, 1, 12.0, 2410),
    (71, 0, 12.0, 2409),
    (73, 0, 12.0, 2400),
    (75, 1, 12.0, 2400),
    (77, 0, 13.8, 2400),
];

// The log has no spread of the round trips; the replays take one that
// leaves the margin at its least, 15 ms, as the old rule had it.
fn replay(
    allowed: u32,
    logged: &[Logged],
    first: u64,
    floor_ms: impl Fn(u64) -> f32,
    at_the_rate: bool,
) -> (Rate, Vec<Decision>) {
    let start = Instant::now();
    let mut rate = Rate::new(allowed);
    let decisions = logged
        .iter()
        .zip(first..)
        .map(|(&(sent, lost, recent_ms, upload), n)| {
            let second = Second {
                sent,
                lost,
                bytes: bytes(if at_the_rate { allowed } else { upload }),
                round_trip: Some(RoundTrip {
                    recent_ms,
                    floor_ms: floor_ms(n),
                    spread_ms: 2.0,
                }),
                unanswered_ms: None,
                shard_loss: None,
                encode_ms: Some(3.0),
                interval: INTERVAL,
                internet: true,
            };
            rate.second(at(start, n), &second)
        })
        .collect();
    (rate, decisions)
}

fn test_1_floor(n: u64) -> f32 {
    if n < 14 { 5.2 } else { 3.6 }
}

fn test_2_floor(n: u64) -> f32 {
    match n {
        ..25 => 5.3,
        25..41 => 4.2,
        _ => 3.7,
    }
}

#[test]
fn first_internet_shares_replayed() {
    for (name, allowed, logged, first, floor) in [
        (
            "test 1",
            15_000,
            &TEST_1[..],
            3,
            test_1_floor as fn(u64) -> f32,
        ),
        ("test 2", 30_000, &TEST_2[..], 4, test_2_floor),
    ] {
        let (rate, decisions) = replay(allowed, logged, first, floor, false);
        let changed: Vec<&Decision> = decisions
            .iter()
            .filter(|d| d.rate_kbps.is_some() || d.step.is_some())
            .collect();
        assert!(changed.is_empty(), "{name}: {changed:?}");
        assert_eq!((rate.rate_kbps(), rate.backoffs()), (allowed, 0), "{name}");
        // Even sending at the rate every second, what the logs lost is no
        // queue: the loss window holds on its own. So do test 2's round
        // trips, whose busy seconds the old rule climbed through. Test 1's
        // say less: only its logged seconds are the log's, and the filler
        // between them sits under the old rule's threshold, so a risen round
        // trip lasting 2 s cannot happen there. Whether a busy share keeps
        // its rate on that Wi-Fi is for the next test over it to show.
        let (rate, decisions) = replay(allowed, logged, first, floor, true);
        assert!(
            decisions.iter().all(|d| *d == Decision::default()),
            "{name} at the rate"
        );
        assert_eq!(rate.backoffs(), 0, "{name} at the rate");
    }
    // Test 2 was busy enough to count as near the rate from its 23rd
    // second, where 16 Mbit/s of 30 went out, and the rules ran then.
    let (_, decisions) = replay(30_000, &TEST_2[..20], 4, test_2_floor, false);
    assert!(decisions.iter().all(|d| d.let_pass.is_none()));
    let (rate, _) = replay(30_000, &TEST_2[..21], 4, test_2_floor, false);
    assert!(rate.judged().near, "{}", rate.judged());
}

// A link that carries 5 Mbit/s under a share allowed 15: the round trip
// rises and stays while the rate is past 5, then the link is freed at 10 s.
// The rate comes down to under 5, steps down to 1080p60 on the way, and
// once the queue is gone climbs back carefully to past the rate that last
// queued, then fast, and steps back up.
#[test]
fn climbs_back_after_a_queue() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let mut changes = Vec::new();
    for n in 0..50 {
        let queued = n < 10 && rate.rate_kbps() > 5_000;
        let second = Second {
            round_trip: rtt(if queued { 90.0 } else { 21.0 }),
            internet: true,
            ..sending(rate.rate_kbps() * 95 / 100)
        };
        let decision = rate.second(at(start, n), &second);
        if decision.rate_kbps.is_some() || decision.step.is_some() {
            changes.push((n, decision.rate_kbps, decision.step));
        }
    }
    let down = Some(Step::Down(SteppedDown::LowRate));
    assert_eq!(
        changes,
        [
            // 70 ms over the floor counts at once.
            (0, Some(12_000), None),
            (2, Some(9_600), None),
            (4, Some(7_680), down),
            (6, Some(6_144), None),
            (8, Some(4_915), None),
            // The queue is gone from 9. Careful: 10 percent with 5 clean
            // seconds before each step, to a step past 6144, the rate that
            // last queued.
            (13, Some(5_407), None),
            (18, Some(5_948), None),
            (23, Some(6_543), None),
            (28, Some(7_197), None),
            // Past it and clean for 2 s: fast. Back to full size once clean
            // for 5 s, 24 s after the queue went, and to the rate allowed
            // 26 s after.
            (30, Some(8_277), None),
            (31, Some(9_519), None),
            (32, Some(10_947), None),
            (33, Some(12_589), Some(Step::Up)),
            (34, Some(14_477), None),
            (35, Some(15_000), None),
        ]
    );
    assert_eq!(rate.backoffs(), 5);
    assert_eq!(rate.small(), None);
}

// One backoff for loss, and nothing queues on the careful way back: 10
// percent a step, each after 5 clean seconds, to the rate allowed.
#[test]
fn careful_climb_after_one_backoff() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    // 30 frames: the next time, over 5 s of 120 frames a second, too.
    let lossy = Second {
        lost: 30,
        round_trip: queue_beside(),
        ..sending(14_000)
    };
    run(&mut rate, start, 0, 1, lossy);
    assert_eq!(rate.rate_kbps(), 12_000);
    let climbed: Vec<(u64, u32)> = run(&mut rate, start, 1, 19, sending(14_000))
        .iter()
        .zip(1..)
        .filter_map(|(decision, n)| decision.rate_kbps.map(|kbps| (n, kbps)))
        .collect();
    assert_eq!(climbed, [(5, 13_200), (10, 14_520), (15, 15_000)]);
    // A sign starts the count again, and so does a round trip risen for
    // less than it takes to count.
    run(&mut rate, start, 20, 1, lossy);
    assert_eq!(rate.rate_kbps(), 12_000);
    run(&mut rate, start, 21, 1, sending(14_000));
    run(
        &mut rate,
        start,
        22,
        1,
        Second {
            round_trip: rtt(40.0),
            ..sending(14_000)
        },
    );
    let after: Vec<Option<u32>> = rates(&run(&mut rate, start, 23, 6, sending(14_000)));
    assert_eq!(after, [None, None, None, None, Some(13_200), None]);
}

// While the share sends little the rate cannot fill anything, so a rate
// that had come down climbs back whatever the link does, but only to the
// rate the last backoff came at: past that, a rate the share does not use
// is one the link has not shown it carries. Busy again, it climbs on.
#[test]
fn climbs_while_signs_are_let_pass() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let lossy = Second {
        lost: 20,
        round_trip: queue_beside(),
        ..sending(14_000)
    };
    run(&mut rate, start, 0, 1, lossy);
    run(&mut rate, start, 2, 1, lossy);
    assert_eq!(rate.rate_kbps(), 9_600);
    // Second 3 still averages the busy second before it, so the let pass
    // starts at 4, and its seconds count as clean.
    let still = Second {
        lost: 12,
        round_trip: rtt(40.0),
        ..sending(1_500)
    };
    let climbed: Vec<(u64, u32)> = run(&mut rate, start, 3, 22, still)
        .iter()
        .zip(3..)
        .filter_map(|(decision, n)| decision.rate_kbps.map(|kbps| (n, kbps)))
        .collect();
    assert_eq!(climbed, [(8, 10_560), (13, 11_616), (18, 12_000)]);
    assert_eq!(rate.backoffs(), 2);
    let busy: Vec<(u64, u32)> = run(&mut rate, start, 25, 10, sending(12_000))
        .iter()
        .zip(25..)
        .filter_map(|(decision, n)| decision.rate_kbps.map(|kbps| (n, kbps)))
        .collect();
    assert_eq!(busy, [(25, 13_200), (30, 14_520), (32, 15_000)]);
}

#[test]
fn the_backoff_floor() {
    let start = Instant::now();
    let lossy = Second {
        lost: 50,
        round_trip: queue_beside(),
        ..sending(15_000)
    };
    let mut rate = Rate::new(15_000);
    run(&mut rate, start, 0, 60, lossy);
    assert_eq!(rate.rate_kbps(), RATE_FLOOR_KBPS);
    let mut rate = Rate::new(800);
    run(&mut rate, start, 0, 10, lossy);
    assert_eq!(rate.rate_kbps(), 800);
    assert_eq!(rate.backoffs(), 0);
}

// The host's bitrate rule arrives as the rate allowed. A lower one applies
// at once; a higher one at once too unless the rate backed off.
#[test]
fn the_rate_allowed_follows_watchers_coming_and_going() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    assert_eq!(rate.allow(7_500), Some(7_500));
    assert_eq!(rate.allow(15_000), Some(15_000));
    assert_eq!(rate.allow(15_000), None);
    rate.second(
        start,
        &Second {
            lost: 10,
            round_trip: queue_beside(),
            ..sending(14_000)
        },
    );
    assert_eq!(rate.rate_kbps(), 12_000);
    assert_eq!(rate.allow(20_000), None, "backed off: the climb gets there");
    assert_eq!(rate.allow(10_000), Some(10_000));
    assert_eq!(rate.allowed_kbps(), 10_000);

    // Stepped down after a backoff, as two friends watching over the
    // internet halve the rate allowed. One leaves: back at the rate
    // allowed, there is no careful climb to wait for.
    let busy = Second {
        internet: true,
        ..sending(14_000)
    };
    let mut rate = Rate::new(15_000);
    rate.second(
        at(start, 0),
        &Second {
            lost: 10,
            round_trip: queue_beside(),
            ..busy
        },
    );
    assert_eq!(rate.rate_kbps(), 12_000);
    assert_eq!(rate.allow(7_000), Some(7_000));
    let down = rate.second(at(start, 1), &busy);
    assert_eq!(down.step, Some(Step::Down(SteppedDown::LowRate)));
    assert_eq!(rate.allow(15_000), Some(15_000));
    run(&mut rate, start, 2, 9, busy);
    assert_eq!(rate.second(at(start, 11), &busy).step, Some(Step::Up));
}

#[test]
fn steps_down_under_8_and_up_at_10() {
    let start = Instant::now();
    let internet = Second {
        internet: true,
        ..sending(1_000)
    };
    // Two friends over the internet halve a 15 Mbit/s upload.
    let mut rate = Rate::new(7_500);
    let first = rate.second(at(start, 0), &internet);
    assert_eq!(first.step, Some(Step::Down(SteppedDown::LowRate)));
    assert_eq!(rate.small(), Some(SteppedDown::LowRate));
    assert_eq!(rate.second(at(start, 1), &internet).step, None);
    // One leaves: 15 Mbit/s again, but not within 10 s of the last step.
    rate.allow(15_000);
    assert_eq!(rate.second(at(start, 5), &internet).step, None);
    run(&mut rate, start, 6, 4, internet);
    assert_eq!(rate.second(at(start, 10), &internet).step, Some(Step::Up));
    assert_eq!(rate.small(), None);

    // A rate allowed between 8 and 10 never steps down.
    let mut rate = Rate::new(9_000);
    for n in 0..30 {
        assert_eq!(
            rate.second(at(start, n), &internet).step,
            None,
            "second {n}"
        );
    }
    // Small for a rate allowed under 8, it stays small while that is under
    // 8, and steps up once it is 8 or more.
    let mut rate = Rate::new(7_900);
    rate.second(at(start, 0), &internet);
    rate.allow(7_950);
    for n in 1..30 {
        assert_eq!(
            rate.second(at(start, n), &internet).step,
            None,
            "second {n}"
        );
    }
    rate.allow(9_900);
    assert_eq!(rate.second(at(start, 30), &internet).step, Some(Step::Up));
    // The last internet watcher leaves: LAN viewers do not count, so it
    // steps back up whatever the rate.
    let mut rate = Rate::new(7_000);
    rate.second(at(start, 0), &internet);
    let lan = sending(1_000);
    assert_eq!(rate.second(at(start, 9), &lan).step, None, "10 s apart");
    assert_eq!(rate.second(at(start, 10), &lan).step, Some(Step::Up));

    // LAN viewers never step it down, whatever the rate.
    let mut rate = Rate::new(2_000);
    for n in 0..20 {
        assert_eq!(rate.second(at(start, n), &lan).step, None);
    }
}

// The old case where the rate never got back to 10: an upload setting of
// 18 Mbit/s and two watchers over the internet allow 9 each, which never
// steps down. A lossy second cuts it to 7.2, which does. The careful climb
// reaches 9 in three steps 5 s apart, and 5 clean seconds at it bring the
// share back to full size, 10 s and more after the step down.
#[test]
fn steps_up_with_allowed_under_10() {
    let start = Instant::now();
    let internet = Second {
        internet: true,
        ..sending(8_500)
    };
    let lossy = Second {
        lost: 10,
        round_trip: queue_beside(),
        ..internet
    };
    let mut rate = Rate::new(9_000);
    assert_eq!(rate.second(at(start, 0), &internet).step, None);
    let down = rate.second(at(start, 1), &lossy);
    assert_eq!(
        (down.rate_kbps, down.step),
        (Some(7_200), Some(Step::Down(SteppedDown::LowRate)))
    );
    let mut changes = Vec::new();
    for n in 2..40 {
        let decision = rate.second(at(start, n), &internet);
        if decision != Decision::default() {
            changes.push((n, decision.rate_kbps, decision.step));
        }
    }
    assert_eq!(
        changes,
        [
            (6, Some(7_920), None),
            (11, Some(8_712), None),
            (16, Some(9_000), None),
            (21, None, Some(Step::Up))
        ]
    );
}

// A link that carries about 9.5 Mbit/s under a share allowed 15: the rate
// queues just past 9.5 and backs off under 8, which steps down. Stepping
// back up at 10 while the climb is still careful would put it straight
// back into the queue, backing off under 8 again, so it stays small.
#[test]
fn a_rate_that_keeps_queuing_near_10_stays_stepped_down() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let mut steps = Vec::new();
    for n in 0..120 {
        let queued = rate.rate_kbps() > 9_500;
        let second = Second {
            round_trip: rtt(if queued { 60.0 } else { 20.0 }),
            internet: true,
            ..sending(rate.rate_kbps())
        };
        if let Some(step) = rate.second(at(start, n), &second).step {
            steps.push((n, step));
        }
    }
    assert_eq!(steps, [(4, Step::Down(SteppedDown::LowRate))]);
    assert!(rate.backoffs() > 5, "{} backoffs", rate.backoffs());
}

#[test]
fn a_slow_encoder_steps_down_for_good() {
    let start = Instant::now();
    let slow = Second {
        encode_ms: Some(9.0),
        ..sending(14_000)
    };
    let mut rate = Rate::new(15_000);
    assert_eq!(rate.second(at(start, 0), &slow).step, None);
    assert_eq!(
        rate.second(at(start, 1), &sending(14_000)).step,
        None,
        "one fast second resets it"
    );
    assert_eq!(rate.second(at(start, 2), &slow).step, None);
    assert_eq!(rate.second(at(start, 3), &slow).step, None);
    assert_eq!(
        rate.second(at(start, 4), &slow).step,
        Some(Step::Down(SteppedDown::SlowEncode))
    );
    // At 60 fps the interval is 16.7 ms, and nothing brings it back up.
    let sixty = Second {
        interval: Duration::from_nanos(1_000_000_000 / 60),
        internet: true,
        encode_ms: Some(9.0),
        ..sending(14_000)
    };
    for n in 5..100 {
        assert_eq!(rate.second(at(start, n), &sixty).step, None);
    }
    assert_eq!(rate.small(), Some(SteppedDown::SlowEncode));

    // A share small for its rate that then encodes too slowly stays small
    // when the rate comes back.
    let mut rate = Rate::new(7_000);
    let internet = Second {
        internet: true,
        ..sending(6_000)
    };
    rate.second(at(start, 0), &internet);
    let slow_sixty = Second {
        encode_ms: Some(20.0),
        ..sixty
    };
    for n in 1..4 {
        rate.second(at(start, n), &slow_sixty);
    }
    assert_eq!(rate.small(), Some(SteppedDown::SlowEncode));
    rate.allow(15_000);
    for n in 4..40 {
        assert_eq!(rate.second(at(start, n), &internet).step, None);
    }
}

#[test]
fn round_trip_quartile_floor_and_spread() {
    let start = Instant::now();
    let ms = |n: u64| Duration::from_millis(n);
    let mut pings = VecDeque::new();
    // 40 s of pings ten a second: 10 ms, with every fifth at 30, and one
    // of 2 ms 35 s ago, which is past the 30 s.
    for n in 0..400u64 {
        let rtt = if n == 50 {
            ms(2)
        } else if n % 5 == 0 {
            ms(30)
        } else {
            ms(10)
        };
        pings.push_back((start + ms(n * 100), rtt));
    }
    let now = start + ms(39_950);
    let rtt = round_trip(&pings, now).expect("pings in the last second");
    assert_eq!(
        (rtt.recent_ms, rtt.floor_ms, rtt.spread_ms),
        (10.0, 10.0, 0.0)
    );
    // A second of 40 ms pings: the quartile rises, and the floor and spread
    // over 30 s do not see it.
    for n in 400..410u64 {
        pings.push_back((start + ms(n * 100), ms(40)));
    }
    let rtt = round_trip(&pings, start + ms(40_950)).expect("pings");
    assert_eq!((rtt.recent_ms, rtt.floor_ms), (40.0, 10.0));
    assert_eq!(rtt.spread_ms, 0.0);
    assert!(rtt.risen());
    // Nothing answered in the last second.
    assert_eq!(round_trip(&pings, start + ms(42_000)), None);
    assert_eq!(round_trip(&VecDeque::new(), now), None);

    // A jittery link, every round trip from 4 to 43 ms in turn: a quarter
    // of them reach 13 ms, 9 over the floor, and the margin is 36.
    let jittery: VecDeque<(Instant, Duration)> = (0..300u64)
        .map(|n| (start + ms(n * 100), ms(4 + n % 40)))
        .collect();
    let rtt = round_trip(&jittery, start + ms(29_950)).expect("pings");
    assert_eq!(
        (rtt.recent_ms, rtt.floor_ms, rtt.spread_ms),
        (16.0, 4.0, 9.0)
    );
    assert_eq!(rtt.margin_ms(), 36.0);
    // Half the 30 s spent in a 200 ms queue of the share's own leaves the
    // spread as the link's own: those round trips are past the floor and
    // MOST_MARGIN_MS, and not counted in it.
    let queued: VecDeque<(Instant, Duration)> = jittery
        .iter()
        .map(|&(at, rtt)| {
            (
                at,
                if at >= start + ms(15_000) {
                    rtt + ms(200)
                } else {
                    rtt
                },
            )
        })
        .collect();
    let rtt = round_trip(&queued, start + ms(29_950)).expect("pings");
    assert_eq!((rtt.floor_ms, rtt.spread_ms), (4.0, 9.0));
    assert!(rtt.risen());
}

// A cut that was enough leaves a full router's queue to drain for a second
// or two. Its round trip falls fast, which is no reason to cut again, and no
// time to climb either.
#[test]
fn a_queue_draining_after_a_cut_is_not_cut_for_again() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let second = |recent_ms| Second {
        round_trip: rtt(recent_ms),
        ..sending(14_000)
    };
    let decisions: Vec<Option<u32>> = [210.0, 210.0, 150.0, 80.0, 30.0, 20.0, 21.0, 20.0, 20.0]
        .into_iter()
        .zip(0..)
        .map(|(recent_ms, n)| rate.second(at(start, n), &second(recent_ms)).rate_kbps)
        .collect();
    assert_eq!(
        decisions,
        [
            Some(12_000),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(13_200)
        ]
    );
    assert_eq!(rate.backoffs(), 1);
    // A queue that stands after the cut is cut for again.
    let mut rate = Rate::new(15_000);
    let standing: Vec<Option<u32>> = [210.0, 210.0, 205.0, 209.0]
        .into_iter()
        .zip(0..)
        .map(|(recent_ms, n)| rate.second(at(start, n), &second(recent_ms)).rate_kbps)
        .collect();
    assert_eq!(standing, [Some(12_000), None, Some(9_600), None]);
}

// The second ticks when the share's thread gets to it, now and then a
// little early against the one it is measured from.
#[test]
fn an_early_tick_still_counts() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let lossy = Second {
        lost: 20,
        round_trip: queue_beside(),
        ..sending(14_000)
    };
    let late = start + Duration::from_millis(12);
    assert_eq!(rate.second(late, &lossy).rate_kbps, Some(12_000));
    let tick = start + Duration::from_millis(1_004);
    assert_eq!(rate.second(tick, &lossy).rate_kbps, None);
    let early = start + Duration::from_millis(2_003);
    assert_eq!(rate.second(early, &lossy).rate_kbps, Some(9_600));
}

#[test]
fn a_viewers_answers_count_the_frames_they_say_were_lost() {
    let heard = |lost: &mut Lost, back: Back| {
        lost.heard(&back);
        lost.take()
    };
    let mut lost = Lost::default();
    assert_eq!(heard(&mut lost, Back::Recover { first: 7, last: 9 }), 3);
    assert_eq!(heard(&mut lost, Back::Recover { first: 5, last: 5 }), 1);
    // Frame numbers wrap.
    let wrapped = Back::Recover {
        first: u32::MAX,
        last: 1,
    };
    assert_eq!(heard(&mut lost, wrapped), 3);
    let outage = Back::Recover {
        first: 0,
        last: 100_000,
    };
    assert_eq!(heard(&mut lost, outage), MOST_LOST_IN_ONE);
    assert_eq!(heard(&mut lost, Back::Idr { seen: 12 }), 1);
    assert_eq!(heard(&mut lost, Back::Loss(Some(2.5))), 0);
    lost.heard(&Back::Idr { seen: 13 });
    lost.heard(&Back::Recover { first: 3, last: 4 });
    assert_eq!(lost.take(), 3);
    assert_eq!(lost.take(), 0);
}

// Frames 0 to 99 went out at the rate before the cut, 100 on at the one
// after: reports of the first kind still coming in say nothing of it.
#[test]
fn after_a_backoff_only_frames_sent_after_it_count() {
    let mut lost = Lost::default();
    for number in 0..100 {
        lost.sent(number);
    }
    lost.backed_off();
    lost.heard(&Back::Recover {
        first: 90,
        last: 95,
    });
    lost.heard(&Back::Idr { seen: 99 });
    assert_eq!(lost.take(), 0);
    lost.heard(&Back::Recover {
        first: 97,
        last: 102,
    });
    lost.heard(&Back::Idr { seen: 100 });
    assert_eq!(lost.take(), 3 + 1);
    // Across the wrap of frame numbers too.
    let mut lost = Lost::default();
    lost.sent(u32::MAX - 1);
    lost.backed_off();
    lost.heard(&Back::Recover {
        first: u32::MAX - 3,
        last: 2,
    });
    assert_eq!(lost.take(), 4, "u32::MAX, 0, 1 and 2");
    // A backoff before anything was sent counts everything.
    let mut lost = Lost::default();
    lost.backed_off();
    lost.heard(&Back::Recover { first: 0, last: 1 });
    assert_eq!(lost.take(), 2);
}

// What the room and the loopback write in their logs.
#[test]
fn what_a_second_was_judged_on_reads_as_one_clause() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let second = Second {
        lost: 2,
        round_trip: Some(RoundTrip {
            recent_ms: 85.12,
            floor_ms: 4.2,
            spread_ms: 4.9,
        }),
        ..sending(14_200)
    };
    let decision = rate.second(start, &second);
    assert_eq!(
        rate.judged().to_string(),
        "sent 14200 kbit/s, 94 percent of 15000; 2 of 120 frames lost over 1 s; round trip 85.1 ms against a floor of 4.2 and a margin of 19.6, risen 1 s, past 2 times the margin"
    );
    assert_eq!(
        rate.line(&decision).as_deref(),
        Some(
            "rate 12000 kbit/s of 15000 allowed, backed off for a risen round trip; sent 14200 kbit/s, 94 percent of 15000; 2 of 120 frames lost over 1 s; round trip 85.1 ms against a floor of 4.2 and a margin of 19.6, risen 1 s, past 2 times the margin"
        )
    );
    let decision = rate.second(
        at(start, 1),
        &Second {
            round_trip: None,
            ..sending(14_200)
        },
    );
    assert!(
        rate.judged().to_string().ends_with("no new round trip"),
        "{}",
        rate.judged()
    );
    assert_eq!(rate.line(&decision), None);
    assert!(
        rate.describe(&decision)
            .starts_with("rate 12000 kbit/s of 15000 allowed; sent")
    );
}

// Shard loss past what a radio loses on its own, with nothing reported
// lost: half the shards whatever the round trip does, a quarter beside a
// round trip past half its margin, or a quarter in five reports in a row.
// The cut is to 0.8 of what arrived, not a fifth off the rate.
#[test]
fn shards_lost_to_a_queue_cut_to_what_arrived() {
    let start = Instant::now();
    let lost = |percent, recent_ms| Second {
        shard_loss: Some(percent),
        round_trip: rtt(recent_ms),
        ..sending(14_000)
    };
    let mut rate = Rate::new(15_000);
    let cut = rate.second(start, &lost(60.0, 20.0));
    // 40 percent of 14000 arrived.
    assert_eq!(
        (cut.rate_kbps, cut.backoff),
        (Some(4_480), Some(Sign::ShardLoss))
    );
    assert!(
        rate.line(&cut).is_some_and(|line| line
            .contains("backed off for shards lost to a queue, to what arrived")
            && line.ends_with("; 60.0 percent of shards lost")),
        "{:?}",
        rate.line(&cut)
    );

    let mut rate = Rate::new(15_000);
    let cut = rate.second(start, &lost(30.0, 28.0));
    assert_eq!(cut.backoff, Some(Sign::ShardLoss));

    let mut rate = Rate::new(15_000);
    let decisions = run(&mut rate, start, 0, 5, lost(30.0, 20.0));
    assert!(decisions[..4].iter().all(|d| *d == Decision::default()));
    assert_eq!(
        (decisions[4].rate_kbps, decisions[4].backoff),
        (Some(7_840), Some(Sign::ShardLoss))
    );
    // Radio loss that comes and goes never makes five in a row.
    let mut rate = Rate::new(15_000);
    for n in 0..30 {
        let percent = if n % 4 == 3 { 10.0 } else { 30.0 };
        assert_eq!(
            rate.second(at(start, n), &lost(percent, 20.0)),
            Decision::default()
        );
    }
}

// While the queue drains after a cut, the watchers' reports still describe
// the rate before it.
#[test]
fn shards_lost_while_the_queue_drains_are_no_sign() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let second = |recent_ms| Second {
        shard_loss: Some(60.0),
        round_trip: rtt(recent_ms),
        ..sending(14_000)
    };
    assert_eq!(
        rate.second(at(start, 0), &second(200.0)).backoff,
        Some(Sign::ShardLoss)
    );
    rate.second(at(start, 1), &second(200.0));
    assert_eq!(rate.second(at(start, 2), &second(100.0)).backoff, None);
    assert_eq!(rate.backoffs(), 1);
}

// Frames lost past the parity count beside shard loss of SHARD_FRAMES too,
// not only beside a risen round trip.
#[test]
fn frames_lost_beside_shards_lost_back_off() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    let second = |shard_loss| Second {
        lost: 10,
        shard_loss,
        ..sending(14_000)
    };
    assert_eq!(rate.second(start, &second(Some(39.0))), Decision::default());
    let cut = rate.second(at(start, 1), &second(Some(40.0)));
    assert_eq!(
        (cut.rate_kbps, cut.backoff),
        (Some(12_000), Some(Sign::Loss))
    );
}

// No pong within the second after a round trip past the margin, with a ping
// waiting a second or more: the queue grew past the second's pings, and
// that counts as past twice the margin. A missing reading on its own, or
// one after a calm second, or with nothing waiting that long, leaves the
// counts as they were.
#[test]
fn a_ping_unanswered_after_a_rise_counts_as_far() {
    let start = Instant::now();
    let gone = |unanswered_ms| Second {
        round_trip: None,
        unanswered_ms,
        ..sending(14_000)
    };
    let risen = Second {
        round_trip: rtt(40.0),
        ..sending(14_000)
    };
    let mut rate = Rate::new(15_000);
    assert_eq!(rate.second(at(start, 0), &risen), Decision::default());
    let cut = rate.second(at(start, 1), &gone(Some(1_500.0)));
    assert_eq!(
        (cut.rate_kbps, cut.backoff),
        (Some(12_000), Some(Sign::RoundTrip))
    );
    assert!(
        rate.judged()
            .to_string()
            .ends_with("no new round trip, a ping unanswered for 1500 ms, past 2 times the margin"),
        "{}",
        rate.judged()
    );

    let mut rate = Rate::new(15_000);
    rate.second(at(start, 0), &sending(14_000));
    run(&mut rate, start, 1, 10, gone(Some(1_500.0)));
    assert_eq!(rate.backoffs(), 0);

    let mut rate = Rate::new(15_000);
    rate.second(at(start, 0), &risen);
    run(&mut rate, start, 1, 10, gone(Some(900.0)));
    assert_eq!(rate.backoffs(), 0);
    assert_eq!(rate.judged().risen_seconds, 1);

    // Sending under half the rate, five such seconds stand like a round
    // trip past twice the margin and cut through the gate.
    let light = Second {
        sent: 100,
        ..sending(7_200)
    };
    let mut rate = Rate::new(15_000);
    rate.second(
        at(start, 0),
        &Second {
            round_trip: rtt(40.0),
            ..light
        },
    );
    let decisions = run(
        &mut rate,
        start,
        1,
        5,
        Second {
            round_trip: None,
            unanswered_ms: Some(2_000.0),
            ..light
        },
    );
    assert!(decisions[..4].iter().all(|d| d.backoff.is_none()));
    assert_eq!(
        (decisions[4].rate_kbps, decisions[4].backoff),
        (Some(5_760), Some(Sign::FarRoundTrip))
    );
}

// A second watcher on the host's own share: its upload carries two copies,
// so a rate the link held under the setting is halved with the setting, and
// the climb from there is careful. A rate the setting held is the new
// allowance already.
#[test]
fn a_second_copy_shares_out_what_the_link_carried() {
    let start = Instant::now();
    let mut rate = Rate::new(15_000);
    assert_eq!(rate.copies(7_500, 1, 2), Some(7_500));

    let mut rate = Rate::new(15_000);
    rate.second(
        start,
        &Second {
            lost: 10,
            round_trip: queue_beside(),
            ..sending(14_000)
        },
    );
    assert_eq!(rate.rate_kbps(), 12_000);
    assert_eq!(rate.copies(7_500, 1, 2), Some(6_000));
    assert_eq!(rate.allowed_kbps(), 7_500);
    // Careful: 10 percent after 5 clean seconds.
    let climbed: Vec<(u64, u32)> = run(&mut rate, start, 3, 8, sending(6_000))
        .iter()
        .zip(3..)
        .filter_map(|(decision, n)| decision.rate_kbps.map(|kbps| (n, kbps)))
        .collect();
    assert_eq!(climbed, [(7, 6_600)]);
    // Fewer copies is the plain allowance.
    assert_eq!(rate.copies(15_000, 2, 1), None);
    assert_eq!(rate.allowed_kbps(), 15_000);
}
