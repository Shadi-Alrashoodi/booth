// The listener's buffer for one sharer's PC sound: 10 ms stereo frames, 100
// a second, held from the network until the mixer pulls them. It works as
// voice's jitter buffer does (jitter.rs), without what only voice needs (two
// frame sizes, the copy of the frame before, Opus's repair data), and with
// one thing only sound needs. The sharer's sound card and the listener's run
// at slightly different rates, tens to a few hundred ppm, so a stream that
// never pauses gains or loses a whole frame every 33 s to a few minutes, and
// the buffer has to add or take away a frame that often without it being
// heard.
//
// Time is counted in pulls, 10 ms of play each, so nothing here reads a
// clock. Frames are decoded when pulled, never when pushed, and nothing
// allocates after new().

use std::collections::VecDeque;
use std::fmt;

use crate::stereo::{CHANNELS, Decoder, FRAME, FrameInfo, MAX_PACKET, StereoError};

pub const FRAME_MS: u32 = 10;

// The deepest the buffer grows, six frames. It is also how late a frame may
// be and still count as late: anything later is lost.
pub const MAX_DEPTH_MS: u32 = 60;

// A frame under this RMS (full scale 1.0) on both channels can be dropped to
// shrink the buffer without anyone hearing it: -50 dBFS.
pub const QUIET_RMS: f32 = 0.003_16;

const CAP: i64 = (MAX_DEPTH_MS / FRAME_MS) as i64;

// In pulls.
const EVALUATE: i64 = 10;
const LATE_WINDOW: i64 = 200;
const CLEAN_WINDOW: i64 = 500;
// How long a shrink that is due waits for a quiet frame before it takes the
// next frame whatever it holds.
const QUIET_WAIT: i64 = 200;

// Opus's guess at a missing frame turns into a buzz after a few frames, so
// it plays for this many and fades out over the next.
const CONCEAL_FRAMES: u32 = 2;
// 2.5 ms, in frames: the fade in after silence, the blend over a quiet frame
// that is dropped, and the blend out of the sound before a decoder that
// starts over.
const FADE: usize = 120;
// 5 ms: the blend over a frame dropped whatever its level, which is heard,
// so it is spread wider.
const LOUD_BLEND: usize = 240;

// Room for the cap's six frames ahead of the play point and a late one just
// behind it, with no two sharing a slot.
const SLOTS: usize = 16;
// Frames remembered as received, which must reach from a cap behind the play
// point to a cap ahead of it.
const SEEN: usize = 64;
// 5 s of arrivals is 500; the rest is room for frames that come while
// nobody pulls.
const MAX_ARRIVALS: usize = 1024;
// A frame this far behind the newest is not late: the sharer's numbering
// started over.
const RESTART_BACK: i64 = LATE_WINDOW;

#[derive(Clone, Copy, Debug)]
pub struct SoundPacket<'a> {
    pub seq: u16,
    pub frame: &'a [u8],
    // The first frame from an encoder that started over: sound turned on, or
    // back after a stop.
    pub first: bool,
    // The sharer's sound stopped after this frame. Without it the end sounds
    // like a loss, is concealed and goes quiet a cap later.
    pub last: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Packet(u16),
    // Made up by Opus, for a frame that is missing or one added to grow.
    Concealed,
    Silence,
    // The faded frame of concealment that finishes the sound after a frame
    // marked last.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pulled {
    // Of two samples each; always FRAME.
    pub frames: usize,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BufferStats {
    pub playing: bool,
    pub depth_frames: u32,
    pub depth_ms: u32,
    // Frames waiting right now, which moves with every arrival and pull.
    pub held_frames: u32,
    // Frames that came after their turn, within the cap. One that played
    // after all, because the buffer grew for it, is counted too.
    pub late: u64,
    pub lost: u64,
    // Frames Opus made up, missing ones and added ones. The silence after
    // the concealment and the fade at the end are not counted.
    pub concealed: u64,
    // Frames added and taken away to change the depth. Frames that came
    // while nobody pulled count as taken away, unless the mixer said it had
    // paused.
    pub inserted: u64,
    pub dropped: u64,
    // Of the frames sent over the last 2 s, the share that never came or
    // came later than the cap.
    pub loss_percent: f32,
}

