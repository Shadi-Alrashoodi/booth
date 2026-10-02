// A stream's SPS is the peer's to write. Ones that a Booth sharer never
// sends but a friend's broken or infected PC could: one for a picture far
// larger than a share, which FFmpeg would size its GPU surfaces for; one
// that asks the decoder to hold frames back and reorder them, which would
// add a frame of delay per frame held for the rest of the session; and ones
// the GPU's decoder does not take, which must be refused with the right
// reason.

mod common;

use annexb::{NAL_PPS, NAL_SPS, Sps};
use decode::{Codec, DecodeError, Decoder};

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

// Bits of an RBSP, most significant first.
struct Writer {
    bytes: Vec<u8>,
    used: u32,
}

impl Writer {
    fn new() -> Writer {
        Writer {
            bytes: Vec::new(),
            used: 8,
        }
    }

    fn bits(&mut self, value: u32, count: u32) {
        for bit in (0..count).rev() {
            if self.used == 8 {
                self.bytes.push(0);
                self.used = 0;
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= ((value >> bit & 1) as u8) << (7 - self.used);
            self.used += 1;
        }
    }

    fn flag(&mut self, on: bool) {
        self.bits(on as u32, 1);
    }

    fn ue(&mut self, value: u32) {
        let coded = value + 1;
        let len = 32 - coded.leading_zeros();
        self.bits(0, len - 1);
        self.bits(coded, len);
    }

    // rbsp_trailing_bits, emulation prevention, start code and header.
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.bits(1, 1);
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for byte in self.bytes {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }
}

// An SPS that reads like the encoder's own, so its PPS and slice headers
// still parse against it, but with another size and bit depth and, when
// `reorder` is some, a VUI that asks for that many frames of reordering.
fn sps_like(sps: &Sps, width: u32, height: u32, depth: u32, reorder: Option<u32>) -> Vec<u8> {
    assert_eq!(sps.profile_idc, 100, "NVENC sends High profile");
    assert!(sps.frame_mbs_only && sps.pic_order_cnt_type != 1);
    let mut w = Writer::new();
    // High 10 for more than 8 bits.
    let profile = if depth == 8 { 100 } else { 110 };
    w.bits(profile, 8);
    w.bits(0, 8);
    w.bits(u32::from(sps.level_idc), 8);
    w.ue(sps.sps_id);
    w.ue(1); // chroma_format_idc: 4:2:0
    w.ue(depth - 8); // bit_depth_luma_minus8
    w.ue(depth - 8); // bit_depth_chroma_minus8
    w.flag(false); // qpprime_y_zero_transform_bypass
    w.flag(false); // seq_scaling_matrix_present
    w.ue(sps.log2_max_frame_num - 4);
    w.ue(sps.pic_order_cnt_type);
    if sps.pic_order_cnt_type == 0 {
        w.ue(sps.log2_max_pic_order_cnt_lsb - 4);
    }
    w.ue(sps.max_num_ref_frames);
    w.flag(false); // gaps_in_frame_num_allowed
    w.ue(width / 16 - 1);
    w.ue(height / 16 - 1);
    w.flag(true); // frame_mbs_only
    w.flag(true); // direct_8x8_inference
    w.flag(false); // frame_cropping
    w.flag(reorder.is_some()); // vui_parameters_present
    if let Some(frames) = reorder {
        w.flag(false); // aspect_ratio_info_present
        w.flag(false); // overscan_info_present
        w.flag(false); // video_signal_type_present
        w.flag(false); // chroma_loc_info_present
        w.flag(false); // timing_info_present
        w.flag(false); // nal_hrd_parameters_present
        w.flag(false); // vcl_hrd_parameters_present
        w.flag(false); // pic_struct_present
        w.flag(true); // bitstream_restriction
        w.flag(true); // motion_vectors_over_pic_boundaries
        w.ue(0); // max_bytes_per_pic_denom
        w.ue(0); // max_bits_per_mb_denom
        w.ue(16); // log2_max_mv_length_horizontal
        w.ue(16); // log2_max_mv_length_vertical
        w.ue(frames); // max_num_reorder_frames
        w.ue(sps.max_num_ref_frames.max(frames)); // max_dec_frame_buffering
    }
    w.nal(0x67)
}

// The IDR access unit with its SPS swapped for `sps`.
fn with_sps(idr: &[u8], sps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in annexb::nal_units(idr) {
        if nal.kind() == NAL_SPS {
            out.extend_from_slice(sps);
        } else {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal.data);
        }
    }
    out
}

struct Setup {
    decoder: Decoder,
    reader: Reader,
    // Frame 0 an IDR, 1 and 2 P frames, 3 an IDR, 4 a P frame.
    units: Vec<Vec<u8>>,
    sps: Sps,
    _turn: std::sync::MutexGuard<'static, ()>,
}

