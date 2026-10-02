use std::f32::consts::TAU;
use std::time::Instant;

use proptest::prelude::*;
use voice::codec::{
    CodecError, Decoder, Encoder, Layer, MAX_FRAME, MAX_PACKET, Mode, PacketInfo, SAMPLE_RATE,
};

// Harmonics of a gliding pitch through three formants, switched on and off at
// a syllable rate, plus a little noise. Enough like speech to make CELT and
// SILK work for their bits, and silent between syllables.
fn speech_like(samples: usize) -> Vec<f32> {
    let mut noise = 0x2545_f491_4f6c_dd1du64;
    let mut phase = 0.0f32;
    (0..samples)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let pitch = 120.0 + 40.0 * (TAU * 0.7 * t).sin();
            phase = (phase + pitch / SAMPLE_RATE as f32).fract();
            let syllable = (TAU * 4.0 * t).sin().max(0.0);
            let voiced: f32 = (1..30)
                .map(|k| {
                    let hz = pitch * k as f32;
                    let formants = (-((hz - 700.0) / 300.0).powi(2)).exp()
                        + 0.5 * (-((hz - 1200.0) / 400.0).powi(2)).exp()
                        + 0.3 * (-((hz - 2500.0) / 500.0).powi(2)).exp();
                    formants * (TAU * phase * k as f32).sin()
                })
                .sum();
            noise ^= noise << 13;
            noise ^= noise >> 7;
            noise ^= noise << 17;
            let hiss = (noise >> 40) as f32 / (1u64 << 23) as f32 - 1.0;
            syllable * (0.25 * voiced + 0.02 * hiss)
        })
        .collect()
}

fn sine(hz: f32, amplitude: f32, samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|i| amplitude * (TAU * hz * i as f32 / SAMPLE_RATE as f32).sin())
        .collect()
}

fn encode_all(encoder: &mut Encoder, signal: &[f32]) -> Vec<Vec<u8>> {
    let mut out = [0u8; MAX_PACKET];
    signal
        .chunks_exact(encoder.mode().frame_samples())
        .map(|frame| {
            let len = encoder.encode(frame, &mut out).unwrap();
            out[..len].to_vec()
        })
        .collect()
}

fn decode_all(packets: &[Vec<u8>]) -> Vec<f32> {
    let mut decoder = Decoder::new().unwrap();
    let mut frame = [0f32; MAX_FRAME];
    let mut audio = Vec::new();
    for packet in packets {
        let len = decoder.decode(packet, &mut frame).unwrap();
        audio.extend_from_slice(&frame[..len]);
    }
    audio
}

// The codec delays its output by its lookahead plus the decoder's own delay,
// which differs per mode and layer, so the best of all small lags is used.
// The first 100 ms are skipped while the codec settles.
fn snr_db(reference: &[f32], decoded: &[f32]) -> f32 {
    let skip = SAMPLE_RATE as usize / 10;
    (0..700)
        .map(|lag| {
            let end = reference.len().min(decoded.len() - lag);
            let (mut signal, mut error) = (0.0f64, 0.0f64);
            for i in skip..end {
                let r = f64::from(reference[i]);
                let e = r - f64::from(decoded[i + lag]);
                signal += r * r;
                error += e * e;
            }
            (10.0 * (signal / error).log10()) as f32
        })
        .fold(f32::MIN, f32::max)
}

#[test]
fn low_delay_packets_are_one_size_for_speech_and_silence() {
    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    let mut signal = speech_like(SAMPLE_RATE as usize * 2);
    signal.extend(std::iter::repeat_n(0.0, SAMPLE_RATE as usize));
    signal.extend(sine(3000.0, 1.0, SAMPLE_RATE as usize / 2));
    let packets = encode_all(&mut encoder, &signal);
    assert_eq!(packets.len(), signal.len() / 240);
    for packet in &packets {
        assert_eq!(packet.len(), 20);
        let info = PacketInfo::read(packet).unwrap();
        assert_eq!(info.layer, Layer::Celt);
        assert_eq!(info.samples, 240);
        assert_eq!(info.mode(), Mode::LowDelay);
    }
}

