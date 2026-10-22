// Shares through a real room on this PC and prints what the friend saw, the
// numbers a share is judged by: frames shown a second, capture to display
// (median and 95th percentile over each 10 s, the worst 10 s given, and the
// slowest frame), stalls and the longest wait between two frames, frames lost
// for good, and the share's bitrate and IDRs. The host shares and the friend
// watches through the link of tests\common\link.rs, which each case leaves
// clean or makes lossy, jittery or narrow, both ways.
//
// Each case is a room of its own: 5 s from the first frame shown to settle,
// then 30 s measured, 60 s for the bursts and the narrowest link. Loopback is
// a LAN path to the room, so the share sends LAN-sized packets and steps down
// for a low rate only after backoffs, not for the rate allowed as it would
// over the internet. A stall is a wait of more than 100 ms between two frames
// shown.
//
// The pattern is made on the GPU and nothing of the screen is captured. The
// desktop cases capture the primary monitor while viewer_pattern moves on all
// of it, so the desktop is busy everywhere; the friend's viewer is hidden then
// so it is not captured itself, and its capture to display ends at a present
// to a window nobody sees. Build viewer_pattern first.
//
//   cargo build --release -p viewer --example viewer_pattern
//   cargo run --release -p room --example share_numbers
//   cargo run --release -p room --example share_numbers -- pattern clean
//
// With no words it runs every clean case, then the hard links at 1440p and
// 120 fps on the pattern and on the desktop. pattern or desktop keeps to one
// source, clean or links to one half. region shares regions of the desktop
// instead, in the middle of the primary monitor: 1920x1080 and 2560x1440 at
// their own size, and all of the monitor as a region, which set beside the
// desktop case is what the crop itself costs; its links at 2560x1440.
// window shares viewer_pattern's own window, sized and placed so that what
// DWM draws of it is exactly the 2560x1440 region's rectangle, and kept above
// everything, through Windows Graphics Capture, and the same pixels as a
// region of the monitor through Desktop Duplication, clean and on the links;
// then the window while it is dragged in and out over a second every 4 s,
// and while it steps between 2560x1440 and 1920x1080 every 250 ms, which
// count the new sizes the share sent. A case says so when the window moved
// or the watch ended while it was measured.

#[path = "../tests/common/mod.rs"]
mod common;

use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::link::{Link, Shape};
use common::{Member, config, invite_to, loopback};
use keys::Identity;
use room::view::{LinkState, OwnShare, PresentPath, SharingNumbers, View, WatchingNumbers};
use room::{Show, Timers, VideoConfig, VideoSource, WindowId};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOZORDER,
    SetWindowPos, WS_EX_TOPMOST,
};

const FFMPEG: [&str; 2] = ["avcodec-62.dll", "avutil-60.dll"];
const WAIT: Duration = Duration::from_secs(15);
const SETTLE: Duration = Duration::from_secs(5);
const MEASURED: u64 = 30;
const MEASURED_LONG: u64 = 60;
// How often the views are read for a new second of numbers.
const LOOK: Duration = Duration::from_millis(50);
// What DWM draws of the pattern window in the window cases, on the same
// rectangle as the 2560x1440 region: a size neither path scales or rounds.
const WINDOW_AT: (u32, u32) = (2560, 1440);
// The stepped case's other size, and how often it changes.
const STEPPED_TO: (u32, u32) = (1920, 1080);
const STEP_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Pattern(u32, u32),
    Desktop,
    // A region of the desktop this size, or all of it as a region.
    Region(Option<(u32, u32)>),
    Window,
    WindowAsRegion,
    WindowDragged,
    WindowStepped,
}

impl Source {
    fn name(self) -> String {
        match self {
            Source::Pattern(width, height) => format!("pattern {width}x{height}"),
            Source::Desktop => String::from("desktop"),
            Source::Region(Some((width, height))) => format!("desktop region {width}x{height}"),
            Source::Region(None) => String::from("desktop region of all of it"),
            Source::Window => String::from("pattern window"),
            Source::WindowAsRegion => String::from("pattern window as a desktop region"),
            Source::WindowDragged => String::from("pattern window dragged to new sizes"),
            Source::WindowStepped => String::from("pattern window stepping between two sizes"),
        }
    }

    fn window(self) -> bool {
        matches!(
            self,
            Source::Window | Source::WindowAsRegion | Source::WindowDragged | Source::WindowStepped
        )
    }

