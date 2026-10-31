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
    Audience, Back, CaptureOptions, Choice, CursorShape, CursorUpdate, FineTimer, Kind, Line,
    PauseReason, Preset, SMALLEST_REGION, Sent, Settings, Setup, Sharer, SharerNumbers, Software,
    WINDOW_CLOSED, WINDOW_GONE, spread,
};

use super::wire::{Shape, ShapeKind};
use super::{Answer, Outbox, OwnShare, SOFTWARE_ENCODER, ShareFacts, ShareNews, Sharing, Shown};
use crate::config::VideoSource;
use crate::log::{Log, log};
use crate::remote::Area;
use crate::view::{Latency, Level, Paused, RunningShare, ShareState, SharingNumbers, SteppedDown};

// Seconds of IDR and invalidation counts the stats panel adds up.
const MINUTE: usize = 60;
// Watched this long with no picture from a shared window, the sharer is told
// why that may be (ShareNews::NoPicture).
const NO_PICTURE_AFTER: Duration = Duration::from_secs(1);
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
    // The last share's number, if that share failed. A share that ended
    // without failing ended on the room's word, so its number is free at
    // once.
    let mut failed = None;
    while let Some(start) = next_share(sharing, &mut failed, log) {
        let number = start.number;
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| share(sharing, source, log, start)));
        sharing.closed();
        sharing.set_numbers(None);
        let failure = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(failure)) => Some(failure),
            Err(payload) => Some(Failed {
                ran: true,
                why: format!("the share's thread stopped: {}", panic_text(&*payload)),
            }),
        };
        failed = failure.as_ref().map(|_| number);
        if let Some(Failed { ran, why }) = failure {
            log!(log, "share {number}: {why}");
            sharing.tell(ShareNews::Failed {
                share: number,
                ran,
                why,
            });
        }
    }
}

