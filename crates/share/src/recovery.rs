// How each side answers loss, kept apart from the threads, devices and link
// they run on so every decision can be tried with plain frame numbers. The
// viewer's side decides what goes back after a frame, a failed decode, a
// drop, or a frame held back for an IDR; the sharer's side what the encoder
// is told when that arrives.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use encode::{Encoder, Recovery};

use crate::numbers::SharerNumbers;

// What a viewer sends back to the sharer: in the room, on the control
// channel through the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Back {
    // Frames first to last were dropped on the way or could not be decoded.
    // The sharer answers nothing about a frame it has not sent.
    Recover { first: u32, last: u32 },
    // Nothing the viewer gets decodes: it has had no IDR since the stream
    // started, or frames go on failing after they were reported. `seen` is
    // the frame that showed it.
    Idr { seen: u32 },
    // Shard loss over the last 2 s, for the parity: once a second, and at
    // once when a frame needed parity or was lost while the last number
    // sent was none or zero.
    Loss(Option<f32>),
}

// Asking for an IDR again while the first one is on its way would only make
// another. The sharer also drops an ask an IDR already answered.
const IDR_ASK_GAP: Duration = channels::video::RECOVER_GAP;

// Failed decodes are reported as the reassembler reports drops: the first
// after a quiet spell at once, the ones in the next 20 ms together when it
// ends. A stream that fails frame after frame, as a friend's PC can send on
// purpose, then makes one message per gap and not one per frame, which would
// fill the room's control queue and push out a "stop watching" behind them.
const FAILED_GAP: Duration = channels::video::RECOVER_GAP;

// Every frame that did not decode is reported, so NVENC never predicts from
// one the decoder does not have. The frame after a report is encoded after
// it, so the third failure in a row means invalidation did not bring the
// decoder back, and only an IDR will. Seen at 20 percent loss: without this
// FFmpeg gave no picture for two seconds after one lost frame.
const IDR_AFTER_FAILURES: u32 = 3;

// The GPU's decoder refusing the stream comes back with every IDR, and the
// loopback's stream never changes, so the run stops with the decoder's
// sentence. Not at the first one: FFmpeg's refusal also covers video memory
// running out, which a game can cause for a moment.
const REFUSALS_TO_STOP: u32 = 3;

// Frames asked for in one recover request are looked at one by one. A
// request this long only comes after an outage, and the first frames of it
// say all there is to say.
const MOST_RECOVERED: u32 = 64;

// Recent invalidations remembered on the sharer's side. A request about the
// frames one of them cut off comes within a round trip or two.
const CUTS_KEPT: usize = 8;

// Frames lost on this PC before they left, remembered so the viewers'
// reports of them can be told apart from loss on the way. The pacer lets a
// frame go only when its thread falls a whole frame behind, so a few do.
const LOST_HERE_KEPT: usize = 16;

// A still screen sends its last picture once more this long after the last
// one went out. A frame lost whole leaves no trace at the viewer until a
// later one shows the gap, and on a still screen no later one comes. Once
// the screen stands still for 100 ms, and at most once for each picture.
pub(crate) const REPEAT_AFTER: Duration = Duration::from_millis(100);

// An IDR that answers a loss or a watcher's IDR ask waits until the last IDR
// has used at most this share of the upload setting: its bits spread over
// the time since it went out.
// A friend's PC that reports every frame lost then costs the sharer at most
// a quarter of the setting in IDRs, on every encoder: one of 90 KB at
// 15 Mbit/s every 192 ms, one of the software encoder's 450 KB at 8 Mbit/s
// every 1.8 s. NVENC's invalidations never wait. The IDR for someone who
// starts watching and the first of each encoder go at once, and the floor
// counts from them too.
pub(crate) const IDR_FLOOR_SHARE: f64 = 0.25;

#[derive(Default)]
pub struct Asks {
    had_idr: bool,
    // Frames in a row that did not decode.
    failing: u32,
    // Refusals of the stream since the last picture.
    refusals: u32,
    last_idr_ask: Option<Instant>,
    // Failed decodes not reported yet, oldest and newest, and when the last
    // report of them went.
    failed: Option<(u32, u32)>,
    last_failed_report: Option<Instant>,
    // The newest frame the reassembler dropped: the IDR that ends a hold has
    // to come after it.
    last_dropped: Option<u32>,
    // The reassembler's count of frames held back for an IDR, when last
    // looked at.
    held: u64,
    // The loss number sent last, and whether a frame was repaired or dropped
    // since.
    told_loss: Option<f32>,
    lost_since: bool,
}

impl Asks {
    // A whole frame out of the reassembler. False when it cannot decode: the
    // stream started with an IDR this side never got. The reassembler cannot
    // know, since it first heard of the stream after that frame.
    pub fn arrived(&mut self, number: u32, idr: bool, now: Instant, back: &mut Vec<Back>) -> bool {
        if self.had_idr || idr {
            return true;
        }
        self.ask_for_idr(number, now, back);
        false
    }

    // A picture has been decoded from an IDR on: frames lost from now on
    // are ones the viewer would have shown.
    pub fn had_idr(&self) -> bool {
        self.had_idr
    }

    pub fn decoded(&mut self, idr: bool) {
        self.had_idr |= idr;
        self.failing = 0;
        self.refusals = 0;
    }

    // A frame that came whole and still did not decode is lost like one that
    // never came. `refused` is the GPU's decoder refusing the stream itself.
    pub fn failed(&mut self, number: u32, refused: bool, now: Instant, back: &mut Vec<Back>) {
        if refused {
            self.refusals += 1;
        }
        if !self.had_idr {
            self.ask_for_idr(number, now, back);
            return;
        }
        let first = self.failed.map_or(number, |(first, _)| first);
        self.failed = Some((first, number));
        self.report_failed(now, back);
        self.failing += 1;
        if self.failing >= IDR_AFTER_FAILURES {
            self.ask_for_idr(number, now, back);
        }
    }

    // When tick() has to be called for failed decodes gathered in a gap.
    pub fn deadline(&self) -> Option<Instant> {
        self.failed
            .and(self.last_failed_report)
            .map(|at| at + FAILED_GAP)
    }

    // Reports the failed decodes gathered, once their gap is over.
    pub fn tick(&mut self, now: Instant, back: &mut Vec<Back>) {
        if self.failed.is_some() {
            self.report_failed(now, back);
        }
    }

    // A frame of a codec the decoder is not for, before that codec's IDR:
    // nothing of it decodes until the IDR, so it is asked for at once, as
    // for a stream whose first IDR never came.
    pub fn other_codec(&mut self, number: u32, now: Instant, back: &mut Vec<Back>) {
        self.ask_for_idr(number, now, back);
    }

    pub fn refused_too_often(&self) -> bool {
        self.refusals >= REFUSALS_TO_STOP
    }

