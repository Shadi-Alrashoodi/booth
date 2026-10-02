// What stands in for the internet between the loopback's two sides when
// --delay, --jitter or --capacity asks for one. Every video packet, the
// viewer's answers and pings both ways cross it, so the rate's backoff
// (share::rate) and the viewer's reassembler meet a network as they do in a
// room.
//
// Toward the viewer, a packet first waits in the queue of a link of
// --capacity, as in the sharer's router, and is dropped when the queue holds
// more than QUEUE_DEPTH; --lift frees the link after that many seconds of
// sending. Then it takes --delay, plus an extra delay that is random,
// exponential with a mean of --jitter: mostly a little, now and then a lot.
// Each draw holds for JITTER_HOLDS, for every packet that goes in that time
// (see there). Toward the sharer only the delay and the jitter, since a
// download is not where a share queues. Neither way reorders: a packet never
// arrives before the one ahead of it, as Wi-Fi's block ack holds the packets
// behind a retried one until it arrives, so one late packet delays those
// behind it too.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use net::pace::{self, Signal, Timer};
use share::rate::{self, RoundTrip};
use share::{Back, LinkNumbers};
use stats::{LinkStats, Thresholds};
use viewer::{LinkState, PathWord};

use super::link::Link;

// A home router's upload buffer holds a few hundred milliseconds at a few
// Mbit/s; past that it drops what arrives.
pub const QUEUE_DEPTH: Duration = Duration::from_millis(200);
// What a video packet carries besides the payload the pacer hands over, as
// the router counts it: the session's 32 bytes, the channel byte, the
// room's two, and IPv4 and UDP's 28.
const OVERHEAD: u64 = 32 + 1 + 2 + 28;
// A ping or its answer, sealed, with its headers.
const PING_BYTES: u64 = 96;
// The room's pings: ten a second each way.
const PING_EVERY: Duration = Duration::from_millis(100);
// Wi-Fi's delay comes in stretches, a busy channel, retries, a laptop
// waking from power save, so a draw of the jitter holds this long for every
// packet sent in it. Drawn for each packet instead, a frame's packets spread
// over tens of milliseconds and the reassembler dropped 5 percent of the
// pattern's frames, where the laptop's Wi-Fi in the first share over the
// internet, on 2026-09-29, dropped 0.5 to 0.8; the pings then read 7/23/78 ms
// for that share's 4/16/65.
const JITTER_HOLDS: Duration = Duration::from_millis(50);
// The sharer's round trips kept for the rate: its 30 s floor and spread,
// and a little more.
const PINGS_KEPT: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    // One way.
    pub delay: Duration,
    // The mean extra delay, one way.
    pub jitter: Duration,
    pub capacity_bits: Option<u64>,
    pub lift_after: Option<Duration>,
    pub seed: u64,
}

