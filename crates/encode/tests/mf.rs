// The hardware encoder the GPU's driver registers with Media Foundation (on
// this PC NVIDIA's) and Windows' software encoder, through the same checks.

mod common;

use encode::annexb::{self, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS, SliceType};
use encode::{AccessUnit, Codec, EncodeError, Encoder, Frame, Kind, Preset, Recovery, Settings};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12};

use common::{Frames, Gpu, References};

#[derive(Clone, Copy)]
struct Case {
    kind: Kind,
    width: u32,
    height: u32,
    fps: u32,
    rate: u32,
}

// The hardware encoder at what Booth shares, the software one at its cap.
const HARDWARE: Case = Case {
    kind: Kind::MfHardware,
    width: 2560,
    height: 1440,
    fps: 120,
    rate: 15_000_000,
};
const SOFTWARE: Case = Case {
    kind: Kind::MfSoftware,
    width: 1920,
    height: 1080,
    fps: 60,
    rate: 8_000_000,
};

impl Case {
    fn frame_bytes(&self, rate: u32) -> f64 {
        f64::from(rate) / f64::from(self.fps) / 8.0
    }
}

/// The first hardware GPU, or WARP when there is none, for the software
/// encoder.
fn gpu() -> Gpu {
    common::hardware().unwrap_or_else(common::warp)
}

/// The encoder, or None with the reason printed when this PC does not have
/// it.
fn open(gpu: &Gpu, case: Case, preset: Preset) -> Option<Box<dyn Encoder>> {
    let settings = Settings {
        bitrate: case.rate,
        preset,
    };
    match encode::open_kind_codec(
        case.kind,
        Codec::H264,
        &gpu.device,
        case.width,
        case.height,
        case.fps,
        &settings,
    ) {
        Ok(encoder) => Some(encoder),
        Err(
            e @ (EncodeError::MediaFoundationMissing
            | EncodeError::NoHardwareEncoder { .. }
            | EncodeError::MediaFoundationExportMissing { .. }),
        ) => {
            println!("skipped: {e}");
            None
        }
        Err(e) => panic!("{} on {}: {e}", case.kind, gpu.name),
    }
}

fn encode(
    encoder: &mut dyn Encoder,
    frames: &mut Frames,
    index: u64,
    force_idr: bool,
) -> AccessUnit {
    let texture = frames.frame(index);
    encoder
        .encode(&Frame {
            texture: &texture,
            index,
            force_idr,
        })
        .unwrap_or_else(|e| panic!("frame {index}: {e}"))
}

fn kinds(unit: &AccessUnit) -> Vec<u8> {
    annexb::nal_units(&unit.data).map(|n| n.kind()).collect()
}

fn has_idr_slice(unit: &AccessUnit) -> bool {
    kinds(unit).contains(&NAL_IDR)
}

fn slice_types(unit: &AccessUnit) -> Vec<SliceType> {
    annexb::nal_units(&unit.data)
        .filter(|n| n.is_slice())
        .filter_map(|n| annexb::slice_type(&n))
        .collect()
}

fn megabits(bytes: usize, seconds: f64) -> f64 {
    bytes as f64 * 8.0 / seconds / 1e6
}

fn first_frame(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    println!("{} on {}", encoder.name(), gpu.name);
    println!("{}", encoder.notes());
    // An encoder that cannot is refused at open, since every loss needs one.
    assert!(
        encoder.notes().contains("IDR on request available"),
        "{}",
        encoder.notes()
    );
    let mut frames = Frames::new(&gpu, case.width, case.height);

    let first = encode(&mut *encoder, &mut frames, 0, false);
    assert!(first.idr, "the first frame must be an IDR");
    let first_kinds = kinds(&first);
    let at = |kind| first_kinds.iter().position(|&k| k == kind);
    let (Some(sps), Some(pps), Some(idr)) = (at(NAL_SPS), at(NAL_PPS), at(NAL_IDR)) else {
        panic!("SPS, PPS and an IDR slice in the first frame: {first_kinds:?}");
    };
    assert!(
        sps < pps && pps < idr,
        "SPS, PPS, then the IDR slice: {first_kinds:?}"
    );
    let sps = annexb::nal_units(&first.data)
        .find(|n| n.kind() == NAL_SPS)
        .and_then(|n| annexb::parse_sps(&n))
        .expect("a readable SPS");
    println!(
        "SPS: profile {}, level {}.{}, {}x{}, {} reference frames, VUI {:?}",
        sps.profile_idc,
        sps.level_idc / 10,
        sps.level_idc % 10,
        sps.width,
        sps.height,
        sps.max_num_ref_frames,
        sps.vui
    );
    assert_eq!((sps.width, sps.height), (case.width, case.height));
    assert_eq!(sps.profile_idc, 100, "High profile");
    // Neither encoder has to write the colour description; the viewer takes
    // BT.709 limited range when the stream says nothing, since that is all
    // capture produces. What a stream does say must be right.
    if let Some(vui) = &sps.vui {
        assert_ne!(vui.full_range, Some(true), "full range in the VUI");
        if let Some(colour) = vui.colour {
            assert_eq!(colour, (1, 1, 1), "BT.709 primaries, transfer and matrix");
        }
        assert!(
            vui.max_num_reorder_frames.unwrap_or(0) == 0,
            "the stream allows reordered frames: {vui:?}"
        );
    }

    for i in 1..240 {
        let unit = encode(&mut *encoder, &mut frames, i, false);
        assert!(
            !unit.idr && !has_idr_slice(&unit),
            "frame {i} is an IDR nobody asked for"
        );
        let types = slice_types(&unit);
        assert!(
            !types.is_empty() && types.iter().all(|&t| t == SliceType::P),
            "frame {i} is not all P slices: {types:?}"
        );
    }
}

