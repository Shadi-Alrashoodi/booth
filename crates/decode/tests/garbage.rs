// What a friend's PC could send once the packet is decrypted: bytes that are
// no stream at all, frames cut short, frames with bytes flipped. Each case
// runs the real decoder, so there are few of them. The decoder must answer
// every one with an error or a picture, never a crash or a hang, and the
// next IDR must decode whole. The same for H.264 and for HEVC.

mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use decode::{Codec, DecodeError, Decoder};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
// The stream every case works from: an IDR, P frames, a second IDR the
// case ends with, and a P frame after it.
const DAMAGEABLE: usize = 6;
const RECOVERY_IDR: usize = 6;
const FRAMES: usize = 8;
// A decode that takes longer is as good as a hang.
const PATIENCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
enum Garbage {
    Random(Vec<u8>),
    // A start code, the NAL header of a slice or parameter set, and noise,
    // so the noise reaches the slice and parameter set parsers.
    Nal {
        header: Vec<u8>,
        payload: Vec<u8>,
    },
    Truncated {
        unit: usize,
        keep: Index,
    },
    Flipped {
        unit: usize,
        flips: Vec<(Index, u8)>,
    },
}

// H.264 NAL headers are one byte, HEVC ones two: IDR slices, other slices,
// the parameter sets and SEI, and for HEVC a CRA picture as well.
fn headers(codec: Codec) -> Vec<Vec<u8>> {
    match codec {
        Codec::H264 => [0x65, 0x25, 0x41, 0x01, 0x67, 0x68, 0x06]
            .map(|byte| vec![byte])
            .to_vec(),
        Codec::Hevc => [19u8, 20, 21, 1, 0, 32, 33, 34, 39]
            .map(|kind| vec![kind << 1, 1])
            .to_vec(),
    }
}

fn garbage(codec: Codec) -> impl Strategy<Value = (Garbage, usize)> {
    let kind = prop_oneof![
        vec(any::<u8>(), 1..16384).prop_map(Garbage::Random),
        (
            prop::sample::select(headers(codec)),
            vec(any::<u8>(), 1..4096)
        )
            .prop_map(|(header, payload)| Garbage::Nal { header, payload }),
        (0..DAMAGEABLE, any::<Index>()).prop_map(|(unit, keep)| Garbage::Truncated { unit, keep }),
        (0..DAMAGEABLE, vec((any::<Index>(), 1..=255u8), 1..16))
            .prop_map(|(unit, flips)| Garbage::Flipped { unit, flips }),
    ];
    // Stream frames that follow the damage before the IDR.
    (kind, 0..3usize)
}

struct Case<'a> {
    decoder: Decoder,
    reader: Reader,
    units: &'a [Vec<u8>],
    outcomes: BTreeMap<String, u32>,
}

impl Case<'_> {
    fn whole(&mut self, n: usize) -> Result<(), TestCaseError> {
        let started = Instant::now();
        let result = self.decoder.decode(&self.units[n]);
        prop_assert!(
            started.elapsed() < PATIENCE,
            "frame {n} took {:?}",
            started.elapsed()
        );
        let decoded = match result {
            Ok(Some(decoded)) => decoded,
            Ok(None) => return Err(TestCaseError::fail(format!("frame {n} gave no picture"))),
            Err(err) => return Err(TestCaseError::fail(format!("frame {n}: {err}"))),
        };
        prop_assert_eq!(
            self.reader.frame_number(&decoded),
            Some(n as u32),
            "frame {} decoded to the wrong picture",
            n
        );
        Ok(())
    }

    fn damaged(&mut self, what: &str, bytes: &[u8]) -> Result<(), TestCaseError> {
        let started = Instant::now();
        let result = self.decoder.decode(bytes);
        prop_assert!(
            started.elapsed() < PATIENCE,
            "{what} of {} bytes took {:?}",
            bytes.len(),
            started.elapsed()
        );
        let outcome = match result {
            Ok(Some(_)) => "a picture".to_string(),
            Ok(None) => "no picture".to_string(),
            Err(DecodeError::Damaged { detail }) => format!("damaged: {detail}"),
            Err(err) => format!("{err}"),
        };
        *self
            .outcomes
            .entry(format!("{what}: {outcome}"))
            .or_default() += 1;
        Ok(())
    }
}

#[test]
fn h264_garbage() {
    garbage_then_an_idr(Codec::H264);
}

#[test]
fn hevc_garbage() {
    garbage_then_an_idr(Codec::Hevc);
}