    // Before the first picture a frame lost is most often the one caught
    // half sent when watching began, which says nothing about the link: it
    // does not send the loss at once, or one join would put every watcher's
    // parity at its ceiling for a second.
    pub fn dropped(&mut self, last: u32) {
        self.last_dropped = Some(last);
        self.lost_since |= self.had_idr;
    }

    // A frame came out with parity standing in for a data shard. Called
    // after the frame was decoded, so a repaired first IDR counts.
    pub fn repaired(&mut self) {
        self.lost_since |= self.had_idr;
    }

    // The loss number, once a second.
    pub fn loss(&mut self, loss: Option<f32>, back: &mut Vec<Back>) {
        back.push(Back::Loss(loss));
        self.told_loss = loss;
        self.lost_since = false;
    }

    // After the reassembler's events: the first loss goes back at once, so
    // the parity follows it within a round trip and not a second later. Only
    // a repaired or dropped frame starts the look, since a frame's parity
    // that is still on its way counts as lost for a moment. `loss` is the
    // reassembler's number now.
    pub fn first_loss(&mut self, loss: impl FnOnce() -> Option<f32>, back: &mut Vec<Back>) {
        if !std::mem::take(&mut self.lost_since) || self.told_loss.is_some_and(|told| told > 0.0) {
            return;
        }
        if let Some(pct) = loss().filter(|&pct| pct > 0.0) {
            back.push(Back::Loss(Some(pct)));
            self.told_loss = Some(pct);
        }
    }

    fn report_failed(&mut self, now: Instant, back: &mut Vec<Back>) {
        let quiet = self
            .last_failed_report
            .is_none_or(|at| now.saturating_duration_since(at) >= FAILED_GAP);
        if let (true, Some((first, last))) = (quiet, self.failed) {
            back.push(Back::Recover { first, last });
            self.failed = None;
            self.last_failed_report = Some(now);
        }
    }

    // `held` is the reassembler's count of frames held back for an IDR. The
    // safety net for a hold the sharer's answer does not end: the
    // reassembler guesses what a frame lost with its header does from the
    // frames before it, and an encoder that invalidates answers the recover
    // request with no IDR at all. Nothing would show again until some other
    // IDR. The reassembler holds frames only after a drop, so there is
    // always a frame for the IDR to come after.
    pub fn held(&mut self, held: u64, now: Instant, back: &mut Vec<Back>) {
        if held <= self.held {
            return;
        }
        self.held = held;
        if let Some(dropped) = self.last_dropped {
            self.ask_for_idr(dropped, now, back);
        }
    }

    fn ask_for_idr(&mut self, seen: u32, now: Instant, back: &mut Vec<Back>) {
        if self
            .last_idr_ask
            .is_none_or(|at| now.saturating_duration_since(at) >= IDR_ASK_GAP)
        {
            back.push(Back::Idr { seen });
            self.last_idr_ask = Some(now);
        }
    }
}

pub struct Answers {
    newest: Option<u64>,
    last_idr: Option<u64>,
    // The encoder said its next frame is an IDR.
    idr_coming: bool,
    force_idr: bool,
    // An IDR that answers a loss or an IDR ask, waiting for the floor. The
    // encoder has not been asked: asking commits it to the IDR.
    held: bool,
    // The last IDR that went to the pacer, and the one before it, which
    // counts again if the pacer lets the last one go unsent.
    last_idr_out: Option<IdrOut>,
    idr_out_before: Option<IdrOut>,
    // The upload setting, in bits a second.
    bitrate: u32,
    // Frames an invalidation already cut off, first to last: nothing encoded
    // after it refers to any of them (encode::Recovery::Invalidated).
    cuts: VecDeque<(u64, u64)>,
    // Frames lost here before they left, newest last.
    lost_here: VecDeque<u64>,
    // When a still screen's last picture goes out once more.
    repeat_at: Option<Instant>,
}

#[derive(Clone, Copy)]
struct IdrOut {
    index: u64,
    at: Instant,
    bytes: usize,
}

// Why a still screen sends its last picture again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Again {
    // Someone waits for an IDR, or the newest frame sent was cut off by an
    // invalidation: the viewers show an older picture than the screen, and
    // encoded again it predicts from a frame before the cut.
    Answer,
    // Nothing went out for REPEAT_AFTER.
    Repeat,
}

impl Answers {
    // `bitrate` is the upload setting, in bits a second.
    pub fn new(bitrate: u32) -> Answers {
        Answers {
            newest: None,
            last_idr: None,
            idr_coming: false,
            force_idr: false,
            held: false,
            last_idr_out: None,
            idr_out_before: None,
            bitrate,
            cuts: VecDeque::new(),
            lost_here: VecDeque::new(),
            repeat_at: None,
        }
    }

