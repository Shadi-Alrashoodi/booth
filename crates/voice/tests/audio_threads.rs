// The capture and render threads against the fake device, on the real clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use voice::audio::fake::{Fake, Setup};
use voice::audio::{AudioError, Capture, Choice, Event, REOPEN_AFTER, Render, StreamInfo};

const PERIOD: u32 = 128;
// 128 frames at 48 kHz.
const PERIOD_TIME: Duration = Duration::from_nanos(2_666_667);

fn events() -> (
    impl FnMut(Event) + Send + 'static,
    Receiver<(Instant, Event)>,
) {
    let (tx, rx) = mpsc::channel();
    (
        move |event| {
            let _ = tx.send((Instant::now(), event));
        },
        rx,
    )
}

fn next(rx: &Receiver<(Instant, Event)>) -> (Instant, Event) {
    rx.recv_timeout(Duration::from_secs(3))
        .expect("an event within 3 s")
}

fn opened(rx: &Receiver<(Instant, Event)>) -> (Instant, StreamInfo) {
    match next(rx) {
        (at, Event::Opened(info)) => (at, info),
        (_, other) => panic!("expected Opened, got {other:?}"),
    }
}

fn wait_for(frames: &AtomicU64, at_least: u64) {
    let until = Instant::now() + Duration::from_secs(3);
    while frames.load(Ordering::Relaxed) < at_least {
        assert!(Instant::now() < until, "the capture thread stalled");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn one_mic(setup: Setup) -> Fake {
    Fake::new(setup, &[("mic", "Fake microphone")], Some("mic"))
}

#[test]
fn capture_delivers_timed_mono_packets() {
    let fake = one_mic(Setup {
        channels: 2,
        signal: |_, channel| if channel == 0 { 0.25 } else { 0.75 },
        ..Setup::default()
    });
    let packets = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&packets);
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Default,
        move |samples: &[f32], time| {
            seen.lock().unwrap().push((samples.to_vec(), time));
        },
        on_event,
    )
    .unwrap();
    let (_, info) = opened(&rx);
    std::thread::sleep(Duration::from_millis(100));
    capture.stop();

    assert_eq!(info.device, "Fake microphone");
    assert_eq!(info.format.channels, 2);
    assert_eq!(info.period_frames, PERIOD);
    assert!((info.period_ms() - 2.667).abs() < 0.001);
    let packets = packets.lock().unwrap();
    println!(
        "{} packets in 100 ms, period {:.3} ms",
        packets.len(),
        info.period_ms()
    );
    assert!(packets.len() >= 30, "{} packets", packets.len());
    for (samples, _) in packets.iter() {
        assert_eq!(samples.len(), PERIOD as usize);
        // The two channels, 0.25 and 0.75, averaged.
        assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");
    }
    // Each packet is stamped with when its first frame was captured, one
    // period after the one before.
    for pair in packets.windows(2) {
        let step = pair[1].1 - pair[0].1;
        let off = step.abs_diff(PERIOD_TIME);
        assert!(off < Duration::from_micros(2), "{step:?}");
    }
}

#[test]
fn stop_takes_under_100_ms_running_or_stalled() {
    for stalled in [false, true] {
        let fake = one_mic(Setup::default());
        let (on_event, rx) = events();
        let capture =
            Capture::start_with(fake.device(), Choice::Default, |_: &[f32], _| {}, on_event)
                .unwrap();
        opened(&rx);
        fake.stall(stalled);
        std::thread::sleep(Duration::from_millis(30));
        let stopping = Instant::now();
        capture.stop();
        let took = stopping.elapsed();
        println!(
            "stop, {}: {:.2} ms",
            if stalled { "stalled" } else { "running" },
            took.as_secs_f64() * 1000.0
        );
        assert!(took < Duration::from_millis(100), "{took:?}");
        assert_eq!(fake.record().stops, 1);
    }
}