fn garbage_then_an_idr(codec: Codec) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..FRAMES)
        .map(|n| stream.next(n == RECOVERY_IDR).data)
        .collect();
    drop(stream);

    let case = RefCell::new(Case {
        decoder,
        reader: Reader::new(&gpu),
        units: &units,
        outcomes: BTreeMap::new(),
    });
    let mut runner = TestRunner::new(Config {
        cases: 128,
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&garbage(codec), |(garbage, after)| {
        let mut case = case.borrow_mut();
        case.whole(0)?;
        let (bytes, resume) = match &garbage {
            Garbage::Random(bytes) => {
                case.whole(1)?;
                (bytes.clone(), 2)
            }
            Garbage::Nal { header, payload } => {
                case.whole(1)?;
                let mut bytes = vec![0, 0, 0, 1];
                bytes.extend_from_slice(header);
                bytes.extend_from_slice(payload);
                (bytes, 2)
            }
            Garbage::Truncated { unit, keep } => {
                for n in 1..*unit {
                    case.whole(n)?;
                }
                let whole = &units[*unit];
                (whole[..keep.index(whole.len() - 1) + 1].to_vec(), unit + 1)
            }
            Garbage::Flipped { unit, flips } => {
                for n in 1..*unit {
                    case.whole(n)?;
                }
                let mut bytes = units[*unit].clone();
                for (at, with) in flips {
                    let at = at.index(bytes.len());
                    bytes[at] ^= with;
                }
                (bytes, unit + 1)
            }
        };
        let what = match &garbage {
            Garbage::Random(_) => "random bytes",
            Garbage::Nal { .. } => "a NAL unit of noise",
            Garbage::Truncated { .. } => "a frame cut short",
            Garbage::Flipped { .. } => "a frame with bytes flipped",
        };
        case.damaged(what, &bytes)?;
        for n in (resume..RECOVERY_IDR).take(after) {
            case.damaged("a stream frame after it", &units[n])?;
        }
        case.whole(RECOVERY_IDR)?;
        case.whole(RECOVERY_IDR + 1)?;
        Ok(())
    });
    if let Err(err) = result {
        panic!("{err}");
    }
    println!("{codec}: what the damaged access units gave:");
    for (outcome, count) in &case.borrow().outcomes {
        println!("  {count:3} {outcome}");
    }
}

// The case the flush before every HEVC IDR (src/decoder.rs) was found by,
// shrunk from a failure of the test above: a CRA of noise and a P frame
// after it. FFmpeg made a stand-in for a reference it could not find with
// the POC the IDR brings, and without the flush it dropped the IDR as a
// duplicate, so the IDR and the frame after it gave no picture.
#[test]
fn cra_of_noise_before_idr() {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(decoder) = common::decoder(&gpu, Codec::Hevc) else {
        return;
    };
    let mut stream = Stream::new(&gpu, Codec::Hevc, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..FRAMES)
        .map(|n| stream.next(n == RECOVERY_IDR).data)
        .collect();
    drop(stream);
    let mut case = Case {
        decoder,
        reader: Reader::new(&gpu),
        units: &units,
        outcomes: BTreeMap::new(),
    };
    if let Err(err) = noise_then_an_idr(&mut case) {
        panic!("{err}");
    }
    for (outcome, count) in &case.outcomes {
        println!("  {count} {outcome}");
    }
}

fn noise_then_an_idr(case: &mut Case<'_>) -> Result<(), TestCaseError> {
    let cra = [0, 0, 0, 1, 21 << 1, 1, 237, 226, 93, 98, 91, 41, 60, 147];
    case.whole(0)?;
    case.whole(1)?;
    case.damaged("a CRA of noise", &cra)?;
    let units = case.units;
    case.damaged("a stream frame after it", &units[2])?;
    case.whole(RECOVERY_IDR)?;
    case.whole(RECOVERY_IDR + 1)
}

// P frames with the stream's parameter sets in front of them and no IDR
// before, as after a reset or from a stream joined in the middle: no
// picture in either codec until the IDR. FFmpeg's H.264 decoder shows
// nothing there by itself; its HEVC decoder would show whatever its pool
// held, so the decoder keeps them from it.
#[test]
fn p_frames_before_first_idr() {
    for codec in common::CODECS {
        p_frames_first(codec);
    }
}

fn p_frames_first(codec: Codec) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..3).map(|_| stream.next(false).data).collect();
    drop(stream);
    let mut reader = Reader::new(&gpu);
    let parameter_set = |nal: &annexb::Nal<'_>| match codec {
        Codec::H264 => matches!(nal.data[0] & 0x1f, 7 | 8),
        Codec::Hevc => matches!(nal.data[0] >> 1 & 0x3f, 32..=34),
    };
    let sets: Vec<u8> = annexb::nal_units(&units[0])
        .filter(parameter_set)
        .flat_map(|nal| [&[0u8, 0, 0, 1][..], nal.data].concat())
        .collect();
    assert!(!sets.is_empty(), "{codec}: no parameter sets in the IDR");
    for (n, unit) in units.iter().enumerate().skip(1) {
        match decoder.decode(&[&sets[..], &unit[..]].concat()) {
            Ok(Some(decoded)) => panic!(
                "{codec}: frame {n} gave a picture with no IDR before it, number {:?}",
                reader.frame_number(&decoded)
            ),
            outcome => println!("{codec}: frame {n} with no IDR before it: {outcome:?}"),
        }
    }
    for (n, unit) in units.iter().enumerate() {
        let decoded = decoder
            .decode(unit)
            .unwrap_or_else(|e| panic!("{codec} frame {n}: {e}"))
            .unwrap_or_else(|| panic!("{codec} frame {n} gave no picture"));
        assert_eq!(
            reader.frame_number(&decoded),
            Some(n as u32),
            "{codec} frame {n}"
        );
    }
}

#[test]
fn empty_and_oversized() {
    for codec in common::CODECS {
        refused_without_decoding(codec);
    }
}

fn refused_without_decoding(codec: Codec) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let empty = decoder.decode(&[]).unwrap_err();
    println!("{empty}");
    assert!(matches!(empty, DecodeError::Empty));
    let size = decode::MAX_ACCESS_UNIT + 1;
    let large = decoder.decode(&vec![0; size]).unwrap_err();
    println!("{large}");
    assert!(matches!(large, DecodeError::TooLarge { size: s } if s == size));
}
