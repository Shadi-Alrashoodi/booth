// The viewer's side: the reassembler, the decoder on the viewer's device,
// and a present the moment a frame is decoded. It blocks on the inbox's
// event and the high-resolution timer together, so a reassembler deadline is
// met within a fraction of a millisecond and nothing waits on the 15.6 ms
// default timer.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use channels::video::{Event, Frame, Reassembler};
use decode::{Codec, DecodeError, Decoder, GpuTime};
use encode::Codec as ShareCodec;
use net::Timer;
use stats::{Level, Thresholds, TraceSample};
use viewer::{
    ControlOut, Cursor, ErrorKind, LinkState, MouseMode, Options, PathWord, Show, Strip, Video,
    Viewer,
};

use crate::cursor::pointer;
use crate::inbox::{Control, Inbox};
use crate::numbers::{ViewerNumbers, end_to_end_level, ms, spread, stage_level};
use crate::recovery::{Asks, Back};
use crate::sharer::SWITCH_GAP;
use crate::{Clock, Line};

// How often the strip's numbers change: each is the median of the frames
// since the last change. Once a frame would be too fast to read.
const STRIP_EVERY: Duration = Duration::from_millis(250);
const SECOND: Duration = Duration::from_secs(1);

// The first decode failure is said, since without a log it would be lost;
// the next few go to the log word for word, and after that only the count.
const FAILURES_LOGGED: u64 = 5;

// Frames presented and waiting for the GPU's time for their picture, which
// comes a frame or two later. Past this many the oldest counts with its
// present alone, as when the decoder has no GPU timing at all.
const MOST_WAITING: usize = 16;

// A frame whose capture and encode times are further apart than this says
// nothing about the encoder. And a quarter second of frames at the most.
const ENCODED_WITHIN_US: u64 = 1_000_000;
const MOST_FRAMED: usize = 256;

// A Booth sharer changes codec at most once a SWITCH_GAP by its own rule,
// a fallback or a new size can add one, and a share's first HEVC IDR
// replaces the H.264 decoder a viewer opens with. Each new decoder is a
// video decoder and its surfaces on the viewer's GPU, next to a game, and
// the viewer keeps the old surfaces for a few hundred presents, so a stream
// that asks for more within a SWITCH_GAP waits for the gap instead.
const NEW_DECODERS_PER_GAP: usize = 3;

// Held back this often within HOLDS_WITHIN, the stream ends the watch with
// a sentence rather than a picture that keeps stopping. A sharer that
// changes codec only as Booth's rules do is not held back at all.
const HOLDS_TO_STOP: usize = 3;
const HOLDS_WITHIN: Duration = Duration::from_secs(30);

pub struct Watch {
    // The window's title bar: whose screen it is.
    pub title: String,
    // The video's size, which the window opens at, scaled down to fit.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub vsync: bool,
    pub show: Show,
    // Whether this viewer may take HEVC at all, when its GPU decodes it.
    // False stands for a GPU that does not, in tests.
    pub hevc: bool,
    // The sharer's clock as this PC reads it, for capture to display.
    // None until the caller knows it (Inbox::set_clock): no frame has a
    // capture to display before then, rather than a wrong one.
    pub clock: Option<Clock>,
    pub link: LinkNumbers,
    // Every frame's decode and capture to display times kept in the numbers
    // run() returns, for a summary at the end as the loopback prints. Off in
    // a room: a share runs for hours, and a friend's PC can send frames as
    // fast as the host passes them on. Each Second has its own either way.
    pub keep_times: bool,
}

// The strip's link numbers as the caller has them: in the room the viewer's
// own link to the host, as room::view::Strip has them, levels included.
#[derive(Debug, Clone)]
pub struct LinkNumbers {
    pub state: LinkState,
    pub rtt_ms: Option<f32>,
    pub rtt_level: Level,
    pub jitter_ms: Option<f32>,
    pub jitter_level: Level,
    // None shows the video's own shard loss instead, which is all the
    // loopback has.
    pub loss_pct: Option<f32>,
    pub loss_level: Level,
    pub path: Option<PathWord>,
    // Oldest first, at most 120: one per ping.
    pub trace: Vec<TraceSample>,
    // From the sharer to this viewer, one way, when the path is not LAN:
    // capture to display's thresholds are that much later. None on LAN and
    // in the loopback.
    pub one_way_ms: Option<f32>,
}

impl Default for LinkNumbers {
    fn default() -> LinkNumbers {
        LinkNumbers {
            state: LinkState::default(),
            rtt_ms: None,
            rtt_level: Level::Good,
            jitter_ms: None,
            jitter_level: Level::Good,
            loss_pct: None,
            loss_level: Level::Good,
            path: None,
            trace: Vec::new(),
            one_way_ms: None,
        }
    }
}

// What the viewer's loop hands its caller.
pub enum Report<'a> {
    // For the sharer: in the room, on the control channel.
    Back(Back),
    Line(Line),
    // Once a second, after the loss number has gone back.
    Second(Second<'a>),
    // This viewer stopped taking HEVC: the GPU's decoder refused a stream
    // of it, or would not open for it. The sharer should hear it, in the
    // room through the host, so the share goes on in H.264.
    NoHevc,
    // A click on the strip, which brings the panel up with the stats panel
    // open.
    StripClicked,
}

pub struct Second<'a> {
    // Since the loop started.
    pub elapsed: Duration,
    // Shard loss over the last 2 s, the number sent back for the parity.
    pub loss: Option<f32>,
    // This second's samples.
    pub window: Window,
    // Everything so far.
    pub numbers: &'a ViewerNumbers,
}

