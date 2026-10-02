use crate::link::LOST_AFTER;

// The biggest change in transit time one pair of packets may add to the
// jitter. On our side a ping out longer than LOST_AFTER already counts as
// lost, so a larger swing says more about the sender's timestamps than about
// the path. Without a cap one wild timestamp would hold the number up for
// minutes.
const MAX_SWING_US: u64 = LOST_AFTER.as_micros() as u64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Arrival {
    pub sent_us: u64,
    pub arrived_us: u64,
}

/// RFC 3550 interarrival jitter. Each packet is compared with the one before
/// it in arrival order, not sequence order, so a reordered packet still
/// counts. The send and arrival times may be on clocks that do not agree;
/// only differences are used.
#[derive(Debug, Default)]
pub(crate) struct Jitter {
    previous: Option<Arrival>,
    us: Option<f64>,
}

impl Jitter {
    pub fn add(&mut self, arrival: Arrival) {
        if let Some(previous) = self.previous.replace(arrival) {
            let d = transit_change(previous, arrival) as f64;
            let j = self.us.unwrap_or(0.0);
            self.us = Some(j + (d - j) / 16.0);
        }
    }

    // The sender started counting again: the next packet is compared with
    // this one, and the number so far stands.
    pub fn follow_from(&mut self, arrival: Arrival) {
        self.previous = Some(arrival);
    }

    pub fn ms(&self) -> Option<f32> {
        self.us.map(|us| (us / 1000.0) as f32)
    }
}

// Both clocks start anywhere and may wrap, so differences are taken modulo
// 2^64 and read as signed, the same way the ping channel reads them.
fn transit_change(previous: Arrival, arrival: Arrival) -> u64 {
    let ours = i128::from(arrival.arrived_us.wrapping_sub(previous.arrived_us) as i64);
    let theirs = i128::from(arrival.sent_us.wrapping_sub(previous.sent_us) as i64);
    (ours - theirs).unsigned_abs().min(u128::from(MAX_SWING_US)) as u64
}
