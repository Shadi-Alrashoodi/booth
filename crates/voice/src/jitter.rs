use std::collections::VecDeque;

use crate::codec::{CodecError, Decoder, MAX_FRAME, MAX_PACKET, PacketInfo, SAMPLE_RATE};

// The deepest the buffer grows. It is also how late a packet may be and
// still count as late: anything later is thrown away and counted as lost.
pub const MAX_DEPTH_MS: u32 = 60;

// A frame under this RMS (full scale 1.0) can be dropped to shrink the buffer
// without anyone hearing it: -50 dBFS, under any speech sound and above the
// hiss of a typical headset microphone.
pub const QUIET_RMS: f32 = 0.003_16;

// A noisier microphone never gets under QUIET_RMS, so a frame also counts as
// quiet when it is within 6 dB of the quietest frames the talker sent lately
// (their background noise) and under this, -40 dBFS, which speech on a
// working microphone rarely is.
const QUIET_CEILING: f32 = 0.01;
// The quietest level lately drifts up this fast, so it follows a room that
// got louder.
const FLOOR_RISE_DB_PER_S: f32 = 3.0;

const EVALUATE_MS: u32 = 100;
const LATE_WINDOW_MS: u32 = 2000;
const CLEAN_WINDOW_MS: u32 = 5000;
// Opus's guess at a lost frame turns into a buzz after a few frames, so after
// this long it fades to silence instead.
const CONCEAL_MS: u32 = 20;
// A dropped frame and the one after it are blended over 2.5 ms, so the join is
// smooth even when the dropped frame was not quite silent.
const DROP_BLEND: usize = 120;
// Room for frames ahead of the play point: more than the 12 that the cap
// allows at 5 ms.
const SLOTS: usize = 16;
// Sequence numbers remembered as received, which must reach from a cap behind
// the play point to a cap ahead of it.
const SEEN: usize = 64;
const MAX_ARRIVALS: usize = 1024;
// Lost frames in a row that still count as scattered loss: the copy of the
// frame before, which the next packet carries, or Opus's repair data can
// bring back one, and the pair when the next packet gets through. A longer
// run is an outage, which neither can.
const SCATTERED_RUN: u32 = 2;