    fn resized(self) -> bool {
        matches!(self, Source::WindowDragged | Source::WindowStepped)
    }
}

struct Case {
    source: Source,
    fps: u8,
    link: &'static str,
    shape: Shape,
    secs: u64,
}

fn links() -> Vec<(&'static str, Shape, u64)> {
    let narrow = |kbps| Shape {
        capacity_kbps: Some(kbps),
        ..Shape::default()
    };
    vec![
        ("5% loss", Shape::loss(5), MEASURED),
        ("20% loss", Shape::loss(20), MEASURED),
        (
            "bursts of 100 to 500 ms every 2 to 6 s and 2% loss",
            Shape {
                bursts: true,
                loss_pct: 2,
                ..Shape::default()
            },
            MEASURED_LONG,
        ),
        (
            "0 to 40 ms of jitter",
            Shape {
                jitter_ms: 40,
                ..Shape::default()
            },
            MEASURED,
        ),
        ("10 Mbit/s", narrow(10_000), MEASURED),
        ("3 Mbit/s", narrow(3_000), MEASURED_LONG),
    ]
}

fn cases(sources: &[&str], halves: &[&str]) -> Vec<Case> {
    let wants = |list: &[&str], word: &str| list.is_empty() || list.contains(&word);
    let mut picked = Vec::new();
    let mut all = Vec::new();
    if wants(sources, "pattern") {
        all.extend([1080, 1440, 2160].map(|h| Source::Pattern(h * 16 / 9, h)));
    }
    if wants(sources, "desktop") {
        all.push(Source::Desktop);
    }
    // Only when asked for: they stand beside the desktop's own numbers.
    if sources.contains(&"region") {
        all.extend([
            Source::Region(Some((1920, 1080))),
            Source::Region(Some((2560, 1440))),
            Source::Region(None),
        ]);
    }
    if sources.contains(&"window") {
        all.extend([Source::Window, Source::WindowAsRegion]);
    }
    if wants(halves, "clean") {
        for &source in &all {
            for fps in [60, 120] {
                picked.push(Case {
                    source,
                    fps,
                    link: "clean",
                    shape: Shape::default(),
                    secs: MEASURED,
                });
            }
        }
        if sources.contains(&"window") {
            for source in [Source::WindowDragged, Source::WindowStepped] {
                picked.push(Case {
                    source,
                    fps: 120,
                    link: "clean",
                    shape: Shape::default(),
                    secs: MEASURED,
                });
            }
        }
    }
    if wants(halves, "links") {
        let linked: Vec<Source> = all
            .into_iter()
            .filter(|&s| {
                matches!(
                    s,
                    Source::Pattern(_, 1440)
                        | Source::Desktop
                        | Source::Region(Some((_, 1440)))
                        | Source::Window
                        | Source::WindowAsRegion
                )
            })
            .collect();
        // Each link for every source in turn, so the cases set side by side
        // ran minutes apart, not a whole source's links apart.
        for (link, shape, secs) in links() {
            for &source in &linked {
                picked.push(Case {
                    source,
                    fps: 120,
                    link,
                    shape,
                    secs,
                });
            }
        }
    }
    picked
}

