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
// a LAN path to the room, so the share sends LAN-sized packets and never
// steps down for a low rate, as it would over the internet. A stall is a wait
// of more than 100 ms between two frames shown.
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
// source, clean or links to one half.

#[path = "../tests/common/mod.rs"]
mod common;

use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::link::{Link, Shape};
use common::{Member, config, invite_to, loopback};
use keys::Identity;
use room::view::{LinkState, OwnShare, PresentPath, SharingNumbers, View, WatchingNumbers};
use room::{Show, Timers, VideoConfig, VideoSource};

const FFMPEG: [&str; 2] = ["avcodec-62.dll", "avutil-60.dll"];
const WAIT: Duration = Duration::from_secs(15);
const SETTLE: Duration = Duration::from_secs(5);
const MEASURED: u64 = 30;
const MEASURED_LONG: u64 = 60;
// How often the views are read for a new second of numbers.
const LOOK: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Pattern(u32, u32),
    Desktop,
}

impl Source {
    fn name(self) -> String {
        match self {
            Source::Pattern(width, height) => format!("pattern {width}x{height}"),
            Source::Desktop => String::from("desktop"),
        }
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
    }
    if wants(halves, "links") {
        for source in all
            .into_iter()
            .filter(|&s| matches!(s, Source::Pattern(_, 1440) | Source::Desktop))
        {
            for (link, shape, secs) in links() {
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

// The pattern moving on the whole monitor at its refresh, for a desktop case.
fn drawing() -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|err| format!("no path to this example: {err}"))?;
    let pattern = exe.with_file_name("viewer_pattern.exe");
    Command::new(&pattern)
        .args(["--fps", "240", "--fullscreen"])
        .stdout(Stdio::null())
        .spawn()
        .map_err(|err| {
            format!(
                "could not start {}: {err}; build it with cargo build --release -p viewer --example viewer_pattern",
                pattern.display()
            )
        })
}

struct Pair {
    // Field order is drop order: the friend leaves before the host.
    friend: Member,
    host: Member,
    link: Link,
}

fn open(source: Source, fps: u8) -> Pair {
    let video = |show| VideoConfig {
        source: match source {
            Source::Pattern(width, height) => VideoSource::Pattern {
                width,
                height,
                busy: false,
            },
            Source::Desktop => VideoSource::Screen,
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
        Source::Desktop => Show::Hidden,
    });
    let friend = Member::join_with(
        friend_config,
        Arc::new(Identity::generate()),
        invite_to(&host, link.addr),
    );
    friend.wait_for(WAIT, "the friend in the room", |v| {
        v.strip.state == LinkState::Live
    });
    host.room().share(fps, None);
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

fn measure(pair: &Pair, secs: u64) -> Vec<Second> {
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
    seconds
}

fn print_case(case: &Case, seconds: &[Second]) {
    let (Some(first), Some(last)) = (seconds.first(), seconds.last()) else {
        println!("  {}: the friend watched nothing", case.link);
        return;
    };
    let (a, b) = (&first.watching, &last.watching);
    let took = last.at.duration_since(first.at).as_secs_f64();
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
    println!(
        "  {}: {fps:.1} fps shown in {took:.0} s, capture to display {latency} (median/p95, worst 10 s), slowest {slowest:.1} ms, {} stalls, longest wait {longest:.0} ms{}, lost for good {}, not decoded {}, repaired {}; {kbps} kbit/s, {idrs} IDRs, {backoffs} backoffs, rate at the end {} kbit/s",
        case.link,
        b.stalls - a.stalls,
        if b.fps == 0 { ", ended dark" } else { "" },
        b.dropped - a.dropped,
        b.decode_failed - a.decode_failed,
        b.repaired - a.repaired,
        shared.last().map_or(0, |s| s.rate_kbps),
    );
}

fn run(case: &Case) {
    let what = format!("{} at {} fps", case.source.name(), case.fps);
    let mut pattern = None;
    if case.source == Source::Desktop {
        match drawing() {
            Ok(child) => pattern = Some(child),
            Err(err) => {
                println!("{what}, {}: skipped, {err}", case.link);
                return;
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    let pair = open(case.source, case.fps);
    thread::sleep(SETTLE);
    let host = pair.host.view();
    let friend = pair.friend.view();
    if let (Some(s), Some(w)) = (&host.numbers.sharing, &friend.numbers.watching) {
        println!(
            "{what}, {}: shared {}x{} at {} fps with {}, present {}{}",
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
            if case.source == Source::Desktop {
                " to a hidden viewer"
            } else {
                ""
            },
        );
    }
    pair.link.shape(case.shape, case.shape);
    let before = pair.link.tallies();
    let seconds = measure(&pair, case.secs);
    let after = pair.link.tallies();
    print_case(case, &seconds);
    if case.link != "clean" {
        println!(
            "    the link lost {:.1}% toward the friend, {:.1}% toward the host",
            after.0.since(&before.0).lost_pct(),
            after.1.since(&before.1).lost_pct()
        );
    }
    drop(pair);
    if let Some(mut child) = pattern {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let mut sources = Vec::new();
    let mut halves = Vec::new();
    for word in &words {
        match word.as_str() {
            "pattern" | "desktop" => sources.push(word.as_str()),
            "clean" | "links" => halves.push(word.as_str()),
            _ => {
                eprintln!("{word:?} is none of pattern, desktop, clean or links");
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