#[test]
fn hardware_first_frame() {
    first_frame(HARDWARE);
}

#[test]
fn software_first_frame() {
    first_frame(SOFTWARE);
}

fn forced_idr(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    for i in 0..40 {
        let unit = encode(&mut *encoder, &mut frames, i, i == 25);
        let expected = i == 0 || i == 25;
        assert_eq!(unit.idr, expected, "frame {i}");
        assert_eq!(
            has_idr_slice(&unit),
            expected,
            "frame {i}: {:?}",
            kinds(&unit)
        );
        if expected {
            let k = kinds(&unit);
            assert!(
                k.contains(&NAL_SPS) && k.contains(&NAL_PPS),
                "frame {i} without SPS and PPS: {k:?}"
            );
            println!(
                "{}: IDR on frame {i}, {} bytes, {:.1} frames' worth",
                encoder.name(),
                unit.len(),
                unit.len() as f64 / case.frame_bytes(case.rate)
            );
        }
    }
}

#[test]
fn hardware_forced_idr() {
    forced_idr(HARDWARE);
}

#[test]
fn software_forced_idr() {
    forced_idr(SOFTWARE);
}

fn recover_with_idr(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    for i in 0..30 {
        encode(&mut *encoder, &mut frames, i, false);
    }
    assert!(!encoder.invalidates());
    // A recent frame and one long gone get the same answer, said before
    // recover is called and without committing the encoder to it.
    assert!(encoder.needs_idr(28) && encoder.needs_idr(3));
    let quiet = encode(&mut *encoder, &mut frames, 30, false);
    assert!(
        !quiet.idr && !has_idr_slice(&quiet),
        "an IDR nobody asked for"
    );
    assert_eq!(encoder.recover(29), Recovery::Idr);
    assert_eq!(encoder.recover(3), Recovery::Idr);
    let next = encode(&mut *encoder, &mut frames, 31, false);
    assert!(next.idr && has_idr_slice(&next), "no IDR after recover");
    let after = encode(&mut *encoder, &mut frames, 32, false);
    assert!(
        !after.idr && !has_idr_slice(&after),
        "two IDRs for one loss"
    );
}

#[test]
fn hardware_recover_with_idr() {
    recover_with_idr(HARDWARE);
}

#[test]
fn software_recover_with_idr() {
    recover_with_idr(SOFTWARE);
}

fn bitrate_holds_over_2_s(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    let count = u64::from(2 * case.fps);
    let units: Vec<AccessUnit> = (0..=count)
        .map(|i| encode(&mut *encoder, &mut frames, i, false))
        .collect();
    let budget = case.frame_bytes(case.rate);
    let target = f64::from(case.rate) / 1e6;
    // Frames 0 to 2 s less one frame, and 1 to 2 s: the same 2 s with and
    // without the opening IDR.
    let with_idr: usize = units[..count as usize].iter().map(AccessUnit::len).sum();
    let after_idr: usize = units[1..].iter().map(AccessUnit::len).sum();
    let (with_idr, after_idr) = (megabits(with_idr, 2.0), megabits(after_idr, 2.0));
    let largest = units[1..].iter().map(AccessUnit::len).max().unwrap_or(0);
    let early: usize = units[1..=10].iter().map(AccessUnit::len).sum();
    println!(
        "{} at {}x{}, {target} Mbit/s: {with_idr:.2} Mbit/s over the first 2 s ({:+.1} percent), {after_idr:.2} over the 2 s after the IDR ({:+.1} percent); IDR {:.1} frames' worth, the 10 frames after it {:.1}, largest other frame {:.2}",
        encoder.name(),
        case.width,
        case.height,
        (with_idr / target - 1.0) * 100.0,
        (after_idr / target - 1.0) * 100.0,
        units[0].len() as f64 / budget,
        early as f64 / budget,
        largest as f64 / budget
    );
    // The opening IDR and the frames right after it are the rate control
    // finding its level, which the start of a share pays once; the setting
    // is held to 15 percent on the 2 s after it.
    assert!(
        (after_idr / target - 1.0).abs() <= 0.15,
        "{after_idr:.2} Mbit/s is more than 15 percent off {target}"
    );
}

