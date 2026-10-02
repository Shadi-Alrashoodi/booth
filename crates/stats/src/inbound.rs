use crate::jitter::{Arrival, Jitter};
use crate::trace::LOSS_WINDOW;

// Sequence numbers remembered behind the newest one. Wider than the loss
// window so a duplicate a little further back is still recognised as one.
// Tied to the bitmap's width so every shift guarded by it stays in range.
const SEEN_WINDOW: u32 = u128::BITS;

const _: () = assert!(
    LOSS_WINDOW < SEEN_WINDOW as usize,
    "inbound loss is read from the seen bitmap, so it must cover the loss window"
);

/// What the peer's own pings say about the path towards us: RFC 3550
/// interarrival jitter and loss from gaps in the peer's sequence numbers.
/// Everything here comes off the wire, so no input may panic or overflow.
#[derive(Debug, Default)]
pub(crate) struct Inbound {
    top: u32,
    // Bit i set: sequence number top - i has arrived.
    seen: u128,
    // Sequence numbers from the oldest one heard up to top. Pings the peer
    // sent before the first one we heard are not counted as lost. Zero until
    // the first ping arrives.
    span: u64,
    jitter: Jitter,
    // The last ping that was too far behind to place. If the very next one
    // follows it closely, the peer has started counting again and we follow,
    // as RFC 3550 appendix A.1 does with bad_seq.
    stray: Option<Stray>,
}

#[derive(Debug, Clone, Copy)]
struct Stray {
    seq: u32,
    arrival: Arrival,
}

enum Place {
    New,
    Known,
    FarBehind,
}

impl Inbound {
    pub fn record(&mut self, seq: u32, peer_sent_us: u64, arrived_us: u64) {
        let arrival = Arrival {
            sent_us: peer_sent_us,
            arrived_us,
        };
        let stray = self.stray.take();
        match self.place(seq) {
            Place::New => {}
            Place::Known => return,
            Place::FarBehind => match stray {
                Some(stray) if follows(stray.seq, seq) => self.restart(stray, seq),
                _ => {
                    self.stray = Some(Stray { seq, arrival });
                    return;
                }
            },
        }
        self.jitter.add(arrival);
    }

    pub fn jitter_ms(&self) -> Option<f32> {
        self.jitter.ms()
    }

    pub fn loss_pct(&self) -> Option<f32> {
        if self.span == 0 {
            return None;
        }
        let expected = self.span.min(LOSS_WINDOW as u64) as u32;
        let mask = (1u128 << expected) - 1;
        let received = (self.seen & mask).count_ones();
        let lost = expected.saturating_sub(received);
        Some(lost as f32 * 100.0 / expected as f32)
    }

    // Known covers a duplicate, so it can neither move the jitter nor be
    // counted twice.
    fn place(&mut self, seq: u32) -> Place {
        if self.span == 0 {
            self.top = seq;
            self.seen = 1;
            self.span = 1;
            return Place::New;
        }

        let ahead = seq.wrapping_sub(self.top);
        if ahead != 0 && ahead < 0x8000_0000 {
            self.seen = if ahead >= SEEN_WINDOW {
                0
            } else {
                self.seen << ahead
            };
            self.seen |= 1;
            self.top = seq;
            self.span = self.span.saturating_add(u64::from(ahead));
            return Place::New;
        }

        let behind = self.top.wrapping_sub(seq);
        if behind >= SEEN_WINDOW {
            return Place::FarBehind;
        }
        let bit = 1u128 << behind;
        if self.seen & bit != 0 {
            return Place::Known;
        }
        self.seen |= bit;
        self.span = self.span.max(u64::from(behind) + 1);
        Place::New
    }

    // The old count is gone for good, so its history goes too. The stray is
    // the new stream's first ping heard, which makes it the jitter's previous
    // arrival and the start of the loss span.
    fn restart(&mut self, stray: Stray, seq: u32) {
        let ahead = seq.wrapping_sub(stray.seq);
        self.top = seq;
        self.seen = 1 | (1u128 << ahead);
        self.span = u64::from(ahead) + 1;
        self.jitter.follow_from(stray.arrival);
    }
}

fn follows(earlier: u32, later: u32) -> bool {
    let ahead = later.wrapping_sub(earlier);
    ahead != 0 && ahead < SEEN_WINDOW
}
