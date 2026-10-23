use voice::audio::{
    Format, FormatError, Sample, from_mono, from_stereo, interleaved, plan, to_mono,
};

// WAVEFORMATEX, then for the extensible form: valid bits, channel mask and
// the sub-format GUID.
fn wave(tag: u16, channels: u16, rate: u32, bits: u16, extra: &[u8]) -> Vec<u8> {
    let block = channels * bits / 8;
    let mut out = Vec::new();
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    out.extend_from_slice(extra);
    out
}

const TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

fn extensible(channels: u16, rate: u32, bits: u16, valid: u16, mask: u32, kind: u32) -> Vec<u8> {
    let mut extra = Vec::new();
    extra.extend_from_slice(&valid.to_le_bytes());
    extra.extend_from_slice(&mask.to_le_bytes());
    extra.extend_from_slice(&kind.to_le_bytes());
    extra.extend_from_slice(&TAIL);
    wave(0xFFFE, channels, rate, bits, &extra)
}

#[test]
fn reads_the_formats_windows_hands_out() {
    // The usual engine format: float stereo at 48 kHz.
    let stereo = Format::parse(&extensible(2, 48_000, 32, 32, 0b11, 3)).unwrap();
    assert_eq!(
        stereo,
        Format {
            rate: 48_000,
            channels: 2,
            sample: Sample::F32,
            channel_mask: 0b11,
        }
    );
    assert_eq!(stereo.to_string(), "48 kHz, 2 channels, 32-bit float");
    // A Bluetooth headset's microphone in hands-free mode.
    let headset = Format::parse(&wave(1, 1, 16_000, 16, &[])).unwrap();
    assert_eq!((headset.rate, headset.sample), (16_000, Sample::I16));
    let float = Format::parse(&wave(3, 2, 44_100, 32, &[])).unwrap();
    assert_eq!(float.sample, Sample::F32);
    let packed = Format::parse(&extensible(2, 96_000, 24, 24, 0b11, 1)).unwrap();
    assert_eq!(packed.sample, Sample::I24);
    // 24 valid bits in a 32-bit container read as 32-bit.
    let padded = Format::parse(&extensible(8, 48_000, 32, 24, 0x63F, 1)).unwrap();
    assert_eq!((padded.sample, padded.channels), (Sample::I32, 8));
    assert_eq!(padded.frame_bytes(), 32);
}

#[test]
fn refuses_what_it_cannot_read() {
    let mut adpcm = wave(1, 1, 8_000, 16, &[]);
    adpcm[0] = 2;
    let mut not_audio = extensible(2, 48_000, 32, 32, 0b11, 3);
    not_audio[39] = 0;
    let mut block = wave(1, 2, 48_000, 16, &[]);
    block[12] = 3;
    let cases = [
        (&wave(1, 2, 48_000, 16, &[])[..10], FormatError::Short(10)),
        (&adpcm[..], FormatError::Tag(2)),
        (
            &extensible(2, 48_000, 32, 32, 0b11, 3)[..30],
            FormatError::Short(30),
        ),
        (
            &not_audio[..],
            FormatError::SubFormat(String::from("{00000003-0000-0010-8000-00aa00389b00}")),
        ),
        (
            &wave(1, 1, 8_000, 8, &[])[..],
            FormatError::Bits {
                bits: 8,
                float: false,
            },
        ),
        (
            &wave(3, 2, 48_000, 64, &[])[..],
            FormatError::Bits {
                bits: 64,
                float: true,
            },
        ),
        (&wave(1, 0, 48_000, 16, &[])[..], FormatError::Channels(0)),
        (&wave(1, 2, 0, 16, &[])[..], FormatError::Rate),
        (
            &block[..],
            FormatError::BlockAlign {
                block_align: 3,
                channels: 2,
                bits: 16,
            },
        ),
    ];
    for (bytes, expected) in cases {
        assert_eq!(Format::parse(bytes), Err(expected.clone()), "{expected}");
    }
    assert_eq!(
        FormatError::Tag(2).to_string(),
        "format tag 0x0002 is not PCM or float"
    );
}

#[test]
fn a_format_made_for_windows_reads_back_the_same() {
    for sample in [Sample::F32, Sample::I16, Sample::I24, Sample::I32] {
        let format = Format {
            rate: 48_000,
            channels: 6,
            sample,
            channel_mask: 0x3F,
        };
        assert_eq!(Format::parse(&format.to_bytes()), Ok(format));
    }
}

