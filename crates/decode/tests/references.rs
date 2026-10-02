// What NVENC's streams say about references around invalidations, held to the
// rules a strict decoder holds a stream to: a picture names only pictures the
// one before it kept, so none comes back once dropped; the decoder never has
// to hold more than the SPS says; nothing is reordered and nothing is kept
// long-term. Recordings of such streams decoded without a single warning in
// FFmpeg's software decoder with every error check on, and whole on NVIDIA's,
// while an Intel Iris Xe failed on a live share of the same kind; this keeps
// the stream side of that finding checked.

mod common;

use annexb::{ListChange, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS, SliceType, hevc};
use decode::Codec;
use encode::{AccessUnit, Recovery};

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FRAMES: u64 = 200;
// (reported after frame, lost frame): at once, and 1, 3 and 6 frames late.
const REPORTS: [(u64, u64); 4] = [(60, 60), (91, 90), (123, 120), (156, 150)];

fn record(gpu: &common::Gpu, codec: Codec) -> Vec<AccessUnit> {
    let mut stream = Stream::new(gpu, codec, WIDTH, HEIGHT);
    let mut units = Vec::new();
    for n in 0..FRAMES {
        let unit = stream.next(false);
        assert!(!unit.idr || n == 0, "frame {n} is an IDR");
        units.push(unit);
        for &(_, lost) in REPORTS.iter().filter(|r| r.0 == n) {
            assert_eq!(stream.encoder.recover(lost), Recovery::Invalidated);
        }
    }
    units
}

fn hevc_rules(units: &[AccessUnit]) {
    let (mut sps, mut pps) = (None, None);
    let mut kept: Vec<i64> = Vec::new();
    let mut previous = 0i64;
    for unit in units {
        let n = unit.index;
        for nal in annexb::nal_units(&unit.data) {
            match hevc::kind(&nal) {
                hevc::SPS => sps = hevc::parse_sps(&nal),
                hevc::PPS => pps = hevc::parse_pps(&nal),
                _ => {}
            }
        }
        let (sps, pps) = (sps.as_ref().expect("an SPS"), pps.as_ref().expect("a PPS"));
        assert_eq!(sps.max_num_reorder_pics, 0, "frame {n}: reordering");
        let nal = annexb::nal_units(&unit.data)
            .find(hevc::is_slice)
            .expect("a slice");
        let header = hevc::slice_header(&nal, sps, pps).expect("a readable slice header");
        assert_eq!(header.long_term_pics, 0, "frame {n}: long-term pictures");
        let Some(set) = header.ref_pic_set else {
            kept = vec![0];
            previous = 0;
            continue;
        };
        assert!(set.after.is_empty(), "frame {n}: later pictures");
        let max_lsb = 1i64 << sps.log2_max_pic_order_cnt_lsb;
        let lsb = i64::from(header.pic_order_cnt_lsb);
        let previous_lsb = previous.rem_euclid(max_lsb);
        let mut msb = previous - previous_lsb;
        if lsb < previous_lsb && previous_lsb - lsb >= max_lsb / 2 {
            msb += max_lsb;
        } else if lsb > previous_lsb && lsb - previous_lsb > max_lsb / 2 {
            msb -= max_lsb;
        }
        let poc = msb + lsb;
        previous = poc;
        let named: Vec<i64> = set
            .before
            .iter()
            .map(|&(d, _)| poc + i64::from(d))
            .collect();
        for p in &named {
            assert!(
                kept.contains(p),
                "frame {n} names picture {p}, which the picture before it had dropped ({kept:?})"
            );
        }
        assert!(
            named.len() < sps.max_dec_pic_buffering as usize,
            "frame {n} keeps {} pictures, the SPS allows {} with the picture itself",
            named.len(),
            sps.max_dec_pic_buffering
        );
        kept = named;
        kept.push(poc);
    }
}

fn h264_rules(units: &[AccessUnit]) {
    let (mut sps, mut pps) = (None, None);
    // Short-term references, oldest first: (frame_num, frame).
    let mut held: Vec<(u32, u64)> = Vec::new();
    let mut last_frame_num = None;
    for unit in units {
        let n = unit.index;
        let mut slice = None;
        for nal in annexb::nal_units(&unit.data) {
            match nal.kind() {
                NAL_SPS => sps = annexb::parse_sps(&nal),
                NAL_PPS => pps = annexb::parse_pps(&nal),
                NAL_SLICE | NAL_IDR if slice.is_none() => slice = Some(nal),
                _ => {}
            }
        }
        let (sps, pps) = (sps.as_ref().expect("an SPS"), pps.as_ref().expect("a PPS"));
        let vui = sps.vui.as_ref().expect("a VUI");
        assert_eq!(vui.max_num_reorder_frames, Some(0), "frame {n}: reordering");
        let nal = slice.expect("a slice");
        let header = annexb::slice_header(&nal, sps, pps).expect("a readable slice header");
        let max = 1i64 << sps.log2_max_frame_num;
        let frame_num = i64::from(header.frame_num);
        if nal.kind() == NAL_IDR {
            held.clear();
        } else if let Some(last) = last_frame_num {
            assert_eq!(frame_num, (last + 1) % max, "frame {n}: a gap in frame_num");
        }
        assert!(header.memory_ops.is_none(), "frame {n}: memory operations");
        if header.slice_type == SliceType::P {
            // PicNum of a held frame, as 8.2.4.1 wraps it.
            let pic_num = |f: u32| {
                let f = i64::from(f);
                if f > frame_num { f - max } else { f }
            };
            let mut pred = frame_num;
            for change in &header.list0_changes {
                let mut no_wrap = match *change {
                    ListChange::Down(d) => pred - i64::from(d),
                    ListChange::Up(d) => pred + i64::from(d),
                    ListChange::LongTerm(_) => panic!("frame {n}: a long-term reference"),
                };
                no_wrap = no_wrap.rem_euclid(max);
                pred = no_wrap;
                let wanted = if no_wrap > frame_num {
                    no_wrap - max
                } else {
                    no_wrap
                };
                assert!(
                    held.iter().any(|&(f, _)| pic_num(f) == wanted),
                    "frame {n} reorders list 0 to picture number {wanted}, which is not held ({held:?})"
                );
            }
        }
        if held.len() as u32 == sps.max_num_ref_frames {
            held.remove(0);
        }
        held.push((header.frame_num, n));
        last_frame_num = Some(frame_num);
    }
}

fn invalidations_keep_to_the_rules(codec: Codec) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let units = record(&gpu, codec);
    match codec {
        Codec::Hevc => hevc_rules(&units),
        Codec::H264 => h264_rules(&units),
    }
    // Every frame given, as when none is lost: the frames the reports name
    // stand for frames that failed in a decoder rather than on the way.
    let mut reader = Reader::new(&gpu);
    for unit in &units {
        let n = unit.index;
        let decoded = decoder
            .decode(&unit.data)
            .unwrap_or_else(|e| panic!("frame {n}: {e}"))
            .unwrap_or_else(|| panic!("frame {n} gave no picture"));
        assert_eq!(reader.frame_number(&decoded), Some(n as u32), "frame {n}");
    }
    println!(
        "{codec}: {FRAMES} frames with losses reported as {REPORTS:?}: every reference kept to the rules and every frame decoded whole"
    );
}

#[test]
fn hevc_invalidations_keep_to_the_rules() {
    invalidations_keep_to_the_rules(Codec::Hevc);
}

#[test]
fn h264_invalidations_keep_to_the_rules() {
    invalidations_keep_to_the_rules(Codec::H264);
}