#[test]
fn hardware_bitrate_holds_over_2_s() {
    bitrate_holds_over_2_s(HARDWARE);
}

#[test]
fn software_bitrate_holds_over_2_s() {
    bitrate_holds_over_2_s(SOFTWARE);
}

fn bitrate_change(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    let second = u64::from(case.fps);
    let before: usize = (0..=second)
        .map(|i| encode(&mut *encoder, &mut frames, i, false))
        .skip(1)
        .map(|u| u.len())
        .sum();
    let low = case.rate / 3;
    encoder.set_bitrate(low).unwrap_or_else(|e| panic!("{e}"));
    let units: Vec<AccessUnit> = (second + 1..=3 * second)
        .map(|i| encode(&mut *encoder, &mut frames, i, false))
        .collect();
    assert!(
        units.iter().all(|u| !u.idr && !has_idr_slice(u)),
        "an IDR after the bitrate change"
    );
    let (first_second, second_second) = units.split_at(second as usize);
    let rate = |units: &[AccessUnit]| megabits(units.iter().map(AccessUnit::len).sum(), 1.0);
    let target = f64::from(low) / 1e6;
    println!(
        "{}: {:.2} Mbit/s in the second before going from {} to {target:.1}, then {:.2} and {:.2} in the two seconds after",
        encoder.name(),
        megabits(before, 1.0),
        f64::from(case.rate) / 1e6,
        rate(first_second),
        rate(second_second)
    );
    // With a VBV of one frame the setting works as a ceiling, as it does on
    // NVENC (tests/nvenc.rs): NVIDIA's Media Foundation encoder lands about
    // 18 percent under at 5 Mbit/s. Over is what would add delay, so that
    // bound is 15 percent; under is held to 35.
    let after = rate(second_second) / target - 1.0;
    assert!(
        (-0.35..=0.15).contains(&after),
        "{:.2} Mbit/s a second after the change is {:+.1} percent off {target:.1}",
        rate(second_second),
        after * 100.0
    );
    // The first second after the change, over only. NVIDIA's encoder takes
    // the new rate on the next frame. Windows' software encoder keeps
    // sending at the old one for about 16 frames first, whatever its buffer
    // is set to, which puts 3.75 Mbit into the first second against 2.67;
    // held to that, so it cannot get worse unnoticed.
    let first_over = rate(first_second) / target - 1.0;
    let limit = if case.kind == Kind::MfSoftware {
        0.5
    } else {
        0.15
    };
    assert!(
        first_over <= limit,
        "{:.2} Mbit/s in the first second after the change is {:+.1} percent over {target:.1}",
        rate(first_second),
        first_over * 100.0
    );
}

#[test]
fn hardware_bitrate_change() {
    bitrate_change(HARDWARE);
}

#[test]
fn software_bitrate_change() {
    bitrate_change(SOFTWARE);
}

fn one_frame_in_flight(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    // encode() gives back each frame's own bitstream before it returns, so
    // what can go wrong is on the encoder's side: a frame it holds until
    // the next one goes in. Booth drains such a frame out instead and says
    // so in the notes once the encoder keeps doing it.
    for i in 0..u64::from(2 * case.fps) {
        let unit = encode(&mut *encoder, &mut frames, i, false);
        assert_eq!(unit.index, i);
        assert!(!unit.is_empty(), "frame {i} came out empty");
    }
    let held = encoder.notes().contains("until drained");
    println!(
        "{}: frames held back until the next one went in: {}",
        encoder.name(),
        if held {
            "yes, so every frame is drained"
        } else {
            "none"
        }
    );
    assert!(!held, "{}", encoder.notes());
}

#[test]
fn hardware_one_frame_in_flight() {
    one_frame_in_flight(HARDWARE);
}

#[test]
fn software_one_frame_in_flight() {
    one_frame_in_flight(SOFTWARE);
}

