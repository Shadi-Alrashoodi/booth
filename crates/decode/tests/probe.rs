// What the probe says on this PC's GPUs and how long it takes. It asks the
// driver only: nothing is decoded, drawn or read back.

mod common;

use std::time::{Duration, Instant};

use decode::{Codec, DecodeError, probe};

const SIZES: [(u32, u32); 5] = [
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (8192, 64),
];
const ASKED: usize = 200;

#[test]
fn every_gpu_answers() {
    let _turn = common::turn();
    let adapters = capture::adapters().unwrap_or_else(|e| panic!("{e}"));
    assert!(!adapters.is_empty(), "no hardware GPU on this PC");
    for adapter in &adapters {
        let device = capture::device_on(adapter).unwrap_or_else(|e| panic!("{e}"));
        println!("{}:", adapter.description);
        let mut first = None;
        let mut times = Vec::new();
        for codec in [Codec::H264, Codec::Hevc] {
            for (width, height) in SIZES {
                let started = Instant::now();
                let answer = probe(&device, codec, width, height);
                first.get_or_insert(started.elapsed());
                for _ in 0..ASKED {
                    let started = Instant::now();
                    let again = probe(&device, codec, width, height);
                    times.push(started.elapsed());
                    assert_eq!(again.is_ok(), answer.is_ok(), "the same question twice");
                }
                match &answer {
                    Ok(()) => println!("  {codec} {width}x{height}: decodes"),
                    Err(err) => println!("  {codec} {width}x{height}: {err}"),
                }
                if let Err(err) = &answer {
                    assert!(
                        matches!(err, DecodeError::NotDecodable { .. }),
                        "an answer, not a failure: {err:?}"
                    );
                }
                // NVIDIA's decoders take both codecs at the sizes Booth
                // shares and H.264 no wider than 4096 (tests/peer_sps.rs
                // finds the same through FFmpeg). HEVC as wide as 8192 but
                // only 64 lines tall is printed, not checked.
                let expected = match (codec, width) {
                    (Codec::H264, 8192) => Some(false),
                    (Codec::Hevc, 8192) => None,
                    _ => Some(true),
                };
                if adapter.vendor_id == common::NVIDIA
                    && let Some(expected) = expected
                {
                    assert_eq!(answer.is_ok(), expected, "{codec} {width}x{height}");
                }
            }
        }
        let (median, p95, max) = common::spread(&times);
        let first = common::ms(first.unwrap_or_default());
        println!(
            "  the first answer on a new device took {first:.3} ms; {} more took median {median:.3} ms, 95th {p95:.3}, max {max:.3}",
            times.len()
        );
        assert!(
            median < 1.0,
            "the probe is asked when someone presses Watch; median {median:.3} ms"
        );
        assert!(
            max < 50.0 && first < 50.0,
            "a probe took {max:.3} ms, the first {first:.3}"
        );
    }
}

#[test]
fn oversized_on_every_gpu() {
    let _turn = common::turn();
    for adapter in capture::adapters().unwrap_or_else(|e| panic!("{e}")) {
        let device = capture::device_on(&adapter).unwrap_or_else(|e| panic!("{e}"));
        for codec in [Codec::H264, Codec::Hevc] {
            let started = Instant::now();
            let err = probe(&device, codec, 8192, 4608).unwrap_err();
            assert!(started.elapsed() < Duration::from_millis(5));
            assert!(
                matches!(
                    err,
                    DecodeError::Oversized {
                        width: 8192,
                        height: 4608
                    }
                ),
                "{}: {err:?}",
                adapter.description
            );
        }
    }
}
