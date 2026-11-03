// The sharer's side: one frame at a time from the source, encoded on the
// source's thread (encode::open_codec says why), cut into packets with
// parity and handed to the pacer. Where the packets go is the pacer's send
// function, and where the pointer goes and the viewers' answers come from
// is the caller's Audience: the loopback's link now, the room's socket
// next.

use std::time::{Duration, Instant};

use capture::{CursorUpdate, Frame, Next, PauseReason};
use channels::video::{FrameFacts, PacketizeError, Packetizer, parity_percent};
use encode::{AccessUnit, Codec, EncodeError, Encoder, Fit, Kind, Settings};
use net::{PaceNumbers, Pacer};

use crate::numbers::{SharerNumbers, ms};
use crate::recovery::{Again, Answers, Back};
use crate::source::{self, Choice, HevcOffer, Opened, Pick, Source, Unopened};
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

// A source that did not open again at resume() is tried again once a
// capture::LOOK_EVERY: a monitor that is not listed, as often as capture
// looks for one, and one Windows refuses, since each such try makes a
// Direct3D device and compiles capture's shaders before Windows answers. In
// between next() waits this long and looks at the desktop, as capture does
// while it is paused.
const AWAY_WAIT: Duration = Duration::from_millis(100);

// A share left to pick its codec (Setup::codec) changes it at most this
// often. Each change opens a new encoder, which holds up the frame it opens
// for (5 to 8 ms for NVENC at 1440p, tests/sharer.rs), and starts with an
// IDR of about six frames' worth. A viewer who needs H.264 gets it at once
// unless a change came within the gap, when only a new encoder opened for
// another reason brings it sooner, and HEVC comes back only once no viewer
// has needed H.264 for a whole gap, so someone who closes the viewer and
// opens it again within it costs no change at all.
pub const SWITCH_GAP: Duration = Duration::from_secs(3);

// A shared window's new size gets a new encoder only this long after the
// last new encoder of any kind, the first after a quiet spell at once; in
// between the window goes out fitted into the frame it has. Every Booth
// viewer counts each new size or codec as a decoder set up again, and
// holds the picture when one comes while its last three came within
// SWITCH_GAP (the third hold in 30 s ends the watch). At this spacing any
// three span more than SWITCH_GAP, so one more the sharer cannot see or
// delay, such as a new watcher's first HEVC decoder, still makes no hold.
pub const RESIZE_SPACING: Duration = Duration::from_millis(1600);

// New encoders for a window's new size that did not open, in a row, before
// the share stops following the window's size and keeps it fitted.
const RESIZE_TRIES: u32 = 3;

// Why a share ended when the software encoder did not open after a GPU
// encoder failed, or failed on any frame later, as the room shows it after
// "Sharing stopped: ". The log has the encoders' own errors.
pub const SOFTWARE_DID_NOT_START: &str = "the GPU encoder failed and the software encoder did not start. Update the graphics driver, then share again";
pub const SOFTWARE_FAILED_TOO: &str = "the GPU encoder failed and the software encoder failed too. Update the graphics driver, then share again";

// Why a share runs on Windows' software encoder until it ends, which the
// panel says once. A share on a GPU with no encoder Booth can use lands on
// it as well, through encode::open_codec's order, and the panel has a
// message of its own for that one.
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
    // share fitted already, until a monitor's new mode needs a fit too.
    // Opened again, the source gets no more.
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
    // follows too, back to HEVC only once SWITCH_GAP allows it.
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
    // `with_picture` is true for the pointer of the picture about to be
    // encoded, which the watchers' viewers may draw with it, and false for
    // one that moved alone.
    fn cursor(&mut self, update: CursorUpdate, with_picture: bool);
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
    // None while paused by hand or resumed while its source would not open
    // again (away), and after a failed reopen, when the share is over.
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
    // What the watchers take, from the host (set_limit): the box a picture
    // fits in and the most frames a second. None keeps capture's defaults.
    limit: Option<capture::Options>,
    // How many times the source opened again. A picture goes out again
    // only on the source that made it: the one before is on a texture of
    // its own device.
    reopens: u64,
    // A software encoder that took over from a failed GPU encoder, until
    // its first frame is out or it fails on it.
    fallback: Option<Fallback>,
    // Paused by hand (pause), until resume.
    held: bool,
    // The codec it was in when paused by hand: resume's new encoder keeps to
    // SWITCH_GAP as any other does.
    held_in: Option<Codec>,
    // Resumed while its source would not open again (open_away).
    away: Option<Away>,
    // When the last new encoder opened, and for a window share, when a new
    // size may try again after a new encoder did not open and how many did
    // not in a row (RESIZE_SPACING, RESIZE_TRIES).
    last_encoder: Option<Instant>,
    resize_retry: Option<Instant>,
    resizes_failed: u32,
    // A shared window's size in the newest picture a source that went had
    // of it: a source opened again while the window is minimized starts at
    // it, since its caption says nothing of its size and its placement
    // nothing of a snap to a side of its monitor.
    window_size: Option<(u32, u32)>,
}

// A monitor's capture, or the test pattern standing in for one, that did not
// open again at resume() for what a running capture waits out: why, whether
// the audience heard it, when it was last tried, and how many tries in a row
// did not find the monitor listed.
struct Away {
    reason: PauseReason,
    told: bool,
    tried: Instant,
    misses: u32,
}

impl Away {
    // A try that did not open. False once a monitor still not listed
    // capture::LOOKS tries after the first is given up on, as a running
    // capture gives up on it; one found listed starts the count again.
    fn refused(&mut self, gone: bool) -> bool {
        self.misses = if gone { self.misses + 1 } else { 0 };
        self.misses <= capture::LOOKS
    }
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
        let adapter = setup.choice.adapter();
        let intel = source::starts_in_software(adapter.vendor_id, setup.encoder, setup.codec);
        let options = match setup.choice {
            Choice::Window { .. } if intel => source::window_box(setup.fps),
            _ => capture::Options {
                max_fps: setup.fps,
                ..capture::Options::default()
            },
        };
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
        opened.source.set_frame(Some(size));
        let mut sharer = Sharer {
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
            limit: None,
            reopens: 0,
            fallback: None,
            held: false,
            held_in: None,
            away: None,
            last_encoder: Some(Instant::now()),
            resize_retry: None,
            resizes_failed: 0,
            window_size: None,
        };
        sharer.box_software_window();
        Ok(sharer)
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

    // None while paused by hand or resumed while its source would not open
    // again, and after a failed reopen, when the share is over.
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

