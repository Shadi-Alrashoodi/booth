// The sharer's side: one frame at a time from the source, encoded on the
// source's thread (encode::open_codec says why), cut into packets with
// parity and handed to the pacer. Where the packets go is the pacer's send
// function, and where the pointer goes and the viewers' answers come from
// is the caller's Audience: the loopback's link now, the room's socket
// next.

use std::time::{Duration, Instant};

use capture::{CursorUpdate, Frame, Next, PauseReason};
use channels::video::{FrameFacts, PacketizeError, Packetizer, parity_percent};
use encode::{AccessUnit, Codec, EncodeError, Encoder, Kind, Settings};
use net::{PaceNumbers, Pacer};

use crate::numbers::{SharerNumbers, ms};
use crate::recovery::{Again, Answers, Back};
use crate::source::{self, Choice, HevcOffer, Opened, Pick, Source};
use crate::{Clock, Line};

// A frame the fps cap held went out on a later call of next(), when its time
// came, rather than as soon as it was converted.
const HELD_AFTER: Duration = Duration::from_millis(1);

// What step_down opens at, and the most the software encoder takes:
// encode::software_fit's rate.
const SMALL_FPS: u32 = 60;

// next() not called for this long, as while nobody watches and the room
// lets the share rest, and the picture capture hands over next can be one
// presented long before: capture to display counts from when it was asked
// for. Calls come back to back otherwise, capture's own wait being 100 ms.
const RESTED_AFTER: Duration = Duration::from_millis(250);

// A share left to pick its codec (Setup::codec) changes it at most this
// often. Each change opens a new encoder, which holds up the frame it opens
// for (5 to 8 ms for NVENC at 1440p, tests/sharer.rs), and starts with an
// IDR of about six frames' worth. A viewer who needs H.264 gets it at once
// unless a change came within the gap, and HEVC comes back only once no
// viewer has needed H.264 for a whole gap, so someone who closes the
// viewer and opens it again within it costs no change at all.
pub const SWITCH_GAP: Duration = Duration::from_secs(3);

// Why a share ended when the software encoder did not open after a GPU
// encoder failed, or failed on any frame later, as the room shows it after
// "Sharing stopped: ". The log has the encoders' own errors.
pub const SOFTWARE_DID_NOT_START: &str = "the GPU encoder failed and the software encoder did not start. Update the graphics driver, then share again";
pub const SOFTWARE_FAILED_TOO: &str = "the GPU encoder failed and the software encoder failed too. Update the graphics driver, then share again";

// Why a share runs on Windows' software encoder until it ends, which the
// panel says once. A share on a GPU with no encoder Booth can use lands on
// it as well, through encode::open_codec's order, and the panel has a
// sentence of its own for that one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Software {
    // Intel graphics, whose hardware encoders Booth does not use yet
    // (source::starts_in_software).
    Intel,
    // The GPU encoder failed during the share (Sharer::fall_back).
    GpuFailed,
}

impl Software {
    pub fn sentence(self) -> &'static str {
        match self {
            Software::Intel => {
                "Intel GPU encoders are not supported yet. Sharing will use the software encoder at up to 1080p60."
            }
            Software::GpuFailed => {
                "The GPU encoder failed, so sharing goes on with the software encoder at up to 1080p60. If it keeps happening, update the graphics driver."
            }
        }
    }
}

// How a share on the software encoder picks the codec of each new encoder:
// H.264, the one codec Windows' software encoder has.
const SOFTWARE_PICK: Pick = Pick {
    codec: Some(Codec::H264),
    takes_hevc: false,
};

#[derive(Debug, Clone, Copy)]
struct OnSoftware {
    why: Software,
    // The width and height limits capture had when the share went onto the
    // software encoder: its fit of the share, or capture's own when the
    // share fitted already. Opened again, the source gets no more.
    limits: (u32, u32),
}

#[derive(Clone)]
pub struct Setup {
    pub choice: Choice,
    pub fps: u32,
    pub settings: Settings,
    // One encoder, for the loopback and the tests. None opens the fastest
    // one that works, as a share does, and on Intel graphics the software
    // encoder (source::starts_in_software). Either way a GPU encoder that
    // fails hands the share to the software encoder for the rest of it
    // (Sharer::fall_back).
    pub encoder: Option<Kind>,
    // The codec, or None to leave it to the share: HEVC when this GPU's
    // encoder offers it at the share's size and rate (on an NVIDIA GPU,
    // through NVENC only) and every viewer decodes it, H.264 otherwise. A
    // share left to pick changes codec as its viewers come and go, each
    // change a new encoder whose first frame is an IDR (SWITCH_GAP). Either
    // codec asked for opens Intel's hardware encoders as well.
    pub codec: Option<Codec>,
    // Whether every viewer decodes HEVC, for a share left to pick: what the
    // first encoder opens for, before Audience::takes_hevc is asked, then
    // its newest answer, which an encoder opened for a new size or rate
    // follows too.
    pub takes_hevc: bool,
    // Video bytes in a packet: PAYLOAD_INTERNET, or PAYLOAD_LAN when every
    // viewer is on the LAN.
    pub payload: usize,
    // Each frame's packets spread over half a frame interval, for a viewer
    // on an internet path (net::pace), or all at once.
    pub spread: bool,
    // The clock the frames' capture and encode times are written in.
    pub clock: Clock,
    // Every frame's encode time kept in the numbers, for a summary at the
    // end as the loopback prints. Off in a room, where a share runs for
    // hours; Audience::sent has each one either way.
    pub keep_times: bool,
}