// A device that takes its time to close, as a Bluetooth headset can: let_go
// returns at once, and what it hands back names the device while it closes
// and disconnects only once it is closed and the callbacks are dropped,
// never before.
#[test]
fn a_stream_let_go_returns_at_once_and_says_when_its_device_closed() {
    const CLOSE: Duration = Duration::from_millis(400);
    let fake = one_mic(Setup {
        close_time: CLOSE,
        ..Setup::default()
    });
    let held = Arc::new(());
    let in_callback = Arc::clone(&held);
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Default,
        move |_: &[f32], _| {
            let _ = &in_callback;
        },
        on_event,
    )
    .unwrap();
    opened(&rx);
    assert_eq!(capture.thread().device().as_deref(), Some("mic"));
    let stopping = Instant::now();
    let thread = capture.let_go();
    let returned = stopping.elapsed();
    assert_eq!(
        thread.ended().recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );
    assert_eq!(fake.record().at_once, 1, "closed before it had to be");
    assert!(thread.running());
    assert_eq!(thread.device().as_deref(), Some("mic"), "while it closes");
    assert_eq!(
        thread.ended().recv_timeout(Duration::from_secs(3)),
        Err(RecvTimeoutError::Disconnected)
    );
    let closed = stopping.elapsed();
    assert!(!thread.running());
    assert_eq!(thread.device(), None);
    println!(
        "let_go returned in {:.2} ms, the device closed {:.0} ms after it",
        returned.as_secs_f64() * 1000.0,
        closed.as_secs_f64() * 1000.0
    );
    assert!(returned < Duration::from_millis(5), "{returned:?}");
    assert!(closed >= CLOSE, "{closed:?}");
    let record = fake.record();
    assert_eq!((record.stops, record.at_once), (1, 0));
    assert_eq!(Arc::strong_count(&held), 1, "the callback outlived the end");
    assert!(rx.try_recv().is_err(), "the stream said something");
}

// Both channels carry a 1 kHz sine of amplitude 0.5: peak 0.5, and RMS 0.5
// over the square root of 2.
#[test]
fn the_meter_reads_peak_and_rms_of_a_known_signal() {
    let fake = one_mic(Setup {
        signal: |frame, _| {
            let t = frame as f64 / 48_000.0;
            (0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as f32
        },
        ..Setup::default()
    });
    let frames = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&frames);
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Default,
        move |samples: &[f32], _| {
            counted.fetch_add(samples.len() as u64, Ordering::Relaxed);
        },
        on_event,
    )
    .unwrap();
    opened(&rx);
    // More than the 50 ms window, so it is full of the sine.
    wait_for(&frames, 3_000);
    let level = capture.level();
    println!("peak {:.4}, rms {:.4}", level.peak, level.rms);
    assert!((level.peak - 0.5).abs() < 0.001, "{level:?}");
    assert!((level.rms - 0.5 / 2f32.sqrt()).abs() < 0.002, "{level:?}");
    capture.stop();
}

// 100 ms of full scale, then silence: once 50 ms of silence are in, the
// meter reads zero, and it reads zero once the stream is gone.
#[test]
fn the_meter_forgets_what_is_older_than_50_ms() {
    let fake = one_mic(Setup {
        signal: |frame, _| if frame < 4_800 { -1.0 } else { 0.0 },
        ..Setup::default()
    });
    let frames = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&frames);
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Default,
        move |samples: &[f32], _| {
            counted.fetch_add(samples.len() as u64, Ordering::Relaxed);
        },
        on_event,
    )
    .unwrap();
    opened(&rx);
    wait_for(&frames, 2_560);
    let loud = capture.level();
    assert_eq!(loud.peak, 1.0, "{loud:?}");
    // 4800 frames of signal, 2400 of silence, and one packet to spare.
    wait_for(&frames, 4_800 + 2_400 + 128);
    let quiet = capture.level();
    assert_eq!((quiet.peak, quiet.rms), (0.0, 0.0), "{quiet:?}");
    capture.stop();
}

#[test]
fn lost_default_device_reopens_after_a_second() {
    let fake = Fake::new(
        Setup::default(),
        &[("a", "Headset A"), ("b", "Speakers B")],
        Some("a"),
    );
    let (on_event, rx) = events();
    let capture =
        Capture::start_with(fake.device(), Choice::Default, |_: &[f32], _| {}, on_event).unwrap();
    let (_, first) = opened(&rx);
    assert_eq!(first.id, "a");
    std::thread::sleep(Duration::from_millis(20));
    fake.unplug("a");
    let lost_at = match next(&rx) {
        (at, Event::Lost(err)) => {
            assert!(err.is_lost(), "{err}");
            assert_eq!(
                err.to_string(),
                "Headset A stopped: it was unplugged, turned off, or changed its format. Connect it again, or choose another device in Booth's settings"
            );
            at
        }
        (_, other) => panic!("expected Lost, got {other:?}"),
    };
    assert!(capture.info().is_none());
    // Windows names the new default a moment later; the thread still waits
    // out its second.
    fake.set_default(Some("b"));
    let (opened_at, second) = opened(&rx);
    let gap = opened_at - lost_at;
    println!(
        "reopened on {} after {:.1} ms",
        second.id,
        gap.as_secs_f64() * 1000.0
    );
    assert_eq!(second.id, "b");
    assert!(gap >= REOPEN_AFTER, "{gap:?}");
    assert!(gap < REOPEN_AFTER + Duration::from_millis(200), "{gap:?}");
    assert_eq!(capture.info().map(|info| info.id).as_deref(), Some("b"));
    capture.stop();
}