    // The floor follows the setting: a lower one makes the last IDR's
    // quarter take longer.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.bitrate = bitrate;
    }

    // For the frame about to be encoded, at `now`.
    pub fn take_force_idr(&mut self, now: Instant) -> bool {
        let released = self.held && !self.within_floor(now);
        if released {
            self.held = false;
        }
        std::mem::take(&mut self.force_idr) || released
    }

    // The next frame is an IDR whatever was sent before and however recent
    // the last IDR, as for someone who just started watching.
    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }

    // Someone waits for an IDR that may go now: asked for, the encoder's
    // answer to a loss, or one the floor held until now. On a still screen
    // only a frame sent again brings it.
    fn idr_due(&self, now: Instant) -> bool {
        self.force_idr || self.idr_coming || (self.held && !self.within_floor(now))
    }

    pub fn encoded(&mut self, index: u64, idr: bool) {
        self.newest = Some(index);
        self.idr_coming = false;
        if idr {
            self.last_idr = Some(index);
            // It answers every loss reported before it.
            self.held = false;
        }
    }

    // A frame went to the pacer. Every picture but a repeat is repeated
    // once if the screen then stands still; a repeat is not.
    pub fn went_out(&mut self, now: Instant, again: Option<Again>) {
        self.repeat_at = (again != Some(Again::Repeat)).then(|| now + REPEAT_AFTER);
    }

    // IDR `index` of `bytes` went to the pacer: the floor counts from it,
    // whatever made it. One too big to send never went and does not count.
    pub fn idr_went_out(&mut self, index: u64, now: Instant, bytes: usize) {
        let out = IdrOut {
            index,
            at: now,
            bytes,
        };
        self.idr_out_before = self.last_idr_out.replace(out);
    }

    // What a still screen sends now: its last picture again, or nothing.
    pub fn again(&self, now: Instant) -> Option<Again> {
        if self.idr_due(now) || self.newest_cut() {
            Some(Again::Answer)
        } else if self.repeat_at.is_some_and(|at| now >= at) {
            Some(Again::Repeat)
        } else {
            None
        }
    }

    // The frame this PC lost itself: the pacer let it go, or it was too big
    // to send. It is recovered before any viewer reports it, at once or,
    // when only an IDR recovers it, once the floor ends.
    //
    // An IDR the pacer let go never left, so the floor counts from the one
    // before it again, and the IDR that stands in for it goes at once, as
    // that one did: it may have been for someone who starts watching, whom
    // the floor never holds.
    pub fn lost_here(
        &mut self,
        encoder: &mut dyn Encoder,
        number: u32,
        now: Instant,
        numbers: &mut SharerNumbers,
    ) {
        if self
            .last_idr_out
            .is_some_and(|out| out.index == u64::from(number))
        {
            self.last_idr_out = self.idr_out_before.take();
            self.force_idr = true;
            numbers.idrs_let_go += 1;
        }
        self.recover(encoder, number, number, now, numbers);
        if self.lost_here.len() == LOST_HERE_KEPT {
            self.lost_here.pop_front();
        }
        self.lost_here.push_back(u64::from(number));
    }

    // Frames `first` to `last` were lost: on the way, before they left, or
    // in the viewer's decoder. Frame numbers are the encoder's indices cut
    // to 32 bits, and a run does not reach 2^32 frames, so they convert back
    // as they are and a range that wraps names frames never sent.
    //
    // Reporting a frame again is not harmless: NVENC would invalidate the
    // good frames encoded since the first report, which then cost more. So a
    // frame an earlier answer already covers is left alone.
    //
    // A report that names a frame not sent yet is left alone too. No viewer
    // can have lost one, and NVENC answers a frame it never made with an
    // IDR, as the Media Foundation encoders answer everything: a friend's PC
    // naming a new future frame each time would get one of about 90 KB per
    // request.
    //
    // A frame only an IDR recovers waits for the floor (IDR_FLOOR_SHARE),
    // and a later one joins the IDR already waiting. The encoder says which
    // those are before it is asked (encode::Encoder::needs_idr), since
    // asking commits it. On the Media Foundation encoders that is every
    // frame. On NVENC it is a frame with no valid one older than it left in
    // the reference memory: the last IDR itself, a frame reported once the
    // one before it has slid out of that memory (11 frames later at 120 fps,
    // about 92 ms), or, for a friend's PC that reports each frame as it
    // arrives, the first report after the frames invalidated one by one have
    // pushed the last IDR out of that memory, 12 frames on at 120 fps. Every
    // other loss on NVENC is invalidated at once. A driver that refuses an
    // invalidation still ends in an IDR that goes at once. NVIDIA's driver
    // took every invalidation a second of such a flood asks for in encode's
    // GPU tests (tests/nvenc.rs).
    pub fn recover(
        &mut self,
        encoder: &mut dyn Encoder,
        first: u32,
        last: u32,
        now: Instant,
        numbers: &mut SharerNumbers,
    ) {
        if first > last || !self.sent(last) {
            numbers.unsent += 1;
            return;
        }
        let (first_index, last_index) = (u64::from(first), u64::from(last));
        numbers.reported_lost_here += self
            .lost_here
            .iter()
            .filter(|&&index| (first_index..=last_index).contains(&index))
            .count() as u64;
        numbers.recoveries += 1;
        let count = (last - first).min(MOST_RECOVERED - 1) + 1;
        let (mut waits, mut idr, mut invalidated) = (false, false, false);
        for offset in 0..count {
            let index = u64::from(first + offset);
            if self.covered(index) {
                continue;
            }
            if encoder.needs_idr(index) && (self.held || self.within_floor(now)) {
                waits |= !self.held;
                self.held = true;
                continue;
            }
            match encoder.recover(index) {
                Recovery::Idr => {
                    idr = true;
                    self.idr_coming = true;
                }
                Recovery::Invalidated => {
                    invalidated = true;
                    if self.cuts.len() == CUTS_KEPT {
                        self.cuts.pop_front();
                    }
                    self.cuts.push_back((index, self.newest.unwrap_or(index)));
                }
            }
        }
        // An IDR the encoder makes at once answers what waited as well.
        numbers.idr_answers += u64::from(idr || waits);
        numbers.floor_waits += u64::from(waits && !idr);
        numbers.invalidated += u64::from(invalidated);
        numbers.covered += u64::from(!waits && !idr && !invalidated);
    }

    // The viewer has no picture to build on after frame `seen`. An IDR sent
    // after that frame is on its way already; if it was lost too, the viewer
    // asks again. An ask about a frame not sent yet would otherwise force an
    // IDR every time. The IDR waits for the floor, as one answering a loss.
    pub fn idr_asked(&mut self, seen: u32, now: Instant, numbers: &mut SharerNumbers) {
        if !self.sent(seen) {
            numbers.unsent += 1;
            return;
        }
        let answered = self.idr_coming
            || self.force_idr
            || self.held
            || self.last_idr.is_some_and(|idr| idr > u64::from(seen));
        if answered {
            return;
        }
        numbers.idr_asks += 1;
        if self.within_floor(now) {
            self.held = true;
            numbers.floor_waits += 1;
        } else {
            self.force_idr = true;
        }
    }

    // Until the last IDR has used IDR_FLOOR_SHARE of the setting.
    pub(crate) fn within_floor(&self, now: Instant) -> bool {
        let Some(out) = self.last_idr_out else {
            return false;
        };
        let floor = out.bytes as f64 * 8.0 / (IDR_FLOOR_SHARE * f64::from(self.bitrate.max(1)));
        now.saturating_duration_since(out.at).as_secs_f64() < floor
    }

    fn sent(&self, number: u32) -> bool {
        self.newest
            .is_some_and(|newest| u64::from(number) <= newest)
    }

    // An invalidation since the newest frame was encoded: a cut reaches up
    // to the newest frame at the time.
    fn newest_cut(&self) -> bool {
        self.newest.is_some_and(|newest| {
            self.cuts
                .iter()
                .any(|&(first, last)| (first..=last).contains(&newest))
        })
    }

    fn covered(&self, index: u64) -> bool {
        self.idr_coming
            || self.force_idr
            || self.last_idr.is_some_and(|idr| idr > index)
            || self
                .cuts
                .iter()
                .any(|&(first, last)| (first..=last).contains(&index))
    }
}

#[cfg(test)]
mod tests {
    use channels::video::{Event, FrameFacts, Packetizer, Reassembler};
    use encode::{AccessUnit, Codec, EncodeError, Frame, Kind};

    use super::*;
    use crate::PAYLOAD_INTERNET;

    // For the tests that send no IDR, where the setting changes nothing.
    const BITRATE: u32 = 15_000_000;

    // Answers every recover request the same way and remembers the frames.
    struct Fake {
        answer: Recovery,
        recovered: Vec<u64>,
    }

    impl Fake {
        fn new(answer: Recovery) -> Fake {
            Fake {
                answer,
                recovered: Vec::new(),
            }
        }
    }

