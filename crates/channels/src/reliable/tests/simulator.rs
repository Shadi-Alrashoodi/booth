use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::ms;
use crate::reliable::{
    MAX_FRAME, MAX_MESSAGE, MAX_QUEUED, Reliable, ReliableCounters, ReliableError, WINDOW,
};
use crate::rtt::RttEstimator;

struct Rng(u64);

impl Rng {
    // splitmix64: tiny, seedable, and good enough to drive a simulation.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn percent(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
}

#[derive(Debug, Clone, Copy)]
enum Reader {
    // Reads everything on every tick.
    Eager,
    // Reads up to `batch` messages on `percent` of ticks, and nothing at all
    // from `stall.0` to `stall.1` us.
    Lazy {
        percent: u64,
        batch: u64,
        stall: (u64, u64),
    },
}

#[derive(Debug, Clone, Copy)]
struct Conditions {
    loss: u64,
    // Percent of frames sent more than once, with 1 to `extra` more copies.
    duplicate: u64,
    extra: u64,
    delay_us: u64,
    jitter_us: u64,
    // Percent of copies held back by up to late_us, long past the sender's
    // retransmit timer.
    late: u64,
    late_us: u64,
    timeout: Option<Duration>,
    reader: Reader,
}

impl Conditions {
    fn lossy(loss: u64) -> Conditions {
        Conditions {
            loss,
            duplicate: 10,
            extra: 1,
            delay_us: 2_000,
            // Jitter several times the delay reorders most bursts.
            jitter_us: 8_000,
            late: 0,
            late_us: 0,
            // Longer than the slowest round trip this link allows, 2 * (2 + 8) ms.
            timeout: Some(ms(29)),
            reader: Reader::Eager,
        }
    }

    // Copies seconds late, so acks several windows old and duplicates of
    // long delivered messages turn up; a retransmit timeout that can be far
    // off; and an application that reads when it gets around to it.
    fn hostile(loss: u64, timeout: Option<Duration>, read_percent: u64) -> Conditions {
        Conditions {
            duplicate: 20,
            extra: 3,
            late: 3,
            late_us: 4_000_000,
            timeout,
            reader: Reader::Lazy {
                percent: read_percent,
                batch: 200,
                stall: (0, 0),
            },
            ..Conditions::lossy(loss)
        }
    }

    // Round trips like the ones the host measured to a friend on a phone
    // hotspot: min 16, average 32, p95 105 ms. Most copies take 8 to 16 ms one
    // way, one in twenty is held up to 170 ms more.
    fn phone_hotspot(loss: u64) -> Conditions {
        Conditions {
            loss,
            duplicate: 0,
            extra: 1,
            delay_us: 8_000,
            jitter_us: 8_000,
            late: 5,
            late_us: 170_000,
            timeout: None,
            reader: Reader::Eager,
        }
    }

    fn one_way_us(&self, rng: &mut Rng) -> u64 {
        let mut us = self.delay_us + rng.below(self.jitter_us + 1);
        if self.late > 0 && rng.percent(self.late) {
            us += rng.below(self.late_us + 1);
        }
        us
    }
}

#[derive(Default)]
struct Link {
    in_transit: BinaryHeap<Reverse<(u64, u64, Vec<u8>)>>,
    order: u64,
    // Loss applies to each copy, so it is measured against copies, not
    // frames.
    copies: u64,
    dropped: u64,
}

impl Link {
    fn put(&mut self, frame: Vec<u8>, now_us: u64, conditions: &Conditions, rng: &mut Rng) {
        let copies = if rng.percent(conditions.duplicate) {
            2 + rng.below(conditions.extra)
        } else {
            1
        };
        for _ in 0..copies {
            self.copies += 1;
            if rng.percent(conditions.loss) {
                self.dropped += 1;
                continue;
            }
            let at = now_us + conditions.one_way_us(rng);
            self.in_transit
                .push(Reverse((at, self.order, frame.clone())));
            self.order += 1;
        }
    }

    fn take_due(&mut self, now_us: u64) -> Vec<Vec<u8>> {
        let mut due = Vec::new();
        while self
            .in_transit
            .peek()
            .is_some_and(|Reverse((at, _, _))| *at <= now_us)
        {
            if let Some(Reverse((_, _, frame))) = self.in_transit.pop() {
                due.push(frame);
            }
        }
        due
    }

    fn next_due(&self) -> Option<u64> {
        self.in_transit.peek().map(|Reverse((at, _, _))| *at)
    }
}

