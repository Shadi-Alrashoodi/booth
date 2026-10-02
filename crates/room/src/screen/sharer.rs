// The share's thread ("share"): this PC's own share, from the screen or the
// test pattern to the watchers, through the share crate's Sharer. It
// waits for Sharing to say this PC shares, opens the capture and the
// encoder, and runs until the share ends or the room closes. Its packets are
// sealed and sent on this thread (pointer updates) and the pacer's (video),
// to the outlets the room keeps in Sharing; what it learns goes back to the
// room as news and numbers, and what the watchers answer comes in through
// Sharing's answers.

use std::any::Any;
use std::collections::VecDeque;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use net::pace::Timer;
use share::rate::{Lost, Rate, Second, Step};
use share::{
    Audience, Back, Choice, CursorShape, CursorUpdate, FineTimer, Kind, Line, MonitorId,
    PauseReason, Preset, Sent, Settings, Setup, Sharer, SharerNumbers, Software, spread,
};

use super::wire::{Shape, ShapeKind};
use super::{Answer, Outbox, OwnShare, SOFTWARE_ENCODER, ShareFacts, ShareNews, Sharing};
use crate::config::VideoSource;
use crate::log::{Log, log};
use crate::remote::Area;
use crate::view::{Latency, Level, Paused, RunningShare, SharingNumbers, SteppedDown};

// Seconds of IDR and invalidation counts the stats panel adds up.
const MINUTE: usize = 60;
// The log has the stats panel's numbers this often, for a test with a
// friend read back later.
const LOG_EVERY_SECONDS: u64 = 10;

pub(crate) fn start(
    sharing: Arc<Sharing>,
    source: VideoSource,
    log: Log,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("share".into())
        .spawn(move || run(&sharing, &source, &log))
}

struct Start {
    number: u32,
    fps: u8,
}

struct Failed {
    // It had opened, so it stopped rather than did not start.
    ran: bool,
    why: String,
}

fn run(sharing: &Arc<Sharing>, source: &VideoSource, log: &Log) {
    let mut last = None;
    while let Some(start) = next_share(sharing, last, log) {
        last = Some(start.number);
        let number = start.number;
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| share(sharing, source, log, start)));
        sharing.closed();
        sharing.set_numbers(None);
        let failed = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(failed)) => Some(failed),
            Err(payload) => Some(Failed {
                ran: true,
                why: format!("the share's thread stopped: {}", panic_text(&*payload)),
            }),
        };
        if let Some(Failed { ran, why }) = failed {
            log!(log, "share {number}: {why}");
            sharing.tell(ShareNews::Failed {
                share: number,
                ran,
                why,
            });
        }
    }
}