#[test]
fn repair_packets_are_one_size_for_speech_and_silence() {
    let mut encoder = Encoder::new(Mode::Repair, true).unwrap();
    encoder.set_expected_loss(20).unwrap();
    let mut signal = speech_like(SAMPLE_RATE as usize * 2);
    signal.extend(std::iter::repeat_n(0.0, SAMPLE_RATE as usize));
    let packets = encode_all(&mut encoder, &signal);
    for packet in &packets {
        assert_eq!(packet.len(), 40);
        let info = PacketInfo::read(packet).unwrap();
        assert_eq!(info.samples, 480);
        assert!(info.can_carry_repair(), "{:?}", info.layer);
    }
}

#[test]
fn variable_rate_stays_under_the_cap() {
    for mode in [Mode::LowDelay, Mode::Repair] {
        let mut encoder = Encoder::new(mode, false).unwrap();
        let mut signal = speech_like(SAMPLE_RATE as usize * 2);
        signal.extend(std::iter::repeat_n(0.0, SAMPLE_RATE as usize));
        let sizes: Vec<usize> = encode_all(&mut encoder, &signal)
            .iter()
            .map(Vec::len)
            .collect();
        assert!(sizes.iter().all(|&len| len <= mode.packet_bytes()));
        assert!(sizes.iter().any(|&len| len < mode.packet_bytes()));

        encoder.set_constant_rate(true).unwrap();
        assert!(
            encode_all(&mut encoder, &signal)
                .iter()
                .all(|packet| packet.len() == mode.packet_bytes())
        );
    }
}

// Floors: 20 dB in the low-delay mode and 15 dB in the repair mode, for a
// 440 Hz tone at half scale. Measured with libopus 1.6.1: 27.6 and
// 19.9 dB, the second lower because repair data takes its share of the
// bits. A wrong sample format or a broken mode setting lands far below.
#[test]
fn a_sine_comes_back_above_the_snr_floor() {
    for (mode, floor) in [(Mode::LowDelay, 20.0), (Mode::Repair, 15.0)] {
        let signal = sine(440.0, 0.5, SAMPLE_RATE as usize);
        let mut encoder = Encoder::new(mode, true).unwrap();
        let decoded = decode_all(&encode_all(&mut encoder, &signal));
        assert_eq!(decoded.len(), signal.len());
        let snr = snr_db(&signal, &decoded);
        println!("{mode:?}: {snr:.1} dB");
        assert!(snr > floor, "{mode:?}: {snr:.1} dB is under {floor} dB");
    }
}

#[test]
fn concealment_makes_a_whole_frame_and_decoding_carries_on() {
    for mode in [Mode::LowDelay, Mode::Repair] {
        let samples = mode.frame_samples();
        let mut encoder = Encoder::new(mode, true).unwrap();
        let packets = encode_all(&mut encoder, &sine(440.0, 0.5, SAMPLE_RATE as usize / 5));
        let mut decoder = Decoder::new().unwrap();
        let mut frame = [0f32; MAX_FRAME];
        for (i, packet) in packets.iter().enumerate() {
            let len = if i % 5 == 3 {
                decoder.conceal(samples, &mut frame).unwrap()
            } else {
                decoder.decode(packet, &mut frame).unwrap()
            };
            assert_eq!(len, samples);
            assert!(frame[..len].iter().all(|s| s.is_finite()));
        }
        // Right after a loss the concealed tone is still there, not silence.
        decoder.conceal(samples, &mut frame).unwrap();
        let peak = frame[..samples].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1, "{mode:?}: concealment peak {peak}");
    }
}