fn own_share(view: &View) -> Option<u32> {
    match view.share.own {
        OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

// viewer_pattern, killed however the case ends, so a panic never leaves it
// above everything in the middle of the screen.
struct Drawing(Child);

impl Drop for Drawing {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// The pattern moving at 240 frames a second: on the whole monitor for a
// desktop case, in a window of its own for a window case.
fn drawing(fullscreen: bool) -> Result<Drawing, String> {
    let exe = std::env::current_exe().map_err(|err| format!("no path to this example: {err}"))?;
    let pattern = exe.with_file_name("viewer_pattern.exe");
    let mut args = vec!["--fps", "240"];
    if fullscreen {
        args.push("--fullscreen");
    }
    Command::new(&pattern)
        .args(args)
        .stdout(Stdio::null())
        .spawn()
        .map(Drawing)
        .map_err(|err| {
            format!(
                "could not start {}: {err}; build it with cargo build --release -p viewer --example viewer_pattern",
                pattern.display()
            )
        })
}

fn primary() -> share::Monitor {
    let monitors = share::monitors().expect("the monitors");
    monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or(monitors.first())
        .cloned()
        .expect("a monitor")
}

fn outer(hwnd: isize) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: a plain query with a live out parameter; a window that is
    // gone leaves it empty.
    let _ = unsafe { GetWindowRect(HWND(hwnd as *mut _), &mut rect) };
    rect
}

// What DWM draws of the window, the picture a window capture takes, in
// desktop pixels.
fn bounds(hwnd: isize) -> Option<RECT> {
    let mut bounds = RECT::default();
    // SAFETY: a RECT of the size passed, which this attribute fills.
    unsafe {
        DwmGetWindowAttribute(
            HWND(hwnd as *mut _),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut bounds as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(bounds)
}

// Sizes and moves the window so DWM draws it on `to`: its own rectangle also
// holds borders nobody sees, which Windows sizes for each window. topmost
// puts it above every other window as well.
fn place(hwnd: isize, to: RECT, topmost: bool) -> Result<(), String> {
    let outer = outer(hwnd);
    let drawn = bounds(hwnd).ok_or("could not read the pattern window's bounds")?;
    let rect = RECT {
        left: to.left - (drawn.left - outer.left),
        top: to.top - (drawn.top - outer.top),
        right: to.right + (outer.right - drawn.right),
        bottom: to.bottom + (outer.bottom - drawn.bottom),
    };
    let order = if topmost {
        SWP_NOACTIVATE
    } else {
        SWP_NOACTIVATE | SWP_NOZORDER
    };
    // SAFETY: a window of another process, placed without the focus
    // changing; a window that is gone makes it fail.
    unsafe {
        SetWindowPos(
            HWND(hwnd as *mut _),
            topmost.then_some(HWND_TOPMOST),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            order,
        )
    }
    .map_err(|err| format!("could not place the pattern window: {err}"))
}

// viewer_pattern's window once it shows, put above every other window and
// drawn on the 2560x1440 region's rectangle in the middle of the primary
// monitor, and that rectangle: nothing covers it, so a region of it is the
// same pixels the window capture takes, and a 120 fps case runs at the
// primary's refresh. The pattern is letterboxed into the window's client.
fn pattern_window(drawing: &Drawing) -> Result<(WindowId, RECT), String> {
    let end = Instant::now() + WAIT;
    let window = loop {
        let windows = share::top_windows().map_err(|err| err.to_string())?;
        if let Some(window) = windows.iter().find(|w| w.id.process == drawing.0.id()) {
            break window.id;
        }
        if Instant::now() >= end {
            return Err(format!(
                "viewer_pattern showed no window within {} s",
                WAIT.as_secs()
            ));
        }
        thread::sleep(LOOK);
    };
    let monitor = primary();
    let (_, region) = middle(Some(WINDOW_AT));
    let left = monitor.left + region.left as i32;
    let top = monitor.top + region.top as i32;
    let to = RECT {
        left,
        top,
        right: left + region.width as i32,
        bottom: top + region.height as i32,
    };
    place(window.hwnd, to, true)?;
    // SAFETY: a plain query; a window that is gone answers 0.
    let style = unsafe { GetWindowLongPtrW(HWND(window.hwnd as *mut _), GWL_EXSTYLE) } as u32;
    if style & WS_EX_TOPMOST.0 == 0 {
        return Err(String::from(
            "Windows did not put the pattern window above the others",
        ));
    }
    // DWM's bounds follow a moment after the move.
    let mut same = 0;
    while same < 2 {
        if Instant::now() >= end {
            return Err(format!(
                "the pattern window is drawn at {:?}, not at {to:?}",
                bounds(window.hwnd)
            ));
        }
        thread::sleep(LOOK);
        same = if bounds(window.hwnd) == Some(to) {
            same + 1
        } else {
            0
        };
    }
    Ok((window, to))
}

// What DWM draws of the window as a region of the monitor it is on.
fn window_region(window: WindowId) -> Result<(share::MonitorId, share::Region), String> {
    let monitor = share::window_monitor(window).ok_or("the pattern window is on no monitor")?;
    let bounds = bounds(window.hwnd).ok_or("could not read the pattern window's bounds")?;
    let (left, top) = (bounds.left - monitor.left, bounds.top - monitor.top);
    let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
    if left < 0
        || top < 0
        || width <= 0
        || height <= 0
        || left + width > monitor.width as i32
        || top + height > monitor.height as i32
    {
        return Err(format!(
            "the pattern window at {bounds:?} is not all on {}",
            monitor.name
        ));
    }
    let region = share::Region {
        left: left as u32,
        top: top as u32,
        width: width as u32,
        height: height as u32,
    };
    Ok((monitor.id, region))
}

// Changes the window's size while a resized case is measured, its top left
// kept, until told to stop, and puts the size back. Dragged pulls the bottom
// right corner in to seven tenths and out again over about a second, a size
// each 16 ms as a mouse drag gives them, then leaves it still for 3 s.
// Stepped switches between WINDOW_AT and STEPPED_TO every STEP_EVERY, so a
// new size always waits and the share's own spacing sets how often one goes
// out.
fn resizing(source: Source, window: WindowId, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    let hwnd = window.hwnd;
    thread::spawn(move || {
        let Some(start) = bounds(hwnd) else {
            return;
        };
        let (width, height) = (start.right - start.left, start.bottom - start.top);
        let size = |width: i32, height: i32| {
            let to = RECT {
                right: start.left + width,
                bottom: start.top + height,
                ..start
            };
            let _ = place(hwnd, to, false);
        };
        let rest = |time: Duration| {
            let until = Instant::now() + time;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                thread::sleep(LOOK.min(until.saturating_duration_since(Instant::now())));
            }
        };
        let mut stepped = false;
        while !stop.load(Ordering::Relaxed) {
            if source == Source::WindowStepped {
                stepped = !stepped;
                let (width, height) = if stepped { STEPPED_TO } else { WINDOW_AT };
                size(width as i32, height as i32);
                rest(STEP_EVERY);
                continue;
            }
            for step in (0..30).chain((0..30).rev()) {
                let tenths = 1.0 - 0.3 * f64::from(step) / 29.0;
                size(
                    (f64::from(width) * tenths) as i32,
                    (f64::from(height) * tenths) as i32,
                );
                thread::sleep(Duration::from_millis(16));
            }
            rest(Duration::from_secs(3));
        }
        size(width, height);
    })
}

// The primary monitor, and the region of it in its middle, or all of it.
fn middle(size: Option<(u32, u32)>) -> (share::MonitorId, share::Region) {
    let monitor = primary();
    let (width, height) = size.unwrap_or((monitor.width, monitor.height));
    let region = share::Region {
        left: monitor.width.saturating_sub(width) / 2,
        top: monitor.height.saturating_sub(height) / 2,
        width,
        height,
    };
    (monitor.id.clone(), region)
}

struct Pair {
    // Field order is drop order: the friend leaves before the host.
    friend: Member,
    host: Member,
    link: Link,
}

fn open(source: Source, fps: u8, window: Option<WindowId>) -> Pair {
    let video = |show| VideoConfig {
        source: match source {
            Source::Pattern(width, height) => VideoSource::Pattern {
                width,
                height,
                busy: false,
            },
            _ => VideoSource::Screen,
        },
        show,
        ..VideoConfig::default()
    };
    // The real timers: a burst of loss must not lose the friend.
    let timers = Timers::default();
    let mut host_config = config("Mara", timers);
    host_config.video = video(Show::Hidden);
    let host = Member::host_with(host_config);
    let link = Link::new(loopback(host.port()));
    let mut friend_config = config("Ana", timers);
    friend_config.video = video(match source {
        Source::Pattern(..) => Show::NoActivate,
        _ => Show::Hidden,
    });
    let friend = Member::join_with(
        friend_config,
        Arc::new(Identity::generate()),
        invite_to(&host, link.addr),
    );
    friend.wait_for(WAIT, "the friend in the room", |v| {
        v.strip.state == LinkState::Live
    });
    match (source, window) {
        (Source::Region(size), _) => {
            let (monitor, region) = middle(size);
            host.room().share_region(fps, monitor, region);
        }
        (Source::WindowAsRegion, Some(window)) => {
            let (monitor, region) = window_region(window).expect("the pattern window's region");
            host.room().share_region(fps, monitor, region);
        }
        (Source::Window | Source::WindowDragged | Source::WindowStepped, Some(window)) => {
            host.room().share_window(fps, window);
        }
        _ => host.room().share(fps, None),
    }
    let share = own_share(&host.wait_for(WAIT, "the share running", |v| {
        own_share(v).is_some() && v.share.running.is_some()
    }))
    .expect("the share");
    friend.wait_for(WAIT, "the share in the roster", |v| {
        v.share.current.as_ref().is_some_and(|c| c.number == share)
    });
    friend.room().watch(share, true);
    friend.wait_for(WAIT, "the first frame shown", |v| {
        v.numbers.watching.as_ref().is_some_and(|w| w.shown > 0)
    });
    Pair { friend, host, link }
}

// A second of the friend's numbers, the host's as they were then, and when
// it was read.
struct Second {
    at: Instant,
    watching: WatchingNumbers,
    sharing: Option<SharingNumbers>,
}

// The seconds read, and when the reading ended: a viewer that stops, or a
// picture that stops changing, adds no seconds but still counts in the time.
fn measure(pair: &Pair, secs: u64) -> (Vec<Second>, Instant) {
    let read = || {
        let view = pair.friend.view();
        view.numbers.watching.map(|watching| Second {
            at: Instant::now(),
            watching,
            sharing: pair.host.view().numbers.sharing,
        })
    };
    let mut seconds: Vec<Second> = read().into_iter().collect();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        thread::sleep(LOOK);
        let Some(now) = read() else {
            continue;
        };
        if seconds
            .last()
            .is_none_or(|last| last.watching != now.watching)
        {
            seconds.push(now);
        }
    }
    (seconds, Instant::now())
}

fn print_case(case: &Case, seconds: &[Second], until: Instant) {
    let (Some(first), Some(last)) = (seconds.first(), seconds.last()) else {
        println!("  {}: the friend watched nothing", case.link);
        return;
    };
    let (a, b) = (&first.watching, &last.watching);
    let took = until.duration_since(first.at).as_secs_f64();
    let dark = b.fps == 0 || until.duration_since(last.at) >= Duration::from_secs(1);
    let fps = if took > 0.0 {
        (b.shown - a.shown) as f64 / took
    } else {
        0.0
    };
    // Every tenth second has the 10 s before it; the worst of those stands
    // for the case.
    let latency = seconds
        .iter()
        .skip(10)
        .step_by(10)
        .filter_map(|s| s.watching.capture_to_display)
        .map(|l| (l.median_ms, l.p95_ms))
        .reduce(|worst, next| if next.1 > worst.1 { next } else { worst })
        .map_or_else(
            || String::from("not measured"),
            |(median, p95)| format!("{median:.2}/{p95:.2} ms"),
        );
    let after = &seconds[1..];
    let slowest = after
        .iter()
        .filter_map(|s| s.watching.slowest_ms)
        .reduce(f32::max)
        .unwrap_or(0.0);
    let longest = after
        .iter()
        .filter_map(|s| s.watching.longest_gap_ms)
        .reduce(f32::max)
        .unwrap_or(0.0);
    let shared: Vec<&SharingNumbers> = seconds.iter().filter_map(|s| s.sharing.as_ref()).collect();
    let kbps =
        shared.iter().map(|s| u64::from(s.video_kbps)).sum::<u64>() / (shared.len().max(1) as u64);
    let (idrs, backoffs) = match (shared.first(), shared.last()) {
        (Some(x), Some(y)) => (y.idrs - x.idrs, y.backoffs - x.backoffs),
        _ => (0, 0),
    };
    let sizes = shared
        .windows(2)
        .filter(|pair| (pair[0].width, pair[0].height) != (pair[1].width, pair[1].height))
        .count();
    println!(
        "  {}: {fps:.1} fps shown in {took:.0} s, capture to display {latency} (median/p95, worst 10 s), slowest {slowest:.1} ms, {} stalls, longest wait {longest:.0} ms{}, lost for good {}, not decoded {}, repaired {}; {kbps} kbit/s, {idrs} IDRs, {backoffs} backoffs, {sizes} new sizes, rate at the end {} kbit/s",
        case.link,
        b.stalls - a.stalls,
        if dark { ", ended dark" } else { "" },
        b.dropped - a.dropped,
        b.decode_failed - a.decode_failed,
        b.repaired - a.repaired,
        shared.last().map_or(0, |s| s.rate_kbps),
    );
}

fn run(case: &Case) {
    let what = format!("{} at {} fps", case.source.name(), case.fps);
    let mut pattern = None;
    let mut window = None;
    let desktop =
        matches!(case.source, Source::Desktop | Source::Region(_)) || case.source.window();
    if desktop {
        let drawing = match drawing(!case.source.window()) {
            Ok(drawing) => drawing,
            Err(err) => {
                println!("{what}, {}: skipped, {err}", case.link);
                return;
            }
        };
        if case.source.window() {
            match pattern_window(&drawing) {
                Ok(placed) => window = Some(placed),
                Err(err) => {
                    println!("{what}, {}: skipped, {err}", case.link);
                    return;
                }
            }
        }
        pattern = Some(drawing);
        thread::sleep(Duration::from_secs(2));
    }
    let pair = open(case.source, case.fps, window.map(|(id, _)| id));
    thread::sleep(SETTLE);
    let host = pair.host.view();
    let friend = pair.friend.view();
    if let (Some(s), Some(w)) = (&host.numbers.sharing, &friend.numbers.watching) {
        println!(
            "{what}, {}: shared {}x{} at {} fps with {}, present {}{}{}",
            case.link,
            s.width,
            s.height,
            s.fps,
            s.encoder,
            match w.present_path {
                Some(PresentPath::Flip) => "flip",
                Some(PresentPath::Composed) => "composed",
                None => "not known",
            },
            if desktop { " to a hidden viewer" } else { "" },
            window.map_or_else(String::new, |(_, at)| format!(
                ", the window drawn at {},{} {}x{}",
                at.left,
                at.top,
                at.right - at.left,
                at.bottom - at.top
            )),
        );
    }
    pair.link.shape(case.shape, case.shape);
    let stop = Arc::new(AtomicBool::new(false));
    let resizer = match window {
        Some((id, _)) if case.source.resized() => {
            Some(resizing(case.source, id, Arc::clone(&stop)))
        }
        _ => None,
    };
    let problem = friend.share.problem;
    let before = pair.link.tallies();
    let (seconds, until) = measure(&pair, case.secs);
    let after = pair.link.tallies();
    stop.store(true, Ordering::Relaxed);
    if let Some(resizer) = resizer {
        let _ = resizer.join();
    }
    print_case(case, &seconds, until);
    if case.link != "clean" {
        println!(
            "    the link lost {:.1}% toward the friend, {:.1}% toward the host",
            after.0.since(&before.0).lost_pct(),
            after.1.since(&before.1).lost_pct()
        );
    }
    let view = pair.friend.view();
    if !view.share.watching || view.share.problem != problem {
        println!(
            "    the watch ended or went wrong: {}",
            view.share.problem.as_deref().unwrap_or("no line says why")
        );
    }
    if let Some((id, at)) = window {
        // A resized case put the size back just now, and DWM follows a
        // moment later.
        let end = Instant::now() + Duration::from_secs(1);
        while bounds(id.hwnd) != Some(at) && Instant::now() < end {
            thread::sleep(LOOK);
        }
        if bounds(id.hwnd) != Some(at) {
            println!(
                "    the pattern window was moved to {:?} while measured, so these numbers are not of it",
                bounds(id.hwnd)
            );
        }
    }
    if let Some(drawing) = &mut pattern
        && drawing.0.try_wait().ok().flatten().is_some()
    {
        println!("    viewer_pattern ended while measured");
    }
    drop(pair);
}

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let mut sources = Vec::new();
    let mut halves = Vec::new();
    for word in &words {
        match word.as_str() {
            "pattern" | "desktop" | "region" | "window" => sources.push(word.as_str()),
            "clean" | "links" => halves.push(word.as_str()),
            _ => {
                eprintln!("{word:?} is none of pattern, desktop, region, window, clean or links");
                return ExitCode::from(2);
            }
        }
    }
    let exe = std::env::current_exe().expect("this example's own path");
    let folder = exe.parent().expect("this example's folder");
    if let Some(missing) = FFMPEG.iter().find(|dll| !folder.join(dll).is_file()) {
        eprintln!(
            "{missing} is not in {}; build FFmpeg with powershell -ExecutionPolicy Bypass -File tools\\build-ffmpeg.ps1 and build again",
            folder.display()
        );
        return ExitCode::FAILURE;
    }
    if let Err(err) = share::make_process_dpi_aware() {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    for case in cases(&sources, &halves) {
        run(&case);
    }
    ExitCode::SUCCESS
}
