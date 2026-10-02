// HEVC on NVENC and on the hardware encoder the GPU's driver registers with
// Media Foundation: the checks tests/nvenc.rs and tests/mf.rs make of H.264,
// and the two codecs side by side.

mod common;

use std::time::Instant;

use encode::annexb::{self, hevc};
use encode::{
    AccessUnit, Codec, EncodeError, Encoder, Frame, Kind, Offer, Preset, Recovery, Settings,
};

use common::{Frames, Gpu, HevcReferences};

const W: u32 = 2560;
const H: u32 = 1440;
const FPS: u32 = 120;
const RATE: u32 = 15_000_000;

/// The GPU each encoder is tested on: NVENC's NVIDIA card, and for Media
/// Foundation the first hardware GPU of any vendor.
fn gpu(kind: Kind) -> Option<Gpu> {
    match kind {
        Kind::Nvenc => common::nvidia(),
        _ => {
            let gpu = common::hardware();
            if gpu.is_none() {
                println!("skipped: no hardware GPU on this PC");
            }
            gpu
        }
    }
}

/// The encoder, or None with the reason printed when this PC does not have
/// it.
fn open(gpu: &Gpu, kind: Kind, codec: Codec, settings: Settings) -> Option<Box<dyn Encoder>> {
    match encode::open_kind_codec(kind, codec, &gpu.device, W, H, FPS, &settings) {
        Ok(encoder) => Some(encoder),
        Err(
            e @ (EncodeError::MediaFoundationMissing
            | EncodeError::NoHardwareEncoder { .. }
            | EncodeError::NvencCodecMissing { .. }),
        ) => {
            println!("skipped: {e}");
            None
        }
        Err(e) => panic!("{kind} {codec} on {}: {e}", gpu.name),
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
    annexb::nal_units(&unit.data)
        .map(|n| hevc::kind(&n))
        .collect()
}

fn has_idr_slice(unit: &AccessUnit) -> bool {
    annexb::nal_units(&unit.data).any(|n| hevc::is_idr(&n))
}

fn frame_bytes(bitrate: u32) -> f64 {
    f64::from(bitrate) / f64::from(FPS) / 8.0
}

fn megabits(units: &[AccessUnit], seconds: f64) -> f64 {
    units.iter().map(AccessUnit::len).sum::<usize>() as f64 * 8.0 / seconds / 1e6
}

fn first_frame(kind: Kind) {
    let _turn = common::turn();
    let Some(gpu) = gpu(kind) else { return };
    let Some(mut encoder) = open(&gpu, kind, Codec::Hevc, Settings::default()) else {
        return;
    };
    println!("{} on {}", encoder.name(), gpu.name);
    if !encoder.notes().is_empty() {
        println!("{}", encoder.notes());
    }
    assert_eq!(encoder.codec(), Codec::Hevc);
    assert!(encoder.name().contains("HEVC"), "{}", encoder.name());
    let mut frames = Frames::new(&gpu, W, H);
    let mut refs = HevcReferences::new();

    let first = encode(&mut *encoder, &mut frames, 0, false);
    assert!(first.idr, "the first frame must be an IDR");
    let first_kinds = kinds(&first);
    let at = |kind| first_kinds.iter().position(|&k| k == kind);
    let idr = first_kinds
        .iter()
        .position(|&k| k == hevc::IDR_W_RADL || k == hevc::IDR_N_LP);
    let (Some(vps), Some(sps), Some(pps), Some(idr)) =
        (at(hevc::VPS), at(hevc::SPS), at(hevc::PPS), idr)
    else {
        panic!("VPS, SPS, PPS and an IDR slice in the first frame: {first_kinds:?}");
    };
    assert!(
        vps < sps && sps < pps && pps < idr,
        "VPS, SPS, PPS, then the IDR slice: {first_kinds:?}"
    );
    refs.frame(0, &first.data);

    let sps = annexb::nal_units(&first.data)
        .find(|n| hevc::kind(n) == hevc::SPS)
        .and_then(|n| hevc::parse_sps(&n))
        .expect("a readable SPS");
    println!(
        "SPS: profile {}, {} tier, level {}, {}x{} coded as {}x{}, {}-bit, chroma format {}, {} pictures held, {} reordered, VUI {:?}",
        sps.profile_idc,
        if sps.high_tier { "High" } else { "Main" },
        f64::from(sps.level_idc) / 30.0,
        sps.width,
        sps.height,
        sps.coded_width,
        sps.coded_height,
        sps.bit_depth_luma,
        sps.chroma_format_idc,
        sps.max_dec_pic_buffering,
        sps.max_num_reorder_pics,
        sps.vui
    );
    assert_eq!((sps.width, sps.height), (W, H));
    assert_eq!(sps.profile_idc, 1, "Main profile");
    assert_eq!((sps.bit_depth_luma, sps.bit_depth_chroma), (8, 8));
    assert_eq!(sps.chroma_format_idc, 1, "4:2:0");
    assert_eq!(
        sps.max_num_reorder_pics, 0,
        "a decoder may hold frames back for reordering"
    );
    if kind == Kind::Nvenc {
        // Level 6 on the High tier, which holds 12 references at 1440p and
        // the top of the upload setting (src/level.rs).
        assert_eq!((sps.level_idc, sps.high_tier), (180, true));
        assert_eq!(
            sps.max_dec_pic_buffering, 13,
            "12 references and the picture being decoded"
        );
        let vui = sps.vui.as_ref().expect("VUI");
        assert_eq!(vui.full_range, Some(false), "limited range");
        assert_eq!(
            vui.colour,
            Some((1, 1, 1)),
            "BT.709 primaries, transfer and matrix"
        );
    } else if let Some(vui) = &sps.vui {
        // As for H.264 in tests/mf.rs: Windows' interface cannot ask for
        // the colour description, but what a stream says must be right.
        assert_ne!(vui.full_range, Some(true), "full range in the VUI");
        if let Some(colour) = vui.colour {
            assert_eq!(colour, (1, 1, 1), "BT.709 primaries, transfer and matrix");
        }
    }

    for i in 1..240 {
        let unit = encode(&mut *encoder, &mut frames, i, false);
        assert!(
            !unit.idr && !has_idr_slice(&unit),
            "frame {i} is an IDR nobody asked for"
        );
        let slices = kinds(&unit).iter().filter(|&&k| k < 32).count();
        assert_eq!(
            slices,
            1,
            "frame {i}: one slice per frame, {:?}",
            kinds(&unit)
        );
        let frame = refs.frame(i, &unit.data);
        assert_eq!(
            frame.header.slice_type,
            annexb::SliceType::P,
            "frame {i} is not a P frame"
        );
        assert_eq!(
            frame.usable.first(),
            Some(&(i - 1)),
            "frame {i} does not predict from the frame before it: {:?}",
            frame.usable
        );
    }
}

#[test]
fn nvenc_first_frame() {
    first_frame(Kind::Nvenc);
}

#[test]
fn hardware_first_frame() {
    first_frame(Kind::MfHardware);
}

fn forced_idr(kind: Kind) {
    let _turn = common::turn();
    let Some(gpu) = gpu(kind) else { return };
    let Some(mut encoder) = open(&gpu, kind, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
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
                k.contains(&hevc::VPS) && k.contains(&hevc::SPS) && k.contains(&hevc::PPS),
                "frame {i} without VPS, SPS and PPS: {k:?}"
            );
            println!(
                "{}: IDR on frame {i}, {} bytes, {:.2} frames' worth",
                encoder.name(),
                unit.len(),
                unit.len() as f64 / frame_bytes(RATE)
            );
        }
    }
}

