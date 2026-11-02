// FFmpeg's HEVC decoder sizes its own tables in CPU memory by an SPS as soon
// as a slice brings the SPS in, before it asks for a format, so the format
// callback's size check in fields.c comes too late for HEVC: a friend's SPS
// for 16384x16000 made FFmpeg 8.1.3 commit about 165 MB for about 25 ms
// before it was refused. Every SPS of every HEVC access unit is read here
// first, wherever it is in the unit, since a slice after it in the same unit
// brings it in, and a unit with one past the limit, or one this reader cannot
// read, never reaches FFmpeg.
//
// Reading an SPS costs up to about 0.06 ms, so a unit of nothing but SPSs
// would cost up to 15 ms. The SPSs are counted first, which reads none, and a
// unit with more than a stream can use is refused unread in about 0.13 ms.
// One with 16 is still read whole: 16 of the heaviest SPS this reader takes
// cost about 0.9 ms, past the quarter millisecond a unit is meant to cost
// here.
//
// In both codecs FFmpeg does all of a unit in one call nothing can interrupt,
// and part of what it sets aside stays until the decoder goes, so a unit that
// holds more than an encoder writes is refused too, before any of it is read.
// Counting takes one pass over the unit, which stops at the first too many.

use annexb::{Nal, hevc};

use crate::decoder::Codec;
use crate::error::DecodeError;
use crate::ffi::{booth_codec_hevc, booth_too_large};

// HEVC numbers SPSs 0 to 15, so no stream needs more in one access unit.
// Booth's encoders put one in front of each IDR.
const MOST_SPS: usize = 16;

// FFmpeg keeps a slot for every NAL unit a unit ever had in that place, about
// 4 KB each, so 2.75 MB of the smallest NAL units committed about 2.9 GB in
// one call and kept it until the watch ended.
//
// What encoders write is set by the slices. NVENC writes one, or several on a
// refreshing frame on a GPU that can neither invalidate nor refresh in a single
// slice, which is not measured. Windows' software encoder writes one per 96
// lines, up to 16 from 1536 lines on, behind an access unit delimiter, an SPS, a
// PPS and two SEIs. Nothing asks the encoders of AMD and Intel for slices, and
// they cannot be run here. So the limit is what can reach the GPU: FFmpeg's
// d3d11va takes 32 slices a picture in H.264 and loses the rest, and takes 256
// in HEVC and fails the frame at the next, which is how tests/failed.rs fails
// one. No picture with more ever showed whole on any copy of Booth.
const MOST_SLICES_H264: usize = 32;
const MOST_SLICES_HEVC: usize = 256;
const MOST_OTHER_UNITS: usize = 32;

// A slot also keeps its list of the NAL unit's emulation prevention bytes at
// the longest it ever was, up to 4 MiB, so a hostile stream can grow one slot
// with each large unit. Past this many NAL units a unit may hold no more than
// this many of those bytes, about what every slot starts with, so what can
// stay is 64 times 4 MiB in either codec: 256 MiB, about what a stream's
// surfaces take. Encoders write a few of them per MB, so an encoder would have
// to cut a picture past 64 NAL units and pad it with zeros as well.
const MOST_UNITS_ESCAPED: usize = 64;
const MOST_ESCAPES: usize = 1024;

// VPSs and PPSs in HEVC, whose SPSs have their own limit, SPSs and PPSs in
// H.264. Every encoder here puts one of each in front of an IDR, and FFmpeg
// works each PPS out as it comes. The longest an HEVC one takes is about 3.6 ms,
// with a tile for every block of an 8192x1152 picture, the longest and thinnest
// the size limit lets through at its full area (fields.c), so 8 take about 29
// ms. Two that take turns get past its check for a PPS it has already.
const MOST_SETS: usize = 8;

// Each one starts a picture: in H.264 a slice with first_mb_in_slice 0 ends the
// one before it and sends it to the GPU, about 390,000 times in one unit, and
// in HEVC first_slice_segment_in_pic_flag zeroes a picture's tables again and
// takes its surfaces after a start that failed.
const MOST_PICTURES: usize = 1;

// FFmpeg copies its list of the SEI messages it keeps for each message, so one
// unit full of the smallest took 1.2 s. 64 KB of them take at most about 2 ms,
// the most in the messages it allocates for and throws away. Encoders write
// about 300 bytes with an IDR.
const MOST_SEI: usize = 64 << 10;

