// Frames the GPU's decoder fails, as an Intel Iris Xe failed frames of an
// NVENC HEVC share with nothing from FFmpeg but "error -1". FFmpeg's d3d11va
// takes at most 256 slices a picture (MAX_SLICES in libavcodec/dxva2_hevc.c)
// and fails the 257th with the same bare -1, after the HEVC decoder has
// queued the picture for output, so a frame given 256 more slice segments
// fails on any GPU with the same error and leaves the same picture behind
// (FFmpeg 8.1.3). What made the Iris Xe's frames fail is not known.

mod common;

use annexb::hevc;
use decode::{Codec, DecodeError, Decoder};
use encode::Recovery;

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FAILS_AT: u64 = 30;
const FRAMES: u64 = 90;

fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 16);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn bits(bytes: &[u8]) -> Vec<bool> {
    bytes
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1 == 1))
        .collect()
}

fn bytes(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |b, (i, &bit)| b | (u8::from(bit) << (7 - i)))
        })
        .collect()
}

// Where an Exp-Golomb number starting at `at` ends.
fn after_ue(bits: &[bool], at: usize) -> usize {
    let zeros = bits[at..].iter().take_while(|b| !**b).count();
    at + 2 * zeros + 1
}

// The access unit with 256 more slice segments of its picture after its one
// slice: the same header but for first_slice_segment_in_pic_flag and a
// slice_segment_address. FFmpeg reads each header whole before it hands the
// segment to d3d11va, which never looks at the slice data behind them.
fn failing(unit: &[u8], sps: &hevc::Sps, pps: &hevc::Pps) -> Vec<u8> {
    let mut out = unit.to_vec();
    let slice = annexb::nal_units(unit)
        .find(|nal| hevc::is_slice(nal))
        .expect("a slice");
    let header = [slice.data[0], slice.data[1]];
    let rbsp = bits(&unescape(&slice.data[2..]));
    assert!(rbsp[0], "the first slice segment of its picture");
    let irap = hevc::is_irap(&slice);
    let pps_id_at = 1 + usize::from(irap);
    let split = after_ue(&rbsp, pps_id_at);
    let ctb = 1u64 << sps.log2_ctb_size;
    let ctbs = u64::from(sps.coded_width).div_ceil(ctb) * u64::from(sps.coded_height).div_ceil(ctb);
    assert!(ctbs > 256, "room for 256 more segments");
    let address_bits = 64 - (ctbs - 1).leading_zeros();
    for address in 1..=256u64 {
        let mut segment = rbsp[..split].to_vec();
        segment[0] = false;
        if pps.dependent_slice_segments_enabled {
            segment.push(false);
        }
        segment.extend((0..address_bits).rev().map(|i| (address >> i) & 1 == 1));
        segment.extend_from_slice(&rbsp[split..]);
        // The stop bit stays last, padded out to a byte again.
        while segment.last() == Some(&false) {
            segment.pop();
        }
        while !segment.len().is_multiple_of(8) {
            segment.push(false);
        }
        out.extend_from_slice(&[0, 0, 0, 1, header[0], header[1]]);
        out.extend_from_slice(&escape(&bytes(&segment)));
    }
    out
}

fn parameter_sets(idr: &[u8]) -> (hevc::Sps, hevc::Pps) {
    let sps = annexb::nal_units(idr)
        .find_map(|nal| hevc::parse_sps(&nal))
        .expect("an SPS with the IDR");
    let pps = annexb::nal_units(idr)
        .find_map(|nal| hevc::parse_pps(&nal))
        .expect("a PPS with the IDR");
    (sps, pps)
}

fn whole(decoder: &mut Decoder, reader: &mut Reader, data: &[u8], n: u64) {
    let decoded = decoder
        .decode(data)
        .unwrap_or_else(|e| panic!("frame {n}: {e}"))
        .unwrap_or_else(|| panic!("frame {n} gave no picture"));
    assert_eq!(
        reader.frame_number(&decoded),
        Some(n as u32),
        "frame {n} decoded to the wrong picture"
    );
}

// `count` frames in a row fail on the GPU. The viewer reports them, and the
// report reaches NVENC once the last of them is encoded, as with a round
// trip of a frame or so.
fn frames_that_fail(count: u64) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, Codec::Hevc) else {
        return;
    };
    let mut stream = Stream::new(&gpu, Codec::Hevc, WIDTH, HEIGHT);
    let mut reader = Reader::new(&gpu);
    let mut sets = None;
    let last_failing = FAILS_AT + count - 1;
    let mut said = Vec::new();
    for n in 0..FRAMES {
        let unit = stream.next(false);
        assert!(!unit.idr || n == 0, "frame {n} is an IDR");
        let (sps, pps) = sets.get_or_insert_with(|| parameter_sets(&unit.data));
        if (FAILS_AT..=last_failing).contains(&n) {
            let err = decoder
                .decode(&failing(&unit.data, sps, pps))
                .expect_err("a frame of 257 slices fails in d3d11va");
            let text = err.to_string();
            assert!(
                matches!(err, DecodeError::Damaged { .. }) && text.ends_with("(error -1)"),
                "frame {n}: {err:?}"
            );
            said.push(format!("{n}: {text}"));
            if n == last_failing {
                assert_eq!(stream.encoder.recover(FAILS_AT), Recovery::Invalidated);
            }
        } else {
            whole(&mut decoder, &mut reader, &unit.data, n);
        }
    }
    println!(
        "{count} failed in a row, each said: {}; every frame after them decoded whole with its own number",
        said.join("; ")
    );
}

// Each failed frame left its picture queued in FFmpeg. The next access unit
// took that picture out first, and when that one failed too, the decoder
// read the leftover as a frame held back for reordering, reset itself and
// showed nothing until an IDR: the laptop's "error -1", then "hold 1 frame
// back", then no picture.
#[test]
fn two_failed_frames() {
    frames_that_fail(2);
}

#[test]
fn one_failed_frame() {
    frames_that_fail(1);
}

#[test]
fn five_failed_frames() {
    frames_that_fail(5);
}