#[test]
fn nvenc_forced_idr() {
    forced_idr(Kind::Nvenc);
}

#[test]
fn hardware_forced_idr() {
    forced_idr(Kind::MfHardware);
}

#[test]
fn nvenc_recover() {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut encoder) = open(&gpu, Kind::Nvenc, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
    // Works out from the slice headers which frames each frame may predict
    // from, as the viewer's decoder will, so the checks below see what NVENC
    // did about a loss and not only that the next frame is still a P frame.
    let mut refs = HevcReferences::new();
    let mut step = |encoder: &mut dyn Encoder, i: u64| {
        let unit = encode(encoder, &mut frames, i, false);
        let frame = refs.frame(i, &unit.data);
        (unit, frame.usable, frame.kept)
    };
    let untouched =
        |usable: &[u64], lost: std::ops::Range<u64>| usable.iter().all(|f| !lost.contains(f));

    for i in 0..60 {
        let (_, usable, kept) = step(&mut *encoder, i);
        if i == 59 {
            println!("frame 59 predicts from {usable:?}; the decoder keeps {kept:?}");
            assert_eq!(usable.first(), Some(&58));
            assert_eq!(kept.len(), 13, "12 references and frame 59 itself");
        }
    }
    assert!(encoder.invalidates(), "{} has no invalidation", gpu.name);
    assert!(!encoder.needs_idr(57));
    assert_eq!(encoder.recover(57), Recovery::Invalidated);
    let (next, usable, kept) = step(&mut *encoder, 60);
    println!("frame 60 after losing 57 predicts from {usable:?}; the decoder keeps {kept:?}");
    assert!(
        !next.idr && !has_idr_slice(&next),
        "an IDR after an invalidation"
    );
    assert!(
        !usable.is_empty() && usable.iter().all(|&f| f < 57),
        "frame 60 may predict from {usable:?}, and only frames before 57 reached the viewer intact"
    );
    for i in 61..70 {
        let (_, usable, _) = step(&mut *encoder, i);
        assert!(
            untouched(&usable, 57..60),
            "frame {i} may predict from {usable:?}, which holds an invalidated frame"
        );
    }

    // Several frames reported together, newest first: each one is handled.
    assert!(!encoder.needs_idr(68) && !encoder.needs_idr(66));
    assert_eq!(encoder.recover(68), Recovery::Invalidated);
    assert_eq!(encoder.recover(66), Recovery::Invalidated);
    let (next, usable, _) = step(&mut *encoder, 70);
    assert!(!next.idr, "an IDR after two invalidations");
    assert!(
        !usable.is_empty() && usable.iter().all(|&f| f < 66) && untouched(&usable, 57..60),
        "frame 70 may predict from {usable:?}"
    );
    for i in 71..100 {
        let (_, usable, _) = step(&mut *encoder, i);
        assert!(
            untouched(&usable, 66..70),
            "frame {i} may predict from {usable:?}, which holds an invalidated frame"
        );
    }

    // The far edge of the twelve references: losing 89 leaves 88 as the
    // only way back, eleven frames behind the newest, and NVENC still has it.
    assert!(!encoder.needs_idr(89));
    assert_eq!(encoder.recover(89), Recovery::Invalidated);
    let (next, usable, _) = step(&mut *encoder, 100);
    assert!(
        !next.idr && !has_idr_slice(&next),
        "an IDR at the window edge"
    );
    assert_eq!(usable, [88], "frame 100 after losing 89");

    for i in 101..110 {
        step(&mut *encoder, i);
    }
    // 80 left the encoder's memory long ago. Saying so commits it to
    // nothing: the sharer may hold that IDR back for a while.
    assert!(encoder.needs_idr(80));
    let (next, _, _) = step(&mut *encoder, 110);
    assert!(
        !next.idr && !has_idr_slice(&next),
        "an IDR nobody asked for"
    );
    assert_eq!(encoder.recover(80), Recovery::Idr);
    let (next, _, _) = step(&mut *encoder, 111);
    assert!(
        next.idr && has_idr_slice(&next),
        "no IDR after recover asked for one"
    );
    println!(
        "recovery IDR: {} bytes, {:.2} frames' worth",
        next.len(),
        next.len() as f64 / frame_bytes(RATE)
    );
    let (after, usable, _) = step(&mut *encoder, 112);
    assert!(!after.idr);
    assert_eq!(usable, [111]);
    assert!(encoder.needs_idr(111) && !encoder.needs_idr(105));
}

