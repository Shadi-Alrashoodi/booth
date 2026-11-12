// The microphone's processing on its own, fed by hand.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::{Duration, Instant};

use voice::audio::{Format, to_mono};
use voice::process::{Mark, NOISE_SUPPRESSION_DELAY, Playback, Processor, Switches};

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
    let mut processor = Processor::new(Arc::new(Switches::new(false, false)));
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

    // With noise suppression on, the packets that finish no block come in
    // between, empty.
    for _ in 0..59 {
        playback.push(&tone(0, 240, 0.9));
        playback.push(&[]);
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

// A microphone opened again, perhaps another one, starts from no gain: what
// automatic volume learnt of the one before says nothing about it.
#[test]
fn restart_forgets_the_gain() {
    let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
    through(&mut processor, &speech(4, -40.0));
    assert!(processor.gain_db() > 10.0, "{} dB", processor.gain_db());
    processor.restart();
    assert_eq!(processor.gain_db(), 0.0);
}

// Nor does the meter keep what it heard of the one before: the first 5 ms
// block after a restart reads only itself, with nothing of the 50 ms before
// in it.
#[test]
fn restart_empties_the_meter() {
    let mut processor = Processor::new(Arc::new(Switches::new(false, false)));
    let level = processor.level();
    through(&mut processor, &[1.0; 2_400]);
    assert_eq!(level.read().peak, 1.0);
    processor.restart();
    through(&mut processor, &[0.0; 240]);
    let quiet = level.read();
    assert_eq!((quiet.peak, quiet.rms), (0.0, 0.0), "{quiet:?}");
}

// Speech in 400 ms words with 200 ms of the room between them, at `db` dBFS
// RMS over a room about 40 dB quieter but no quieter than about -75 dBFS,
// since no floor learns near silence, after a second of the room alone.
fn speech(seconds: usize, db: f32) -> Vec<f32> {
    let amplitude = 10f32.powf(db / 20.0) * 2f32.sqrt();
    let room = 10f32.powf((db - 35.0).max(-70.0) / 20.0);
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
        let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
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
    let switches = Arc::new(Switches::new(false, false));
    let mut processor = Processor::new(Arc::clone(&switches));
    let quiet = speech(3, -34.0);
    assert_eq!(through(&mut processor, &quiet), quiet);
    switches
        .automatic_volume
        .store(true, std::sync::atomic::Ordering::Relaxed);
    through(&mut processor, &quiet);
    assert!(processor.gain_db() > 5.0);
}

// White noise at `db` dBFS RMS, a fan or a room, for `samples` samples.
fn room(samples: usize, db: f32, seed: &mut u32) -> Vec<f32> {
    let amplitude = 10f32.powf(db / 20.0) * 3f32.sqrt();
    (0..samples)
        .map(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 17;
            *seed ^= *seed << 5;
            amplitude * (*seed as f32 / u32::MAX as f32 * 2.0 - 1.0)
        })
        .collect()
}

// Digital silence, a packet Windows marks silent or a microphone's own mute,
// is not the room's floor: a steady room after it is not taken for a voice
// and pumped up, whether the silence comes first or after the room, a 5 ms
// block or a 10 ms packet of it, or a run that starts and ends partway
// through blocks, 8 samples of the room in each.
#[test]
fn digital_silence_does_not_pump_the_room() {
    let mut seed = 0x2545_f491;
    for (before, zeros) in [
        (2 * 48_000, 240),
        (0, 240),
        (2 * 48_000, 480),
        (2 * 48_000 + 8, 464),
        (8, 464),
    ] {
        let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
        let mut signal = room(before, -50.0, &mut seed);
        signal.extend(vec![0.0; zeros]);
        signal.extend(room(5 * 48_000, -50.0, &mut seed));
        through(&mut processor, &signal);
        assert_eq!(
            processor.gain_db(),
            0.0,
            "{zeros} zeros after {before} samples"
        );
    }
}

// Nor is a mute that leaves the converter's last bit toggling, about
// -120 dBFS and never zero.
#[test]
fn a_toggling_last_bit_does_not_pump_the_room() {
    let mut seed = 0x2545_f491;
    let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
    let mut signal = room(2 * 48_000, -50.0, &mut seed);
    signal.extend((0..240).map(|n| if n % 2 == 0 { 1e-6 } else { -1e-6 }));
    signal.extend(room(5 * 48_000, -50.0, &mut seed));
    through(&mut processor, &signal);
    assert_eq!(processor.gain_db(), 0.0);
}