// A frame push would not take. Only frames FrameInfo::read accepts are held,
// so libopus never sees anything else.
#[derive(Debug)]
pub struct Refused {
    pub seq: u16,
    pub reason: StereoError,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (seq {})", self.reason, self.seq)
    }
}

impl std::error::Error for Refused {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    // Nothing to play; the next frame starts a spell.
    Idle,
    // Frames are in, the first pull has not happened yet.
    Starting,
    Playing,
}

#[derive(Clone, Copy)]
struct Slot {
    ext: i64,
    held: bool,
    len: usize,
    bytes: [u8; MAX_PACKET],
}

const EMPTY_SLOT: Slot = Slot {
    ext: 0,
    held: false,
    len: 0,
    bytes: [0; MAX_PACKET],
};

// When a frame came, as `at` = pulls so far, and its lag: `at` minus its
// number. The play point has a lag too (pulls minus the frame playing next).
// A frame is late when its lag is above the play point's, and the depth is
// how far the play point's lag sits above the smallest frame lag lately.
#[derive(Clone, Copy)]
struct Arrival {
    at: i64,
    lag: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Adjust {
    Insert,
    // A shrink: one frame less once the depth has held for 5 s and nothing
    // in that time would have been late at one frame less.
    Drop,
    // Taking off excess: depth no late frame asked for.
    Trim,
}

// One per watch. The network side pushes frames as they arrive; the mixer
// pulls one at a time when it needs more sound.
pub struct Buffer {
    decoder: Decoder,
    slots: [Slot; SLOTS],
    seen: [i64; SEEN],
    phase: Phase,
    // The render side's period in frames, 0 until the mixer says.
    period: usize,
    newest: Option<i64>,
    ends_after: Option<i64>,
    // Where the sharer's encoder last started over: anything older came from
    // before that and never plays.
    spell_first: Option<i64>,
    // The decoder starts over before the next frame it decodes.
    reset_due: bool,
    // Idle because the frames stopped, not because the sharer said so. What
    // comes next and is older than where playing restarts was later than the
    // cap, so it counts as lost rather than dropped.
    timed_out: bool,
    // The mixer stopped pulling (deafened). Until the next pull, what falls
    // out of the buffer was neither late nor thrown away to cut delay.
    paused: bool,

    play: i64,
    play_lag: i64,
    pulls: i64,
    last_change: Option<i64>,
    last_late: Option<i64>,
    // Measured: how far behind the fastest frame lately the play point is.
    depth: i64,
    // What the rules asked for: the minimum at first, a frame more for each
    // grow, one less for each shrink. A spell starts at this depth. Measured
    // depth above it is excess: the play point fell behind (a stalled render
    // thread) or started late (a slow first frame), and no late frame asked
    // for it, so it comes off without the 5 s wait.
    target: i64,
    min_lag: i64,
    arrivals: VecDeque<Arrival>,
    pending: Option<Adjust>,
    // The pull at which the pending shrink became due.
    due_since: i64,

    // Since the last pull: how many frames came faster than the fastest one
    // known at that pull, and the smallest lag of any.
    gap_floor: i64,
    gap_early: u32,
    gap_fastest: i64,

    missing_run: u32,
    // The frame the play point passed last was made up, not decoded, so if
    // it comes now it can still play next.
    made_up: bool,
    audible: bool,
    fade_in: bool,
    scratch: [f32; FRAME * CHANNELS],

    judged: Option<i64>,
    outcomes: VecDeque<bool>,
    outcomes_lost: u32,
    stats: BufferStats,
}

impl Buffer {
    pub fn new() -> Result<Buffer, StereoError> {
        Ok(Buffer {
            decoder: Decoder::new()?,
            slots: [EMPTY_SLOT; SLOTS],
            seen: [i64::MIN; SEEN],
            phase: Phase::Idle,
            period: 0,
            newest: None,
            ends_after: None,
            spell_first: None,
            reset_due: false,
            timed_out: false,
            paused: false,
            play: 0,
            play_lag: 0,
            pulls: 0,
            last_change: None,
            last_late: None,
            depth: 0,
            target: 0,
            min_lag: 0,
            arrivals: VecDeque::with_capacity(MAX_ARRIVALS),
            pending: None,
            due_since: 0,
            gap_floor: i64::MIN,
            gap_early: 0,
            gap_fastest: i64::MAX,
            missing_run: 0,
            made_up: false,
            audible: false,
            fade_in: true,
            scratch: [0.0; FRAME * CHANNELS],
            judged: None,
            outcomes: VecDeque::with_capacity(LATE_WINDOW as usize),
            outcomes_lost: 0,
            stats: BufferStats::default(),
        })
    }