struct Peer {
    reliable: Reliable,
    outbox: VecDeque<Vec<u8>>,
    expect: Vec<Vec<u8>>,
    inbox: Vec<Vec<u8>>,
    most_waiting: usize,
}

impl Peer {
    fn new(start: u32, outbox: &[Vec<u8>], expect: &[Vec<u8>]) -> Peer {
        Peer {
            reliable: Reliable::starting_at(start),
            outbox: outbox.iter().cloned().collect(),
            expect: expect.to_vec(),
            inbox: Vec::new(),
            most_waiting: 0,
        }
    }

    fn settled(&self) -> bool {
        let counters = self.reliable.counters();
        self.outbox.is_empty()
            && counters.in_flight == 0
            && counters.queued == 0
            && self.inbox.len() == self.expect.len()
    }

    fn wants_app_tick(&self) -> bool {
        !self.outbox.is_empty() || !self.reliable.delivered.is_empty()
    }

    fn read(&mut self, now_us: u64, conditions: &Conditions, rng: &mut Rng) {
        let batch = match conditions.reader {
            Reader::Eager => usize::MAX,
            Reader::Lazy {
                percent,
                batch,
                stall: (from, to),
            } => {
                if (from..to).contains(&now_us) || !rng.percent(percent) {
                    return;
                }
                1 + rng.below(batch) as usize
            }
        };
        for _ in 0..batch {
            let Some(message) = self.reliable.next_delivered() else {
                break;
            };
            let index = self.inbox.len();
            assert!(
                self.expect.get(index) == Some(&message),
                "message {index} of {} is out of order or a duplicate",
                self.expect.len()
            );
            self.inbox.push(message);
        }
    }

    fn tick(
        &mut self,
        incoming: &mut Link,
        outgoing: &mut Link,
        base: Instant,
        now_us: u64,
        conditions: &Conditions,
        rng: &mut Rng,
    ) {
        let now = base + Duration::from_micros(now_us);
        for frame in incoming.take_due(now_us) {
            if let Err(e) = self.reliable.receive(&frame, now) {
                panic!("a correct peer's frame was rejected: {e}");
            }
        }
        self.most_waiting = self.most_waiting.max(self.reliable.delivered.len());
        self.read(now_us, conditions, rng);

        // The application writes in small bursts, and sometimes faster than
        // the window drains.
        for _ in 0..rng.below(6) {
            let Some(message) = self.outbox.front() else {
                break;
            };
            match self.reliable.send(message) {
                Ok(()) => {
                    self.outbox.pop_front();
                }
                Err(ReliableError::Full) => break,
                Err(e) => panic!("send failed: {e}"),
            }
        }

        while let Some(frame) = self.reliable.poll_transmit(now, conditions.timeout) {
            assert!(frame.len() <= MAX_FRAME);
            outgoing.put(frame, now_us, conditions, rng);
        }
        assert!(self.reliable.in_flight.len() <= WINDOW);
        assert!(self.reliable.delivered.len() < MAX_QUEUED + WINDOW);
    }
}

fn micros_after(base: Instant, at: Instant) -> u64 {
    at.duration_since(base).as_nanos().div_ceil(1000) as u64
}

struct Outcome {
    simulated_ms: u64,
    copies: u64,
    dropped: u64,
    a: ReliableCounters,
    b: ReliableCounters,
    most_waiting_on_b: usize,
}