// The next share of this PC's to run: not `last`, which ran already and
// may still be granted while the room takes in that it failed. None once
// the room closes.
fn next_share(sharing: &Sharing, last: Option<u32>, log: &Log) -> Option<Start> {
    let mut waits_failed = false;
    loop {
        if sharing.closing() {
            return None;
        }
        if let OwnShare::Sharing { number, fps } = sharing.state()
            && Some(number) != last
        {
            return Some(Start { number, fps });
        }
        if let Err(err) = net::pace::wait(sharing.signal(), None) {
            if !std::mem::replace(&mut waits_failed, true) {
                log!(
                    log,
                    "share: {err}; looking every 50 ms for a share to start instead"
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn share(
    sharing: &Arc<Sharing>,
    source: &VideoSource,
    log: &Log,
    start: Start,
) -> Result<(), Failed> {
    let number = start.number;
    let did_not_start = |why: String| Failed { ran: false, why };
    // A 1 ms timer resolution while sharing, so the last change before the
    // screen goes still is not up to 15 ms late. Given back when this
    // returns, whichever way.
    let _timer = match FineTimer::raise() {
        Ok(timer) => Some(timer),
        Err(why) => {
            log!(log, "share {number}: {why}");
            None
        }
    };
    let choice = choose(source, sharing.monitor(), log, number).map_err(did_not_start)?;
    // Capture turns a rotated monitor upright and scales the whole of it
    // into the picture, so the picture is exactly this rectangle.
    sharing.set_area(match &choice {
        Choice::Screen(monitor) => Some(Area {
            left: monitor.left,
            top: monitor.top,
            width: monitor.width,
            height: monitor.height,
        }),
        _ => None,
    });
    let facts = sharing.facts();
    let allowed = facts.map_or(sharing.upload_kbps, |facts| facts.rate_kbps);
    let setup = Setup {
        choice,
        fps: u32::from(start.fps),
        settings: Settings {
            bitrate: bits(allowed),
            preset: Preset::P1,
        },
        encoder: None,
        // As the watchers decode: the host's facts say whether every one
        // takes HEVC, and the share goes in it where this GPU offers it.
        codec: None,
        takes_hevc: facts.is_none_or(|facts| facts.hevc),
        payload: facts.map_or(share::PAYLOAD_INTERNET, |facts| facts.payload()),
        spread: facts.is_none_or(|facts| facts.spread),
        clock: sharing.share_clock(),
        keep_times: false,
    };
    let send = {
        let mut outbox = sharing.outbox();
        move |packet: &[u8]| {
            outbox.video(packet);
        }
    };
    let mut say = |line: Line| said(log, number, line);
    let mut sharer = Sharer::open(setup, send, &mut say).map_err(did_not_start)?;
    let mut software = software_sentence(sharer.kind(), sharer.software());
    let (width, height) = sharer.size();
    log!(
        log,
        "share {number}: {} at {width}x{height} and {} fps, {} kbit/s",
        sharer.encoder_name(),
        sharer.fps(),
        allowed
    );
    if let Some(sentence) = software {
        sharing.tell(ShareNews::Software {
            share: number,
            sentence,
        });
    }
    sharing.opened(
        number,
        RunningShare {
            software: software.is_some(),
            paused: None,
        },
    );
    announce_fps(sharing, number, &sharer);

    let ran = |why: String| Failed { ran: true, why };
    let mut watchers = Watchers::new(sharing, log, number);
    let mut rate = Rate::new(allowed);
    let mut applied = facts;
    let mut asked_fps = start.fps;
    let mut tally = Tally::new(sharing.upload_bytes());
    let mut next_second = Instant::now() + Duration::from_secs(1);
    let mut seconds = 0u64;
    let mut lost_here_before = 0u64;
    let mut rest = Rest::new(log, number);
    loop {
        if sharing.closing() {
            break;
        }
        match sharing.state() {
            OwnShare::Sharing { number: now, fps } if now == number => {
                if fps != asked_fps {
                    asked_fps = fps;
                    sharer.set_fps(u32::from(fps), &mut say).map_err(ran)?;
                    log!(
                        log,
                        "share {number}: {fps} fps asked, running at {} fps",
                        sharer.fps()
                    );
                    // Even with nothing opened again: the roster has the rate
                    // the share runs at, which stepped down is not the one
                    // asked, and only this thread knows it.
                    announce_fps(sharing, number, &sharer);
                }
            }
            _ => break,
        }
        let facts = sharing.facts();
        if facts != applied {
            applied = facts;
            if let Some(facts) = facts {
                apply(&mut sharer, &mut rate, &facts, log, number).map_err(ran)?;
            }
        }
        // Nobody watches: nothing is captured, encoded or sent, which
        // spares a client's upload, where its own voice would wait behind
        // the video, and the GPU next to a game. The facts say so only once
        // the host has told them. A new watcher's IDR ask ends the rest at
        // once, since the host can hold the facts back for FACTS_EVERY.
        if applied.is_some_and(|facts| facts.watchers == 0) && !sharing.answers_waiting() {
            rest.until(sharing, next_second);
        } else {
            sharer.next(&mut watchers).map_err(ran)?;
            // The GPU encoder failed on that frame and the software encoder
            // took the share over, at 60 fps from now on if it ran faster.
            if software.is_none()
                && let Some(why) = sharer.software()
            {
                software = Some(why.sentence());
                on_software(sharing, number, &sharer, why.sentence());
            }
        }

        let now = Instant::now();
        if now >= next_second {
            next_second = (next_second + Duration::from_secs(1)).max(now);
            let frames = std::mem::take(&mut watchers.sent);
            let mut encode_ms: Vec<f32> = frames.iter().map(|frame| frame.encode_ms).collect();
            // Frames the sharer lost itself are no sign of the network.
            let lost_here = sharer.numbers().reported_lost_here;
            let excused =
                lost_here.saturating_sub(std::mem::replace(&mut lost_here_before, lost_here));
            let second = Second {
                sent: u32::try_from(frames.len()).unwrap_or(u32::MAX),
                lost: watchers
                    .lost
                    .take()
                    .saturating_sub(u32::try_from(excused).unwrap_or(u32::MAX)),
                bytes: frames.iter().map(|frame| frame.packet_bytes as u64).sum(),
                round_trip: sharing.take_round_trip(),
                encode_ms: spread(&mut encode_ms).map(|(median, _)| median),
                interval: Duration::from_secs(1) / sharer.fps(),
                internet: applied.is_some_and(|facts| facts.internet > 0),
            };
            let decision = rate.second(now, &second);
            if let Some(line) = rate.line(&decision) {
                log!(log, "share {number}: {line}");
            }
            if decision.backoff.is_some() {
                watchers.lost.backed_off();
            }
            if let Some(kbps) = decision.rate_kbps {
                set_bitrate(&mut sharer, kbps, log, number);
            }
            let slow = second
                .encode_ms
                .is_some_and(|ms| ms > second.interval.as_secs_f32() * 1000.0);
            if let Some(step) = decision.step
                && take_step(&mut sharer, step, &mut say).map_err(ran)?
            {
                log!(
                    log,
                    "share {number}: {} to {}x{} at {} fps, {} kbit/s",
                    match step {
                        Step::Down(_) => "stepped down",
                        Step::Up => "stepped back up",
                    },
                    sharer.size().0,
                    sharer.size().1,
                    sharer.fps(),
                    rate.rate_kbps()
                );
                announce_fps(sharing, number, &sharer);
            }
            let numbers = tally.second(
                &frames,
                &mut encode_ms,
                &sharer,
                &rate,
                software.is_some(),
                slow,
                sharing.upload_bytes(),
            );
            seconds += 1;
            if seconds.is_multiple_of(LOG_EVERY_SECONDS) {
                let let_pass = match rate.take_let_pass() {
                    0 => String::new(),
                    passed => format!(
                        ", not near the rate with a sign of a queue let pass in {passed} of the last {LOG_EVERY_SECONDS} s"
                    ),
                };
                log!(
                    log,
                    "share {number}: {seconds} s: {}{let_pass}",
                    numbers_line(&numbers)
                );
            }
            sharing.set_numbers(Some(numbers));
        }
    }
    let numbers = sharer.finish();
    log!(log, "share {number}: over, {}", summary(&numbers));
    Ok(())
}

// What the source can show: the monitor asked for, or the primary one; or
// the test pattern on the GPU that drives the primary monitor. A
// monitor that is gone is never swapped for another: the person chose what
// to show. The error is the chat's clause; the log has the device name.
fn choose(
    source: &VideoSource,
    monitor: Option<MonitorId>,
    log: &Log,
    number: u32,
) -> Result<Choice, String> {
    if let VideoSource::Pattern { width, height, .. } = source
        && (*width == 0 || *height == 0)
    {
        return Err(format!("the test pattern cannot be {width}x{height}"));
    }
    let monitors = share::monitors().map_err(|err| err.to_string())?;
    let primary = || monitors.iter().find(|m| m.primary).or(monitors.first());
    match source {
        VideoSource::Screen => match monitor {
            Some(id) => monitors
                .iter()
                .find(|monitor| monitor.id == id)
                .cloned()
                .map(Choice::Screen)
                .ok_or_else(|| {
                    log!(
                        log,
                        "share {number}: the monitor asked for, {}, is not attached",
                        id.device_name
                    );
                    String::from(
                        "the monitor you chose is not attached any more. Choose another one",
                    )
                }),
            None => primary()
                .cloned()
                .map(Choice::Screen)
                .ok_or_else(|| String::from("no monitor is attached to this PC")),
        },
        VideoSource::Pattern {
            width,
            height,
            busy,
        } => {
            let adapter = match primary() {
                Some(monitor) => monitor.adapter.clone(),
                None => share::adapters()
                    .map_err(|err| err.to_string())?
                    .into_iter()
                    .next()
                    .ok_or("this PC has no graphics card the pattern can be made on")?,
            };
            Ok(Choice::Pattern {
                adapter,
                width: *width,
                height: *height,
                busy: *busy,
            })
        }
        // No thread runs without a source to open.
        VideoSource::Hooks => Err(String::from("this room shares nothing by itself")),
    }
}

// The pacer and the packet size follow the host's facts, and the rate
// follows its rule.
fn apply(
    sharer: &mut Sharer,
    rate: &mut Rate,
    facts: &ShareFacts,
    log: &Log,
    number: u32,
) -> Result<(), String> {
    sharer.set_payload(facts.payload())?;
    sharer.set_spread(facts.spread);
    if let Some(kbps) = rate.allow(facts.rate_kbps) {
        set_bitrate(sharer, kbps, log, number);
    }
    log!(
        log,
        "share {number}: {} watching, {} over the internet, {} kbit/s allowed, {} kbit/s in use, packets spread {}, all decode hevc {}",
        facts.watchers,
        facts.internet,
        facts.rate_kbps,
        rate.rate_kbps(),
        crate::log::yes_no(facts.spread),
        crate::log::yes_no(facts.hevc)
    );
    Ok(())
}

// While nobody watches, the share's thread waits for the room's signal,
// which new facts, a new state and the room closing all set, or for the
// next second's numbers. Without a waitable timer it looks every REST_POLL.
const REST_POLL: Duration = Duration::from_millis(20);

struct Rest<'a> {
    timer: io::Result<Timer>,
    log: &'a Log,
    number: u32,
    said: bool,
}

impl<'a> Rest<'a> {
    fn new(log: &'a Log, number: u32) -> Rest<'a> {
        Rest {
            timer: Timer::new(),
            log,
            number,
            said: false,
        }
    }

    fn until(&mut self, sharing: &Sharing, at: Instant) {
        let waited = match &self.timer {
            Ok(timer) => timer
                .set_at(at)
                .and_then(|()| net::pace::wait(sharing.signal(), Some(timer)))
                .map(drop),
            Err(err) => Err(io::Error::new(err.kind(), err.to_string())),
        };
        if let Err(err) = waited {
            if !std::mem::replace(&mut self.said, true) {
                log!(
                    self.log,
                    "share {}: {err}; looking every {} ms for a watcher instead",
                    self.number,
                    REST_POLL.as_millis()
                );
            }
            thread::sleep(REST_POLL);
        }
    }
}

// A share goes on at the rate it had when the encoder cannot take a new
// one; the log says so.
fn set_bitrate(sharer: &mut Sharer, kbps: u32, log: &Log, number: u32) {
    if let Err(err) = sharer.set_bitrate(bits(kbps)) {
        log!(log, "share {number}: could not set {kbps} kbit/s: {err}");
    }
}

fn take_step(sharer: &mut Sharer, step: Step, say: &mut dyn FnMut(Line)) -> Result<bool, String> {
    match step {
        Step::Down(_) => sharer.step_down(say),
        Step::Up => sharer.step_up(say),
    }
}

// The roster says the rate the share runs at, which the watchers'
// reassemblers wait one interval of: stepped down, or fitted to the
// software encoder, it is not the one asked for. The room passes it on
// only when it changed.
fn announce_fps(sharing: &Sharing, number: u32, sharer: &Sharer) {
    let fps = u8::try_from(sharer.fps()).unwrap_or(u8::MAX);
    sharing.tell(ShareNews::Fps { share: number, fps });
}

// What the panel says, once, of a share on Windows' software encoder: why
// the share is on it, or, when no GPU encoder here would open at all, that
// none did. None while a GPU encoder runs the share.
fn software_sentence(kind: Option<Kind>, why: Option<Software>) -> Option<&'static str> {
    match why {
        Some(why) => Some(why.sentence()),
        None => (kind == Some(Kind::MfSoftware)).then_some(SOFTWARE_ENCODER),
    }
}

// The share went onto the software encoder after it opened: the panel says
// why, the stats panel shows the encoder in warn from the next second, and
// the roster gets the rate the share runs at now.
fn on_software(sharing: &Sharing, number: u32, sharer: &Sharer, sentence: &'static str) {
    sharing.tell(ShareNews::Software {
        share: number,
        sentence,
    });
    sharing.set_running(sharing.running().map(|running| RunningShare {
        software: true,
        ..running
    }));
    announce_fps(sharing, number, sharer);
}

fn bits(kbps: u32) -> u32 {
    kbps.saturating_mul(1000)
}

fn said(log: &Log, number: u32, line: Line) {
    let (Line::Say(text) | Line::Log(text)) = line;
    log!(log, "share {number}: {text}");
}

fn summary(numbers: &SharerNumbers) -> String {
    format!(
        "{:.0} s: {} frames encoded, {} IDRs, {} codec changes, {} recover requests ({} invalidated, {} answered with an IDR, {} covered already), {} IDR asks, {} about frames not sent, {} frames reported that were lost here, {} too big, {} frames the pacer let go ({} IDRs, each made again)",
        numbers.ran.as_secs_f32(),
        numbers.encoded,
        numbers.idrs,
        numbers.codec_changes,
        numbers.recoveries,
        numbers.invalidated,
        numbers.idr_answers,
        numbers.covered,
        numbers.idr_asks,
        numbers.unsent,
        numbers.reported_lost_here,
        numbers.too_big,
        numbers.pace.discarded,
        numbers.idrs_let_go,
    )
}

fn numbers_line(numbers: &SharingNumbers) -> String {
    format!(
        "{} at {}x{} {} fps, {} frames encoded in the last second, encode {}, {} kbit/s of video at a rate of {} ({} allowed, {} backoffs), parity {}%, {} IDRs and {} invalidations in the last minute, {} frames let go by the send thread, upload {} kbit/s{}",
        numbers.encoder,
        numbers.width,
        numbers.height,
        numbers.fps,
        numbers.encoded_fps,
        numbers.encode_ms.map_or_else(
            || String::from("not measured"),
            |l| format!("median {:.2} ms, p95 {:.2} ms", l.median_ms, l.p95_ms)
        ),
        numbers.video_kbps,
        numbers.rate_kbps,
        numbers.allowed_kbps,
        numbers.backoffs,
        numbers.parity_pct,
        numbers.idrs_last_minute,
        numbers.invalidations_last_minute,
        numbers.let_go,
        numbers.upload_kbps,
        match numbers.stepped_down {
            Some(SteppedDown::LowRate) => ", stepped down to 1080p60 for the rate",
            Some(SteppedDown::SlowEncode) => ", stepped down to 1080p60 for a slow encoder",
            None => "",
        }
    )
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

// The share's side of whoever watches: the pointer goes out on this thread
// as capture has it, and the answers come in just before each frame is
// encoded.
struct Watchers<'a> {
    sharing: &'a Arc<Sharing>,
    outbox: Outbox,
    log: &'a Log,
    number: u32,
    // Someone started watching since the sharer last asked: the next frame
    // is an IDR for them, which the floor between IDRs never holds.
    started: bool,
    // The pointer's shape and where it is, which a new watcher needs again:
    // capture sends a shape only when it changes.
    shape: Option<(u32, Shape)>,
    pointer: Option<(i32, i32, bool)>,
    shape_refused: bool,
    // Frames watchers reported lost since the rate last looked.
    lost: Lost,
    // This second's frames, for the numbers.
    sent: Vec<Sent>,
    // Every watcher decodes HEVC, as the facts said when the answers were
    // last taken.
    hevc: bool,
}

impl<'a> Watchers<'a> {
    fn new(sharing: &'a Arc<Sharing>, log: &'a Log, number: u32) -> Watchers<'a> {
        Watchers {
            sharing,
            outbox: sharing.outbox(),
            log,
            number,
            started: false,
            shape: None,
            pointer: None,
            shape_refused: false,
            lost: Lost::default(),
            sent: Vec::new(),
            hevc: true,
        }
    }

    fn send_shape(&mut self, shape: Shape) {
        match self.sharing.shape(shape.clone()) {
            Ok(id) => self.shape = Some((id, shape)),
            Err(err) => {
                if !std::mem::replace(&mut self.shape_refused, true) {
                    log!(
                        self.log,
                        "share {}: a pointer shape left out: {err}",
                        self.number
                    );
                }
            }
        }
    }

    // Someone started watching after the shape went out: it goes again,
    // under a new id, with the pointer where it is.
    fn shape_again(&mut self) {
        let Some((_, shape)) = self.shape.take() else {
            return;
        };
        self.send_shape(shape);
        if let (Some((x, y, visible)), Some((id, _))) = (self.pointer, &self.shape) {
            self.outbox.pointer(x, y, visible, *id);
        }
    }
}

impl Audience for Watchers<'_> {
    fn cursor(&mut self, update: CursorUpdate) {
        if let Some(shape) = &update.shape {
            match room_shape(shape, update.scale) {
                Some(shape) => self.send_shape(shape),
                None => {
                    if !std::mem::replace(&mut self.shape_refused, true) {
                        log!(
                            self.log,
                            "share {}: a pointer shape of {}x{} left out, past what Booth sends",
                            self.number,
                            shape.width,
                            shape.height
                        );
                    }
                }
            }
        }
        self.pointer = Some((update.x, update.y, update.visible));
        let id = self.shape.as_ref().map_or(0, |(id, _)| *id);
        self.outbox.pointer(update.x, update.y, update.visible, id);
    }

    // With the facts, taken together: a new watcher's IDR ask comes in the
    // facts that count it, and takes_hevc answers from the same look.
    fn back(&mut self, into: &mut Vec<Back>) {
        let (answers, facts) = self.sharing.take_answers_and_facts();
        self.hevc = facts.is_none_or(|facts| facts.hevc);
        for answer in answers {
            let back = match answer {
                Answer::Recover { first, last } => Back::Recover { first, last },
                Answer::Idr { seen: Some(seen) } => Back::Idr { seen },
                // A new watcher: not an ask like a viewer's, which the floor
                // holds up to 1.8 s on the software encoder, but the IDR
                // started_watching brings on the next frame. No loss.
                Answer::Idr { seen: None } => {
                    self.started = true;
                    self.shape_again();
                    continue;
                }
                Answer::Loss(loss) => Back::Loss(loss),
            };
            self.lost.heard(&back);
            into.push(back);
        }
    }

    fn started_watching(&mut self) -> bool {
        std::mem::take(&mut self.started)
    }

    fn takes_hevc(&mut self) -> bool {
        self.hevc
    }

    fn sent(&mut self, frame: &Sent) {
        self.lost.sent(frame.number);
        self.sent.push(*frame);
    }

    fn line(&mut self, line: Line) {
        said(self.log, self.number, line);
    }

    fn resumed(&mut self) {
        log!(self.log, "share {}: capture back", self.number);
        self.sharing
            .set_running(self.sharing.running().map(|running| RunningShare {
                paused: None,
                ..running
            }));
    }

    fn paused(&mut self, reason: &PauseReason) {
        log!(self.log, "share {}: capture paused: {reason}", self.number);
        let paused = match reason {
            PauseReason::SecureDesktop => Paused::SecureDesktop,
            PauseReason::Taken => Paused::Taken,
            PauseReason::Disconnected => Paused::Disconnected,
            PauseReason::Changing(_) => Paused::Changing,
        };
        self.sharing.tell(ShareNews::Paused {
            share: self.number,
            paused,
        });
        self.sharing
            .set_running(self.sharing.running().map(|running| RunningShare {
                paused: Some(paused),
                ..running
            }));
    }
}

// capture's pointer shape as the room sends it: None for one past what
// wire::Shape takes, which Windows does not draw.
fn room_shape(shape: &CursorShape, scale: f32) -> Option<Shape> {
    let shape = Shape {
        kind: match shape.kind {
            share::CursorKind::Monochrome => ShapeKind::Monochrome,
            share::CursorKind::Color => ShapeKind::Color,
            share::CursorKind::MaskedColor => ShapeKind::MaskedColor,
        },
        width: u16::try_from(shape.width).ok()?,
        height: u16::try_from(shape.height).ok()?,
        pitch: u16::try_from(shape.pitch).ok()?,
        hotspot_x: i16::try_from(shape.hotspot_x).ok()?,
        hotspot_y: i16::try_from(shape.hotspot_y).ok()?,
        scale_milli: (scale * 1000.0).round().clamp(1.0, 8000.0) as u16,
        bytes: shape.bytes.clone(),
    };
    shape.check().ok()?;
    Some(shape)
}

// Each second's numbers for the stats panel.
struct Tally {
    upload_before: Option<u64>,
    idrs_before: u64,
    invalidated_before: u64,
    // Per second, newest last: IDRs and invalidations.
    minute: VecDeque<(u32, u32)>,
}

impl Tally {
    fn new(upload: Option<u64>) -> Tally {
        Tally {
            upload_before: upload,
            idrs_before: 0,
            invalidated_before: 0,
            minute: VecDeque::with_capacity(MINUTE),
        }
    }

    // `encode_ms` is the second's, which this sorts.
    #[allow(clippy::too_many_arguments)]
    fn second(
        &mut self,
        frames: &[Sent],
        encode_ms: &mut [f32],
        sharer: &Sharer,
        rate: &Rate,
        software: bool,
        slow: bool,
        upload: Option<u64>,
    ) -> SharingNumbers {
        let numbers = sharer.numbers();
        let idrs = numbers.idrs.saturating_sub(self.idrs_before);
        let invalidated = numbers.invalidated.saturating_sub(self.invalidated_before);
        self.idrs_before = numbers.idrs;
        self.invalidated_before = numbers.invalidated;
        if self.minute.len() == MINUTE {
            self.minute.pop_front();
        }
        self.minute.push_back((
            u32::try_from(idrs).unwrap_or(u32::MAX),
            u32::try_from(invalidated).unwrap_or(u32::MAX),
        ));
        let upload_kbps = match (self.upload_before, upload) {
            (Some(before), Some(now)) => kbps(now.saturating_sub(before)),
            _ => 0,
        };
        self.upload_before = upload;
        let (width, height) = sharer.size();
        let bytes: u64 = frames.iter().map(|frame| frame.bytes as u64).sum();
        let encode_ms = spread(encode_ms).map(|(median_ms, p95_ms)| Latency {
            median_ms,
            p95_ms,
            about: false,
        });
        SharingNumbers {
            encoder: sharer.encoder_name().to_string(),
            software,
            width,
            height,
            fps: sharer.fps(),
            fps_level: if slow { Level::Warn } else { Level::Good },
            encoded_fps: u32::try_from(frames.len()).unwrap_or(u32::MAX),
            encode_ms,
            video_kbps: kbps(bytes),
            rate_kbps: rate.rate_kbps(),
            allowed_kbps: rate.allowed_kbps(),
            encoder_kbps: sharer.encoder_bitrate() / 1000,
            backoffs: rate.backoffs(),
            parity_pct: sharer.parity(),
            idrs_last_minute: self.minute.iter().map(|(idrs, _)| idrs).sum(),
            invalidations_last_minute: self.minute.iter().map(|(_, inv)| inv).sum(),
            idrs: numbers.idrs,
            let_go: sharer.pace_numbers().discarded,
            idrs_let_go: numbers.idrs_let_go,
            stepped_down: rate.small().filter(|_| sharer.stepped_down()),
            upload_kbps,
        }
    }
}

fn kbps(bytes: u64) -> u32 {
    u32::try_from(bytes.saturating_mul(8) / 1000).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use share::CursorKind;

    // What capture gives crosses the room as it was, its scale with it, and
    // the watcher's viewer gets it back as capture made it.
    #[test]
    fn pointer_shape_round_trips() {
        let captured = CursorShape {
            kind: CursorKind::MaskedColor,
            width: 32,
            height: 32,
            pitch: 128,
            hotspot_x: 3,
            hotspot_y: 4,
            bytes: vec![7; 32 * 32 * 4],
        };
        let shape = room_shape(&captured, 1.5).expect("a shape Booth sends");
        assert_eq!(
            (shape.kind, shape.scale_milli),
            (ShapeKind::MaskedColor, 1500)
        );
        assert_eq!(super::super::cursor_shape(&shape), captured);
        let monochrome = CursorShape {
            kind: CursorKind::Monochrome,
            width: 32,
            height: 64,
            pitch: 4,
            bytes: vec![0xf0; 4 * 64],
            ..captured.clone()
        };
        let shape = room_shape(&monochrome, 1.0).expect("a monochrome shape");
        assert_eq!(super::super::cursor_shape(&shape), monochrome);
        // Past what the room sends: left out, not cut.
        let wide = CursorShape {
            width: 300,
            pitch: 1200,
            bytes: vec![7; 300 * 32 * 4],
            ..captured
        };
        assert!(room_shape(&wide, 1.0).is_none());
    }

    // The chat says what happened in plain words, and the device name goes
    // to the log only. choose only lists the monitors; nothing is captured.
    #[test]
    fn gone_monitor_sentence() {
        let (log, captured) = Log::capture(8);
        let nowhere = MonitorId {
            device_name: String::from(r"\\.\NOT-A-MONITOR"),
            adapter_luid: 0,
        };
        let Err(why) = choose(&VideoSource::Screen, Some(nowhere), &log, 4) else {
            panic!("a monitor that does not exist was chosen");
        };
        assert_eq!(
            crate::screen::share_failed(&why, false),
            "Could not start sharing: the monitor you chose is not attached any more. Choose another one."
        );
        assert_eq!(
            captured.lines(),
            [r"share 4: the monitor asked for, \\.\NOT-A-MONITOR, is not attached"]
        );
        let pattern = VideoSource::Pattern {
            width: 0,
            height: 1440,
            busy: false,
        };
        assert_eq!(
            choose(&pattern, None, &log, 5).err().as_deref(),
            Some("the test pattern cannot be 0x1440")
        );
    }

    // The source can answer with a pointer update, or with nothing new,
    // before its next picture.
    fn next_frame(sharer: &mut Sharer, watchers: &mut Watchers) -> Sent {
        let before = watchers.sent.len();
        for _ in 0..20 {
            sharer.next(watchers).unwrap_or_else(|err| panic!("{err}"));
            if let Some(frame) = watchers.sent.get(before) {
                return *frame;
            }
        }
        panic!("no frame went out in 20 tries");
    }

    // The panel's sentence for a share on Windows' software encoder: Intel
    // graphics and a GPU encoder that failed each have their own, and a PC
    // whose GPU has no encoder that opened has the first one. A share on a
    // GPU encoder says nothing.
    #[test]
    fn software_encoder_sentences() {
        assert_eq!(software_sentence(Some(Kind::Nvenc), None), None);
        assert_eq!(software_sentence(Some(Kind::MfHardware), None), None);
        assert_eq!(software_sentence(None, None), None);
        assert_eq!(
            software_sentence(Some(Kind::MfSoftware), None),
            Some(
                "No GPU encoder on this PC would open. Sharing will use the software encoder at up to 1080p60."
            )
        );
        assert_eq!(
            software_sentence(Some(Kind::MfSoftware), Some(Software::Intel)),
            Some(
                "Intel GPU encoders are not supported yet. Sharing will use the software encoder at up to 1080p60."
            )
        );
        assert_eq!(
            software_sentence(Some(Kind::MfSoftware), Some(Software::GpuFailed)),
            Some(
                "The GPU encoder failed, so sharing goes on with the software encoder at up to 1080p60. If it keeps happening, update the graphics driver."
            )
        );
        for sentence in [
            SOFTWARE_ENCODER,
            Software::Intel.sentence(),
            Software::GpuFailed.sentence(),
        ] {
            assert!(sentence.starts_with(char::is_uppercase), "{sentence}");
            assert!(sentence.ends_with('.'), "{sentence}");
            assert!(sentence.is_ascii() && !sentence.contains('!'), "{sentence}");
        }
    }

    // The floor between IDRs holds a viewer's ask, never the IDR for someone
    // who starts watching, from the room's answer to the encoder. Windows'
    // software encoder answers every loss with an IDR and has the longest
    // floor. The test pattern only; nothing of the screen is captured.
    #[test]
    fn new_watcher_idr_skips_floor() {
        let adapters = share::adapters().unwrap_or_else(|err| panic!("{err}"));
        let Some(adapter) = adapters.first().cloned() else {
            println!("skipped: this PC has no graphics card to make the pattern on");
            return;
        };
        let wake = net::pace::Signal::new().expect("create an event");
        let sharing = Sharing::new(
            crate::peer::Clock::new(Instant::now()),
            false,
            None,
            1000,
            wake,
        );
        let log = Log::off();
        let setup = Setup {
            choice: Choice::Pattern {
                adapter,
                width: 1280,
                height: 720,
                busy: false,
            },
            fps: 60,
            settings: Settings::default(),
            encoder: Some(Kind::MfSoftware),
            codec: Some(share::Codec::H264),
            takes_hevc: false,
            payload: share::PAYLOAD_LAN,
            spread: false,
            clock: sharing.share_clock(),
            keep_times: false,
        };
        let mut sharer =
            Sharer::open(setup, |_: &[u8]| {}, &mut |_| {}).unwrap_or_else(|err| panic!("{err}"));
        let mut watchers = Watchers::new(&sharing, &log, 1);
        let began = Instant::now();
        let first = next_frame(&mut sharer, &mut watchers);
        assert!(first.idr);
        // The first IDR at 15 Mbit/s, hundreds of kilobytes on this encoder,
        // and the floor after it at 1 Mbit/s: a quarter of that takes
        // seconds to carry it.
        sharer
            .set_bitrate(1_000_000)
            .unwrap_or_else(|err| panic!("{err}"));
        let floor = Duration::from_secs_f64(first.bytes as f64 * 8.0 / 250_000.0);

        sharing.answer(Answer::Idr {
            seen: Some(first.number),
        });
        let asked = next_frame(&mut sharer, &mut watchers);
        assert!(!asked.idr, "a viewer's ask inside the floor waits");
        assert_eq!(sharer.numbers().floor_waits, 1);

        sharing.answer(Answer::Idr { seen: None });
        let joined = next_frame(&mut sharer, &mut watchers);
        let after = began.elapsed();
        println!(
            "{}: an IDR of {} bytes, a floor of {:.0} ms after it; the new watcher's IDR went out as frame {}, {:.0} ms into the share",
            sharer.encoder_name(),
            first.bytes,
            floor.as_secs_f64() * 1000.0,
            joined.number,
            after.as_secs_f64() * 1000.0
        );
        assert!(joined.idr && joined.number == asked.number + 1);
        assert!(
            after < floor,
            "the floor after an IDR of {} bytes ended before the new watcher's frame",
            first.bytes
        );
        assert_eq!(
            watchers.lost.take(),
            1,
            "only the viewer's ask counts as a loss"
        );
    }
}