fn setup() -> Option<Setup> {
    let turn = common::turn();
    let gpu = common::nvidia()?;
    let decoder = common::decoder(&gpu, Codec::H264)?;
    let mut stream = Stream::new(&gpu, Codec::H264, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..5).map(|n| stream.next(n == 3).data).collect();
    let sps = annexb::nal_units(&units[0])
        .find(|nal| nal.kind() == NAL_SPS)
        .and_then(|nal| annexb::parse_sps(&nal))
        .expect("an SPS this reader can read in the first frame");
    assert_eq!((sps.width, sps.height), (WIDTH, HEIGHT));
    Some(Setup {
        decoder,
        reader: Reader::new(&gpu),
        units,
        sps,
        _turn: turn,
    })
}

impl Setup {
    fn whole(&mut self, n: usize) {
        let decoded = self
            .decoder
            .decode(&self.units[n])
            .unwrap_or_else(|e| panic!("frame {n}: {e}"))
            .unwrap_or_else(|| panic!("frame {n} gave no picture"));
        assert_eq!(
            self.reader.frame_number(&decoded),
            Some(n as u32),
            "frame {n}"
        );
    }
}

#[test]
fn rebuilt_sps_decodes() {
    // Checks the writer: the rebuilt SPS with the stream's own size must
    // decode, or the refusals below would prove nothing.
    let Some(mut s) = setup() else { return };
    let sps = sps_like(&s.sps, WIDTH, HEIGHT, 8, None);
    let rebuilt = annexb::parse_sps(&annexb::nal_units(&sps).next().unwrap()).unwrap();
    assert_eq!((rebuilt.width, rebuilt.height), (WIDTH, HEIGHT));
    let idr = with_sps(&s.units[0], &sps);
    let decoded = s
        .decoder
        .decode(&idr)
        .unwrap_or_else(|e| panic!("{e}"))
        .expect("a picture");
    assert_eq!(s.reader.frame_number(&decoded), Some(0));
    s.whole(1);
    s.whole(2);
}

#[test]
fn oversized_picture() {
    let Some(mut s) = setup() else { return };
    s.whole(0);
    s.whole(1);
    // 8192x4608 is 147456 macroblocks, four times the most Booth takes.
    let idr = with_sps(&s.units[0], &sps_like(&s.sps, 8192, 4608, 8, None));
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(
        matches!(
            err,
            DecodeError::Oversized {
                width: 8192,
                height: 4608
            }
        ),
        "{err:?}"
    );
    s.whole(3);
    s.whole(4);
}

#[test]
fn too_wide_for_the_gpu() {
    let Some(mut s) = setup() else { return };
    s.whole(0);
    // 2048 macroblocks, inside Booth's limit, but twice as wide as the
    // 4096 that NVIDIA's and every other H.264 hardware decoder stops at.
    let idr = with_sps(&s.units[0], &sps_like(&s.sps, 8192, 64, 8, None));
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(
        matches!(
            err,
            DecodeError::Unsupported {
                width: 8192,
                height: 64,
                ..
            }
        ),
        "{err:?}"
    );
    s.whole(3);
    s.whole(4);
}

#[test]
fn reordering_refused() {
    let Some(mut s) = setup() else { return };
    s.whole(0);
    let idr = with_sps(&s.units[0], &sps_like(&s.sps, WIDTH, HEIGHT, 8, Some(2)));
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(matches!(err, DecodeError::HeldBack { .. }), "{err:?}");
    // The reset took every reference, and FFmpeg shows no P frame after it
    // until an IDR, even with the stream's own parameter sets in front of it
    // again (tests/peer_sps_hevc.rs checks the same for HEVC).
    let mut sets: Vec<u8> = annexb::nal_units(&s.units[0])
        .filter(|nal| matches!(nal.kind(), NAL_SPS | NAL_PPS))
        .flat_map(|nal| [&[0u8, 0, 0, 1][..], nal.data].concat())
        .collect();
    sets.extend_from_slice(&s.units[1]);
    let after = s.decoder.decode(&sets);
    println!("a P frame after the reset: {after:?}");
    assert!(!matches!(after, Ok(Some(_))), "{after:?}");
    // Nothing of the refused stream comes out later with these.
    s.whole(3);
    s.whole(4);
}

#[test]
fn ten_bit_refused_as_format() {
    // FFmpeg offers no GPU format for 10-bit H.264, so the refusal must not
    // tell the sharer to share smaller, which would change nothing.
    let Some(mut s) = setup() else { return };
    s.whole(0);
    let idr = with_sps(&s.units[0], &sps_like(&s.sps, WIDTH, HEIGHT, 10, None));
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(
        matches!(&err, DecodeError::WrongFormat { profile, .. } if profile == "High 10"),
        "{err:?}"
    );
    s.whole(3);
    s.whole(4);
}