// Nor is a mute that leaves dither at -90 dBFS: a block of it on the blocks
// or 8 samples off them, 2 s of it, or 2 s of it first.
#[test]
fn dither_mute_keeps_gain() {
    let mut seed = 0x2545_f491;
    for (before, dither) in [
        (2 * 48_000, 240),
        (2 * 48_000 + 8, 240),
        (2 * 48_000, 2 * 48_000),
        (0, 2 * 48_000),
    ] {
        let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
        let mut signal = room(before, -45.0, &mut seed);
        signal.extend(room(dither, -90.0, &mut seed));
        signal.extend(room(5 * 48_000, -45.0, &mut seed));
        through(&mut processor, &signal);
        assert_eq!(
            processor.gain_db(),
            0.0,
            "{dither} samples of dither after {before}"
        );
    }
}

// A millisecond of exact zeros or more is near silence wherever it starts,
// though it fills none of the millisecond windows counted from the start.
#[test]
fn zeros_across_windows_marked() {
    let mut processor = Processor::new(Arc::new(Switches::new(false, false)));
    let mut signal = room(144, -40.0, &mut 0x2545_f491);
    signal[24..84].fill(0.0);
    let marks = processor.run_marked(&signal, Instant::now()).1;
    for (at, &mark) in marks.iter().enumerate() {
        let wanted = if (24..84).contains(&at) {
            Mark::Quiet
        } else {
            Mark::Room
        };
        assert_eq!(mark, wanted, "sample {at}");
    }
}

// After a restart those windows count from its first sample.
#[test]
fn restart_realigns_marks() {
    let mut processor = Processor::new(Arc::new(Switches::new(false, false)));
    let mut seed = 0x2545_f491;
    processor.run_marked(&room(100, -40.0, &mut seed), Instant::now());
    processor.restart();
    let mut signal = room(48, -90.0, &mut seed);
    signal.extend(room(48, -40.0, &mut seed));
    let marks = processor.run_marked(&signal, Instant::now()).1;
    assert_eq!(marks[..48], [Mark::Quiet; 48]);
    assert_eq!(marks[48..], [Mark::Room; 48]);
}

// Cycles of a 150 Hz voice with 8 Hz of vibrato five times a second. The
// pitch is integrated: the pitch times t would sweep further every second.
fn voice_cycles(t: f32) -> f32 {
    150.0 * t + 8.0 / (TAU * 5.0) * (1.0 - (TAU * 5.0 * t).cos())
}

// Something RNNoise takes for a voice, which a steady tone is not: 150 Hz and
// its harmonics in 250 ms syllables, over white noise at -48 dBFS RMS.
fn noisy_voice(n: usize) -> (f32, bool) {
    let t = n as f32 / 48_000.0;
    let phase = t % 0.4;
    let cycles = voice_cycles(t);
    let voice: f32 = (1..20)
        .map(|h| (TAU * h as f32 * cycles).sin() / h as f32)
        .sum();
    let envelope = if t > 1.0 && phase < 0.25 {
        (TAU * phase / 0.5).sin()
    } else {
        0.0
    };
    let mut seed = (n as u32).wrapping_mul(0x9e37_79b9) ^ 0x5bd1_e995;
    seed ^= seed << 13;
    seed ^= seed >> 17;
    seed ^= seed << 5;
    let noise = 0.007 * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0);
    (0.08 * voice * envelope + noise, phase < 0.25)
}

#[test]
fn noisy_voice_holds_pitch() {
    for ms in 0..10_000 {
        let t = ms as f32 / 1000.0;
        let hz = (voice_cycles(t + 0.01) - voice_cycles(t)) / 0.01;
        assert!((hz - 150.0).abs() < 9.0, "{hz} Hz at {t} s");
    }
}

fn rms_db(samples: &[f32]) -> f32 {
    10.0 * (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).log10()
}

