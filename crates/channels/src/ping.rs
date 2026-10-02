use std::collections::VecDeque;

use crate::channel::FrameError;
use crate::reader::Reader;

const KIND_PING: u8 = 0;
const KIND_PONG: u8 = 1;
const PING_LEN: usize = 1 + 4 + 8;
const PONG_LEN: usize = 1 + 4 + 8 * 3;

pub const OFFSET_SAMPLES: usize = 32;

// Times are microseconds on the sender's own monotonic clock, arbitrary
// epoch. In a pong, t1 is echoed from the ping, t2 is when the ping arrived
// and t3 is when the pong left, both on the answering side's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingMessage {
    Ping { seq: u32, t1: u64 },
    Pong { seq: u32, t1: u64, t2: u64, t3: u64 },
}

impl PingMessage {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            PingMessage::Ping { seq, t1 } => {
                out.reserve(PING_LEN);
                out.push(KIND_PING);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(&t1.to_le_bytes());
            }
            PingMessage::Pong { seq, t1, t2, t3 } => {
                out.reserve(PONG_LEN);
                out.push(KIND_PONG);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(&t1.to_le_bytes());
                out.extend_from_slice(&t2.to_le_bytes());
                out.extend_from_slice(&t3.to_le_bytes());
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Result<PingMessage, FrameError> {
        let (&kind, body) = buf.split_first().ok_or(FrameError::Empty)?;
        let expected = match kind {
            KIND_PING => PING_LEN,
            KIND_PONG => PONG_LEN,
            other => return Err(FrameError::UnknownKind(other)),
        };
        let wrong_length = FrameError::Length {
            expected,
            actual: buf.len(),
        };
        if buf.len() != expected {
            return Err(wrong_length);
        }

        let mut body = Reader::new(body);
        let seq = body.u32().ok_or(wrong_length)?;
        let t1 = body.u64().ok_or(wrong_length)?;
        if kind == KIND_PING {
            return Ok(PingMessage::Ping { seq, t1 });
        }
        let t2 = body.u64().ok_or(wrong_length)?;
        let t3 = body.u64().ok_or(wrong_length)?;
        Ok(PingMessage::Pong { seq, t1, t2, t3 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub rtt_us: i64,
    // Peer clock minus ours: our time plus this is the peer's time.
    pub offset_us: i64,
}

// t1 (ping sent) and t4 (pong received) are on our clock, t2 and t3 on the
// peer's. Differences are taken modulo 2^64 and read as signed, so the two
// epochs can be anything, including either clock wrapping, as long as the
// clocks are within 2^63 us of each other.
//
// None when the numbers cannot be true: our clock running backwards, the peer
// claiming its clock ran backwards, or the peer claiming it held the ping
// longer than the whole round trip. A zero round trip is possible on
// loopback and is accepted.
pub fn clock_sample(t1: u64, t2: u64, t3: u64, t4: u64) -> Option<ClockSample> {
    let elapsed = signed_diff(t4, t1);
    let held = signed_diff(t3, t2);
    if elapsed < 0 || held < 0 || held > elapsed {
        return None;
    }
    let sum = i128::from(signed_diff(t2, t1)) + i128::from(signed_diff(t3, t4));
    Some(ClockSample {
        rtt_us: elapsed - held,
        offset_us: i64::try_from(sum / 2).ok()?,
    })
}

fn signed_diff(later: u64, earlier: u64) -> i64 {
    later.wrapping_sub(earlier) as i64
}

// NTP's minimum filter. The sample with the shortest round trip spent the
// least time in queues, so its offset carries the least error.
#[derive(Debug, Clone, Default)]
pub struct OffsetEstimator {
    samples: VecDeque<ClockSample>,
}

impl OffsetEstimator {
    pub fn new() -> OffsetEstimator {
        OffsetEstimator::default()
    }

    pub fn push(&mut self, sample: ClockSample) {
        if self.samples.len() == OFFSET_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    // On a tie the newest sample wins, since the clocks drift apart slowly.
    pub fn best(&self) -> Option<ClockSample> {
        self.samples
            .iter()
            .rev()
            .min_by_key(|sample| sample.rtt_us)
            .copied()
    }

    // For a path change: samples from the old path say nothing about the new.
    pub fn clear(&mut self) {
        self.samples.clear();
    }
}