    pub fn push(&mut self, packet: SoundPacket<'_>) -> Result<(), Refused> {
        FrameInfo::read(packet.frame).map_err(|reason| Refused {
            seq: packet.seq,
            reason,
        })?;
        let ext = self.extend(packet.seq);
        let newer = self.newest.is_none_or(|newest| ext > newest);
        let back_far = self
            .newest
            .is_some_and(|newest| newest - ext > RESTART_BACK);
        if back_far || (packet.first && (newer || self.phase == Phase::Idle)) {
            self.begin(ext, packet.last, true);
        } else if self.phase == Phase::Idle {
            if !newer {
                // A straggler from a spell that has ended.
                return Ok(());
            }
            self.begin(ext, packet.last, false);
        } else {
            if self.spell_first.is_some_and(|first| ext < first) {
                return Ok(());
            }
            if self.phase == Phase::Starting {
                if self.judged.is_some_and(|judged| ext <= judged) {
                    return Ok(());
                }
            } else {
                let lead = ext - self.play;
                if lead < 0 {
                    self.arrived_late(ext, packet);
                    return Ok(());
                }
                if lead >= CAP {
                    // The play point fell too far behind the frames: pulls
                    // stopped (deafened, a stalled render thread) or the
                    // sharer jumped ahead. Start again from the newest
                    // frames at the depth from before: what came since the
                    // last pull only looks early because nobody pulled.
                    let since_last_pull = self.pulls;
                    while self
                        .arrivals
                        .back()
                        .is_some_and(|arrival| arrival.at >= since_last_pull)
                    {
                        self.arrivals.pop_back();
                    }
                    self.phase = Phase::Starting;
                    self.timed_out = false;
                }
            }
        }
        if self.is_seen(ext) {
            return Ok(());
        }
        self.store(ext, packet.frame);
        self.see(ext);
        if self.newest.is_none_or(|newest| ext > newest) {
            self.newest = Some(ext);
            self.ends_after = packet.last.then_some(ext);
        }
        if self.phase == Phase::Playing {
            if !self.paused {
                self.record(ext);
            }
        } else if let Some(newest) = self.newest {
            // Anything a cap behind the newest frame cannot survive the
            // restart, so it is judged now, while it is still remembered,
            // and let go, so a buffer nobody pulls never holds more than the
            // cap.
            let oldest_kept = newest - CAP + 1;
            self.judge_through(oldest_kept - 1, self.timed_out);
            self.discard_before(oldest_kept, !self.timed_out && !self.paused);
        }
        Ok(())
    }

    // The mixer stopped pulling: deafened. It calls this every period until
    // it pulls again, which starts at the depth kept, from silence.
    pub fn pause(&mut self) {
        self.paused = true;
        self.timed_out = false;
        self.audible = false;
        self.fade_in = true;
    }

    // The render side's period in frames. A period longer than a frame pulls
    // several frames at once, which the minimum depth has to cover.
    pub fn set_period(&mut self, frames: usize) {
        self.period = frames;
    }

    // `out` must hold FRAME frames of two samples. None means the sharer is
    // silent and nothing is buffered; the mixer leaves the stream out.
    pub fn pull(&mut self, out: &mut [f32]) -> Option<Pulled> {
        assert!(
            out.len() >= FRAME * CHANNELS,
            "a pull needs room for {} samples, got {}",
            FRAME * CHANNELS,
            out.len()
        );
        match self.phase {
            Phase::Idle => {
                self.paused = false;
                return None;
            }
            Phase::Starting => self.restart(),
            Phase::Playing if self.paused => self.resume(),
            Phase::Playing => self.line_up(),
        }
        let pulled = self.next_frame(&mut out[..FRAME * CHANNELS]);
        self.pulls += 1;
        self.paused = false;
        self.gap_floor = self.min_lag;
        self.gap_early = 0;
        self.gap_fastest = i64::MAX;
        if self.phase == Phase::Playing {
            self.judge_through(self.play - CAP - 1, false);
            if self.pulls % EVALUATE == 0 {
                self.evaluate();
            }
            if let Some(newest) = self.newest
                && self.play > newest + CAP
            {
                self.go_idle(true);
            }
        }
        Some(pulled)
    }

