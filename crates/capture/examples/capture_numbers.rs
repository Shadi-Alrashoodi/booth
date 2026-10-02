// Captures a monitor, or the test pattern, and prints numbers once a
// second. It never shows, saves or reads back a picture.
//
//   cargo run --release -p capture --example capture_numbers -- [index] [--pattern WxH] [--seconds N]

use std::process::ExitCode;
use std::time::{Duration, Instant};

use capture::{Capture, Monitor, Next, Options, Pattern, Rotation};

struct Args {
    index: Option<usize>,
    pattern: Option<(u32, u32)>,
    seconds: u64,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        index: None,
        pattern: None,
        seconds: 10,
    };
    let mut words = std::env::args().skip(1);
    while let Some(word) = words.next() {
        match word.as_str() {
            "--pattern" => {
                let size = words
                    .next()
                    .ok_or("--pattern needs a size, as in 3840x2160")?;
                let (width, height) = size
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .filter(|&(w, h): &(u32, u32)| w >= 16 && h >= 16 && w <= 16384 && h <= 16384)
                    .ok_or_else(|| format!("{size} is not a size from 16x16 to 16384x16384"))?;
                args.pattern = Some((width, height));
            }
            "--seconds" => {
                let seconds = words.next().ok_or("--seconds needs a number")?;
                args.seconds = seconds
                    .parse()
                    .ok()
                    .filter(|&s| s > 0)
                    .ok_or_else(|| format!("{seconds} is not a whole number of seconds"))?;
            }
            index => {
                args.index = Some(
                    index
                        .parse()
                        .map_err(|_| format!("{index} is not a monitor number or an option"))?,
                );
            }
        }
    }
    Ok(args)
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// Median and 95th percentile, in ms.
fn spread(times: &mut [Duration]) -> String {
    if times.is_empty() {
        return "-".to_string();
    }
    times.sort();
    let at = |share: f64| ms(times[((times.len() - 1) as f64 * share).round() as usize]);
    format!("{:.3}/{:.3}", at(0.5), at(0.95))
}

#[derive(Default)]
struct Second {
    arrived: u32,
    out: u32,
    skipped: u32,
    cursors: u32,
    idle: u32,
    waits: Vec<Duration>,
    converts: Vec<Duration>,
    gpu: Vec<Duration>,
    to_out: Vec<Duration>,
    held_to_out: Vec<Duration>,
    size: (u32, u32),
}

