// The microphone's processing on its own, fed by hand.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::{Duration, Instant};

use voice::process::{Playback, Processor, Switches};

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
    let mut processor = Processor::new(Arc::new(Switches::new(false)));
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

// Speech in 400 ms words with 200 ms of the room between them, at `db` dBFS
// RMS over a room 35 dB quieter, after a second of the room alone.
fn speech(seconds: usize, db: f32) -> Vec<f32> {
    let amplitude = 10f32.powf(db / 20.0) * 2f32.sqrt();
    let room = 10f32.powf((db - 35.0) / 20.0);
    let mut seed = 0x9e37_79b9_u32;
    let mut out = Vec::new();
    for n in 0..(seconds + 1) * 48_000 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let noise = room * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0);
        let in_word = n >= 48_000 && (n - 48_000) % 28_800 < 19_200;
        let voice = if in_word {
            tone(n, 1, amplitude)[0]
        } else {
            0.0
        };
        out.push(voice + noise);
    }
    out
}

// The RMS in dB of the words in the last second of `samples`.
fn words_db(samples: &[f32]) -> f32 {
    let last = &samples[samples.len() - 48_000..];
    let loud: Vec<f32> = last
        .chunks(240)
        .map(|block| block.iter().map(|s| s * s).sum::<f32>() / 240.0)
        .filter(|&power| power > 1e-7)
        .collect();
    let max = loud.iter().copied().fold(0.0f32, f32::max);
    let words: Vec<f32> = loud.into_iter().filter(|&p| p > max / 4.0).collect();
    10.0 * (words.iter().sum::<f32>() / words.len() as f32).log10()
}

fn through(processor: &mut Processor, signal: &[f32]) -> Vec<f32> {
    let start = Instant::now();
    let mut out = Vec::with_capacity(signal.len());
    for packet in signal.chunks(480) {
        out.extend_from_slice(processor.run(packet, start).0);
    }
    out
}

// Automatic volume brings a quiet microphone up and a loud one down to about
// -20 dBFS over a few seconds of speech, never past +20 dB, holds the gain
// through a long silence so the room is not pumped up, and with the switch
// off does nothing at all.
#[test]
fn automatic_volume_evens_out_quiet_and_loud() {
    for (db, gain) in [(-34.0, 14.0), (-50.0, 20.0), (-10.0, -10.0)] {
        let mut processor = Processor::new(Arc::new(Switches::new(true)));
        let out = through(&mut processor, &speech(6, db));
        let heard = words_db(&out);
        println!(
            "words at {db} dB come out at {heard:.1} dB, gain {:.1} dB",
            processor.gain_db()
        );
        assert!(
            (processor.gain_db() - gain).abs() < 0.5,
            "{}",
            processor.gain_db()
        );
        assert!((heard - (db + gain)).abs() < 1.5, "{heard}");
        assert!(out.iter().all(|s| s.abs() <= 1.0));
        let held = processor.gain_db();
        through(&mut processor, &vec![0.0; 3 * 48_000]);
        assert_eq!(processor.gain_db(), held);
    }
    let switches = Arc::new(Switches::new(false));
    let mut processor = Processor::new(Arc::clone(&switches));
    let quiet = speech(3, -34.0);
    assert_eq!(through(&mut processor, &quiet), quiet);
    switches
        .automatic_volume
        .store(true, std::sync::atomic::Ordering::Relaxed);
    through(&mut processor, &quiet);
    assert!(processor.gain_db() > 5.0);
}
