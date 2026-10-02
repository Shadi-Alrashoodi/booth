use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

mod wire;

#[cfg(test)]
mod tests;

use crate::rtt::{MAX_TIMEOUT, MIN_TIMEOUT};
use wire::{Ack, Frame};

// A data frame plus the channel byte and the session's 32 bytes of overhead
// stays under a 1200-byte datagram.
pub const MAX_MESSAGE: usize = 1100;
pub const MAX_FRAME: usize = wire::DATA_HEADER + MAX_MESSAGE;
const _: () = assert!(1 + MAX_FRAME + 32 <= 1200);
pub const WINDOW: usize = 64;
pub const MAX_QUEUED: usize = 1024;

const TIMEOUT_UNKNOWN_RTT: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
// One ack covers at most a window of messages, and the caller takes the
// delays after every frame it hands in, so this is only reached by a caller
// that never takes them.
const MAX_ACK_DELAYS: usize = 2 * WINDOW;

#[derive(Debug)]
struct Sent {
    message: Vec<u8>,
    // When it first went out, for the ack delay.
    first_sent: Instant,
    deadline: Instant,
    // Reset when the path comes back; `resent` is not.
    retransmits: u32,
    resent: bool,
    acked: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReliableCounters {
    // New messages put on the wire, retransmissions not included.
    pub sent: u64,
    pub retransmissions: u64,
    pub in_flight: usize,
    // Accepted by send but waiting for room in the window.
    pub queued: usize,
}

#[derive(Debug)]
pub struct Reliable {
    queue: VecDeque<Vec<u8>>,
    // in_flight[i] carries sequence send_base + i. It runs from the oldest
    // message not known to be received to the newest one sent, so its length
    // is the span the window limits, not the count of unacked messages.
    in_flight: VecDeque<Sent>,
    send_base: u32,
    // The timeout from the last poll_transmit. path_recovered needs one and
    // is not given one.
    last_base: Duration,

    expected: u32,
    // Received but not yet delivered, at slot seq % WINDOW. Only sequences
    // in [expected, expected + WINDOW) are ever held, so slots never collide,
    // and WINDOW divides 2^32 so the slot stays right across wraparound.
    // expected itself is only held while delivered is full.
    held: [Option<Vec<u8>>; WINDOW],
    delivered: VecDeque<Vec<u8>>,
    ack_owed: bool,

    sent: u64,
    retransmissions: u64,
    // From first send to ack, on our clock, for messages acked without a
    // retransmit. An ack for a resent one could answer either copy (Karn).
    ack_delays: Vec<Duration>,
}

impl Default for Reliable {
    fn default() -> Reliable {
        Reliable::new()
    }
}

impl Reliable {
    /// Both sides create theirs when the session starts and drop it with the
    /// session. If only one side starts over (a restart, a rejoin that kept
    /// the old one), the two disagree about sequence numbers for good: the
    /// side that kept its state sees nothing wrong, the fresh side gets
    /// BadAck or OutOfWindow for the other side's frames.
    pub fn new() -> Reliable {
        Reliable::starting_at(0)
    }

    // Both sides must start at the same sequence.
    pub(crate) fn starting_at(seq: u32) -> Reliable {
        Reliable {
            queue: VecDeque::new(),
            in_flight: VecDeque::with_capacity(WINDOW),
            send_base: seq,
            last_base: TIMEOUT_UNKNOWN_RTT,
            expected: seq,
            held: [const { None }; WINDOW],
            delivered: VecDeque::new(),
            ack_owed: false,
            sent: 0,
            retransmissions: 0,
            ack_delays: Vec::new(),
        }
    }

    pub fn send(&mut self, message: &[u8]) -> Result<(), ReliableError> {
        if message.len() > MAX_MESSAGE {
            return Err(ReliableError::TooBig(message.len()));
        }
        if self.queue.len() + self.in_flight.len() >= MAX_QUEUED {
            return Err(ReliableError::Full);
        }
        self.queue.push_back(message.to_vec());
        Ok(())
    }

    pub fn receive(&mut self, frame: &[u8], now: Instant) -> Result<(), ReliableError> {
        let frame = wire::parse(frame)?;
        // Check everything before touching any state, so a bad frame is
        // dropped whole.
        let ack = frame.ack();
        self.check_ack(ack)?;
        if let Frame::Data { seq, .. } = frame {
            self.check_seq(seq)?;
        }

        let mut news = self.apply_ack(ack, now);
        if let Frame::Data { seq, message, .. } = frame {
            news |= self.accept(seq, message);
        }
        // Only a frame that tells us something new counts as the path
        // working. A repeat of the last ack is also what a receiver whose
        // application stopped reading sends back, and resetting on that would
        // turn the backoff into a steady stream of retransmissions.
        if news {
            self.path_recovered(now);
        }
        Ok(())
    }