#[derive(Debug, Clone, Default)]
pub struct Window {
    // The sharer's, as the inbox had them.
    pub encode_ms: Vec<f32>,
    // On the GPU (decode::GpuTime); each trails its frame by a frame or two.
    pub decode_ms: Vec<f32>,
    // FFmpeg's call alone, on the CPU.
    pub decode_call_ms: Vec<f32>,
    // Each comes once the GPU's time for its picture is in.
    pub end_to_end_ms: Vec<f32>,
    pub presented: u64,
}

#[derive(Debug, Clone, Copy)]
enum Stage {
    Decode,
    DecodeCall,
    EndToEnd,
}

impl Window {
    fn times(&mut self, stage: Stage) -> &mut Vec<f32> {
        match stage {
            Stage::Decode => &mut self.decode_ms,
            Stage::DecodeCall => &mut self.decode_call_ms,
            Stage::EndToEnd => &mut self.end_to_end_ms,
        }
    }
}

// Each frame's times: since the strip last changed, since the last Second,
// and since the start when the caller keeps them.
#[derive(Default)]
struct Times {
    quarter: Window,
    second: Window,
    run: Option<Window>,
}

impl Times {
    fn new(keep: bool) -> Times {
        Times {
            run: keep.then(Window::default),
            ..Times::default()
        }
    }

    fn add(&mut self, stage: Stage, ms: f32) {
        self.quarter.times(stage).push(ms);
        self.second.times(stage).push(ms);
        if let Some(run) = &mut self.run {
            run.times(stage).push(ms);
        }
    }

    fn presented(&mut self) {
        self.quarter.presented += 1;
        self.second.presented += 1;
    }

    fn into_numbers(self, numbers: &mut ViewerNumbers) {
        if let Some(run) = self.run {
            numbers.decode_ms = run.decode_ms;
            numbers.decode_call_ms = run.decode_call_ms;
            numbers.end_to_end_ms = run.end_to_end_ms;
        }
    }
}

pub struct Screen {
    // Before the viewer: its pictures are on the viewer's device. For the
    // codec of the frames coming now: a new one comes with the IDR that
    // starts another codec.
    decoder: Decoder,
    viewer: Viewer,
    // The GPU's driver says it decodes HEVC at the largest picture a share
    // sends, and no HEVC stream was refused since.
    takes_hevc: bool,
    // Frames in a codec the decoder is not for, said once in the log.
    codec_said: bool,
    // The decoder in use has given a picture: one that replaces it is a
    // change of codec, not the share's first IDR.
    decoder_used: bool,
    new_decoders: NewDecoders,
    inbox: Arc<Inbox>,
    timer: Timer,
    strip: Strip,
    // The caller gave a loss number, so the video's does not show.
    link_loss: bool,
    // LinkNumbers::one_way_ms, 0 when there is none.
    one_way_ms: f32,
    pointer: Option<Cursor>,
    interval: Duration,
    clock: Option<Clock>,
    asks: Asks,
    // What `asks` said to send back, on its way to the caller.
    back: Vec<Back>,
    gpu_times: Vec<GpuTime>,
    waiting: Waiting,
    // Capture to display of the frames `waiting` let go, on their way to
    // the numbers.
    settled: Vec<f32>,
    numbers: ViewerNumbers,
    shown_this_wake: bool,
    // Each frame's own capture to encoded, on the sharer's clock, for the
    // strip's "enc" when the caller hands no encode times over: the room
    // has none but what the frames carry. It includes capture's colour
    // conversion, a fraction of a millisecond.
    framed_encode_ms: Vec<f32>,
    times: Times,
    // Where the viewer's input goes while this PC controls the share, and
    // whether it does, as the inbox said last, and how the viewer was told
    // the mouse goes.
    out: Option<Arc<dyn ControlOut>>,
    control: Option<Control>,
    mode: Option<MouseMode>,
}

// A frame presented, before its GPU time came in.
struct Shown {
    unit: u64,
    captured: Option<Instant>,
    returned: Instant,
}

impl Shown {
    // Capture to display: to the present returning, or to the picture being
    // done on the GPU when that came later. With the decoder's GPU timing
    // the present returns before the video engine is done, and the GPU
    // waits for the picture before it draws (decode's timing.rs). The
    // picture's end, began + took, is early by as long as a GPU busy with a
    // game kept the first timestamp waiting, so under load this reads low.
    // None when the sharer's clock is not known yet, or reads a moment this
    // PC's clock cannot reach back to.
    fn end_to_end(&self, done: Option<Instant>) -> Option<f32> {
        let captured = self.captured?;
        let end = done.map_or(self.returned, |done| done.max(self.returned));
        Some(ms(end.saturating_duration_since(captured)))
    }
}

// Frames presented whose GPU time has not come in, oldest first. The times
// come oldest first too, since the GPU finishes the pictures in order.
#[derive(Default)]
struct Waiting(VecDeque<Shown>);

impl Waiting {
    // Past MOST_WAITING the oldest counts with its present alone.
    fn presented(&mut self, shown: Shown, settled: &mut Vec<f32>) {
        self.0.push_back(shown);
        if self.0.len() > MOST_WAITING
            && let Some(oldest) = self.0.pop_front()
        {
            settled.extend(oldest.end_to_end(None));
        }
    }

    // The GPU's time for `unit` came in, its picture done at `done`. A
    // frame before it whose time never came counts with its present alone.
    fn gpu_time(&mut self, unit: u64, done: Instant, settled: &mut Vec<f32>) {
        while self.0.front().is_some_and(|shown| shown.unit <= unit)
            && let Some(shown) = self.0.pop_front()
        {
            settled.extend(shown.end_to_end((shown.unit == unit).then_some(done)));
        }
    }

    fn give_up(&mut self, settled: &mut Vec<f32>) {
        settled.extend(self.0.drain(..).filter_map(|shown| shown.end_to_end(None)));
    }
}

