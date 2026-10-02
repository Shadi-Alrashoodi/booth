// The viewer's side. A frame is ready once it has as many shards as it has
// data shards, any mix; parity is only worked through when a data shard is
// missing. Frames come out in frame number order and nothing waits in a
// queue: a frame that is not ready waits one frame interval (10 ms at least)
// from its latest packet, and is dropped the moment a later frame is ready
// first. After a drop, frames that cannot be decoded without the lost one are
// held back until the next IDR: always after a lost IDR, and after any other
// frame when the sharer's encoder cannot invalidate references.
//
// There are no timers in here. Every call takes the time, next_deadline says
// when expire must be called, and the caller waits for that on the
// high-resolution timer.
//
// Rebuilding a frame from parity takes about 0.13 ms in a release build
// whatever the frame's size, since the decoder transforms over all 65536
// points of its field each time. A friend's PC can send packets that each
// make a one-shard frame need its parity and keep the thread that runs this
// busy, so that thread must not be the one voice and input go through.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;
use std::time::{Duration, Instant};

use reed_solomon_simd::ReedSolomonDecoder;

use super::loss::{LossWindow, VideoLoss};
use super::wire::{FrameFacts, MAX_DATA, Packet, PacketError, read_flags, read_frame, read_packet};

// Frames that have packets and are neither out nor dropped. At 120 fps a
// frame that waits its full 10 ms has the next frame beside it and the one
// after that on its way; four leave room for a jittery path.
pub const MAX_PENDING: usize = 4;
// Bytes of shards held for those frames. A frame holds the shards that
// arrived, one after another, and nothing for the ones its packets only
// claim, so a packet claiming a new frame of the largest size the format
// allows (2048 data and 2048 parity shards of 1344 bytes, 5.5 MB) costs the
// work of its own bytes. A frame comes out once it has as many shards as it
// has data shards, so it never holds more than 2.76 MB and any one frame
// fits. Buffers grow only as shards go into them, so allocation follows the
// bytes that came, and buffers of frames gone are kept for reuse up to as
// much again: under 40 MB at the very most, a few hundred KB for a real
// stream.
pub const MAX_HELD_BYTES: usize = 8 << 20;

// Each shard a frame holds is kept after its index, a u16.
const INDEX: usize = 2;

pub const SHORTEST_WAIT: Duration = Duration::from_millis(10);

// After a recover request, drops in the next this long go out together as
// one more request when it ends. The first drop after a quiet spell goes at
// once, since the sooner the sharer hears, the sooner the picture is whole.
pub const RECOVER_GAP: Duration = Duration::from_millis(20);

// Frame numbers ahead of the next one to come out that are taken as the
// stream going on: 8.5 s at 120 fps, more than the 3 s after which the room
// calls a peer reconnecting. Further ahead is a stray, unless the packet
// after it is close to it, which means the stream really moved there. The
// sharer's numbers only go forward, so packets from behind never move the
// stream back, however many come, as after a path change that delivers a
// few from long ago: they are late.
const AHEAD: u32 = 1024;
// Before the first frame is out or dropped, a packet up to this far behind
// the first one heard moves the start back to it.
const BEHIND: u32 = 64;
// How close the packet after a stray must be to it to follow it.
const CLOSE: u32 = 4;

// Resolved frames whose late shards still count as received, so parity that
// arrives after its frame came out is not counted as lost.
const RECENT: usize = 8;

const MAX_SHARDS: usize = 2 * MAX_DATA as usize;
type Bits = [u64; MAX_SHARDS / 64];

fn has(bits: &Bits, index: usize) -> bool {
    bits[index / 64] & (1 << (index % 64)) != 0
}