    pub fn stats(&self) -> BufferStats {
        let depth = self.depth.max(0) as u32;
        let held = self.slots.iter().filter(|slot| slot.held).count() as u32;
        let loss_percent = if self.outcomes.is_empty() {
            0.0
        } else {
            self.outcomes_lost as f32 * 100.0 / self.outcomes.len() as f32
        };
        BufferStats {
            playing: self.phase != Phase::Idle,
            depth_frames: depth,
            depth_ms: depth * FRAME_MS,
            held_frames: held,
            loss_percent,
            ..self.stats
        }
    }

    fn extend(&self, seq: u16) -> i64 {
        match self.newest {
            Some(newest) => newest + i64::from(seq.wrapping_sub(newest as u16) as i16),
            None => i64::from(seq),
        }
    }

    // One frame, and more while the render period is longer than a frame,
    // since a period's frames are pulled together and the last of them has
    // to be in already.
    fn min_depth(&self) -> i64 {
        let per_period = self.period.div_ceil(FRAME).max(1) as i64;
        per_period.min(CAP)
    }

    // A spell starts at `ext`. `fresh` when the sharer's encoder started
    // over (a frame marked first, numbering that went back far), so the
    // decoder does too and nothing older belongs to this spell.
    fn begin(&mut self, ext: i64, last: bool, fresh: bool) {
        if self.newest.is_some_and(|newest| ext <= newest) {
            // The numbering went back. The numbers it uses now may have
            // been seen before.
            self.seen = [i64::MIN; SEEN];
            self.judged = Some(ext - 1);
        }
        if fresh {
            self.spell_first = Some(ext);
            self.reset_due = true;
            self.judged = Some(self.judged.map_or(ext - 1, |judged| judged.max(ext - 1)));
        }
        self.slots = [EMPTY_SLOT; SLOTS];
        self.newest = Some(ext);
        self.ends_after = last.then_some(ext);
        self.phase = Phase::Starting;
    }

    // Playing starts `depth - 1` frames behind the newest one, at the depth
    // the buffer learned. What came before the newest is judged as frames
    // arrive, a cap behind it, so a frame that overtook the one before it
    // still finds room.
    fn restart(&mut self) {
        let newest = self.newest.expect("a starting buffer holds a frame");
        let first_contact = self.last_change.is_none();
        let min = self.min_depth();
        let kept = self.target.clamp(min, CAP);
        // The gap before a spell is the best place for a shrink that is due,
        // and sound that never goes quiet still shrinks this way.
        let shrink = !first_contact && kept > min && self.clean() && self.settled();
        let depth = kept - i64::from(shrink);
        let start = newest - (depth - 1);
        let counted = !self.timed_out && !self.paused;
        self.discard_before(newest - (kept - 1), counted);
        self.discard_before(start, false);
        if first_contact {
            // Nothing before the first frame was ever sent to this listener.
            let first = self
                .slots
                .iter()
                .filter(|slot| slot.held)
                .map(|slot| slot.ext)
                .min();
            let before_first = first.unwrap_or(newest) - 1;
            self.judged = Some(self.judged.map_or(before_first, |j| j.max(before_first)));
        }
        self.judge_through(start - 1, self.timed_out);
        self.play = start;
        self.play_lag = self.pulls - start;
        self.depth = depth;
        self.target = depth;
        self.min_lag = self.play_lag - (depth - 1);
        // The timing before a gap or a pause says nothing about the timing
        // after it, so the history starts over, and with it the 5 s a
        // shrink waits for.
        self.arrivals.clear();
        self.last_change = Some(self.pulls);
        if shrink {
            self.stats.dropped += 1;
        }
        self.pending = None;
        self.missing_run = 0;
        self.made_up = false;
        self.fade_in = !self.audible;
        self.timed_out = false;
        self.phase = Phase::Playing;
    }