// When a new codec's IDR may bring a new decoder (NEW_DECODERS_PER_GAP),
// kept apart from the decoder so the rule can be tried with plain times.
#[derive(Default)]
struct NewDecoders {
    // When the last few were made, oldest first.
    made: VecDeque<Instant>,
    // When each hold within HOLDS_WITHIN began.
    holds: VecDeque<Instant>,
    // The hold under way: when it ends, and the newest frame of the new
    // codec it held back, for the IDR asked for then.
    hold: Option<(Instant, u32)>,
}

impl NewDecoders {
    // Until when a new codec's frames wait, if they do now.
    fn until(&self, now: Instant) -> Option<Instant> {
        let oldest = *self.made.front()?;
        (self.made.len() >= NEW_DECODERS_PER_GAP)
            .then_some(oldest + SWITCH_GAP)
            .filter(|until| now < *until)
    }

    fn made(&mut self, now: Instant) {
        if self.made.len() == NEW_DECODERS_PER_GAP {
            self.made.pop_front();
        }
        self.made.push_back(now);
        self.hold = None;
    }

    // Frame `number` of a new codec waits until `until`. When that starts a
    // hold, how many began within HOLDS_WITHIN, this one included.
    fn held(&mut self, number: u32, until: Instant, now: Instant) -> Option<usize> {
        let starts = self.hold.is_none_or(|(ends, _)| ends != until);
        self.hold = Some((until, number));
        if !starts {
            return None;
        }
        self.holds
            .retain(|at| now.saturating_duration_since(*at) < HOLDS_WITHIN);
        self.holds.push_back(now);
        Some(self.holds.len())
    }

    // The frame to ask an IDR after, once the hold under way is over.
    fn over(&mut self, now: Instant) -> Option<u32> {
        let (until, seen) = self.hold?;
        if now < until {
            return None;
        }
        self.hold = None;
        Some(seen)
    }

    fn deadline(&self) -> Option<Instant> {
        self.hold.map(|(until, _)| until)
    }
}

impl Screen {
    // Opens the window and the decoder on the window's device. Nothing is
    // shown until run().
    pub fn open(
        watch: &Watch,
        inbox: Arc<Inbox>,
        say: &mut dyn FnMut(Line),
    ) -> Result<Screen, String> {
        Screen::open_to_control(watch, inbox, None, say)
    }

    // As open, for a viewer that can control the share it shows: while the
    // inbox says this PC controls it, the viewer hands its input to `out`.
    pub fn open_to_control(
        watch: &Watch,
        inbox: Arc<Inbox>,
        out: Option<Arc<dyn ControlOut>>,
        say: &mut dyn FnMut(Line),
    ) -> Result<Screen, String> {
        let wake = Arc::clone(&inbox);
        let viewer = Viewer::open(&Options {
            title: watch.title.clone(),
            video_width: watch.width,
            video_height: watch.height,
            vsync: watch.vsync,
            show: watch.show,
            wake: Some(Arc::new(move || wake.wake())),
            control: out.clone(),
        })
        .map_err(|err| err.to_string())?;
        let decoder = Decoder::new(viewer.device(), Codec::H264).map_err(|err| err.to_string())?;
        let largest = capture::Options::default();
        let hevc = if watch.hevc {
            decode::probe(
                viewer.device(),
                Codec::Hevc,
                largest.max_width,
                largest.max_height,
            )
            .map_err(|err| err.to_string())
        } else {
            Err(String::from("it was told not to"))
        };
        say(Line::Say(format!(
            "viewer on {}, tearing {}, {}",
            viewer.adapter(),
            if viewer.tearing() {
                "allowed"
            } else {
                "not allowed"
            },
            match &hevc {
                Ok(()) => String::from("takes HEVC"),
                Err(why) => format!("does not take HEVC: {why}"),
            }
        )));
        let timer = Timer::new().map_err(|err| err.to_string())?;
        if let Some(note) = timer.note() {
            say(Line::Say(note.to_string()));
        }
        let mut screen = Screen {
            decoder,
            viewer,
            takes_hevc: hevc.is_ok(),
            codec_said: false,
            decoder_used: false,
            new_decoders: NewDecoders::default(),
            inbox,
            timer,
            strip: Strip::default(),
            link_loss: false,
            one_way_ms: 0.0,
            pointer: None,
            interval: Duration::from_secs(1) / watch.fps.max(1),
            clock: watch.clock,
            asks: Asks::default(),
            back: Vec::new(),
            gpu_times: Vec::new(),
            waiting: Waiting::default(),
            settled: Vec::new(),
            numbers: ViewerNumbers::default(),
            shown_this_wake: false,
            framed_encode_ms: Vec::new(),
            times: Times::new(watch.keep_times),
            out,
            control: None,
            mode: None,
        };
        screen.set_link(watch.link.clone());
        Ok(screen)
    }

    // Whether this viewer takes HEVC: what the sharer, in the room every
    // watcher, has to decode for the share to go in it.
    pub fn takes_hevc(&self) -> bool {
        self.takes_hevc
    }

