// The viewer's thread ("watch"): the share this PC watches, from the
// packets the room hands over to the window, through the share crate's
// Screen. It waits for Watching to say a watch started, opens the window and
// the decoder, and runs until the share ends, this PC stops watching, the
// person closes the window, or the room closes. The packets never pass
// through here: the room's receive thread puts them straight into the
// viewer's inbox. What goes back to the sharer leaves through Watching, and
// the numbers once a second.

use std::any::Any;
use std::collections::VecDeque;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use share::{
    Control, ControlOut, Inbox, Line, LinkNumbers, LinkState, PathWord, PresentPath, Report,
    Screen, Second, ViewerNumbers, Watch, end_to_end_level, spread, stage_level,
};
use stats::TraceSample;

use super::{Back, WatchEvent, WatchNews, Watching};
use crate::config::{LossKnob, VideoConfig};
use crate::log::{Log, log};
use crate::numbers::level;
use crate::remote::{Controls, ViewerInput};
use crate::view::{self, KnobNumbers, Latency, Role, TracePoint, View, WatchingNumbers};

// The window opens 16:9, scaled down to fit, since the share's own size
// comes with its first frame; the picture keeps its shape inside it.
const OPEN_WIDTH: u32 = 1920;
const OPEN_HEIGHT: u32 = 1080;
// Capture to display over the last 10 s, like mouth to ear.
const LATENCY_SECONDS: usize = 10;
// The log has the stats panel's numbers this often, for a test with a
// friend read back later.
const LOG_EVERY_SECONDS: u64 = 10;

// `controls` is where a viewer's input goes while this PC controls the
// share it shows.
pub(crate) fn start(
    watching: Arc<Watching>,
    video: VideoConfig,
    controls: Arc<Controls>,
    log: Log,
) -> io::Result<JoinHandle<()>> {
    let out: Arc<dyn ControlOut> = Arc::new(ViewerInput::new(controls, video.capture.clone()));
    thread::Builder::new()
        .name("watch".into())
        .spawn(move || run(&watching, &video, &out, &log))
}

struct Start {
    share: u32,
    fps: u8,
    name: String,
    inbox: Arc<Inbox>,
}

enum Ended {
    // The share ended, this PC stopped watching, or the room closed.
    ByRoom,
    // The person closed the window.
    ByPerson,
}

fn run(watching: &Watching, video: &VideoConfig, out: &Arc<dyn ControlOut>, log: &Log) {
    // As the room opens, well before anyone can press Watch, so the host
    // hears with the Watch itself whether this PC decodes HEVC.
    if video.hevc {
        let asked = Instant::now();
        let answer = share::primary_takes_hevc();
        let took = asked.elapsed().as_secs_f32() * 1000.0;
        match &answer {
            Ok(()) => log!(
                log,
                "watch: this pc's viewer takes hevc, asked in {took:.1} ms"
            ),
            Err(why) => log!(
                log,
                "watch: this pc's viewer does not take hevc, asked in {took:.1} ms: {why}"
            ),
        }
        watching.set_takes_hevc(answer.is_ok());
    }
    while let Some(start) = next_watch(watching, log) {
        let (share, name) = (start.share, start.name.clone());
        let outcome =
            panic::catch_unwind(AssertUnwindSafe(|| watch(watching, video, out, log, start)));
        watching.set_numbers(None);
        // The window is closed by now, whichever way the watch ended.
        watching.set_showing(false);
        let why = match outcome {
            Ok(Ok(Ended::ByRoom)) => continue,
            Ok(Ok(Ended::ByPerson)) => {
                log!(log, "watch {share}: the viewer was closed");
                watching.tell(WatchNews::Closed { share });
                continue;
            }
            Ok(Err(why)) => why,
            Err(payload) => format!("the viewer's thread stopped: {}", panic_text(&*payload)),
        };
        log!(log, "watch {share}: {why}");
        watching.tell(WatchNews::Failed { share, name, why });
    }
}