    // The first pull after a pause. Frames that came meanwhile were not
    // timed, and when they piled up past the depth kept, playing jumps to
    // that depth behind the newest, letting the rest go uncounted.
    fn resume(&mut self) {
        let kept = self.target.clamp(self.min_depth(), CAP);
        if self
            .newest
            .is_some_and(|newest| newest - (kept - 1) > self.play)
        {
            self.restart();
        }
    }

    fn go_idle(&mut self, timed_out: bool) {
        if let Some(newest) = self.newest {
            self.judge_through(newest, false);
        }
        self.slots = [EMPTY_SLOT; SLOTS];
        self.made_up = false;
        self.phase = Phase::Idle;
        self.timed_out = timed_out;
        if !timed_out {
            // The sharer said it stopped, so its next spell starts from a
            // reset encoder. After an outage its encoder carried on, and so
            // does this decoder.
            let _ = self.decoder.reset();
            self.reset_due = false;
        }
        self.audible = false;
        self.fade_in = true;
        self.pending = None;
    }

    // A frame after its turn: its stand-in has played. When nothing came
    // late in the 5 s before, this is the clocks drifting apart by a frame
    // or a lone spike, and the buffer grows at once: the frame plays next
    // and the stand-in becomes the frame added, which costs 10 ms of
    // concealment and nothing lost. Left to the 2 s count, a drift crossing
    // would make every frame after it late until the next check, up to
    // 100 ms of concealment and silence.
    fn arrived_late(&mut self, ext: i64, packet: SoundPacket<'_>) {
        if self.is_seen(ext) {
            return;
        }
        if self.judged.is_some_and(|judged| ext <= judged) {
            // Later than the cap: already counted as lost when its time
            // passed.
            return;
        }
        self.see(ext);
        self.stats.late += 1;
        if self.newest.is_none_or(|newest| ext > newest) {
            self.newest = Some(ext);
            self.ends_after = packet.last.then_some(ext);
        }
        let lone = self
            .last_late
            .is_none_or(|at| self.pulls - at > CLEAN_WINDOW);
        self.last_late = Some(self.pulls);
        let fits = self.depth < CAP && self.newest.is_some_and(|newest| newest - ext < CAP);
        if lone && fits && !self.paused && self.made_up && ext == self.play - 1 {
            self.store(ext, packet.frame);
            self.play = ext;
            self.play_lag += 1;
            self.depth += 1;
            self.target = self.target.max(self.depth);
            self.last_change = Some(self.pulls);
            self.pending = None;
            self.made_up = false;
            self.stats.inserted += 1;
        }
        if !self.paused {
            self.record(ext);
        }
    }

    // Two or more frames came in faster than any of the last 2 s with no
    // pull between them: the render side stalled and the play point fell
    // that far behind, which shows as excess depth. The history moves to the
    // new timing, without the frames that came during the stall, so it does
    // not stand in the way of taking the excess off. A path that suddenly
    // got faster looks the same and is handled the same.
    fn line_up(&mut self) {
        if self.gap_early < 2 {
            return;
        }
        let slip = self.gap_floor - self.gap_fastest;
        let now = self.pulls;
        while self
            .arrivals
            .back()
            .is_some_and(|arrival| arrival.at >= now)
        {
            self.arrivals.pop_back();
        }
        for arrival in &mut self.arrivals {
            arrival.lag -= slip;
        }
    }

    fn excess(&self) -> i64 {
        (self.depth - self.target).max(0)
    }

    fn record(&mut self, ext: i64) {
        if self.arrivals.len() == MAX_ARRIVALS {
            self.arrivals.pop_front();
        }
        let lag = self.pulls - ext;
        self.arrivals.push_back(Arrival {
            at: self.pulls,
            lag,
        });
        self.gap_fastest = self.gap_fastest.min(lag);
        if lag < self.gap_floor {
            self.gap_early += 1;
        }
        if lag < self.min_lag {
            self.min_lag = lag;
            self.depth = (self.play_lag - lag + 1).clamp(1, CAP);
        }
    }