    /// Call when the path to the peer works again after an outage: the first
    /// packet after the peer went quiet, or from its new address. Messages
    /// whose retransmissions backed off during the outage go out one normal
    /// timeout from now instead of up to 2 s later. Not for every packet:
    /// while the peer's application is not reading, the backoff is what keeps
    /// retransmissions down to one every 2 s.
    pub fn path_recovered(&mut self, now: Instant) {
        let deadline = now + self.last_base;
        for sent in &mut self.in_flight {
            if !sent.acked && sent.retransmits > 0 {
                sent.deadline = sent.deadline.min(deadline);
                sent.retransmits = 0;
            }
        }
    }

    /// Taking a message can make room for one that was waiting, which owes
    /// the peer an ack, so call poll_transmit after reading.
    pub fn next_delivered(&mut self) -> Option<Vec<u8>> {
        let message = self.delivered.pop_front()?;
        if self.release() {
            self.ack_owed = true;
        }
        Some(message)
    }

    /// `timeout` is the retransmit timeout from an RttEstimator, None while
    /// no round trip is known.
    pub fn poll_transmit(&mut self, now: Instant, timeout: Option<Duration>) -> Option<Vec<u8>> {
        // The floor also stops a zero from sending the same frame again on
        // every poll, which would never end a poll_transmit loop.
        let base = timeout.map_or(TIMEOUT_UNKNOWN_RTT, |timeout| {
            timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT)
        });
        self.last_base = base;
        let ack = self.current_ack();

        let due = self
            .in_flight
            .iter()
            .position(|sent| !sent.acked && sent.deadline <= now);
        if let Some(index) = due {
            let seq = self.send_base.wrapping_add(index as u32);
            let sent = self.in_flight.get_mut(index)?;
            sent.retransmits = sent.retransmits.saturating_add(1);
            sent.resent = true;
            sent.deadline = now + backoff(base, sent.retransmits);
            self.retransmissions += 1;
            self.ack_owed = false;
            return Some(wire::data(ack, seq, &sent.message));
        }

        if self.in_flight.len() < WINDOW
            && let Some(message) = self.queue.pop_front()
        {
            let seq = self.send_base.wrapping_add(self.in_flight.len() as u32);
            let frame = wire::data(ack, seq, &message);
            self.in_flight.push_back(Sent {
                message,
                first_sent: now,
                deadline: now + base,
                retransmits: 0,
                resent: false,
                acked: false,
            });
            self.sent += 1;
            self.ack_owed = false;
            return Some(frame);
        }

        if self.ack_owed {
            self.ack_owed = false;
            return Some(wire::ack_only(ack));
        }
        None
    }