#[test]
fn chosen_device_gone_ends_the_stream() {
    let fake = Fake::new(
        Setup::default(),
        &[("a", "Headset A"), ("b", "Speakers B")],
        Some("b"),
    );
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Device(String::from("a")),
        |_: &[f32], _| {},
        on_event,
    )
    .unwrap();
    assert_eq!(opened(&rx).1.id, "a");
    fake.unplug("a");
    match next(&rx) {
        (_, Event::Gone(err)) => assert!(err.is_lost(), "{err}"),
        (_, other) => panic!("expected Gone, got {other:?}"),
    }
    // Not opened again, on b or anything else.
    assert!(
        rx.recv_timeout(REOPEN_AFTER + Duration::from_millis(300))
            .is_err()
    );
    assert_eq!(fake.record().opens.len(), 1);
    capture.stop();
}

// A device that goes away stops being signalled, so only the check on a
// quiet wait can see it.
#[test]
fn a_chosen_output_that_goes_away_is_gone() {
    let fake = Fake::new(
        Setup::default(),
        &[("out", "Fake speakers"), ("hdmi", "Monitor")],
        Some("hdmi"),
    );
    let (on_event, rx) = events();
    let render = Render::start_with(
        fake.device(),
        Choice::Device(String::from("out")),
        |samples: &mut [f32]| samples.fill(0.0),
        on_event,
    )
    .unwrap();
    assert_eq!(opened(&rx).1.id, "out");
    std::thread::sleep(Duration::from_millis(20));
    let unplugged = Instant::now();
    fake.unplug("out");
    let (at, event) = next(&rx);
    match event {
        Event::Gone(err) => assert!(err.is_lost(), "{err}"),
        other => panic!("expected Gone, got {other:?}"),
    }
    println!(
        "gone {:.1} ms after the unplug",
        (at - unplugged).as_secs_f64() * 1000.0
    );
    assert!(at - unplugged < Duration::from_millis(200));
    assert!(render.info().is_none());
    render.stop();
}

// Opening a Bluetooth headset can take a second while it changes profile. A
// stop asked for meanwhile ends the stream as soon as the open returns,
// before it starts, and says nothing.
#[test]
fn a_stop_during_a_slow_open_ends_it_before_it_starts() {
    let fake = one_mic(Setup {
        open_time: Duration::from_millis(300),
        ..Setup::default()
    });
    let (on_event, rx) = events();
    let capture =
        Capture::start_with(fake.device(), Choice::Default, |_: &[f32], _| {}, on_event).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let stopping = Instant::now();
    capture.stop();
    println!(
        "stop during the open took {:.1} ms",
        stopping.elapsed().as_secs_f64() * 1000.0
    );
    let record = fake.record();
    assert_eq!(record.opens.len(), 1);
    assert_eq!(record.starts, 0);
    assert_eq!(record.at_once, 0);
    assert!(rx.try_recv().is_err(), "the stream said something");
}

#[test]
fn capture_counts_lost_sound_but_not_on_the_first_packet() {
    let fake = one_mic(Setup::default());
    let frames = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&frames);
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Default,
        move |samples: &[f32], _| {
            counted.fetch_add(samples.len() as u64, Ordering::Relaxed);
        },
        on_event,
    )
    .unwrap();
    opened(&rx);
    // The fake marks each stream's first packet, as Windows may after a
    // start.
    wait_for(&frames, 1_280);
    assert_eq!(capture.glitches(), 0);
    fake.glitch();
    let now = frames.load(Ordering::Relaxed);
    wait_for(&frames, now + 1_280);
    assert_eq!(capture.glitches(), 1);
    capture.stop();
}

#[test]
fn a_chosen_device_that_is_not_there_fails_at_once() {
    let fake = Fake::new(Setup::default(), &[("b", "Speakers B")], Some("b"));
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Device(String::from("a")),
        |_: &[f32], _| {},
        on_event,
    )
    .unwrap();
    match next(&rx) {
        (_, Event::Failed(AudioError::NotConnected { .. })) => {}
        (_, other) => panic!("expected NotConnected, got {other:?}"),
    }
    capture.stop();
}

#[test]
fn a_new_windows_default_is_followed_at_once() {
    let fake = Fake::new(
        Setup::default(),
        &[("a", "Headset A"), ("b", "Speakers B")],
        Some("a"),
    );
    let (on_event, rx) = events();
    let capture =
        Capture::start_with(fake.device(), Choice::Default, |_: &[f32], _| {}, on_event).unwrap();
    opened(&rx);
    // The same default again is no change.
    fake.set_default(Some("a"));
    std::thread::sleep(Duration::from_millis(30));
    let asked = Instant::now();
    fake.set_default(Some("b"));
    let (at, info) = opened(&rx);
    println!(
        "switched to b in {:.2} ms",
        (at - asked).as_secs_f64() * 1000.0
    );
    assert_eq!(info.id, "b");
    assert!(at - asked < Duration::from_millis(100));
    assert_eq!(fake.record().opens.len(), 2);
    capture.stop();
}

