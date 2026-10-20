// Between a friend and the host: a UDP relay on loopback that does to each
// packet, each way on its own, what a bad home connection does. Nothing
// overtakes, as on Wi-Fi.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use net::Socket;

use super::loopback;

// How much a home router queues before it drops.
const QUEUE: Duration = Duration::from_millis(200);
// Wi-Fi's delay comes in stretches, so one draw of the jitter holds this
// long for every packet in it.
const JITTER_HOLDS: Duration = Duration::from_millis(50);

// What one direction of the link does to each packet.
#[derive(Clone, Copy, Debug, Default)]
pub struct Shape {
    // Each packet dropped on its own at random.
    pub loss_pct: u32,
    // Every 2 to 6 s everything is dropped for 100 to 500 ms.
    pub bursts: bool,
    // Extra one-way delay, drawn evenly from 0 to this for each stretch of
    // JITTER_HOLDS. Nothing overtakes.
    pub jitter_ms: u64,
    // A link of this rate with a QUEUE of buffer, which drops what does not
    // fit.
    pub capacity_kbps: Option<u64>,
    pub blocked: bool,
}

impl Shape {
    pub fn loss(loss_pct: u32) -> Shape {
        Shape {
            loss_pct,
            ..Shape::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tally {
    pub passed: u64,
    pub random: u64,
    pub burst: u64,
    pub queue: u64,
    pub blocked: u64,
}

impl Tally {
    pub fn since(&self, before: &Tally) -> Tally {
        Tally {
            passed: self.passed - before.passed,
            random: self.random - before.random,
            burst: self.burst - before.burst,
            queue: self.queue - before.queue,
            blocked: self.blocked - before.blocked,
        }
    }

    pub fn lost_pct(&self) -> f64 {
        let lost = self.random + self.burst + self.queue + self.blocked;
        let all = lost + self.passed;
        if all == 0 {
            0.0
        } else {
            lost as f64 * 100.0 / all as f64
        }
    }
}

// xorshift64*, so a run draws the same whatever the timing.
fn roll(dice: &mut u64) -> u64 {
    *dice ^= *dice >> 12;
    *dice ^= *dice << 25;
    *dice ^= *dice >> 27;
    dice.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn between(dice: &mut u64, least: Duration, most: Duration) -> Duration {
    let spread = (most - least).as_micros() as u64;
    least + Duration::from_micros(roll(dice) % (spread + 1))
}

struct Way {
    shape: Shape,
    dice: u64,
    // When the link of capacity_kbps is done with what it holds.
    busy_until: Instant,
    // When the last packet is due, which the next one never beats.
    last_due: Instant,
    // The jitter drawn last, and until when it holds.
    held: (Instant, Duration),
    burst: Option<(Instant, Instant)>,
    tally: Tally,
}

impl Way {
    fn new(seed: u64) -> Way {
        let now = Instant::now();
        Way {
            shape: Shape::default(),
            dice: seed,
            busy_until: now,
            last_due: now,
            held: (now, Duration::ZERO),
            burst: None,
            tally: Tally::default(),
        }
    }

    fn next_burst(&mut self, now: Instant) -> (Instant, Instant) {
        let start = now
            + between(
                &mut self.dice,
                Duration::from_secs(2),
                Duration::from_secs(6),
            );
        let end = start
            + between(
                &mut self.dice,
                Duration::from_millis(100),
                Duration::from_millis(500),
            );
        (start, end)
    }

    // When the packet arrives, or None when the link drops it.
    fn due(&mut self, now: Instant, len: usize) -> Option<Instant> {
        let shape = self.shape;
        if shape.blocked {
            self.tally.blocked += 1;
            return None;
        }
        if shape.bursts {
            let (start, end) = match self.burst {
                Some((_, end)) if now >= end => self.next_burst(now),
                Some(burst) => burst,
                None => self.next_burst(now),
            };
            self.burst = Some((start, end));
            if now >= start {
                self.tally.burst += 1;
                return None;
            }
        } else {
            self.burst = None;
        }
        if shape.loss_pct > 0 && roll(&mut self.dice) % 100 < u64::from(shape.loss_pct) {
            self.tally.random += 1;
            return None;
        }
        let mut arrives = now;
        if let Some(kbps) = shape.capacity_kbps {
            let start = self.busy_until.max(now);
            if start.saturating_duration_since(now) > QUEUE {
                self.tally.queue += 1;
                return None;
            }
            // The IPv4 and UDP headers go through the link too.
            let bits = (len as u64 + 28) * 8;
            self.busy_until = start + Duration::from_micros(bits * 1000 / kbps);
            arrives = self.busy_until;
        }
        if shape.jitter_ms > 0 {
            if now >= self.held.0 {
                let extra = Duration::from_millis(roll(&mut self.dice) % (shape.jitter_ms + 1));
                self.held = (now + JITTER_HOLDS, extra);
            }
            arrives += self.held.1;
        }
        self.last_due = self.last_due.max(arrives);
        self.tally.passed += 1;
        Some(self.last_due)
    }
}

struct Ways {
    stop: AtomicBool,
    to_host: Mutex<Way>,
    to_friend: Mutex<Way>,
    friend: Mutex<Option<SocketAddr>>,
    friend_side: Socket,
    host_side: Socket,
    host: SocketAddr,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// Between the friend and the host: the friend sends to `addr`, and each way
// has a Shape of its own.
pub struct Link {
    pub addr: SocketAddr,
    ways: Arc<Ways>,
    threads: Vec<JoinHandle<()>>,
}

impl Link {
    pub fn new(host: SocketAddr) -> Link {
        let friend_side = Socket::bind(0).expect("bind the link's friend side");
        let addr = loopback(friend_side.local_port());
        let ways = Arc::new(Ways {
            stop: AtomicBool::new(false),
            to_host: Mutex::new(Way::new(0x9E37_79B9_7F4A_7C15)),
            to_friend: Mutex::new(Way::new(0xD1B5_4A32_D192_ED03)),
            friend: Mutex::new(None),
            friend_side,
            host_side: Socket::bind(0).expect("bind the link's host side"),
            host,
        });
        let mut threads = Vec::new();
        for toward_host in [true, false] {
            let (held, due) = mpsc::channel::<(Instant, Vec<u8>)>();
            let reader = Arc::clone(&ways);
            threads.push(thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let (from, way) = if toward_host {
                    (&reader.friend_side, &reader.to_host)
                } else {
                    (&reader.host_side, &reader.to_friend)
                };
                while !reader.stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., sender)) = from.recv_from(&mut buf) else {
                        continue;
                    };
                    if toward_host {
                        *lock(&reader.friend) = Some(sender);
                    }
                    let Some(at) = lock(way).due(Instant::now(), len) else {
                        continue;
                    };
                    if held.send((at, buf[..len].to_vec())).is_err() {
                        break;
                    }
                }
            }));
            let sender = Arc::clone(&ways);
            threads.push(thread::spawn(move || {
                for (at, packet) in due {
                    thread::sleep(at.saturating_duration_since(Instant::now()));
                    if toward_host {
                        let _ = sender.host_side.send_to(&packet, sender.host);
                    } else if let Some(friend) = *lock(&sender.friend) {
                        let _ = sender.friend_side.send_to(&packet, friend);
                    }
                }
            }));
        }
        Link {
            addr,
            ways,
            threads,
        }
    }

    pub fn shape(&self, to_friend: Shape, to_host: Shape) {
        lock(&self.ways.to_friend).shape = to_friend;
        lock(&self.ways.to_host).shape = to_host;
    }

    pub fn tallies(&self) -> (Tally, Tally) {
        (
            lock(&self.ways.to_friend).tally,
            lock(&self.ways.to_host).tally,
        )
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.ways.stop.store(true, Ordering::Release);
        let _ = self.ways.friend_side.wake();
        let _ = self.ways.host_side.wake();
        // The readers end, which ends the senders' channels.
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