// Runs two peers over a lossy, reordering, duplicating link on a fake clock
// until both sides have read everything and every message is acked, then
// until the last copy in transit has arrived.
fn simulate(
    seed: u64,
    start: u32,
    conditions: Conditions,
    a_to_b: &[Vec<u8>],
    b_to_a: &[Vec<u8>],
) -> Outcome {
    let base = Instant::now();
    let mut rng = Rng(seed);
    let mut a = Peer::new(start, a_to_b, b_to_a);
    let mut b = Peer::new(start, b_to_a, a_to_b);
    let mut ab = Link::default();
    let mut ba = Link::default();
    let mut now_us = 0;

    for _ in 0..2_000_000 {
        a.tick(&mut ba, &mut ab, base, now_us, &conditions, &mut rng);
        b.tick(&mut ab, &mut ba, base, now_us, &conditions, &mut rng);

        if a.settled() && b.settled() {
            let outcome = Outcome {
                simulated_ms: now_us / 1000,
                copies: ab.copies + ba.copies,
                dropped: ab.dropped + ba.dropped,
                a: a.reliable.counters(),
                b: b.reliable.counters(),
                most_waiting_on_b: b.most_waiting,
            };
            // Late copies still on the way must not be delivered again or
            // upset either side.
            while let Some(next) = [ab.next_due(), ba.next_due()].into_iter().flatten().min() {
                now_us = next;
                a.tick(&mut ba, &mut ab, base, now_us, &conditions, &mut rng);
                b.tick(&mut ab, &mut ba, base, now_us, &conditions, &mut rng);
            }
            assert!(a.settled() && b.settled(), "seed {seed}");
            assert_eq!(b.inbox, a_to_b, "a to b, seed {seed}");
            assert_eq!(a.inbox, b_to_a, "b to a, seed {seed}");
            return outcome;
        }

        let mut next = [
            ab.next_due(),
            ba.next_due(),
            a.reliable.next_timeout().map(|t| micros_after(base, t)),
            b.reliable.next_timeout().map(|t| micros_after(base, t)),
        ]
        .into_iter()
        .flatten()
        .min();
        if a.wants_app_tick() || b.wants_app_tick() {
            let app = now_us + 1_000;
            next = Some(next.map_or(app, |n| n.min(app)));
        }
        let Some(next) = next else {
            panic!(
                "simulation stalled at {now_us} us, seed {seed}: a {:?}, b {:?}, b got {} of {}, a got {} of {}",
                a.reliable.counters(),
                b.reliable.counters(),
                b.inbox.len(),
                a_to_b.len(),
                a.inbox.len(),
                b_to_a.len()
            );
        };
        assert!(next > now_us, "time must move forward");
        now_us = next;
    }
    panic!("simulation did not finish, seed {seed}");
}

fn messages(rng: &mut Rng, sizes: &[usize]) -> Vec<Vec<u8>> {
    sizes
        .iter()
        .map(|&size| (0..size).map(|_| rng.next() as u8).collect())
        .collect()
}

fn random_sizes(rng: &mut Rng, count: usize) -> Vec<usize> {
    (0..count)
        .map(|_| rng.below(MAX_MESSAGE as u64 + 1) as usize)
        .collect()
}

fn check_lossy(name: &str, outcome: &Outcome, a_to_b: usize, b_to_a: usize, loss: u64) {
    println!(
        "{name}: {} + {} messages in {} simulated ms, {} of {} copies dropped, {} + {} retransmissions",
        outcome.a.sent,
        outcome.b.sent,
        outcome.simulated_ms,
        outcome.dropped,
        outcome.copies,
        outcome.a.retransmissions,
        outcome.b.retransmissions
    );
    assert_eq!(outcome.a.sent, a_to_b as u64);
    assert_eq!(outcome.b.sent, b_to_a as u64);
    let percent = outcome.dropped * 100 / outcome.copies;
    assert!(
        percent + 5 >= loss && percent <= loss + 5,
        "link dropped {percent} percent, asked for {loss}"
    );
    assert!(outcome.a.retransmissions > 0);
}

#[test]
fn twenty_percent_loss() {
    let mut rng = Rng(20);
    let sizes = random_sizes(&mut rng, 600);
    let a_to_b = messages(&mut rng, &sizes);
    let b_to_a = messages(&mut rng, &sizes[..150]);
    let outcome = simulate(1, 0, Conditions::lossy(20), &a_to_b, &b_to_a);
    check_lossy("20 percent", &outcome, 600, 150, 20);
}

#[test]
fn fifty_percent_loss() {
    let mut rng = Rng(50);
    let sizes = random_sizes(&mut rng, 300);
    let a_to_b = messages(&mut rng, &sizes);
    let b_to_a = messages(&mut rng, &sizes[..100]);
    let outcome = simulate(2, 0, Conditions::lossy(50), &a_to_b, &b_to_a);
    check_lossy("50 percent", &outcome, 300, 100, 50);
}

#[test]
fn unknown_round_trip_still_delivers() {
    let mut rng = Rng(7);
    let sizes = random_sizes(&mut rng, 200);
    let a_to_b = messages(&mut rng, &sizes);
    let conditions = Conditions {
        timeout: None,
        ..Conditions::lossy(20)
    };
    let outcome = simulate(3, 0, conditions, &a_to_b, &[]);
    check_lossy("20 percent, no rtt", &outcome, 200, 0, 20);
}