#[test]
fn a_chosen_device_stays_when_the_default_moves() {
    let fake = Fake::new(
        Setup::default(),
        &[("a", "Headset A"), ("b", "Speakers B")],
        Some("b"),
    );
    let (on_event, rx) = events();
    let capture = Capture::start_with(
        fake.device(),
        Choice::Device(String::from("a")),
        |_: &[f32], _| {},
        on_event,
    )
    .unwrap();
    opened(&rx);
    fake.set_default(Some("a"));
    fake.set_default(Some("b"));
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    assert_eq!(fake.record().opens.len(), 1);
    capture.stop();
}

// Under Windows default with nothing plugged in, the stream says so and
// opens when a device arrives, without anyone asking again.
#[test]
fn with_no_device_the_stream_waits_for_one() {
    let fake = Fake::new(Setup::default(), &[], None);
    let (on_event, rx) = events();
    let capture =
        Capture::start_with(fake.device(), Choice::Default, |_: &[f32], _| {}, on_event).unwrap();
    match next(&rx) {
        (_, Event::Failed(AudioError::NoDevice(_))) => {}
        (_, other) => panic!("expected NoDevice, got {other:?}"),
    }
    fake.plug("c", "USB headset");
    fake.set_default(Some("c"));
    assert_eq!(opened(&rx).1.device, "USB headset");
    let stopping = Instant::now();
    capture.stop();
    assert!(stopping.elapsed() < Duration::from_millis(100));
}

#[test]
fn a_stream_that_is_not_48_khz_is_refused() {
    let fake = one_mic(Setup {
        rate: 44_100,
        ..Setup::default()
    });
    let (on_event, rx) = events();
    let capture =
        Capture::start_with(fake.device(), Choice::Default, |_: &[f32], _| {}, on_event).unwrap();
    match next(&rx) {
        (_, Event::Failed(AudioError::Rate { rate: 44_100, .. })) => {}
        (_, other) => panic!("expected Rate, got {other:?}"),
    }
    capture.stop();
}

#[test]
fn render_fills_what_the_device_asks_for() {
    let fake = Fake::new(
        Setup {
            channels: 2,
            buffer_frames: 1056,
            ..Setup::default()
        },
        &[("out", "Fake speakers")],
        Some("out"),
    );
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&sizes);
    let mut next_value = 0.0f32;
    let (on_event, rx) = events();
    let render = Render::start_with(
        fake.device(),
        Choice::Default,
        move |samples: &mut [f32]| {
            seen.lock().unwrap().push(samples.len());
            for sample in samples.iter_mut() {
                next_value = (next_value + 0.001) % 0.5;
                *sample = next_value;
            }
        },
        on_event,
    )
    .unwrap();
    let (_, info) = opened(&rx);
    std::thread::sleep(Duration::from_millis(100));
    let latency = render.latency().unwrap();
    let queued = render.queued().unwrap();
    let underruns = render.underruns();
    render.stop();

    // Two periods queued at most, never the whole 1056-frame buffer.
    assert_eq!(info.render_target(), 2 * PERIOD);
    let sizes = sizes.lock().unwrap();
    let record = fake.record();
    println!(
        "{} fills, queued ahead {:.2} ms, latency {:.2} ms, underruns {underruns}",
        sizes.len(),
        queued.as_secs_f64() * 1000.0,
        latency.as_secs_f64() * 1000.0
    );
    assert!(sizes.len() >= 30, "{}", sizes.len());
    // Each fill is exactly what went to the device. A wake that came late
    // on a busy PC finds more room and asks for more, never past the target.
    let asked: Vec<u32> = sizes.iter().map(|&n| n as u32).collect();
    assert_eq!(asked, record.packets);
    assert!(
        asked.iter().all(|&n| n <= info.render_target()),
        "{asked:?}"
    );
    let one_period = asked.iter().filter(|&&n| n == PERIOD).count();
    assert!(one_period * 10 >= asked.len() * 8, "{asked:?}");
    let written = &record.written;
    assert_eq!(written.len(), sizes.iter().sum::<usize>() * 2);
    for [left, right] in written.as_chunks::<2>().0 {
        assert_eq!(left, right);
    }
    assert!(written.iter().any(|&s| s != 0.0));
    // Period, the fake's 1.5 ms stream latency, and about one period queued.
    let expected = PERIOD_TIME + Duration::from_micros(1500) + PERIOD_TIME;
    assert!(latency.abs_diff(expected) < PERIOD_TIME, "{latency:?}");
    assert!(underruns <= 2, "{underruns}");
}