#[test]
fn encode_time_next_to_nvenc() {
    let _turn = common::turn();
    let gpu = gpu();
    let mut lines = Vec::new();
    for (case, kind) in [
        (HARDWARE, Kind::Nvenc),
        (HARDWARE, Kind::MfHardware),
        (SOFTWARE, Kind::MfSoftware),
    ] {
        for preset in [Preset::P1, Preset::P4] {
            let case = Case { kind, ..case };
            let settings = Settings {
                bitrate: case.rate,
                preset,
            };
            let mut encoder = match encode::open_kind_codec(
                kind,
                Codec::H264,
                &gpu.device,
                case.width,
                case.height,
                case.fps,
                &settings,
            ) {
                Ok(encoder) => encoder,
                Err(e) => {
                    lines.push(format!("{kind}: skipped, {e}"));
                    continue;
                }
            };
            let mut frames = Frames::new(&gpu, case.width, case.height);
            let ms: Vec<f64> = (0..240)
                .map(|i| {
                    encode(&mut *encoder, &mut frames, i, false)
                        .encode_time()
                        .as_secs_f64()
                        * 1000.0
                })
                .collect();
            let (median, p95, max) = common::spread(&ms[1..]);
            // NVENC's name says its preset already.
            let mut label = encoder.name().to_string();
            if kind != Kind::Nvenc {
                label = format!("{label}, {preset:?}");
            }
            lines.push(format!(
                "{label} at {}x{}, 240 frames: encode median {median:.2} ms, p95 {p95:.2} ms, max {max:.2} ms (first frame {:.2})",
                case.width,
                case.height,
                ms[0]
            ));
        }
    }
    for line in lines {
        println!("{line}");
    }
}

fn decodable_stream(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    // Works out from the slice headers what each frame predicts from, as a
    // decoder does, through a forced IDR and a recovery IDR.
    let mut refs = References::new();
    let mut sps = None;
    let mut pps = None;
    for i in 0..240u64 {
        if i == 150 {
            assert_eq!(encoder.recover(149), Recovery::Idr);
        }
        let unit = encode(&mut *encoder, &mut frames, i, i == 90);
        let (usable, header) = refs.frame(i, &unit.data);
        if unit.idr {
            assert!(usable.is_empty());
        } else {
            assert_eq!(
                usable.first(),
                Some(&(i - 1)),
                "frame {i} does not predict from the frame before it: {usable:?}"
            );
        }
        for nal in annexb::nal_units(&unit.data) {
            match nal.kind() {
                NAL_SPS => sps = annexb::parse_sps(&nal),
                NAL_PPS => pps = annexb::parse_pps(&nal),
                _ => {}
            }
        }
        let (sps, pps) = (sps.as_ref().expect("SPS"), pps.as_ref().expect("PPS"));
        // Every slice of an access unit belongs to the same picture.
        for nal in annexb::nal_units(&unit.data).filter(|n| n.is_slice()) {
            let slice = annexb::slice_header(&nal, sps, pps)
                .unwrap_or_else(|| panic!("frame {i}: a slice header this reader cannot read"));
            assert_eq!(slice.frame_num, header.frame_num, "frame {i}");
            assert_eq!(nal.kind() == NAL_IDR, unit.idr, "frame {i}");
            assert!(nal.kind() == NAL_IDR || nal.kind() == NAL_SLICE);
        }
        assert_eq!(unit.idr, i == 0 || i == 90 || i == 150, "frame {i}");
    }
}

#[test]
fn hardware_decodable_stream() {
    decodable_stream(HARDWARE);
}

#[test]
fn software_decodable_stream() {
    decodable_stream(SOFTWARE);
}

fn refused_frames(case: Case) {
    let _turn = common::turn();
    let gpu = gpu();
    let Some(mut encoder) = open(&gpu, case, Preset::P1) else {
        return;
    };
    let mut frames = Frames::new(&gpu, case.width, case.height);
    encode(&mut *encoder, &mut frames, 5, false);

    let texture = frames.frame(5);
    let again = encoder.encode(&Frame {
        texture: &texture,
        index: 5,
        force_idr: false,
    });
    assert!(
        matches!(again, Err(EncodeError::FrameOutOfOrder { .. })),
        "frame 5 twice"
    );
    let bgra = common::texture(&gpu, case.width, case.height, DXGI_FORMAT_B8G8R8A8_UNORM);
    let Err(error) = encoder.encode(&Frame {
        texture: &bgra,
        index: 6,
        force_idr: false,
    }) else {
        panic!("a BGRA texture was accepted");
    };
    assert!(matches!(error, EncodeError::WrongFrame { .. }), "{error}");
    let small = common::texture(&gpu, 1280, 720, DXGI_FORMAT_NV12);
    let Err(error) = encoder.encode(&Frame {
        texture: &small,
        index: 7,
        force_idr: false,
    }) else {
        panic!("a 1280x720 texture was accepted");
    };
    println!("{error}");
    assert!(matches!(error, EncodeError::WrongFrame { .. }), "{error}");

    let unit = encode(&mut *encoder, &mut frames, 8, false);
    assert!(!unit.is_empty() && !unit.idr);
}

#[test]
fn hardware_refused_frames() {
    refused_frames(HARDWARE);
}

#[test]
fn software_refused_frames() {
    refused_frames(SOFTWARE);
}