// The next share of this PC's to run: not `failed`, which the room can
// still show granted until it takes the failure in. Once it shows anything
// but Sharing that share is over for it, whether it took the failure in or
// ended the share first, and the number can be granted again: a host that
// restarted numbers its shares afresh. None once the room closes.
fn next_share(sharing: &Sharing, failed: &mut Option<u32>, log: &Log) -> Option<Start> {
    let mut waits_failed = false;
    loop {
        if sharing.closing() {
            return None;
        }
        if let Some(start) = startable(sharing.state(), failed) {
            return Some(start);
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

fn startable(state: OwnShare, failed: &mut Option<u32>) -> Option<Start> {
    match state {
        OwnShare::Sharing { number, fps } if Some(number) != *failed => Some(Start { number, fps }),
        OwnShare::Sharing { .. } => None,
        OwnShare::Off | OwnShare::Asking { .. } | OwnShare::Refused(_) => {
            *failed = None;
            None
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
    let choice = choose(source, &sharing.shown(), log, number).map_err(did_not_start)?;
    let window = match &choice {
        Choice::Window { window, .. } => Some(*window),
        _ => None,
    };
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
            held: false,
        },
    );
    announce_fps(sharing, number, &sharer);

    let ran = |why: String| Failed { ran: true, why };
    let mut watchers = Watchers::new(sharing, log, number, window.is_some());
    let mut rate = Rate::new(allowed);
    let mut applied = facts;
    let mut applied_limits = None;
    let mut asked_fps = start.fps;
    let mut tally = Tally::new(sharing.upload_bytes());
    let mut next_second = Instant::now() + Duration::from_secs(1);
    let mut seconds = 0u64;
    let mut lost_here_before = 0u64;
    let mut rest = Rest::new(log, number);
    let mut told_state = ShareState::Live;
    let mut first_picture = FirstPicture::default();
    // The share rested with nobody watching, and its capture, or a window
    // share's own look at the desktop, has not answered since.
    let mut unseen = false;
    let mut desktop = window.map(|_| Desktop::new(sharing, log, number));
    loop {
        if sharing.closing() {
            break;
        }
        match sharing.state() {
            OwnShare::Sharing { number: now, fps } if now == number => {
                // Not while capture is paused: a new frame rate opens the
                // source again. It is applied once a picture comes back.
                if fps != asked_fps && !sharer.paused() && !sharer.held() {
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
        // Paused by hand: the screen is let go before anything more of it is
        // read, and taken up again when the pause ends. The panel and the
        // watchers are told first: nothing of the screen is read from here
        // on, and letting go of the capture and the encoder can take seconds
        // while other work holds the core.
        if sharing.held() != sharer.held() {
            if sharing.held() {
                watchers.hide_pointer();
                sharing.set_running(sharing.running().map(|running| RunningShare {
                    held: true,
                    ..running
                }));
                told_state = ShareState::Paused;
                sharing.tell(ShareNews::State {
                    share: number,
                    state: ShareState::Paused,
                });
                sharer.pause();
                log!(
                    log,
                    "share {number}: paused, nothing of the screen is read until it resumes"
                );
            } else {
                sharer.resume(&mut say).map_err(ran)?;
                let (width, height) = sharer.size();
                log!(
                    log,
                    "share {number}: resumed at {width}x{height} and {} fps",
                    sharer.fps()
                );
                sharing.set_running(sharing.running().map(|running| RunningShare {
                    held: sharer.held(),
                    ..running
                }));
            }
        }
        // Not while capture is paused either: a new box or rate opens the
        // source again.
        let limits = sharing.limits();
        if limits != applied_limits && !sharer.paused() && !sharer.held() {
            applied_limits = limits;
            if let Some(limits) = limits {
                let limit = CaptureOptions {
                    max_width: u32::from(limits.width),
                    max_height: u32::from(limits.height),
                    max_fps: u32::from(limits.fps),
                };
                if sharer.set_limit(limit, &mut say).map_err(ran)? {
                    let (width, height) = sharer.size();
                    log!(
                        log,
                        "share {number}: running at {width}x{height} and {} fps for what the watchers take",
                        sharer.fps()
                    );
                    announce_fps(sharing, number, &sharer);
                }
            }
        }
        let facts = sharing.facts();
        if facts != applied {
            let before = applied;
            applied = facts;
            if let Some(facts) = facts {
                apply(
                    &mut sharer,
                    &mut rate,
                    before.as_ref(),
                    &facts,
                    sharing.host,
                    log,
                    number,
                )
                .map_err(ran)?;
            }
        }
        // Nobody watches: nothing is captured, encoded or sent, which
        // spares a client's upload, where its own voice would wait behind
        // the video, and the GPU next to a game. The facts say so only once
        // the host has told them. A new watcher's IDR ask ends the rest at
        // once, since the host can hold the facts back for FACTS_EVERY, and
        // the share does not rest again on facts that do not count them
        // yet.
        let nobody =
            applied.is_some_and(|facts| facts.watchers == 0) && !sharing.joining(Instant::now());
        if sharer.held() || (nobody && !sharing.answers_waiting()) {
            // Nothing looks at the desktop while the share rests, and the
            // lock screen can come up meanwhile: the PC sound waits until
            // the capture, or for a window the share itself, has looked
            // again.
            if nobody && !std::mem::replace(&mut unseen, true) {
                sharing.set_unseen(number, true);
            }
            rest.until(sharing, next_second);
            // Nothing reads the window while the share rests, so its closing
            // is looked for here: the share ends as it would have.
            if window.is_some_and(|window| !window.exists()) {
                return Err(ran(String::from(WINDOW_CLOSED)));
            }
        } else {
            // Before each wait for a picture, up to 100 ms on a still window.
            // The sound after a rest waits for this look only, and stops at
            // the next look after the desktop goes, which comes later than
            // one wait only when the encoder or the capture opens again in
            // between. A session lost outside the console can take up to
            // 100 ms more: capture keeps the session manager's answer that
            // long.
            if let Some(desktop) = &mut desktop {
                desktop.follow(&mut unseen, share::desktop_away);
            }
            let asked = Instant::now();
            let late = sharer.numbers().held;
            sharer.next(&mut watchers).map_err(ran)?;
            // A monitor's capture that is not paused saw the desktop as it
            // is now: it loses its duplication when the secure desktop comes
            // up, so even a still screen's answer says it is the sharer's. A
            // picture the fps cap held says nothing: it can be from before
            // the rest.
            if unseen && desktop.is_none() && !sharer.paused() && sharer.numbers().held == late {
                unseen = false;
                sharing.set_unseen(number, false);
            }
            // Only while someone watches: a share at rest reads nothing.
            if let Some(window) = window
                && applied.is_some_and(|facts| facts.watchers > 0)
                && first_picture.after(
                    asked.elapsed(),
                    sharer.numbers().captured > 0,
                    window.minimized(),
                )
            {
                log!(
                    log,
                    "share {number}: no picture from the window in its first {} s of being watched",
                    NO_PICTURE_AFTER.as_secs()
                );
                sharing.tell(ShareNews::NoPicture { share: number });
            }
            // The GPU encoder failed on that frame and the software encoder
            // took the share over, at 60 fps from now on if it ran faster.
            if software.is_none()
                && let Some(why) = sharer.software()
            {
                software = Some(why.sentence());
                on_software(sharing, number, &sharer, why.sentence());
            }
        }

        // The watchers hear why no frames come, from the frame it changed on.
        let state = if sharer.held() {
            ShareState::Paused
        } else if sharer.paused() {
            ShareState::Away
        } else {
            ShareState::Live
        };
        if state != told_state {
            told_state = state;
            sharing.tell(ShareNews::State {
                share: number,
                state,
            });
        }

        let now = Instant::now();
        if now >= next_second {
            next_second = (next_second + Duration::from_secs(1)).max(now);
            stats::gauge!("room.outbox.plain", watchers.outbox.plain);
            stats::gauge!("room.outbox.sealed", watchers.outbox.sealed);
            stats::gauge!("room.share_tally.minute", tally.minute);
            sharer.gauge();
            let frames = std::mem::take(&mut watchers.sent);
            let mut encode_ms: Vec<f32> = frames.iter().map(|frame| frame.encode_ms).collect();
            // Frames the sharer lost itself are no sign of the network.
            let lost_here = sharer.numbers().reported_lost_here;
            let excused =
                lost_here.saturating_sub(std::mem::replace(&mut lost_here_before, lost_here));
            let (round_trip, unanswered_ms) = sharing.take_round_trip();
            let second = Second {
                sent: u32::try_from(frames.len()).unwrap_or(u32::MAX),
                lost: watchers
                    .lost
                    .take()
                    .saturating_sub(u32::try_from(excused).unwrap_or(u32::MAX)),
                bytes: frames.iter().map(|frame| frame.packet_bytes as u64).sum(),
                round_trip,
                unanswered_ms,
                shard_loss: watchers.shard_loss(now),
                encode_ms: spread(&mut encode_ms).map(|(median, _)| median),
                interval: Duration::from_secs(1) / sharer.fps(),
                internet: applied.is_some_and(|facts| facts.internet > 0),
            };
            let slow = second
                .encode_ms
                .is_some_and(|ms| ms > second.interval.as_secs_f32() * 1000.0);
            // While capture is paused nothing goes out to judge the link by,
            // and a step opens the source again, which a monitor that left
            // the desktop would not survive.
            let decision = (!sharer.paused() && !sharer.held()).then(|| rate.second(now, &second));
            if let Some(decision) = &decision {
                if let Some(line) = rate.line(decision) {
                    log!(log, "share {number}: {line}");
                }
                if decision.backoff.is_some() {
                    watchers.lost.backed_off();
                }
                if let Some(kbps) = decision.rate_kbps {
                    set_bitrate(&mut sharer, kbps, log, number);
                }
            }
            if let Some(step) = decision.and_then(|decision| decision.step)
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
                let ignored = match rate.take_ignored_seconds() {
                    0 => String::new(),
                    count => format!(
                        ", not near the rate, a sign of a queue ignored in {count} of the last {LOG_EVERY_SECONDS} s"
                    ),
                };
                log!(
                    log,
                    "share {number}: {seconds} s: {}{ignored}",
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

// What the source can show: the monitor asked for, or the primary one, a
// region of a monitor or a window; or the test pattern on the GPU that
// drives the primary monitor. A monitor or window that is gone is never
// swapped for another: the person chose what to show. The error is the
// chat's clause; the log has the device name or the window's handle, never
// its title.
fn choose(source: &VideoSource, shown: &Shown, log: &Log, number: u32) -> Result<Choice, String> {
    if let Shown::Region { region, .. } = shown
        && (region.width < SMALLEST_REGION || region.height < SMALLEST_REGION)
    {
        return Err(format!(
            "the region is {}x{}, smaller than {SMALLEST_REGION} by {SMALLEST_REGION} pixels. Draw a bigger one",
            region.width, region.height
        ));
    }
    if let VideoSource::Pattern { width, height, .. } = source
        && (*width == 0 || *height == 0)
    {
        return Err(format!("the test pattern cannot be {width}x{height}"));
    }
    let monitors = share::monitors().map_err(|err| err.to_string())?;
    let primary = || monitors.iter().find(|m| m.primary).or(monitors.first());
    let attached = |id: &share::MonitorId| {
        monitors
            .iter()
            .find(|monitor| monitor.id == *id)
            .cloned()
            .ok_or_else(|| {
                log!(
                    log,
                    "share {number}: the monitor asked for, {}, is not attached",
                    id.device_name
                );
                String::from("the monitor you chose is not attached any more. Choose another one")
            })
    };
    match source {
        VideoSource::Screen => match shown {
            Shown::Monitor(Some(id)) => attached(id).map(Choice::Screen),
            Shown::Region { monitor, region } => attached(monitor).map(|monitor| Choice::Region {
                monitor,
                region: *region,
            }),
            Shown::Monitor(None) => primary()
                .cloned()
                .map(Choice::Screen)
                .ok_or_else(|| String::from("no monitor is attached to this PC")),
            Shown::Window(window) => {
                if !window.exists() {
                    log!(
                        log,
                        "share {number}: the window asked for, {:#x}, is not open",
                        window.hwnd
                    );
                    return Err(String::from(WINDOW_GONE));
                }
                // On the GPU of the monitor it is on, so no copy goes
                // between GPUs when it can be helped.
                let adapter = match share::window_monitor(*window) {
                    Some(monitor) => monitor.adapter,
                    None => match primary() {
                        Some(monitor) => monitor.adapter.clone(),
                        None => share::adapters()
                            .map_err(|err| err.to_string())?
                            .into_iter()
                            .next()
                            .ok_or("this PC has no graphics card a window can be captured on")?,
                    },
                };
                log!(
                    log,
                    "share {number}: window {:#x} on {}",
                    window.hwnd,
                    adapter.description
                );
                Ok(Choice::Window {
                    window: *window,
                    adapter,
                })
            }
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
// follows its rule. The host's own share goes out once for each watcher
// over the internet, so more of them is more copies on its uplink
// (Rate::copies); a friend's goes to the host once, and the host's uplink
// carries the copies.
fn apply(
    sharer: &mut Sharer,
    rate: &mut Rate,
    before: Option<&ShareFacts>,
    facts: &ShareFacts,
    host: bool,
    log: &Log,
    number: u32,
) -> Result<(), String> {
    sharer.set_payload(facts.payload())?;
    sharer.set_spread(facts.spread);
    let changed = match before {
        Some(before) if host => rate.copies(
            facts.rate_kbps,
            u32::from(before.internet),
            u32::from(facts.internet),
        ),
        _ => rate.allow(facts.rate_kbps),
    };
    if let Some(kbps) = changed {
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

// How long a window share has waited for its first picture while it was
// watched and not minimized, until a picture comes or the sharer is told.
#[derive(Default)]
struct FirstPicture {
    waited: Duration,
    done: bool,
}

impl FirstPicture {
    // True once, when NO_PICTURE_AFTER of such waiting passed with no
    // picture.
    fn after(&mut self, took: Duration, pictured: bool, minimized: bool) -> bool {
        if self.done || minimized {
            return false;
        }
        if pictured {
            self.done = true;
            return false;
        }
        self.waited += took;
        self.done = self.waited >= NO_PICTURE_AFTER;
        self.done
    }
}

// A window's capture goes on through the lock screen, a UAC prompt and a
// disconnected session without a word, where a monitor's pauses, and what
// the PC plays then is not the sharer's to send. So a window share looks at
// the desktop itself while it is watched, a few microseconds on this PC's
// own screen, and while it is away the PC sound stops as on a monitor's
// pause for it, until a look finds the desktop the sharer's again. A
// minimized window keeps its sound.
struct Desktop<'a> {
    sharing: &'a Sharing,
    log: &'a Log,
    number: u32,
    away: Option<PauseReason>,
}

impl<'a> Desktop<'a> {
    fn new(sharing: &'a Sharing, log: &'a Log, number: u32) -> Desktop<'a> {
        Desktop {
            sharing,
            log,
            number,
            away: None,
        }
    }

    // `look` says why the desktop is away, if it is, or that Windows could
    // not say. `unseen` is the share's: after a rest the sound waits for a
    // look that finds the desktop the sharer's, which takes it.
    fn follow(&mut self, unseen: &mut bool, look: impl FnOnce() -> Option<PauseReason>) {
        let away = look();
        if away != self.away {
            self.sharing.set_away(self.number, away.is_some());
            match &away {
                Some(PauseReason::Disconnected) => log!(
                    self.log,
                    "share {}: this Windows session is disconnected, so no pc sound goes out until it is back",
                    self.number
                ),
                Some(PauseReason::Changing(why)) => log!(
                    self.log,
                    "share {}: {why}, so no pc sound goes out until Windows says it is",
                    self.number
                ),
                Some(_) => log!(
                    self.log,
                    "share {}: Windows shows another desktop than the window's, such as the lock screen or a UAC prompt, so no pc sound goes out until it is back",
                    self.number
                ),
                None => log!(
                    self.log,
                    "share {}: the window's desktop is back",
                    self.number
                ),
            }
            self.away = away;
        }
        if self.away.is_none() && std::mem::take(unseen) {
            self.sharing.set_unseen(self.number, false);
        }
    }
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
    // A window's pictures say nothing of the desktop, which the share looks
    // at itself (Desktop).
    window: bool,
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
    // The worst watcher's shard loss as last reported, and when it came.
    shard_loss: Option<(f32, Instant)>,
}

// Watchers report their shard loss once a second; a report older than two
// of those is no longer the link's.
const SHARD_LOSS_FOR: Duration = Duration::from_secs(2);

impl<'a> Watchers<'a> {
    fn new(sharing: &'a Arc<Sharing>, log: &'a Log, number: u32, window: bool) -> Watchers<'a> {
        Watchers {
            sharing,
            outbox: sharing.outbox(),
            log,
            number,
            window,
            started: false,
            shape: None,
            pointer: None,
            shape_refused: false,
            lost: Lost::default(),
            sent: Vec::new(),
            hevc: true,
            shard_loss: None,
        }
    }

    // The newest shard loss report, while it is at most SHARD_LOSS_FOR old.
    fn shard_loss(&self, now: Instant) -> Option<f32> {
        self.shard_loss
            .filter(|&(_, at)| now.saturating_duration_since(at) <= SHARD_LOSS_FOR)
            .map(|(percent, _)| percent)
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
    // under a new id, with the pointer where it is. No picture comes with
    // that pointer, so it is not marked.
    fn shape_again(&mut self) {
        let Some((_, shape)) = self.shape.take() else {
            return;
        };
        self.send_shape(shape);
        if let (Some((x, y, visible)), Some((id, _))) = (self.pointer, &self.shape) {
            self.outbox.pointer(x, y, visible, *id, false);
        }
    }

    // For a pause: the pointer the watchers last saw would stay on their
    // picture otherwise. No picture follows it either.
    fn hide_pointer(&mut self) {
        let (x, y) = self.pointer.map_or((0, 0), |(x, y, _)| (x, y));
        self.pointer = Some((x, y, false));
        let id = self.shape.as_ref().map_or(0, |(id, _)| *id);
        self.outbox.pointer(x, y, false, id, false);
    }
}

impl Audience for Watchers<'_> {
    fn cursor(&mut self, update: CursorUpdate, with_picture: bool) {
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
        self.outbox
            .pointer(update.x, update.y, update.visible, id, with_picture);
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
                Answer::Loss(loss) => {
                    self.shard_loss = loss.map(|percent| (percent, Instant::now()));
                    Back::Loss(loss)
                }
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
        // A window back from minimized on the lock screen keeps its sound
        // stopped: only the share's next look at the desktop lets it go.
        if !self.window {
            self.sharing.set_away(self.number, false);
        }
        log!(self.log, "share {}: capture back", self.number);
        self.sharing
            .set_running(self.sharing.running().map(|running| RunningShare {
                paused: None,
                ..running
            }));
    }

    fn paused(&mut self, reason: &PauseReason) {
        // Locked, on a UAC prompt or disconnected, the PC may be left alone,
        // and what it plays then is not the sharer's to send: the PC sound
        // stops as for a pause by hand, before anything says paused. A
        // minimized window, a monitor taken, changing or gone keep it, and
        // keep it stopped once it is: a monitor asleep on the lock screen
        // can leave the desktop, and only a picture says it is the sharer's
        // again.
        if matches!(
            reason,
            PauseReason::SecureDesktop | PauseReason::Disconnected
        ) {
            self.sharing.set_away(self.number, true);
        }
        log!(self.log, "share {}: capture paused: {reason}", self.number);
        let paused = match reason {
            PauseReason::SecureDesktop => Paused::SecureDesktop,
            PauseReason::Taken => Paused::Taken,
            PauseReason::Disconnected => Paused::Disconnected,
            PauseReason::Changing(_) => Paused::Changing,
            PauseReason::Missing => Paused::Missing,
            PauseReason::Minimized => Paused::Minimized,
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
    use session::Session;
    use share::{CursorKind, MonitorId, Region, WindowId};

    use crate::screen::ShareOutlet;
    use crate::screen::wire::{SENT, read_pointer};
    use crate::socket::Socket;
    use crate::talk::Outlet;
    use crate::testing::{Wire, session_pair};

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
        let Err(why) = choose(
            &VideoSource::Screen,
            &Shown::Monitor(Some(nowhere)),
            &log,
            4,
        ) else {
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
            choose(&pattern, &Shown::Monitor(None), &log, 5)
                .err()
                .as_deref(),
            Some("the test pattern cannot be 0x1440")
        );
    }

    // A window that closed before the share started, or a handle that names
    // no window any more, is refused with what to do; the log has the
    // handle, never a title.
    #[test]
    fn gone_window_sentence() {
        let (log, captured) = Log::capture(8);
        let gone = WindowId {
            hwnd: 0x5150,
            process: 4,
        };
        let Err(why) = choose(&VideoSource::Screen, &Shown::Window(gone), &log, 7) else {
            panic!("a window that does not exist was chosen");
        };
        assert_eq!(
            crate::screen::share_failed(&why, false),
            "Could not start sharing: the window you chose is not open any more. Choose another one."
        );
        assert_eq!(
            captured.lines(),
            ["share 7: the window asked for, 0x5150, is not open"]
        );
    }

    // Told once, after a second of waiting that was neither minimized nor
    // ended by a picture, in words the chat can show as they are.
    #[test]
    fn no_picture_rule() {
        let tick = Duration::from_millis(100);
        let mut first = FirstPicture::default();
        for _ in 0..9 {
            assert!(!first.after(tick, false, false));
        }
        for _ in 0..20 {
            assert!(!first.after(tick, false, true), "minimized time counted");
        }
        assert!(first.after(tick, false, false));
        assert!(!first.after(tick, false, false), "told twice");
        let mut pictured = FirstPicture::default();
        assert!(!pictured.after(tick, true, false));
        assert!(!pictured.after(Duration::from_secs(5), false, false));
        let text = crate::screen::NO_PICTURE;
        assert!(text.is_ascii() && !text.contains('!') && text.ends_with('.'));
    }

    // A share that failed is not started again while the room shows it
    // granted, as it can until it takes the failure in; another share is.
    // Off, Asking or Refused say the room let it go, and a grant of the same
    // number starts after that: a host that restarted numbers its shares
    // afresh.
    #[test]
    fn a_failed_share_waits_until_the_room_lets_it_go() {
        let granted = |number| OwnShare::Sharing { number, fps: 60 };
        let start = |state: OwnShare, failed: &mut Option<u32>| {
            startable(state, failed).map(|start| (start.number, start.fps))
        };
        let mut failed = Some(1);
        assert_eq!(start(granted(1), &mut failed), None, "started again");
        assert_eq!(failed, Some(1));
        assert_eq!(start(granted(2), &mut failed), Some((2, 60)));
        for over in [
            OwnShare::Off,
            OwnShare::Asking { fps: 60 },
            OwnShare::Refused(crate::screen::Refusal::NotLive),
        ] {
            let mut failed = Some(1);
            assert_eq!(start(over.clone(), &mut failed), None, "{over:?}");
            assert_eq!(failed, None, "{over:?}");
            assert_eq!(start(granted(1), &mut failed), Some((1, 60)), "{over:?}");
        }
    }

    // The same rule on the share's own thread: a share that fails is told to
    // the room once, not over and over while the room still shows it
    // granted. Another number starts, and so does the failed one once the
    // room has shown it Off. The pattern at 0x0 is refused before any monitor
    // or graphics card is looked at, so nothing is captured.
    #[test]
    fn a_failed_share_fails_once_while_it_is_granted() {
        let wake = net::pace::Signal::new().expect("create an event");
        let clock = crate::peer::Clock::new(Instant::now());
        let sharing = Sharing::new(clock, false, None, 1000, wake);
        let thread = start(
            Arc::clone(&sharing),
            VideoSource::Pattern {
                width: 0,
                height: 0,
                busy: false,
            },
            Log::off(),
        )
        .expect("start the share's thread");
        // The shares told failed until `share` is, and for 300 ms after.
        let failures = |share: u32| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut until = deadline;
            let mut told = Vec::new();
            while Instant::now() < until {
                told.extend(
                    sharing
                        .take_news()
                        .into_iter()
                        .filter_map(|news| match news {
                            ShareNews::Failed { share, .. } => Some(share),
                            _ => None,
                        }),
                );
                if until == deadline && told.contains(&share) {
                    until = Instant::now() + Duration::from_millis(300);
                }
                thread::sleep(Duration::from_millis(5));
            }
            told
        };

        sharing.set_state(OwnShare::Sharing { number: 1, fps: 60 });
        let first = failures(1);
        sharing.set_state(OwnShare::Sharing { number: 2, fps: 60 });
        let another = failures(2);
        sharing.set_state(OwnShare::Off);
        // Long enough for the thread to see Off before the grant below.
        thread::sleep(Duration::from_millis(500));
        sharing.set_state(OwnShare::Sharing { number: 2, fps: 60 });
        let again = failures(2);
        // Before the asserts: a thread that fails over and over must not
        // outlive the test.
        sharing.close();
        thread.join().expect("the share's thread ended");

        assert_eq!(first, [1]);
        assert_eq!(another, [2]);
        assert_eq!(again, [2]);
    }

    // A region smaller than SMALLEST_REGION either way is refused before any
    // monitor is looked at.
    #[test]
    fn small_region_sentence() {
        let (log, _) = Log::capture(8);
        let small = Region {
            left: 0,
            top: 0,
            width: 255,
            height: 1080,
        };
        let nowhere = MonitorId {
            device_name: String::from(r"\\.\NOT-A-MONITOR"),
            adapter_luid: 0,
        };
        let shown = Shown::Region {
            monitor: nowhere,
            region: small,
        };
        let Err(why) = choose(&VideoSource::Screen, &shown, &log, 6) else {
            panic!("a region too small was shared");
        };
        assert_eq!(
            crate::screen::share_failed(&why, false),
            "Could not start sharing: the region is 255x1080, smaller than 256 by 256 pixels. Draw a bigger one."
        );
    }

    // The lock screen, a UAC prompt and a disconnected session stop the
    // share's PC sound as a pause by hand does, and it comes back with the
    // picture, not with another pause. A minimized window and the monitor's
    // own pauses keep it. A share that ended can still report a pause: it
    // stops nothing of the next.
    #[test]
    fn the_secure_desktop_and_a_lost_session_stop_pc_sound() {
        let wake = net::pace::Signal::new().expect("create an event");
        let clock = crate::peer::Clock::new(Instant::now());
        let sharing = Sharing::new(clock, false, None, 1000, wake);
        sharing.set_state(OwnShare::Sharing { number: 1, fps: 60 });
        let log = Log::off();
        let mut watchers = Watchers::new(&sharing, &log, 1, false);
        let sound = Arc::clone(sharing.sound());
        for (reason, stops) in [
            (PauseReason::Minimized, false),
            (PauseReason::Taken, false),
            (PauseReason::Missing, false),
            (PauseReason::SecureDesktop, true),
            (PauseReason::Disconnected, true),
        ] {
            watchers.paused(&reason);
            assert_eq!(sound.paused(), stops, "{reason}");
            watchers.resumed();
            assert!(!sound.paused(), "{reason}");
        }
        // The monitor asleep on the lock screen leaves the desktop.
        watchers.paused(&PauseReason::SecureDesktop);
        watchers.paused(&PauseReason::Missing);
        assert!(sound.paused());
        watchers.resumed();
        assert!(!sound.paused());
        // Paused by hand as well, it stays paused when the picture is back.
        watchers.paused(&PauseReason::SecureDesktop);
        sharing.set_held(true);
        watchers.resumed();
        assert!(sound.paused());
        sharing.set_held(false);
        assert!(!sound.paused());
        let mut ended = Watchers::new(&sharing, &log, 2, false);
        ended.paused(&PauseReason::SecureDesktop);
        assert!(!sound.paused(), "another share's pause");
    }

    // A window's capture says nothing of the lock screen or a lost session,
    // so the share looks at the desktop itself before each wait for a
    // picture. Away, the PC sound stops, and only a look that finds the
    // desktop the sharer's lets it go on, not the window's picture back from
    // minimized, which by itself keeps the sound. After a rest the sound
    // waits for a look that finds the desktop. The last share's thread can
    // still be looking when the next share starts: it shuts nothing of it.
    #[test]
    fn a_window_share_looks_at_the_desktop_for_its_pc_sound() {
        let wake = net::pace::Signal::new().expect("create an event");
        let clock = crate::peer::Clock::new(Instant::now());
        let sharing = Sharing::new(clock, false, None, 1000, wake);
        sharing.set_state(OwnShare::Sharing { number: 1, fps: 60 });
        let log = Log::off();
        let mut watchers = Watchers::new(&sharing, &log, 1, true);
        let mut desktop = Desktop::new(&sharing, &log, 1);
        let sound = Arc::clone(sharing.sound());
        let mut unseen = false;
        desktop.follow(&mut unseen, || None);
        watchers.paused(&PauseReason::Minimized);
        assert!(!sound.shut(), "minimized");
        watchers.resumed();
        desktop.follow(&mut unseen, || Some(PauseReason::SecureDesktop));
        assert!(sound.shut(), "the lock screen");
        watchers.paused(&PauseReason::Minimized);
        watchers.resumed();
        assert!(sound.shut(), "the window's picture let the sound go");
        desktop.follow(&mut unseen, || Some(PauseReason::Disconnected));
        assert!(sound.shut(), "disconnected");
        desktop.follow(&mut unseen, || None);
        assert!(!sound.shut(), "back");

        // A rest, as the share's thread marks one.
        let rest = |unseen: &mut bool| {
            *unseen = true;
            sharing.set_unseen(1, true);
        };
        rest(&mut unseen);
        assert!(sound.shut(), "resting");
        desktop.follow(&mut unseen, || Some(PauseReason::SecureDesktop));
        assert!(unseen && sound.shut(), "locked during the rest");
        desktop.follow(&mut unseen, || None);
        assert!(!unseen && !sound.shut(), "back after the rest");
        rest(&mut unseen);
        desktop.follow(&mut unseen, || None);
        assert!(!unseen && !sound.shut(), "a rest on the sharer's desktop");

        sharing.set_state(OwnShare::Sharing { number: 2, fps: 60 });
        desktop.follow(&mut unseen, || Some(PauseReason::SecureDesktop));
        assert!(!sound.shut(), "the last share's look");
    }

    // The pointer updates that came to `at` since the last look, as the far
    // end of its link reads them: x, shown, marked.
    fn pointers(at: &Wire, far: &mut Session) -> Vec<(i32, bool, bool)> {
        let mut plain = Vec::new();
        at.packets()
            .iter()
            .map(|packet| {
                far.decrypt(packet, &mut plain).expect("opens");
                let (channel, payload) = channels::unframe(&plain).expect("a frame");
                assert_eq!(channel, channels::Channel::Cursor);
                let (_, pointer) =
                    read_pointer(payload, SENT, true).expect("an update Booth sends");
                (pointer.x, pointer.visible, pointer.with_picture)
            })
            .collect()
    }

    // Only the pointer of a picture is marked, and only on the link whose
    // copy reads the mark. The pointer sent again with the shape for a new
    // watcher, and the one that hides it for a pause, have no picture after
    // them.
    #[test]
    fn only_a_pictures_pointer_is_marked() {
        let wake = net::pace::Signal::new().expect("create an event");
        let clock = crate::peer::Clock::new(Instant::now());
        let socket = Arc::new(Socket::bind(0, Log::off()).expect("bind a socket"));
        let sharing = Sharing::new(clock, false, Some(socket), 1000, wake);
        let (reads, older) = (Wire::new(), Wire::new());
        let (near, mut far) = session_pair();
        let (older_near, mut older_far) = session_pair();
        let outlet = |session: &Session, to: &Wire, marks: bool| ShareOutlet {
            link: Outlet {
                sealer: session.sealer().expect("the initiator may send"),
                to: to.addr(),
                sent: Arc::default(),
            },
            marks,
        };
        sharing.set_outlets(vec![
            outlet(&near, &reads, true),
            outlet(&older_near, &older, false),
        ]);
        let log = Log::off();
        let mut watchers = Watchers::new(&sharing, &log, 1, false);
        let at = |x: i32, shape: Option<CursorShape>| CursorUpdate {
            x,
            y: 6,
            visible: true,
            scale: 1.0,
            shape,
        };
        let arrow = CursorShape {
            kind: CursorKind::Color,
            width: 32,
            height: 32,
            pitch: 128,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![7; 32 * 32 * 4],
        };
        watchers.cursor(at(5, Some(arrow)), true);
        assert_eq!(pointers(&reads, &mut far), [(5, true, true)]);
        assert_eq!(pointers(&older, &mut older_far), [(5, true, false)]);
        watchers.cursor(at(7, None), false);
        assert_eq!(pointers(&reads, &mut far), [(7, true, false)]);
        watchers.shape_again();
        assert_eq!(
            pointers(&reads, &mut far),
            [(7, true, false)],
            "a new watcher's"
        );
        watchers.hide_pointer();
        assert_eq!(pointers(&reads, &mut far), [(7, false, false)], "a pause's");
        assert_eq!(pointers(&older, &mut older_far).len(), 3);
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
    // software encoder makes the largest IDRs and so has the longest floor.
    // The test pattern only; nothing of the screen is captured.
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
        let mut watchers = Watchers::new(&sharing, &log, 1, false);
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