    impl Encoder for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        // NVENC invalidates; the Media Foundation encoders answer with an IDR.
        fn kind(&self) -> Kind {
            match self.answer {
                Recovery::Invalidated => Kind::Nvenc,
                Recovery::Idr => Kind::MfHardware,
            }
        }

        fn codec(&self) -> Codec {
            Codec::H264
        }

        fn encode(&mut self, _: &Frame<'_>) -> Result<AccessUnit, EncodeError> {
            panic!("the recovery tests never encode")
        }

        fn set_bitrate(&mut self, _: u32) -> Result<(), EncodeError> {
            Ok(())
        }

        fn recover(&mut self, lost_frame_index: u64) -> Recovery {
            self.recovered.push(lost_frame_index);
            self.answer
        }

        fn invalidates(&self) -> bool {
            self.answer == Recovery::Invalidated
        }
    }

    fn encoded(answers: &mut Answers, frames: std::ops::RangeInclusive<u64>) {
        for index in frames {
            answers.encoded(index, index == 0);
        }
    }

    #[test]
    fn frames_before_the_first_idr_ask_for_one_once_per_gap() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let start = Instant::now();
        assert!(!asks.arrived(3, false, start, &mut back));
        assert!(!asks.arrived(4, false, start + Duration::from_millis(8), &mut back));
        assert_eq!(back, [Back::Idr { seen: 3 }]);
        assert!(!asks.arrived(5, false, start + IDR_ASK_GAP, &mut back));
        assert_eq!(back[1..], [Back::Idr { seen: 5 }]);
        // A failed decode before the first IDR has nothing to invalidate.
        back.clear();
        asks.failed(6, false, start + IDR_ASK_GAP * 2, &mut back);
        assert_eq!(back, [Back::Idr { seen: 6 }]);
        back.clear();
        assert!(asks.arrived(7, true, start + IDR_ASK_GAP * 3, &mut back));
        asks.decoded(true);
        assert!(asks.arrived(8, false, start + IDR_ASK_GAP * 3, &mut back));
        assert!(back.is_empty());
    }

    #[test]
    fn failed_decodes_are_reported() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let now = Instant::now();
        let ms = |ms: u64| now + Duration::from_millis(ms);
        asks.decoded(true);
        assert_eq!(asks.deadline(), None);
        // The first after a quiet spell goes at once; the next ones in its
        // gap together when the gap is over.
        asks.failed(10, false, now, &mut back);
        assert_eq!(
            back,
            [Back::Recover {
                first: 10,
                last: 10
            }]
        );
        back.clear();
        asks.failed(11, false, ms(8), &mut back);
        assert!(back.is_empty());
        assert_eq!(asks.deadline(), Some(now + FAILED_GAP));
        asks.failed(12, false, ms(16), &mut back);
        assert_eq!(back, [Back::Idr { seen: 12 }]);
        back.clear();
        asks.tick(ms(19), &mut back);
        assert!(back.is_empty());
        asks.tick(now + FAILED_GAP, &mut back);
        assert_eq!(
            back,
            [Back::Recover {
                first: 11,
                last: 12
            }]
        );
        assert_eq!(asks.deadline(), None);
        back.clear();
        asks.decoded(false);
        asks.failed(20, false, now + FAILED_GAP * 3, &mut back);
        assert_eq!(
            back,
            [Back::Recover {
                first: 20,
                last: 20
            }]
        );
    }

    // A friend's PC sends an IDR and then thousands of small frames a second
    // that fail to decode. Each is still reported, inside a range, but the
    // messages come once per gap and not per frame.
    #[test]
    fn a_stream_that_keeps_failing_makes_one_report_per_gap() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let start = Instant::now();
        asks.decoded(true);
        for number in 1..=5000u32 {
            let now = start + Duration::from_micros(u64::from(number) * 200);
            if asks.deadline().is_some_and(|at| at <= now) {
                asks.tick(now, &mut back);
            }
            asks.failed(number, false, now, &mut back);
        }
        asks.tick(start + Duration::from_secs(2), &mut back);
        let ranges: Vec<(u32, u32)> = back
            .iter()
            .filter_map(|message| match *message {
                Back::Recover { first, last } => Some((first, last)),
                _ => None,
            })
            .collect();
        // One second of failures, a gap of 20 ms.
        assert!(ranges.len() <= 51, "{} reports", ranges.len());
        let asked = back.len() - ranges.len();
        assert!(asked <= 51, "{asked} IDR asks");
        let mut next = 1;
        for (first, last) in ranges {
            assert_eq!(first, next, "no frame left out");
            next = last + 1;
        }
        assert_eq!(next, 5001);
    }

    #[test]
    fn refusals_stop_the_run_only_when_they_keep_coming() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let now = Instant::now();
        asks.decoded(true);
        asks.failed(1, true, now, &mut back);
        asks.failed(2, false, now, &mut back);
        asks.failed(3, true, now, &mut back);
        assert!(!asks.refused_too_often());
        asks.decoded(true);
        for number in 4..6 {
            asks.failed(number, true, now, &mut back);
        }
        assert!(!asks.refused_too_often(), "a picture came in between");
        asks.failed(6, true, now, &mut back);
        assert!(asks.refused_too_often());
    }

    #[test]
    fn frames_held_for_an_idr_ask_for_one_after_the_last_drop() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let start = Instant::now();
        asks.decoded(true);
        asks.held(0, start, &mut back);
        asks.dropped(11);
        asks.held(1, start, &mut back);
        assert_eq!(back, [Back::Idr { seen: 11 }]);
        // The same count again is not a new hold, and a new one inside the
        // gap waits for the next.
        asks.held(1, start + IDR_ASK_GAP, &mut back);
        asks.held(2, start + Duration::from_millis(5), &mut back);
        assert_eq!(back.len(), 1);
        asks.dropped(14);
        asks.held(3, start + IDR_ASK_GAP, &mut back);
        assert_eq!(back[1..], [Back::Idr { seen: 14 }]);
    }

    // IDR 0 arrives, frame 1 loses its first data shard and more than its
    // parity covers, and frame 1's wait runs out before frame 2 comes. That
    // takes a late frame 2 at 120 fps, and none at 100 or less, where the
    // wait is one frame interval. Every header says what the encoder does,
    // and IDR 0's does not count for the guess, so frame 2's header decides:
    // NVENC invalidates frame 1 and frame 2 comes out. Before, IDR 0's
    // header said an IDR was needed, the reassembler held frame 2, and only
    // the viewer's IDR ask moved the picture again.
    #[test]
    fn a_lost_header_after_an_idr() {
        let interval = Duration::from_secs(1) / 120;
        let mut packetizer = Packetizer::new(PAYLOAD_INTERNET).unwrap();
        let mut reassembler = Reassembler::new(interval);
        let (mut asks, mut answers) = (Asks::default(), Answers::new(BITRATE));
        let mut encoder = Fake::new(Recovery::Invalidated);
        let mut numbers = SharerNumbers::default();
        let mut back = Vec::new();
        let mut out = Vec::new();
        let start = Instant::now();
        for number in 0..3u32 {
            let idr = number == 0;
            answers.encoded(u64::from(number), idr);
            let facts = FrameFacts {
                number,
                idr,
                survives_loss: encoder.invalidates(),
                hevc: false,
                captured: 0,
                encoded: 0,
            };
            let unit = vec![number as u8 + 1; 4000];
            let packets = packetizer.packetize(&facts, &unit, 20).unwrap();
            let lost = if number == 1 {
                usize::from(packets.parity()) + 1
            } else {
                0
            };
            assert!(lost < usize::from(packets.data()), "frame 1 keeps a shard");
            let at = start + Duration::from_millis([0, 8, 20][number as usize]);
            for packet in packets.iter().skip(lost) {
                reassembler.push(packet, at);
                while let Some(event) = reassembler.event() {
                    match event {
                        Event::Frame(frame) => {
                            out.push(frame.facts.number);
                            assert!(asks.arrived(
                                frame.facts.number,
                                frame.facts.idr,
                                at,
                                &mut back
                            ));
                            asks.decoded(frame.facts.idr);
                        }
                        Event::Dropped { last, .. } => asks.dropped(last),
                        Event::Recover { first, last } => back.push(Back::Recover { first, last }),
                    }
                }
            }
            asks.held(reassembler.numbers().skipped, at, &mut back);
        }
        assert_eq!(out, [0, 2], "frame 2 comes out");
        assert_eq!(reassembler.numbers().skipped, 0);
        assert_eq!(back, [Back::Recover { first: 1, last: 1 }]);
        for message in back {
            match message {
                Back::Recover { first, last } => {
                    answers.recover(&mut encoder, first, last, Instant::now(), &mut numbers);
                }
                Back::Idr { seen } => answers.idr_asked(seen, Instant::now(), &mut numbers),
                Back::Loss(_) => {}
            }
        }
        assert_eq!(encoder.recovered, [1]);
        assert!(!answers.take_force_idr(Instant::now()), "no IDR");
        assert_eq!((numbers.invalidated, numbers.idr_asks), (1, 0));
    }

    // Frame 500 is lost here and invalidated at once, which cuts off 500 and
    // 501, the newest then. The viewer later reports 500 and 501 together;
    // 501 was not lost here, but that invalidation already covers it, and
    // recovering it again would invalidate 502 and 503, which are good.
    #[test]
    fn frames_already_cut_off_are_left_alone() {
        let mut answers = Answers::new(BITRATE);
        let mut encoder = Fake::new(Recovery::Invalidated);
        let mut numbers = SharerNumbers::default();
        encoded(&mut answers, 0..=501);
        answers.recover(&mut encoder, 500, 500, Instant::now(), &mut numbers);
        encoded(&mut answers, 502..=503);
        answers.recover(&mut encoder, 500, 501, Instant::now(), &mut numbers);
        assert_eq!(encoder.recovered, [500]);
        assert_eq!((numbers.invalidated, numbers.covered), (1, 1));
        // 502 and 503 came after the cut, so losing them is news, and the
        // one call for 502 cuts off 503 as well.
        answers.recover(&mut encoder, 501, 503, Instant::now(), &mut numbers);
        assert_eq!(encoder.recovered, [500, 502]);
        assert_eq!((numbers.recoveries, numbers.invalidated), (3, 2));
    }

    #[test]
    fn an_idr_covers_every_report_about_the_frames_before_it() {
        let mut answers = Answers::new(BITRATE);
        let mut encoder = Fake::new(Recovery::Idr);
        let mut numbers = SharerNumbers::default();
        encoded(&mut answers, 0..=10);
        answers.recover(&mut encoder, 3, 3, Instant::now(), &mut numbers);
        // Until the IDR is encoded the encoder has said all it will.
        answers.recover(&mut encoder, 4, 4, Instant::now(), &mut numbers);
        answers.idr_asked(4, Instant::now(), &mut numbers);
        assert!(!answers.take_force_idr(Instant::now()));
        answers.encoded(11, true);
        answers.recover(&mut encoder, 9, 10, Instant::now(), &mut numbers);
        assert_eq!(encoder.recovered, [3]);
        // The IDR itself lost is news.
        answers.recover(&mut encoder, 10, 11, Instant::now(), &mut numbers);
        assert_eq!(encoder.recovered, [3, 11]);
        assert_eq!((numbers.idr_answers, numbers.covered), (2, 2));
        assert_eq!(numbers.idr_asks, 0);
    }

    #[test]
    fn an_idr_ask_is_dropped_when_a_later_idr_answers_it() {
        let mut answers = Answers::new(BITRATE);
        let mut numbers = SharerNumbers::default();
        encoded(&mut answers, 0..=5);
        answers.idr_asked(5, Instant::now(), &mut numbers);
        answers.idr_asked(5, Instant::now(), &mut numbers);
        assert_eq!(numbers.idr_asks, 1);
        assert!(answers.take_force_idr(Instant::now()));
        answers.encoded(6, true);
        answers.idr_asked(5, Instant::now(), &mut numbers);
        assert!(!answers.take_force_idr(Instant::now()));
        answers.idr_asked(6, Instant::now(), &mut numbers);
        assert!(
            answers.take_force_idr(Instant::now()),
            "IDR 6 was lost as well"
        );
        assert_eq!(numbers.idr_asks, 2);
    }

    // A friend's PC names a frame that was never sent, a new one each time.
    // NVENC would answer each with an IDR, and every watcher would get about
    // 90 KB more per request.
    #[test]
    fn reports_about_frames_not_sent_yet_are_left_unanswered() {
        let mut answers = Answers::new(BITRATE);
        let mut encoder = Fake::new(Recovery::Idr);
        let mut numbers = SharerNumbers::default();
        answers.recover(&mut encoder, 0, 0, Instant::now(), &mut numbers);
        answers.idr_asked(0, Instant::now(), &mut numbers);
        assert_eq!(numbers.unsent, 2, "nothing is sent before the first frame");
        encoded(&mut answers, 0..=100);
        for ahead in 1..=50u32 {
            answers.recover(
                &mut encoder,
                100 + ahead * 200,
                100 + ahead * 200,
                Instant::now(),
                &mut numbers,
            );
            answers.idr_asked(100 + ahead, Instant::now(), &mut numbers);
        }
        answers.idr_asked(u32::MAX, Instant::now(), &mut numbers);
        // Partly sent, and wrapping past the last frame number to old ones.
        answers.recover(&mut encoder, 99, 101, Instant::now(), &mut numbers);
        answers.recover(&mut encoder, u32::MAX - 1, 3, Instant::now(), &mut numbers);
        assert!(encoder.recovered.is_empty());
        assert!(!answers.take_force_idr(Instant::now()));
        assert_eq!(numbers.unsent, 2 + 50 * 2 + 3);
        assert_eq!((numbers.recoveries, numbers.idr_asks), (0, 0));
        // The newest frame sent is fair to report.
        answers.recover(&mut encoder, 100, 100, Instant::now(), &mut numbers);
        assert_eq!(encoder.recovered, [100]);
        assert_eq!((numbers.recoveries, numbers.idr_answers), (1, 1));
    }

    #[test]
    fn the_first_loss_goes_back_at_once() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let unlooked = || -> Option<f32> { panic!("the loss was looked at with nothing lost") };
        asks.decoded(true);
        // Parity still on its way reads as loss for a moment; without a
        // repaired or dropped frame the number is not looked at.
        asks.first_loss(unlooked, &mut back);
        asks.repaired();
        asks.first_loss(|| Some(4.0), &mut back);
        assert_eq!(back, [Back::Loss(Some(4.0))]);
        asks.first_loss(unlooked, &mut back);
        back.clear();
        // Told already: the next number waits for its second.
        asks.dropped(20);
        asks.first_loss(|| Some(9.0), &mut back);
        assert!(back.is_empty());
        asks.loss(Some(8.0), &mut back);
        asks.dropped(21);
        asks.first_loss(|| Some(9.0), &mut back);
        assert_eq!(back, [Back::Loss(Some(8.0))]);
        back.clear();
        // After a second that said zero, the next loss goes at once again.
        asks.loss(Some(0.0), &mut back);
        asks.repaired();
        asks.first_loss(|| Some(0.0), &mut back);
        asks.first_loss(unlooked, &mut back);
        asks.dropped(40);
        asks.first_loss(|| Some(2.5), &mut back);
        assert_eq!(back, [Back::Loss(Some(0.0)), Back::Loss(Some(2.5))]);
    }

    // Bo starts watching mid-frame on a spread path and gets one shard of
    // three. That frame is dropped before the first IDR, and 67 percent loss
    // at once would put the parity at its ceiling for every watcher. It
    // waits for the once-a-second number, where it is one frame among many.
    // A repaired first IDR is real loss and goes at once.
    #[test]
    fn loss_before_the_first_picture_waits() {
        let (mut asks, mut back) = (Asks::default(), Vec::new());
        let unlooked = || -> Option<f32> { panic!("the loss was looked at before any picture") };
        asks.dropped(7);
        asks.first_loss(unlooked, &mut back);
        asks.repaired();
        asks.first_loss(unlooked, &mut back);
        assert!(back.is_empty());
        // Frames held after that drop still ask for an IDR after it.
        asks.held(1, Instant::now(), &mut back);
        assert_eq!(back, [Back::Idr { seen: 7 }]);
        back.clear();
        asks.decoded(true);
        asks.repaired();
        asks.first_loss(|| Some(1.5), &mut back);
        assert_eq!(back, [Back::Loss(Some(1.5))]);
    }

    // At 20 percent loss Mara changes slide and the screen stands still.
    // Frame 10 loses more than its parity covers, and NVENC answers Ana's
    // report with an invalidation, not an IDR. Nothing new is captured, so
    // the last picture goes out again, predicted from a frame before the
    // cut, or Ana would see the old slide until the screen changed. Every
    // picture is also repeated once when the screen stands still, which
    // shows a frame lost whole as a gap; a repeat is not repeated in turn.
    #[test]
    fn a_still_screen_resends_after_an_invalidation() {
        let mut answers = Answers::new(BITRATE);
        let mut encoder = Fake::new(Recovery::Invalidated);
        let mut numbers = SharerNumbers::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        encoded(&mut answers, 0..=10);
        answers.went_out(at(0), None);
        assert_eq!(answers.again(at(0)), None);
        answers.recover(&mut encoder, 10, 10, Instant::now(), &mut numbers);
        assert_eq!(answers.again(at(1)), Some(Again::Answer));
        answers.encoded(11, false);
        answers.went_out(at(1), Some(Again::Answer));
        assert_eq!(answers.again(at(2)), None);
        assert_eq!(answers.again(at(1) + REPEAT_AFTER), Some(Again::Repeat));
        answers.encoded(12, false);
        answers.went_out(at(1) + REPEAT_AFTER, Some(Again::Repeat));
        assert_eq!(answers.again(at(10_000)), None);
        // A frame lost further back cuts off every frame up to the newest,
        // so the newest is sent again too.
        answers.recover(&mut encoder, 5, 5, Instant::now(), &mut numbers);
        assert_eq!(answers.again(at(10_000)), Some(Again::Answer));
        assert_eq!(encoder.recovered, [10, 5]);
        // A report an earlier cut covers changes nothing.
        answers.encoded(13, false);
        answers.went_out(at(10_000), Some(Again::Answer));
        answers.recover(&mut encoder, 6, 12, Instant::now(), &mut numbers);
        assert_eq!(answers.again(at(10_001)), None);
        assert_eq!(numbers.covered, 1);
    }

    // Under load the pacer lets frames go. The sharer recovers each itself,
    // and the viewers' reports of them later are counted apart, so the rate
    // does not back off for a busy PC.
    #[test]
    fn reports_of_frames_lost_here_are_counted_apart() {
        let mut answers = Answers::new(BITRATE);
        let mut encoder = Fake::new(Recovery::Invalidated);
        let mut numbers = SharerNumbers::default();
        encoded(&mut answers, 0..=20);
        answers.lost_here(&mut encoder, 15, Instant::now(), &mut numbers);
        assert_eq!((numbers.recoveries, numbers.reported_lost_here), (1, 0));
        assert_eq!(numbers.idrs_let_go, 0, "frame 15 was no IDR");
        answers.recover(&mut encoder, 14, 16, Instant::now(), &mut numbers);
        answers.recover(&mut encoder, 15, 15, Instant::now(), &mut numbers);
        answers.recover(&mut encoder, 18, 18, Instant::now(), &mut numbers);
        assert_eq!(numbers.reported_lost_here, 2);
        assert_eq!(encoder.recovered, [15, 14]);
        // Only the newest LOST_HERE_KEPT are known.
        encoded(&mut answers, 21..=100);
        for number in 30..30 + LOST_HERE_KEPT as u32 + 1 {
            answers.lost_here(&mut encoder, number, Instant::now(), &mut numbers);
        }
        answers.recover(&mut encoder, 15, 15, Instant::now(), &mut numbers);
        answers.recover(&mut encoder, 30, 31, Instant::now(), &mut numbers);
        assert_eq!(numbers.reported_lost_here, 3);
    }

    // An encoder as the sharer's loop sees it: its first frame is an IDR, and
    // so is the frame after it is told to make one or answers a loss with
    // one. With room for `kept` frames of reference memory it works as
    // encode's NVENC does (nvenc/references.rs): the frames since the last
    // IDR slide through that memory, an invalidated one keeps its place
    // until it slides out, and a loss is invalidated while a valid frame
    // older than it is still there, and answered with an IDR otherwise.
    // Without, it answers every loss with an IDR, as the Media Foundation
    // encoders do. Either way it says which before it is asked.
    struct Model {
        kept: Option<usize>,
        // Oldest first, each with whether it is still one to predict from.
        memory: VecDeque<(u64, bool)>,
        idr_next: bool,
        recovered: Vec<u64>,
    }

    impl Model {
        fn new(kept: Option<usize>) -> Model {
            Model {
                kept,
                memory: VecDeque::new(),
                idr_next: false,
                recovered: Vec::new(),
            }
        }

        // Whether frame `index` comes out an IDR.
        fn next(&mut self, index: u64, force_idr: bool) -> bool {
            let idr = force_idr || self.idr_next || self.memory.is_empty();
            self.idr_next = false;
            if idr {
                self.memory.clear();
            }
            self.memory.push_back((index, true));
            if self.memory.len() > self.kept.unwrap_or(1) {
                self.memory.pop_front();
            }
            idr
        }
    }

    impl Encoder for Model {
        fn name(&self) -> &str {
            "model"
        }

        fn kind(&self) -> Kind {
            if self.kept.is_some() {
                Kind::Nvenc
            } else {
                Kind::MfHardware
            }
        }

        fn codec(&self) -> Codec {
            Codec::H264
        }

        fn encode(&mut self, _: &Frame<'_>) -> Result<AccessUnit, EncodeError> {
            panic!("the model's frames come from Run::frame")
        }

        fn set_bitrate(&mut self, _: u32) -> Result<(), EncodeError> {
            Ok(())
        }

        fn recover(&mut self, lost_frame_index: u64) -> Recovery {
            self.recovered.push(lost_frame_index);
            if self.needs_idr(lost_frame_index) {
                self.idr_next = true;
                return Recovery::Idr;
            }
            for (index, valid) in &mut self.memory {
                *valid &= *index < lost_frame_index;
            }
            Recovery::Invalidated
        }

        fn needs_idr(&self, lost_frame_index: u64) -> bool {
            let older = self
                .memory
                .iter()
                .any(|&(index, valid)| valid && index < lost_frame_index);
            self.kept.is_none() || self.idr_next || !older
        }

        fn invalidates(&self) -> bool {
            self.kept.is_some()
        }
    }

    // The sharer's loop in small, as Sharer::next runs it: a frame every
    // interval, what came back answered before it, and an IDR of `idr_bytes`
    // whenever the answers or the encoder make one.
    struct Run {
        answers: Answers,
        encoder: Model,
        numbers: SharerNumbers,
        interval: Duration,
        idr_bytes: usize,
        next: u64,
        start: Instant,
        // When the next frame goes. What comes back arrives just before.
        now: Instant,
        // When each IDR went out.
        idrs: Vec<Instant>,
    }

    impl Run {
        fn new(kept: Option<usize>, bitrate: u32, fps: u32, idr_bytes: usize) -> Run {
            let start = Instant::now();
            Run {
                answers: Answers::new(bitrate),
                encoder: Model::new(kept),
                numbers: SharerNumbers::default(),
                interval: Duration::from_secs(1) / fps,
                idr_bytes,
                next: 0,
                start,
                now: start,
                idrs: Vec::new(),
            }
        }

        // True when the frame is an IDR.
        fn frame(&mut self) -> bool {
            let index = self.next;
            let force_idr = self.answers.take_force_idr(self.now);
            let idr = self.encoder.next(index, force_idr);
            self.answers.encoded(index, idr);
            self.answers.went_out(self.now, None);
            if idr {
                self.answers.idr_went_out(index, self.now, self.idr_bytes);
                self.idrs.push(self.now);
            }
            self.next += 1;
            self.now += self.interval;
            idr
        }

        fn newest(&self) -> u32 {
            (self.next - 1) as u32
        }

        fn recover(&mut self, first: u32, last: u32) {
            self.answers
                .recover(&mut self.encoder, first, last, self.now, &mut self.numbers);
        }

        fn idr_asked(&mut self, seen: u32) {
            self.answers.idr_asked(seen, self.now, &mut self.numbers);
        }

        fn lost_here(&mut self, number: u32) {
            self.answers
                .lost_here(&mut self.encoder, number, self.now, &mut self.numbers);
        }
    }

    // NVENC at 120 fps keeps 12 frames. Inside the floor after the stream's
    // first IDR, lost frames whose frame before it still keeps are
    // invalidated at once, as with no floor at all. Only the IDR itself,
    // lost, waits.
    #[test]
    fn invalidation_never_waits_for_the_floor() {
        let mut run = Run::new(Some(12), 15_000_000, 120, 90_000);
        for _ in 0..10 {
            run.frame();
        }
        run.recover(5, 5);
        assert_eq!(run.encoder.recovered, [5]);
        assert!(!run.frame());
        run.recover(10, 10);
        assert!(!run.frame());
        // 9 and 10 were cut off already.
        run.recover(9, 11);
        assert_eq!(run.encoder.recovered, [5, 10, 11]);
        assert_eq!((run.numbers.invalidated, run.numbers.floor_waits), (3, 0));
        assert!(run.now - run.start < Duration::from_millis(192));
        run.recover(0, 0);
        assert_eq!(run.encoder.recovered, [5, 10, 11]);
        assert_eq!(run.numbers.floor_waits, 1);
        assert_eq!(run.idrs.len(), 1);
    }

    // A friend's PC reports every frame lost the moment it arrives. An
    // encoder that answers every loss with an IDR then sends one each time
    // the last has used a quarter of the setting, at the first frame after:
    // 192 ms apart for 90 KB IDRs at 15 Mbit/s and 120 fps, 1.8 s for the
    // software encoder's 450 KB ones at 8 Mbit/s and 60 fps. The encoder is
    // never asked, since asking commits it to an IDR on the next frame.
    #[test]
    fn idr_only_encoders_keep_to_the_floor() {
        // Bits a second, frames a second, IDR bytes, the floor in ms, IDRs
        // in 10 s.
        let cases = [
            (15_000_000, 120, 90_000, 192, 50),
            (8_000_000, 60, 450_000, 1800, 6),
        ];
        for (bitrate, fps, idr_bytes, floor_ms, idrs) in cases {
            let floor = Duration::from_millis(floor_ms);
            let mut run = Run::new(None, bitrate, fps, idr_bytes);
            for _ in 0..fps * 10 {
                run.frame();
                let newest = run.newest();
                run.recover(newest, newest);
            }
            for pair in run.idrs.windows(2) {
                let gap = pair[1] - pair[0];
                assert!(
                    gap >= floor && gap < floor + run.interval,
                    "{gap:?} between IDRs at {bitrate} bits a second"
                );
            }
            assert_eq!(run.idrs.len(), idrs);
            assert_eq!(run.numbers.floor_waits as usize, idrs);
            assert!(run.encoder.recovered.is_empty());
        }
    }

    // The stream's first IDR is lost: NVENC can only answer that with an
    // IDR, as the Media Foundation encoders answer every loss. The report
    // comes 50 ms in; neither encoder is asked, and the first frame after
    // the floor, 192 ms after the IDR went, is the IDR that answers it. A
    // still screen sends its last picture again for it. An IDR ask inside
    // the next floor waits the same way.
    #[test]
    fn a_request_inside_the_floor_waits() {
        let floor = Duration::from_millis(192);
        for kept in [Some(12), None] {
            let mut run = Run::new(kept, 15_000_000, 120, 90_000);
            for _ in 0..6 {
                run.frame();
            }
            run.recover(0, 0);
            assert!(run.encoder.recovered.is_empty());
            assert_eq!(run.numbers.floor_waits, 1);
            let start = run.start;
            let just_before = start + floor - Duration::from_millis(1);
            assert_ne!(run.answers.again(just_before), Some(Again::Answer));
            assert_eq!(run.answers.again(start + floor), Some(Again::Answer));
            while run.now < start + floor {
                assert!(!run.frame(), "{kept:?}");
            }
            assert!(run.frame(), "the first frame after the floor");
            assert!(run.idrs[1] - start < floor + run.interval);
            assert!(!run.frame(), "answered, nothing waits");

            run.idr_asked(run.newest());
            assert_eq!((run.numbers.idr_asks, run.numbers.floor_waits), (1, 2));
            let second = run.idrs[1];
            while run.now < second + floor {
                assert!(!run.frame(), "{kept:?}");
            }
            assert!(run.frame());
            assert!(run.encoder.recovered.is_empty());
        }
    }

    // Someone starts watching 25 ms after the stream's first IDR, and a new
    // size brings a new encoder 25 ms later. Neither IDR waits, and the
    // floor counts from the newest of them.
    #[test]
    fn idrs_for_a_watcher_or_encoder_never_wait() {
        let floor = Duration::from_millis(192);
        let mut run = Run::new(None, 15_000_000, 120, 90_000);
        for _ in 0..3 {
            run.frame();
        }
        run.answers.force_idr();
        assert!(run.frame(), "someone who starts watching never waits");
        run.frame();
        run.frame();
        run.encoder = Model::new(None);
        assert!(run.frame(), "a new encoder starts with an IDR");
        assert_eq!(run.idrs.len(), 3);
        let newest_idr = run.idrs[2];
        run.recover(run.newest(), run.newest());
        assert_eq!(run.numbers.floor_waits, 1);
        while run.now < newest_idr + floor {
            assert!(!run.frame());
        }
        assert!(run.frame());
        assert!(run.idrs[3] - newest_idr >= floor);
    }

    // One request from a lost IDR on. On NVENC the IDR waits, and the frame
    // after it, which still has the IDR to predict from, is invalidated at
    // once. The numbers show both.
    #[test]
    fn a_request_answered_both_ways_counts_both() {
        let mut run = Run::new(Some(12), 15_000_000, 120, 90_000);
        for _ in 0..4 {
            run.frame();
        }
        run.recover(0, 1);
        assert_eq!(run.encoder.recovered, [1]);
        let numbers = &run.numbers;
        assert_eq!(
            (
                numbers.idr_answers,
                numbers.floor_waits,
                numbers.invalidated
            ),
            (1, 1, 1)
        );
        assert_eq!(numbers.covered, 0);
    }

    // Bo starts watching and the pacer lets the IDR for him go unsent, as it
    // can on a PC busy with a game; the sharer finds out when it puts the
    // next frame. That IDR never left, so the floor counts from the one
    // before it, and the IDR that stands in for it goes at once, as it did:
    // at frame 30, after the stream's first IDR has used its quarter, and at
    // frame 10, before.
    #[test]
    fn an_idr_the_pacer_let_go() {
        for kept in [None, Some(12)] {
            for watching_at in [30, 10] {
                let mut run = Run::new(kept, 15_000_000, 120, 90_000);
                while run.next < watching_at {
                    run.frame();
                }
                run.answers.force_idr();
                assert!(run.frame());
                assert!(!run.frame());
                run.lost_here(watching_at as u32);
                assert!(run.frame(), "{kept:?}, watching from frame {watching_at}");
                assert_eq!(run.numbers.floor_waits, 0);
                assert_eq!(run.numbers.idrs_let_go, 1);
                assert!(run.encoder.recovered.is_empty());
                // The floor counts from the one that stood in.
                let stood_in = run.newest();
                run.recover(stood_in, stood_in);
                assert_eq!(run.numbers.floor_waits, 1);
            }
        }
    }

    // A friend's PC floods the sharer for 10 s: every frame reported lost the
    // moment it arrives and an IDR asked for after each. The IDRs past the
    // stream's first come to at most a quarter of the setting over those
    // 10 s.
    fn flood(kept: Option<usize>) -> Run {
        let (bitrate, fps, idr_bytes) = (15_000_000, 120, 90_000);
        let mut run = Run::new(kept, bitrate, fps, idr_bytes);
        for _ in 0..fps * 10 {
            run.frame();
            let newest = run.newest();
            run.recover(newest, newest);
            run.idr_asked(newest);
        }
        let bytes = (run.idrs.len() - 1) * idr_bytes;
        let quarter = IDR_FLOOR_SHARE * f64::from(bitrate) / 8.0 * 10.0;
        println!(
            "{} IDRs, {bytes} bytes past the first against a quarter of {quarter}",
            run.idrs.len()
        );
        assert!(bytes as f64 <= quarter);
        run
    }

    #[test]
    fn a_flood_stays_under_a_quarter() {
        let run = flood(None);
        assert_eq!(run.idrs.len(), 50);
    }

    // On NVENC the reports are invalidated one by one until the frames they
    // cut off push the stream's IDR out of the reference memory. The report
    // after that needs an IDR, which the encoder says before it is asked, so
    // it waits for the floor as on the Media Foundation encoders; the frames
    // meanwhile predict from the one it named, and their reports are
    // invalidated at once again. Every IDR went at the floor's end, and 22
    // reports in each 24 frames were invalidated without waiting.
    #[test]
    fn a_flood_on_nvenc_stays_under_a_quarter() {
        let run = flood(Some(12));
        assert_eq!(run.idrs.len(), 50);
        let numbers = &run.numbers;
        assert_eq!((numbers.idr_answers, numbers.floor_waits), (50, 50));
        assert_eq!(numbers.invalidated, 50 * 22);
    }
}