    // The rules, run every 100 ms of play. "Late" is measured against the
    // depth being judged: grow when more than 1 percent of the last 2 s
    // would be late at the current depth; shrink when nothing in the last
    // 5 s would have been late at one frame less, at most once per 5 s.
    // Counting late frames against the depth they arrived at instead would
    // keep growing for 2 s after one burst. Excess comes off under the same
    // clean test without the 5 s wait.
    fn evaluate(&mut self) {
        let now = self.pulls;
        while self
            .arrivals
            .front()
            .is_some_and(|arrival| arrival.at < now - CLEAN_WINDOW)
        {
            self.arrivals.pop_front();
        }
        self.measure_depth();
        let late = self
            .arrivals
            .iter()
            .filter(|arrival| arrival.at >= now - LATE_WINDOW && arrival.lag > self.play_lag)
            .count() as i64;
        if late * 100 > LATE_WINDOW {
            // Late frames back all of the depth there is now.
            self.target = self.target.max(self.depth);
            self.pending = (self.depth < CAP).then_some(Adjust::Insert);
            return;
        }
        let min = self.min_depth();
        let clean = self.clean();
        // A shrink waits up to 2 s for a quiet frame, and only while it is
        // still one the network allows.
        let still_due = match self.pending {
            Some(Adjust::Drop) => clean && self.depth > min,
            Some(Adjust::Trim) => clean && self.excess() > 0,
            _ => true,
        };
        if !still_due {
            self.pending = None;
        }
        if self.pending.is_some() || self.depth <= min || !clean {
            return;
        }
        if self.excess() > 0 {
            self.pending = Some(Adjust::Trim);
        } else if self.settled() && self.target > min {
            self.pending = Some(Adjust::Drop);
        } else {
            return;
        }
        self.due_since = now;
    }

    // Nothing in the history would have been late with one frame less.
    fn clean(&self) -> bool {
        self.arrivals
            .iter()
            .all(|arrival| arrival.lag < self.play_lag)
    }

    // 5 s of play since the depth last changed or playing last started.
    fn settled(&self) -> bool {
        self.last_change
            .is_some_and(|change| self.pulls - change >= CLEAN_WINDOW)
    }

    // The depth is how far the play point sits behind the fastest frame of
    // the last 2 s. With no frame in that time it stays as it was.
    fn measure_depth(&mut self) {
        let since = self.pulls - LATE_WINDOW;
        if let Some(min_lag) = self
            .arrivals
            .iter()
            .filter(|arrival| arrival.at >= since)
            .map(|arrival| arrival.lag)
            .min()
        {
            self.min_lag = min_lag;
            self.depth = (self.play_lag - min_lag + 1).clamp(1, CAP);
        }
    }

    fn next_frame(&mut self, out: &mut [f32]) -> Pulled {
        if self.pending.is_none() && self.depth < self.min_depth() {
            self.pending = Some(Adjust::Insert);
        }
        if self.pending == Some(Adjust::Insert) {
            self.pending = None;
            if self.depth < CAP {
                self.play_lag += 1;
                self.depth += 1;
                self.target = self.target.max(self.depth);
                self.last_change = Some(self.pulls);
                self.stats.inserted += 1;
                let source = self.make_up(out);
                return Pulled {
                    frames: FRAME,
                    source,
                };
            }
        }
        // Before the first frame of a spell this is the wait the depth asks
        // for, and a frame that comes in time for its turn after all plays.
        let Some(slot) = self.take(self.play) else {
            return self.missing(out);
        };
        if !self.decode(&slot, out) {
            return self.missing(out);
        }
        if let Some(pulled) = self.shrink(out) {
            return pulled;
        }
        self.play += 1;
        Pulled {
            frames: FRAME,
            source: Source::Packet(slot.ext as u16),
        }
    }

    fn missing(&mut self, out: &mut [f32]) -> Pulled {
        if let Some(newest) = self.newest
            && self.ends_after == Some(newest)
            && self.play > newest
        {
            // The sharer's sound stopped. One faded frame of concealment
            // finishes the codec's overlap and brings the level down to
            // nothing.
            if self.audible && self.decoder.conceal(out).is_ok() {
                fade_out(out);
            } else {
                out.fill(0.0);
            }
            self.play += 1;
            self.go_idle(false);
            return Pulled {
                frames: FRAME,
                source: Source::End,
            };
        }
        self.play += 1;
        self.made_up = true;
        let source = self.make_up(out);
        Pulled {
            frames: FRAME,
            source,
        }
    }