#[derive(Clone, Copy, Debug)]
pub struct VoicePacket<'a> {
    pub seq: u16,
    pub frame: &'a [u8],
    // The frame before this one, sent again while redundancy is on.
    pub previous: Option<&'a [u8]>,
    // Redundancy is on. Set on every packet while it is, including the first
    // of a transmission, which has no frame before it to carry, so the buffer
    // starts at two frames without waiting for the second packet.
    pub redundancy: bool,
    // The talker stopped after this frame: push to talk released, or the open
    // mic tail over. Without it the end sounds like a loss and is concealed.
    pub last: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Packet(u16),
    // Rebuilt from the copy the next packet carried.
    Copy(u16),
    // Rebuilt from Opus's own repair data in the next packet.
    Repaired(u16),
    Concealed,
    Silence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pulled {
    pub samples: usize,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JitterStats {
    pub playing: bool,
    pub frame_ms: u32,
    pub depth_frames: u32,
    pub depth_ms: u32,
    // Frames waiting right now, which moves with every arrival and pull.
    pub held_frames: u32,
    pub late: u64,
    pub lost: u64,
    // Frames Opus made up in place of a missing one. The silence that follows
    // a long gap is not counted.
    pub concealed: u64,
    pub redundancy_used: u64,
    pub repaired: u64,
    // Late frames decoded after their stand-in had played, so the frame
    // after them decoded from the history the talker's encoder had. Each is
    // counted in `late` too.
    pub late_decoded: u64,
    // Frames added and removed to change the depth. Frames that arrived while
    // nobody pulled count as removed, unless the mixer said it had paused.
    pub inserted: u64,
    pub dropped: u64,
    // Of the frames sent over the last 2 s, the share that never arrived or
    // arrived later than the cap...
    pub loss_percent: f32,
    // ...and the share lost one or two in a row. A run still going at the
    // newest frame judged is left out until it ends, since it may yet
    // become an outage.
    pub scattered_percent: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    // Nothing to play; the next packet starts a transmission.
    Idle,
    // Packets are in, the first pull has not happened yet.
    Starting,
    Playing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Empty,
    Primary,
    Copy,
}

#[derive(Clone, Copy)]
struct Slot {
    ext: i64,
    held: Held,
    samples: usize,
    repair: bool,
    len: usize,
    bytes: [u8; MAX_PACKET],
}

const EMPTY_SLOT: Slot = Slot {
    ext: 0,
    held: Held::Empty,
    samples: 0,
    repair: false,
    len: 0,
    bytes: [0; MAX_PACKET],
};

impl Slot {
    // `bytes` is the slice PacketInfo::read accepted to give `info`, and it
    // refuses anything longer than MAX_PACKET, so they fit.
    fn fill(&mut self, ext: i64, held: Held, bytes: &[u8], info: PacketInfo) {
        self.ext = ext;
        self.held = held;
        self.samples = info.samples;
        self.repair = info.can_carry_repair();
        self.len = bytes.len();
        self.bytes[..bytes.len()].copy_from_slice(bytes);
    }
}

// When a packet arrived, as `at` = frames pulled so far, and its lag: `at`
// minus its sequence number. The play point has a lag too (pulls minus the
// frame being played). A packet is late when its lag is above the play point's,
// and the depth is how far the play point's lag sits above the smallest
// packet lag seen lately.
#[derive(Clone, Copy)]
struct Arrival {
    at: i64,
    lag: i64,
    // From an earlier transmission. Nothing ties its timing to this one's,
    // so it is lined up with the fastest packet of this one, which gets
    // better as more of this one arrives.
    earlier: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Adjust {
    Insert,
    // A shrink: one frame less once the depth has held for 5 s and nothing
    // in that time would have been late at one frame less.
    Drop,
    // Taking off excess: depth no late packet asked for.
    Trim,
}

// One per talker. The network side pushes packets as they arrive; the mixer
// pulls one frame at a time when it needs more audio. Time here is counted in
// pulls, so the buffer never reads a clock.
pub struct JitterBuffer {
    decoder: Decoder,
    slots: [Slot; SLOTS],
    seen: [i64; SEEN],
    phase: Phase,
    frame: usize,
    // The render side's period in samples, 0 until the mixer says.
    period: usize,
    newest: Option<i64>,
    ends_after: Option<i64>,
    redundancy: bool,
    // Idle because the packets stopped, not because the talker said so. What
    // arrives next and is older than where playing restarts was later than the
    // cap, so it counts as lost rather than dropped.
    timed_out: bool,
    // The mixer stopped pulling (deafened). Until the next pull, what falls
    // out of the buffer was neither late nor thrown away to cut delay.
    paused: bool,

    play: i64,
    play_lag: i64,
    pulls: i64,
    pulled_samples: u64,
    next_evaluation: u64,
    last_change: Option<i64>,
    // Measured: how far behind the fastest packet lately the play point is.
    depth: i64,
    // What the rules asked for: the minimum at first contact, a frame more
    // for each grow, a frame less for each shrink. A transmission starts at
    // this depth. Measured depth above it is excess: the play point fell
    // behind (a stalled render side) or started late (a slow first packet),
    // and no late packet asked for it, so it comes off at the next quiet
    // frames instead of one frame per 5 s. Kept at the measured depth, it
    // would grow by the first packet's lateness with every transmission.
    target: i64,
    min_lag: i64,
    // The smallest lag of this transmission's packets.
    fastest: i64,
    arrivals: VecDeque<Arrival>,
    pending: Option<Adjust>,
    preroll_until: i64,

    // Since the last pull: how many packets came faster than the fastest one
    // known at that pull, and the smallest lag of any packet.
    gap_floor: i64,
    gap_early: u32,
    gap_fastest: i64,

    missing_run: u32,
    // The frame the play point passed last was made up (concealed, silence,
    // a stand-in) rather than decoded, and the decoder has decoded nothing
    // since.
    made_up: bool,
    // That frame, arrived after all, waiting for the next pull to decode it.
    late: Slot,
    audible: bool,
    fade_in: bool,
    blend: bool,
    // The quietest frame RMS this talker sent lately.
    floor: f32,
    scratch: [f32; MAX_FRAME],

    judged: Option<i64>,
    outcomes: VecDeque<bool>,
    outcomes_lost: u32,
    // Frames lost in a row just before the oldest outcome kept, so a run
    // the window cuts through is still counted as the outage it was.
    lost_before: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new() -> Result<JitterBuffer, CodecError> {
        Ok(JitterBuffer {
            decoder: Decoder::new()?,
            slots: [EMPTY_SLOT; SLOTS],
            seen: [i64::MIN; SEEN],
            phase: Phase::Idle,
            frame: 240,
            period: 0,
            newest: None,
            ends_after: None,
            redundancy: false,
            timed_out: false,
            paused: false,
            play: 0,
            play_lag: 0,
            pulls: 0,
            pulled_samples: 0,
            next_evaluation: evaluate_samples(),
            last_change: None,
            depth: 0,
            min_lag: 0,
            target: 0,
            fastest: 0,
            arrivals: VecDeque::with_capacity(MAX_ARRIVALS),
            pending: None,
            preroll_until: 0,
            gap_floor: i64::MIN,
            gap_early: 0,
            gap_fastest: i64::MAX,
            missing_run: 0,
            made_up: false,
            late: EMPTY_SLOT,
            audible: false,
            fade_in: true,
            blend: false,
            floor: QUIET_RMS / 2.0,
            scratch: [0.0; MAX_FRAME],
            judged: None,
            outcomes: VecDeque::with_capacity(window_frames(240) as usize),
            outcomes_lost: 0,
            lost_before: 0,
            stats: JitterStats::default(),
        })
    }

    pub fn push(&mut self, packet: VoicePacket<'_>) -> Result<(), CodecError> {
        let info = PacketInfo::read(packet.frame)?;
        let ext = self.extend(packet.seq);
        match self.phase {
            Phase::Idle => {
                if let Some(newest) = self.newest
                    && ext <= newest
                    && newest - ext < window_frames(self.frame)
                {
                    // A straggler from a transmission that has ended.
                    return Ok(());
                }
                self.start(ext, &packet);
            }
            Phase::Starting => {
                if self.judged.is_some_and(|judged| ext <= judged) {
                    return Ok(());
                }
            }
            Phase::Playing => {
                let lead = ext - self.play;
                if lead < 0 {
                    if info.samples == self.frame || self.newest.is_some_and(|newest| ext <= newest)
                    {
                        self.arrived_late(ext, packet.frame, info);
                        return Ok(());
                    }
                    let redundancy = packet.redundancy || packet.previous.is_some();
                    self.back_to_switch(ext, info.samples, redundancy);
                } else if lead >= self.cap() {
                    // The play point fell too far behind the packets: pulls
                    // stopped (deafen, a stalled audio thread) or the talker
                    // jumped ahead. Start again from the newest packets, at
                    // the depth from before: what arrived since the last pull
                    // only looks early because nobody was pulling.
                    let since_last_pull = self.pulls;
                    self.arrivals.retain(|arrival| arrival.at < since_last_pull);
                    self.measure_depth();
                    self.phase = Phase::Starting;
                    self.timed_out = false;
                }
            }
        }
        if self.is_seen(ext) {
            return Ok(());
        }
        self.store(ext, Held::Primary, packet.frame, info);
        self.see(ext);
        if self.newest.is_none_or(|newest| ext > newest) {
            self.newest = Some(ext);
            self.redundancy = packet.redundancy || packet.previous.is_some();
            self.ends_after = packet.last.then_some(ext);
        }
        if let Some(previous) = packet.previous
            && let Ok(info) = PacketInfo::read(previous)
        {
            let prior = ext - 1;
            let wanted = self.phase != Phase::Playing || prior >= self.play;
            if wanted && self.slot(prior).is_none() && !self.judged.is_some_and(|j| prior <= j) {
                self.store(prior, Held::Copy, previous, info);
            }
        }
        if self.phase == Phase::Playing {
            self.record(ext, info.samples);
        } else if let Some(newest) = self.newest {
            // Anything a cap behind the newest packet cannot survive the
            // restart, so it is judged now, while it is still remembered, and
            // let go, so a buffer nobody pulls never holds more than the cap.
            let oldest_kept = newest - self.cap() + 1;
            self.judge_through(oldest_kept - 1, self.timed_out);
            self.discard_before(oldest_kept, !self.timed_out && !self.paused);
        }
        Ok(())
    }

    // The mixer stopped pulling: deafened. A buffer that timed out before
    // this would otherwise count everything the talker says next as lost,
    // since nothing tells it the time is passing.
    pub fn pause(&mut self) {
        self.paused = true;
        self.timed_out = false;
    }

    // The render side's period in samples. A period longer than a frame pulls
    // several frames at once, which the minimum depth has to cover.
    pub fn set_period(&mut self, samples: usize) {
        self.period = samples;
    }

    // `out` must hold MAX_FRAME samples. None means the talker is silent and
    // nothing is buffered; the mixer leaves them out.
    pub fn pull(&mut self, out: &mut [f32]) -> Option<Pulled> {
        assert!(
            out.len() >= MAX_FRAME,
            "a pull needs room for {MAX_FRAME} samples, got {}",
            out.len()
        );
        match self.phase {
            Phase::Idle => {
                self.paused = false;
                return None;
            }
            Phase::Starting => self.restart(),
            Phase::Playing => self.line_up(),
        }
        let pulled = self.next_frame(out);
        self.pulls += 1;
        self.pulled_samples += pulled.samples as u64;
        self.paused = false;
        self.gap_floor = self.min_lag;
        self.gap_early = 0;
        self.gap_fastest = i64::MAX;
        if self.phase == Phase::Playing {
            self.judge_through(self.play - self.cap() - 1, false);
            if self.pulled_samples >= self.next_evaluation {
                self.next_evaluation += evaluate_samples();
                self.evaluate();
            }
            if let Some(newest) = self.newest
                && self.play > newest + self.cap()
            {
                self.go_idle(true);
            }
        }
        Some(pulled)
    }

    pub fn stats(&self) -> JitterStats {
        let frame_ms = frame_ms(self.frame);
        let depth = self.depth.max(0) as u32;
        let held = self
            .slots
            .iter()
            .filter(|slot| {
                slot.held != Held::Empty && (self.phase != Phase::Playing || slot.ext >= self.play)
            })
            .count() as u32;
        let share = |frames: u32| {
            if self.outcomes.is_empty() {
                0.0
            } else {
                frames as f32 * 100.0 / self.outcomes.len() as f32
            }
        };
        JitterStats {
            playing: self.phase != Phase::Idle,
            frame_ms,
            depth_frames: depth,
            depth_ms: depth * frame_ms,
            held_frames: held,
            loss_percent: share(self.outcomes_lost),
            scattered_percent: share(self.scattered()),
            ..self.stats
        }
    }

    // Lost frames in the window that belong to runs of at most
    // SCATTERED_RUN, counting the part of a run from before the window.
    fn scattered(&self) -> u32 {
        let mut scattered = 0;
        let mut run = self.lost_before;
        let mut run_here = 0;
        for &lost in &self.outcomes {
            if lost {
                run = run.saturating_add(1);
                run_here += 1;
                continue;
            }
            if run <= SCATTERED_RUN {
                scattered += run_here;
            }
            run = 0;
            run_here = 0;
        }
        scattered
    }

    fn extend(&self, seq: u16) -> i64 {
        match self.newest {
            Some(newest) => newest + i64::from(seq.wrapping_sub(newest as u16) as i16),
            None => i64::from(seq),
        }
    }

    fn cap(&self) -> i64 {
        i64::from(MAX_DEPTH_MS / frame_ms(self.frame))
    }

    fn min_depth(&self) -> i64 {
        self.min_depth_for(self.frame, self.redundancy)
    }

    // One frame; two while each packet carries a copy of the previous frame
    // or in the 10 ms mode, because the copy or Opus's repair data for frame N
    // arrives with frame N+1. A render period longer than a frame takes more
    // than one frame at a time, and the last of them has to be in already.
    fn min_depth_for(&self, samples: usize, redundancy: bool) -> i64 {
        let base = if redundancy || samples == 480 { 2 } else { 1 };
        let per_period = self.period.div_ceil(samples).max(1) as i64;
        (base + per_period - 1).min(i64::from(MAX_DEPTH_MS / frame_ms(samples)))
    }

    // What came before `ext` is judged as packets arrive, a cap behind the
    // newest, so a frame that overtook the one before it still finds room.
    fn start(&mut self, ext: i64, packet: &VoicePacket<'_>) {
        if self.newest.is_some_and(|newest| ext <= newest) {
            // The talker's numbering went backwards, a restart on their side.
            // The numbers it uses now may have been seen before.
            self.seen = [i64::MIN; SEEN];
            self.judged = Some(ext - 1);
        }
        self.slots = [EMPTY_SLOT; SLOTS];
        self.newest = Some(ext);
        self.redundancy = packet.redundancy || packet.previous.is_some();
        self.ends_after = packet.last.then_some(ext);
        self.phase = Phase::Starting;
    }

    fn restart(&mut self) {
        let newest = self.newest.expect("a starting buffer holds a packet");
        if let Some(samples) = self.slot(newest).map(|slot| slot.samples)
            && samples != self.frame
        {
            self.set_frame(samples);
        }
        let first_contact = self.last_change.is_none();
        let measured = self.depth.max(1);
        let kept = self.target.clamp(self.min_depth(), self.cap());
        // The pause before a transmission is the quietest stretch there is,
        // so a shrink that is due happens here: this one starts a frame
        // shallower. A talker who never makes a quiet frame, music or a loud
        // room, still shrinks this way.
        let shrink = !first_contact && kept > self.min_depth() && self.clean() && self.settled();
        let depth = kept - i64::from(shrink);
        let start = newest - (depth - 1);
        let counted = !self.timed_out && !self.paused;
        self.discard_before(newest - (kept - 1), counted);
        self.discard_before(start, false);
        self.preroll_until = self
            .slots
            .iter()
            .filter(|slot| slot.held != Held::Empty)
            .map(|slot| slot.ext)
            .min()
            .unwrap_or(newest);
        if first_contact {
            // Nothing before the first packet was ever sent to us.
            let before_first = self.preroll_until - 1;
            self.judged = Some(self.judged.map_or(before_first, |j| j.max(before_first)));
        }
        self.judge_through(start - 1, self.timed_out);

        // Keep the history of earlier transmissions comparable with this one:
        // it is measured against the play point, which moves here. The newest
        // packet sets the depth, so it lines up with the fastest arrival
        // before, and is the fastest arrival from now on. What in that
        // history came later than this depth allows now counts as late, and
        // grows the buffer if there was enough of it: the excess it arrived
        // in was not something to count on.
        let play_lag = self.pulls - start;
        let shift = (self.pulls - newest) - (self.play_lag - (measured - 1));
        for arrival in &mut self.arrivals {
            arrival.lag += shift;
            arrival.earlier = true;
        }
        self.play_lag = play_lag;
        self.play = start;
        self.depth = depth;
        self.target = depth;
        self.min_lag = play_lag - (depth - 1);
        self.fastest = self.min_lag;
        for ext in start..=newest {
            if self
                .slot(ext)
                .is_some_and(|slot| slot.held == Held::Primary)
            {
                self.push_arrival(ext);
            }
        }

        if first_contact || shrink {
            self.last_change = Some(self.pulls);
        }
        if shrink {
            self.stats.dropped += 1;
        }
        self.pending = None;
        self.missing_run = 0;
        self.made_up = false;
        self.late = EMPTY_SLOT;
        self.blend = self.audible && self.preroll_until == start;
        self.fade_in = !self.audible;
        self.phase = Phase::Playing;
    }

    fn go_idle(&mut self, timed_out: bool) {
        if let Some(newest) = self.newest {
            self.judge_through(newest, false);
        }
        self.slots = [EMPTY_SLOT; SLOTS];
        self.late = EMPTY_SLOT;
        self.made_up = false;
        self.phase = Phase::Idle;
        self.timed_out = timed_out;
        if !timed_out {
            // The talker said it stopped, so its next transmission starts
            // from a reset encoder; a decoder still holding this one's tail
            // would put a seam into that transmission's first frames. After
            // an outage the talker's encoder carried on, and so does this.
            let _ = self.decoder.reset();
        }
        self.audible = false;
        self.fade_in = true;
        self.pending = None;
    }

    fn arrived_late(&mut self, ext: i64, frame: &[u8], info: PacketInfo) {
        if self.is_seen(ext) {
            return;
        }
        if self.judged.is_some_and(|judged| ext <= judged) {
            // Later than the cap: already counted as lost when its time
            // passed, and an outage like this must not grow the buffer.
            return;
        }
        self.see(ext);
        self.stats.late += 1;
        self.record(ext, info.samples);
        // Its stand-in has played, so it never plays itself. Decoded before
        // the next frame, it still gives the decoder the history the
        // talker's encoder predicted that frame from. Without it, after a
        // stand-in for the first frame of a word Opus rebuilds the next
        // frames from the silence before it, and the word stays near silent
        // for 20 to 45 ms. That works only while nothing has been decoded
        // since the stand-in, and the blend joins the next frame to the
        // stand-in that played.
        if self.late_in_reach(ext, info.samples) {
            self.late.fill(ext, Held::Primary, frame, info);
            self.blend |= self.audible;
        }
    }

    fn late_in_reach(&self, ext: i64, samples: usize) -> bool {
        self.made_up && ext == self.play - 1 && samples == self.frame
    }

    // The late frame, when it is still the one just before the play point,
    // decoded for the decoder's sake. The audio is not played.
    fn decode_late(&mut self) -> bool {
        if self.late.held == Held::Empty {
            return false;
        }
        let late = std::mem::replace(&mut self.late, EMPTY_SLOT);
        if !self.late_in_reach(late.ext, late.samples) {
            return false;
        }
        let mut unplayed = [0f32; MAX_FRAME];
        let decoded = self
            .decoder
            .decode(&late.bytes[..late.len], &mut unplayed)
            .is_ok_and(|len| len == self.frame);
        if decoded {
            self.stats.late_decoded += 1;
            self.made_up = false;
            self.missing_run = 0;
        }
        decoded
    }

    // The talker moved to 10 ms frames and the first one came after its
    // number had gone by, played as a 5 ms stand-in: a 10 ms frame is
    // complete 5 ms after a 5 ms one would have been. The stand-in filled
    // time, not this frame, so playing goes back to it and blends in from the
    // stand-in. When the frame before it is missing too, playing goes back one
    // further, to the copy or the repair data this packet carries; that costs
    // nothing, since the new mode's minimum depth adds a frame there anyway.
    fn back_to_switch(&mut self, ext: i64, samples: usize, redundancy: bool) {
        let newest = self.newest.unwrap_or(ext - 1);
        let deeper = self.min_depth_for(samples, redundancy) > 1;
        let to = if deeper && ext - 1 > newest {
            ext - 1
        } else {
            ext
        };
        self.play_lag += self.play - to;
        self.play = to;
        self.made_up = false;
        self.blend = self.audible;
    }

    // Keeps the history in step with the packets arriving now, so it never
    // stands in the way of taking excess off.
    //
    // Two or more packets came in faster than any of the last 2 s with no
    // pull between them: the render side stalled (a thread that missed its
    // turn, a driver glitch) and the play point fell that far behind, which
    // shows as excess depth. The whole history moves to the new timing,
    // without the packets that came during the stall. A path that suddenly
    // got faster looks the same and is handled the same.
    //
    // Otherwise, a packet faster than any before in this transmission means
    // its first packet, which earlier transmissions were lined up with, was
    // slow: they move by the difference.
    fn line_up(&mut self) {
        if self.gap_early >= 2 {
            let slip = self.gap_floor - self.gap_fastest;
            let now = self.pulls;
            self.arrivals.retain(|arrival| arrival.at < now);
            for arrival in &mut self.arrivals {
                arrival.lag -= slip;
            }
            self.fastest -= slip;
        } else if self.gap_fastest < self.fastest {
            let by = self.fastest - self.gap_fastest;
            self.fastest = self.gap_fastest;
            for arrival in self.arrivals.iter_mut().filter(|arrival| arrival.earlier) {
                arrival.lag -= by;
            }
        }
    }

    fn excess(&self) -> i64 {
        (self.depth - self.target).max(0)
    }

    // Arrivals are timed in pulls of the frame size playing. A packet of the
    // other size is ahead of a mode switch, where the history starts over;
    // timed in the wrong unit it would only make the depth look deeper.
    fn record(&mut self, ext: i64, samples: usize) {
        if samples != self.frame {
            return;
        }
        self.push_arrival(ext);
        let lag = self.pulls - ext;
        self.gap_fastest = self.gap_fastest.min(lag);
        if lag < self.gap_floor {
            self.gap_early += 1;
        }
        if lag < self.min_lag {
            self.min_lag = lag;
            self.depth = (self.play_lag - self.min_lag + 1).clamp(1, self.cap());
        }
    }

    fn push_arrival(&mut self, ext: i64) {
        if self.arrivals.len() == MAX_ARRIVALS {
            self.arrivals.pop_front();
        }
        self.arrivals.push_back(Arrival {
            at: self.pulls,
            lag: self.pulls - ext,
            earlier: false,
        });
    }

    // The buffer's rules, run every 100 ms of played audio. "Late" is
    // measured against the depth being judged: grow when more than 1 percent
    // of the last 2 s would be late at the current depth; shrink when nothing
    // in the last 5 s would have been late at one frame less, at most one
    // frame per 5 s. Counting late packets against the depth they arrived at
    // instead would keep growing for 2 s after one burst, and shrink straight
    // back into the jitter that caused it. Excess comes off under the same
    // clean test without the 5 s wait.
    fn evaluate(&mut self) {
        let late_window = window_frames(self.frame);
        let clean_window = self.frames_in(CLEAN_WINDOW_MS);
        let now = self.pulls;
        while self
            .arrivals
            .front()
            .is_some_and(|arrival| arrival.at < now - clean_window)
        {
            self.arrivals.pop_front();
        }
        self.measure_depth();
        if self.pending == Some(Adjust::Trim) && self.excess() == 0 {
            self.pending = None;
        }
        let late = self
            .arrivals
            .iter()
            .filter(|arrival| arrival.at >= now - late_window && arrival.lag > self.play_lag)
            .count() as i64;
        if late * 100 > late_window {
            // Late packets back all of the depth there is now.
            self.target = self.target.max(self.depth);
            if self.depth < self.cap() {
                self.pending = Some(Adjust::Insert);
            }
            return;
        }
        if self.pending.is_some() || self.depth <= self.min_depth() || !self.clean() {
            return;
        }
        if self.excess() > 0 {
            self.pending = Some(Adjust::Trim);
        } else if self.settled() && self.target > self.min_depth() {
            self.pending = Some(Adjust::Drop);
        }
    }

    // Nothing in the history would have been late with one frame less.
    fn clean(&self) -> bool {
        self.arrivals
            .iter()
            .all(|arrival| arrival.lag < self.play_lag)
    }

    // 5 s of playing since the depth last changed.
    fn settled(&self) -> bool {
        let clean_window = self.frames_in(CLEAN_WINDOW_MS);
        self.last_change
            .is_some_and(|change| self.pulls - change >= clean_window)
    }

    // The depth is how far the play point sits behind the fastest packet of
    // the last 2 s. With no packet in that time it stays as it was.
    fn measure_depth(&mut self) {
        let since = self.pulls - window_frames(self.frame);
        if let Some(min_lag) = self
            .arrivals
            .iter()
            .filter(|arrival| arrival.at >= since)
            .map(|arrival| arrival.lag)
            .min()
        {
            self.min_lag = min_lag;
            self.depth = (self.play_lag - min_lag + 1).clamp(1, self.cap());
        }
    }

    fn next_frame(&mut self, out: &mut [f32]) -> Pulled {
        // A missing frame is taken to be the size of the one after it, so
        // the first frame after a mode switch can be lost and still be
        // stood in for, or rebuilt, at its own size.
        let here = self
            .slot(self.play)
            .or_else(|| self.slot(self.play + 1))
            .map(|slot| slot.samples);
        if let Some(samples) = here
            && samples != self.frame
        {
            self.switch_frame(samples);
        }
        if self.pending.is_none() && self.depth < self.min_depth() {
            self.pending = Some(Adjust::Insert);
        }
        if self.pending == Some(Adjust::Insert) {
            self.pending = None;
            if self.depth < self.cap() {
                self.play_lag += 1;
                self.depth += 1;
                self.target = self.target.max(self.depth);
                self.last_change = Some(self.pulls);
                self.stats.inserted += 1;
                return self.stand_in(out);
            }
        }
        if self.play < self.preroll_until {
            self.play += 1;
            self.made_up = true;
            return self.stand_in(out);
        }

        // The blend carries on from what played, so it is made before a late
        // frame moves the decoder on to the talker's own audio.
        let blending = std::mem::take(&mut self.blend);
        if blending && self.decoder.conceal(self.frame, &mut self.scratch).is_err() {
            self.scratch.fill(0.0);
        }
        let caught_up = self.decode_late();
        let Some(source) = self.decode_at(self.play, out) else {
            if blending {
                let samples = self.frame;
                if caught_up && self.decoder.conceal(samples, out).is_ok() {
                    crossfade(&self.scratch[..samples], &mut out[..samples], samples);
                } else {
                    out[..samples].copy_from_slice(&self.scratch[..samples]);
                }
                self.play += 1;
                self.made_up = true;
                self.missing_run += 1;
                self.stats.concealed += 1;
                return Pulled {
                    samples,
                    source: Source::Concealed,
                };
            }
            return self.missing(out);
        };
        self.made_up = false;
        let samples = self.frame;
        let steady = !blending && !self.fade_in;
        if blending {
            crossfade(&self.scratch[..samples], &mut out[..samples], samples);
        } else if self.fade_in {
            fade(&mut out[..samples], true);
        }
        self.fade_in = false;
        self.audible = true;
        self.missing_run = 0;
        let level = rms(&out[..samples]);
        if steady {
            // A faded frame is quieter than what the talker sent and would
            // pull the level down.
            self.hear(level, samples);
        }

        let shrinking = match self.pending {
            Some(Adjust::Drop) => Some(Adjust::Drop),
            Some(Adjust::Trim) if self.excess() > 0 => Some(Adjust::Trim),
            _ => None,
        };
        if let Some(shrinking_by) = shrinking
            && self.depth > self.min_depth()
            && level < self.quiet_level()
            && self
                .slot(self.play + 1)
                .is_some_and(|next| next.samples == samples)
        {
            let mut next = [0f32; MAX_FRAME];
            if let Some(next_source) = self.decode_at(self.play + 1, &mut next) {
                crossfade(&out[..samples], &mut next[..samples], DROP_BLEND);
                out[..samples].copy_from_slice(&next[..samples]);
                self.play += 2;
                self.play_lag -= 1;
                self.depth -= 1;
                self.pending = None;
                if shrinking_by == Adjust::Trim {
                    // The rest of the excess comes off at the next quiet
                    // frame, not after the next check: nothing about the
                    // network has changed.
                    self.pending = (self.excess() > 0).then_some(Adjust::Trim);
                } else {
                    self.target = (self.target - 1).max(self.min_depth());
                    self.last_change = Some(self.pulls);
                }
                self.stats.dropped += 1;
                return Pulled {
                    samples,
                    source: next_source,
                };
            }
        }
        self.play += 1;
        Pulled { samples, source }
    }

    fn missing(&mut self, out: &mut [f32]) -> Pulled {
        let samples = self.frame;
        let out = &mut out[..samples];
        if let Some(newest) = self.newest
            && self.ends_after == Some(newest)
            && self.play > newest
        {
            // The talker stopped. One faded frame of concealment finishes the
            // codec's overlap and brings the level down to zero.
            if self.audible && self.decoder.conceal(samples, out).is_ok() {
                fade(out, false);
            } else {
                out.fill(0.0);
            }
            self.play += 1;
            self.go_idle(false);
            return Pulled {
                samples,
                source: Source::Silence,
            };
        }

        self.play += 1;
        self.made_up = true;
        self.missing_run += 1;
        let conceal_frames = CONCEAL_MS / frame_ms(samples);
        let concealing = self.audible && self.missing_run <= conceal_frames + 1;
        if concealing && self.decoder.conceal(samples, out).is_ok() {
            self.stats.concealed += 1;
            if self.missing_run > conceal_frames {
                fade(out, false);
                self.audible = false;
                self.fade_in = true;
            }
            return Pulled {
                samples,
                source: Source::Concealed,
            };
        }
        out.fill(0.0);
        self.audible = false;
        self.fade_in = true;
        Pulled {
            samples,
            source: Source::Silence,
        }
    }

    // A frame that is not the next one in line: concealment while audio is
    // playing, so it carries on from what came before, silence otherwise.
    fn stand_in(&mut self, out: &mut [f32]) -> Pulled {
        let samples = self.frame;
        let out = &mut out[..samples];
        if self.audible && self.decoder.conceal(samples, out).is_ok() {
            return Pulled {
                samples,
                source: Source::Concealed,
            };
        }
        out.fill(0.0);
        Pulled {
            samples,
            source: Source::Silence,
        }
    }

    fn decode_at(&mut self, ext: i64, out: &mut [f32]) -> Option<Source> {
        let seq = ext as u16;
        if let Some(slot) = self.slot(ext) {
            let slot = *slot;
            self.decoder
                .decode(&slot.bytes[..slot.len], out)
                .ok()
                .filter(|&len| len == self.frame)?;
            return Some(if slot.held == Held::Copy {
                self.stats.redundancy_used += 1;
                Source::Copy(seq)
            } else {
                Source::Packet(seq)
            });
        }
        let next = *self.slot(ext + 1)?;
        if next.held != Held::Primary || !next.repair || next.samples != self.frame {
            return None;
        }
        self.decoder
            .recover(&next.bytes[..next.len], self.frame, out)
            .ok()?;
        self.stats.repaired += 1;
        Some(Source::Repaired(seq))
    }

    // The talker changed mode in the middle of a transmission. The depth is
    // read from what is buffered; the minimum-depth rule then adds a frame if
    // the 10 ms mode needs one. What the rules asked for is the new mode's
    // minimum plus whatever late packets had added to the old one. Back from
    // the 10 ms mode, what is buffered includes that mode's extra frame,
    // which 5 ms frames do not need, so it comes off as excess.
    fn switch_frame(&mut self, samples: usize) {
        let old_ms = i64::from(frame_ms(self.frame));
        let grown_ms = (self.target - self.min_depth()).max(0) * old_ms;
        self.set_frame(samples);
        let beyond_cap = self.play + self.cap();
        for slot in &mut self.slots {
            if slot.held != Held::Empty && slot.ext >= beyond_cap {
                slot.held = Held::Empty;
                self.stats.dropped += 1;
            }
        }
        let held = self.newest.map_or(1, |newest| newest - self.play + 1);
        self.depth = held.clamp(1, self.cap());
        let new_ms = i64::from(frame_ms(samples));
        let grown = (grown_ms + new_ms - 1) / new_ms;
        self.target = (self.min_depth() + grown).min(self.cap());
        self.min_lag = self.play_lag - self.depth + 1;
        self.fastest = self.min_lag;
        self.last_change = Some(self.pulls);
        // Whatever was pending was worked out in frames of the old size.
        self.pending = None;
    }

    // Frame size follows the talker's mode. The depths carry over in
    // milliseconds; the arrival history is in frames of the old size, so it
    // starts over.
    fn set_frame(&mut self, samples: usize) {
        let old_ms = i64::from(frame_ms(self.frame));
        let new_ms = i64::from(frame_ms(samples));
        let convert = |frames: i64| (frames * old_ms + new_ms - 1) / new_ms;
        self.depth = convert(self.depth);
        self.target = convert(self.target);
        self.frame = samples;
        self.arrivals.clear();
        let window = window_frames(samples) as usize;
        while self.outcomes.len() > window {
            self.forget_oldest_outcome();
        }
    }

    fn forget_oldest_outcome(&mut self) {
        match self.outcomes.pop_front() {
            Some(true) => {
                self.outcomes_lost -= 1;
                self.lost_before = self.lost_before.saturating_add(1);
            }
            Some(false) => self.lost_before = 0,
            None => {}
        }
    }

    // Frames that arrived but will never be played. `counted` says whether they
    // are dropped for delay; when not, they were later than the cap and the
    // loss count has them.
    fn discard_before(&mut self, oldest_kept: i64, counted: bool) {
        for slot in &mut self.slots {
            if slot.held != Held::Empty && slot.ext < oldest_kept {
                if slot.held == Held::Primary && counted {
                    self.stats.dropped += 1;
                }
                slot.held = Held::Empty;
            }
        }
    }

    fn slot(&self, ext: i64) -> Option<&Slot> {
        let slot = &self.slots[ext.rem_euclid(SLOTS as i64) as usize];
        (slot.held != Held::Empty && slot.ext == ext).then_some(slot)
    }

    fn store(&mut self, ext: i64, held: Held, bytes: &[u8], info: PacketInfo) {
        let slot = &mut self.slots[ext.rem_euclid(SLOTS as i64) as usize];
        if slot.held != Held::Empty && slot.ext > ext {
            return;
        }
        if slot.held == Held::Primary && slot.ext == ext {
            return;
        }
        slot.fill(ext, held, bytes, info);
    }

    fn is_seen(&self, ext: i64) -> bool {
        self.seen[ext.rem_euclid(SEEN as i64) as usize] == ext
    }

    fn see(&mut self, ext: i64) {
        self.seen[ext.rem_euclid(SEEN as i64) as usize] = ext;
    }

    // Settles, for every sequence number up to `upto`, whether it counts as
    // lost, and keeps the last 2 s of that for the loss percentage. A big jump
    // in numbering is judged over its last 2 s only, so a talker restarting
    // their count does not read as thousands of losses.
    fn judge_through(&mut self, upto: i64, arrived_counts_as_lost: bool) {
        let window = window_frames(self.frame);
        let from = match self.judged {
            Some(judged) if judged >= upto => return,
            Some(judged) => (judged + 1).max(upto - window + 1),
            None => upto + 1,
        };
        for ext in from..=upto {
            let lost = arrived_counts_as_lost || !self.is_seen(ext);
            if lost {
                self.stats.lost += 1;
                self.outcomes_lost += 1;
            }
            if self.outcomes.len() == window as usize {
                self.forget_oldest_outcome();
            }
            self.outcomes.push_back(lost);
        }
        self.judged = Some(upto);
    }

    fn frames_in(&self, ms: u32) -> i64 {
        i64::from(ms / frame_ms(self.frame))
    }

    // Follows the quietest frames the talker sends: down at once, up
    // FLOOR_RISE_DB_PER_S.
    fn hear(&mut self, level: f32, samples: usize) {
        let rise_db = FLOOR_RISE_DB_PER_S * frame_ms(samples) as f32 / 1000.0;
        let rise = 10f32.powf(rise_db / 20.0);
        self.floor = (self.floor * rise).min(level).max(QUIET_RMS / 2.0);
    }

    // Under this a frame can be dropped: 6 dB over the talker's quietest,
    // never under QUIET_RMS and never over QUIET_CEILING.
    fn quiet_level(&self) -> f32 {
        (2.0 * self.floor).clamp(QUIET_RMS, QUIET_CEILING)
    }
}

fn frame_ms(samples: usize) -> u32 {
    (samples * 1000 / SAMPLE_RATE as usize) as u32
}

fn window_frames(samples: usize) -> i64 {
    i64::from(LATE_WINDOW_MS / frame_ms(samples))
}

fn evaluate_samples() -> u64 {
    u64::from(SAMPLE_RATE * EVALUATE_MS / 1000)
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

// Linear ramp over the whole frame, up from silence or down to it.
fn fade(samples: &mut [f32], up: bool) {
    let len = samples.len() as f32;
    for (i, sample) in samples.iter_mut().enumerate() {
        let gain = (i as f32 + 1.0) / len;
        *sample *= if up { gain } else { 1.0 - gain };
    }
}

// Blends `from` into the start of `into` over `len` samples; past that `into`
// is left alone.
fn crossfade(from: &[f32], into: &mut [f32], len: usize) {
    let len = len.min(into.len()).min(from.len());
    for i in 0..len {
        let gain = (i as f32 + 1.0) / (len as f32 + 1.0);
        into[i] = from[i] * (1.0 - gain) + into[i] * gain;
    }
}
