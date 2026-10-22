// On this PC's own devices. Nothing here records sound or makes any: the
// devices are listed, the output plays silence for a fraction of a second,
// and the microphone is asked for its period and format, and opened once
// for a moment to see what Windows does to it, what it hears going nowhere.
// PC sound is opened for half a second, and of what other programs play only
// the packets' sizes and times are kept.

use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use voice::audio::{
    self, AudioError, Capture, Choice, Direction, Echo, Event, PC_SOUND_BUILD, PcSound, Render,
};

// The tests that open a stream take turns: opening a Bluetooth headset's
// microphone switches it to hands-free, and its output stream stops while
// it does.
static ONE_STREAM_AT_A_TIME: Mutex<()> = Mutex::new(());

#[test]
fn lists_the_devices_on_this_pc() {
    let lists = audio::lists().expect("list the devices");
    for (what, list) in [("inputs", &lists.inputs), ("outputs", &lists.outputs)] {
        println!("{what}: {}", list.devices.len());
        for device in &list.devices {
            let default = if device.is_default { "  (default)" } else { "" };
            println!("  {}{default}\n    {}", device.name, device.id);
        }
        // Windows names at most one default per role.
        assert!(list.devices.iter().filter(|d| d.is_default).count() <= 1);
        if let Some(default) = list.default_device() {
            assert!(list.find(&default.id).is_some());
        }
    }
}

// What the settings screen uses: its own thread, the lists read once, and
// Windows' notifications registered until it is dropped.
#[test]
fn the_watch_reads_the_same_lists_and_stops_quickly() {
    let (tx, rx) = mpsc::channel();
    let watch = audio::Watch::start(move || {
        let _ = tx.send(());
    })
    .expect("start the watch");
    rx.recv_timeout(Duration::from_secs(3))
        .expect("the first reading within 3 s");
    let watched = watch.lists().expect("a reading").expect("readable lists");
    let direct = audio::lists().expect("list the devices");
    assert_eq!(watched, direct);
    let stopping = Instant::now();
    drop(watch);
    let took = stopping.elapsed();
    println!("watch stopped in {:.2} ms", took.as_secs_f64() * 1000.0);
    assert!(took < Duration::from_millis(100));
}

// All zeros, for at most 300 ms: long enough to see the period and the queue
// settle, far under the one second allowed.
#[test]
fn the_default_output_plays_silence_and_reports_its_latency() {
    let _turn = ONE_STREAM_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tx, rx) = mpsc::channel();
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&sizes);
    let render = Render::start(
        Choice::Default,
        move |samples: &mut [f32]| {
            samples.fill(0.0);
            seen.lock().unwrap().push(samples.len());
        },
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("start the render thread");
    let info = match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Event::Opened(info)) => info,
        Ok(Event::Failed(AudioError::NoDevice(_))) => {
            println!("no output device on this PC; skipped");
            return;
        }
        other => panic!("the default output did not open: {other:?}"),
    };
    std::thread::sleep(Duration::from_millis(300));
    let latency = render.latency().expect("still open");
    let queued = render.queued().expect("still open");
    let underruns = render.underruns();
    let stopping = Instant::now();
    render.stop();
    let stop_time = stopping.elapsed();

    let sizes = sizes.lock().unwrap();
    let frames: usize = sizes.iter().sum();
    println!("device: {}", info.device);
    println!("engine format: {}", info.engine);
    println!("stream format: {}", info.format);
    println!(
        "period: {:.2} ms, small period: {}, resampled: {}, pro audio: {}",
        info.period_ms(),
        info.small_period,
        info.resampled,
        info.pro_audio
    );
    println!(
        "buffer: {} frames ({:.2} ms), stream latency {:.2} ms",
        info.buffer_frames,
        f64::from(info.buffer_frames) / 48.0,
        info.stream_latency.as_secs_f64() * 1000.0
    );
    println!(
        "queued ahead of a write: {:.2} ms, render latency (period + stream + queued): {:.2} ms",
        queued.as_secs_f64() * 1000.0,
        latency.as_secs_f64() * 1000.0
    );
    println!(
        "callbacks: {}, frames: {frames}, sizes {:?}..., underruns {underruns}, stop took {:.2} ms",
        sizes.len(),
        &sizes[..sizes.len().min(8)],
        stop_time.as_secs_f64() * 1000.0
    );
    assert_eq!(info.format.rate, 48_000);
    assert!(info.period > Duration::ZERO && info.period <= Duration::from_millis(25));
    assert!(!sizes.is_empty(), "the device never asked for samples");
    assert!(stop_time < Duration::from_millis(100));
}

#[test]
fn default_microphone_probe() {
    let probe = match audio::probe(Direction::Input, &Choice::Default) {
        Ok(probe) => probe,
        Err(AudioError::NoDevice(_)) => {
            println!("no microphone on this PC; skipped");
            return;
        }
        Err(err) => panic!("{err}"),
    };
    println!("device: {}", probe.device);
    println!("engine format: {}", probe.engine);
    println!("bluetooth hands-free: {}", probe.hands_free);
    println!(
        "period a stream would get: {:.2} ms, resampled by Windows: {}",
        probe.period.as_secs_f64() * 1000.0,
        probe.resampled
    );
    println!(
        "device period: default {:.2} ms, minimum {:.2} ms",
        probe.device_default_period.as_secs_f64() * 1000.0,
        probe.device_min_period.as_secs_f64() * 1000.0
    );
    match probe.engine_periods {
        Some(p) => println!(
            "engine periods in frames: default {}, fundamental {}, min {}, max {}",
            p.default, p.fundamental, p.min, p.max
        ),
        None => println!("engine periods: not offered"),
    }
    match probe.engine_running {
        Some(frames) => println!("engine runs now at {frames} frames"),
        None => println!("engine period now: not offered"),
    }
    assert!(probe.period > Duration::ZERO);
}