// The sharer's side of whoever watches, besides the packets themselves.
// Every call comes from the thread that calls Sharer::next.
pub trait Audience {
    // A pointer update, the moment capture has it and never behind a frame.
    fn cursor(&mut self, update: CursorUpdate);
    // What came back from the viewers since the last call, added to `into`.
    // Asked just before each frame is encoded, so the answer is in it.
    fn back(&mut self, into: &mut Vec<Back>);
    // Whether someone started watching since the last call, asked just
    // after back(). The frame about to be encoded is then an IDR for them,
    // at once: an IDR a viewer asks for through back() waits for the floor
    // between IDRs, since a friend's PC can ask for one every time.
    fn started_watching(&mut self) -> bool {
        false
    }
    // Whether every viewer decodes HEVC, asked just after started_watching
    // before each frame when Setup::codec leaves the codec to the share. A
    // new watcher's part in it should count from the same frame that
    // started_watching first says true for, so one IDR, the new encoder's
    // first frame, does for both: the room's host sends a new watcher's
    // IDR ask inside the facts that count them.
    fn takes_hevc(&mut self) -> bool {
        true
    }
    // A frame went to the pacer.
    fn sent(&mut self, frame: &Sent);
    fn line(&mut self, line: Line);
    // Windows took the desktop away (capture::Next::Paused): no picture
    // comes until it gives it back, and the next Frame says it did. Once
    // each time.
    fn paused(&mut self, reason: &PauseReason) {
        self.line(Line::Say(format!("capture paused: {reason}")));
    }
    // The first picture from capture after a pause. A still screen's last
    // picture sent again meanwhile is not one: it is from before.
    fn resumed(&mut self) {
        self.line(Line::Say(String::from("capture back")));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sent {
    pub number: u32,
    pub idr: bool,
    // The access unit, without parity.
    pub bytes: usize,
    // Data and parity: how many packets, and the bytes they carry, which
    // the rate holds against itself (rate::NEAR_SHARE).
    pub packets: usize,
    pub packet_bytes: usize,
    pub encode_ms: f32,
    pub parity: u32,
}

pub struct Sharer {
    // None only after a failed step_down: the share is over.
    opened: Option<Opened>,
    setup: Setup,
    size: (u32, u32),
    fps: u32,
    interval: Duration,
    packetizer: Packetizer,
    pacer: Pacer,
    // The next frame's number and encoder index; one counter for both, so a
    // recover request's numbers are the encoder's indices. It goes on across
    // new encoders.
    next: u64,
    answers: Answers,
    // The viewers' last loss report, which sets each frame's parity
    // (channels::video::parity_for_loss), and the percentage the stats
    // panel shows for it (channels::video::parity_percent).
    loss: Option<f32>,
    parity: u32,
    back: Vec<Back>,
    numbers: SharerNumbers,
    began: Option<Instant>,
    // The width and height limits step_down opened capture with, until
    // step_up. set_fps gives capture the same ones again: the size they
    // came out at would not do as limits, since capture works the other
    // side out from the monitor's size and can round it a few pixels
    // smaller.
    small: Option<(u32, u32)>,
    // The rate the encoder in use took, in bits a second: what it opened
    // with, or the last set_bitrate it did not refuse.
    encoder_bitrate: u32,
    // The last picture sent, for a still screen to send again as an IDR
    // when someone starts watching. Capture converts only into its other
    // texture until it hands over a newer picture, so this one stays whole
    // meanwhile.
    last: Option<Frame>,
    // Capture said it paused, and no picture came since.
    paused: bool,
    // When next() last returned.
    left: Option<Instant>,
    // For a share left to pick its codec: when it changed codec and when a
    // viewer last needed H.264, and what encode::offer said of HEVC at the
    // share's size and rate.
    switches: Switches,
    hevc_offer: Option<HevcOffer>,
    // How much of the encoder's notes the log has: a Media Foundation
    // encoder adds to them as it goes, about frames it held back or a new
    // output format, which is what the log of a GPU that cannot be tried
    // here should show.
    notes_said: usize,
    // Set once the share runs on the software encoder until it ends: every
    // encoder from then on is that one, in H.264, at no more than it takes.
    // A GPU encoder that stalled at one size would cost the same freeze
    // again at the next, and a step down and back up would try it twice.
    // After a GPU encoder failed, any failure ends the share with
    // SOFTWARE_FAILED_TOO.
    software: Option<OnSoftware>,
    // What capture was last asked for.
    options: capture::Options,
    // How many times the source opened again. A picture goes out again
    // only on the source that made it: the one before is on a texture of
    // its own device.
    reopens: u64,
    // A software encoder that took over from a failed GPU encoder, until
    // its first frame is out or it fails on it.
    fallback: Option<Fallback>,
}

// The change from a failed GPU encoder to the software one, timed for the
// log.
struct Fallback {
    was: String,
    failed: Instant,
    shut_down: Duration,
    opened: Duration,
    // The size and rate the source opened again at for the software
    // encoder, when the share was larger than it takes.
    reopened: Option<((u32, u32), u32)>,
}

impl Fallback {
    // A shutdown near 2 s is a Media Foundation encoder whose event thread
    // did not end when told to.
    fn timings(&self) -> String {
        let what = match self.reopened {
            Some(((width, height), fps)) => {
                format!(
                    "the source again at {width}x{height} and {fps} fps with the software encoder"
                )
            }
            None => String::from("the software encoder"),
        };
        format!(
            "{:.1} ms to shut the GPU encoder down, {:.1} ms to open {what}",
            ms(self.shut_down),
            ms(self.opened)
        )
    }

    // From the failure to the software encoder's first frame, which is the
    // failed frame made again or, on a source opened again, its first
    // picture.
    fn first_frame(&self, name: &str, index: u64, unit: &AccessUnit) -> String {
        let what = match self.reopened {
            Some(_) => "the new source's first picture",
            None => "it again",
        };
        format!(
            "{} to {name} in {:.1} ms from the failure to frame {index}: {}, {:.1} ms to encode {what}{}",
            self.was,
            ms(unit.ready.saturating_duration_since(self.failed)),
            self.timings(),
            ms(unit.encode_time()),
            if unit.idr { ", as an IDR" } else { "" }
        )
    }
}

// Whether a frame that did not encode hands the share to the software
// encoder: a GPU encoder's own failure does, but not a frame it should
// never have been given, which the software encoder would refuse as well.
fn falls_back(kind: Kind, err: &EncodeError) -> bool {
    kind != Kind::MfSoftware
        && !matches!(
            err,
            EncodeError::WrongFrame { .. } | EncodeError::FrameOutOfOrder { .. }
        )
}

impl Sharer {
    // Opens the source and an encoder for it, and starts the pacer's thread
    // with `send`, which runs there for every packet in order. What is worth
    // saying on the way, such as why the share runs smaller or which faster
    // encoder was passed over, goes to `say`.
    pub fn open(
        setup: Setup,
        send: impl FnMut(&[u8]) + Send + 'static,
        say: &mut dyn FnMut(Line),
    ) -> Result<Sharer, String> {
        let options = capture::Options {
            max_fps: setup.fps,
            ..capture::Options::default()
        };
        let adapter = setup.choice.adapter();
        let intel = source::starts_in_software(adapter.vendor_id, setup.encoder, setup.codec);
        let (kind, pick) = if intel {
            say(Line::Say(source::intel_in_software(&adapter.description)));
            (Some(Kind::MfSoftware), SOFTWARE_PICK)
        } else {
            let pick = Pick {
                codec: setup.codec,
                takes_hevc: setup.takes_hevc,
            };
            (setup.encoder, pick)
        };
        let mut opened = source::open(
            &setup.choice,
            kind,
            pick,
            &setup.settings,
            options,
            None,
            say,
        )?;
        let options = opened.options;
        let software = intel.then_some(OnSoftware {
            why: Software::Intel,
            limits: (options.max_width, options.max_height),
        });
        let hevc_offer = opened.hevc.take();
        let notes = opened.encoder.notes();
        if !notes.is_empty() {
            say(Line::Say(format!("{}: {notes}", opened.encoder.name())));
        }
        let notes_said = notes.len();
        let packetizer = Packetizer::new(setup.payload).map_err(|err| err.to_string())?;
        let pacer = Pacer::start(send).map_err(|err| err.to_string())?;
        if let Some(note) = pacer.note() {
            say(Line::Say(note.to_string()));
        }
        let encoder_bitrate = setup.settings.bitrate;
        let (size, fps) = (opened.source.size(), opened.fps);
        Ok(Sharer {
            opened: Some(opened),
            setup,
            size,
            fps,
            interval: Duration::from_secs(1) / fps,
            packetizer,
            pacer,
            next: 0,
            answers: Answers::new(encoder_bitrate),
            loss: None,
            parity: parity_percent(None),
            back: Vec::new(),
            numbers: SharerNumbers::default(),
            began: None,
            small: None,
            last: None,
            encoder_bitrate,
            paused: false,
            left: None,
            switches: Switches::default(),
            hevc_offer,
            notes_said,
            software,
            options,
            reopens: 0,
            fallback: None,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn encoder_name(&self) -> &str {
        self.opened
            .as_ref()
            .map_or("no encoder", |opened| opened.encoder.name())
    }

    // None only after a failed reopen, when the share is over.
    pub fn codec(&self) -> Option<Codec> {
        self.opened.as_ref().map(|opened| opened.encoder.codec())
    }

    // The parity percent the stats panel shows, from the viewers' loss
    // reports. Each frame's own follows the loss model, parity_for_loss.
    pub fn parity(&self) -> u32 {
        self.parity
    }

    pub fn numbers(&self) -> &SharerNumbers {
        &self.numbers
    }

    pub fn pace_numbers(&self) -> PaceNumbers {
        self.pacer.numbers()
    }

    // For the frames from the next one on. A ceiling, like
    // encode::Settings::bitrate.
    pub fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), String> {
        self.setup.settings.bitrate = bits_per_second;
        self.answers.set_bitrate(bits_per_second);
        if let Some(opened) = &mut self.opened {
            opened
                .encoder
                .set_bitrate(bits_per_second)
                .map_err(|err| err.to_string())?;
            self.encoder_bitrate = bits_per_second;
        }
        Ok(())
    }

    // What the encoder in use took: set_bitrate's rate unless it refused
    // that, and a source opened again takes the rate asked for last.
    pub fn encoder_bitrate(&self) -> u32 {
        self.encoder_bitrate
    }

    pub fn set_spread(&mut self, spread: bool) {
        self.setup.spread = spread;
    }

    // For the frames from the next one on: PAYLOAD_INTERNET once someone on
    // an internet or tunnel path starts watching, whose MTU a LAN packet
    // would not fit, and PAYLOAD_LAN again when every viewer is on the LAN.
    pub fn set_payload(&mut self, payload: usize) -> Result<(), String> {
        if payload != self.setup.payload {
            self.packetizer = Packetizer::new(payload).map_err(|err| err.to_string())?;
            self.setup.payload = payload;
        }
        Ok(())
    }

    // The next frame is an IDR, as for someone who just started watching,
    // however recent the last one.
    pub fn force_idr(&mut self) {
        self.answers.force_idr();
    }

    // Under 8 Mbit/s a viewer, 1440p120 is not worth its frames, and an
    // encoder that cannot keep up needs fewer pixels. The source and the
    // encoder open again at 1080p60's size and rate, the shape kept as for
    // the software encoder, and the next frame is an IDR. Frame numbers go
    // on. False when the share is that small already, as one on the
    // software encoder always is. An error ends the share.
    pub fn step_down(&mut self, say: &mut dyn FnMut(Line)) -> Result<bool, String> {
        let Some(fit) = encode::software_fit(self.size.0, self.size.1, self.fps) else {
            return Ok(false);
        };
        let options = capture::Options {
            max_width: fit.width,
            max_height: fit.height,
            max_fps: fit.fps,
        };
        self.reopen(options, say)?;
        self.small = Some((fit.width, fit.height));
        Ok(true)
    }

    // Back to the size and rate the share was asked for, after step_down,
    // or as near as the software encoder takes. False when it was not
    // stepped down, or when that is where it is already: a share that went
    // onto the software encoder while stepped down stays at that size.
    pub fn step_up(&mut self, say: &mut dyn FnMut(Line)) -> Result<bool, String> {
        if self.small.take().is_none() {
            return Ok(false);
        }
        self.reopen(self.source_options(self.setup.fps), say)
    }

    // The frame rate the share is asked for from now on. Stepped down or on
    // the software encoder, it counts only below 1080p60's rate, and step_up
    // brings the rest. True when the source opened again for it.
    pub fn set_fps(&mut self, fps: u32, say: &mut dyn FnMut(Line)) -> Result<bool, String> {
        if fps == 0 || fps == self.setup.fps {
            return Ok(false);
        }
        self.setup.fps = fps;
        let options = self.source_options(fps);
        if options.max_fps == self.fps {
            return Ok(false);
        }
        self.reopen(options, say)
    }

    pub fn stepped_down(&self) -> bool {
        self.small.is_some()
    }

    // None only after a failed reopen, when the share is over.
    pub fn kind(&self) -> Option<Kind> {
        self.opened.as_ref().map(|opened| opened.encoder.kind())
    }

    // Why the share runs on the software encoder until it ends, for the
    // panel's sentence (Software::sentence). None while a GPU encoder runs
    // it, and for a share that was set up for the software encoder or that
    // found no GPU encoder to open.
    pub fn software(&self) -> Option<Software> {
        self.software.map(|software| software.why)
    }

    // What capture is asked for at `fps`: as much as it gives, or step_down's
    // size while stepped down, and on the software encoder no more than it
    // took when the share went onto it.
    fn source_options(&self, fps: u32) -> capture::Options {
        match (self.small, self.software) {
            (Some((max_width, max_height)), _)
            | (
                None,
                Some(OnSoftware {
                    limits: (max_width, max_height),
                    ..
                }),
            ) => capture::Options {
                max_width,
                max_height,
                max_fps: fps.min(SMALL_FPS),
            },
            (None, None) => capture::Options {
                max_fps: fps,
                ..capture::Options::default()
            },
        }
    }

    // True when the source opened again, which it does only for options
    // other than the ones it has: on the software encoder a step or a new
    // rate can come to what it runs at already.
    fn reopen(
        &mut self,
        options: capture::Options,
        say: &mut dyn FnMut(Line),
    ) -> Result<bool, String> {
        if options == self.options && self.opened.is_some() {
            return Ok(false);
        }
        // Windows allows one duplication of a monitor per process, so the
        // old one goes before the new one opens, and the last picture with
        // it, since it is on the old one's texture.
        self.last = None;
        self.opened = None;
        self.reopens += 1;
        let mut opened = source::open(
            &self.setup.choice,
            self.encoder_kind(),
            self.pick(),
            &self.setup.settings,
            options,
            self.hevc_offer.clone(),
            say,
        )?;
        if let Some(offer) = opened.hevc.take() {
            self.hevc_offer = Some(offer);
        }
        self.size = opened.source.size();
        self.fps = opened.fps;
        self.interval = Duration::from_secs(1) / self.fps;
        self.notes_said = opened.encoder.notes().len();
        self.options = opened.options;
        self.opened = Some(opened);
        self.encoder_bitrate = self.setup.settings.bitrate;
        Ok(true)
    }

    // The encoder a new one is asked of: the one the share was set up for,
    // or the software encoder once the share is on it.
    fn encoder_kind(&self) -> Option<Kind> {
        match self.software {
            Some(_) => Some(Kind::MfSoftware),
            None => self.setup.encoder,
        }
    }

    // How a new encoder's codec is chosen: as the share was set up, and in
    // H.264 once the share is on the software encoder.
    fn pick(&self) -> Pick {
        match self.software {
            Some(_) => SOFTWARE_PICK,
            None => Pick {
                codec: self.setup.codec,
                takes_hevc: self.setup.takes_hevc,
            },
        }
    }

    // Waits for the source's next picture, at most 100 ms on a still screen,
    // and sends it: counts it, passes the pointer on, answers what came
    // back, encodes, packetizes and hands the packets to the pacer.
    pub fn next(&mut self, audience: &mut dyn Audience) -> Result<(), String> {
        let asked = Instant::now();
        self.began.get_or_insert(asked);
        let rested = self
            .left
            .is_some_and(|left| asked.saturating_duration_since(left) >= RESTED_AFTER);
        let result = self.take(asked, rested, audience);
        self.left = Some(Instant::now());
        result?;
        if let Some(why) = self.pacer.failure() {
            return Err(format!("the video send thread stopped: {why}"));
        }
        Ok(())
    }

    fn take(
        &mut self,
        asked: Instant,
        rested: bool,
        audience: &mut dyn Audience,
    ) -> Result<(), String> {
        let Some(opened) = &mut self.opened else {
            return Err(String::from(
                "the share has no picture: its source did not open again",
            ));
        };
        match opened.source.next()? {
            Next::Frame(mut frame) => {
                if rested {
                    frame.present = frame.present.max(asked);
                }
                if std::mem::take(&mut self.paused) {
                    audience.resumed();
                }
                self.count_source(&frame);
                if let Some(cursor) = frame.cursor.clone() {
                    audience.cursor(cursor);
                }
                if (frame.width, frame.height) != self.size {
                    self.new_size(&frame, audience)?;
                }
                self.answer(audience);
                let reopens = self.reopens;
                self.send(&frame, None, audience)?;
                if self.reopens == reopens {
                    self.last = Some(frame);
                }
            }
            Next::Cursor(update) => {
                audience.cursor(update);
                self.still(audience)?;
            }
            Next::Idle => self.still(audience)?,
            Next::Paused(reason) => {
                self.paused = true;
                audience.paused(&reason);
            }
        }
        Ok(())
    }

    // Stops the pacer; packets it has not sent are dropped.
    pub fn finish(mut self) -> SharerNumbers {
        self.numbers.pace = self.pacer.numbers();
        self.numbers.ran = self.began.map_or(Duration::ZERO, |began| began.elapsed());
        self.numbers
    }

    // No new picture: what came back is answered now rather than with the
    // next frame, and the last picture goes out again (recovery::Again)
    // when the answer needs it: an IDR someone waits for, such as a friend
    // who just started watching, or a frame after an invalidation, which
    // NVENC makes instead of an IDR. Otherwise the viewers would wait for
    // the screen to change. It also goes once more when the screen has
    // stood still a moment, so a last frame lost whole shows as a gap.
    fn still(&mut self, audience: &mut dyn Audience) -> Result<(), String> {
        self.answer(audience);
        let now = Instant::now();
        let Some(again) = self.answers.again(now) else {
            return Ok(());
        };
        let Some(mut frame) = self.last.clone() else {
            return Ok(());
        };
        // Capture to display measures the way to the viewer, not how long
        // the screen stood still.
        frame.present = now;
        frame.cursor = None;
        self.send(&frame, Some(again), audience)
    }

    fn count_source(&mut self, frame: &Frame) {
        let numbers = &mut self.numbers;
        numbers.captured += 1 + u64::from(frame.skipped);
        numbers.skipped += u64::from(frame.skipped);
        if frame.converted.elapsed() > HELD_AFTER {
            numbers.held += 1;
        }
    }

    // A mode change on the shared monitor: a new encoder, whose first frame
    // is an IDR, as a new share would start.
    fn new_size(&mut self, frame: &Frame, audience: &mut dyn Audience) -> Result<(), String> {
        let size = (frame.width, frame.height);
        audience.line(Line::Say(format!(
            "the picture went from {}x{} to {}x{}: starting a new encoder",
            self.size.0, self.size.1, size.0, size.1
        )));
        let pick = self.pick();
        let kind = self.encoder_kind();
        let Some(opened) = &mut self.opened else {
            return Ok(());
        };
        let mut say = |line| audience.line(line);
        let chosen = source::codec_for(
            &opened.source,
            &self.setup.choice,
            kind,
            pick,
            self.hevc_offer.take(),
            size,
            self.fps,
            &mut say,
        );
        self.hevc_offer = chosen.hevc;
        let settings = &self.setup.settings;
        let fps = self.fps;
        let encoder = opened
            .source
            .encoder(chosen.kind, chosen.codec, size, fps, settings);
        opened.encoder = match encoder {
            Ok(encoder) => encoder,
            Err(err) if chosen.codec == Codec::Hevc && self.setup.codec.is_none() => {
                audience.line(Line::Log(format!(
                    "HEVC did not open at {}x{}: {err}; going on in H.264",
                    size.0, size.1
                )));
                if let Some(offer) = &mut self.hevc_offer {
                    offer.kind = None;
                    offer.why_not = err.to_string();
                }
                opened
                    .source
                    .encoder(self.setup.encoder, Codec::H264, size, fps, settings)
                    .map_err(|err| err.to_string())?
            }
            Err(err) => return Err(err.to_string()),
        };
        self.notes_said = opened.encoder.notes().len();
        self.size = size;
        self.encoder_bitrate = self.setup.settings.bitrate;
        Ok(())
    }

    // What came back since the last frame, all of it before this frame is
    // encoded, so the answer is in it.
    fn answer(&mut self, audience: &mut dyn Audience) {
        let mut back = std::mem::take(&mut self.back);
        audience.back(&mut back);
        // First, so that it covers the losses reported with it.
        if audience.started_watching() {
            self.answers.force_idr();
        }
        let now = Instant::now();
        // Before the losses too: after a change they are about the old
        // encoder's frames, which the new one's first IDR covers.
        if self.setup.codec.is_none() {
            let takes_hevc = audience.takes_hevc();
            self.setup.takes_hevc = takes_hevc;
            self.pick_codec(takes_hevc, now, audience);
        }
        for message in back.drain(..) {
            match message {
                Back::Recover { first, last } => self.recover(first, last, now),
                Back::Idr { seen } => self.answers.idr_asked(seen, now, &mut self.numbers),
                Back::Loss(loss) => {
                    self.loss = loss;
                    self.parity = parity_percent(loss);
                }
            }
        }
        self.back = back;
    }

    // A share left to pick its codec changes it when the viewers' answer
    // and SWITCH_GAP say so, and never once it is on the software encoder,
    // which has H.264 only. An encoder that will not open leaves the one in
    // use as it is: HEVC is then taken as not offered at this size and
    // rate, and H.264 is tried again after the gap.
    //
    // The change to H.264 is the IDR a new watcher gets anyway. The change
    // back to HEVC is an IDR nobody asked for, so it also waits for the
    // floor after the last IDR, as one answering a loss does
    // (recovery::IDR_FLOOR_SHARE): a friend's PC coming and going without
    // HEVC cannot make IDRs past what that rule allows.
    fn pick_codec(&mut self, takes_hevc: bool, now: Instant, audience: &mut dyn Audience) {
        if self.software.is_some() {
            return;
        }
        let Some(current) = self.codec() else {
            return;
        };
        let Some(codec) = self.switches.due(current, takes_hevc, now) else {
            return;
        };
        if codec == Codec::Hevc && self.answers.within_floor(now) {
            return;
        }
        let kind = match codec {
            Codec::H264 => self.setup.encoder,
            Codec::Hevc => match self.offered_hevc(audience) {
                Some(kind) => Some(kind),
                None => return,
            },
        };
        self.switches.changed(now);
        let was = self.encoder_name().to_string();
        match self.switch(codec, kind) {
            Ok(()) => audience.line(Line::Log(format!(
                "{was} to {} in {:.1} ms: {}",
                self.encoder_name(),
                ms(now.elapsed()),
                match codec {
                    Codec::H264 => "a viewer does not decode HEVC",
                    Codec::Hevc => "every viewer decodes HEVC",
                }
            ))),
            Err(why) => audience.line(Line::Log(format!(
                "{was} stays: {codec} did not open: {why}"
            ))),
        }
    }

    // The encoder that takes HEVC at the share's size and rate, asked once
    // for each.
    fn offered_hevc(&mut self, audience: &mut dyn Audience) -> Option<Kind> {
        let opened = self.opened.as_ref()?;
        let pick = Pick {
            codec: None,
            takes_hevc: true,
        };
        let mut say = |line| audience.line(line);
        let chosen = source::codec_for(
            &opened.source,
            &self.setup.choice,
            self.setup.encoder,
            pick,
            self.hevc_offer.take(),
            self.size,
            self.fps,
            &mut say,
        );
        self.hevc_offer = chosen.hevc;
        (chosen.codec == Codec::Hevc)
            .then_some(chosen.kind)
            .flatten()
    }

    // A new encoder in `codec` in place of the one in use, whose first
    // frame is an IDR; frame numbers go on. The old one stays if the new
    // one does not open, and the error says why the new one did not.
    fn switch(&mut self, codec: Codec, kind: Option<Kind>) -> Result<(), String> {
        let Some(opened) = &mut self.opened else {
            return Err(String::from("the share has no encoder to change"));
        };
        let encoder = opened
            .source
            .encoder(kind, codec, self.size, self.fps, &self.setup.settings);
        let encoder: Box<dyn Encoder> = match encoder {
            Ok(encoder) => encoder,
            Err(err) => {
                if codec == Codec::Hevc
                    && let Some(offer) = &mut self.hevc_offer
                {
                    offer.kind = None;
                    offer.why_not = err.to_string();
                }
                return Err(err.to_string());
            }
        };
        opened.encoder = encoder;
        self.changed_encoder(true);
        Ok(())
    }

    // A new encoder is in place, the frame numbers going on: a new codec is
    // told to the viewers the way every such change is, by its first frame,
    // an IDR whose header says the codec.
    fn changed_encoder(&mut self, codec_changed: bool) {
        self.encoder_bitrate = self.setup.settings.bitrate;
        self.notes_said = self
            .opened
            .as_ref()
            .map_or(0, |opened| opened.encoder.notes().len());
        // The losses reported of the old encoder's frames are the new IDR's
        // to answer; the new encoder never made them.
        self.answers.force_idr();
        if codec_changed {
            self.numbers.codec_changes += 1;
        }
    }

    // The GPU encoder failed on frame `index`, as Intel's HEVC encoder did
    // on an Iris Xe laptop when it stopped asking for frames: Windows'
    // software encoder takes the share over in H.264 until it ends. The GPU
    // encoder goes first, all of it, shut down and its event thread ended,
    // so that nothing of it (the frames it holds, its events, its hold on
    // the Direct3D device) is still about while the software one starts on
    // the same device. A share the software encoder takes as it is goes on
    // with this same frame, made again as an IDR, and the result is true:
    // the caller sends the frame again. A larger one opens its source again
    // at the software encoder's fit, and its next picture is that encoder's
    // first, an IDR, under this frame's number. If the software encoder does
    // not open, the share ends with what to do, and if capture does not open
    // again, with capture's own error.
    fn fall_back(
        &mut self,
        index: u64,
        err: &EncodeError,
        audience: &mut dyn Audience,
    ) -> Result<bool, String> {
        let failed = Instant::now();
        let Some(opened) = self.opened.take() else {
            return Err(String::from("the share has no encoder to change"));
        };
        let Opened {
            mut encoder,
            source,
            fps,
            options,
            hevc,
        } = opened;
        let was = encoder.name().to_string();
        let was_codec = encoder.codec();
        audience.line(Line::Say(format!(
            "{was} could not encode frame {index}: {err}; the software encoder takes the share over until it ends"
        )));
        let notes = encoder.notes();
        if let Some(new) = notes.get(self.notes_said..).filter(|new| !new.is_empty()) {
            audience.line(Line::Log(format!(
                "{was}: {}",
                new.trim_start_matches(['.', ' '])
            )));
        }
        let left = encoder.shut_down();
        drop(encoder);
        let shut_down = failed.elapsed();
        if let Some(left) = left {
            audience.line(Line::Log(format!("{was}: {left}")));
        }
        let opening = Instant::now();
        let fit = encode::software_fit(self.size.0, self.size.1, self.fps);
        let did_not_start = |why: &dyn std::fmt::Display, audience: &mut dyn Audience| {
            audience.line(Line::Log(format!(
                "the software encoder did not open after {was} failed: {why} ({:.1} ms to shut the GPU encoder down, and {:.1} ms trying)",
                ms(shut_down),
                ms(opening.elapsed())
            )));
            String::from(SOFTWARE_DID_NOT_START)
        };
        let reopened = match fit {
            None => {
                let encoder = source
                    .encoder(
                        Some(Kind::MfSoftware),
                        Codec::H264,
                        self.size,
                        self.fps,
                        &self.setup.settings,
                    )
                    .map_err(|why| did_not_start(&why, audience))?;
                self.opened = Some(Opened {
                    encoder,
                    source,
                    fps,
                    options,
                    hevc,
                });
                None
            }
            Some(fit) => {
                audience.line(source::runs_smaller(fit, self.size, self.fps));
                // Windows allows one duplication of a monitor per process,
                // and the last picture is on the old one's texture.
                drop(source);
                self.last = None;
                self.reopens += 1;
                let options = capture::Options {
                    max_width: fit.width,
                    max_height: fit.height,
                    max_fps: fit.fps,
                };
                // Capture failing to open again is capture's own error, for
                // which a new graphics driver is no answer.
                let source = Source::open(&self.setup.choice, options).map_err(|why| {
                    audience.line(Line::Log(format!(
                        "the source did not open again for the software encoder after {was} failed: {why}"
                    )));
                    why
                })?;
                let size = source.size();
                let encoder = source
                    .encoder(
                        Some(Kind::MfSoftware),
                        Codec::H264,
                        size,
                        fit.fps,
                        &self.setup.settings,
                    )
                    .map_err(|why| did_not_start(&why, audience))?;
                self.size = size;
                self.fps = fit.fps;
                self.interval = Duration::from_secs(1) / self.fps;
                self.opened = Some(Opened {
                    encoder,
                    source,
                    fps: fit.fps,
                    options,
                    hevc,
                });
                Some((self.size, self.fps))
            }
        };
        let opened = opening.elapsed();
        if let Some(now) = &self.opened {
            self.options = now.options;
            // Which encoder took over and what it took, as a share's first
            // encoder says at open.
            if !now.encoder.notes().is_empty() {
                audience.line(Line::Log(format!(
                    "{}: {}",
                    now.encoder.name(),
                    now.encoder.notes()
                )));
            }
        }
        self.software = Some(OnSoftware {
            why: Software::GpuFailed,
            limits: (self.options.max_width, self.options.max_height),
        });
        self.changed_encoder(was_codec != Codec::H264);
        self.fallback = Some(Fallback {
            was,
            failed,
            shut_down,
            opened,
            reopened,
        });
        Ok(reopened.is_none())
    }

    fn new_notes(&mut self, audience: &mut dyn Audience) {
        let Some(opened) = &self.opened else {
            return;
        };
        let notes = opened.encoder.notes();
        if let Some(new) = notes.get(self.notes_said..).filter(|new| !new.is_empty()) {
            audience.line(Line::Log(format!(
                "{}: {}",
                opened.encoder.name(),
                new.trim_start_matches(['.', ' '])
            )));
        }
        self.notes_said = notes.len();
    }

    // Why the share ends on frame `index`, which did not encode. What the
    // encoder noted before it goes to the log first: a new output format or
    // a frame drained out is what tells one failure on a GPU that cannot be
    // tried here from another. Once a GPU encoder failed, the failure is the
    // software encoder's too, and the room shows it plainly; the log has the
    // error.
    fn encode_failed(
        &mut self,
        index: u64,
        err: &EncodeError,
        audience: &mut dyn Audience,
    ) -> String {
        self.new_notes(audience);
        if self.software() != Some(Software::GpuFailed) {
            return format!("could not encode frame {index}: {err}");
        }
        let first = self.fallback.take().map_or(String::new(), |fallback| {
            format!(", its first ({})", fallback.timings())
        });
        audience.line(Line::Log(format!(
            "{} could not encode frame {index}{first}: {err}",
            self.encoder_name()
        )));
        String::from(SOFTWARE_FAILED_TOO)
    }

    fn recover(&mut self, first: u32, last: u32, now: Instant) {
        if let Some(opened) = &mut self.opened {
            self.answers
                .recover(&mut *opened.encoder, first, last, now, &mut self.numbers);
        }
    }

    // A frame lost here before it left, recovered at once, or when the
    // floor ends if only an IDR recovers it; an IDR the pacer let go is
    // made again at once. The viewer reports it too once the next one
    // arrives, and the answer then is that it is covered.
    fn lost_here(&mut self, number: u32) {
        if let Some(opened) = &mut self.opened {
            self.answers.lost_here(
                &mut *opened.encoder,
                number,
                Instant::now(),
                &mut self.numbers,
            );
        }
    }

    // `again` is why a still screen sends its last picture again, None for
    // a picture from capture.
    fn send(
        &mut self,
        frame: &Frame,
        again: Option<Again>,
        audience: &mut dyn Audience,
    ) -> Result<(), String> {
        let Some(opened) = &mut self.opened else {
            return Ok(());
        };
        let index = self.next;
        let encoded = opened.encoder.encode(&encode::Frame {
            texture: &frame.texture,
            index,
            force_idr: self.answers.take_force_idr(Instant::now()),
        });
        let unit = match encoded {
            Ok(unit) => unit,
            // A GPU encoder can stop or fail where the software encoder
            // would not, as a Media Foundation one does that stops asking
            // for frames or answers a keyframe request with something else:
            // the share goes on with the software encoder (fall_back), on
            // this same frame when the source did not have to open again.
            Err(err) if falls_back(opened.encoder.kind(), &err) => {
                if self.fall_back(index, &err, audience)? {
                    return self.send(frame, again, audience);
                }
                return Ok(());
            }
            Err(err) => return Err(self.encode_failed(index, &err, audience)),
        };
        if let Some(fallback) = self.fallback.take() {
            audience.line(Line::Log(fallback.first_frame(
                opened.encoder.name(),
                index,
                &unit,
            )));
        }
        self.new_notes(audience);
        self.next += 1;
        self.answers.encoded(index, unit.idr);
        let encode_ms = ms(unit.encode_time());
        self.numbers.encoded += 1;
        self.numbers.bytes += unit.len() as u64;
        if self.setup.keep_times {
            self.numbers.encode_ms.push(encode_ms);
        }
        if unit.idr {
            self.numbers.idrs += 1;
        }
        let Some(opened) = &self.opened else {
            return Ok(());
        };
        // What the encoder does after a loss, on every frame: the viewer
        // holds frames after a lost IDR in any case.
        let facts = FrameFacts {
            number: index as u32,
            idr: unit.idr,
            survives_loss: opened.encoder.invalidates(),
            hevc: opened.encoder.codec() == Codec::Hevc,
            captured: self.setup.clock.micros(frame.present),
            encoded: self.setup.clock.micros(unit.ready),
        };
        let packets = match self
            .packetizer
            .packetize_for_loss(&facts, &unit.data, self.loss)
        {
            Ok(packets) => packets,
            Err(PacketizeError::TooBig { .. }) => {
                self.numbers.too_big += 1;
                self.lost_here(facts.number);
                return Ok(());
            }
            Err(err) => return Err(format!("could not packetize frame {index}: {err}")),
        };
        let mut burst = self.pacer.burst();
        let mut packet_bytes = 0;
        for packet in packets.iter() {
            burst.push(packet);
            packet_bytes += packet.len();
        }
        let count = burst.len();
        let discarded = self.pacer.numbers().discarded;
        self.pacer.put(
            burst,
            self.interval,
            self.setup.spread,
            self.encoder_bitrate,
        );
        let now = Instant::now();
        self.answers.went_out(now, again);
        if unit.idr {
            self.answers.idr_went_out(index, now, unit.len());
        }
        audience.sent(&Sent {
            number: facts.number,
            idr: facts.idr,
            bytes: unit.len(),
            packets: count,
            packet_bytes,
            encode_ms,
            parity: self.parity,
        });
        // The frame put before this one never went out. Only this thread
        // puts, so the count moving here is that frame and no other.
        if self.pacer.numbers().discarded > discarded
            && let Some(before) = index.checked_sub(1)
        {
            self.lost_here(before as u32);
        }
        Ok(())
    }
}

// When a share left to pick its codec may change it (SWITCH_GAP), kept
// apart from the encoders so the rule can be tried with plain times.
#[derive(Debug, Default)]
pub(crate) struct Switches {
    last: Option<Instant>,
    needed_h264: Option<Instant>,
}

impl Switches {
    // The codec to change to now, if any, before asking whether HEVC is
    // offered; `takes_hevc` is whether every viewer decodes it.
    pub(crate) fn due(&mut self, current: Codec, takes_hevc: bool, now: Instant) -> Option<Codec> {
        if !takes_hevc {
            self.needed_h264 = Some(now);
        }
        let quiet = |at: Option<Instant>| {
            at.is_none_or(|at| now.saturating_duration_since(at) >= SWITCH_GAP)
        };
        match (current, takes_hevc) {
            (Codec::Hevc, false) if quiet(self.last) => Some(Codec::H264),
            (Codec::H264, true) if quiet(self.last) && quiet(self.needed_h264) => Some(Codec::Hevc),
            _ => None,
        }
    }

    // A change was made, or tried and the encoder did not open.
    pub(crate) fn changed(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A viewer without HEVC comes and goes: H.264 at once, HEVC a gap after
    // it left, and never two changes within a gap.
    #[test]
    fn codec_changes_wait_for_the_gap() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut switches = Switches::default();
        // Everyone takes HEVC and the share is in it.
        assert_eq!(switches.due(Codec::Hevc, true, at(0)), None);
        // Someone who does not: H.264 at once, the first change of all.
        assert_eq!(switches.due(Codec::Hevc, false, at(100)), Some(Codec::H264));
        switches.changed(at(100));
        // They leave half a second later: HEVC waits for a gap after the
        // change and after they last needed H.264.
        assert_eq!(switches.due(Codec::H264, false, at(600)), None);
        assert_eq!(switches.due(Codec::H264, true, at(700)), None);
        assert_eq!(switches.due(Codec::H264, true, at(3599)), None);
        assert_eq!(switches.due(Codec::H264, true, at(3600)), Some(Codec::Hevc));
        switches.changed(at(3600));
        // They come back at once: H.264 waits for a gap after the change
        // back.
        assert_eq!(switches.due(Codec::Hevc, false, at(3700)), None);
        assert_eq!(switches.due(Codec::Hevc, false, at(6599)), None);
        assert_eq!(
            switches.due(Codec::Hevc, false, at(6600)),
            Some(Codec::H264)
        );
        switches.changed(at(6600));
        // Closing the viewer and opening it again within a gap of its last
        // frame: no change.
        assert_eq!(switches.due(Codec::H264, false, at(9_990)), None);
        assert_eq!(switches.due(Codec::H264, true, at(10_000)), None);
        assert_eq!(switches.due(Codec::H264, false, at(11_000)), None);
        assert_eq!(switches.due(Codec::H264, true, at(12_000)), None);
        assert_eq!(
            switches.due(Codec::H264, true, at(14_000)),
            Some(Codec::Hevc)
        );
        // A share already in the codec its viewers take never changes.
        let mut settled = Switches::default();
        assert_eq!(settled.due(Codec::H264, false, at(0)), None);
        assert_eq!(settled.due(Codec::Hevc, true, at(0)), None);
    }

    #[test]
    fn which_failures_fall_back_to_software() {
        let stalled = EncodeError::EncoderMisbehaved {
            problem: "the hardware encoder has not asked for a frame in 2 s",
        };
        let nvenc = EncodeError::Nvenc {
            action: "encode a frame",
            status: 20,
            detail: String::new(),
        };
        let no_output = EncodeError::NoOutput {
            index: 7,
            waited: Duration::from_secs(2),
        };
        for kind in [Kind::Nvenc, Kind::MfHardware] {
            for err in [&stalled, &nvenc, &no_output] {
                assert!(falls_back(kind, err), "{kind}: {err}");
            }
        }
        // The software encoder has nothing to fall back to, and a frame the
        // sharer got wrong would fail on it as well.
        assert!(!falls_back(Kind::MfSoftware, &stalled));
        let wrong = EncodeError::WrongFrame {
            problem: String::from("the texture is on another Direct3D device than the encoder"),
        };
        let out_of_order = EncodeError::FrameOutOfOrder {
            index: 3,
            previous: 4,
        };
        assert!(!falls_back(Kind::Nvenc, &wrong));
        assert!(!falls_back(Kind::MfHardware, &out_of_order));
    }

    // The panel shows these as they are. The room puts a share's end after
    // "Sharing stopped: " and adds the full stop.
    #[test]
    fn software_sentences() {
        let ended = |why: &str| format!("Sharing stopped: {why}.");
        for sentence in [
            Software::Intel.sentence().to_string(),
            Software::GpuFailed.sentence().to_string(),
            ended(SOFTWARE_FAILED_TOO),
            ended(SOFTWARE_DID_NOT_START),
        ] {
            assert!(sentence.is_ascii(), "{sentence}");
            assert!(!sentence.contains('!'), "{sentence}");
            assert!(
                sentence.ends_with('.') && !sentence.ends_with(".."),
                "{sentence}"
            );
            for part in sentence.split(". ") {
                assert!(
                    part.starts_with(|c: char| c.is_ascii_uppercase()),
                    "{sentence}"
                );
            }
        }
        for why in [SOFTWARE_FAILED_TOO, SOFTWARE_DID_NOT_START] {
            assert!(
                why.starts_with(|c: char| c.is_ascii_lowercase()) && !why.ends_with('.'),
                "{why}"
            );
        }
    }
}