// The next share to show, from the starts and ends the room queued. None
// once the room closes.
fn next_watch(watching: &Watching, log: &Log) -> Option<Start> {
    let mut waits_failed = false;
    let mut pending: Option<(u32, u8, String)> = None;
    loop {
        if watching.closing() {
            return None;
        }
        for event in watching.take_events() {
            match event {
                WatchEvent::Started { share, fps, name } => pending = Some((share, fps, name)),
                WatchEvent::Ended { share } => {
                    if pending
                        .as_ref()
                        .is_some_and(|(waiting, _, _)| *waiting == share)
                    {
                        pending = None;
                    }
                }
                WatchEvent::Fps { share, fps } => {
                    if let Some((_, waiting_fps, _)) =
                        pending.as_mut().filter(|(waiting, _, _)| *waiting == share)
                    {
                        *waiting_fps = fps;
                    }
                }
            }
        }
        // No inbox means the watch ended again before this thread got to it,
        // or Windows would not make the inbox's event, which leaves nothing
        // to show either way.
        if let Some((share, fps, name)) = pending.take() {
            match watching.inbox_for(share) {
                Some(inbox) => {
                    return Some(Start {
                        share,
                        fps,
                        name,
                        inbox,
                    });
                }
                None => log!(log, "watch {share}: over before the viewer opened"),
            }
        }
        if let Err(err) = net::pace::wait(watching.signal(), None) {
            if !std::mem::replace(&mut waits_failed, true) {
                log!(
                    log,
                    "watch: {err}; looking every 50 ms for a watch to start instead"
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn watch(
    watching: &Watching,
    video: &VideoConfig,
    out: &Arc<dyn ControlOut>,
    log: &Log,
    start: Start,
) -> Result<Ended, String> {
    let share = start.share;
    let watch = Watch {
        title: format!("{}'s screen", start.name),
        width: OPEN_WIDTH,
        height: OPEN_HEIGHT,
        fps: u32::from(start.fps),
        vsync: video.vsync,
        show: video.show,
        hevc: video.hevc,
        clock: watching.sharer_clock(share).map(|(clock, _)| clock),
        // The room hands the strip's numbers over within LINK_EVERY.
        link: LinkNumbers::default(),
        keep_times: false,
    };
    let mut say = |line: Line| said(log, share, line);
    let screen = Screen::open_to_control(
        &watch,
        Arc::clone(&start.inbox),
        Some(Arc::clone(out)),
        &mut say,
    )?;
    log!(log, "watch {share}: showing {}'s screen", start.name);
    // The viewer's own GPU has the last word, which the room passes on to
    // the host when it differs from what the Watch said.
    watching.set_takes_hevc(screen.takes_hevc());
    watching.set_showing(true);
    let mut tally = Tally::new(watching.knob());
    let mut seconds = 0u64;
    let numbers = screen.run(&mut |report| match report {
        Report::Back(back) => watching.back(room_back(share, back)),
        Report::Line(line) => said(log, share, line),
        Report::Second(second) => {
            let about = watching.sharer_clock(share).is_some_and(|(_, about)| about);
            let (fps, one_way_ms) = watching.pace(share).unwrap_or((start.fps, None));
            let numbers =
                tally.second(second, about, watching.knob_dropped(share), fps, one_way_ms);
            seconds += 1;
            if seconds.is_multiple_of(LOG_EVERY_SECONDS) {
                log!(
                    log,
                    "watch {share}: {seconds} s: {}",
                    numbers_line(&numbers)
                );
            }
            watching.set_numbers(Some(numbers));
        }
        Report::StripClicked => watching.strip_clicked(),
        Report::NoHevc => watching.set_takes_hevc(false),
    })?;
    log!(log, "watch {share}: over, {}", summary(&numbers));
    Ok(if start.inbox.stopped() {
        Ended::ByRoom
    } else {
        Ended::ByPerson
    })
}

fn room_back(share: u32, back: share::Back) -> Back {
    match back {
        share::Back::Recover { first, last } => Back::Recover { share, first, last },
        share::Back::Idr { seen } => Back::Idr { share, seen },
        share::Back::Loss(loss) => Back::Loss { share, loss },
    }
}

fn said(log: &Log, share: u32, line: Line) {
    let (Line::Say(text) | Line::Log(text)) = line;
    log!(log, "watch {share}: {text}");
}

fn summary(numbers: &ViewerNumbers) -> String {
    let reassembly = &numbers.reassembly;
    format!(
        "{:.0} s: {} frames shown, {} decoded, last in {}, {} codec changes, {} did not decode, {} came whole and {} were lost before the first IDR, {} repaired, {} dropped after it, {} held for an IDR, {} packets, {} lost, {}",
        numbers.ran.as_secs_f32(),
        numbers.presented,
        numbers.decoded,
        numbers
            .codec
            .map_or_else(|| String::from("no codec yet"), |codec| codec.to_string()),
        numbers.codec_changes,
        numbers.decode_failed,
        numbers.before_first_idr,
        numbers.lost_before_first_idr,
        reassembly.repaired,
        reassembly
            .dropped()
            .saturating_sub(numbers.lost_before_first_idr),
        reassembly.skipped,
        reassembly.shards_received,
        reassembly.shards_lost,
        share::path_word(numbers.path),
    )
}

fn numbers_line(numbers: &WatchingNumbers) -> String {
    format!(
        "{} fps in {}, capture to display {}, decode on the GPU {}, loss {}, {} shown, {} repaired, {} dropped, {} not decoded{}",
        numbers.fps,
        numbers
            .codec
            .map_or_else(|| String::from("no codec yet"), |codec| codec.to_string()),
        numbers.capture_to_display.map_or_else(
            || String::from("not measured"),
            |l| format!(
                "median {:.2} ms, p95 {:.2} ms{}",
                l.median_ms,
                l.p95_ms,
                if l.about { " (about)" } else { "" }
            )
        ),
        numbers
            .decode_ms
            .map_or_else(|| String::from("not measured"), |ms| format!("{ms:.2} ms")),
        numbers
            .video_loss_pct
            .map_or_else(|| String::from("not measured"), |pct| format!("{pct:.1}%")),
        numbers.shown,
        numbers.repaired,
        numbers.dropped,
        numbers.decode_failed,
        numbers.knob.map_or_else(String::new, |knob| format!(
            ", the loss knob at {}% (seed {}) dropped {} packets",
            knob.percent, knob.seed, knob.dropped
        )),
    )
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

// Each second's numbers for the stats panel.
struct Tally {
    // Capture to display, one second each, newest last.
    latency: VecDeque<Vec<f32>>,
    knob: Option<LossKnob>,
}

impl Tally {
    fn new(knob: Option<LossKnob>) -> Tally {
        Tally {
            latency: VecDeque::with_capacity(LATENCY_SECONDS),
            knob,
        }
    }

    fn second(
        &mut self,
        second: Second<'_>,
        about: bool,
        knob_dropped: u64,
        fps: u8,
        one_way_ms: Option<f32>,
    ) -> WatchingNumbers {
        let Second {
            loss,
            mut window,
            numbers,
            ..
        } = second;
        if self.latency.len() == LATENCY_SECONDS {
            self.latency.pop_front();
        }
        self.latency
            .push_back(std::mem::take(&mut window.end_to_end_ms));
        let mut all: Vec<f32> = self.latency.iter().flatten().copied().collect();
        let capture_to_display = spread(&mut all).map(|(median_ms, p95_ms)| Latency {
            median_ms,
            p95_ms,
            about,
        });
        let decode_ms = spread(&mut window.decode_ms).map(|(median, _)| median);
        let interval = Duration::from_secs(1) / u32::from(fps.max(1));
        WatchingNumbers {
            decode_ms,
            decode_level: decode_ms
                .map_or(view::Level::Good, |ms| level(stage_level(ms, interval))),
            capture_to_display,
            capture_to_display_level: capture_to_display.map_or(view::Level::Good, |latency| {
                level(end_to_end_level(
                    latency.median_ms,
                    one_way_ms.unwrap_or(0.0),
                ))
            }),
            fps: u32::try_from(window.presented).unwrap_or(u32::MAX),
            video_loss_pct: loss,
            shown: numbers.presented,
            repaired: numbers.reassembly.repaired,
            // The frame caught half sent when watching began is no loss.
            dropped: numbers
                .reassembly
                .dropped()
                .saturating_sub(numbers.lost_before_first_idr),
            decode_failed: numbers.decode_failed,
            before_first_idr: numbers.before_first_idr,
            present_path: numbers.path.map(|path| match path {
                PresentPath::Flip => view::PresentPath::Flip,
                PresentPath::Composed => view::PresentPath::Composed,
            }),
            codec: numbers.codec.map(|codec| match codec {
                share::Codec::H264 => view::Codec::H264,
                share::Codec::Hevc => view::Codec::Hevc,
            }),
            knob: self.knob.map(|knob| KnobNumbers {
                percent: knob.percent,
                seed: knob.seed,
                dropped: knob_dropped,
            }),
        }
    }
}

// Whether this PC controls the share its viewer shows, from the view as the
// panel shows it: the viewer's strip says so, and its capture may run.
pub(crate) fn control_for(view: &View) -> Option<Control> {
    let control = &view.share.control;
    control
        .controlling
        .as_ref()
        .filter(|_| view.share.watching)
        .map(|_| Control {
            paused: control.paused,
        })
}

// The strip's numbers for the viewer, from the view as the panel shows it:
// this PC's link to the host, or on the host its worst link.
pub(crate) fn link_numbers(view: &View) -> LinkNumbers {
    let strip = &view.strip;
    let stats_level = |level: view::Level| match level {
        view::Level::Good => stats::Level::Good,
        view::Level::Warn => stats::Level::Warn,
        view::Level::Bad => stats::Level::Bad,
    };
    LinkNumbers {
        state: match strip.state {
            view::LinkState::Alone => LinkState::Alone,
            view::LinkState::Connecting => LinkState::Connecting,
            view::LinkState::Live => LinkState::Live,
            view::LinkState::Reconnecting => LinkState::Reconnecting,
            view::LinkState::Lost => LinkState::Lost,
            view::LinkState::Closed => LinkState::Closed,
        },
        rtt_ms: strip.rtt_ms,
        rtt_level: stats_level(strip.rtt_level),
        jitter_ms: strip.jitter_ms,
        jitter_level: stats_level(strip.jitter_level),
        loss_pct: strip.loss_pct,
        loss_level: stats_level(strip.loss_level),
        path: strip.path.map(|path| match path {
            view::PathWord::Lan => PathWord::Lan,
            view::PathWord::Direct => PathWord::Direct,
        }),
        trace: strip
            .trace
            .iter()
            .map(|point| match point {
                TracePoint::Rtt(ms) => TraceSample::Rtt(*ms),
                TracePoint::Lost => TraceSample::Lost,
            })
            .collect(),
        one_way_ms: one_way_ms(view),
    }
}

// Over the internet, capture to display's thresholds are later by the
// network's one-way time. Video from a friend's share passes through
// the host, so it is half the sharer's round trip to the host, which the
// host measures and every panel shows, plus half this PC's own when that
// link is not on the LAN. Each leg counts by itself: a client on the host's
// LAN watching a friend far away still waits for the friend's leg. A
// sharer on the LAN adds well under a millisecond. None when neither leg
// adds anything.
fn one_way_ms(view: &View) -> Option<f32> {
    let sharer = view.share.current.as_ref()?.key;
    let sharer_rtt = view
        .people
        .iter()
        .find(|person| person.key == sharer && !person.is_host)
        .and_then(|person| person.rtt_ms)
        .unwrap_or(0.0);
    let own_rtt = match (view.role, view.strip.path) {
        (Role::Client, Some(view::PathWord::Direct)) => view.strip.rtt_ms.unwrap_or(0.0),
        _ => 0.0,
    };
    let one_way = (sharer_rtt + own_rtt) / 2.0;
    (one_way > 0.0).then_some(one_way)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{CurrentShare, Person, ShareView, Strip};

    fn person(key: u8, is_host: bool, rtt_ms: Option<f32>) -> Person {
        Person {
            key: [key; 32],
            name: String::new(),
            fingerprint: String::new(),
            rtt_ms,
            rtt_level: view::Level::Good,
            is_you: false,
            is_host,
            joined_by_invite: false,
            reconnecting: false,
            talking: false,
            sharing: false,
        }
    }

    fn sharing(key: u8) -> ShareView {
        ShareView {
            current: Some(CurrentShare {
                key: [key; 32],
                name: String::from("Ines"),
                number: 1,
                fps: 120,
                yours: false,
                watchers: None,
            }),
            ..ShareView::default()
        }
    }

    // A client 30 ms from the host watching a friend 50 ms from it: the
    // video crosses both links, one way each.
    #[test]
    fn one_way_time_per_link() {
        let mut view = crate::testing::empty_view(Role::Client);
        view.strip = Strip {
            rtt_ms: Some(30.0),
            path: Some(view::PathWord::Direct),
            ..Strip::default()
        };
        view.people = vec![person(1, true, None), person(2, false, Some(50.0))];
        view.share = sharing(2);
        assert_eq!(one_way_ms(&view), Some(40.0));
        // The host shares: its own link is the whole way.
        view.share = sharing(1);
        assert_eq!(one_way_ms(&view), Some(15.0));
        // On the host, only the sharer's link.
        view.role = Role::Host;
        view.share = sharing(2);
        assert_eq!(one_way_ms(&view), Some(25.0));
        // A client on the host's LAN watching a friend who shares over the
        // internet still waits for the friend's leg.
        view.role = Role::Client;
        view.strip = Strip {
            rtt_ms: Some(1.0),
            path: Some(view::PathWord::Lan),
            ..Strip::default()
        };
        assert_eq!(one_way_ms(&view), Some(25.0));
        // The host's own share over the LAN: the thresholds stay as they are.
        view.share = sharing(1);
        assert_eq!(one_way_ms(&view), None);
        let numbers = link_numbers(&view);
        assert_eq!(
            (numbers.path, numbers.one_way_ms),
            (Some(PathWord::Lan), None)
        );
        // Before the host has measured the sharer, its leg is not known.
        view.people[1].rtt_ms = None;
        view.share = sharing(2);
        assert_eq!(one_way_ms(&view), None);
    }

    // The viewer hears it controls only while the view says so and this PC
    // watches the share, and hears the sharer's administrator window.
    #[test]
    fn viewer_control_follows_view() {
        let mut view = crate::testing::empty_view(Role::Client);
        view.share = sharing(2);
        view.share.watching = true;
        assert_eq!(control_for(&view), None);
        view.share.control.controlling = Some(view::Party {
            key: [2; 32],
            name: String::from("Ines"),
        });
        assert_eq!(control_for(&view), Some(Control { paused: false }));
        view.share.control.paused = true;
        assert_eq!(control_for(&view), Some(Control { paused: true }));
        view.share.watching = false;
        assert_eq!(control_for(&view), None);
    }

    #[test]
    fn capture_to_display_over_ten_seconds() {
        let mut tally = Tally::new(Some(LossKnob {
            percent: 5.0,
            seed: 7,
        }));
        let viewer = ViewerNumbers::default();
        let mut last = None;
        for n in 0..12u8 {
            let window = share::Window {
                end_to_end_ms: vec![f32::from(n) + 1.0; 10],
                decode_ms: vec![1.5, 2.0, 2.5],
                presented: 118,
                ..share::Window::default()
            };
            let second = Second {
                elapsed: Duration::from_secs(u64::from(n) + 1),
                loss: Some(5.2),
                window,
                numbers: &viewer,
            };
            last = Some(tally.second(second, n == 11, 40, 120, None));
        }
        let numbers = last.expect("a second");
        // Seconds 3 to 12 are kept: 3.0 to 12.0 ten times each.
        let latency = numbers.capture_to_display.expect("a capture to display");
        assert_eq!(
            (latency.median_ms, latency.p95_ms, latency.about),
            (7.0, 12.0, true)
        );
        assert_eq!(numbers.decode_ms, Some(2.0));
        assert_eq!(numbers.fps, 118);
        assert_eq!(numbers.capture_to_display_level, view::Level::Good);
        assert_eq!(
            numbers.knob,
            Some(KnobNumbers {
                percent: 5.0,
                seed: 7,
                dropped: 40
            })
        );
    }
}