    // A frame that is not the sharer's: concealment while sound plays, so it
    // carries on from what came before, faded out once it has gone on for
    // CONCEAL_FRAMES, silence after that.
    fn make_up(&mut self, out: &mut [f32]) -> Source {
        self.missing_run += 1;
        if self.audible
            && self.missing_run <= CONCEAL_FRAMES + 1
            && self.decoder.conceal(out).is_ok()
        {
            if self.missing_run > CONCEAL_FRAMES {
                fade_out(out);
                self.audible = false;
                self.fade_in = true;
            }
            self.stats.concealed += 1;
            return Source::Concealed;
        }
        out.fill(0.0);
        self.audible = false;
        self.fade_in = true;
        Source::Silence
    }

    // Straight after concealment Opus blends the frame in itself, over the
    // 2.5 ms its frames overlap by; a fade from silence there would click.
    // After silence the frame fades in over FADE. A decoder that starts
    // over while sound plays starts from nothing, so the sound before it is
    // carried on by concealment and blended out instead.
    fn decode(&mut self, slot: &Slot, out: &mut [f32]) -> bool {
        let blend = self.reset_due && self.audible;
        if blend && self.decoder.conceal(&mut self.scratch).is_err() {
            self.scratch.fill(0.0);
        }
        if std::mem::take(&mut self.reset_due) {
            let _ = self.decoder.reset();
        }
        let decoded = self
            .decoder
            .decode(&slot.bytes[..slot.len], out)
            .is_ok_and(|frames| frames == FRAME);
        if !decoded {
            return false;
        }
        if blend {
            crossfade(&self.scratch, out, FADE);
        } else if self.fade_in {
            fade_in(out);
        }
        self.fade_in = false;
        self.audible = true;
        self.missing_run = 0;
        self.made_up = false;
        true
    }

    // A shrink that is due drops the frame just decoded when it is quiet on
    // both channels, or after 2 s of waiting for one, whatever it holds. The
    // frame after it is decoded too, so the decoder's history has no gap,
    // and the dropped frame is blended into it.
    fn shrink(&mut self, out: &mut [f32]) -> Option<Pulled> {
        let adjust = self.pending.filter(|&adjust| adjust != Adjust::Insert)?;
        if self.depth <= self.min_depth() {
            return None;
        }
        let waited = self.pulls - self.due_since >= QUIET_WAIT;
        if !waited && !quiet(out) {
            return None;
        }
        let ext = self.play + 1;
        let next = *self.slot(ext)?;
        let decoded = self
            .decoder
            .decode(&next.bytes[..next.len], &mut self.scratch)
            .is_ok_and(|frames| frames == FRAME);
        if !decoded {
            return None;
        }
        self.take(ext);
        let blend = if waited { LOUD_BLEND } else { FADE };
        crossfade(out, &mut self.scratch, blend);
        out.copy_from_slice(&self.scratch);
        self.play += 2;
        self.play_lag -= 1;
        self.depth -= 1;
        self.due_since = self.pulls;
        if adjust == Adjust::Trim {
            // The rest of the excess comes off at the next quiet frames, not
            // after the next check: nothing about the network has changed.
            self.pending = (self.excess() > 0).then_some(Adjust::Trim);
        } else {
            self.target = (self.target - 1).max(self.min_depth());
            self.last_change = Some(self.pulls);
            self.pending = None;
        }
        self.stats.dropped += 1;
        Some(Pulled {
            frames: FRAME,
            source: Source::Packet(ext as u16),
        })
    }

    // Frames that came but will never play. `counted` says whether they are
    // dropped for delay; when not, they were later than the cap and the loss
    // count has them, or the mixer had paused.
    fn discard_before(&mut self, oldest_kept: i64, counted: bool) {
        for slot in &mut self.slots {
            if slot.held && slot.ext < oldest_kept {
                if counted {
                    self.stats.dropped += 1;
                }
                slot.held = false;
            }
        }
    }