#[test]
fn sequence_numbers_wrap() {
    let mut rng = Rng(32);
    let sizes = random_sizes(&mut rng, 400);
    let a_to_b = messages(&mut rng, &sizes);
    let b_to_a = messages(&mut rng, &sizes[..100]);
    let start = u32::MAX - 150;
    let outcome = simulate(4, start, Conditions::lossy(20), &a_to_b, &b_to_a);
    check_lossy("wrap, 20 percent", &outcome, 400, 100, 20);
    let outcome = simulate(5, start, Conditions::lossy(50), &a_to_b[..200], &b_to_a);
    check_lossy("wrap, 50 percent", &outcome, 200, 100, 50);
}

// The receiver stops reading for 8 s while the sender keeps writing, so its
// queue fills and the flow control path runs under loss and late copies.
#[test]
fn stalled_reader_on_a_hostile_network() {
    for (seed, timeout) in [(11, Some(ms(29))), (12, Some(ms(20))), (13, None)] {
        let mut rng = Rng(seed);
        let sizes = random_sizes(&mut rng, 3000);
        let a_to_b = messages(&mut rng, &sizes);
        let b_to_a = messages(&mut rng, &sizes[..500]);
        let conditions = Conditions {
            reader: Reader::Lazy {
                percent: 50,
                batch: 200,
                stall: (200_000, 8_200_000),
            },
            ..Conditions::hostile(20, timeout, 50)
        };
        let outcome = simulate(seed, u32::MAX - 1000, conditions, &a_to_b, &b_to_a);
        println!(
            "stalled reader, seed {seed}: {} simulated ms, {} of {} copies dropped, {} + {} retransmissions, {} messages waiting at most",
            outcome.simulated_ms,
            outcome.dropped,
            outcome.copies,
            outcome.a.retransmissions,
            outcome.b.retransmissions,
            outcome.most_waiting_on_b
        );
        assert!(
            outcome.most_waiting_on_b >= MAX_QUEUED,
            "seed {seed}: the queue never filled, at most {} waited",
            outcome.most_waiting_on_b
        );
    }
}

struct Paced {
    retransmissions: u64,
    // What the pings measured, the ones that made it back.
    round_trips: Vec<Duration>,
    copies: u64,
    dropped: u64,
}

// Min, average and nearest-rank p95, worked out in microseconds and rounded
// to the nearest ms only at the end.
fn round_trip_ms(round_trips: &[Duration]) -> (u64, u64, u64) {
    let mut sorted: Vec<u64> = round_trips
        .iter()
        .map(|rtt| rtt.as_micros() as u64)
        .collect();
    sorted.sort_unstable();
    let count = sorted.len() as u64;
    let average = (sorted.iter().sum::<u64>() + count / 2) / count;
    let p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1];
    let ms = |us: u64| (us + 500) / 1000;
    (ms(sorted[0]), ms(average), ms(p95))
}

// One message a second from a to b, each right behind a ping, the way the
// host sends its roster. The pings cross the same link and feed the estimator
// that sets a's retransmit timeout. b only acks.
fn one_a_second(seed: u64, conditions: Conditions, seconds: u64) -> Paced {
    let base = Instant::now();
    let mut rng = Rng(seed);
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    let mut estimator = RttEstimator::new();
    let mut ab = Link::default();
    let mut ba = Link::default();
    // When each pong gets back and the round trip it measured.
    let mut pongs: Vec<(u64, Duration)> = Vec::new();
    let mut round_trips = Vec::new();
    let sent: Vec<Vec<u8>> = (0..seconds)
        .map(|i| format!("roster {i}").into_bytes())
        .collect();
    let mut next_message = 0;
    let mut next_send = Some(0);
    let mut got = Vec::new();
    let mut now_us = 0;

    loop {
        let now = base + Duration::from_micros(now_us);
        for frame in ab.take_due(now_us) {
            b.receive(&frame, now).unwrap();
        }
        for frame in ba.take_due(now_us) {
            a.receive(&frame, now).unwrap();
        }
        pongs.retain(|&(at, rtt)| {
            if at > now_us {
                return true;
            }
            estimator.sample(rtt);
            round_trips.push(rtt);
            false
        });
        while let Some(message) = b.next_delivered() {
            got.push(message);
        }
        if next_send == Some(now_us) {
            if !rng.percent(conditions.loss) && !rng.percent(conditions.loss) {
                let rtt_us = conditions.one_way_us(&mut rng) + conditions.one_way_us(&mut rng);
                pongs.push((now_us + rtt_us, Duration::from_micros(rtt_us)));
            }
            a.send(&sent[next_message]).unwrap();
            next_message += 1;
            next_send = (next_message < sent.len()).then_some(now_us + 1_000_000);
        }
        while let Some(frame) = a.poll_transmit(now, estimator.retransmit_timeout()) {
            ab.put(frame, now_us, &conditions, &mut rng);
        }
        while let Some(frame) = b.poll_transmit(now, None) {
            ba.put(frame, now_us, &conditions, &mut rng);
        }

        let next = [
            ab.next_due(),
            ba.next_due(),
            pongs.iter().map(|&(at, _)| at).min(),
            a.next_timeout().map(|t| micros_after(base, t)),
            next_send,
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next) = next else {
            break;
        };
        assert!(next > now_us, "time must move forward");
        assert!(
            next < (seconds + 60) * 1_000_000,
            "seed {seed}: still not delivered a minute after the last message"
        );
        now_us = next;
    }
    assert_eq!(got, sent, "seed {seed}");
    Paced {
        retransmissions: a.counters().retransmissions,
        round_trips,
        copies: ab.copies + ba.copies,
        dropped: ab.dropped + ba.dropped,
    }
}

