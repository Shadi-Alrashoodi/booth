use std::time::{Duration, Instant};

use proptest::prelude::*;
use stats::{LOST_AFTER, LinkStats, StreamStats, TRACE_LEN, TraceSample};

#[derive(Debug, Clone)]
enum Op {
    Ping {
        seq: u32,
        after_ms: u16,
    },
    Pong {
        seq: u32,
        rtt_ms: u32,
        after_ms: u16,
    },
    PeerPing {
        seq: u32,
        sent_us: u64,
        arrived_us: u64,
    },
    Tick {
        after_ms: u16,
    },
}

fn seq() -> impl Strategy<Value = u32> {
    prop_oneof![0u32..40, (u32::MAX - 20)..=u32::MAX, any::<u32>()]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (seq(), 0u16..300).prop_map(|(seq, after_ms)| Op::Ping { seq, after_ms }),
        (seq(), any::<u32>(), 0u16..300).prop_map(|(seq, rtt_ms, after_ms)| Op::Pong {
            seq,
            rtt_ms,
            after_ms
        }),
        (seq(), any::<u64>(), any::<u64>()).prop_map(|(seq, sent_us, arrived_us)| {
            Op::PeerPing {
                seq,
                sent_us,
                arrived_us,
            }
        }),
        (0u16..3000).prop_map(|after_ms| Op::Tick { after_ms }),
    ]
}

fn percent(v: Option<f32>) -> bool {
    v.is_none_or(|p| (0.0..=100.0).contains(&p))
}

proptest! {
    #[test]
    fn any_sequence_of_calls_keeps_the_numbers_sane(ops in prop::collection::vec(op(), 0..400)) {
        let mut stats = LinkStats::new();
        let mut now = Instant::now();
        for op in ops {
            match op {
                Op::Ping { seq, after_ms } => {
                    now += Duration::from_millis(u64::from(after_ms));
                    stats.ping_sent(seq, now);
                }
                Op::Pong { seq, rtt_ms, after_ms } => {
                    now += Duration::from_millis(u64::from(after_ms));
                    stats.pong_received(seq, Duration::from_millis(u64::from(rtt_ms)), now);
                }
                Op::PeerPing { seq, sent_us, arrived_us } => {
                    stats.peer_ping_received(seq, sent_us, arrived_us);
                }
                Op::Tick { after_ms } => {
                    now += Duration::from_millis(u64::from(after_ms));
                    stats.tick(now);
                }
            }

            let s = stats.snapshot();
            prop_assert!(s.trace.len() <= TRACE_LEN);
            prop_assert!(s.pongs_received + s.lost <= s.pings_sent);
            prop_assert!(percent(s.loss_pct));
            prop_assert!(percent(s.inbound_loss_pct));
            prop_assert!(s.jitter_ms.is_none_or(|j| j.is_finite() && j >= 0.0));
            for sample in &s.trace {
                if let TraceSample::Rtt(ms) = sample {
                    prop_assert!(ms.is_finite() && *ms >= 0.0);
                    prop_assert!(Duration::from_secs_f32(ms / 1000.0) <= LOST_AFTER);
                }
            }
            if let (Some(min), Some(max), Some(avg), Some(p95)) =
                (s.rtt_min_ms, s.rtt_max_ms, s.rtt_avg_ms, s.rtt_p95_ms)
            {
                prop_assert!(min <= p95 && p95 <= max);
                prop_assert!(avg >= min - 1e-3 && avg <= max + 1e-3);
            }
        }
    }

    #[test]
    fn trace_follows_send_order_whatever_order_pongs_come_in(
        (answer_order, answered) in (1usize..150).prop_flat_map(|n| (
            Just((0..n).collect::<Vec<usize>>()).prop_shuffle(),
            prop::collection::vec(any::<bool>(), n),
        )),
        first_seq in seq(),
    ) {
        let n = answered.len();
        let mut stats = LinkStats::new();
        let t0 = Instant::now();
        for i in 0..n {
            stats.ping_sent(first_seq.wrapping_add(i as u32), t0 + Duration::from_millis(i as u64));
        }
        let at = t0 + Duration::from_millis(1000);
        for i in answer_order {
            if answered[i] {
                let rtt = Duration::from_millis(i as u64 + 1);
                stats.pong_received(first_seq.wrapping_add(i as u32), rtt, at);
            }
        }
        stats.tick(t0 + Duration::from_secs(10));

        let expected: Vec<TraceSample> = (0..n)
            .map(|i| if answered[i] { TraceSample::Rtt(i as f32 + 1.0) } else { TraceSample::Lost })
            .collect();
        let tail = &expected[n.saturating_sub(TRACE_LEN)..];
        prop_assert_eq!(stats.snapshot().trace, tail.to_vec());
    }

    // Numbers near the last ones heard most of the time, anything at all
    // now and then, as a friend's broken or hostile build could send.
    #[test]
    fn any_stream_of_packets_keeps_its_numbers_sane(
        packets in prop::collection::vec(
            (prop_oneof![3 => -3i32..20, 1 => any::<i32>()], any::<Option<u64>>(), any::<u64>(), 0u16..300),
            0..600,
        ),
        first in any::<u16>(),
    ) {
        let mut stats = StreamStats::new();
        let mut now = Instant::now();
        let mut seq = first;
        for (step, sent_us, arrived_us, after_ms) in packets {
            seq = seq.wrapping_add(step as u16);
            now += Duration::from_millis(u64::from(after_ms));
            stats.record(seq, sent_us, arrived_us, now);
            if let Some(loss) = stats.loss(now) {
                prop_assert!(loss.lost < loss.expected);
                prop_assert!(percent(loss.percent()));
            }
            prop_assert!(stats.jitter_ms().is_none_or(|j| j.is_finite() && j >= 0.0));
        }
    }
}