    /// The ack delays recorded since the last call: from a message's first
    /// send to the frame that acked it, for messages that were never resent.
    /// Take them after every receive.
    pub fn ack_delays(&mut self) -> std::vec::Drain<'_, Duration> {
        self.ack_delays.drain(..)
    }

    pub fn next_timeout(&self) -> Option<Instant> {
        self.in_flight
            .iter()
            .filter(|sent| !sent.acked)
            .map(|sent| sent.deadline)
            .min()
    }

    pub fn counters(&self) -> ReliableCounters {
        ReliableCounters {
            sent: self.sent,
            retransmissions: self.retransmissions,
            in_flight: self.in_flight.iter().filter(|sent| !sent.acked).count(),
            queued: self.queue.len(),
        }
    }

    /// The messages the peer has not acked, oldest first: those in flight,
    /// then those waiting for the window. For a caller that drops this
    /// stream and wants them to go on the next one. The peer may already
    /// have some of them, with only the ack lost.
    pub fn unacked(&self) -> impl Iterator<Item = &[u8]> {
        self.in_flight
            .iter()
            .filter(|sent| !sent.acked)
            .map(|sent| sent.message.as_slice())
            .chain(self.queue.iter().map(Vec::as_slice))
    }

    fn current_ack(&self) -> Ack {
        let mut bits = 0u64;
        // expected + WINDOW shares a slot with expected, so the last bit is
        // never set.
        for i in 0..WINDOW as u32 - 1 {
            if self.held[slot(self.expected.wrapping_add(1 + i))].is_some() {
                bits |= 1 << i;
            }
        }
        Ack {
            next: self.expected,
            bits,
        }
    }

    // A correct peer only acks what we sent. An ack older than send_base is
    // normal (reordering) and its cumulative part is ignored; one past our
    // newest sequence means the two sides disagree about the stream.
    fn check_ack(&self, ack: Ack) -> Result<(), ReliableError> {
        let span = self.in_flight.len() as u32;
        let bad = |acked: u32| ReliableError::BadAck {
            acked,
            next_to_send: self.send_base.wrapping_add(span),
        };
        let cumulative = ack.next.wrapping_sub(self.send_base);
        if !is_behind(cumulative) && cumulative > span {
            return Err(bad(ack.next.wrapping_sub(1)));
        }
        for seq in selective(ack) {
            let offset = seq.wrapping_sub(self.send_base);
            if !is_behind(offset) && offset >= span {
                return Err(bad(seq));
            }
        }
        Ok(())
    }

    // Returns whether anything was acked for the first time.
    fn apply_ack(&mut self, ack: Ack, now: Instant) -> bool {
        let mut news = false;
        let delays = &mut self.ack_delays;
        let mut acked = |sent: &mut Sent| {
            if sent.acked {
                return;
            }
            sent.acked = true;
            news = true;
            if !sent.resent && delays.len() < MAX_ACK_DELAYS {
                delays.push(now.saturating_duration_since(sent.first_sent));
            }
        };
        let cumulative = ack.next.wrapping_sub(self.send_base);
        if !is_behind(cumulative) {
            for sent in self.in_flight.iter_mut().take(cumulative as usize) {
                acked(sent);
            }
        }
        for seq in selective(ack) {
            let offset = seq.wrapping_sub(self.send_base);
            if !is_behind(offset)
                && let Some(sent) = self.in_flight.get_mut(offset as usize)
            {
                acked(sent);
            }
        }
        while self.in_flight.front().is_some_and(|sent| sent.acked) {
            self.in_flight.pop_front();
            self.send_base = self.send_base.wrapping_add(1);
        }
        news
    }

    // The sender never goes more than WINDOW past the oldest message it has
    // not seen acked, and that is never past our expected, so anything
    // further ahead is a broken or hostile peer.
    fn check_seq(&self, seq: u32) -> Result<(), ReliableError> {
        let offset = seq.wrapping_sub(self.expected);
        if !is_behind(offset) && offset >= WINDOW as u32 {
            return Err(ReliableError::OutOfWindow(seq));
        }
        Ok(())
    }

    // Returns whether the message was new to us.
    fn accept(&mut self, seq: u32, message: &[u8]) -> bool {
        // Every data frame is acked, duplicates too: a duplicate usually
        // means our last ack was lost.
        self.ack_owed = true;
        if is_behind(seq.wrapping_sub(self.expected)) {
            return false;
        }
        let held = &mut self.held[slot(seq)];
        if held.is_some() {
            return false;
        }
        *held = Some(message.to_vec());
        self.release();
        true
    }

    // If the application stops reading, once MAX_QUEUED are waiting the next
    // message stays in its slot, unacked, so the sender backs off instead of
    // this side growing without limit. Reading makes room and the ack that
    // follows reopens the sender's window at once. The run held behind that
    // message must go with it: the sender has their selective acks and has
    // moved its window past them, so leaving any behind would put its next
    // frames outside ours.
    fn release(&mut self) -> bool {
        if self.delivered.len() >= MAX_QUEUED {
            return false;
        }
        let before = self.expected;
        while let Some(message) = self.held[slot(self.expected)].take() {
            self.delivered.push_back(message);
            self.expected = self.expected.wrapping_add(1);
        }
        self.expected != before
    }
}

// Serial number arithmetic (RFC 1982): an offset between two sequences reads
// as negative when the first is older.
fn is_behind(offset: u32) -> bool {
    (offset as i32) < 0
}

fn selective(ack: Ack) -> impl Iterator<Item = u32> {
    let mut bits = ack.bits;
    std::iter::from_fn(move || {
        if bits == 0 {
            return None;
        }
        let bit = bits.trailing_zeros();
        bits &= bits - 1;
        Some(ack.next.wrapping_add(1 + bit))
    })
}

fn slot(seq: u32) -> usize {
    seq as usize % WINDOW
}

fn backoff(base: Duration, retransmits: u32) -> Duration {
    base.saturating_mul(1 << retransmits.min(16))
        .min(BACKOFF_MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliableError {
    TooBig(usize),
    Full,
    Length(usize),
    UnknownKind(u8),
    // The peer acked a sequence we never sent. Both numbers go in the
    // message so a log line shows which side's state is stale.
    BadAck { acked: u32, next_to_send: u32 },
    OutOfWindow(u32),
}

impl fmt::Display for ReliableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReliableError::TooBig(len) => {
                write!(f, "message is {len} bytes, the limit is {MAX_MESSAGE}")
            }
            ReliableError::Full => write!(f, "send queue is full at {MAX_QUEUED} messages"),
            ReliableError::Length(len) => write!(f, "reliable frame of {len} bytes is malformed"),
            ReliableError::UnknownKind(kind) => write!(f, "unknown reliable frame kind {kind}"),
            ReliableError::BadAck {
                acked,
                next_to_send,
            } => write!(
                f,
                "ack covers sequence {acked} but the next one to send is {next_to_send}"
            ),
            ReliableError::OutOfWindow(seq) => {
                write!(f, "sequence {seq} is outside the receive window")
            }
        }
    }
}

impl std::error::Error for ReliableError {}