    fn slot(&self, ext: i64) -> Option<&Slot> {
        let slot = &self.slots[ext.rem_euclid(SLOTS as i64) as usize];
        (slot.held && slot.ext == ext).then_some(slot)
    }

    // A frame leaves its slot as it plays, so every held slot is still to
    // come.
    fn take(&mut self, ext: i64) -> Option<Slot> {
        let slot = &mut self.slots[ext.rem_euclid(SLOTS as i64) as usize];
        if !slot.held || slot.ext != ext {
            return None;
        }
        slot.held = false;
        Some(*slot)
    }

    // `bytes` is a frame FrameInfo::read accepted, which refuses anything
    // longer than MAX_PACKET, so it fits.
    fn store(&mut self, ext: i64, bytes: &[u8]) {
        let slot = &mut self.slots[ext.rem_euclid(SLOTS as i64) as usize];
        if slot.held && slot.ext > ext {
            return;
        }
        slot.ext = ext;
        slot.held = true;
        slot.len = bytes.len();
        slot.bytes[..bytes.len()].copy_from_slice(bytes);
    }

    fn is_seen(&self, ext: i64) -> bool {
        self.seen[ext.rem_euclid(SEEN as i64) as usize] == ext
    }

    fn see(&mut self, ext: i64) {
        self.seen[ext.rem_euclid(SEEN as i64) as usize] = ext;
    }

    // Settles, for every frame number up to `upto`, whether it counts as
    // lost, and keeps the last 2 s of that for the loss percentage. A big
    // jump in numbering is judged over its last 2 s only, so a sharer
    // restarting their count does not read as thousands of losses.
    fn judge_through(&mut self, upto: i64, arrived_counts_as_lost: bool) {
        let from = match self.judged {
            Some(judged) if judged >= upto => return,
            Some(judged) => (judged + 1).max(upto - LATE_WINDOW + 1),
            None => upto + 1,
        };
        for ext in from..=upto {
            let lost = arrived_counts_as_lost || !self.is_seen(ext);
            if lost {
                self.stats.lost += 1;
                self.outcomes_lost += 1;
            }
            if self.outcomes.len() == LATE_WINDOW as usize
                && self.outcomes.pop_front() == Some(true)
            {
                self.outcomes_lost -= 1;
            }
            self.outcomes.push_back(lost);
        }
        self.judged = Some(upto);
    }
}

fn quiet(frame: &[f32]) -> bool {
    let (pairs, _) = frame.as_chunks::<2>();
    let (left, right) = pairs
        .iter()
        .fold((0.0f32, 0.0f32), |(left, right), [l, r]| {
            (left + l * l, right + r * r)
        });
    let under = QUIET_RMS * QUIET_RMS * pairs.len() as f32;
    left < under && right < under
}

// Up from silence over the frame's first FADE frames.
fn fade_in(frame: &mut [f32]) {
    let (pairs, _) = frame.as_chunks_mut::<2>();
    for (i, [left, right]) in pairs.iter_mut().take(FADE).enumerate() {
        let gain = (i as f32 + 1.0) / FADE as f32;
        *left *= gain;
        *right *= gain;
    }
}

// Down to silence over the whole frame.
fn fade_out(frame: &mut [f32]) {
    let (pairs, _) = frame.as_chunks_mut::<2>();
    let len = pairs.len() as f32;
    for (i, [left, right]) in pairs.iter_mut().enumerate() {
        let gain = 1.0 - (i as f32 + 1.0) / len;
        *left *= gain;
        *right *= gain;
    }
}

// Blends `from` into the start of `into` over `len` frames; past that `into`
// is left alone.
fn crossfade(from: &[f32], into: &mut [f32], len: usize) {
    let (from, _) = from.as_chunks::<2>();
    let (into, _) = into.as_chunks_mut::<2>();
    let len = len.min(into.len()).min(from.len());
    for (i, (into, from)) in into.iter_mut().zip(from).take(len).enumerate() {
        let gain = (i as f32 + 1.0) / (len as f32 + 1.0);
        into[0] = from[0] * (1.0 - gain) + into[0] * gain;
        into[1] = from[1] * (1.0 - gain) + into[1] * gain;
    }
}