// One round trip in twenty on this link takes over 105 ms, and its ack comes
// back after any timeout set below that. So a little over three retransmits
// a minute is the floor for a timeout that does not hold every retransmit
// past the p95, and the bound leaves some room above it.
#[test]
fn jittery_link_costs_few_retransmissions() {
    let minutes = 50;
    let mut retransmissions = 0;
    let mut round_trips = Vec::new();
    for seed in 0..minutes {
        let paced = one_a_second(seed, Conditions::phone_hotspot(0), 60);
        assert_eq!(paced.dropped, 0);
        retransmissions += paced.retransmissions;
        round_trips.extend(paced.round_trips);
    }
    let (min, average, p95) = round_trip_ms(&round_trips);
    println!(
        "jittery link: round trip min {min} ms, average {average} ms, p95 {p95} ms; {retransmissions} retransmissions in {minutes} minutes of one message a second"
    );
    assert!((16..=17).contains(&min), "min {min} ms");
    assert!((29..=35).contains(&average), "average {average} ms");
    assert!((95..=115).contains(&p95), "p95 {p95} ms");
    assert!(
        retransmissions <= 4 * minutes,
        "{retransmissions} retransmissions in {minutes} minutes"
    );
}

#[test]
fn jittery_link_with_loss() {
    for seed in 0..10 {
        let paced = one_a_second(seed, Conditions::phone_hotspot(20), 60);
        let percent = paced.dropped * 100 / paced.copies;
        assert!(
            (15..=25).contains(&percent),
            "seed {seed}: dropped {percent} percent"
        );
        assert!(paced.retransmissions > 0, "seed {seed}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn any_lossy_network(
        seed in any::<u64>(),
        sizes in proptest::collection::vec(0..=MAX_MESSAGE, 1..120),
        loss in prop_oneof![Just(20u64), Just(50u64)],
        start in prop_oneof![Just(0u32), Just(u32::MAX - 40), any::<u32>()],
        timeout_known in any::<bool>(),
    ) {
        let mut rng = Rng(seed);
        let a_to_b = messages(&mut rng, &sizes);
        let b_to_a = messages(&mut rng, &sizes[..sizes.len() / 3]);
        let mut conditions = Conditions::lossy(loss);
        if !timeout_known {
            conditions.timeout = None;
        }
        simulate(seed, start, conditions, &a_to_b, &b_to_a);
    }

    #[test]
    fn any_hostile_network(
        seed in any::<u64>(),
        sizes in proptest::collection::vec(0..=MAX_MESSAGE, 1..150),
        loss in 0u64..=60,
        start in prop_oneof![Just(0u32), Just(u32::MAX - 40), any::<u32>()],
        timeout in prop_oneof![
            Just(None),
            Just(Some(Duration::ZERO)),
            (0u64..250).prop_map(|n| Some(ms(n))),
        ],
        read_percent in 1u64..100,
    ) {
        let mut rng = Rng(seed);
        let a_to_b = messages(&mut rng, &sizes);
        let b_to_a = messages(&mut rng, &sizes[..sizes.len() / 2]);
        let conditions = Conditions::hostile(loss, timeout, read_percent);
        simulate(seed, start, conditions, &a_to_b, &b_to_a);
    }
}