// With noise suppression on, the noise between syllables drops by well over
// 10 dB while the syllables stay within 2 dB; nothing is lost or repeated
// whatever the packet size, and each 10 ms block comes out 10 ms after it
// went in, with the capture time moved back to match.
#[test]
fn noise_suppression_quiets_the_room_and_keeps_the_voice() {
    let mut processor = Processor::new(Arc::new(Switches::new(true, false)));
    let start = Instant::now();
    let total = 4 * 48_000;
    let signal: Vec<f32> = (0..total).map(|n| noisy_voice(n).0).collect();
    let mut out = Vec::new();
    let mut fed = 0;
    for size in [128usize, 441, 480, 7, 1024].into_iter().cycle() {
        if fed >= total {
            break;
        }
        let packet = &signal[fed..(fed + size).min(total)];
        let captured = start + Duration::from_nanos(fed as u64 * 1_000_000_000 / 48_000);
        let (done, when) = processor.run(packet, captured);
        if !done.is_empty() {
            let block_start = out.len();
            let expected =
                start + Duration::from_nanos(block_start as u64 * 1_000_000_000 / 48_000);
            let off = when.max(expected - NOISE_SUPPRESSION_DELAY)
                - when.min(expected - NOISE_SUPPRESSION_DELAY);
            assert!(off < Duration::from_micros(30), "{off:?}");
            assert_eq!(done.len() % 480, 0);
        }
        out.extend_from_slice(done);
        fed += packet.len();
    }
    assert_eq!(out.len(), total / 480 * 480);
    // Out is 480 samples behind in.
    let (mut words, mut gaps, mut words_in, mut gaps_in) = (vec![], vec![], vec![], vec![]);
    for n in 2 * 48_000..out.len() - 480 {
        let phase = (n as f32 / 48_000.0) % 0.4;
        let (heard, kept) = (out[n + 480], signal[n]);
        if (0.05..0.2).contains(&phase) {
            words.push(heard);
            words_in.push(kept);
        } else if (0.27..0.38).contains(&phase) {
            gaps.push(heard);
            gaps_in.push(kept);
        }
    }
    let (gap_in, gap_out) = (rms_db(&gaps_in), rms_db(&gaps));
    let (word_in, word_out) = (rms_db(&words_in), rms_db(&words));
    println!("gaps {gap_in:.1} -> {gap_out:.1} dB, syllables {word_in:.1} -> {word_out:.1} dB");
    assert!(gap_out < gap_in - 12.0);
    assert!((word_out - word_in).abs() < 2.0);
}

// The marks come out with the sound they belong to: noise suppression's
// first blocks are Settling, and a mute is Quiet along with the nine blocks
// after it that RNNoise's memory of it reaches.
#[test]
fn marks_through_noise_suppression() {
    let mut processor = Processor::new(Arc::new(Switches::new(true, false)));
    let mut seed = 0x2545_f491;
    let mut signal = room(6 * 48_000, -40.0, &mut seed);
    signal[96_000..100_800].fill(0.0);
    let start = Instant::now();
    let mut marks = Vec::new();
    for packet in signal.chunks(441) {
        marks.extend_from_slice(processor.run_marked(packet, start).1);
    }
    assert_eq!(marks.len(), signal.len() / 480 * 480);
    for (at, &mark) in marks.iter().enumerate() {
        let wanted = match at {
            0..4_320 => Mark::Settling,
            96_000..105_120 => Mark::Quiet,
            _ => Mark::Room,
        };
        assert_eq!(mark, wanted, "sample {at}");
    }
}