#[test]
fn fec_rebuilds_a_lost_10ms_frame_better_than_concealment() {
    let mut encoder = Encoder::new(Mode::Repair, true).unwrap();
    // Nothing reported: the repair mode must still carry repair data.
    encoder.set_expected_loss(0).unwrap();
    let packets = encode_all(&mut encoder, &speech_like(SAMPLE_RATE as usize * 3));

    let (mut fec_error, mut plc_error) = (0.0f64, 0.0f64);
    for lost in (20..packets.len() - 2).step_by(7) {
        let mut reference = Decoder::new().unwrap();
        let mut repaired = Decoder::new().unwrap();
        let mut concealed = Decoder::new().unwrap();
        let mut frame = [0f32; MAX_FRAME];
        for packet in &packets[..lost] {
            reference.decode(packet, &mut frame).unwrap();
            repaired.decode(packet, &mut frame).unwrap();
            concealed.decode(packet, &mut frame).unwrap();
        }
        let mut want = [0f32; 480];
        let mut got = [0f32; 480];
        let mut guess = [0f32; 480];
        assert_eq!(reference.decode(&packets[lost], &mut want).unwrap(), 480);
        assert_eq!(
            repaired.recover(&packets[lost + 1], 480, &mut got).unwrap(),
            480
        );
        assert_eq!(concealed.conceal(480, &mut guess).unwrap(), 480);
        for i in 0..480 {
            fec_error += f64::from(want[i] - got[i]).powi(2);
            plc_error += f64::from(want[i] - guess[i]).powi(2);
        }
        // The packet the repair data came from still decodes in full.
        assert_eq!(
            repaired.decode(&packets[lost + 1], &mut frame).unwrap(),
            480
        );
    }
    println!("error energy: fec {fec_error:.2}, concealment {plc_error:.2}");
    assert!(
        fec_error * 2.0 < plc_error,
        "fec {fec_error:.2} is not well under concealment {plc_error:.2}"
    );
}

#[test]
fn one_decoder_follows_mode_switches() {
    let signal = sine(300.0, 0.4, SAMPLE_RATE as usize);
    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    let mut packets = Vec::new();
    let mut at = 0;
    for mode in [Mode::LowDelay, Mode::Repair, Mode::LowDelay, Mode::Repair] {
        encoder.set_mode(mode).unwrap();
        assert_eq!(encoder.mode(), mode);
        let frames = &signal[at..at + 9600];
        at += 9600;
        for packet in encode_all(&mut encoder, frames) {
            assert_eq!(packet.len(), mode.packet_bytes());
            assert_eq!(PacketInfo::read(&packet).unwrap().mode(), mode);
            packets.push(packet);
        }
    }
    assert_eq!(packets.len(), 40 + 20 + 40 + 20);

    let mut decoder = Decoder::new().unwrap();
    let mut frame = [0f32; MAX_FRAME];
    let mut total = 0;
    for packet in &packets {
        let len = decoder.decode(packet, &mut frame).unwrap();
        assert_eq!(len, PacketInfo::read(packet).unwrap().samples);
        total += len;
    }
    assert_eq!(total, 4 * 9600);
    // The tone is still coming out at the end, after three switches.
    let peak = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.2, "peak after switching {peak}");
}

#[test]
fn toc_parsing_accepts_only_one_mono_5_or_10_ms_frame() {
    let toc = |config: u8, stereo: bool, code: u8| config << 3 | u8::from(stereo) << 2 | code;
    let ok = |packet: &[u8]| PacketInfo::read(packet).unwrap();
    let refused = |packet: &[u8]| {
        matches!(
            PacketInfo::read(packet),
            Err(CodecError::UnsupportedPacket { .. })
        )
    };

    let celt5 = ok(&[toc(29, false, 0), 0, 0]);
    assert_eq!(
        (celt5.layer, celt5.samples, celt5.mode()),
        (Layer::Celt, 240, Mode::LowDelay)
    );
    assert!(!celt5.can_carry_repair());
    let padded = ok(&[toc(29, false, 3), 0x41, 2, 0, 0, 0]);
    assert_eq!(padded.samples, 240);
    let celt10 = ok(&[toc(30, false, 0), 0]);
    assert_eq!((celt10.samples, celt10.mode()), (480, Mode::Repair));
    assert!(!celt10.can_carry_repair());
    let silk = ok(&[toc(8, false, 3), 0x01, 0]);
    assert_eq!((silk.layer, silk.samples), (Layer::Silk, 480));
    assert!(silk.can_carry_repair());
    let hybrid = ok(&[toc(12, false, 0), 0]);
    assert_eq!((hybrid.layer, hybrid.mode()), (Layer::Hybrid, Mode::Repair));

    assert!(refused(&[toc(29, true, 0), 0]));
    assert!(refused(&[toc(29, false, 1), 0, 0]));
    assert!(refused(&[toc(29, false, 2), 1, 0, 0]));
    assert!(refused(&[toc(29, false, 3), 0x02, 0, 0]));
    assert!(refused(&[toc(29, false, 3)]));
    assert!(refused(&[toc(28, false, 0), 0]));
    assert!(refused(&[toc(31, false, 0), 0]));
    assert!(refused(&[toc(9, false, 0), 0]));
    assert!(refused(&[toc(13, false, 0), 0]));
    assert!(matches!(
        PacketInfo::read(&[]),
        Err(CodecError::EmptyPacket)
    ));
    assert!(matches!(
        PacketInfo::read(&[toc(29, false, 0); MAX_PACKET + 1]),
        Err(CodecError::PacketTooLong(len)) if len == MAX_PACKET + 1
    ));
}