// The default microphone opened asking for Windows' echo cancellation, as a
// room does with the switch on: what Windows lists as on for it, and what
// came of the ask, which depends on its driver and this Windows.
#[test]
fn default_microphone_with_echo_cancellation() {
    let _turn = ONE_STREAM_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tx, rx) = mpsc::channel();
    let capture = Capture::start_cancelling_echo(
        Choice::Default,
        None,
        |_: &[f32], _| {},
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("start the capture thread");
    let info = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Event::Opened(info)) => info,
        Ok(Event::Failed(AudioError::NoDevice(_))) => {
            println!("no microphone on this PC; skipped");
            return;
        }
        other => panic!("the default microphone did not open: {other:?}"),
    };
    capture.stop();
    println!("device: {}", info.device);
    println!("windows effects on: {:?}", info.effects);
    println!("echo cancellation: {}", info.echo);
    println!(
        "period {:.2} ms, stream latency {:.2} ms",
        info.period_ms(),
        info.stream_latency.as_secs_f64() * 1000.0
    );
    assert_ne!(info.echo, Echo::NotAsked);
    // The microphone's own canceller, or Windows' in the communications
    // category: either way Windows lists it on the stream Booth kept.
    assert_eq!(info.echo.cancelled(), info.effects.echo_cancellation);
}

// The build as the registry has it: a test runs under no compatibility mode.
fn windows_build() -> u32 {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "CurrentBuildNumber",
        ])
        .output()
        .expect("run reg");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .and_then(|build| build.parse().ok())
        .expect("a build number from reg")
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// Windows' process loopback opened as a room will: its stream as Windows
// gives it, and the packets' stamps, one after another, read soon after.
#[test]
fn pc_sound_opens_and_stamps_its_packets() {
    if audio::default_id(Direction::Output)
        .expect("ask for the default output")
        .is_none()
    {
        println!("skipped: Windows has no speakers or headphones turned on");
        return;
    }
    let build = windows_build();
    if build < PC_SOUND_BUILD {
        println!(
            "skipped: PC sound needs Windows build {PC_SOUND_BUILD} or later, this PC runs build {build}"
        );
        return;
    }
    let _turn = ONE_STREAM_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tx, rx) = mpsc::channel();
    let packets = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&packets);
    let sound = PcSound::start(
        move |samples: &[f32], time, gap| {
            let quiet = samples.iter().all(|&s| s == 0.0);
            seen.lock()
                .unwrap()
                .push((samples.len() / 2, time, gap, quiet, Instant::now()));
        },
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("start the pc sound thread");
    let info = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Event::Opened(info)) => info,
        other => panic!("pc sound did not open: {other:?}"),
    };
    std::thread::sleep(Duration::from_millis(500));
    let (event_reads, quiet_reads) = sound.reads();
    let glitches = sound.glitches();
    let stopping = Instant::now();
    sound.stop();
    let stop_time = stopping.elapsed();

    let packets = packets.lock().unwrap();
    println!(
        "{}: {}, opened in {:.1} ms",
        info.device,
        info.format,
        ms(info.opened_in)
    );
    println!(
        "period {:.2} ms ({} frames), buffer {} frames, stream latency {:.2} ms, refused first: {:?}",
        info.period_ms(),
        info.period_frames,
        info.buffer_frames,
        ms(info.stream_latency),
        info.refused
    );
    let mut ages: Vec<Duration> = packets
        .iter()
        .map(|(_, time, _, _, read)| read.saturating_duration_since(*time))
        .collect();
    ages.sort();
    let at = |share: f64| ages.get(((ages.len().max(1) - 1) as f64 * share).round() as usize);
    println!(
        "in 500 ms: {} packets, sizes {:?}, {} all zeros, {} after a gap; {event_reads} reads on the event, {quiet_reads} after a quiet wait; {glitches} glitches; stop took {:.2} ms",
        packets.len(),
        packets.iter().map(|p| p.0).take(8).collect::<Vec<_>>(),
        packets.iter().filter(|p| p.3).count(),
        packets.iter().filter(|p| p.2).count(),
        ms(stop_time)
    );
    if let (Some(median), Some(p95), Some(most)) = (at(0.5), at(0.95), at(1.0)) {
        println!(
            "read after its stamp: median {:.2} ms, 95th {:.2} ms, longest {:.2} ms",
            ms(*median),
            ms(*p95),
            ms(*most)
        );
    }
    assert_eq!((info.format.rate, info.format.channels), (48_000, 2));
    for pair in packets.windows(2) {
        let (frames, time, ..) = pair[0];
        let (_, next, gap, ..) = pair[1];
        assert!(next > time, "stamps went backwards");
        if !gap {
            let length = Duration::from_secs_f64(frames as f64 / 48_000.0);
            let step = next - time;
            assert!(
                step.abs_diff(length) < Duration::from_millis(1),
                "{step:?} after a packet of {length:?}"
            );
        }
    }
    assert!(stop_time < Duration::from_millis(100));
}
