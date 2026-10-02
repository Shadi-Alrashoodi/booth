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

use crate::error::DecodeError;
use crate::ffi::{booth_codec_hevc, booth_too_large};

// HEVC numbers SPSs 0 to 15, so no stream needs more in one access unit.
// Booth's encoders put one in front of each IDR.
const MOST_SPS: usize = 16;

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
    const SEI: [u8; 2] = [39 << 1, 1];

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
        // pool up to 128, and one row of blocks past 4096x2304.
        for (width, height) in [(16384, 16000), (8192, 4608), (648, 14384), (4096, 2320)] {
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

    // The guard's own time, which is what is held to the quarter millisecond,
    // and with decode's search for the first picture in front of it, which
    // together are all decode reads of a unit before FFmpeg sees it. The
    // search adds up to about 0.12 ms and is only printed.
    fn timed(name: &str, unit: &[u8]) -> (Duration, Result<(), DecodeError>) {
        let (took, answer) = fastest(unit, check_hevc);
        let (with_search, _) = fastest(unit, |unit| {
            std::hint::black_box(hevc_idr(unit));
            check_hevc(unit)
        });
        println!(
            "{name}, {} bytes: {answer:?} in {:.3} ms, {:.3} ms with the search for the first picture",
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
        // one of nothing but the smallest SEIs, which decode searches to the
        // end for a picture.
        for (name, piece) in [("A P frame", slice(TRAIL)), ("SEIs", vec![0, 0, 1, SEI[0]])] {
            let mut unit = Vec::new();
            fill_to_the_limit(&mut unit, &piece);
            let (took, answer) = timed(name, &unit);
            assert!(answer.is_ok());
            assert!(held(took), "{took:?}");
        }
        // The most a unit that passes can cost: 16 of the heaviest SPSs and
        // the rest in the smallest SEIs. Reading the 16 takes about 0.9 ms,
        // past the quarter millisecond, so it is only printed.
        let mut worst = heaviest_sps().repeat(MOST_SPS);
        fill_to_the_limit(&mut worst, &[0, 0, 1, SEI[0]]);
        let (_, answer) = timed("16 of the heaviest SPS, then SEIs", &worst);
        assert!(answer.is_ok());
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
}