// The 48 kHz check: at the engine's own 48 kHz the stream takes the engine
// format and may use the small period; anything else is converted by
// Windows to float at 48 kHz, keeping the channels.
#[test]
fn only_a_48_khz_engine_is_used_as_it_is() {
    let engine = Format::parse(&extensible(2, 48_000, 32, 32, 0b11, 3)).unwrap();
    let as_is = plan(&engine);
    assert!(!as_is.resampled);
    assert_eq!(as_is.format, engine);
    for (engine, channels) in [
        (
            Format::parse(&extensible(2, 44_100, 16, 16, 0b11, 1)).unwrap(),
            2,
        ),
        (Format::parse(&wave(1, 1, 16_000, 16, &[])).unwrap(), 1),
        (Format::parse(&wave(3, 2, 96_000, 32, &[])).unwrap(), 2),
    ] {
        let converted = plan(&engine);
        assert!(converted.resampled, "{engine}");
        assert_eq!(converted.format.rate, 48_000);
        assert_eq!(converted.format.sample, Sample::F32);
        assert_eq!(converted.format.channels, channels);
        assert_eq!(converted.format.channel_mask, engine.channel_mask);
    }
}

fn f32_frames(frames: &[&[f32]]) -> Vec<u8> {
    frames
        .iter()
        .flat_map(|frame| frame.iter().flat_map(|s| s.to_le_bytes()))
        .collect()
}

#[test]
fn stereo_becomes_mono_by_averaging() {
    let format = Format::float_48k(2, 0b11);
    let data = f32_frames(&[&[0.5, -0.5], &[1.0, 0.0], &[-0.25, -0.75], &[0.3, 0.3]]);
    let mut mono = vec![9.0];
    to_mono(&data, &format, &mut mono);
    assert_eq!(mono, [9.0, 0.0, 0.5, -0.5, 0.3]);
    // A frame cut short at the end is left out.
    let mut mono = Vec::new();
    to_mono(&data[..data.len() - 1], &format, &mut mono);
    assert_eq!(mono.len(), 3);
}

const LAYOUTS: [(u16, u32); 11] = [
    (1, 0),
    (2, 0),
    (4, 0),
    (6, 0),
    (8, 0),
    (2, 0x3),
    // Quad, 5.1 with back speakers, 5.1 with side speakers, 7.1.
    (4, 0x33),
    (6, 0x3F),
    (6, 0x60F),
    (8, 0x63F),
    // A mask with fewer speakers than channels.
    (6, 0x3),
];

// A voice through the stereo path, the same on both sides, comes out byte
// for byte as through the mono one, so a listener who never gets PC sound
// hears exactly what they did.
#[test]
fn the_same_on_both_sides_is_what_mono_writes() {
    let mut seed = 0x2545_f491u32;
    let mut random = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 * 3.0 - 1.5
    };
    let mut mono = vec![
        0.0,
        -0.0,
        0.5,
        -0.5,
        1.0,
        -1.0,
        2.0,
        -2.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        f32::from_bits(1),
        f32::MAX,
    ];
    mono.extend((0..500).map(|_| random()));
    let stereo: Vec<f32> = mono.iter().flat_map(|&m| [m, m]).collect();
    for sample in [Sample::F32, Sample::I16, Sample::I24, Sample::I32] {
        for (channels, channel_mask) in LAYOUTS {
            let format = Format {
                rate: 48_000,
                channels,
                sample,
                channel_mask,
            };
            let mut through_mono = vec![0u8; mono.len() * format.frame_bytes()];
            let mut through_stereo = vec![1u8; mono.len() * format.frame_bytes()];
            from_mono(&mono, &format, &mut through_mono);
            from_stereo(&stereo, &format, &mut through_stereo);
            assert_eq!(through_mono, through_stereo, "{format:?}");
        }
    }
}

#[test]
fn each_speaker_gets_its_side_of_the_mix() {
    let (left, right) = (0.5, -0.25);
    let middle = (left + right) * 0.5;
    for ((channels, channel_mask), expected) in [
        ((1, 0), vec![middle]),
        // One speaker plays both sides, whichever its mask names.
        ((1, 0x1), vec![middle]),
        ((1, 0x4), vec![middle]),
        ((2, 0), vec![left, right]),
        ((4, 0), vec![left, right, middle, middle]),
        ((2, 0x3), vec![left, right]),
        ((4, 0x33), vec![left, right, left, right]),
        // Front left and right, centre, LFE, back left and right.
        ((6, 0x3F), vec![left, right, middle, middle, left, right]),
        // The same with side speakers.
        ((6, 0x60F), vec![left, right, middle, middle, left, right]),
        (
            (8, 0x63F),
            vec![left, right, middle, middle, left, right, left, right],
        ),
        ((6, 0x3), vec![left, right, middle, middle, middle, middle]),
    ] {
        let format = Format {
            rate: 48_000,
            channels,
            sample: Sample::F32,
            channel_mask,
        };
        let mut out = vec![0u8; format.frame_bytes()];
        from_stereo(&[left, right], &format, &mut out);
        let (samples, _) = out.as_chunks::<4>();
        let got: Vec<f32> = samples.iter().map(|b| f32::from_le_bytes(*b)).collect();
        assert_eq!(got, expected, "{channels} channels, mask {channel_mask:#x}");
    }
}