fn mark(bits: &mut Bits, index: usize) {
    bits[index / 64] |= 1 << (index % 64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    // Its shard is new and was kept.
    Kept,
    Duplicate,
    // For a frame that came out whole without it: parity it did not need,
    // or a data shard parity stood in for. With no loss, every parity shard
    // ends up here.
    Surplus,
    // For a frame already dropped, or one too old to remember.
    Late,
    // Too far from the frames in play to be part of the stream.
    Far,
    // Broke the format; counted as a protocol error.
    Refused(PacketError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    // Its wait ran out.
    Deadline,
    // A later frame was ready first, or ran out of time first.
    Overtaken,
    // Too many frames or bytes in play; the oldest goes.
    Memory,
    // Its packets or its header broke the format.
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub facts: FrameFacts,
    pub access_unit: &'a [u8],
    // A data shard was missing and parity stood in for it.
    pub repaired: bool,
    // From the frame's first packet to the one that made it ready.
    pub assembly: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    Frame(Frame<'a>),
    // Frames `first` to `last`, wrapping, dropped for the same reason.
    Dropped {
        first: u32,
        last: u32,
        why: DropReason,
    },
    // For the sharer: the oldest and newest frame dropped since the last
    // request. It invalidates or sends an IDR from `first` on.
    Recover {
        first: u32,
        last: u32,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoNumbers {
    pub delivered: u64,
    // Delivered frames that needed parity.
    pub repaired: u64,
    pub dropped_deadline: u64,
    pub dropped_overtaken: u64,
    pub dropped_memory: u64,
    pub dropped_refused: u64,
    // Frames that were whole but held back while waiting for an IDR.
    pub skipped: u64,
    pub shards_received: u64,
    pub shards_lost: u64,
    pub duplicates: u64,
    pub surplus: u64,
    pub late: u64,
    pub far: u64,
    // Times the stream was followed to frame numbers far from the old ones.
    pub restarts: u64,
    pub protocol_errors: u64,
    pub recover_requests: u64,
}

impl VideoNumbers {
    pub fn dropped(&self) -> u64 {
        self.dropped_deadline + self.dropped_overtaken + self.dropped_memory + self.dropped_refused
    }
}

struct Pending {
    number: u32,
    // The counts and shard length from the frame's first packet; every later
    // packet must match all three. The shard length is the frame's own, as
    // it follows the frame's size.
    data: u16,
    parity: u16,
    shard: usize,
    have: Bits,
    received: u16,
    data_received: u16,
    // Each shard kept after its index, in the order they came.
    stored: Vec<u8>,
    first_at: Instant,
    last_at: Instant,
    // (IDR, survives loss) from data shard 0, before the frame is whole.
    flags: Option<(bool, bool)>,
    refused: bool,
}

impl Pending {
    fn total(&self) -> usize {
        usize::from(self.data) + usize::from(self.parity)
    }

    fn ready(&self) -> bool {
        !self.refused && self.received >= self.data
    }

    fn shards(&self) -> impl Iterator<Item = (usize, &[u8])> {
        self.stored.chunks_exact(INDEX + self.shard).map(|kept| {
            let (index, bytes) = kept.split_at(INDEX);
            (usize::from(u16::from_le_bytes([index[0], index[1]])), bytes)
        })
    }
}

struct Recent {
    number: u32,
    counts: (u16, u16, usize),
    have: Bits,
    // It had enough shards and came out, or was held back for an IDR.
    whole: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    No,
    // A dropped frame broke the ones after it.
    UntilIdr,
    // A frame was dropped before the header of any frame but an IDR said
    // what the encoder does after a loss. The next header says: an encoder
    // that invalidates references would never send the IDR a hold waits for.
    UntilKnown,
}

enum Queued {
    Frame {
        facts: FrameFacts,
        range: Range<usize>,
        repaired: bool,
        assembly: Duration,
    },
    Dropped {
        first: u32,
        last: u32,
        why: DropReason,
    },
    Recover {
        first: u32,
        last: u32,
    },
}

pub struct Reassembler {
    wait: Duration,
    // The oldest frame number neither out nor dropped. None until the first
    // packet, which is where the stream starts.
    next: Option<u32>,
    // Until a frame is out or dropped, a packet from a little before `next`
    // moves the start back to it: the first packet heard need not be from
    // the first frame sent.
    started: bool,
    // Ordered by frame number from `next`.
    pending: Vec<Pending>,
    // Bytes stored for the pending frames.
    held: usize,
    spare: Vec<Vec<u8>>,
    // The data shards of the frame put together last, in order. It grows to
    // the largest frame put together, which had at least that many bytes
    // arrive.
    out: Vec<u8>,
    events: VecDeque<Queued>,
    hold: Hold,
    // What the newest header of a frame that is not an IDR said, for a
    // dropped frame that never showed its own. An IDR's header is left out:
    // losing an IDR says nothing about losing the frames after it. None
    // before any such header has been read.
    survives_loss: Option<bool>,
    gathered: Option<(u32, u32)>,
    last_recover: Option<Instant>,
    stray: Option<u32>,
    recent: VecDeque<Recent>,
    loss: LossWindow,
    shards_expected: u64,
    numbers: VideoNumbers,
    decoder: Option<ReedSolomonDecoder>,
}

impl Reassembler {
    // `interval` is the sharer's frame interval. One for each share: frame
    // numbers that start over, as a new share's may, are behind the old
    // ones, and every packet of them would be late.
    pub fn new(interval: Duration) -> Reassembler {
        Reassembler {
            wait: interval.max(SHORTEST_WAIT),
            next: None,
            started: false,
            pending: Vec::with_capacity(MAX_PENDING + 1),
            held: 0,
            spare: Vec::new(),
            out: Vec::new(),
            events: VecDeque::new(),
            hold: Hold::No,
            survives_loss: None,
            gathered: None,
            last_recover: None,
            stray: None,
            recent: VecDeque::with_capacity(RECENT),
            loss: LossWindow::default(),
            shards_expected: 0,
            numbers: VideoNumbers::default(),
            decoder: None,
        }
    }

    pub fn set_interval(&mut self, interval: Duration) {
        self.wait = interval.max(SHORTEST_WAIT);
    }

    // Events from the call before are gone once this is called.
    pub fn push(&mut self, packet: &[u8], now: Instant) -> Arrival {
        self.events.clear();
        // A frame whose wait ran out before this packet came is gone, however
        // late the caller's timer was.
        self.run(now);
        let arrival = self.take(packet, now);
        self.run(now);
        arrival
    }

    pub fn expire(&mut self, now: Instant) {
        self.events.clear();
        self.run(now);
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let frames = self
            .pending
            .iter()
            .map(|pending| pending.last_at + self.wait);
        let recover = self
            .gathered
            .and(self.last_recover)
            .map(|at| at + RECOVER_GAP);
        frames.chain(recover).min()
    }

    // What the last push or expire produced, oldest first. A frame's bytes
    // stay valid until the next push or expire.
    pub fn event(&mut self) -> Option<Event<'_>> {
        Some(match self.events.pop_front()? {
            Queued::Frame {
                facts,
                range,
                repaired,
                assembly,
            } => Event::Frame(Frame {
                facts,
                access_unit: &self.out[range],
                repaired,
                assembly,
            }),
            Queued::Dropped { first, last, why } => Event::Dropped { first, last, why },
            Queued::Recover { first, last } => Event::Recover { first, last },
        })
    }

    pub fn numbers(&self) -> VideoNumbers {
        VideoNumbers {
            shards_lost: self
                .shards_expected
                .saturating_sub(self.numbers.shards_received),
            ..self.numbers
        }
    }

    // Shard loss over the last 2 s, for the parity rule and the stats panel.
    pub fn loss(&self, now: Instant) -> VideoLoss {
        self.loss.loss(now)
    }

    fn run(&mut self, now: Instant) {
        self.advance(now);
        if let Some((first, last)) = self.gathered {
            let quiet = self
                .last_recover
                .is_none_or(|at| now.saturating_duration_since(at) >= RECOVER_GAP);
            if quiet {
                self.events.push_back(Queued::Recover { first, last });
                self.gathered = None;
                self.last_recover = Some(now);
                self.numbers.recover_requests += 1;
            }
        }
    }

    fn take(&mut self, bytes: &[u8], now: Instant) -> Arrival {
        let packet = match read_packet(bytes) {
            Ok(packet) => packet,
            Err(err) => {
                self.numbers.protocol_errors += 1;
                return Arrival::Refused(err);
            }
        };
        let next = *self.next.get_or_insert(packet.frame);
        let ahead = packet.frame.wrapping_sub(next);
        if ahead >= AHEAD {
            // Serial number arithmetic: the half of the numbers before
            // `next` is behind it.
            if ahead >= 1 << 31 {
                self.stray = None;
                // Every frame in play stays within AHEAD of the start.
                let fits = self
                    .pending
                    .last()
                    .is_none_or(|newest| newest.number.wrapping_sub(packet.frame) < AHEAD);
                if self.started || next.wrapping_sub(packet.frame) > BEHIND || !fits {
                    return self.late(&packet, now);
                }
                self.next = Some(packet.frame);
                return self.store(&packet, now);
            }
            if !self.follow_stray(packet.frame, now) {
                return Arrival::Far;
            }
        }
        self.stray = None;
        self.store(&packet, now)
    }

    // A packet far ahead of the stream is ignored, unless the one before it
    // was far ahead too and close to it: then the sharer's numbers really
    // moved, after a long outage, and the stream follows.
    fn follow_stray(&mut self, frame: u32, now: Instant) -> bool {
        let close = |a: u32, b: u32| b.wrapping_sub(a) < CLOSE;
        let start = match self.stray {
            Some(stray) if close(stray, frame) => stray,
            Some(stray) if close(frame, stray) => frame,
            _ => {
                self.stray = Some(frame);
                self.numbers.far += 1;
                return false;
            }
        };
        if let Some(newest) = self.pending.last().map(|pending| pending.number) {
            self.drop_through(newest, DropReason::Overtaken, now);
        }
        let next = self.next.unwrap_or(start);
        let skipped = start.wrapping_sub(next);
        // The frames never seen in between are asked for together. Only a
        // sharer breaking the rules puts the start at or before frames just
        // dropped; the stream then goes on from where it is.
        if skipped < 1 << 31 {
            if skipped > 0 {
                let survives = self.survives_loss;
                self.note_drop(next, start.wrapping_sub(1), survives);
            }
            self.next = Some(start);
        }
        self.started = false;
        self.numbers.restarts += 1;
        true
    }

    fn late(&mut self, packet: &Packet<'_>, now: Instant) -> Arrival {
        let Some(recent) = self
            .recent
            .iter_mut()
            .find(|recent| recent.number == packet.frame)
        else {
            self.numbers.late += 1;
            return Arrival::Late;
        };
        let counts = (packet.data, packet.parity, packet.shard.len());
        if counts != recent.counts {
            self.numbers.protocol_errors += 1;
            return Arrival::Refused(mismatch(counts, recent.counts));
        }
        let index = usize::from(packet.index);
        if has(&recent.have, index) {
            self.numbers.duplicates += 1;
            return Arrival::Duplicate;
        }
        mark(&mut recent.have, index);
        let whole = recent.whole;
        self.loss.late_shard(now);
        self.numbers.shards_received += 1;
        if whole {
            self.numbers.surplus += 1;
            Arrival::Surplus
        } else {
            self.numbers.late += 1;
            Arrival::Late
        }
    }

    fn store(&mut self, packet: &Packet<'_>, now: Instant) -> Arrival {
        let slot = match self
            .pending
            .iter()
            .position(|pending| pending.number == packet.frame)
        {
            Some(slot) => slot,
            None => match self.open(packet, now) {
                Some(slot) => slot,
                None => return self.late(packet, now),
            },
        };
        let pending = &mut self.pending[slot];
        if pending.refused {
            self.numbers.late += 1;
            return Arrival::Late;
        }
        let counts = (packet.data, packet.parity, packet.shard.len());
        let first = (pending.data, pending.parity, pending.shard);
        if counts != first {
            self.numbers.protocol_errors += 1;
            pending.refused = true;
            let buffer = std::mem::take(&mut pending.stored);
            self.held -= buffer.len();
            self.recycle(buffer);
            return Arrival::Refused(mismatch(counts, first));
        }
        let index = usize::from(packet.index);
        if has(&pending.have, index) {
            self.numbers.duplicates += 1;
            return Arrival::Duplicate;
        }
        // Past the byte limit the oldest frame goes, this one too when it is
        // the oldest.
        let bytes = INDEX + packet.shard.len();
        while self.held + bytes > MAX_HELD_BYTES {
            let Some(oldest) = self.pending.first().map(|pending| pending.number) else {
                break;
            };
            self.drop_through(oldest, DropReason::Memory, now);
            if oldest == packet.frame {
                return self.late(packet, now);
            }
        }
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.number == packet.frame)
        else {
            return self.late(packet, now);
        };
        pending
            .stored
            .extend_from_slice(&packet.index.to_le_bytes());
        pending.stored.extend_from_slice(packet.shard);
        self.held += bytes;
        mark(&mut pending.have, index);
        pending.received += 1;
        if !packet.is_parity() {
            pending.data_received += 1;
        }
        pending.last_at = now;
        if index == 0 {
            pending.flags = read_flags(packet.shard);
            if let Some((false, survives)) = pending.flags {
                self.survives_loss = Some(survives);
            }
        }
        Arrival::Kept
    }

    // A slot for a frame not seen before, making room by dropping the oldest
    // frame in play. None when the oldest is this one.
    fn open(&mut self, packet: &Packet<'_>, now: Instant) -> Option<usize> {
        let (next, ahead) = loop {
            let next = self.next?;
            let ahead = packet.frame.wrapping_sub(next);
            if ahead >= AHEAD {
                return None;
            }
            if self.pending.len() < MAX_PENDING {
                break (next, ahead);
            }
            match self.pending.first() {
                Some(oldest) if oldest.number.wrapping_sub(next) < ahead => {
                    let oldest = oldest.number;
                    self.drop_through(oldest, DropReason::Memory, now);
                }
                _ => {
                    self.drop_through(packet.frame, DropReason::Memory, now);
                    return None;
                }
            }
        };
        let stored = self.spare_for(usize::from(packet.data) * (INDEX + packet.shard.len()));
        let slot = self
            .pending
            .iter()
            .position(|pending| pending.number.wrapping_sub(next) > ahead)
            .unwrap_or(self.pending.len());
        self.pending.insert(
            slot,
            Pending {
                number: packet.frame,
                data: packet.data,
                parity: packet.parity,
                shard: packet.shard.len(),
                have: [0; MAX_SHARDS / 64],
                received: 0,
                data_received: 0,
                stored,
                first_at: now,
                last_at: now,
                flags: None,
                refused: false,
            },
        );
        Some(slot)
    }

    fn advance(&mut self, now: Instant) {
        while let Some(next) = self.next {
            if let Some(head) = self.pending.first()
                && head.number == next
                && head.refused
            {
                self.drop_through(next, DropReason::Refused, now);
                continue;
            }
            if let Some(ready) = self.pending.iter().find(|pending| pending.ready()) {
                let number = ready.number;
                if number != next {
                    self.drop_through(number.wrapping_sub(1), DropReason::Overtaken, now);
                }
                self.deliver(now);
                continue;
            }
            let wait = self.wait;
            let expired = self
                .pending
                .iter()
                .rev()
                .find(|pending| now.saturating_duration_since(pending.last_at) >= wait);
            if let Some(expired) = expired {
                let number = expired.number;
                self.drop_through(number, DropReason::Deadline, now);
                continue;
            }
            break;
        }
    }

    // Drops every frame from `next` to `last`: `last` for `why`, the ones
    // before it as overtaken. Every caller passes a frame in play or one
    // before it, all within AHEAD of `next`.
    fn drop_through(&mut self, last: u32, why: DropReason, now: Instant) {
        let Some(next) = self.next else {
            return;
        };
        debug_assert!(
            last.wrapping_sub(next) < AHEAD,
            "frame {last} is not in play from {next}"
        );
        if last.wrapping_sub(next) >= AHEAD {
            self.drop_all(now);
            return;
        }
        let mut unseen = 0;
        for offset in 0..=last.wrapping_sub(next) {
            let number = next.wrapping_add(offset);
            let why = if number == last {
                why
            } else {
                DropReason::Overtaken
            };
            if self
                .pending
                .first()
                .is_some_and(|head| head.number == number)
            {
                let pending = self.pending.remove(0);
                let why = if pending.refused {
                    DropReason::Refused
                } else {
                    why
                };
                let survives = self.survives(pending.flags);
                self.retire_dropped(pending, now);
                self.count_drop(number, why, survives);
            } else {
                unseen += 1;
                let survives = self.survives_loss;
                self.count_drop(number, why, survives);
            }
        }
        if unseen > 0 {
            self.shards_expected += u64::from(self.loss.unseen(now, unseen));
        }
        self.next = Some(last.wrapping_add(1));
        self.started = true;
    }

    // Only for a caller that broke the rule above: every frame in play goes,
    // one by one, so advance cannot come back to any of them.
    fn drop_all(&mut self, now: Instant) {
        while !self.pending.is_empty() {
            let pending = self.pending.remove(0);
            let number = pending.number;
            let survives = self.survives(pending.flags);
            self.retire_dropped(pending, now);
            self.count_drop(number, DropReason::Overtaken, survives);
            self.next = Some(number.wrapping_add(1));
        }
        self.started = true;
    }

    // Whether the frames after a dropped one still decode. Its own header
    // says, when its first shard came: nothing before an IDR is kept to
    // predict from, so a lost IDR needs the next one whatever the encoder.
    // A frame whose header never came is guessed from the newest header of
    // a frame that is not an IDR; the viewer asks for an IDR itself when
    // that guess leaves it with frames it cannot decode.
    fn survives(&self, flags: Option<(bool, bool)>) -> Option<bool> {
        match flags {
            Some((idr, survives)) => Some(survives && !idr),
            None => self.survives_loss,
        }
    }

    fn count_drop(&mut self, number: u32, why: DropReason, survives: Option<bool>) {
        let counter = match why {
            DropReason::Deadline => &mut self.numbers.dropped_deadline,
            DropReason::Overtaken => &mut self.numbers.dropped_overtaken,
            DropReason::Memory => &mut self.numbers.dropped_memory,
            DropReason::Refused => &mut self.numbers.dropped_refused,
        };
        *counter += 1;
        match self.events.back_mut() {
            Some(Queued::Dropped {
                last,
                why: queued_why,
                ..
            }) if *queued_why == why && last.wrapping_add(1) == number => *last = number,
            _ => self.events.push_back(Queued::Dropped {
                first: number,
                last: number,
                why,
            }),
        }
        self.note_drop(number, number, survives);
    }

    fn note_drop(&mut self, first: u32, last: u32, survives: Option<bool>) {
        match survives {
            Some(true) => {}
            Some(false) => self.hold = Hold::UntilIdr,
            None if self.hold == Hold::No => self.hold = Hold::UntilKnown,
            None => {}
        }
        self.gathered = Some(match self.gathered {
            Some((oldest, _)) => (oldest, last),
            None => (first, last),
        });
    }

    // The head frame is ready: put its data shards in order, rebuilding the
    // missing ones from parity, read its header, and hand it out, unless it
    // is held back waiting for an IDR.
    fn deliver(&mut self, now: Instant) {
        let pending = self.pending.remove(0);
        let number = pending.number;
        self.next = Some(number.wrapping_add(1));
        self.started = true;
        let repaired = pending.data_received < pending.data;
        let len = usize::from(pending.data) * pending.shard;
        let read = self
            .assemble(&pending, len, repaired)
            .then(|| read_frame(&self.out[..len], pending.shard).ok())
            .flatten();
        let assembly = now.saturating_duration_since(pending.first_at);
        let Some((mut facts, range)) = read else {
            self.numbers.protocol_errors += 1;
            let survives = self.survives(pending.flags);
            self.retire_dropped(pending, now);
            self.count_drop(number, DropReason::Refused, survives);
            return;
        };
        facts.number = number;
        if !facts.idr {
            self.survives_loss = Some(facts.survives_loss);
        }
        let stored = self.retire(pending, now, true);
        self.recycle(stored);
        if self.hold == Hold::UntilKnown {
            self.hold = if facts.survives_loss {
                Hold::No
            } else {
                Hold::UntilIdr
            };
        }
        if self.hold == Hold::UntilIdr && !facts.idr {
            self.numbers.skipped += 1;
            return;
        }
        self.hold = Hold::No;
        self.numbers.delivered += 1;
        if repaired {
            self.numbers.repaired += 1;
        }
        self.events.push_back(Queued::Frame {
            facts,
            range,
            repaired,
            assembly,
        });
    }

    // The frame's data shards end to end in `out`. Every one of them is
    // written, from a shard that came or from parity, so nothing an earlier
    // frame left there is read.
    fn assemble(&mut self, pending: &Pending, len: usize, repaired: bool) -> bool {
        if self.out.len() < len {
            self.out.resize(len, 0);
        }
        let (data, shard) = (usize::from(pending.data), pending.shard);
        for (index, bytes) in pending.shards() {
            if index < data {
                self.out[index * shard..(index + 1) * shard].copy_from_slice(bytes);
            }
        }
        !repaired || self.rebuild(pending).is_ok()
    }

    fn rebuild(&mut self, pending: &Pending) -> Result<(), reed_solomon_simd::Error> {
        let (data, parity, shard) = (
            usize::from(pending.data),
            usize::from(pending.parity),
            pending.shard,
        );
        let decoder = match self.decoder.as_mut() {
            Some(decoder) => {
                decoder.reset(data, parity, shard)?;
                decoder
            }
            None => self
                .decoder
                .insert(ReedSolomonDecoder::new(data, parity, shard)?),
        };
        for (index, bytes) in pending.shards() {
            if index < data {
                decoder.add_original_shard(index, bytes)?;
            } else {
                decoder.add_recovery_shard(index - data, bytes)?;
            }
        }
        let result = decoder.decode()?;
        for (index, restored) in result.restored_original_iter() {
            self.out[index * shard..(index + 1) * shard].copy_from_slice(restored);
        }
        Ok(())
    }

    // A frame out of play, whole or dropped: its shards count toward loss,
    // and it is remembered for shards that are still on their way. Returns
    // its buffer.
    fn retire(&mut self, mut pending: Pending, now: Instant, whole: bool) -> Vec<u8> {
        let total = pending.total() as u32;
        let received = u32::from(pending.received);
        self.loss.frame(now, total, received);
        self.shards_expected += u64::from(total);
        self.numbers.shards_received += u64::from(received);
        if !pending.refused {
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back(Recent {
                number: pending.number,
                counts: (pending.data, pending.parity, pending.shard),
                have: pending.have,
                whole,
            });
        }
        let stored = std::mem::take(&mut pending.stored);
        self.held -= stored.len();
        stored
    }

    fn retire_dropped(&mut self, pending: Pending, now: Instant) {
        let stored = self.retire(pending, now, false);
        self.recycle(stored);
    }

    // An empty spare buffer: the smallest with room for `most` bytes, or
    // else the largest, which grows as shards come.
    fn spare_for(&mut self, most: usize) -> Vec<u8> {
        let holds = self
            .spare
            .iter()
            .enumerate()
            .filter(|(_, buffer)| buffer.capacity() >= most)
            .min_by_key(|(_, buffer)| buffer.capacity());
        let largest = || {
            self.spare
                .iter()
                .enumerate()
                .max_by_key(|(_, buffer)| buffer.capacity())
        };
        match holds.or_else(largest).map(|(index, _)| index) {
            Some(index) => self.spare.swap_remove(index),
            None => Vec::new(),
        }
    }

    fn recycle(&mut self, mut buffer: Vec<u8>) {
        buffer.clear();
        let kept: usize = self.spare.iter().map(Vec::capacity).sum();
        if buffer.capacity() > 0
            && self.spare.len() <= MAX_PENDING
            && kept + buffer.capacity() <= MAX_HELD_BYTES
        {
            self.spare.push(buffer);
        }
    }
}

fn mismatch(counts: (u16, u16, usize), first: (u16, u16, usize)) -> PacketError {
    PacketError::Mismatch {
        data: counts.0,
        parity: counts.1,
        shard: counts.2,
        first,
    }
}

impl fmt::Debug for Reassembler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reassembler")
            .field("next", &self.next)
            .field("pending", &self.pending.len())
            .field("hold", &self.hold)
            .field("numbers", &self.numbers())
            .finish()
    }
}