    // Until the inbox is stopped or the window closed. An error is one the
    // viewer cannot go on after: the device lost, the window broken, or the
    // GPU's decoder refusing the stream every time.
    pub fn run(mut self, report: &mut dyn FnMut(Report<'_>)) -> Result<ViewerNumbers, String> {
        let mut reassembler = Reassembler::new(self.interval);
        let began = Instant::now();
        let mut next_strip = began + STRIP_EVERY;
        let mut next_second = began + SECOND;
        let mut batch = VecDeque::new();
        loop {
            if self.inbox.stopped() || self.inbox.closed() || self.viewer.closed() {
                break;
            }
            let wake_at = [
                reassembler.next_deadline(),
                self.asks.deadline(),
                self.new_decoders.deadline(),
            ]
            .into_iter()
            .flatten()
            .fold(next_second.min(next_strip), Instant::min);
            self.timer.set_at(wake_at).map_err(|err| err.to_string())?;
            net::pace::wait(self.inbox.signal(), Some(&self.timer))
                .map_err(|err| err.to_string())?;

            self.shown_this_wake = false;
            let taken = self.inbox.take(&mut batch);
            if let Some(fps) = taken.fps.filter(|&fps| fps > 0) {
                self.interval = Duration::from_secs(1) / fps;
                reassembler.set_interval(self.interval);
            }
            if let Some(clock) = taken.clock {
                self.clock = Some(clock);
            }
            if let Some(link) = taken.link {
                self.set_link(link);
            }
            // The pointer first, so a frame that came with it shows it.
            let moved = taken.cursor.is_some();
            if let Some(update) = taken.cursor {
                self.pointer = Some(pointer(update, self.pointer.take()));
            }
            let controlled = taken.control.is_some();
            if let Some(control) = taken.control {
                self.set_control(control);
            }
            self.follow_mouse_mode();
            for packet in &batch {
                reassembler.push(packet, Instant::now());
                while let Some(event) = reassembler.event() {
                    self.event(event, report)?;
                }
            }
            reassembler.expire(Instant::now());
            while let Some(event) = reassembler.event() {
                self.event(event, report)?;
            }
            self.after_events(&reassembler, report);
            if ((moved || controlled) && !self.shown_this_wake) || self.viewer.needs_redraw() {
                self.redraw()?;
            }
            if self.viewer.strip_clicked() {
                report(Report::StripClicked);
            }

            let now = Instant::now();
            if now >= next_strip {
                next_strip = now + STRIP_EVERY;
                let loss = reassembler.loss(now).percent();
                self.update_strip(loss);
            }
            if now >= next_second {
                next_second += SECOND;
                let loss = reassembler.loss(now).percent();
                self.asks.loss(loss, &mut self.back);
                self.send_back(report);
                self.numbers.reassembly = reassembler.numbers();
                let window = std::mem::take(&mut self.times.second);
                report(Report::Second(Second {
                    elapsed: now.saturating_duration_since(began),
                    loss,
                    window,
                    numbers: &self.numbers,
                }));
            }
        }
        self.numbers.reassembly = reassembler.numbers();
        self.numbers.ran = began.elapsed();
        self.take_gpu_times();
        self.waiting.give_up(&mut self.settled);
        self.settle();
        self.times.into_numbers(&mut self.numbers);
        Ok(self.numbers)
    }

    fn event(
        &mut self,
        event: Event<'_>,
        report: &mut dyn FnMut(Report<'_>),
    ) -> Result<(), String> {
        match event {
            Event::Frame(frame) => {
                let repaired = frame.repaired;
                let shown = self.frame(frame, report);
                if repaired {
                    self.asks.repaired();
                }
                shown
            }
            Event::Dropped { first, last, .. } => {
                if !self.asks.had_idr() {
                    let frames = last.wrapping_sub(first).saturating_add(1);
                    self.numbers.lost_before_first_idr += u64::from(frames);
                }
                self.asks.dropped(last);
                Ok(())
            }
            // Before the first picture the viewer waits for an IDR whatever
            // happens, and a frame it never had damages nothing it shows:
            // the frame whose first packets went out before this viewer was
            // added, most often. Recovering it would cost every other viewer
            // an invalidated reference.
            Event::Recover { first, last } => {
                if self.asks.had_idr() {
                    report(Report::Back(Back::Recover { first, last }));
                }
                Ok(())
            }
        }
    }

    // After the reassembler's events: whether it held frames back for an IDR
    // this time, failed decodes whose gap is over, the first loss, and a new
    // codec's IDR once the frames of it held back have waited long enough.
    // A still screen sends no frame to ask with then.
    fn after_events(&mut self, reassembler: &Reassembler, report: &mut dyn FnMut(Report<'_>)) {
        let now = Instant::now();
        if let Some(seen) = self.new_decoders.over(now) {
            self.asks.other_codec(seen, now, &mut self.back);
        }
        self.asks
            .held(reassembler.numbers().skipped, now, &mut self.back);
        self.asks.tick(now, &mut self.back);
        self.asks
            .first_loss(|| reassembler.loss(now).percent(), &mut self.back);
        self.send_back(report);
    }

    fn send_back(&mut self, report: &mut dyn FnMut(Report<'_>)) {
        for message in self.back.drain(..) {
            report(Report::Back(message));
        }
    }

    fn frame(
        &mut self,
        frame: Frame<'_>,
        report: &mut dyn FnMut(Report<'_>),
    ) -> Result<(), String> {
        let number = frame.facts.number;
        if let Some(took) = frame.facts.encoded.checked_sub(frame.facts.captured)
            && took < ENCODED_WITHIN_US
            && self.framed_encode_ms.len() < MOST_FRAMED
        {
            self.framed_encode_ms.push(took as f32 / 1000.0);
        }
        let idr = frame.facts.idr;
        // Before anything is asked of the sharer: an IDR asked for would be
        // HEVC too. The host has heard that this viewer takes none, and the
        // share's next IDR is in H.264. Before the first picture it counts
        // as any frame before the first IDR does: one sent before the host
        // had this viewer's word reaches it now and then.
        if frame.facts.hevc && !self.takes_hevc {
            if self.asks.had_idr() {
                self.numbers.decode_failed += 1;
            } else {
                self.numbers.before_first_idr += 1;
            }
            self.said_once(format!(
                "frame {number} is HEVC, which this viewer does not take: it waits for the share's H.264"
            ), report);
            return Ok(());
        }
        if !self
            .asks
            .arrived(number, idr, Instant::now(), &mut self.back)
        {
            self.numbers.before_first_idr += 1;
            self.send_back(report);
            return Ok(());
        }
        let codec = if frame.facts.hevc {
            Codec::Hevc
        } else {
            Codec::H264
        };
        if codec != self.decoder.codec() && !self.other_codec(codec, number, idr, report)? {
            return Ok(());
        }
        let setups = self.decoder.setups();
        let result = self.decoder.decode(frame.access_unit);
        // A new size or profile sets FFmpeg's decoder and surfaces up again,
        // which costs what a new decoder does and counts as one.
        if setups > 0 && self.decoder.setups() > setups {
            self.set_up_again(number)?;
        }
        let decoded = match result {
            Ok(Some(decoded)) => decoded,
            Ok(None) => {
                return self.lost(
                    number,
                    "the decoder gave no picture and no reason",
                    false,
                    report,
                );
            }
            Err(err @ DecodeError::DeviceLost { .. }) => return Err(err.to_string()),
            // HEVC refused, by the GPU or by the guard in front of FFmpeg,
            // ends nothing: the share goes on in H.264 once the sharer hears
            // it.
            Err(err) if codec == Codec::Hevc && refuses_the_stream(&err) => {
                self.numbers.decode_failed += 1;
                self.no_hevc(&err.to_string(), report);
                return Ok(());
            }
            Err(err) => {
                let refused = refuses_the_stream(&err);
                return self.lost(number, &err.to_string(), refused, report);
            }
        };
        self.asks.decoded(idr);
        self.decoder_used = true;
        self.numbers.decoded += 1;
        self.numbers.codec = Some(share_codec(codec));
        self.times.add(Stage::DecodeCall, ms(decoded.submit_time));
        let video = Video {
            texture: &decoded.texture,
            index: decoded.index,
            width: decoded.width,
            height: decoded.height,
        };
        let presented = self.present(Some(video))?;
        if let Some(returned) = presented {
            self.numbers.presented += 1;
            self.times.presented();
            self.numbers.first_present.get_or_insert(returned);
            self.numbers.last_present = Some(returned);
            let shown = Shown {
                unit: decoded.unit,
                captured: self
                    .clock
                    .and_then(|clock| clock.instant(frame.facts.captured)),
                returned,
            };
            self.waiting.presented(shown, &mut self.settled);
        }
        self.take_gpu_times();
        Ok(())
    }

    // The GPU's times for pictures decoded before, as they come in, and the
    // capture to display of the frames they finish.
    fn take_gpu_times(&mut self) {
        let mut times = std::mem::take(&mut self.gpu_times);
        self.decoder.gpu_times(&mut times);
        for time in times.drain(..) {
            self.times.add(Stage::Decode, ms(time.took));
            self.waiting
                .gpu_time(time.unit, time.began + time.took, &mut self.settled);
        }
        self.gpu_times = times;
        self.settle();
    }

    fn settle(&mut self) {
        for end_to_end in self.settled.drain(..) {
            self.times.add(Stage::EndToEnd, end_to_end);
        }
    }

    // A frame in a codec the decoder is not for. The IDR that starts a
    // codec brings a new decoder; any other frame of it waits for that IDR,
    // which is asked for, and a frame of the old codec after the new one's
    // IDR, which no sharer sends, goes too. Past NEW_DECODERS_PER_GAP the
    // new codec's frames wait for the gap without a word to the sharer, and
    // its IDR is asked for once the gap is over. True when the frame can go
    // to the new decoder.
    fn other_codec(
        &mut self,
        codec: Codec,
        number: u32,
        idr: bool,
        report: &mut dyn FnMut(Report<'_>),
    ) -> Result<bool, String> {
        let now = Instant::now();
        if let Some(until) = self.new_decoders.until(now) {
            self.numbers.decode_failed += 1;
            let Some(holds) = self.new_decoders.held(number, until, now) else {
                return Ok(false);
            };
            if holds >= HOLDS_TO_STOP {
                return Err(format!(
                    "the share kept changing codec, more than {NEW_DECODERS_PER_GAP} times in {} s, which a Booth sharer does not do",
                    SWITCH_GAP.as_secs()
                ));
            }
            report(Report::Line(Line::Log(format!(
                "frame {number} would need a new decoder for {codec}, past {NEW_DECODERS_PER_GAP} in {} s: frames of it wait {:.0} ms",
                SWITCH_GAP.as_secs(),
                ms(until.saturating_duration_since(now))
            ))));
            return Ok(false);
        }
        if !idr {
            self.numbers.decode_failed += 1;
            let decoder = self.decoder.codec();
            self.said_once(
                format!(
                    "frame {number} is {codec}, the decoder is for {decoder}: it waits for an IDR of it"
                ),
                report,
            );
            self.asks.other_codec(number, now, &mut self.back);
            self.send_back(report);
            return Ok(false);
        }
        // The old decoder's GPU times name its own pictures: they go first,
        // and a frame still waiting for one counts with its present alone.
        self.take_gpu_times();
        self.waiting.give_up(&mut self.settled);
        self.settle();
        self.new_decoders.made(now);
        let made = Instant::now();
        match Decoder::new(self.viewer.device(), codec) {
            Ok(decoder) => {
                report(Report::Line(Line::Log(format!(
                    "frame {number} starts {codec}: a new decoder in place of the {} one, made in {:.1} ms",
                    self.decoder.codec(),
                    ms(made.elapsed())
                ))));
                self.decoder = decoder;
                if std::mem::take(&mut self.decoder_used) {
                    self.numbers.codec_changes += 1;
                }
                self.codec_said = false;
                Ok(true)
            }
            Err(err @ DecodeError::DeviceLost { .. }) => Err(err.to_string()),
            Err(err) if codec == Codec::Hevc => {
                self.numbers.decode_failed += 1;
                self.no_hevc(&err.to_string(), report);
                Ok(false)
            }
            Err(err) => self
                .lost(number, &err.to_string(), true, report)
                .map(|()| false),
        }
    }

    // Frames in a codec the decoder is not for, said in the log once until
    // a new decoder comes.
    fn said_once(&mut self, line: String, report: &mut dyn FnMut(Report<'_>)) {
        if !std::mem::replace(&mut self.codec_said, true) {
            report(Report::Line(Line::Log(line)));
        }
    }

    // FFmpeg set its decoder and surfaces up again for a stream that had
    // them. Past NEW_DECODERS_PER_GAP within a SWITCH_GAP each one is a
    // hold, and a stream that keeps doing it ends the watch.
    fn set_up_again(&mut self, number: u32) -> Result<(), String> {
        let now = Instant::now();
        if let Some(until) = self.new_decoders.until(now)
            && self
                .new_decoders
                .held(number, until, now)
                .is_some_and(|holds| holds >= HOLDS_TO_STOP)
        {
            return Err(format!(
                "the share kept changing size, more than {NEW_DECODERS_PER_GAP} times in {} s, which a Booth sharer does not do",
                SWITCH_GAP.as_secs()
            ));
        }
        self.new_decoders.made(now);
        Ok(())
    }

    // This viewer takes no HEVC from now on, and the sharer hears it once.
    fn no_hevc(&mut self, why: &str, report: &mut dyn FnMut(Report<'_>)) {
        if std::mem::replace(&mut self.takes_hevc, false) {
            report(Report::Line(Line::Say(format!(
                "this viewer cannot show HEVC: {why}. Asking for H.264"
            ))));
            report(Report::NoHevc);
        }
    }

    // Err ends the run, with the decoder's sentence, when the GPU's decoder
    // keeps refusing the stream.
    fn lost(
        &mut self,
        number: u32,
        why: &str,
        refused: bool,
        report: &mut dyn FnMut(Report<'_>),
    ) -> Result<(), String> {
        self.numbers.decode_failed += 1;
        let line = format!("frame {number} did not decode: {why}");
        match self.numbers.decode_failed {
            1 => report(Report::Line(Line::Say(line))),
            2..=FAILURES_LOGGED => report(Report::Line(Line::Log(line))),
            _ => {}
        }
        self.asks
            .failed(number, refused, Instant::now(), &mut self.back);
        self.send_back(report);
        if self.asks.refused_too_often() {
            return Err(why.to_string());
        }
        Ok(())
    }

    // Draws the last picture again, with the pointer where it is now.
    fn redraw(&mut self) -> Result<(), String> {
        self.present(None).map(|_| ())
    }

    // When the present returned, or None when nothing was shown: minimized,
    // or the window closed just now, which the loop sees next.
    fn present(&mut self, video: Option<Video<'_>>) -> Result<Option<Instant>, String> {
        let presented = self.viewer.present(&viewer::Frame {
            video,
            cursor: self.pointer.as_ref(),
            strip: &self.strip,
        });
        let returned = Instant::now();
        // The viewer keeps a shape once it has it; it comes again only when
        // it changes.
        if let Some(pointer) = &mut self.pointer {
            pointer.shape = None;
        }
        match presented {
            Ok(presented) => {
                self.shown_this_wake = true;
                // None after a resize or F11 until Windows reports on a
                // present made since, and the log says so then rather than
                // repeat the word from before.
                self.numbers.path = presented.path;
                Ok(presented.shown.then_some(returned))
            }
            Err(err) if err.kind() == ErrorKind::Closed => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn update_strip(&mut self, loss: Option<f32>) {
        if !self.link_loss {
            self.strip.loss_pct = loss;
            self.strip.loss_level =
                loss.map_or(Level::Good, |pct| Thresholds::default().loss_level(pct));
        }
        // A still screen decodes nothing, and the last picture's time would
        // wait for the next.
        self.take_gpu_times();
        let mut window = std::mem::take(&mut self.times.quarter);
        let mut encode = self.inbox.take_encode_ms();
        let framed = std::mem::take(&mut self.framed_encode_ms);
        if encode.is_empty() {
            encode = framed;
        }
        self.times.second.encode_ms.extend_from_slice(&encode);
        let strip = &mut self.strip;
        // A still screen sends nothing; the numbers of the last frames stay.
        if let Some((median, _)) = spread(&mut encode) {
            strip.encode_ms = Some(median);
            strip.encode_level = stage_level(median, self.interval);
        }
        if let Some((median, _)) = spread(&mut window.decode_ms) {
            strip.decode_ms = Some(median);
            strip.decode_level = stage_level(median, self.interval);
        }
        if let Some((median, _)) = spread(&mut window.end_to_end_ms) {
            strip.end_to_end_ms = Some(median);
        }
        // Also for the last number of a still screen, when the path changed.
        if let Some(median) = strip.end_to_end_ms {
            strip.end_to_end_level = end_to_end_level(median, self.one_way_ms);
        }
    }

    fn set_link(&mut self, link: LinkNumbers) {
        self.one_way_ms = link.one_way_ms.unwrap_or(0.0);
        self.link_loss = show_link(&mut self.strip, link);
    }

    // The strip says so while this PC controls, with the release key as the
    // app names it, asked once as control starts. A viewer with nowhere to
    // send its input captures nothing, and says nothing either.
    fn set_control(&mut self, control: Option<Control>) {
        let starts = self.control.is_none();
        self.control = control;
        match (control, &self.out) {
            (Some(control), Some(out)) => {
                if starts || self.strip.controlling.is_none() {
                    self.strip.controlling = Some(out.release_key());
                }
                self.strip.control_paused = control.paused;
            }
            _ => {
                self.strip.controlling = None;
                self.strip.control_paused = false;
            }
        }
    }

    // Relative mode while the sharer's pointer is hidden, as a game hides
    // it; absolute mode otherwise, the pattern included, which has no
    // pointer at all.
    fn follow_mouse_mode(&mut self) {
        let mode = self
            .control
            .filter(|_| self.out.is_some())
            .map(|_| mouse_mode(self.pointer.as_ref()));
        if mode != self.mode {
            self.mode = mode;
            self.viewer.set_control(mode);
        }
    }
}

fn mouse_mode(pointer: Option<&Cursor>) -> MouseMode {
    if pointer.is_some_and(|pointer| !pointer.visible) {
        MouseMode::Relative
    } else {
        MouseMode::Absolute
    }
}

// The caller's link numbers into the strip. True when they carry a loss
// number, which then shows instead of the video's.
fn show_link(strip: &mut Strip, link: LinkNumbers) -> bool {
    strip.state = link.state;
    strip.rtt_ms = link.rtt_ms;
    strip.rtt_level = link.rtt_level;
    strip.jitter_ms = link.jitter_ms;
    strip.jitter_level = link.jitter_level;
    strip.path = link.path;
    strip.trace = link.trace;
    match link.loss_pct {
        Some(pct) => {
            strip.loss_pct = Some(pct);
            strip.loss_level = link.loss_level;
            true
        }
        None => false,
    }
}

fn share_codec(codec: Codec) -> ShareCodec {
    match codec {
        Codec::H264 => ShareCodec::H264,
        Codec::Hevc => ShareCodec::Hevc,
    }
}

// Whether a viewer on this PC would take HEVC, asked before any viewer
// opens: the driver's answer on the GPU that drives the primary monitor,
// where a viewer's window opens unless the pointer is on another GPU's
// monitor, at the largest picture a share sends, so it holds whatever the
// share's size. The viewer asks again on its own device when it opens
// (Screen::takes_hevc). Makes a Direct3D device for it and lets it go.
pub fn primary_takes_hevc() -> Result<(), String> {
    let monitors = capture::monitors().map_err(|err| err.to_string())?;
    let adapter = match monitors.iter().find(|m| m.primary).or(monitors.first()) {
        Some(monitor) => monitor.adapter.clone(),
        None => capture::adapters()
            .map_err(|err| err.to_string())?
            .into_iter()
            .next()
            .ok_or("this PC has no graphics card to show a share on")?,
    };
    let device = capture::device_on(&adapter).map_err(|err| err.to_string())?;
    let largest = capture::Options::default();
    decode::probe(&device, Codec::Hevc, largest.max_width, largest.max_height)
        .map_err(|err| err.to_string())
}

// The stream itself refused, by the GPU's decoder or by the guard in front of
// FFmpeg, which comes back with every IDR of it, rather than one frame being
// broken.
fn refuses_the_stream(err: &DecodeError) -> bool {
    matches!(
        err,
        DecodeError::Unsupported { .. }
            | DecodeError::WrongFormat { .. }
            | DecodeError::Oversized { .. }
            | DecodeError::UnreadableSps
            | DecodeError::TooManySps { .. }
            | DecodeError::NotOnGpu
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refusal_of_the_stream_counts_toward_stopping() {
        let oversized = DecodeError::Oversized {
            width: 8192,
            height: 4608,
        };
        assert!(refuses_the_stream(&oversized));
        assert!(refuses_the_stream(&DecodeError::NotOnGpu));
        // An SPS the guard in front of FFmpeg cannot read will not read on
        // the next IDR either, and a sharer that packs a unit with SPSs will
        // do it again: asking for an IDR would loop.
        assert!(refuses_the_stream(&DecodeError::UnreadableSps));
        assert!(refuses_the_stream(&DecodeError::TooManySps { count: 40 }));
        let damaged = DecodeError::Damaged {
            detail: String::from("invalid data"),
        };
        assert!(!refuses_the_stream(&damaged));
        assert!(!refuses_the_stream(&DecodeError::Empty));
        assert!(!refuses_the_stream(&DecodeError::HeldBack { frames: 1 }));
    }

    // Capture to display waits for the GPU's time of the picture, and runs
    // to the later of the present returning and the picture being done.
    #[test]
    fn capture_to_display_ends_at_the_later_time() {
        let captured = Instant::now();
        let at = |ms: u64| captured + Duration::from_millis(ms);
        let shown = |unit: u64, returned: u64| Shown {
            unit,
            captured: Some(captured),
            returned: at(returned),
        };
        let (mut waiting, mut settled) = (Waiting::default(), Vec::new());
        waiting.presented(shown(1, 3), &mut settled);
        waiting.presented(shown(2, 4), &mut settled);
        waiting.presented(shown(3, 5), &mut settled);
        assert!(settled.is_empty(), "nothing before its GPU time");
        // Unit 1's time never came; unit 2's picture was done after its
        // present returned.
        waiting.gpu_time(2, at(6), &mut settled);
        assert_eq!(settled, [3.0, 6.0]);
        settled.clear();
        // Done before the present returned: the present counts.
        waiting.gpu_time(3, at(4), &mut settled);
        assert_eq!(settled, [5.0]);
        settled.clear();
        // A time for a frame never presented settles nothing.
        waiting.gpu_time(9, at(9), &mut settled);
        assert!(settled.is_empty());

        // With no GPU times at all, each frame counts with its present
        // alone once MOST_WAITING newer ones wait.
        for unit in 10..10 + MOST_WAITING as u64 + 2 {
            waiting.presented(shown(unit, 7), &mut settled);
        }
        assert_eq!(settled, [7.0, 7.0]);
        settled.clear();
        waiting.give_up(&mut settled);
        assert_eq!(settled.len(), MOST_WAITING);
        let unknown = Shown {
            captured: None,
            ..shown(0, 1)
        };
        assert_eq!(unknown.end_to_end(Some(at(2))), None);
    }

    // A share's first HEVC IDR, a change to H.264 and a fallback right after
    // it each get a decoder at once. A fourth within a SWITCH_GAP of the
    // first waits for the gap, and its IDR is asked for once, when the gap
    // is over. A stream that keeps asking for more ends the watch at its
    // third hold within HOLDS_WITHIN; holds longer ago do not count.
    #[test]
    fn new_decoders_past_the_limit_wait() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let gap = SWITCH_GAP.as_millis() as u64;
        let mut new = NewDecoders::default();
        for ms in [0, 500, 600] {
            assert_eq!(new.until(at(ms)), None, "at {ms} ms");
            new.made(at(ms));
        }
        let until = new.until(at(700)).expect("a fourth waits");
        assert_eq!(until, at(gap));
        assert_eq!(new.held(10, until, at(700)), Some(1));
        assert_eq!(new.held(11, until, at(710)), None, "the same hold");
        assert_eq!(new.deadline(), Some(until));
        assert_eq!(new.over(at(gap - 1)), None);
        assert_eq!(new.over(at(gap)), Some(11));
        assert_eq!((new.over(at(gap + 1)), new.deadline()), (None, None));
        assert_eq!(new.until(at(gap)), None);
        new.made(at(gap));

        // Past the limit again at once: a second hold, then a third.
        let until = new.until(at(gap + 10)).expect("held again");
        assert_eq!(until, at(500 + gap));
        assert_eq!(new.held(20, until, at(gap + 10)), Some(2));
        new.made(at(500 + gap));
        let until = new.until(at(510 + gap)).expect("and again");
        assert_eq!(new.held(30, until, at(510 + gap)), Some(3));

        // A hold HOLDS_WITHIN after the first two counts alone.
        let later = HOLDS_WITHIN.as_millis() as u64 + 600 + gap;
        let mut new = NewDecoders::default();
        for ms in [0, 1, 2] {
            new.made(at(ms));
        }
        assert_eq!(new.held(1, new.until(at(3)).unwrap(), at(3)), Some(1));
        for ms in [gap, gap + 1, gap + 2] {
            new.made(at(ms));
        }
        let until = new.until(at(gap + 3)).unwrap();
        assert_eq!(new.held(2, until, at(gap + 3)), Some(2));
        for ms in [later, later + 1, later + 2] {
            new.made(at(ms));
        }
        let until = new.until(at(later + 3)).unwrap();
        assert_eq!(new.held(3, until, at(later + 3)), Some(1));
    }

    // A game hides the sharer's pointer: raw motion goes. The desktop, and a
    // share with no pointer at all such as the pattern, get points.
    #[test]
    fn raw_mouse_only_while_the_pointer_is_hidden() {
        let shown = Cursor {
            x: 0,
            y: 0,
            visible: true,
            scale: 1.0,
            shape: None,
        };
        let hidden = Cursor {
            visible: false,
            ..shown.clone()
        };
        assert_eq!(mouse_mode(None), MouseMode::Absolute);
        assert_eq!(mouse_mode(Some(&shown)), MouseMode::Absolute);
        assert_eq!(mouse_mode(Some(&hidden)), MouseMode::Relative);
    }

    #[test]
    fn the_callers_link_fills_the_strip() {
        let mut strip = Strip::default();
        let looped = LinkNumbers {
            state: LinkState::Live,
            path: Some(PathWord::Loop),
            ..LinkNumbers::default()
        };
        assert!(!show_link(&mut strip, looped));
        assert_eq!(
            (strip.state, strip.path, strip.rtt_ms),
            (LinkState::Live, Some(PathWord::Loop), None)
        );
        let room = LinkNumbers {
            state: LinkState::Live,
            rtt_ms: Some(4.0),
            loss_pct: Some(1.5),
            loss_level: Level::Warn,
            path: Some(PathWord::Lan),
            trace: vec![TraceSample::Rtt(4.0), TraceSample::Lost],
            ..LinkNumbers::default()
        };
        assert!(show_link(&mut strip, room));
        assert_eq!(
            (strip.rtt_ms, strip.loss_pct, strip.loss_level),
            (Some(4.0), Some(1.5), Level::Warn)
        );
        assert_eq!(strip.trace.len(), 2);
    }

    // A room's viewer keeps a second's times at most, however long the
    // share runs; the loopback keeps them all for its summary.
    #[test]
    fn times_for_the_whole_run_are_kept_only_when_asked() {
        let mut room = Times::new(false);
        let mut looped = Times::new(true);
        for times in [&mut room, &mut looped] {
            for frame in 0..1000 {
                times.add(Stage::Decode, 1.5);
                times.add(Stage::DecodeCall, 0.1);
                if frame % 2 == 0 {
                    times.add(Stage::EndToEnd, 3.0);
                }
                times.presented();
            }
            let second = std::mem::take(&mut times.second);
            assert_eq!(second.decode_ms.len(), 1000);
            assert_eq!(second.end_to_end_ms.len(), 500);
            assert_eq!((second.presented, times.quarter.presented), (1000, 1000));
        }
        assert!(room.run.is_none());
        let mut numbers = ViewerNumbers::default();
        room.into_numbers(&mut numbers);
        assert!(numbers.decode_ms.is_empty() && numbers.end_to_end_ms.is_empty());
        looped.into_numbers(&mut numbers);
        assert_eq!(
            (
                numbers.decode_ms.len(),
                numbers.decode_call_ms.len(),
                numbers.end_to_end_ms.len()
            ),
            (1000, 1000, 500)
        );
    }
}