// Noise suppression fades into a mute and out of it in blocks far under the
// room, and RNNoise is unsettled for some blocks after. Automatic volume's
// floor learns none of them, so the room after a mute is not pumped up: each
// run against the same sound with no mute in it.
#[test]
fn suppression_fades_keep_floor() {
    let mut seed = 0x2545_f491;
    let white = room(6 * 48_000, -40.0, &mut seed);
    let voice: Vec<f32> = (0..6 * 48_000).map(|n| noisy_voice(n).0).collect();
    let dither = room(4_800, -90.0, &mut 0x51ed_270b);
    let mut cases = Vec::new();
    for dithered in [false, true] {
        for at in [108_960, 109_200, 109_432] {
            cases.push(("voice", &voice, at, dithered));
        }
    }
    for (at, dithered) in [
        (96_000, false),
        (96_312, false),
        (96_000, true),
        (96_096, true),
    ] {
        cases.push(("white", &white, at, dithered));
    }
    for (name, plain, at, dithered) in cases {
        let mut muted = plain.clone();
        if dithered {
            muted[at..at + 4_800].copy_from_slice(&dither);
        } else {
            muted[at..at + 4_800].fill(0.0);
        }
        let switches = Arc::new(Switches::new(true, true));
        let mut processor = Processor::new(Arc::clone(&switches));
        let mut control = Processor::new(switches);
        let start = Instant::now();
        let (mut fed, mut worst) = (0, 0.0f32);
        for (packet, same) in muted.chunks(441).zip(plain.chunks(441)) {
            processor.run(packet, start);
            control.run(same, start);
            fed += packet.len();
            if (at..at + 2 * 48_000).contains(&fed) {
                worst = worst.min(processor.floor_db() - control.floor_db());
            }
        }
        let (gain, control_gain) = (processor.gain_db(), control.gain_db());
        let what = format!(
            "{name} with {} at {at}",
            if dithered { "dither" } else { "zeros" }
        );
        println!(
            "{what}: floor at worst {worst:.1} dB from the control's, gain {gain:.1} dB against {control_gain:.1}"
        );
        assert!(worst >= -3.0, "{what}: {worst} dB");
        assert!(gain <= control_gain + 3.0, "{what}: {gain} dB");
    }
}

// A captured sample that is not a number reaches noise suppression as
// silence, one far past full scale as eight times full scale, and the tone
// after it comes through as it would have.
#[test]
fn denoiser_after_bad_samples() {
    let mut clean = tone(0, 24_000, 0.5);
    let mut captured: Vec<u8> = clean
        .iter()
        .flat_map(|s| [s.to_le_bytes(), s.to_le_bytes()])
        .flatten()
        .collect();
    for (frame, at, bad, becomes) in [
        (100, 0, f32::NAN, 0.0),
        (5_000, 4, f32::INFINITY, 0.0),
        (10_000, 0, -1e20, -8.0),
        (15_000, 4, 1e35, 8.0),
    ] {
        clean[frame] = becomes;
        captured[frame * 8 + at..frame * 8 + at + 4].copy_from_slice(&bad.to_le_bytes());
    }
    let mut mono = Vec::new();
    to_mono(&captured, &Format::float_48k(2, 0b11), &mut mono);
    let switches = Arc::new(Switches::new(true, false));
    let heard = through(&mut Processor::new(Arc::clone(&switches)), &mono);
    assert!(heard.iter().all(|s| s.is_finite()));
    assert_eq!(heard, through(&mut Processor::new(switches), &clean));
}

// A sample past full scale, as a float microphone can give, reaches noise
// suppression as it is.
#[test]
fn denoiser_past_full_scale() {
    let switches = Arc::new(Switches::new(true, false));
    let heard = |peak: f32| {
        let mut signal = tone(0, 4_800, 0.5);
        signal[2_400] = peak;
        through(&mut Processor::new(Arc::clone(&switches)), &signal)
    };
    assert_ne!(heard(4.0), heard(1.0));
}

// What the processing costs on the capture thread for each 10 ms packet,
// both stages on: printed, and held under a tenth of the packet in a
// release build.
#[test]
fn processing_time() {
    let mut processor = Processor::new(Arc::new(Switches::new(true, true)));
    let signal: Vec<f32> = (0..10 * 48_000).map(|n| noisy_voice(n).0).collect();
    let start = Instant::now();
    let mut took: Vec<Duration> = signal
        .chunks(480)
        .map(|packet| {
            let began = Instant::now();
            std::hint::black_box(processor.run(packet, start));
            began.elapsed()
        })
        .collect();
    took.sort_unstable();
    let at = |share: f64| took[((took.len() - 1) as f64 * share).round() as usize];
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "{build} build: noise suppression and automatic volume, a 10 ms packet in median {:.1} us, p99 {:.1} us",
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6
    );
    if !cfg!(debug_assertions) {
        assert!(at(0.5) < Duration::from_millis(1), "{:?}", at(0.5));
    }
}