// As in tests/nvenc.rs: every frame reported lost the moment it arrives, for
// a second, answered as the sharer answers it, with the IDRs held to the
// keyframe floor. The same twelve references give the same count.
#[test]
fn nvenc_flood_of_reports() {
    const FLOOR_FRAMES: u64 = 24;
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut encoder) = open(&gpu, Kind::Nvenc, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
    let mut refs = HevcReferences::new();
    let (mut last_idr, mut held) = (0, false);
    let mut cut = Vec::new();
    let (mut idrs, mut invalidated) = (0, 0);
    for i in 0..u64::from(FPS) {
        let force_idr = held && i >= last_idr + FLOOR_FRAMES;
        let unit = encode(&mut *encoder, &mut frames, i, force_idr);
        let usable = refs.frame(i, &unit.data).usable;
        assert_eq!(unit.idr, i == 0 || force_idr, "frame {i}");
        if unit.idr {
            (last_idr, held) = (i, false);
            cut.clear();
            idrs += 1;
        } else {
            assert!(
                !usable.is_empty() && usable.iter().all(|f| !cut.contains(f)),
                "frame {i} may predict from {usable:?}, and {cut:?} were invalidated"
            );
        }
        if encoder.needs_idr(i) {
            held = true;
            continue;
        }
        assert_eq!(
            encoder.recover(i),
            Recovery::Invalidated,
            "the driver refused to invalidate frame {i}"
        );
        cut.push(i);
        invalidated += 1;
    }
    println!("{idrs} IDRs and {invalidated} invalidations in {FPS} frames");
    assert_eq!((idrs, invalidated), (5, 110));
}

