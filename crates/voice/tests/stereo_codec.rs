use std::f64::consts::PI;
use std::time::{Duration, Instant};

use voice::stereo::{
    CHANNELS, Decoder, Encoder, FRAME, FrameInfo, LOOKAHEAD, MAX_PACKET, PACKET_BYTES, StereoError,
};

// 500 Hz on the left and 700 Hz on the right, frame `n` of them.
fn two_tones(n: usize) -> Vec<f32> {
    (0..FRAME)
        .flat_map(|i| {
            let t = (n * FRAME + i) as f64 / 48_000.0;
            [
                (0.3 * (2.0 * PI * 500.0 * t).sin()) as f32,
                (0.3 * (2.0 * PI * 700.0 * t).sin()) as f32,
            ]
        })
        .collect()
}

// The level at `hz` of one channel of interleaved stereo, in dB of full scale.
fn level_db(stereo: &[f32], channel: usize, hz: f64) -> f64 {
    let samples: Vec<f64> = stereo
        .iter()
        .skip(channel)
        .step_by(CHANNELS)
        .map(|&s| f64::from(s))
        .collect();
    let n = samples.len() as f64;
    let (mut re, mut im, mut sum) = (0.0, 0.0, 0.0);
    for (i, s) in samples.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / (n - 1.0)).cos();
        let angle = 2.0 * PI * hz * i as f64 / 48_000.0;
        re += s * w * angle.cos();
        im += s * w * angle.sin();
        sum += w;
    }
    let amplitude = 2.0 * (re * re + im * im).sqrt() / sum;
    20.0 * amplitude.max(1e-9).log10()
}

#[test]
fn left_and_right_come_back_apart_at_a_steady_size() {
    let mut encoder = Encoder::new().unwrap();
    let mut decoder = Decoder::new().unwrap();
    assert_eq!(encoder.lookahead().unwrap(), LOOKAHEAD);
    assert!(encoder.phase_inversion_disabled().unwrap());
    let mut decoded = Vec::new();
    let mut packet = [0u8; MAX_PACKET];
    let mut out = [0.0f32; FRAME * CHANNELS];
    let mut took = Vec::new();
    for n in 0..100 {
        let pcm = two_tones(n);
        let started = Instant::now();
        let len = encoder.encode(&pcm, &mut packet).unwrap();
        took.push(started.elapsed());
        assert_eq!(len, PACKET_BYTES);
        assert_eq!(PACKET_BYTES, 120);
        FrameInfo::read(&packet[..len]).unwrap();
        assert_eq!(decoder.decode(&packet[..len], &mut out).unwrap(), FRAME);
        if n >= 10 {
            decoded.extend_from_slice(&out);
        }
    }
    took.sort();
    let median = took[took.len() / 2];
    println!(
        "a 10 ms stereo frame encodes in {:.3} ms (median), {:.3} ms at worst",
        median.as_secs_f64() * 1000.0,
        took[took.len() - 1].as_secs_f64() * 1000.0
    );
    let (left_own, left_other) = (level_db(&decoded, 0, 500.0), level_db(&decoded, 0, 700.0));
    let (right_own, right_other) = (level_db(&decoded, 1, 700.0), level_db(&decoded, 1, 500.0));
    println!(
        "left: 500 Hz {left_own:.1} dB, 700 Hz {left_other:.1} dB; right: 700 Hz {right_own:.1} dB, 500 Hz {right_other:.1} dB"
    );
    // 0.3 is -10.5 dBFS.
    assert!((left_own + 10.5).abs() < 1.5 && (right_own + 10.5).abs() < 1.5);
    assert!(left_own - left_other > 40.0 && right_own - right_other > 40.0);
    if !cfg!(debug_assertions) {
        assert!(median < Duration::from_millis(1), "{median:?}");
    }
}

#[test]
fn a_frame_made_up_and_a_reset_decoder_go_on() {
    let mut encoder = Encoder::new().unwrap();
    let mut decoder = Decoder::new().unwrap();
    let mut packet = [0u8; MAX_PACKET];
    let mut out = [0.0f32; FRAME * CHANNELS];
    for n in 0..5 {
        let len = encoder.encode(&two_tones(n), &mut packet).unwrap();
        decoder.decode(&packet[..len], &mut out).unwrap();
    }
    assert_eq!(decoder.conceal(&mut out).unwrap(), FRAME);
    assert!(out.iter().any(|&s| s != 0.0));
    encoder.reset().unwrap();
    decoder.reset().unwrap();
    let len = encoder.encode(&two_tones(6), &mut packet).unwrap();
    assert_eq!(decoder.decode(&packet[..len], &mut out).unwrap(), FRAME);
    assert!(matches!(
        encoder.encode(&[0.0; FRAME], &mut packet),
        Err(StereoError::FrameLength { .. })
    ));
    assert!(matches!(
        decoder.decode(&packet[..len], &mut [0.0; FRAME]),
        Err(StereoError::BufferTooShort { .. })
    ));
}

// Only one stereo CELT frame of 10 ms passes, at any bandwidth, padded by
// Opus or not; everything else is refused before libopus sees it.
#[test]
fn only_what_booth_sends_is_read() {
    let toc = |config: u8, stereo: bool, code: u8| (config << 3) | (u8::from(stereo) << 2) | code;
    for config in [18, 22, 26, 30] {
        assert!(FrameInfo::read(&[toc(config, true, 0), 1, 2]).is_ok());
        assert!(FrameInfo::read(&[toc(config, true, 3), 1, 0]).is_ok());
    }
    let refused = |packet: &[u8]| FrameInfo::read(packet).unwrap_err().to_string();
    assert_eq!(refused(&[]), "pc sound frame is empty");
    assert_eq!(
        refused(&[toc(30, false, 0), 1]),
        "pc sound frame is mono; Booth sends stereo"
    );
    assert_eq!(
        refused(&[toc(29, true, 0), 1]),
        "pc sound frame holds a 5 ms frame; Booth sends 10 ms"
    );
    assert_eq!(
        refused(&[toc(31, true, 0), 1]),
        "pc sound frame holds a 20 ms frame; Booth sends 10 ms"
    );
    // SILK and hybrid.
    for config in [1, 9, 13] {
        assert_eq!(
            refused(&[toc(config, true, 0), 1]),
            "pc sound frame is not CELT; Booth sends CELT only"
        );
    }
    assert_eq!(
        refused(&[toc(30, true, 1), 1, 2]),
        "pc sound frame holds 2 frames; Booth sends one per packet"
    );
    assert_eq!(
        refused(&[toc(30, true, 3), 2, 0]),
        "pc sound frame holds 2 frames; Booth sends one per packet"
    );
    let long = vec![toc(30, true, 0); MAX_PACKET + 1];
    assert_eq!(
        refused(&long),
        "pc sound frame of 161 bytes is longer than the 160 Booth accepts"
    );
    // Whatever bytes come, they are read or refused, never a panic.
    let mut seed = 0x9e37_79b9u32;
    for _ in 0..20_000 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let len = (seed >> 24) as usize % (MAX_PACKET + 8);
        let bytes: Vec<u8> = (0..len)
            .map(|i| (seed.rotate_left(i as u32 % 32) >> 3) as u8)
            .collect();
        let _ = FrameInfo::read(&bytes);
    }
}