fn is_sei(codec: Codec, nal: &Nal<'_>) -> bool {
    match codec {
        Codec::H264 => nal.kind() == annexb::NAL_SEI,
        Codec::Hevc => matches!(hevc::kind(nal), hevc::PREFIX_SEI | hevc::SUFFIX_SEI),
    }
}

pub(crate) fn check_unit(codec: Codec, access_unit: &[u8]) -> Result<(), DecodeError> {
    let most_units = MOST_OTHER_UNITS
        + match codec {
            Codec::H264 => MOST_SLICES_H264,
            Codec::Hevc => MOST_SLICES_HEVC,
        };
    let (mut units, mut sets, mut pictures, mut sei) = (0, 0, 0, 0);
    for data in annexb::after_start_codes(access_unit) {
        units += 1;
        if units > most_units {
            return Err(DecodeError::Packed {
                most: most_units,
                what: "NAL units",
            });
        }
        let nal = Nal { data };
        // The byte after a NAL unit's header is the slice's own: an emulation
        // prevention byte comes only after two zero bytes, and no header a
        // decoder takes ends in a zero byte.
        let (set, picture) = match codec {
            Codec::H264 => (
                matches!(nal.kind(), annexb::NAL_SPS | annexb::NAL_PPS),
                nal.is_slice() && data.get(1).is_some_and(|byte| byte & 0x80 != 0),
            ),
            Codec::Hevc => (
                matches!(hevc::kind(&nal), hevc::VPS | hevc::PPS),
                hevc::is_slice(&nal) && data.get(2).is_some_and(|byte| byte & 0x80 != 0),
            ),
        };
        sets += usize::from(set);
        if sets > MOST_SETS {
            return Err(DecodeError::Packed {
                most: MOST_SETS,
                what: "parameter sets",
            });
        }
        pictures += usize::from(picture);
        if pictures > MOST_PICTURES {
            return Err(DecodeError::ManyPictures);
        }
        if is_sei(codec, &nal) {
            sei += data.len();
            if sei > MOST_SEI {
                return Err(DecodeError::Packed {
                    most: MOST_SEI,
                    what: "bytes of SEI",
                });
            }
        }
    }
    if units > MOST_UNITS_ESCAPED
        && access_unit
            .windows(3)
            .filter(|w| *w == [0, 0, 3])
            .nth(MOST_ESCAPES)
            .is_some()
    {
        return Err(DecodeError::Packed {
            most: MOST_ESCAPES,
            what: "emulation prevention bytes in more than 64 NAL units",
        });
    }
    Ok(())
}

pub(crate) fn check_hevc(access_unit: &[u8]) -> Result<(), DecodeError> {
    let count = annexb::hevc::sps_count(access_unit);
    if count == 0 {
        return Ok(());
    }
    if count > MOST_SPS {
        return Err(DecodeError::TooManySps { count });
    }
    for size in annexb::hevc::coded_sizes(access_unit) {
        let (width, height) = size.ok_or(DecodeError::UnreadableSps)?;
        if booth_too_large(booth_codec_hevc, width.into(), height.into()) != 0 {
            return Err(DecodeError::Oversized { width, height });
        }
    }
    Ok(())
}

