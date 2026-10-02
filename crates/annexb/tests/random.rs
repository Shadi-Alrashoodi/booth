use annexb::{Codec, Nal, Pps, Sps, hevc};
use proptest::prelude::*;

// What NVENC's SPS and PPS say, for reading random slice headers with.
fn nvenc_sps() -> Sps {
    Sps {
        log2_max_frame_num: 8,
        log2_max_pic_order_cnt_lsb: 8,
        frame_mbs_only: true,
        chroma_array_type: 1,
        ..Sps::default()
    }
}

fn nvenc_pps() -> Pps {
    Pps {
        num_ref_idx_l0_default_active: 12,
        num_ref_idx_l1_default_active: 1,
        ..Pps::default()
    }
}

// Everything the header can switch on, so the random bytes go down every
// branch of the slice header.
fn busy_sps() -> Sps {
    Sps {
        log2_max_frame_num: 16,
        pic_order_cnt_type: 1,
        frame_mbs_only: false,
        separate_colour_plane: true,
        ..Sps::default()
    }
}

fn busy_pps() -> Pps {
    Pps {
        bottom_field_pic_order_in_frame_present: true,
        num_ref_idx_l0_default_active: 32,
        num_ref_idx_l1_default_active: 32,
        weighted_pred: true,
        weighted_bipred_idc: 1,
        redundant_pic_cnt_present: true,
        ..Pps::default()
    }
}

// What NVENC's HEVC SPS and PPS say at 2560x1440.
fn nvenc_hevc_sps() -> hevc::Sps {
    hevc::Sps {
        chroma_format_idc: 1,
        coded_width: 2560,
        coded_height: 1440,
        log2_max_pic_order_cnt_lsb: 8,
        log2_min_cb_size: 3,
        log2_ctb_size: 5,
        sample_adaptive_offset: true,
        temporal_mvp: true,
        ..hevc::Sps::default()
    }
}

fn nvenc_hevc_pps() -> hevc::Pps {
    hevc::Pps {
        num_ref_idx_l0_default_active: 1,
        num_ref_idx_l1_default_active: 1,
        ..hevc::Pps::default()
    }
}

// Every branch of HEVC's slice header: sets in the SPS to name and predict
// from, long-term pictures, extra header bits, list changes, an 18-bit
// segment address.
fn busy_hevc_sps() -> hevc::Sps {
    let set = |before: &[(i32, bool)], after: &[(i32, bool)]| hevc::RefPicSet {
        before: before.to_vec(),
        after: after.to_vec(),
    };
    hevc::Sps {
        chroma_format_idc: 3,
        separate_colour_plane: true,
        coded_width: 8192,
        coded_height: 8192,
        log2_max_pic_order_cnt_lsb: 16,
        log2_min_cb_size: 3,
        log2_ctb_size: 4,
        sample_adaptive_offset: true,
        short_term_ref_pic_sets: vec![
            set(&[(-1, true), (-2, false)], &[]),
            set(&[(-1, true)], &[(1, true), (3, false)]),
            set(&[], &[]),
        ],
        long_term_ref_pics_present: true,
        long_term_ref_pics_sps: vec![true, false, true],
        temporal_mvp: true,
        ..hevc::Sps::default()
    }
}

fn busy_hevc_pps() -> hevc::Pps {
    hevc::Pps {
        dependent_slice_segments_enabled: true,
        output_flag_present: true,
        num_extra_slice_header_bits: 7,
        num_ref_idx_l0_default_active: 15,
        num_ref_idx_l1_default_active: 15,
        lists_modification_present: true,
        ..hevc::Pps::default()
    }
}

fn read_everything(nal: &Nal<'_>) {
    let _ = nal.kind();
    let _ = nal.ref_idc();
    let _ = annexb::slice_type(nal);
    let _ = annexb::parse_sps(nal);
    let _ = annexb::parse_pps(nal);
    let _ = annexb::slice_header(nal, &nvenc_sps(), &nvenc_pps());
    let _ = annexb::slice_header(nal, &busy_sps(), &busy_pps());

    let _ = hevc::kind(nal);
    let _ = hevc::temporal_id(nal);
    let _ = (hevc::is_slice(nal), hevc::is_idr(nal), hevc::is_irap(nal));
    for codec in [Codec::H264, Codec::Hevc] {
        let _ = (nal.is_slice_in(codec), nal.is_idr_in(codec));
    }
    let _ = hevc::parse_sps(nal);
    let _ = hevc::parse_pps(nal);
    let _ = hevc::slice_header(nal, &nvenc_hevc_sps(), &nvenc_hevc_pps());
    let _ = hevc::slice_header(nal, &busy_hevc_sps(), &busy_hevc_pps());
}

// HEVC's two header bytes for a VPS, SPS, PPS, IDR, CRA and ordinary frame,
// TemporalId 0.
const HEVC_HEADERS: [[u8; 2]; 6] = [
    [0x40, 1],
    [0x42, 1],
    [0x44, 1],
    [0x26, 1],
    [0x2a, 1],
    [0x02, 1],
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5000))]

    #[test]
    fn random_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..2048)) {
        for nal in annexb::nal_units(&data) {
            read_everything(&nal);
        }
        // Parsers see NAL units that did not come from the splitter too.
        read_everything(&Nal { data: &data });
        let _ = hevc::coded_sizes(&data).count();
    }

    #[test]
    fn after_a_header_byte(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        for header in [0x67u8, 0x68, 0x65, 0x41, 0x01] {
            let mut nal = vec![header];
            nal.extend_from_slice(&data);
            read_everything(&Nal { data: &nal });
        }
        for header in HEVC_HEADERS {
            let mut nal = header.to_vec();
            nal.extend_from_slice(&data);
            read_everything(&Nal { data: &nal });
        }
    }

    // Mostly ones: long runs of set bits are what walk the HEVC readers into
    // their optional parts (VUI, sets predicted from others, long-term
    // pictures, list changes) instead of stopping at the first flag.
    #[test]
    fn dense_after_an_hevc_header(
        data in proptest::collection::vec(prop_oneof![3 => Just(0xffu8), 1 => any::<u8>()], 0..512),
    ) {
        for header in HEVC_HEADERS {
            let mut nal = header.to_vec();
            nal.extend_from_slice(&data);
            read_everything(&Nal { data: &nal });
        }
    }

    #[test]
    fn split_round_trip(
        units in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..200), 1..10),
        four_byte in proptest::collection::vec(any::<bool>(), 10),
    ) {
        // Make each unit look like one an encoder writes: a nonzero header
        // byte, emulation prevention, and a last byte that is not zero.
        let escaped: Vec<Vec<u8>> = units
            .iter()
            .map(|unit| {
                let mut out = vec![unit[0] | 0x01];
                let mut zeros = 0;
                for &byte in &unit[1..] {
                    if zeros >= 2 && byte <= 3 {
                        out.push(3);
                        zeros = 0;
                    }
                    zeros = if byte == 0 { zeros + 1 } else { 0 };
                    out.push(byte);
                }
                if out.last() == Some(&0) {
                    out.push(0x80);
                }
                out
            })
            .collect();
        let mut stream = Vec::new();
        for (unit, &long) in escaped.iter().zip(&four_byte) {
            if long {
                stream.push(0);
            }
            stream.extend_from_slice(&[0, 0, 1]);
            stream.extend_from_slice(unit);
        }
        let back: Vec<&[u8]> = annexb::nal_units(&stream).map(|n| n.data).collect();
        let expected: Vec<&[u8]> = escaped.iter().map(Vec::as_slice).collect();
        prop_assert_eq!(back, expected);
    }
}