impl Shape {
    // Nothing between the two sides: packets go straight into the viewer's
    // inbox.
    pub fn idle(&self) -> bool {
        self.delay.is_zero() && self.jitter.is_zero() && self.capacity_bits.is_none()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkNumbers {
    pub queue_dropped: u64,
    // The sharer's pings answered, and their round trips.
    pub pings: u64,
    pub rtt_min: Duration,
    pub rtt_max: Duration,
    pub rtt_sum: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Sharer,
    Viewer,
}

enum Parcel {
    Video(Vec<u8>),
    Back(Back),
    // Answered the moment it arrives.
    Ping { from: Side, seq: u32, sent: Instant },
    Pong { to: Side, seq: u32, sent: Instant },
}

struct State {
    toward_viewer: VecDeque<(Instant, Parcel)>,
    toward_sharer: VecDeque<(Instant, Parcel)>,
    // When the link of --capacity is done with what it holds.
    busy_until: Instant,
    // The last packet's arrival each way, which the next never beats.
    last_to_viewer: Instant,
    last_to_sharer: Instant,
    // Since the first video packet, for --lift.
    sending_since: Option<Instant>,
    draws: Draws,
    // Each way: the extra delay drawn last, and until when it holds.
    held_to_viewer: (Instant, Duration),
    held_to_sharer: (Instant, Duration),
    next_seq: u32,
    sharer_pings: VecDeque<(Instant, Duration)>,
    viewer: LinkStats,
    numbers: NetworkNumbers,
}

pub struct Network {
    shape: Shape,
    link: Arc<Link>,
    state: Mutex<State>,
    wake: Signal,
    epoch: Instant,
}

impl Network {
    pub fn new(shape: Shape, link: Arc<Link>) -> io::Result<Network> {
        let now = Instant::now();
        Ok(Network {
            shape,
            link,
            state: Mutex::new(State {
                toward_viewer: VecDeque::new(),
                toward_sharer: VecDeque::new(),
                busy_until: now,
                last_to_viewer: now,
                last_to_sharer: now,
                sending_since: None,
                draws: Draws(shape.seed ^ 0x6a09_e667_f3bc_c908),
                held_to_viewer: (now, Duration::ZERO),
                held_to_sharer: (now, Duration::ZERO),
                next_seq: 0,
                sharer_pings: VecDeque::new(),
                viewer: LinkStats::new(),
                numbers: NetworkNumbers::default(),
            }),
            wake: Signal::new()?,
            epoch: now,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn idle(&self) -> bool {
        self.shape.idle()
    }

    // From the pacer's thread, for each packet the loss knob let through.
    pub fn video(&self, packet: &[u8]) {
        if self.idle() {
            self.link.inbox.packet(packet);
            return;
        }
        let now = Instant::now();
        let mut state = self.lock();
        state.sending_since.get_or_insert(now);
        let bytes = packet.len() as u64 + OVERHEAD;
        self.toward_viewer(&mut state, now, Parcel::Video(packet.to_vec()), bytes);
    }

    // From the viewer's thread: what the room would send on the control
    // channel, which loses nothing.
    pub fn back(&self, message: Back) {
        if self.idle() {
            self.link.send_back(message);
            return;
        }
        let now = Instant::now();
        let mut state = self.lock();
        self.toward_sharer(&mut state, now, Parcel::Back(message));
    }

    // For the rate, as the room's links give it.
    pub fn round_trip(&self, now: Instant) -> Option<RoundTrip> {
        rate::round_trip(&self.lock().sharer_pings, now)
    }

    pub fn numbers(&self) -> NetworkNumbers {
        self.lock().numbers
    }

    // The network's own thread, until the link stops: packets arrive when
    // they are due, and pings go both ways.
    pub fn run(&self) -> Result<(), String> {
        // Failing only makes the delays a little late on a busy PC.
        let _ = pace::raise_priority();
        let timer = Timer::new().map_err(|err| err.to_string())?;
        let mut next_ping = Instant::now();
        while !self.link.stopped() {
            let now = Instant::now();
            if now >= next_ping {
                self.ping(now);
                next_ping = (next_ping + PING_EVERY).max(now);
            }
            let next = self
                .deliver(now)
                .map_or(next_ping, |due| due.min(next_ping));
            timer
                .set_at(next)
                .and_then(|()| pace::wait(&self.wake, Some(&timer)).map(drop))
                .map_err(|err| format!("the loopback's network stopped: {err}"))?;
        }
        Ok(())
    }

    fn toward_viewer(&self, state: &mut State, now: Instant, parcel: Parcel, bytes: u64) {
        let sending = state
            .sending_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let capacity = self
            .shape
            .capacity_bits
            .filter(|_| self.shape.lift_after.is_none_or(|after| sending < after));
        let leaves = match capacity {
            Some(bits) => {
                let starts = state.busy_until.max(now);
                if starts.saturating_duration_since(now) > QUEUE_DEPTH {
                    state.numbers.queue_dropped += 1;
                    return;
                }
                state.busy_until =
                    starts + Duration::from_secs_f64((bytes * 8) as f64 / bits as f64);
                state.busy_until
            }
            None => now,
        };
        let extra = held(
            &mut state.held_to_viewer,
            &mut state.draws,
            self.shape.jitter,
            leaves,
        );
        let due = (leaves + self.shape.delay + extra).max(state.last_to_viewer);
        state.last_to_viewer = due;
        let first = state.toward_viewer.is_empty();
        state.toward_viewer.push_back((due, parcel));
        if first {
            self.wake.set();
        }
    }

    fn toward_sharer(&self, state: &mut State, now: Instant, parcel: Parcel) {
        let extra = held(
            &mut state.held_to_sharer,
            &mut state.draws,
            self.shape.jitter,
            now,
        );
        let due = (now + self.shape.delay + extra).max(state.last_to_sharer);
        state.last_to_sharer = due;
        let first = state.toward_sharer.is_empty();
        state.toward_sharer.push_back((due, parcel));
        if first {
            self.wake.set();
        }
    }

    fn ping(&self, now: Instant) {
        let mut state = self.lock();
        let seq = state.next_seq;
        state.next_seq = seq.wrapping_add(1);
        let from_sharer = Parcel::Ping {
            from: Side::Sharer,
            seq,
            sent: now,
        };
        self.toward_viewer(&mut state, now, from_sharer, PING_BYTES);
        state.viewer.ping_sent(seq, now);
        let from_viewer = Parcel::Ping {
            from: Side::Viewer,
            seq,
            sent: now,
        };
        self.toward_sharer(&mut state, now, from_viewer);
    }

    // Hands over what is due, and says when the next thing is.
    fn deliver(&self, now: Instant) -> Option<Instant> {
        let mut arrived = Vec::new();
        let next = {
            let mut guard = self.lock();
            let state = &mut *guard;
            for queue in [&mut state.toward_viewer, &mut state.toward_sharer] {
                while queue.front().is_some_and(|(due, _)| *due <= now) {
                    if let Some((_, parcel)) = queue.pop_front() {
                        arrived.push(parcel);
                    }
                }
            }
            [state.toward_viewer.front(), state.toward_sharer.front()]
                .into_iter()
                .flatten()
                .map(|(due, _)| *due)
                .min()
        };
        for parcel in arrived {
            self.arrive(parcel, now);
        }
        next
    }

    fn arrive(&self, parcel: Parcel, now: Instant) {
        match parcel {
            Parcel::Video(packet) => self.link.inbox.packet(&packet),
            Parcel::Back(message) => self.link.send_back(message),
            Parcel::Ping {
                from: Side::Sharer,
                seq,
                sent,
            } => {
                let mut state = self.lock();
                let (sent_us, arrived_us) = (self.micros(sent), self.micros(now));
                state.viewer.peer_ping_received(seq, sent_us, arrived_us);
                let pong = Parcel::Pong {
                    to: Side::Sharer,
                    seq,
                    sent,
                };
                self.toward_sharer(&mut state, now, pong);
            }
            Parcel::Ping {
                from: Side::Viewer,
                seq,
                sent,
            } => {
                let mut state = self.lock();
                let pong = Parcel::Pong {
                    to: Side::Viewer,
                    seq,
                    sent,
                };
                self.toward_viewer(&mut state, now, pong, PING_BYTES);
            }
            Parcel::Pong {
                to: Side::Sharer,
                sent,
                ..
            } => {
                let rtt = now.saturating_duration_since(sent);
                let mut state = self.lock();
                while state
                    .sharer_pings
                    .front()
                    .is_some_and(|(at, _)| now.saturating_duration_since(*at) > PINGS_KEPT)
                {
                    state.sharer_pings.pop_front();
                }
                state.sharer_pings.push_back((now, rtt));
                let numbers = &mut state.numbers;
                numbers.rtt_min = if numbers.pings == 0 {
                    rtt
                } else {
                    numbers.rtt_min.min(rtt)
                };
                numbers.rtt_max = numbers.rtt_max.max(rtt);
                numbers.rtt_sum += rtt;
                numbers.pings += 1;
            }
            Parcel::Pong {
                to: Side::Viewer,
                seq,
                sent,
            } => {
                let snapshot = {
                    let mut state = self.lock();
                    state
                        .viewer
                        .pong_received(seq, now.saturating_duration_since(sent), now);
                    state.viewer.snapshot()
                };
                self.link.inbox.set_link(strip(snapshot));
            }
        }
    }

    fn micros(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_micros() as u64
    }
}

// The viewer's strip as a room's watcher has it: its own round trip and
// jitter, "loop" where the path goes, and capture to display's thresholds
// later by half the round trip, the way one way over the internet.
fn strip(snapshot: stats::LinkSnapshot) -> LinkNumbers {
    let thresholds = Thresholds::default();
    LinkNumbers {
        state: LinkState::Live,
        rtt_ms: snapshot.rtt_ms,
        rtt_level: snapshot
            .rtt_ms
            .map_or(stats::Level::Good, |ms| thresholds.rtt_level(ms)),
        jitter_ms: snapshot.jitter_ms,
        jitter_level: snapshot
            .jitter_ms
            .map_or(stats::Level::Good, |ms| thresholds.jitter_level(ms)),
        path: Some(PathWord::Loop),
        trace: snapshot.trace,
        one_way_ms: snapshot.rtt_ms.map(|ms| ms / 2.0),
        ..LinkNumbers::default()
    }
}

// The extra delay one way for a packet going in at `at`: the draw that
// holds then, or a new one.
fn held(
    held: &mut (Instant, Duration),
    draws: &mut Draws,
    mean: Duration,
    at: Instant,
) -> Duration {
    let (until, extra) = held;
    if at >= *until {
        *extra = draws.exponential(mean);
        *until = at + JITTER_HOLDS;
    }
    *extra
}

// SplitMix64, as share's loss knob draws, from the run's seed.
struct Draws(u64);

impl Draws {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn exponential(&mut self, mean: Duration) -> Duration {
        if mean.is_zero() {
            return Duration::ZERO;
        }
        let uniform = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        mean.mul_f64(-(1.0 - uniform).ln())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(capacity_bits: Option<u64>) -> Shape {
        Shape {
            delay: Duration::from_millis(2),
            jitter: Duration::from_millis(6),
            capacity_bits,
            lift_after: None,
            seed: 7,
        }
    }

    #[test]
    fn exponential_draws_have_the_mean_asked_for() {
        let mut draws = Draws(1);
        let mean = Duration::from_millis(6);
        let total: Duration = (0..100_000).map(|_| draws.exponential(mean)).sum();
        let got = total.as_secs_f64() * 1000.0 / 100_000.0;
        assert!((got - 6.0).abs() < 0.1, "{got} ms");
        assert_eq!(draws.exponential(Duration::ZERO), Duration::ZERO);
    }

    // Nothing is reordered, and nothing arrives before the delay.
    #[test]
    fn packets_arrive_in_order_after_the_delay() {
        let link = Arc::new(Link::new(true).expect("an inbox"));
        let network = Network::new(shape(None), link).expect("a network");
        let start = Instant::now();
        let mut state = network.lock();
        for n in 0..1000u32 {
            let at = start + Duration::from_micros(u64::from(n) * 100);
            network.toward_viewer(
                &mut state,
                at,
                Parcel::Video(n.to_le_bytes().to_vec()),
                1200,
            );
        }
        let dues: Vec<Instant> = state.toward_viewer.iter().map(|(due, _)| *due).collect();
        assert!(dues.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(dues[0] >= start + Duration::from_millis(2));
        // The first 50 ms of packets share one draw, so they keep their
        // spacing: a frame's packets arrive together, late or not.
        let first = dues[0] - start;
        for (n, due) in dues.iter().enumerate().take(500) {
            assert_eq!(*due - Duration::from_micros(n as u64 * 100), start + first);
        }
    }

    // At 5 Mbit/s a packet of 1200 bytes and its overhead takes about 2 ms,
    // and once 200 ms wait, the rest is dropped until the queue drains.
    #[test]
    fn the_capacity_queues_packets_and_drops_past_its_depth() {
        let link = Arc::new(Link::new(true).expect("an inbox"));
        let network = Network::new(shape(Some(5_000_000)), link).expect("a network");
        let now = Instant::now();
        let mut state = network.lock();
        for n in 0..200u32 {
            network.toward_viewer(
                &mut state,
                now,
                Parcel::Video(n.to_le_bytes().to_vec()),
                1200,
            );
        }
        let per_packet = (1200.0 * 8.0) / 5_000_000.0;
        let kept = state.toward_viewer.len();
        assert_eq!(kept, (0.2 / per_packet) as usize + 1);
        assert_eq!(state.numbers.queue_dropped, 200 - kept as u64);
        // Lifted after 1 s of sending, nothing waits for the link.
        drop(state);
        let lifted = Shape {
            lift_after: Some(Duration::from_secs(1)),
            ..shape(Some(5_000_000))
        };
        let link = Arc::new(Link::new(true).expect("an inbox"));
        let network = Network::new(lifted, link).expect("a network");
        let mut state = network.lock();
        state.sending_since = Some(now);
        let later = now + Duration::from_secs(1);
        for n in 0..200u32 {
            network.toward_viewer(
                &mut state,
                later,
                Parcel::Video(n.to_le_bytes().to_vec()),
                1200,
            );
        }
        assert_eq!(state.numbers.queue_dropped, 0);
    }
}