    pub fn gauge(&self) {
        stats::gauge!("share.sharer.back", self.back);
        stats::gauge!("share.sharer.encode_ms", self.numbers.encode_ms);
        self.answers.gauge();
        self.packetizer.gauge();
        if let Some(opened) = &self.opened {
            opened.encoder.gauge();
        }
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

    // Lets go of the source and the encoder, for someone who paused their
    // share: nothing of the screen is read until resume(), and the last
    // picture goes with them, as does what the pacer has not sent yet, so
    // whoever starts watching meanwhile gets nothing from before the pause.
    // next() is not called while it holds.
    pub fn pause(&mut self) {
        self.forget_pictures();
        self.held_in = self.codec().or(self.held_in);
        self.keep_window_size();
        self.opened = None;
        self.held = true;
        self.away = None;
    }

    // Before the source goes (window_size).
    fn keep_window_size(&mut self) {
        let opened = self.opened.as_ref();
        self.window_size = opened
            .and_then(|opened| opened.source.content())
            .or(self.window_size);
    }

    // Nothing from before goes out from here on: neither the last picture,
    // kept to send again, nor what the pacer still holds.
    fn forget_pictures(&mut self) {
        self.last = None;
        self.pacer.cancel();
    }

    // The same source again, at what it ran at before the pause, its first
    // frame an IDR; frame numbers go on. False when it was not paused. A
    // monitor's capture that Windows will not give the desktop yet, or that
    // is not listed, leaves the share paused as a running capture does:
    // next() says why and opens it once it can (open_away). Any other error
    // ends the share.
    pub fn resume(&mut self, say: &mut dyn FnMut(Line)) -> Result<bool, String> {
        if !std::mem::take(&mut self.held) {
            return Ok(false);
        }
        match self.reopen(self.options, say) {
            Ok(_) => {}
            Err(Unopened {
                away: Some(reason),
                gone,
                ..
            }) => {
                self.paused = true;
                self.away = Some(Away {
                    reason,
                    told: false,
                    tried: Instant::now(),
                    misses: u32::from(gone),
                });
            }
            Err(unopened) => return Err(unopened.why),
        }
        self.answers.force_idr();
        Ok(true)
    }

    pub fn held(&self) -> bool {
        self.held
    }

    // Why a source that did not open again at resume() stands away, until
    // it opens.
    pub fn away(&self) -> Option<&PauseReason> {
        self.away.as_ref().map(|away| &away.reason)
    }

    // The audience hears that reason now rather than at the next call, which
    // a share nobody watches does not make while it rests; next() then says
    // it only once it changes in kind.
    pub fn tell_away(&mut self, audience: &mut dyn Audience) {
        if let Some(away) = &mut self.away
            && !std::mem::replace(&mut away.told, true)
        {
            audience.paused(&away.reason);
        }
    }

    // No picture was asked for a while, as while nobody watches and the
    // room lets the share rest: one a monitor's capture held from before
    // goes out only after a look at the desktop (Capture::rested).
    pub fn rested(&mut self) {
        if let Some(opened) = &mut self.opened {
            opened.source.rested();
        }
    }

    // Under 8 Mbit/s a viewer, 1440p120 is not worth its frames, and an
    // encoder that cannot keep up needs fewer pixels. The source and the
    // encoder open again at 1080p60's size and rate, the shape kept as for
    // the software encoder, and the next frame is an IDR. Frame numbers go
    // on. False when the share is that small already, as one on the
    // software encoder always is. An error ends the share.
    pub fn step_down(&mut self, say: &mut dyn FnMut(Line)) -> Result<bool, String> {
        let Some(fit) = self.small_fit() else {
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
            .map_err(String::from)
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
        self.reopen(options, say).map_err(String::from)
    }

    // The largest picture and the most frames a second every watcher takes,
    // from the host. A box past capture's default only counts while the
    // share is in HEVC, and source::open keeps H.264 to that default. True
    // when the source opened again for it.
    pub fn set_limit(
        &mut self,
        limit: capture::Options,
        say: &mut dyn FnMut(Line),
    ) -> Result<bool, String> {
        if self.limit == Some(limit) {
            return Ok(false);
        }
        self.limit = Some(limit);
        self.reopen(self.source_options(self.setup.fps), say)
            .map_err(String::from)
    }

    pub fn stepped_down(&self) -> bool {
        self.small.is_some()
    }

    // What 1080p60 makes of this share, None when it is that small already:
    // its size with the shape kept, or for a window, which changes size,
    // the box of 1080p60 (source::window_box).
    fn small_fit(&self) -> Option<Fit> {
        match self.setup.choice {
            Choice::Window { .. } => {
                let small = source::window_box(self.fps);
                let fits = self.size.0 <= small.max_width
                    && self.size.1 <= small.max_height
                    && self.fps <= small.max_fps;
                (!fits).then_some(Fit {
                    width: small.max_width,
                    height: small.max_height,
                    fps: small.max_fps,
                })
            }
            _ => encode::software_fit(self.size.0, self.size.1, self.fps),
        }
    }

    // None while paused by hand or resumed while its source would not open
    // again, and after a failed reopen, when the share is over.
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
        self.source_options_in(self.codec(), fps)
    }

    // The same for a share in `codec`.
    fn source_options_in(&self, codec: Option<Codec>, fps: u32) -> capture::Options {
        let largest = match codec {
            Some(Codec::Hevc) => self.limit,
            _ => self.limit.map(within_default),
        };
        let software_window = self.software_window().then(|| {
            let small = source::window_box(fps);
            (small.max_width, small.max_height)
        });
        options_for(
            fps,
            self.small
                .or(self.software.map(|software| software.limits))
                .or(software_window),
            largest,
        )
    }

    // A window share whose encoder is Windows' software one, however it came
    // to be, which keeps to 1080p60's box whatever size the window takes
    // (source::window_box).
    fn software_window(&self) -> bool {
        matches!(self.setup.choice, Choice::Window { .. }) && self.kind() == Some(Kind::MfSoftware)
    }

    // After a new encoder for a window: on the software encoder, the live
    // capture keeps to its box from now on.
    fn box_software_window(&mut self) {
        if !self.software_window() {
            return;
        }
        let small = source::window_box(self.fps);
        if let Some(opened) = &mut self.opened {
            let options = capture::Options {
                max_fps: opened.options.max_fps,
                ..small
            };
            opened.source.set_limits(options);
            opened.options = options;
            self.options = options;
        }
    }

    // True when the source opened again, which it does only for a picture
    // or a rate other than the one it has: on the software encoder a step or
    // a new rate can come to what it runs at already, and a 4K box changes
    // nothing for a monitor 1440 high but the next mode change.
    fn reopen(
        &mut self,
        options: capture::Options,
        say: &mut dyn FnMut(Line),
    ) -> Result<bool, Unopened> {
        if self.keeps(options) {
            self.take_options(options);
            return Ok(false);
        }
        let now = Instant::now();
        let current = self.codec().or(self.held_in);
        let pick = self.pick_in(current, now);
        // HEVC can come back with this encoder, and a watchers' limit past
        // capture's default counts in it. If H.264 opens after all, source::open
        // brings the box in again.
        let options = if current == Some(Codec::H264)
            && pick.codec.is_none()
            && pick.takes_hevc
            && options == self.source_options_in(current, options.max_fps)
        {
            self.source_options_in(Some(Codec::Hevc), options.max_fps)
        } else {
            options
        };
        // Windows allows one duplication of a monitor per process, so the
        // old one goes before the new one opens, and the last picture with
        // it, since it is on the old one's texture.
        self.last = None;
        self.keep_window_size();
        self.opened = None;
        self.reopens += 1;
        let mut opened = source::open(
            &self.setup.choice,
            self.encoder_kind(),
            pick,
            &self.setup.settings,
            options,
            self.hevc_offer.clone(),
            self.window_size,
            say,
        )?;
        self.held_in = None;
        if current.is_some_and(|was| was != opened.encoder.codec()) {
            self.switches.changed(now);
            self.numbers.codec_changes += 1;
        }
        if let Some(offer) = opened.hevc.take() {
            self.hevc_offer = Some(offer);
        }
        self.size = opened.source.size();
        self.fps = opened.fps;
        self.interval = Duration::from_secs(1) / self.fps;
        self.notes_said = opened.encoder.notes().len();
        self.options = opened.options;
        opened.source.set_frame(Some(self.size));
        self.opened = Some(opened);
        self.encoder_bitrate = self.setup.settings.bitrate;
        self.last_encoder = Some(Instant::now());
        // A new source and encoder at the window's own size end any row of
        // failed resizes.
        self.resizes_failed = 0;
        self.resize_retry = None;
        self.box_software_window();
        Ok(true)
    }

    // Whether the source in use gives the same picture at the same rate
    // under `options`. A window's frame is the share's own, which a box
    // keeps as long as the frame fits in it: a bigger size the window has
    // under the new box then comes as any new size of the window does.
    fn keeps(&self, options: capture::Options) -> bool {
        let Some(opened) = &self.opened else {
            return false;
        };
        match self.setup.choice {
            Choice::Window { .. } => {
                let fits = |max: u32, side: u32| {
                    max == 0 || (max & !1).max(capture::SMALLEST_SIDE) >= side
                };
                options.max_fps == self.options.max_fps
                    && fits(options.max_width, self.size.0)
                    && fits(options.max_height, self.size.1)
            }
            _ => same_picture(opened.source.plan(), self.options.max_fps, options),
        }
    }

    // `options` for the source in use, which keeps its picture under them.
    fn take_options(&mut self, options: capture::Options) {
        if let Some(opened) = &mut self.opened {
            opened.source.set_limits(options);
            opened.options = options;
        }
        self.options = options;
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

    // The same for a new encoder that takes over from one in `current`: back
    // to HEVC only once SWITCH_GAP allows it. A viewer who needs H.264 gets
    // it with this encoder, gap or no gap.
    fn pick_in(&self, current: Option<Codec>, now: Instant) -> Pick {
        let mut pick = self.pick();
        if current == Some(Codec::H264) && !self.switches.hevc_may(now) {
            pick.takes_hevc = false;
        }
        pick
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
            return Err(why);
        }
        Ok(())
    }

    fn take(
        &mut self,
        asked: Instant,
        rested: bool,
        audience: &mut dyn Audience,
    ) -> Result<(), String> {
        if self.away.is_some() && !self.open_away(audience)? {
            return Ok(());
        }
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
                if let Some(cursor) = frame.cursor.take() {
                    audience.cursor(cursor, true);
                }
                // A test's picture that never goes out after its pointer, as
                // one too big to send does.
                #[cfg(feature = "fault")]
                if !crate::fault::picture_goes() {
                    return Ok(());
                }
                // Decided before this picture goes out fitted, so it goes
                // once, to the new encoder, at the window's own size.
                if let Some(mut own) = self.window_resize(Instant::now())? {
                    own.present = frame.present;
                    if self.go(own, audience)? {
                        return Ok(());
                    }
                    if let Some(cursor) = self.again_dropped()? {
                        audience.cursor(cursor, true);
                    }
                }
                self.go(frame, audience)?;
            }
            Next::Cursor(update) => {
                audience.cursor(update, false);
                self.quiet(audience)?;
            }
            Next::Idle => self.quiet(audience)?,
            Next::Paused(reason) => {
                self.paused = true;
                // Locked, on a UAC prompt or disconnected, the PC may be left
                // alone: nothing from before goes out meanwhile, not even to
                // whoever starts watching, as after a pause by hand, and the
                // first picture after it is an IDR, which also stands in for
                // the frames cut off on their way. Which reason a monitor's
                // capture gives first is up to Windows: a display still
                // changing can hide the lock, and a monitor that left the
                // desktop can be told just before it. Only a minimized window
                // keeps its picture: its share looks at the desktop itself.
                if !matches!(reason, PauseReason::Minimized) {
                    self.forget_pictures();
                    self.answers.force_idr();
                }
                audience.paused(&reason);
            }
        }
        Ok(())
    }

    // A source that did not open again at resume(): the audience hears why,
    // as for capture's own pause, and again when the reason changes in
    // kind. It is tried again once a capture::LOOK_EVERY, first if that
    // long went by since the last try, as after a rest, so a reason that
    // no longer holds is not told. Between tries the desktop is looked at,
    // so the lock screen stops the PC sound as soon as it would for a
    // running capture, and what came back is answered, as still() does
    // during capture's pause, so the share can rest once nobody watches. A
    // monitor still not listed capture::LOOKS tries after the first ends
    // the share, as it would while it runs. True once it opened.
    fn open_away(&mut self, audience: &mut dyn Audience) -> Result<bool, String> {
        let Some(mut away) = self.away.take() else {
            return Ok(true);
        };
        if away.told {
            std::thread::sleep(AWAY_WAIT);
        }
        let now = Instant::now();
        let reason = if now.saturating_duration_since(away.tried) >= capture::LOOK_EVERY {
            away.tried = now;
            match self.reopen(self.options, &mut |line| audience.line(line)) {
                Ok(_) => return Ok(true),
                Err(Unopened {
                    away: Some(reason),
                    gone,
                    why,
                }) => {
                    if !away.refused(gone) {
                        return Err(why);
                    }
                    reason
                }
                Err(unopened) => return Err(unopened.why),
            }
        } else {
            // As capture's look for a monitor that is not listed: one that
            // could not ask counts as a lost session.
            match capture::desktop_away() {
                None => away.reason.clone(),
                Some(PauseReason::Changing(_)) => PauseReason::Disconnected,
                Some(reason) => reason,
            }
        };
        if !std::mem::replace(&mut away.told, true)
            || std::mem::discriminant(&reason) != std::mem::discriminant(&away.reason)
        {
            audience.paused(&reason);
        }
        away.reason = reason;
        self.away = Some(away);
        self.answer(audience);
        Ok(false)
    }

    // A picture from capture out to the viewers, through a new encoder
    // first when its size is new. False when that encoder did not open for
    // a window, which keeps the one it has, or when a monitor's new mode
    // needs the software encoder's fit: nothing went out.
    fn go(&mut self, mut frame: Frame, audience: &mut dyn Audience) -> Result<bool, String> {
        if (frame.width, frame.height) != self.size && !self.new_size(&frame, audience)? {
            return Ok(false);
        }
        let reopens = self.reopens;
        self.answer(audience);
        // A change of codec opened the source again, at a new size: this
        // frame is the old one's, and the next is the new one's.
        if self.reopens != reopens {
            return Ok(true);
        }
        // Only a window's picture made again at its own size (window_resize)
        // still has its pointer here, which goes only with that picture.
        if let Some(cursor) = frame.cursor.take() {
            audience.cursor(cursor, true);
        }
        self.send(&frame, None, audience)?;
        if self.reopens == reopens {
            self.last = Some(frame);
        }
        Ok(true)
    }

    // No new picture: a window that stood still at a size it went out
    // fitted at goes out at its own size once it may, else as still().
    fn quiet(&mut self, audience: &mut dyn Audience) -> Result<(), String> {
        if let Some(own) = self.window_resize(Instant::now())? {
            if self.go(own, audience)? {
                return Ok(());
            }
            if let Some(cursor) = self.again_dropped()? {
                audience.cursor(cursor, false);
            }
        }
        self.still(audience)
    }

    // For a window share, its newest picture at the window's own size when
    // that is not the share's and a new encoder may open now
    // (RESIZE_SPACING); until then its pictures go out fitted into the
    // share's frame. With no picture kept, the next one comes fitted and
    // kept, and is decided then. None for any other source.
    fn window_resize(&mut self, now: Instant) -> Result<Option<Frame>, String> {
        let may = self.resizes_failed < RESIZE_TRIES
            && self.resize_retry.is_none_or(|at| now >= at)
            && self
                .last_encoder
                .is_none_or(|at| now.saturating_duration_since(at) >= RESIZE_SPACING)
            && !self.answers.within_floor(now);
        let size = self.size;
        let Some(opened) = &mut self.opened else {
            return Ok(None);
        };
        let Some(natural) = opened.source.natural() else {
            return Ok(None);
        };
        if natural == size || !may {
            opened.source.set_frame(Some(size));
            return Ok(None);
        }
        opened.source.set_frame(None);
        let own = opened.source.again()?;
        if own.is_none() {
            opened.source.set_frame(Some(size));
        }
        Ok(own)
    }

    // The picture from window_resize did not go out: capture goes back to
    // the share's frame, with the pointer's update for it, as the one made
    // for that picture was dropped with it (WindowCapture::again_dropped).
    fn again_dropped(&mut self) -> Result<Option<CursorUpdate>, String> {
        match &mut self.opened {
            Some(opened) => opened.source.again_dropped(),
            None => Ok(None),
        }
    }

    // A new encoder for a window's new size did not open: the window goes on
    // fitted into the frame it has, and the size is tried again after the
    // spacing, RESIZE_TRIES times in a row at most.
    fn resize_failed(&mut self, size: (u32, u32), why: &str, audience: &mut dyn Audience) {
        self.resizes_failed += 1;
        self.resize_retry = Some(Instant::now() + RESIZE_SPACING);
        let frame = self.size;
        if let Some(opened) = &mut self.opened {
            opened.source.set_frame(Some(frame));
        }
        audience.line(Line::Log(format!(
            "no new encoder for the window at {}x{}: {why}; it goes on fitted into {}x{}",
            size.0, size.1, frame.0, frame.1
        )));
        if self.resizes_failed == RESIZE_TRIES {
            audience.line(Line::Say(format!(
                "no encoder opened for the window's new size {RESIZE_TRIES} times in a row, so the share stays at {}x{} with the window fitted into it",
                frame.0, frame.1
            )));
        }
    }

    // Capture is paused: no picture comes until it gives one again.
    pub fn paused(&self) -> bool {
        self.paused
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

    // A mode change on the shared monitor, or a shared window's new size: a
    // new encoder, whose first frame is an IDR, as a new share would start.
    // False when it did not open for a window, which keeps its encoder, or
    // when a monitor's new mode is past what the software encoder takes: its
    // pictures go on at the fit from the next one, and this frame does not
    // go out.
    fn new_size(&mut self, frame: &Frame, audience: &mut dyn Audience) -> Result<bool, String> {
        let size = (frame.width, frame.height);
        let now = Instant::now();
        let window = matches!(self.setup.choice, Choice::Window { .. });
        let current = self.codec();
        let pick = self.pick_in(current, now);
        let kind = self.encoder_kind();
        let Some(opened) = &mut self.opened else {
            return Ok(false);
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
        // H.264 keeps to capture's default box (source::open): a window past
        // it goes on fitted while capture brings its pictures within it.
        let default = capture::Options::default();
        let past_default = size.0 > default.max_width || size.1 > default.max_height;
        if window && past_default && chosen.codec == Codec::H264 {
            self.keep_to_h264_box();
            return Ok(false);
        }
        audience.line(Line::Say(format!(
            "the picture went from {}x{} to {}x{}: starting a new encoder",
            self.size.0, self.size.1, size.0, size.1
        )));
        let settings = &self.setup.settings;
        let fps = self.fps;
        let encoder = opened
            .source
            .encoder(chosen.kind, chosen.codec, size, fps, settings);
        let encoder = match encoder {
            Ok(encoder) => Ok(encoder),
            Err(err) if chosen.codec == Codec::Hevc && self.setup.codec.is_none() => {
                audience.line(Line::Log(format!(
                    "HEVC did not open at {}x{}: {err}; going on in H.264",
                    size.0, size.1
                )));
                if let Some(offer) = &mut self.hevc_offer {
                    offer.kind = None;
                    offer.why_not = err.to_string();
                }
                if window && past_default {
                    self.keep_to_h264_box();
                    return Ok(false);
                }
                opened
                    .source
                    .encoder(self.setup.encoder, Codec::H264, size, fps, settings)
            }
            Err(err) => Err(err),
        };
        match encoder {
            Ok(encoder) => opened.encoder = encoder,
            Err(why) if window => {
                self.resize_failed(size, &why.to_string(), audience);
                return Ok(false);
            }
            Err(
                EncodeError::NoEncoderForGpu { fit: Some(fit), .. }
                | EncodeError::SoftwareLimit { fit, .. },
            ) => {
                audience.line(source::runs_smaller(fit, size, fps));
                let options = capture::Options {
                    max_width: fit.width,
                    max_height: fit.height,
                    max_fps: fit.fps,
                };
                // Capture brings its next picture within the box by itself. A
                // source opened now can be refused while the display is still
                // changing, which would end the share, so only a lower rate
                // opens one: capture's fps cap is set as it opens.
                if fit.fps == fps {
                    self.take_options(options);
                } else {
                    self.reopen(options, &mut |line| audience.line(line))?;
                }
                if let Some(software) = &mut self.software {
                    software.limits = (fit.width, fit.height);
                }
                return Ok(false);
            }
            Err(why) => return Err(why.to_string()),
        }
        let codec_changed = current.is_some_and(|was| was != opened.encoder.codec());
        if codec_changed {
            self.switches.changed(now);
        }
        self.size = size;
        self.resizes_failed = 0;
        self.numbers.resized += u64::from(window);
        self.changed_encoder(codec_changed);
        self.box_software_window();
        Ok(true)
    }

    // A window that would need H.264 past capture's default box: capture
    // takes H.264's box, and the window goes on fitted into the frame it has
    // until its next picture comes within the box.
    fn keep_to_h264_box(&mut self) {
        let options = self.source_options_in(Some(Codec::H264), self.setup.fps);
        self.take_options(options);
        let size = self.size;
        if let Some(opened) = &mut self.opened {
            opened.source.set_frame(Some(size));
        }
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
        // A window's encoders keep apart (RESIZE_SPACING), and the change
        // back to HEVC can wait for that as it waits for the floor.
        let spaced = !matches!(self.setup.choice, Choice::Window { .. })
            || self
                .last_encoder
                .is_none_or(|at| now.saturating_duration_since(at) >= RESIZE_SPACING);
        if codec == Codec::Hevc && (self.answers.within_floor(now) || !spaced) {
            return;
        }
        // A watchers' limit past capture's default counts only in HEVC, so
        // under one the change of codec can also be a change of size: the
        // source then opens again for it, and takes the codec from
        // takes_hevc.
        let options = self.source_options_in(Some(codec), self.setup.fps);
        // Not while capture is paused: a monitor's source does not open
        // while the desktop is away, and that would end the share. Such a
        // change waits for the first picture back.
        if self.paused && !self.keeps(options) {
            return;
        }
        let kind = match codec {
            Codec::H264 => self.setup.encoder,
            Codec::Hevc => match self.offered_hevc(audience) {
                Some(kind) => Some(kind),
                None => return,
            },
        };
        let was = self.encoder_name().to_string();
        let changed = if self.keeps(options) {
            self.switch(codec, kind)
                .map(|()| self.take_options(options))
        } else {
            self.reopen(options, &mut |line| audience.line(line))
                .map(|_| ())
                .map_err(String::from)
        };
        // After it: a reopen asks whether HEVC may come back, and due has
        // just said it may.
        self.switches.changed(now);
        match changed {
            Ok(()) => audience.line(Line::Log(format!(
                "{was} to {} in {:.1} ms: {}",
                self.encoder_name(),
                ms(now.elapsed()),
                match codec {
                    Codec::H264 => "a viewer does not decode HEVC",
                    Codec::Hevc => "every viewer decodes HEVC",
                }
            ))),
            // The source did not open again: the next frame ends the share.
            Err(why) if self.opened.is_none() => {
                audience.line(Line::Log(format!("{was} to {codec} at a new size: {why}")))
            }
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
        self.last_encoder = Some(Instant::now());
        // A window's pictures go out in the new encoder's frame from now on.
        let size = self.size;
        if let Some(opened) = &mut self.opened {
            opened.source.set_frame(Some(size));
        }
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
            mut source,
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
        let fit = self.small_fit();
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
                // A window that fits now must still fit when it grows.
                let options = match self.setup.choice {
                    Choice::Window { .. } => {
                        let small = source::window_box(self.fps);
                        source.set_limits(small);
                        capture::Options {
                            max_fps: options.max_fps,
                            ..small
                        }
                    }
                    _ => options,
                };
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
                self.window_size = source.content().or(self.window_size);
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
                let last = self.window_size;
                let source = Source::open(&self.setup.choice, options, last).map_err(|why| {
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
        // The GPU encoder whose opens may have failed is gone.
        self.resizes_failed = 0;
        self.resize_retry = None;
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

    // Whether a new encoder may bring HEVC back now, as due() lets it: no
    // change and no viewer needing H.264 for a whole gap.
    pub(crate) fn hevc_may(&self, now: Instant) -> bool {
        let quiet = |at: Option<Instant>| {
            at.is_none_or(|at| now.saturating_duration_since(at) >= SWITCH_GAP)
        };
        quiet(self.last) && quiet(self.needed_h264)
    }
}

// Capture's options at `fps`: within `small` (step_down's size, or what
// the software encoder took) at no more than its rate, and within the
// watchers' limit; with neither, the limit's box or capture's default.
fn options_for(
    fps: u32,
    small: Option<(u32, u32)>,
    limit: Option<capture::Options>,
) -> capture::Options {
    let options = match small {
        Some((max_width, max_height)) => capture::Options {
            max_width,
            max_height,
            max_fps: fps.min(SMALL_FPS),
        },
        None => capture::Options {
            max_fps: fps,
            ..limit.unwrap_or_default()
        },
    };
    match limit {
        Some(limit) => capture::Options {
            max_width: options.max_width.min(limit.max_width),
            max_height: options.max_height.min(limit.max_height),
            max_fps: options.max_fps.min(limit.max_fps),
        },
        None => options,
    }
}

// Whether capture asked for `options` makes the picture `plan` describes
// at the rate `fps`, from the same source: the same monitor, region or
// window.
fn same_picture(plan: capture::Plan, fps: u32, options: capture::Options) -> bool {
    options.max_fps == fps && plan.refit(options.max_width, options.max_height) == plan
}

// A limit with its box brought down to capture's default, which is all
// H.264 is sent at.
fn within_default(limit: capture::Options) -> capture::Options {
    let default = capture::Options::default();
    capture::Options {
        max_width: limit.max_width.min(default.max_width),
        max_height: limit.max_height.min(default.max_height),
        ..limit
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::*;
    use crate::PAYLOAD_LAN;

    const NVIDIA: u32 = 0x10de;
    const INTEL: u32 = 0x8086;

    #[derive(Default)]
    struct Watching {
        takes_hevc: bool,
        sent: Vec<Sent>,
        lines: Vec<Line>,
        paused: Vec<PauseReason>,
        // How many times what came back was asked for.
        asked: usize,
    }

    impl Audience for Watching {
        fn cursor(&mut self, _: CursorUpdate, _: bool) {}

        fn back(&mut self, _: &mut Vec<Back>) {
            self.asked += 1;
        }

        fn takes_hevc(&mut self) -> bool {
            self.takes_hevc
        }

        fn sent(&mut self, frame: &Sent) {
            self.sent.push(*frame);
        }

        fn line(&mut self, line: Line) {
            self.lines.push(line);
        }

        fn paused(&mut self, reason: &PauseReason) {
            self.paused.push(reason.clone());
        }
    }

    // One share at a time on the GPU.
    static TURN: Mutex<()> = Mutex::new(());

    fn gpu() -> Option<(MutexGuard<'static, ()>, capture::Adapter)> {
        let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        let adapters = capture::adapters().unwrap_or_else(|err| panic!("{err}"));
        let Some(adapter) = adapters
            .iter()
            .find(|adapter| adapter.vendor_id == NVIDIA)
            .or(adapters.first())
        else {
            println!("skipped: this PC has no graphics card to make the pattern on");
            return None;
        };
        Some((turn, adapter.clone()))
    }

    // A share of the pattern, left to pick its codec, whose viewers take HEVC.
    fn pattern_share(adapter: capture::Adapter, size: (u32, u32), encoder: Option<Kind>) -> Sharer {
        let setup = Setup {
            choice: Choice::Pattern {
                adapter,
                width: size.0,
                height: size.1,
                busy: false,
            },
            fps: 60,
            settings: Settings::default(),
            encoder,
            codec: None,
            takes_hevc: true,
            payload: PAYLOAD_LAN,
            spread: false,
            clock: Clock::starting(Instant::now()),
            keep_times: false,
        };
        Sharer::open(setup, |_: &[u8]| {}, &mut |_| {}).unwrap_or_else(|err| panic!("{err}"))
    }

    // What a monitor's capture does at a new mode, which a test cannot bring
    // about: the source goes on under the same limits, its next picture at
    // the new size, and a source opened again sees the new size too. The
    // pattern takes no new box while it runs, so called again, this is the
    // capture bringing its next picture within the box it was just given.
    fn new_mode(sharer: &mut Sharer, width: u32, height: u32) {
        let Choice::Pattern { adapter, busy, .. } = sharer.setup.choice.clone() else {
            panic!("not a pattern");
        };
        let choice = Choice::Pattern {
            adapter,
            width,
            height,
            busy,
        };
        sharer.setup.choice = choice;
        let opened = sharer.opened.as_mut().expect("a source");
        let Source::Pattern(pattern) = &opened.source else {
            panic!("not a pattern");
        };
        let pattern = capture::Pattern::with_source(
            pattern.device(),
            width,
            height,
            capture::Rotation::Identity,
            sharer.options,
        )
        .unwrap_or_else(|err| panic!("{err}"));
        opened.source = Source::Pattern(Box::new(pattern));
    }

    // A monitor share that fitted the software encoder, whose mode then grows
    // past it: capture takes the fit with no source opened, which a display
    // still changing could refuse, the frame at the new size does not go out,
    // and the next one goes on the encoder it has, the frame numbers going on.
    #[test]
    fn a_monitor_grown_past_the_software_encoder_goes_on_at_its_fit() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let intel = capture::Adapter {
            vendor_id: INTEL,
            ..adapter.clone()
        };
        for (adapter, encoder, software) in [
            (intel, None, Some(Software::Intel)),
            (adapter.clone(), Some(Kind::MfSoftware), None),
        ] {
            let mut sharer = pattern_share(adapter, (1920, 1080), encoder);
            assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
            assert_eq!(sharer.software(), software);
            let mut watching = Watching::default();
            sharer
                .next(&mut watching)
                .unwrap_or_else(|err| panic!("{err}"));
            assert_eq!(watching.sent.len(), 1);
            new_mode(&mut sharer, 2560, 1440);
            sharer
                .next(&mut watching)
                .unwrap_or_else(|err| panic!("{err}"));
            assert_eq!(watching.sent.len(), 1);
            assert_eq!(sharer.reopens, 0);
            assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
            assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
            assert!(
                watching.lines.contains(&Line::Say(String::from(
                    "the software encoder takes up to 1080p at 60 fps, so this share runs at 1920x1080 and 60 fps instead of 2560x1440 and 60"
                ))),
                "{:?}",
                watching.lines
            );
            new_mode(&mut sharer, 2560, 1440);
            sharer
                .next(&mut watching)
                .unwrap_or_else(|err| panic!("{err}"));
            assert_eq!(watching.sent.len(), 2);
            assert!(!watching.sent[1].idr);
            assert_eq!(watching.sent[1].number, 1);
            assert_eq!(sharer.reopens, 0);
            // Whatever opens the source next keeps to the fit.
            if software.is_some() {
                let options = sharer.source_options(60);
                assert_eq!((options.max_width, options.max_height), (1920, 1080));
            }
        }

        // A mode of another shape fits at another size, whose new encoder
        // starts with an IDR.
        let mut sharer = pattern_share(adapter, (1920, 1080), Some(Kind::MfSoftware));
        let mut watching = Watching::default();
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        new_mode(&mut sharer, 2560, 1600);
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(watching.sent.len(), 1);
        new_mode(&mut sharer, 2560, 1600);
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(watching.sent.len(), 2, "{:?}", watching.lines);
        assert!(watching.sent[1].idr);
        assert_eq!(watching.sent[1].number, 1);
        assert_eq!(sharer.reopens, 0);
        assert_eq!(sharer.size(), (1818, 1136));
    }

    // No GPU encoder takes a monitor's new mode, as when another program holds
    // all of NVENC's sessions, and the software encoder only takes it smaller.
    // At the rate it has, the share takes the fit with no source opened, and
    // the encoder it has goes on. Faster than the software encoder takes, the
    // source opens again for the rate and lands on the software encoder.
    #[test]
    fn a_monitor_mode_no_gpu_encoder_takes_goes_on_at_the_fit() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let mut sharer = pattern_share(adapter.clone(), (1920, 1080), None);
        let kind = sharer.kind();
        if kind == Some(Kind::MfSoftware) {
            println!("skipped: no GPU encoder here to refuse");
            return;
        }
        let mut watching = Watching::default();
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        encode::fault::refuse_gpu(0);
        new_mode(&mut sharer, 2560, 1440);
        let refused = sharer.next(&mut watching);
        encode::fault::clear();
        refused.unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(watching.sent.len(), 1, "{:?}", watching.lines);
        assert_eq!(sharer.reopens, 0);
        assert_eq!(
            (sharer.options.max_width, sharer.options.max_height),
            (1920, 1080)
        );
        new_mode(&mut sharer, 2560, 1440);
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(watching.sent.len(), 2);
        assert!(!watching.sent[1].idr);
        assert_eq!(sharer.kind(), kind);
        drop(sharer);

        let mut sharer = pattern_share(adapter, (1280, 720), None);
        sharer
            .set_fps(120, &mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        if sharer.fps() != 120 {
            println!("skipped: no GPU encoder here takes 1280x720 and 120 fps");
            return;
        }
        let reopens = sharer.reopens;
        let mut watching = Watching::default();
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        encode::fault::refuse_gpu(0);
        new_mode(&mut sharer, 2560, 1440);
        let refused = sharer.next(&mut watching);
        encode::fault::clear();
        refused.unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(watching.sent.len(), 1, "{:?}", watching.lines);
        assert_eq!(sharer.reopens, reopens + 1);
        assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
        assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
        assert_eq!(sharer.software(), None);
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(watching.sent.len(), 2);
        assert!(watching.sent[1].idr);
        assert_eq!(watching.sent[1].number, 1);
    }

    // While capture is paused, a change of codec that would open the source
    // again waits, since a monitor's source does not open while the desktop
    // is away, and it comes with the first picture back. One that keeps the
    // source goes at once.
    #[test]
    fn a_codec_change_that_opens_the_source_waits_out_a_pause() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        if adapter.vendor_id != NVIDIA {
            println!("skipped: no NVIDIA GPU, so no NVENC to share 4K in HEVC with");
            return;
        }
        let mut sharer = pattern_share(adapter.clone(), (3840, 2160), None);
        let four_k = capture::Options {
            max_width: 4096,
            max_height: 2160,
            max_fps: 60,
        };
        sharer
            .set_limit(four_k, &mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            (sharer.size(), sharer.codec()),
            ((3840, 2160), Some(Codec::Hevc))
        );
        let reopens = sharer.reopens;
        sharer.paused = true;
        // Someone who does not take HEVC starts watching, and capture says
        // nothing is new.
        let mut watching = Watching::default();
        sharer
            .quiet(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(sharer.reopens, reopens, "{:?}", watching.lines);
        assert_eq!(
            (sharer.size(), sharer.codec()),
            ((3840, 2160), Some(Codec::Hevc))
        );
        sharer.paused = false;
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(sharer.reopens, reopens + 1);
        assert_eq!(
            (sharer.size(), sharer.codec()),
            ((2560, 1440), Some(Codec::H264))
        );

        // Now in H.264 under the limit, with HEVC due, which takes the box
        // back to 4K.
        watching.takes_hevc = true;
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        std::thread::sleep(SWITCH_GAP);
        let reopens = sharer.reopens;
        sharer.paused = true;
        sharer
            .quiet(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(sharer.reopens, reopens, "{:?}", watching.lines);
        assert_eq!(sharer.codec(), Some(Codec::H264));
        sharer.paused = false;
        let until = Instant::now() + SWITCH_GAP * 2;
        while sharer.codec() != Some(Codec::Hevc) {
            assert!(Instant::now() < until, "{:?}", watching.lines);
            sharer
                .next(&mut watching)
                .unwrap_or_else(|err| panic!("{err}"));
        }
        assert_eq!(sharer.size(), (3840, 2160));

        let mut sharer = pattern_share(adapter, (2560, 1440), None);
        assert_eq!(sharer.codec(), Some(Codec::Hevc));
        watching.takes_hevc = false;
        sharer.paused = true;
        sharer
            .quiet(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(sharer.reopens, 0);
        assert_eq!(sharer.codec(), Some(Codec::H264));
    }

    // HEVC coming back with a new encoder that opened for another reason, once
    // the gap is over and before pick_codec has changed back, brings the 4K
    // limit as pick_codec's change does.
    #[test]
    fn a_new_encoder_that_brings_hevc_back_brings_the_4k_limit() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        if adapter.vendor_id != NVIDIA {
            println!("skipped: no NVIDIA GPU, so no NVENC to share 4K in HEVC with");
            return;
        }
        let mut sharer = pattern_share(adapter, (3840, 2160), None);
        let four_k = capture::Options {
            max_width: 4096,
            max_height: 2160,
            max_fps: 60,
        };
        sharer
            .set_limit(four_k, &mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        let mut watching = Watching::default();
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            (sharer.size(), sharer.codec()),
            ((2560, 1440), Some(Codec::H264))
        );
        watching.takes_hevc = true;
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        sharer.pause();
        std::thread::sleep(SWITCH_GAP);
        sharer
            .resume(&mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            (sharer.size(), sharer.codec()),
            ((3840, 2160), Some(Codec::Hevc))
        );
        assert_eq!(sharer.numbers().codec_changes, 2);
    }

    // A change of codec that came with a new encoder for a new rate is a
    // change like any other: it counts, and starts a SWITCH_GAP.
    #[test]
    fn a_reopen_that_changes_codec_starts_the_gap() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let mut sharer = pattern_share(adapter, (2560, 1440), None);
        if sharer.codec() != Some(Codec::Hevc) {
            println!("skipped: no encoder here takes HEVC at 2560x1440 and 60 fps");
            return;
        }
        sharer.setup.takes_hevc = false;
        let opened = sharer
            .set_fps(30, &mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        assert!(opened);
        assert_eq!(sharer.codec(), Some(Codec::H264));
        assert_eq!(sharer.numbers().codec_changes, 1);
        assert!(!sharer.switches.hevc_may(Instant::now()));
    }

    // A monitor's new mode inside a SWITCH_GAP of the change to H.264 keeps
    // H.264, as a window's new size does.
    #[test]
    fn a_new_mode_inside_the_gap_keeps_h264() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let mut sharer = pattern_share(adapter, (1920, 1080), None);
        if sharer.codec() != Some(Codec::Hevc) {
            println!("skipped: no encoder here takes HEVC at 1920x1080 and 60 fps");
            return;
        }
        let mut watching = Watching::default();
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(sharer.codec(), Some(Codec::H264));
        watching.takes_hevc = true;
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        new_mode(&mut sharer, 2560, 1440);
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            (sharer.size(), sharer.codec()),
            ((2560, 1440), Some(Codec::H264)),
            "{:?}",
            watching.lines
        );
    }

    // The watchers' limit sets the box when nothing smaller does, and each
    // side and the rate are the smaller of the two.
    #[test]
    fn capture_keeps_to_the_watchers_limit() {
        let default = capture::Options::default();
        let four_k = capture::Options {
            max_width: 4096,
            max_height: 2160,
            max_fps: 120,
        };
        let capped = capture::Options {
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
        };
        let options = |max_width, max_height, max_fps| capture::Options {
            max_width,
            max_height,
            max_fps,
        };
        assert_eq!(options_for(144, None, None), options(4096, 1440, 144));
        assert_eq!(options_for(144, None, Some(four_k)), four_k);
        assert_eq!(options_for(120, None, Some(capped)), capped);
        assert_eq!(
            options_for(120, Some((1920, 1080)), Some(four_k)),
            options(1920, 1080, 60)
        );
        assert_eq!(options_for(120, Some((2560, 1440)), Some(capped)), capped);
        assert_eq!(within_default(four_k), options(4096, 1440, 120));
        assert_eq!(within_default(capped), capped);
        assert_eq!(within_default(default), default);
    }

    // A 4K box leaves a 1440p monitor's picture as it is and brings a 4K
    // one to its own size; a new rate or a smaller box is a new picture.
    #[test]
    fn a_new_box_is_a_new_picture_only_when_the_picture_changes() {
        let default = capture::Options::default();
        let four_k = capture::Options {
            max_width: 4096,
            max_height: 2160,
            max_fps: 120,
        };
        let plan = |width, height, rotation| {
            capture::Plan::new(
                width,
                height,
                rotation,
                default.max_width,
                default.max_height,
            )
        };
        let monitor_1440 = plan(2560, 1440, capture::Rotation::Identity);
        assert!(same_picture(monitor_1440, 120, default));
        assert!(same_picture(monitor_1440, 120, four_k));
        assert!(!same_picture(monitor_1440, 60, four_k));
        assert!(!same_picture(
            monitor_1440,
            120,
            capture::Options {
                max_width: 1920,
                max_height: 1080,
                max_fps: 120,
            }
        ));
        let monitor_4k = plan(3840, 2160, capture::Rotation::Identity);
        assert_eq!((monitor_4k.width, monitor_4k.height), (2560, 1440));
        assert!(same_picture(monitor_4k, 120, default));
        assert!(!same_picture(monitor_4k, 120, four_k));
        // Turned on its side, 1440 wide and 2560 high, it is scaled down
        // in either box.
        let portrait = plan(2560, 1440, capture::Rotation::Rotate90);
        assert_eq!((portrait.width, portrait.height), (810, 1440));
        assert!(!same_picture(portrait, 120, four_k));
        // A region kept as it is in every box keeps its capture; one the
        // 1080 box scales down, or narrows under 256, does not.
        let short = capture::Options {
            max_width: 4096,
            max_height: 1080,
            max_fps: 120,
        };
        let region = |width, height| {
            let region = capture::Region {
                left: 100,
                top: 0,
                width,
                height,
            };
            capture::Plan::region(
                2560,
                1440,
                capture::Rotation::Identity,
                region,
                default.max_width,
                default.max_height,
            )
            .unwrap()
        };
        assert!(same_picture(region(1280, 720), 120, four_k));
        assert!(same_picture(region(1280, 720), 120, short));
        assert!(!same_picture(region(2000, 1200), 120, short));
        assert!(!same_picture(region(256, 1440), 120, short));
        // A window under 256 a side is padded the same way in every box.
        let window = plan(300, 200, capture::Rotation::Identity);
        assert!(same_picture(window, 120, four_k));
        assert!(same_picture(window, 120, short));
    }

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
        // A window's new size may bring HEVC back only when due() would: a
        // gap after the last change and after H.264 was last needed.
        let mut window = Switches::default();
        assert!(window.hevc_may(at(0)));
        window.changed(at(100));
        assert!(!window.hevc_may(at(3099)));
        assert!(window.hevc_may(at(3100)));
        assert_eq!(window.due(Codec::H264, false, at(3200)), None);
        assert!(!window.hevc_may(at(6199)));
        assert!(window.hevc_may(at(6200)));
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

    // This PC's first monitor under a name no monitor has, which capture
    // never finds. None, with the reason printed, with no monitor attached.
    fn unlisted_monitor() -> Option<capture::Monitor> {
        capture::make_process_dpi_aware().unwrap_or_else(|err| panic!("{err}"));
        let monitors = capture::monitors().unwrap_or_else(|err| panic!("{err}"));
        let Some(mut monitor) = monitors.into_iter().next() else {
            println!("skipped: no monitor is attached to the desktop");
            return None;
        };
        monitor.id.device_name = String::from("not a listed monitor");
        Some(monitor)
    }

    // What looks at the desktop find, from the test, until it is dropped and
    // they ask Windows again.
    struct Looks;

    impl Looks {
        fn find(away: Option<PauseReason>) -> Looks {
            Looks::set(away);
            Looks
        }

        fn set(away: Option<PauseReason>) {
            capture::fault::set_desktop(away, Duration::ZERO);
        }
    }

    impl Drop for Looks {
        fn drop(&mut self) {
            capture::fault::clear();
        }
    }

    // The next call of a share standing away tries its source again.
    fn try_due(sharer: &mut Sharer) {
        let away = sharer.away.as_mut().expect("a share standing away");
        away.tried = Instant::now()
            .checked_sub(capture::LOOK_EVERY)
            .expect("the PC started more than a second ago");
    }

    // A monitor that is not listed at resume() is waited for as a running
    // capture waits for it (capture's missing.rs): the try that still misses
    // it capture::LOOKS tries after the first gives up, and a try that finds
    // it listed but refused, as when another program took it, starts the
    // count again.
    #[test]
    fn a_monitor_away_at_resume_is_given_up_as_capture_gives_up() {
        let mut away = Away {
            reason: PauseReason::Missing,
            told: true,
            tried: Instant::now(),
            misses: 1,
        };
        for _ in 1..capture::LOOKS {
            assert!(away.refused(true));
        }
        assert!(away.refused(false), "found, and taken");
        for _ in 0..capture::LOOKS {
            assert!(away.refused(true));
        }
        assert!(!away.refused(true));
    }

    // A monitor that is not listed when the share resumes leaves it standing
    // away. Its first call says why from the try resume() made, with no try
    // of its own. The calls between tries look at the desktop and say the
    // lock screen at once, a try says a reason of a new kind too, and what
    // came back is answered on every call. The try that still misses the
    // monitor capture::LOOKS tries after the first ends the share. The
    // pattern shares before the pause, and the monitors are only listed:
    // nothing of the screen is captured.
    #[test]
    fn a_monitor_not_listed_at_resume_stands_away_until_it_is_given_up() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let Some(gone) = unlisted_monitor() else {
            return;
        };
        let _looks = Looks::find(None);
        let mut sharer = pattern_share(adapter, (640, 360), None);
        sharer.pause();
        sharer.setup.choice = Choice::Screen(gone);
        assert_eq!(sharer.resume(&mut |_| {}), Ok(true));
        assert!(sharer.paused() && sharer.codec().is_none());
        assert_eq!(sharer.away(), Some(&PauseReason::Missing));
        let reopens = sharer.reopens;
        let mut watching = Watching::default();
        let next = |sharer: &mut Sharer, watching: &mut Watching| {
            sharer.next(watching).unwrap_or_else(|err| panic!("{err}"));
        };
        next(&mut sharer, &mut watching);
        assert_eq!(watching.paused, [PauseReason::Missing]);
        assert_eq!((sharer.reopens, watching.asked), (reopens, 1));

        Looks::set(Some(PauseReason::SecureDesktop));
        next(&mut sharer, &mut watching);
        let locked = [PauseReason::Missing, PauseReason::SecureDesktop];
        assert_eq!(watching.paused, locked);
        assert_eq!((sharer.reopens, watching.asked), (reopens, 2));
        // A try on the lock screen says the same, and nothing new.
        try_due(&mut sharer);
        next(&mut sharer, &mut watching);
        assert_eq!((sharer.reopens, watching.asked), (reopens + 1, 3));
        assert_eq!(watching.paused, locked);
        // Back from it, the look keeps the reason until a try finds the
        // monitor still missing.
        Looks::set(None);
        next(&mut sharer, &mut watching);
        assert_eq!(watching.paused, locked);
        try_due(&mut sharer);
        next(&mut sharer, &mut watching);
        assert_eq!(watching.paused[2..], [PauseReason::Missing]);

        // Three tries have missed it, resume()'s with them.
        for _ in 3..capture::LOOKS {
            try_due(&mut sharer);
            next(&mut sharer, &mut watching);
        }
        try_due(&mut sharer);
        sharer
            .next(&mut watching)
            .expect_err("the wait for the monitor ran out");
        assert_eq!(sharer.reopens, reopens + u64::from(capture::LOOKS));
        assert_eq!(watching.paused.len(), 3);
    }

    // Resumed while its monitor is not listed, a share keeps the codec it
    // was paused in for its next encoder: within a SWITCH_GAP of the change
    // to H.264 the source opens again in H.264, and no change is counted.
    // With no call for as long as the wait between tries, as with nobody
    // watching since resume(), the first call tries first, and with the
    // source back says nothing of the reason resume() found: it opens, its
    // first frame an IDR.
    #[test]
    fn a_share_resumed_while_away_keeps_its_codec_and_tries_first() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let Some(gone) = unlisted_monitor() else {
            return;
        };
        let mut sharer = pattern_share(adapter, (2560, 1440), None);
        if sharer.codec() != Some(Codec::Hevc) {
            println!("skipped: no encoder here takes HEVC at 2560x1440 and 60 fps");
            return;
        }
        sharer.setup.takes_hevc = false;
        sharer
            .set_fps(30, &mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(sharer.codec(), Some(Codec::H264));
        sharer.setup.takes_hevc = true;
        sharer.pause();
        let pattern = std::mem::replace(&mut sharer.setup.choice, Choice::Screen(gone));
        assert_eq!(sharer.resume(&mut |_| {}), Ok(true));
        assert!(sharer.away().is_some());
        sharer.setup.choice = pattern;
        try_due(&mut sharer);
        let mut watching = Watching {
            takes_hevc: true,
            ..Watching::default()
        };
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
        assert!(watching.paused.is_empty(), "{:?}", watching.paused);
        assert!(sharer.away().is_none());
        assert!(
            watching.sent.len() == 1 && watching.sent[0].idr,
            "{:?}",
            watching.sent
        );
        assert_eq!(sharer.codec(), Some(Codec::H264));
        assert_eq!(sharer.numbers().codec_changes, 1);
    }

    // A pause by hand while the share stands away ends the wait: resumed
    // with its source back, it opens at once, its first frame an IDR, with
    // no word of the reason it stood away for. Between tries a look that
    // cannot ask counts as a lost session, as capture's look for a monitor
    // that is not listed has it. A try refused for anything a running
    // capture does not wait out, as a window gone, ends the share.
    #[test]
    fn a_pause_by_hand_ends_the_wait_for_a_source_away() {
        let Some((_turn, adapter)) = gpu() else {
            return;
        };
        let Some(gone) = unlisted_monitor() else {
            return;
        };
        let _looks = Looks::find(None);
        let mut sharer = pattern_share(adapter.clone(), (640, 360), None);
        let mut watching = Watching {
            takes_hevc: true,
            ..Watching::default()
        };
        let next = |sharer: &mut Sharer, watching: &mut Watching| {
            sharer.next(watching).unwrap_or_else(|err| panic!("{err}"));
        };
        sharer.pause();
        let pattern = std::mem::replace(&mut sharer.setup.choice, Choice::Screen(gone.clone()));
        assert_eq!(sharer.resume(&mut |_| {}), Ok(true));
        next(&mut sharer, &mut watching);
        Looks::set(Some(PauseReason::Changing(String::from("a test"))));
        next(&mut sharer, &mut watching);
        let lost = [PauseReason::Missing, PauseReason::Disconnected];
        assert_eq!(watching.paused, lost);

        sharer.pause();
        assert!(sharer.away().is_none());
        Looks::set(None);
        sharer.setup.choice = pattern;
        assert_eq!(sharer.resume(&mut |_| {}), Ok(true));
        assert!(sharer.away().is_none());
        next(&mut sharer, &mut watching);
        assert_eq!(watching.paused, lost);
        assert!(
            watching.sent.len() == 1 && watching.sent[0].idr,
            "{:?}",
            watching.sent
        );

        sharer.pause();
        sharer.setup.choice = Choice::Screen(gone);
        assert_eq!(sharer.resume(&mut |_| {}), Ok(true));
        sharer.setup.choice = Choice::Window {
            window: capture::WindowId {
                hwnd: 0,
                process: 0,
            },
            adapter,
        };
        try_due(&mut sharer);
        sharer
            .next(&mut watching)
            .expect_err("a window gone ends the share");
    }
}