#[test]
fn misuse_is_an_error_with_a_plain_message() {
    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    let mut out = [0u8; MAX_PACKET];
    let err = encoder.encode(&[0.0; 480], &mut out).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the encoder got 480 samples; a frame in this mode is 240"
    );
    let err = encoder.encode(&[0.0; 240], &mut out[..10]).unwrap_err();
    assert!(matches!(
        err,
        CodecError::BufferTooShort {
            needed: 20,
            actual: 10
        }
    ));

    let mut decoder = Decoder::new().unwrap();
    let mut frame = [0f32; MAX_FRAME];
    assert!(matches!(
        decoder.conceal(100, &mut frame),
        Err(CodecError::NotAFrame(100))
    ));
    assert!(matches!(
        decoder.conceal(480, &mut frame[..240]),
        Err(CodecError::BufferTooShort {
            needed: 480,
            actual: 240
        })
    ));
    let refusal = |packet: &[u8]| PacketInfo::read(packet).unwrap_err().to_string();
    assert_eq!(
        refusal(&[29 << 3 | 1, 0, 0]),
        "voice packet holds 2 frames; Booth sends one per packet"
    );
    assert_eq!(
        refusal(&[31 << 3, 0]),
        "voice packet holds a 20 ms frame; Booth sends 5 or 10 ms"
    );
    assert_eq!(
        refusal(&[29 << 3 | 4, 0]),
        "voice packet is stereo; Booth sends mono"
    );
    // The decoder refuses the same packets without passing them to libopus.
    assert!(matches!(
        decoder.decode(&[29 << 3 | 1, 0, 0], &mut frame),
        Err(CodecError::UnsupportedPacket { frames: 2, .. })
    ));
}

// Printed, not checked: debug builds of libopus are several times slower
// than the release build the figures beside COMPLEXITY come from.
#[test]
fn encode_time_per_frame() {
    for mode in [Mode::LowDelay, Mode::Repair] {
        let mut encoder = Encoder::new(mode, true).unwrap();
        let signal = speech_like(SAMPLE_RATE as usize * 2);
        let mut out = [0u8; MAX_PACKET];
        let mut times: Vec<f64> = signal
            .chunks_exact(mode.frame_samples())
            .map(|frame| {
                let start = Instant::now();
                encoder.encode(frame, &mut out).unwrap();
                start.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        times.sort_by(f64::total_cmp);
        println!(
            "{mode:?}: encode median {:.0} us, 99th percentile {:.0} us per {} ms frame",
            times[times.len() / 2],
            times[times.len() * 99 / 100],
            mode.frame_ms()
        );
    }
}

proptest! {
    // libopus is C and sees these bytes after the handshake.
    // Whatever a peer sends, decoding is an error or one frame, never a crash.
    #[test]
    fn any_bytes_decode_to_a_frame_or_an_error(packet in prop::collection::vec(any::<u8>(), 0..MAX_PACKET + 8)) {
        let mut decoder = Decoder::new().unwrap();
        let mut frame = [0f32; MAX_FRAME];
        if let Ok(len) = decoder.decode(&packet, &mut frame) {
            prop_assert!(len == 240 || len == 480);
            prop_assert!(frame[..len].iter().all(|s| s.is_finite()));
        }
        if let Ok(len) = decoder.recover(&packet, 480, &mut frame) {
            prop_assert_eq!(len, 480);
        }
    }
}

// Mouth to ear adds the lookahead from Mode, so it has to be what libopus
// really holds back.
#[test]
fn the_lookahead_written_down_is_what_libopus_holds_back() {
    for mode in [Mode::LowDelay, Mode::Repair] {
        let mut encoder = Encoder::new(mode, true).unwrap();
        assert_eq!(
            encoder.lookahead().unwrap(),
            mode.lookahead_samples(),
            "{mode:?}"
        );
    }
}