#[test]
fn stereo_stays_as_it_is_and_bad_samples_go_silent() {
    let format = Format::float_48k(2, 0b11);
    let data = f32_frames(&[&[0.5, -0.5], &[f32::NAN, 1.0], &[-0.25, f32::INFINITY]]);
    let mut stereo = vec![9.0];
    interleaved(&data, &format, &mut stereo);
    assert_eq!(stereo, [9.0, 0.5, -0.5, 0.0, 1.0, -0.25, 0.0]);
    // A frame cut short at the end is left out, its first channel too.
    let mut stereo = Vec::new();
    interleaved(&data[..data.len() - 1], &format, &mut stereo);
    assert_eq!(stereo.len(), 4);

    let i16_format = Format {
        sample: Sample::I16,
        ..format
    };
    let data: Vec<u8> = [i16::MIN, 16_384, 0, -16_384]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let mut stereo = Vec::new();
    interleaved(&data, &i16_format, &mut stereo);
    assert_eq!(stereo, [-1.0, 0.5, 0.0, -0.5]);
}

#[test]
fn integer_samples_read_to_the_same_scale() {
    let i16_format = Format {
        rate: 48_000,
        channels: 1,
        sample: Sample::I16,
        channel_mask: 0,
    };
    let data: Vec<u8> = [i16::MIN, 0, 16_384]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let mut mono = Vec::new();
    to_mono(&data, &i16_format, &mut mono);
    assert_eq!(mono, [-1.0, 0.0, 0.5]);

    let i24_format = Format {
        sample: Sample::I24,
        ..i16_format
    };
    // -2^23, 2^22 and -1, three bytes each.
    let data = [0x00, 0x00, 0x80, 0x00, 0x00, 0x40, 0xFF, 0xFF, 0xFF];
    let mut mono = Vec::new();
    to_mono(&data, &i24_format, &mut mono);
    assert_eq!(mono[0], -1.0);
    assert_eq!(mono[1], 0.5);
    assert!(mono[2] < 0.0 && mono[2] > -1e-6);

    let i32_format = Format {
        sample: Sample::I32,
        channels: 2,
        ..i16_format
    };
    let data: Vec<u8> = [i32::MIN, i32::MIN]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let mut mono = Vec::new();
    to_mono(&data, &i32_format, &mut mono);
    assert_eq!(mono, [-1.0]);
}

#[test]
fn mono_goes_to_every_channel_in_any_sample_format() {
    let mono = [0.5, -0.25, 0.0];
    for sample in [Sample::F32, Sample::I16, Sample::I24, Sample::I32] {
        let format = Format {
            rate: 48_000,
            channels: 3,
            sample,
            channel_mask: 0,
        };
        let mut bytes = vec![0xAA; mono.len() * format.frame_bytes()];
        from_mono(&mono, &format, &mut bytes);
        let size = sample.bytes();
        for frame in bytes.chunks_exact(format.frame_bytes()) {
            let first = &frame[..size];
            assert!(frame.chunks_exact(size).all(|c| c == first), "{sample:?}");
        }
        let mut back = Vec::new();
        to_mono(&bytes, &format, &mut back);
        for (was, is) in mono.iter().zip(&back) {
            assert!((was - is).abs() < 1.0 / 32_000.0, "{sample:?}: {was} {is}");
        }
    }
}

#[test]
fn what_would_hurt_a_speaker_goes_out_as_limits_or_silence() {
    let format = Format::float_48k(1, 0);
    let mut bytes = vec![0u8; 4 * 4];
    from_mono(&[f32::NAN, 3.0, -7.0, f32::INFINITY], &format, &mut bytes);
    let mut back = Vec::new();
    to_mono(&bytes, &format, &mut back);
    assert_eq!(back, [0.0, 1.0, -1.0, 0.0]);

    let i16_format = Format {
        sample: Sample::I16,
        ..format
    };
    let mut bytes = vec![0u8; 2 * 2];
    from_mono(&[2.0, -2.0], &i16_format, &mut bytes);
    assert_eq!(bytes, [0xFF, 0x7F, 0x01, 0x80]);
}
