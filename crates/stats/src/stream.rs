use std::collections::VecDeque;
use std::ops::Add;
use std::time::{Duration, Instant};

use crate::jitter::{Arrival, Jitter};

/// Loss on a media channel is counted over the packets that arrived in the
/// last this long, and a stream heard within it is one that flows.
pub const STREAM_WINDOW: Duration = Duration::from_secs(2);

// A jump further ahead than this is the sender counting from somewhere new,
// not loss: 5 s of 5 ms voice frames, longer than a silence the room waits
// out before it calls a peer reconnecting.
const MAX_GAP: i64 = 1000;

// Sequence numbers remembered behind the newest one, for a packet that comes
// late or twice: far more than any jitter buffer waits. Tied to the bitmap's
// width so every shift guarded by it stays in range.
const SEEN: i64 = u128::BITS as i64;

// 2 s of the host's cap on one friend's voice, 250 frames a second, fits
// twice over. This only bounds a flood.
const MAX_EVENTS: usize = 1024;

/// What a stream lost over the window, counted so several streams can be
/// added up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamLoss {
    pub lost: u32,
    pub expected: u32,
}

impl Add for StreamLoss {
    type Output = StreamLoss;

    fn add(self, other: StreamLoss) -> StreamLoss {
        StreamLoss {
            lost: self.lost.saturating_add(other.lost),
            expected: self.expected.saturating_add(other.expected),
        }
    }
}

impl StreamLoss {
    pub fn percent(&self) -> Option<f32> {
        (self.expected > 0)
            .then(|| self.lost.min(self.expected) as f32 * 100.0 / self.expected as f32)
    }
}

/// One media stream as it arrives, for the strip while media flows: RFC 3550
/// interarrival jitter over its packets' send and arrival times, and loss from
/// gaps in its sequence numbers over the last STREAM_WINDOW. Everything here
/// comes off the wire, so no input may panic or overflow.
#[derive(Debug, Default)]
pub struct StreamStats {
    // The newest sequence number heard, extended past the 16 bits on the
    // wire so it never wraps.
    top: Option<i64>,
    // Bit i set: top - i has arrived.
    seen: u128,
    events: VecDeque<Event>,
    jitter: Jitter,
    // The last packet too far behind to place. If the very next one follows
    // it closely, the sender has started counting again and we follow, as
    // RFC 3550 appendix A.1 does with bad_seq.
    stray: Option<u16>,
}

// One packet that arrived, and the numbers skipped just before it that are
// still missing.
#[derive(Debug, Clone, Copy)]
struct Event {
    at: Instant,
    seq: i64,
    missing_from: i64,
    missing: u32,
}

enum Place {
    Ahead {
        seq: i64,
        missing_from: i64,
        missing: u32,
    },
    Filled(i64),
    Known,
    FarBehind(i64),
}

impl StreamStats {
    pub fn new() -> StreamStats {
        StreamStats::default()
    }

    /// `sent_us` is when the packet's content was captured and `arrived_us`
    /// when it arrived, both in microseconds, None when the send time cannot
    /// be told yet. Only differences are used, so the two need not share a
    /// clock and either may wrap. `at` is the arrival on the monotonic clock,
    /// for the window.
    pub fn record(&mut self, seq: u16, sent_us: Option<u64>, arrived_us: u64, at: Instant) {
        self.forget_before(at);
        let stray = self.stray.take();
        let event = match self.place(seq) {
            Place::Ahead {
                seq,
                missing_from,
                missing,
            } => Event {
                at,
                seq,
                missing_from,
                missing,
            },
            Place::Filled(seq) => {
                self.fill(seq);
                Event::received(at, seq)
            }
            Place::Known => return,
            Place::FarBehind(extended) => match stray {
                Some(earlier) if follows(earlier, seq) => {
                    self.top = Some(extended);
                    self.seen = 1;
                    Event::received(at, extended)
                }
                _ => {
                    self.stray = Some(seq);
                    return;
                }
            },
        };
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
        if let Some(sent_us) = sent_us {
            self.jitter.add(Arrival {
                sent_us,
                arrived_us,
            });
        }
    }

    pub fn jitter_ms(&self) -> Option<f32> {
        self.jitter.ms()
    }

    /// Over the packets that arrived in the last STREAM_WINDOW. None when
    /// none did: the stream does not flow.
    pub fn loss(&self, now: Instant) -> Option<StreamLoss> {
        let (received, lost) = self
            .events
            .iter()
            .filter(|event| now.saturating_duration_since(event.at) < STREAM_WINDOW)
            .fold((0u32, 0u32), |(received, lost), event| {
                (received + 1, lost.saturating_add(event.missing))
            });
        (received > 0).then_some(StreamLoss {
            lost,
            expected: lost.saturating_add(received),
        })
    }

    fn place(&mut self, seq: u16) -> Place {
        let Some(top) = self.top else {
            let seq = i64::from(seq);
            self.top = Some(seq);
            self.seen = 1;
            return Place::Ahead {
                seq,
                missing_from: seq,
                missing: 0,
            };
        };
        let extended = top + i64::from(seq.wrapping_sub(top as u16) as i16);
        let ahead = extended - top;
        if ahead > 0 {
            let kept = if ahead >= SEEN { 0 } else { self.seen << ahead };
            self.seen = kept | 1;
            self.top = Some(extended);
            let missing = if ahead <= MAX_GAP { ahead - 1 } else { 0 };
            return Place::Ahead {
                seq: extended,
                missing_from: top + 1,
                missing: missing as u32,
            };
        }
        let behind = -ahead;
        if behind >= SEEN {
            return Place::FarBehind(extended);
        }
        let bit = 1u128 << behind;
        if self.seen & bit != 0 {
            return Place::Known;
        }
        self.seen |= bit;
        Place::Filled(extended)
    }

    // A late packet: the gap it was counted in, if still in the window, has
    // one fewer missing.
    fn fill(&mut self, seq: i64) {
        if let Some(event) = self
            .events
            .iter_mut()
            .rev()
            .find(|event| event.missing > 0 && (event.missing_from..event.seq).contains(&seq))
        {
            event.missing -= 1;
        }
    }

    fn forget_before(&mut self, now: Instant) {
        while self
            .events
            .front()
            .is_some_and(|event| now.saturating_duration_since(event.at) >= STREAM_WINDOW)
        {
            self.events.pop_front();
        }
    }
}

impl Event {
    fn received(at: Instant, seq: i64) -> Event {
        Event {
            at,
            seq,
            missing_from: seq,
            missing: 0,
        }
    }
}

fn follows(earlier: u16, later: u16) -> bool {
    let ahead = i64::from(later.wrapping_sub(earlier));
    ahead != 0 && ahead < SEEN
}
