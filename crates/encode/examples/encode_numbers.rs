// Encodes the test pattern for 10 s on P1, then on P4, paced like a
// real share, and prints the numbers an encoder is judged by.
// Nothing is captured: every frame is made up.
//
//   cargo run --release --example encode_numbers
//   cargo run --release --example encode_numbers -- hardware
//   cargo run --release --example encode_numbers -- nvenc hevc
//
// With no word it opens what a share would (NVENC on this PC) in H.264;
// nvenc, hardware or software forces that encoder, and h264 or hevc the
// codec. Every encoder runs at 2560x1440 and 120 fps except the software
// one, which runs at the size and rate it takes instead.

#[path = "../tests/common/mod.rs"]
mod common;

use std::thread;
use std::time::{Duration, Instant};

use encode::{Codec, EncodeError, Encoder, Frame, Kind, Preset, Settings};

const WIDTH: u32 = 2560;
const HEIGHT: u32 = 1440;
const FPS: u32 = 120;
const SECONDS: u32 = 10;

fn main() {
    let mut forced = None;
    let mut codec = Codec::H264;
    for word in std::env::args().skip(1) {
        if let Ok(kind) = word.parse::<Kind>() {
            forced = Some(kind);
        } else if let Ok(chosen) = word.parse::<Codec>() {
            codec = chosen;
        } else {
            eprintln!(
                "{word:?} is neither an encoder (nvenc, hardware, software) nor a codec (h264, hevc)"
            );
            std::process::exit(2);
        }
    }
    let Some(gpu) = common::nvidia() else {
        std::process::exit(1);
    };
    let (width, height, fps) = match forced {
        Some(Kind::MfSoftware) => encode::software_fit(WIDTH, HEIGHT, FPS)
            .map_or((WIDTH, HEIGHT, FPS), |fit| (fit.width, fit.height, fit.fps)),
        _ => (WIDTH, HEIGHT, FPS),
    };
    println!(
        "{} at {width}x{height}, {fps} fps, {SECONDS} s per preset",
        gpu.name
    );

    for preset in [Preset::P1, Preset::P4] {
        let settings = Settings {
            bitrate: 15_000_000,
            preset,
        };
        let opened = match forced {
            Some(kind) => {
                encode::open_kind_codec(kind, codec, &gpu.device, width, height, fps, &settings)
            }
            None => encode::open_codec(codec, &gpu.device, width, height, fps, &settings),
        };
        let mut encoder = opened.unwrap_or_else(|e: EncodeError| {
            eprintln!("{e}");
            std::process::exit(1);
        });
        run(&mut *encoder, &gpu, &settings, width, height, fps);
    }
}

fn run(
    encoder: &mut dyn Encoder,
    gpu: &common::Gpu,
    settings: &Settings,
    width: u32,
    height: u32,
    fps: u32,
) {
    let mut frames = common::Frames::new(gpu, width, height);
    let interval = Duration::from_secs(1) / fps;
    let count = u64::from(fps * SECONDS);

    let mut ms = Vec::new();
    let mut sizes = Vec::new();
    let mut idr_sizes = Vec::new();
    let mut late = 0;
    let start = Instant::now();
    for i in 0..count {
        let due = start + interval * i as u32;
        let now = Instant::now();
        if now < due {
            thread::sleep(due - now);
        } else if now > due + interval {
            late += 1;
        }
        let texture = frames.frame(i);
        let unit = encoder
            .encode(&Frame {
                texture: &texture,
                index: i,
                force_idr: false,
            })
            .unwrap_or_else(|e| panic!("frame {i}: {e}"));
        ms.push(unit.encode_time().as_secs_f64() * 1000.0);
        if unit.idr {
            idr_sizes.push(unit.len());
        } else {
            sizes.push(unit.len());
        }
    }

    let budget = settings.bitrate as f64 / f64::from(fps) / 8.0;
    let total: usize = sizes.iter().sum::<usize>() + idr_sizes.iter().sum::<usize>();
    let (median, p95, max) = common::spread(&ms);
    let largest = sizes.iter().copied().max().unwrap_or(0);
    let over_2x = sizes.iter().filter(|&&s| s as f64 > 2.0 * budget).count();
    println!("{}:", encoder.name());
    if !encoder.notes().is_empty() {
        println!("  {}", encoder.notes());
    }
    println!("  encode ms: median {median:.2}, p95 {p95:.2}, max {max:.2}");
    println!(
        "  bitrate: {:.2} Mbit/s against {:.2}",
        total as f64 * 8.0 / f64::from(SECONDS) / 1e6,
        settings.bitrate as f64 / 1e6
    );
    println!(
        "  IDR: {} bytes, {:.2} frames' worth ({budget:.0} bytes)",
        idr_sizes.first().copied().unwrap_or(0),
        idr_sizes.first().copied().unwrap_or(0) as f64 / budget
    );
    println!(
        "  largest other frame: {largest} bytes, {:.2} frames' worth; {over_2x} of {} over twice",
        largest as f64 / budget,
        sizes.len()
    );
    println!("  frames more than one interval late: {late}");
}