#[test]
fn hardware_recover_with_idr() {
    let _turn = common::turn();
    let Some(gpu) = gpu(Kind::MfHardware) else {
        return;
    };
    let Some(mut encoder) = open(&gpu, Kind::MfHardware, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
    let mut refs = HevcReferences::new();
    for i in 0..240u64 {
        if i == 150 {
            assert!(!encoder.invalidates() && encoder.needs_idr(149));
            assert_eq!(encoder.recover(149), Recovery::Idr);
            assert_eq!(encoder.recover(3), Recovery::Idr);
        }
        let unit = encode(&mut *encoder, &mut frames, i, i == 90);
        let frame = refs.frame(i, &unit.data);
        assert_eq!(unit.idr, i == 0 || i == 90 || i == 150, "frame {i}");
        assert_eq!(has_idr_slice(&unit), unit.idr, "frame {i}");
        if unit.idr {
            assert!(frame.usable.is_empty());
        } else {
            assert_eq!(
                frame.usable.first(),
                Some(&(i - 1)),
                "frame {i} does not predict from the frame before it: {:?}",
                frame.usable
            );
        }
    }
}

fn bitrate_holds(kind: Kind) {
    let _turn = common::turn();
    let Some(gpu) = gpu(kind) else { return };
    let Some(mut encoder) = open(&gpu, kind, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
    let units: Vec<AccessUnit> = (0..=240)
        .map(|i| encode(&mut *encoder, &mut frames, i, false))
        .collect();
    let budget = frame_bytes(RATE);
    let after_idr = megabits(&units[1..], 2.0);
    let largest = units[1..].iter().map(AccessUnit::len).max().unwrap_or(0);
    let over_twice = units[1..]
        .iter()
        .filter(|u| u.len() as f64 > 2.0 * budget)
        .count();
    println!(
        "{} at 2560x1440: {after_idr:.2} Mbit/s over the 2 s after the IDR against 15 ({:+.1} percent); IDR {:.2} frames' worth; largest other frame {:.2}, {over_twice} of 240 over twice",
        encoder.name(),
        (after_idr / 15.0 - 1.0) * 100.0,
        units[0].len() as f64 / budget,
        largest as f64 / budget
    );
    // The bounds H.264 is held to on each encoder (tests/nvenc.rs and
    // tests/mf.rs): with a one-frame VBV the setting is a ceiling, so over is
    // held tight and under loosely.
    let (under, over) = match kind {
        Kind::Nvenc => (0.25, 0.10),
        _ => (0.15, 0.15),
    };
    assert!(
        (-under..=over).contains(&(after_idr / 15.0 - 1.0)),
        "{after_idr:.2} Mbit/s is outside -{under} to +{over} of 15"
    );
    if kind == Kind::Nvenc {
        for (i, unit) in units.iter().enumerate().skip(1) {
            assert!(
                unit.len() as f64 <= 2.0 * budget,
                "frame {i} is {} bytes, more than twice a frame's worth ({budget:.0})",
                unit.len()
            );
        }
    }
}

#[test]
fn nvenc_bitrate_holds_at_15_mbit() {
    bitrate_holds(Kind::Nvenc);
}

#[test]
fn hardware_bitrate_holds_at_15_mbit() {
    bitrate_holds(Kind::MfHardware);
}

fn bitrate_change(kind: Kind) {
    let _turn = common::turn();
    let Some(gpu) = gpu(kind) else { return };
    let Some(mut encoder) = open(&gpu, kind, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
    for i in 0..120 {
        encode(&mut *encoder, &mut frames, i, false);
    }
    let low = 5_000_000;
    encoder.set_bitrate(low).unwrap_or_else(|e| panic!("{e}"));
    let units: Vec<AccessUnit> = (120..240)
        .map(|i| encode(&mut *encoder, &mut frames, i, false))
        .collect();
    assert!(
        units.iter().all(|u| !u.idr && !has_idr_slice(u)),
        "an IDR after the bitrate change"
    );
    let rate = megabits(&units, 1.0);
    let budget = frame_bytes(low);
    let largest = units.iter().map(AccessUnit::len).max().unwrap_or(0);
    println!(
        "{}: the second after going from 15 to 5 Mbit/s: {rate:.2} Mbit/s ({:+.1} percent); first frame {:.2} frames' worth, largest {:.2}",
        encoder.name(),
        (rate / 5.0 - 1.0) * 100.0,
        units[0].len() as f64 / budget,
        largest as f64 / budget
    );
    // H.264's bounds on each encoder: 15 percent over, 35 under.
    assert!(
        (-0.35..=0.15).contains(&(rate / 5.0 - 1.0)),
        "{rate:.2} Mbit/s is outside -35 to +15 percent of 5"
    );
    if kind == Kind::Nvenc {
        for (i, unit) in units.iter().enumerate() {
            assert!(
                unit.len() as f64 <= 2.0 * budget,
                "frame {} after the change is {} bytes, more than twice the new frame's worth ({budget:.0})",
                120 + i,
                unit.len()
            );
        }
    }
}

#[test]
fn nvenc_bitrate_change() {
    bitrate_change(Kind::Nvenc);
}

#[test]
fn hardware_bitrate_change() {
    bitrate_change(Kind::MfHardware);
}

#[test]
fn hardware_one_frame_in_flight() {
    let _turn = common::turn();
    let Some(gpu) = gpu(Kind::MfHardware) else {
        return;
    };
    let Some(mut encoder) = open(&gpu, Kind::MfHardware, Codec::Hevc, Settings::default()) else {
        return;
    };
    let mut frames = Frames::new(&gpu, W, H);
    for i in 0..240 {
        let unit = encode(&mut *encoder, &mut frames, i, false);
        assert_eq!(unit.index, i);
        assert!(!unit.is_empty(), "frame {i} came out empty");
    }
    assert!(
        !encoder.notes().contains("until drained"),
        "{}",
        encoder.notes()
    );
}

/// Encode ms and frame sizes of one encoder on 240 frames of the pattern.
struct Run {
    label: String,
    ms: Vec<f64>,
    units: Vec<AccessUnit>,
}

fn run(gpu: &Gpu, kind: Kind, codec: Codec, preset: Preset, forced_idr_at: u64) -> Option<Run> {
    let settings = Settings {
        bitrate: RATE,
        preset,
    };
    let mut encoder = match encode::open_kind_codec(kind, codec, &gpu.device, W, H, FPS, &settings)
    {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("{kind} {codec}: skipped, {e}");
            return None;
        }
    };
    let mut frames = Frames::new(gpu, W, H);
    let units: Vec<AccessUnit> = (0..240)
        .map(|i| encode(&mut *encoder, &mut frames, i, i == forced_idr_at))
        .collect();
    let ms = units
        .iter()
        .map(|u| u.encode_time().as_secs_f64() * 1000.0)
        .collect();
    // NVENC's name says its preset already.
    let mut label = encoder.name().to_string();
    if kind != Kind::Nvenc {
        label = format!("{label}, {preset:?}");
    }
    Some(Run { label, ms, units })
}

#[test]
fn encode_time_next_to_h264() {
    let _turn = common::turn();
    let Some(gpu) = gpu(Kind::MfHardware) else {
        return;
    };
    let mut lines = Vec::new();
    for (kind, preset) in [
        (Kind::Nvenc, Preset::P1),
        (Kind::Nvenc, Preset::P4),
        (Kind::MfHardware, Preset::P1),
    ] {
        for codec in Codec::ALL {
            let Some(run) = run(&gpu, kind, codec, preset, u64::MAX) else {
                continue;
            };
            let (median, p95, max) = common::spread(&run.ms[1..]);
            lines.push(format!(
                "{} at 2560x1440, 240 frames: encode median {median:.2} ms, p95 {p95:.2} ms, max {max:.2} ms (first frame {:.2})",
                run.label, run.ms[0]
            ));
        }
    }
    for line in lines {
        println!("{line}");
    }
}

// An IDR comes out at about six frames' worth in H.264, where one frame's
// worth was the aim. The same pattern at the same rate in both codecs, with
// a forced IDR a second in, where the rate control has settled.
#[test]
fn frame_sizes_next_to_h264() {
    let _turn = common::turn();
    let Some(gpu) = gpu(Kind::MfHardware) else {
        return;
    };
    let budget = frame_bytes(RATE);
    let mut lines = Vec::new();
    for kind in [Kind::Nvenc, Kind::MfHardware] {
        for codec in Codec::ALL {
            let Some(run) = run(&gpu, kind, codec, Preset::P1, 120) else {
                continue;
            };
            let units = &run.units;
            let others: Vec<f64> = units
                .iter()
                .filter(|u| !u.idr)
                .map(|u| u.len() as f64)
                .collect();
            let (median, _, _) = common::spread(&others);
            let largest = units
                .iter()
                .filter(|u| !u.idr)
                .max_by_key(|u| u.len())
                .expect("frames other than IDRs");
            let after_idrs = [&units[1], &units[121]].map(|u| u.len() as f64 / budget);
            lines.push(format!(
                "{}: first IDR {} bytes ({:.2} frames' worth), forced IDR {} bytes ({:.2}), the frames after them {:.2} and {:.2}, other frames median {median:.0} bytes ({:.2}) and largest {} (frame {}, {:.2}), {:.2} Mbit/s over the 2 s",
                run.label,
                units[0].len(),
                units[0].len() as f64 / budget,
                units[120].len(),
                units[120].len() as f64 / budget,
                after_idrs[0],
                after_idrs[1],
                median / budget,
                largest.len(),
                largest.index,
                largest.len() as f64 / budget,
                megabits(units, 2.0)
            ));
        }
    }
    for line in lines {
        println!("{line}");
    }
}

fn describe(offer: &Offer) -> String {
    let kinds: Vec<String> = offer.kinds.iter().map(|k| k.word().to_string()).collect();
    format!("{:?}; refused: {}", kinds, offer.refused.join("; "))
}

#[test]
fn offer_answers_on_this_pc() {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    for (codec, width, height, fps) in [
        (Codec::Hevc, W, H, FPS),
        (Codec::H264, W, H, FPS),
        (Codec::Hevc, 1920, 1080, 60),
        (Codec::H264, 1920, 1080, 60),
    ] {
        // The first answer loads the driver's library and Media
        // Foundation; the room asks once per share, so the first counts.
        let mut ms = Vec::new();
        let mut last = None;
        for _ in 0..5 {
            let started = Instant::now();
            let offer = encode::offer(codec, &gpu.device, width, height, fps)
                .unwrap_or_else(|e| panic!("{e}"));
            ms.push(started.elapsed().as_secs_f64() * 1000.0);
            last = Some(offer);
        }
        let offer = last.expect("five answers");
        println!(
            "{codec} at {width}x{height} {fps} fps on {}: {} (first answer {:.1} ms, then {:.1} to {:.1})",
            gpu.name,
            describe(&offer),
            ms[0],
            ms[1..].iter().copied().fold(f64::MAX, f64::min),
            ms[1..].iter().copied().fold(0.0, f64::max)
        );
        assert!(
            offer.takes(Kind::Nvenc) && offer.takes(Kind::MfHardware),
            "{}",
            describe(&offer)
        );
        assert_eq!(
            offer.takes(Kind::MfSoftware),
            codec == Codec::H264 && fps <= 60
        );
    }

    // 8K at 60 fits level 6.2, with 5 references where 6 were asked for;
    // a square 8K at 240 fps fits no level at all.
    let offer =
        encode::offer(Codec::Hevc, &gpu.device, 7680, 4320, 60).unwrap_or_else(|e| panic!("{e}"));
    println!("HEVC at 7680x4320 60 fps: {}", describe(&offer));
    assert!(offer.takes(Kind::Nvenc), "{}", describe(&offer));
    let offer =
        encode::offer(Codec::Hevc, &gpu.device, 8192, 8192, 240).unwrap_or_else(|e| panic!("{e}"));
    println!("HEVC at 8192x8192 240 fps: {}", describe(&offer));
    assert!(!offer.any());
    assert_eq!(
        offer.refused,
        ["8192x8192 at 240 fps is past every HEVC level, so no encoder takes it"]
    );

    // A GPU with no encoder of its own gets reasons, not encoders, and the
    // software encoder for H.264 at what it takes.
    let warp = common::warp();
    let offer =
        encode::offer(Codec::Hevc, &warp.device, 1920, 1080, 60).unwrap_or_else(|e| panic!("{e}"));
    println!("HEVC on WARP: {}", describe(&offer));
    assert!(!offer.any());
    let offer =
        encode::offer(Codec::H264, &warp.device, 1920, 1080, 60).unwrap_or_else(|e| panic!("{e}"));
    println!("H.264 on WARP: {}", describe(&offer));
    assert_eq!(offer.kinds, [Kind::MfSoftware]);
}

#[test]
fn no_hevc_without_gpu_encoder() {
    let _turn = common::turn();
    let warp = common::warp();
    let Err(error) = encode::open_codec(
        Codec::Hevc,
        &warp.device,
        1920,
        1080,
        60,
        &Settings::default(),
    ) else {
        panic!("an HEVC encoder opened on WARP");
    };
    println!("{error}");
    assert!(
        matches!(
            error,
            EncodeError::NoEncoderForGpu {
                codec: Codec::Hevc,
                fit: None,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("no HEVC screen share encoder"));
    assert!(error.to_string().contains("no software HEVC encoder"));

    let Err(error) = encode::open_kind_codec(
        Kind::MfSoftware,
        Codec::Hevc,
        &warp.device,
        1920,
        1080,
        60,
        &Settings::default(),
    ) else {
        panic!("the software encoder opened for HEVC");
    };
    assert!(matches!(error, EncodeError::NoSoftwareHevc), "{error}");
}

#[test]
fn codec_words_round_trip() {
    for codec in Codec::ALL {
        assert_eq!(codec.word().parse::<Codec>(), Ok(codec));
    }
    assert_eq!("HEVC".parse::<Codec>(), Ok(Codec::Hevc));
    let error = "av1".parse::<Codec>().unwrap_err();
    assert_eq!(
        error,
        "there is no codec called \"av1\": the choices are h264 and hevc"
    );
}