// A unit check_unit has passed, so the walk is short: what FFmpeg may have kept
// of a unit that gave no picture (src/decoder.rs).
pub(crate) fn sei_bytes(codec: Codec, access_unit: &[u8]) -> usize {
    annexb::after_start_codes(access_unit)
        .filter(|&data| is_sei(codec, &Nal { data }))
        .map(<[u8]>::len)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::{MAX_ACCESS_UNIT, hevc_idr};
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    const SPS: [u8; 2] = [33 << 1, 1];
    const PPS: [u8; 2] = [34 << 1, 1];
    const IDR: [u8; 2] = [19 << 1, 1];
    const TRAIL: [u8; 2] = [1 << 1, 1];

    // Bits of an RBSP, most significant first.
    struct Writer {
        bits: Vec<bool>,
    }

    impl Writer {
        fn new() -> Writer {
            Writer { bits: Vec::new() }
        }

        fn u(&mut self, n: u32, value: u32) -> &mut Writer {
            for i in (0..n).rev() {
                self.bits.push((value >> i) & 1 == 1);
            }
            self
        }

        fn ue(&mut self, value: u32) -> &mut Writer {
            let coded = u64::from(value) + 1;
            let len = 64 - coded.leading_zeros();
            for _ in 1..len {
                self.bits.push(false);
            }
            for i in (0..len).rev() {
                self.bits.push((coded >> i) & 1 == 1);
            }
            self
        }

        // Start code, header, then the RBSP with its stop bit and emulation
        // prevention.
        fn nal(&mut self, header: [u8; 2]) -> Vec<u8> {
            self.bits.push(true);
            while !self.bits.len().is_multiple_of(8) {
                self.bits.push(false);
            }
            let mut out = vec![0, 0, 0, 1, header[0], header[1]];
            let mut zeros = 0;
            for byte in self.bits.chunks(8) {
                let byte = byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b));
                if zeros >= 2 && byte <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                out.push(byte);
            }
            out
        }
    }

    // An SPS as NVENC writes one, Main profile at level 6, coded at
    // `width` x `height` with no conformance window.
    fn sps(width: u32, height: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u(4, 0).u(3, 0).u(1, 1);
        w.u(2, 0).u(1, 0).u(5, 1).u(32, 0x6000_0000);
        w.u(4, 0b1001).u(32, 0).u(12, 0).u(8, 180);
        w.ue(0).ue(1).ue(width).ue(height).u(1, 0);
        w.ue(0).ue(0).ue(4).u(1, 1).ue(12).ue(0).ue(0);
        w.ue(0).ue(2).ue(0).ue(3).ue(0).ue(0);
        w.u(1, 0).u(1, 1).u(1, 1).u(1, 0).ue(0).u(1, 0);
        w.u(1, 1).u(1, 0).u(1, 1);
        w.u(1, 0).u(1, 0).u(1, 1).u(3, 5).u(1, 0).u(1, 1);
        w.u(8, 1).u(8, 1).u(8, 1);
        w.u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0);
        w.nal(SPS)
    }

    // What surrounds the SPS in an IDR access unit: a PPS and a slice of
    // made-up picture data.
    fn pps() -> Vec<u8> {
        Writer::new().ue(0).ue(0).u(16, 0x5a5a).nal(PPS)
    }

    fn slice(header: [u8; 2]) -> Vec<u8> {
        let mut w = Writer::new();
        for i in 0..400u32 {
            w.u(8, i.wrapping_mul(2_654_435_761) >> 24);
        }
        w.nal(header)
    }

    fn idr(sps: &[u8]) -> Vec<u8> {
        [sps, &pps(), &slice(IDR)].concat()
    }

    #[test]
    fn booth_sizes_pass() {
        // NVENC codes 720 lines as 736 and 1080 as 1088.
        for (width, height) in [
            (1280, 736),
            (1920, 1088),
            (2560, 1440),
            (3440, 1440),
            (3840, 2160),
            (5120, 1440),
            (4096, 2304),
            (8192, 1152),
            (1152, 8192),
        ] {
            let unit = idr(&sps(width, height));
            assert!(check_hevc(&unit).is_ok(), "{width}x{height}");
        }
        // P frames carry no SPS, so nothing in them is read.
        assert!(check_hevc(&slice(TRAIL)).is_ok());
    }

    #[test]
    fn size_past_the_limit() {
        // The size that made FFmpeg commit about 165 MB, one four times the
        // area Booth takes, one past the limit only once FFmpeg rounds its
        // pool up to 128, one row of blocks past 4096x2304, and ones with room
        // in the area that are past 8192 on a side.
        for (width, height) in [
            (16384, 16000),
            (8192, 4608),
            (1168, 7312),
            (4096, 2320),
            (73728, 128),
            (8200, 128),
            (128, 8200),
        ] {
            let refused = check_hevc(&idr(&sps(width, height)));
            assert!(
                matches!(refused, Err(DecodeError::Oversized { width: w, height: h }) if (w, h) == (width, height)),
                "{width}x{height}: {refused:?}"
            );
        }
    }

    #[test]
    fn unreadable_sps() {
        let whole = sps(2560, 1440);
        // Cut after its start code, header and six bytes, inside the
        // profile.
        let cut = &whole[..12];
        let refused = check_hevc(&[cut, &pps(), &slice(IDR)].concat());
        assert!(
            matches!(refused, Err(DecodeError::UnreadableSps)),
            "{refused:?}"
        );
        // 128x128 coding tree blocks, which HEVC does not have.
        let mut w = Writer::new();
        w.u(4, 0).u(3, 0).u(1, 1);
        w.u(2, 0).u(1, 0).u(5, 1).u(32, 0x6000_0000);
        w.u(4, 0b1001).u(32, 0).u(12, 0).u(8, 180);
        w.ue(0).ue(1).ue(2560).ue(1440).u(1, 0);
        w.ue(0).ue(0).ue(4).u(1, 1).ue(12).ue(0).ue(0);
        w.ue(0).ue(4).ue(0).ue(3).ue(0).ue(0);
        let refused = check_hevc(&idr(&w.nal(SPS)));
        assert!(
            matches!(refused, Err(DecodeError::UnreadableSps)),
            "{refused:?}"
        );
    }

    #[test]
    fn every_sps_is_read() {
        // A good SPS first and a hostile one after the slice: FFmpeg keeps
        // the second for the next slice that names it.
        let unit = [idr(&sps(2560, 1440)), sps(16384, 16000)].concat();
        let refused = check_hevc(&unit);
        assert!(
            matches!(
                refused,
                Err(DecodeError::Oversized {
                    width: 16384,
                    height: 16000
                })
            ),
            "{refused:?}"
        );
        // One in front of a P frame, which a slice of it brings in as
        // surely as an IDR's.
        let unit = [sps(16384, 16000), slice(TRAIL)].concat();
        assert!(matches!(
            check_hevc(&unit),
            Err(DecodeError::Oversized { .. })
        ));
    }

    #[test]
    fn more_than_16_spss() {
        let good = sps(2560, 1440);
        let sixteen = [good.repeat(16), pps(), slice(IDR)].concat();
        assert!(check_hevc(&sixteen).is_ok());
        // One more counts wherever it is, after the slice too.
        let refused = check_hevc(&[&sixteen[..], &good].concat());
        assert!(
            matches!(refused, Err(DecodeError::TooManySps { count: 17 })),
            "{refused:?}"
        );
        // The first of 16 is read and refused for what it is; the first of
        // 17 never is.
        let huge = sps(16384, 16000);
        let cut = &good[..12];
        for first in [&huge[..], cut] {
            let read = check_hevc(&[first, &good.repeat(15), &pps(), &slice(IDR)].concat());
            assert!(
                matches!(
                    read,
                    Err(DecodeError::Oversized { .. } | DecodeError::UnreadableSps)
                ),
                "{read:?}"
            );
            let unread = check_hevc(&[first, &good.repeat(16), &pps(), &slice(IDR)].concat());
            assert!(
                matches!(unread, Err(DecodeError::TooManySps { count: 17 })),
                "{unread:?}"
            );
        }
    }

    // The SPS this reader takes longest over and still reads whole: seven
    // sub-layers, every scaling list written out, 64 reference picture sets
    // of 16 pictures and 32 long-term ones, each value as long as its check
    // lets it be. About 13.5 KB.
    fn heaviest_sps() -> Vec<u8> {
        let most = u32::MAX - 1;
        let mut w = Writer::new();
        w.u(4, 0).u(3, 6).u(1, 1);
        w.u(2, 0).u(1, 0).u(5, 1).u(32, 0x6000_0000);
        w.u(4, 0b1001).u(32, 0).u(12, 0).u(8, 180);
        for _ in 0..6 {
            w.u(1, 1).u(1, 1);
        }
        w.u(2, 0).u(2, 0);
        for _ in 0..6 {
            w.u(32, 0).u(32, 0).u(24, 0).u(8, 180);
        }
        // A conformance window, which nothing bounds.
        w.ue(0).ue(1).ue(2560).ue(1440).u(1, 1);
        w.ue(most).ue(most).ue(most).ue(most);
        w.ue(0).ue(0).ue(12).u(1, 1);
        for _ in 0..7 {
            w.ue(most).ue(most).ue(most);
        }
        w.ue(0).ue(2).ue(most).ue(most).ue(most).ue(most);
        w.u(1, 1).u(1, 1);
        for size_id in 0..4u32 {
            let matrices = if size_id == 3 { 2 } else { 6 };
            for _ in 0..matrices {
                w.u(1, 1);
                if size_id > 1 {
                    w.ue(most);
                }
                for _ in 0..(1u32 << (4 + (size_id << 1))).min(64) {
                    w.ue(most);
                }
            }
        }
        w.u(1, 1).u(1, 1).u(1, 1).u(8, 0).ue(most).ue(most).u(1, 0);
        w.ue(64);
        for index in 0..64 {
            if index > 0 {
                w.u(1, 0);
            }
            w.ue(16).ue(0);
            for _ in 0..16 {
                w.ue((1 << 15) - 1).u(1, 1);
            }
        }
        w.u(1, 1).ue(32);
        for _ in 0..32 {
            w.u(16, 0xffff).u(1, 1);
        }
        w.u(1, 1).u(1, 0).u(1, 1);
        w.u(1, 1).u(8, 255).u(16, 1).u(16, 1);
        w.u(1, 1).u(1, 1).u(1, 1).u(3, 5).u(1, 0).u(1, 1);
        w.u(8, 1).u(8, 1).u(8, 1);
        w.u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0);
        w.nal(SPS)
    }

    // The fastest of 20, so a preemption does not count.
    fn fastest(
        unit: &[u8],
        read: impl Fn(&[u8]) -> Result<(), DecodeError>,
    ) -> (Duration, Result<(), DecodeError>) {
        let mut best = Duration::MAX;
        let mut answer = Ok(());
        for _ in 0..20 {
            let started = Instant::now();
            answer = read(std::hint::black_box(unit));
            best = best.min(started.elapsed());
        }
        (best, answer)
    }

    // The SPS guard's own time, which is what is held to the quarter
    // millisecond, and with the count and decode's search for the first
    // picture in front of it, which together are all decode reads of a unit
    // before FFmpeg sees it. Those two add up to about 0.2 ms and are only
    // printed.
    fn timed(name: &str, unit: &[u8]) -> (Duration, Result<(), DecodeError>) {
        let (took, answer) = fastest(unit, check_hevc);
        let (with_search, _) = fastest(unit, |unit| {
            check_unit(Codec::Hevc, unit)?;
            std::hint::black_box(hevc_idr(unit));
            check_hevc(unit)
        });
        println!(
            "{name}, {} bytes: {answer:?} in {:.3} ms, {:.3} ms with the count and the search for the first picture",
            unit.len(),
            took.as_secs_f64() * 1000.0,
            with_search.as_secs_f64() * 1000.0
        );
        (took, answer)
    }

    fn fill_to_the_limit(unit: &mut Vec<u8>, piece: &[u8]) {
        while unit.len() + piece.len() <= MAX_ACCESS_UNIT {
            unit.extend_from_slice(piece);
        }
    }

    // Only a release build, what booth.exe is, is held to the time; a debug
    // build counts about 140 times slower.
    #[test]
    fn packed_unit_in_one_pass() {
        const QUARTER_MS: Duration = Duration::from_micros(250);
        let held = |took: Duration| cfg!(debug_assertions) || took < QUARTER_MS;
        for (name, one) in [
            ("NVENC's", sps(2560, 1440)),
            ("the heaviest", heaviest_sps()),
        ] {
            let mut flood = Vec::new();
            fill_to_the_limit(&mut flood, &one);
            let (took, answer) = timed(&format!("{name} SPS of {} bytes", one.len()), &flood);
            assert!(
                matches!(answer, Err(DecodeError::TooManySps { count }) if count == flood.len() / one.len()),
                "{answer:?}"
            );
            assert!(held(took), "{took:?}");
        }
        // Units with no SPS: a P frame, which is every frame but an IDR, and
        // one NAL unit that is no picture, which decode searches to the end
        // for a picture.
        for (name, header) in [("A P frame", TRAIL), ("No picture", [0x46, 1])] {
            let mut unit = vec![0, 0, 1, header[0], header[1]];
            fill_to_the_limit(&mut unit, &[0xff]);
            let (took, answer) = timed(name, &unit);
            assert!(answer.is_ok());
            assert!(held(took), "{took:?}");
        }
        // The most a unit that passes can cost: 16 of the heaviest SPSs and
        // the rest in one NAL unit. Reading the 16 takes about 0.9 ms, past
        // the quarter millisecond, so it is only printed.
        let mut worst = heaviest_sps().repeat(MOST_SPS);
        fill_to_the_limit(&mut worst, &[0xff]);
        let (_, answer) = timed("16 of the heaviest SPS, then one long NAL unit", &worst);
        assert!(answer.is_ok());
    }

    // Start code and header, then `body` bytes of 0x55.
    fn unit(header: &[u8], body: usize) -> Vec<u8> {
        let mut out = vec![0, 0, 1];
        out.extend_from_slice(header);
        out.resize(out.len() + body, 0x55);
        out
    }

    const H264_SLICE: [u8; 2] = [0x41, 0x55];
    const H264_FIRST_SLICE: [u8; 2] = [0x41, 0x88];
    const H264_IDR: [u8; 2] = [0x65, 0x88];
    const HEVC_SLICE: [u8; 3] = [1 << 1, 1, 0x55];
    const HEVC_FIRST_SLICE: [u8; 3] = [1 << 1, 1, 0x88];
    const HEVC_IDR: [u8; 3] = [19 << 1, 1, 0x88];

    fn refused_as(unit: &[u8], codec: Codec, what: &str) {
        match check_unit(codec, unit) {
            Err(DecodeError::Packed { what: said, .. }) if said == what => {}
            other => panic!("{codec}, {} bytes, for {what}: {other:?}", unit.len()),
        }
    }

    // As Windows' software encoder writes them at the most, measured; the
    // other encoders here write fewer of each.
    #[test]
    fn real_units_pass() {
        let mut idr = [
            unit(&[9, 0x10], 0),
            unit(&[7], 26),
            unit(&[8], 3),
            unit(&[6], 50),
            unit(&[6], 244),
            unit(&H264_IDR, 1000),
        ]
        .concat();
        for _ in 1..16 {
            idr.extend_from_slice(&unit(&[0x65, 0x55], 100_000));
        }
        assert!(check_unit(Codec::H264, &idr).is_ok());
        // NVENC's, with nothing but an IDR slice behind the parameter sets.
        let idr = [
            unit(&[32 << 1, 1], 20),
            unit(&[33 << 1, 1], 40),
            unit(&[34 << 1, 1], 7),
            unit(&HEVC_IDR, 100_000),
        ]
        .concat();
        assert!(check_unit(Codec::Hevc, &idr).is_ok());
        assert!(check_unit(Codec::Hevc, &unit(&HEVC_FIRST_SLICE, 9)).is_ok());
        assert!(check_unit(Codec::H264, &unit(&H264_FIRST_SLICE, 9)).is_ok());
    }

    // Each of 2.75 MB's worth would have kept about 4 KB in FFmpeg for the
    // rest of the watch.
    #[test]
    fn nal_units_past_the_limit() {
        let aud = unit(&[9], 0);
        for (codec, most, piece) in [
            (Codec::H264, 64, aud),
            (Codec::Hevc, 288, unit(&[0x46, 1], 0)),
        ] {
            assert!(check_unit(codec, &piece.repeat(most)).is_ok(), "{codec}");
            refused_as(&piece.repeat(most + 1), codec, "NAL units");
            refused_as(
                &piece.repeat(MAX_ACCESS_UNIT / piece.len()),
                codec,
                "NAL units",
            );
            // Padding and headers FFmpeg skips each take a slot's place first.
            refused_as(&[0u8, 0, 1].repeat(most + 1), codec, "NAL units");
            refused_as(&[0u8, 0, 1, 0xff].repeat(most + 1), codec, "NAL units");
        }
    }

    #[test]
    fn slices_up_to_what_reaches_the_gpu() {
        // 32 of H.264's and 32 others, and what tests/failed.rs fails a frame
        // with: HEVC's 256 slices past the first behind three parameter sets.
        let others = unit(&[6], 9).repeat(31);
        let slices = unit(&H264_SLICE, 9).repeat(32);
        let full = [&unit(&H264_IDR, 9)[..], &others, &slices].concat();
        assert!(check_unit(Codec::H264, &full).is_ok());
        let sets = [
            unit(&[32 << 1, 1], 9),
            unit(&[33 << 1, 1], 9),
            unit(&[34 << 1, 1], 9),
        ]
        .concat();
        let slices = unit(&HEVC_SLICE, 9).repeat(256);
        let failing = [&sets[..], &unit(&HEVC_FIRST_SLICE, 9), &slices].concat();
        assert!(check_unit(Codec::Hevc, &failing).is_ok());
    }

    #[test]
    fn escapes_past_64_units() {
        let run = |escapes: usize| [0u8, 0, 3].repeat(escapes);
        let slice = unit(&HEVC_SLICE, 9);
        let with = |units: usize, escapes: usize| {
            [
                &unit(&HEVC_IDR, 9)[..],
                &run(escapes),
                &slice.repeat(units - 1),
            ]
            .concat()
        };
        assert!(check_unit(Codec::Hevc, &with(65, 1024)).is_ok());
        let what = "emulation prevention bytes in more than 64 NAL units";
        refused_as(&with(65, 1025), Codec::Hevc, what);
        // As many as a padded frame can have, in as many units as an encoder
        // might cut one into.
        assert!(check_unit(Codec::Hevc, &with(64, 500_000)).is_ok());
    }

    #[test]
    fn parameter_sets_past_the_limit() {
        // Alternating so that FFmpeg's check for one it has already does not
        // save it, wherever they are in the unit, with the forbidden bit too.
        let hevc = |kind: u8| unit(&[kind << 1, 1], 9);
        let idr = unit(&HEVC_IDR, 9);
        let eight = [hevc(34).repeat(4), hevc(32).repeat(4)].concat();
        assert!(check_unit(Codec::Hevc, &[&eight[..], &idr].concat()).is_ok());
        for ninth in [hevc(34), hevc(32), unit(&[(34 << 1) | 0x80, 1], 9)] {
            let late = [&eight[..], &idr, &ninth].concat();
            refused_as(&late, Codec::Hevc, "parameter sets");
        }
        // An SPS has MOST_SPS of its own in HEVC, read in check_hevc.
        let sixteen = hevc(33).repeat(MOST_SPS);
        assert!(check_unit(Codec::Hevc, &[&eight[..], &sixteen, &idr].concat()).is_ok());

        let idr = unit(&H264_IDR, 9);
        let eight = [unit(&[0x67], 9).repeat(2), unit(&[0x68], 9).repeat(6)].concat();
        assert!(check_unit(Codec::H264, &[&eight[..], &idr].concat()).is_ok());
        for ninth in [unit(&[0x67], 9), unit(&[0x68], 9), unit(&[0x08], 9)] {
            let late = [&eight[..], &idr, &ninth].concat();
            refused_as(&late, Codec::H264, "parameter sets");
        }
    }

    #[test]
    fn more_than_one_picture() {
        let many = |first: &[u8], second: &[u8], codec: Codec| {
            let refused = check_unit(codec, &[unit(first, 9), unit(second, 9)].concat());
            assert!(
                matches!(refused, Err(DecodeError::ManyPictures)),
                "{refused:?}"
            );
        };
        many(&H264_IDR, &H264_IDR, Codec::H264);
        many(&H264_FIRST_SLICE, &H264_IDR, Codec::H264);
        many(&H264_IDR, &H264_FIRST_SLICE, Codec::H264);
        many(&H264_FIRST_SLICE, &[0x61, 0x88], Codec::H264);
        many(&HEVC_IDR, &HEVC_IDR, Codec::Hevc);
        many(&HEVC_IDR, &HEVC_FIRST_SLICE, Codec::Hevc);
        // Any slice type, which FFmpeg sorts out for itself after this.
        many(&HEVC_FIRST_SLICE, &[9 << 1, 1, 0x80], Codec::Hevc);
        // The other slices of a picture are the rest of that picture, and a
        // slice cut short has no flag to read.
        let rest = [
            &unit(&H264_IDR, 9)[..],
            &unit(&H264_SLICE, 9),
            &[0, 0, 1, 0x41],
        ]
        .concat();
        assert!(check_unit(Codec::H264, &rest).is_ok());
        let rest = [
            &unit(&HEVC_IDR, 9)[..],
            &unit(&HEVC_SLICE, 9),
            &[0, 0, 1, 2, 1],
        ]
        .concat();
        assert!(check_unit(Codec::Hevc, &rest).is_ok());
        // An SEI whose first byte has the same bit set is no picture.
        let sei = [&unit(&H264_IDR, 9)[..], &unit(&[6, 0x88], 9)].concat();
        assert!(check_unit(Codec::H264, &sei).is_ok());
    }

    #[test]
    fn sei_past_the_limit() {
        for (codec, headers) in [
            (Codec::H264, vec![vec![6]]),
            (Codec::Hevc, vec![vec![39 << 1, 1], vec![40 << 1, 1]]),
        ] {
            for header in headers {
                let whole = unit(&header, MOST_SEI - header.len());
                assert!(check_unit(codec, &whole).is_ok(), "{codec}");
                refused_as(
                    &unit(&header, MOST_SEI + 1 - header.len()),
                    codec,
                    "bytes of SEI",
                );
                // In pieces.
                let half = unit(&header, MOST_SEI / 2 - header.len());
                assert!(check_unit(codec, &half.repeat(2)).is_ok(), "{codec}");
                let more = [&half.repeat(2)[..], &unit(&header, 0)].concat();
                refused_as(&more, codec, "bytes of SEI");
            }
        }
    }

    // One pass, and a unit that holds too many costs what the count read of it.
    // Past 64 NAL units it costs a look for its escapes too, which is only
    // printed, since no encoder comes here.
    #[test]
    fn count_in_one_pass() {
        const QUARTER_MS: Duration = Duration::from_micros(250);
        let held = |took: Duration| cfg!(debug_assertions) || took < QUARTER_MS;
        let timed = |name: &str, unit: &[u8]| {
            let (took, answer) = fastest(unit, |unit| check_unit(Codec::Hevc, unit).map(drop));
            println!(
                "{name}, {} bytes: {answer:?} in {:.3} ms",
                unit.len(),
                took.as_secs_f64() * 1000.0
            );
            took
        };
        let mut long = vec![0, 0, 1, TRAIL[0], TRAIL[1]];
        fill_to_the_limit(&mut long, &[0xff]);
        let mut padding = Vec::new();
        fill_to_the_limit(&mut padding, &[0, 0, 1]);
        let mut smallest = Vec::new();
        fill_to_the_limit(&mut smallest, &[0, 0, 1, 0x46, 1]);
        for (name, unit) in [
            ("One long slice", &long),
            ("Padding", &padding),
            ("The smallest NAL units", &smallest),
        ] {
            let took = timed(name, unit);
            assert!(held(took), "{name}: {took:?}");
        }
        let mut most = unit(&[0x46, 1], 0).repeat(288);
        fill_to_the_limit(&mut most, &[0xff]);
        timed("288 NAL units", &most);
    }

    // Start codes and SPS headers mixed into the noise, so the noise reaches
    // the SPS reader. About a third of the pieces are SPS headers, so some
    // units carry more than 16 and most fewer.
    fn noise() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop_oneof![
            Just(vec![0u8, 0, 1, SPS[0], SPS[1]]),
            Just(vec![0u8, 0, 1]),
            prop::collection::vec(any::<u8>(), 0..32)
        ];
        prop::collection::vec(piece, 0..64).prop_map(|pieces| pieces.concat())
    }

    // Whatever the answer, it has to be the one the rule gives: a unit
    // passes only when it carries at most 16 SPSs and every one reads and is
    // within the limit.
    fn agrees_with_the_rule(unit: &[u8]) -> Result<(), TestCaseError> {
        let sizes: Vec<Option<(u32, u32)>> = annexb::hevc::coded_sizes(unit).collect();
        let too_large =
            |(w, h): (u32, u32)| booth_too_large(booth_codec_hevc, w.into(), h.into()) != 0;
        let too_many = sizes.len() > MOST_SPS;
        match check_hevc(unit) {
            Ok(()) => {
                prop_assert!(!too_many && sizes.iter().all(|s| s.is_some_and(|s| !too_large(s))))
            }
            Err(DecodeError::TooManySps { count }) => {
                prop_assert!(too_many);
                prop_assert_eq!(count, sizes.len());
            }
            Err(DecodeError::UnreadableSps) => {
                prop_assert!(!too_many && sizes.contains(&None))
            }
            Err(DecodeError::Oversized { width, height }) => {
                prop_assert!(!too_many && too_large((width, height)));
                prop_assert!(sizes.contains(&Some((width, height))));
            }
            Err(other) => return Err(TestCaseError::fail(format!("{other:?}"))),
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn random_bytes_never_panic(bytes in noise()) {
            agrees_with_the_rule(&bytes)?;
            let _ = check_unit(Codec::H264, &bytes);
            let _ = check_unit(Codec::Hevc, &bytes);
        }

        #[test]
        fn damaged_sps_never_panics(
            width in 0u32..20_000,
            height in 0u32..20_000,
            flips in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 0..6),
            keep in any::<prop::sample::Index>(),
        ) {
            let mut sps = sps(width, height);
            // Past the start code and header, which keep it an SPS.
            let body = sps.len() - 6;
            for (at, with) in flips {
                sps[6 + at.index(body)] ^= with;
            }
            sps.truncate(6 + keep.index(body + 1));
            agrees_with_the_rule(&idr(&sps))?;
            agrees_with_the_rule(&[pps(), sps.clone(), slice(TRAIL)].concat())?;
        }
    }

    #[test]
    fn sei_bytes_add_up() {
        let h264 = [
            unit(&[7], 9),
            unit(&[6], 50),
            unit(&H264_IDR, 9),
            unit(&[6], 244),
        ]
        .concat();
        assert_eq!(sei_bytes(Codec::H264, &h264), 51 + 245);
        let hevc = [
            unit(&[39 << 1, 1], 8),
            unit(&HEVC_IDR, 9),
            unit(&[40 << 1, 1], 18),
        ]
        .concat();
        assert_eq!(sei_bytes(Codec::Hevc, &hevc), 10 + 20);
        assert_eq!(sei_bytes(Codec::Hevc, &unit(&HEVC_IDR, 9)), 0);
    }
}
