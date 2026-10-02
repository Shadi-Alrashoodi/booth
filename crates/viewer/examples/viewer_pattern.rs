// Opens the viewer and shows the capture crate's test pattern at
// 2560x1440 and 120 fps until the window is closed, with a pointer going
// round in a circle and made-up link numbers in the strip. F11 goes
// fullscreen; a few presents later the strip says whether DWM stepped
// aside ("flip") or drew the picture into the desktop ("composed"), and no
// word there means Windows has not said. Once a second it prints what the
// viewer measured, with "not known" for no word.
//
//   cargo run --release -p viewer --example viewer_pattern -- [--vsync] [--fps N]

use std::process::ExitCode;
use std::time::{Duration, Instant};

use capture::Pattern;
use stats::{Level, TraceSample};
use viewer::{
    Cursor, CursorKind, CursorShape, Frame, LinkState, Options, PathWord, PresentPath, Strip,
    Video, Viewer,
};

const WIDTH: u32 = 2560;
const HEIGHT: u32 = 1440;

struct Args {
    vsync: bool,
    fps: u32,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        vsync: false,
        fps: 120,
    };
    let mut words = std::env::args().skip(1);
    while let Some(word) = words.next() {
        match word.as_str() {
            "--vsync" => args.vsync = true,
            "--fps" => {
                let fps = words.next().ok_or("--fps needs a number")?;
                args.fps = fps
                    .parse()
                    .ok()
                    .filter(|&fps| (1..=1000).contains(&fps))
                    .ok_or_else(|| format!("{fps} is not a frame rate from 1 to 1000"))?;
            }
            other => return Err(format!("{other} is not an option; try --vsync or --fps N")),
        }
    }
    Ok(args)
}

// A plain white arrow with a black edge, 20x30, its hotspot at the tip.
fn arrow() -> CursorShape {
    let (width, height) = (20u32, 30u32);
    let mut bytes = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let inside = x <= y * 2 / 3 && y < 26;
            let edge = inside && (x == 0 || x == y * 2 / 3 || y == 25);
            let pixel = match (inside, edge) {
                (true, true) => [0, 0, 0, 255],
                (true, false) => [255, 255, 255, 255],
                _ => [0, 0, 0, 0],
            };
            bytes.extend_from_slice(&pixel);
        }
    }
    CursorShape {
        kind: CursorKind::Color,
        width,
        height,
        pitch: width * 4,
        hotspot_x: 0,
        hotspot_y: 0,
        bytes,
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn word(path: Option<PresentPath>) -> &'static str {
    match path {
        Some(PresentPath::Flip) => "flip",
        Some(PresentPath::Composed) => "composed",
        None => "not known",
    }
}

fn run(args: &Args) -> Result<(), String> {
    let mut options = Options::new("Booth viewer pattern", WIDTH, HEIGHT);
    options.vsync = args.vsync;
    let mut viewer = Viewer::open(&options).map_err(|err| err.to_string())?;
    println!(
        "viewer on {}, tearing {}, {}; F11 for fullscreen, close the window to stop",
        viewer.adapter(),
        if viewer.tearing() {
            "allowed"
        } else {
            "not allowed"
        },
        if args.vsync {
            "presenting on the refresh"
        } else {
            "presenting at once"
        }
    );
    let mut pattern =
        Pattern::new(viewer.device(), WIDTH, HEIGHT, args.fps).map_err(|err| err.to_string())?;
    let mut strip = Strip {
        state: LinkState::Live,
        rtt_ms: Some(3.0),
        jitter_ms: Some(0.4),
        loss_pct: Some(0.0),
        path: Some(PathWord::Lan),
        encode_ms: Some(2.5),
        decode_ms: Some(1.6),
        ..Strip::default()
    };
    let started = Instant::now();
    let mut shape = Some(arrow());
    let mut next_ping = started;
    let mut pings = 0u64;
    let mut second = Instant::now();
    let mut took = Vec::new();
    let mut last_path = None;
    loop {
        let frame = pattern.next().map_err(|err| err.to_string())?;
        // A ping every 250 ms, so the trace moves while you watch: mostly
        // flat, a bump now and then, and a lost one.
        if Instant::now() >= next_ping {
            next_ping += Duration::from_millis(250);
            pings += 1;
            let sample = match pings % 40 {
                13 => TraceSample::Lost,
                27 => TraceSample::Rtt(55.0),
                _ => TraceSample::Rtt(2.5 + (pings % 7) as f32 * 0.3),
            };
            strip.trace.push(sample);
            if strip.trace.len() > 120 {
                strip.trace.remove(0);
            }
            if let TraceSample::Rtt(ms) = sample {
                strip.rtt_ms = Some(ms);
                strip.rtt_level = if ms < 40.0 { Level::Good } else { Level::Warn };
            }
        }
        let angle = started.elapsed().as_secs_f32() * 1.5;
        let cursor = Cursor {
            x: (WIDTH as f32 / 2.0 + angle.cos() * 400.0) as i32,
            y: (HEIGHT as f32 / 2.0 + angle.sin() * 300.0) as i32,
            visible: true,
            scale: 1.0,
            shape: shape.take(),
        };
        let result = viewer.present(&Frame {
            video: Some(Video {
                texture: &frame.texture,
                index: 0,
                width: frame.width,
                height: frame.height,
            }),
            cursor: Some(&cursor),
            strip: &strip,
        });
        // From the pattern making the picture to the present returning:
        // what the strip's e2e would be with no network in between.
        strip.end_to_end_ms = Some(ms(frame.present.elapsed()) as f32);
        strip.end_to_end_level = Level::Good;
        let presented = match result {
            Ok(presented) => presented,
            Err(err) if viewer.closed() => {
                println!("window closed ({err})");
                return Ok(());
            }
            Err(err) => return Err(err.to_string()),
        };
        took.push(presented.took);
        if presented.path != last_path {
            println!(
                "present path: {}{}",
                word(presented.path),
                if viewer.fullscreen() {
                    ", fullscreen"
                } else {
                    ", windowed"
                }
            );
            last_path = presented.path;
        }
        if second.elapsed() >= Duration::from_secs(1) {
            took.sort();
            let at = |share: f64| ms(took[((took.len() - 1) as f64 * share).round() as usize]);
            println!(
                "{} frames, present took median {:.3} ms, 95th {:.3} ms; buffers {:?}; {}",
                took.len(),
                at(0.5),
                at(0.95),
                viewer.buffer_size(),
                word(presented.path)
            );
            took.clear();
            second = Instant::now();
        }
        if viewer.closed() {
            println!("window closed");
            return Ok(());
        }
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