impl Second {
    fn add(&mut self, second: Second) {
        self.arrived += second.arrived;
        self.out += second.out;
        self.skipped += second.skipped;
        self.cursors += second.cursors;
        self.idle += second.idle;
        self.waits.extend(second.waits);
        self.converts.extend(second.converts);
        self.gpu.extend(second.gpu);
        self.to_out.extend(second.to_out);
        self.held_to_out.extend(second.held_to_out);
        self.size = second.size;
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(problem) => {
            eprintln!("capture_numbers: {problem}");
            eprintln!("usage: capture_numbers [index] [--pattern WxH] [--seconds N]");
            return ExitCode::from(2);
        }
    };
    if let Err(err) = capture::make_process_dpi_aware() {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    let monitors = match capture::monitors() {
        Ok(monitors) => monitors,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    for (index, monitor) in monitors.iter().enumerate() {
        println!("{index}: {monitor}");
    }
    let chosen = match args.index {
        Some(index) => monitors.get(index),
        None => monitors.iter().find(|m| m.primary).or(monitors.first()),
    };
    let result = match (args.pattern, chosen) {
        (Some(size), _) => run_pattern(chosen, size, args.seconds),
        (None, Some(monitor)) => run_capture(monitor, args.seconds),
        (None, None) if monitors.is_empty() => {
            Err("no monitor is attached to the desktop".to_string())
        }
        (None, None) => Err(format!(
            "there is no monitor {}: the list above goes from 0 to {}",
            args.index.unwrap_or_default(),
            monitors.len() - 1
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run_capture(monitor: &Monitor, seconds: u64) -> Result<(), String> {
    println!("capturing {} for {seconds} s", monitor.name);
    let opened = Instant::now();
    let mut capture = Capture::open(monitor, Options::default()).map_err(|err| err.to_string())?;
    println!(
        "opened in {:.1} ms, shader compiled in {:.1} ms",
        ms(opened.elapsed()),
        ms(capture.compile_time())
    );
    println!(
        "second  arrived  out  skipped  pointer  idle  present>acquired  acquired>converted  gpu convert  present>out  held  held present>out  size  (ms as median/95th)"
    );
    let start = Instant::now();
    let mut last_gpu = None;
    let mut all = Second::default();
    for second in 1..=seconds {
        let mut now = Second::default();
        let end = start + Duration::from_secs(second);
        while Instant::now() < end {
            let next = capture.next().map_err(|err| err.to_string())?;
            let out = Instant::now();
            match next {
                Next::Frame(frame) => {
                    now.out += 1;
                    // A frame the fps cap held went out on a later call, when its
                    // time came, rather than as soon as it was converted.
                    if out - frame.converted > Duration::from_millis(1) {
                        now.held_to_out.push(out - frame.present);
                    } else {
                        now.to_out.push(out - frame.present);
                    }
                    now.arrived += 1 + frame.skipped;
                    now.skipped += frame.skipped;
                    now.waits.push(frame.acquired - frame.present);
                    now.converts.push(frame.converted - frame.acquired);
                    if let Some(time) = frame.gpu_convert
                        && frame.gpu_convert != last_gpu
                    {
                        now.gpu.push(time);
                        last_gpu = frame.gpu_convert;
                    }
                    now.size = (frame.width, frame.height);
                    if frame.cursor.is_some() {
                        now.cursors += 1;
                    }
                }
                Next::Cursor(_) => now.cursors += 1,
                Next::Idle => now.idle += 1,
                Next::Paused(reason) => println!("paused: {reason}"),
            }
        }
        println!(
            "{second:>6}  {:>7}  {:>3}  {:>7}  {:>7}  {:>4}  {:>16}  {:>18}  {:>11}  {:>11}  {:>4}  {:>16}  {}x{}",
            now.arrived,
            now.out,
            now.skipped,
            now.cursors,
            now.idle,
            spread(&mut now.waits),
            spread(&mut now.converts),
            spread(&mut now.gpu),
            spread(&mut now.to_out),
            now.held_to_out.len(),
            spread(&mut now.held_to_out),
            now.size.0,
            now.size.1
        );
        all.add(now);
    }
    println!(
        "   all  {:>7}  {:>3}  {:>7}  {:>7}  {:>4}  {:>16}  {:>18}  {:>11}  {:>11}  {:>4}  {:>16}",
        all.arrived,
        all.out,
        all.skipped,
        all.cursors,
        all.idle,
        spread(&mut all.waits),
        spread(&mut all.converts),
        spread(&mut all.gpu),
        spread(&mut all.to_out),
        all.held_to_out.len(),
        spread(&mut all.held_to_out)
    );
    Ok(())
}

fn run_pattern(
    monitor: Option<&Monitor>,
    (width, height): (u32, u32),
    seconds: u64,
) -> Result<(), String> {
    let adapters = capture::adapters().map_err(|err| err.to_string())?;
    let adapter = monitor
        .map(|m| m.adapter.clone())
        .or_else(|| adapters.first().cloned())
        .ok_or("this PC has no hardware graphics adapter")?;
    let device = capture::device_on(&adapter).map_err(|err| err.to_string())?;
    let options = Options::default();
    let mut pattern = Pattern::with_source(&device, width, height, Rotation::Identity, options)
        .map_err(|err| err.to_string())?;
    println!(
        "pattern {width}x{height} to {}x{} at {} fps on {} for {seconds} s, shader compiled in {:.1} ms",
        pattern.width(),
        pattern.height(),
        options.max_fps,
        adapter.description,
        ms(pattern.compile_time())
    );
    println!("second  frames  gpu convert  size  (ms as median/95th)");
    let start = Instant::now();
    let mut last_gpu = None;
    let mut all = Second::default();
    for second in 1..=seconds {
        let mut now = Second::default();
        let end = start + Duration::from_secs(second);
        while Instant::now() < end {
            let frame = pattern.next().map_err(|err| err.to_string())?;
            now.out += 1;
            if let Some(time) = frame.gpu_convert
                && frame.gpu_convert != last_gpu
            {
                now.gpu.push(time);
                last_gpu = frame.gpu_convert;
            }
            now.size = (frame.width, frame.height);
        }
        println!(
            "{second:>6}  {:>6}  {:>11}  {}x{}",
            now.out,
            spread(&mut now.gpu),
            now.size.0,
            now.size.1
        );
        all.add(now);
    }
    println!("   all  {:>6}  {:>11}", all.out, spread(&mut all.gpu));
    Ok(())
}
