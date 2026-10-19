// The microphone's processing on its own, fed by hand.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use voice::process::{Playback, Processor};

// 440 Hz at `amplitude` for `samples` samples, from sample `from` on.
fn tone(from: usize, samples: usize, amplitude: f32) -> Vec<f32> {
    (from..from + samples)
        .map(|n| amplitude * (TAU * 440.0 * n as f32 / 48_000.0).sin())
        .collect()
}

// With nothing turned on the sound comes out as it went in, at the time it
// was captured, in packets of whatever size the device gives. The level is
// of what comes out: the peak over 50 ms, and the RMS of the loudest 5 ms
// block in it, which a short loud word sets where the 50 ms RMS would not.
#[test]
fn nothing_on_passes_the_sound_and_meters_it() {
    let mut processor = Processor::new();
    let level = processor.level();
    let start = Instant::now();
    let mut sent = Vec::new();
    let mut heard = Vec::new();
    let mut at = 0;
    for size in [128, 441, 480, 7, 1024] {
        let mut packet = tone(at, size, 0.05);
        if size == 480 {
            packet = tone(at, size, 0.5);
        }
        let captured = start + Duration::from_micros(at as u64 * 1_000_000 / 48_000);
        let (out, when) = processor.run(&packet, captured);
        assert_eq!(when, captured);
        heard.extend_from_slice(out);
        sent.extend_from_slice(&packet);
        at += size;
    }
    assert_eq!(heard, sent);
    let reading = level.read();
    println!("{reading:?}");
    assert!((reading.peak - 0.5).abs() < 0.01, "{reading:?}");
    // The loud 480 sit across two 5 ms blocks, one of them nearly all loud.
    assert!(reading.loudest > 0.3, "{reading:?}");
    assert!(reading.rms < reading.loudest * 0.75, "{reading:?}");
}

// Hear yourself's queue: nothing while off; on, what went in comes out in
// order and then silence; never more than 20 ms behind, so what plays is the
// newest; and sound near full scale for 300 ms turns it off until it is
// turned on again.
#[test]
fn playback_keeps_up_and_trips_on_feedback() {
    let playback = Playback::default();
    let mut out = [1.0f32; 480];
    playback.push(&[0.3; 240]);
    playback.fill(&mut out);
    assert!(out.iter().all(|&s| s == 0.0));

    playback.set_on(true);
    let voice = tone(0, 240, 0.2);
    playback.push(&voice);
    playback.fill(&mut out);
    assert_eq!(&out[..240], &voice[..]);
    assert!(out[240..].iter().all(|&s| s == 0.0));

    let long = tone(0, 2400, 0.2);
    playback.push(&long);
    let mut all = [0.0f32; 2400];
    playback.fill(&mut all);
    assert_eq!(&all[..960], &long[1440..]);
    assert!(all[960..].iter().all(|&s| s == 0.0));

    for _ in 0..59 {
        playback.push(&tone(0, 240, 0.9));
    }
    assert!(!playback.tripped());
    playback.push(&tone(0, 240, 0.9));
    assert!(playback.tripped());
    playback.push(&voice);
    playback.fill(&mut out);
    assert!(out.iter().all(|&s| s == 0.0));
    playback.set_on(true);
    assert!(!playback.tripped());
}
