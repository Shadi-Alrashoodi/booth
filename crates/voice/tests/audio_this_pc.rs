// On this PC's own devices. Nothing here records sound or makes any: the
// devices are listed, the output plays silence for a fraction of a second,
// and the microphone is only asked for its period and format, never started.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use voice::audio::{self, AudioError, Choice, Direction, Event, Render};

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
