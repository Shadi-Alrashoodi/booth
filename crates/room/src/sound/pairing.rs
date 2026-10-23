// How far a watcher's PC sound is from the picture: for each picture shown,
// how much later than it the sound captured at the same moment left this
// PC's speakers. Both carry their capture stamps on the sharer's clock as
// sent, and both were timed on this PC's, so each difference stays on one
// clock and needs no offset, and it holds before the offsets are known:
// (Es - Pv) + (Cv - Cs'), where Cv is the picture's stamp and Pv when it was
// shown, Cs' a sound frame's stamp less the codec's lookahead and Es when
// that frame left for the speakers.

use std::collections::VecDeque;

use crate::talk::WINDOW_US;
use crate::view::AfterPicture;

// A picture goes with the 10 ms sound frame its capture falls in, give or
// take a millisecond of stamp jitter.
const FRAME_US: u64 = 10_000;
const SLACK_US: u64 = 1_000;
// Further apart than this is a clock or a peer that is wrong.
const FARTHEST_US: i64 = 1_000_000;
// The pictures kept for a sound that has not played yet are one handover's
// at most, which Watching holds to as many as this; a guard, no more.
const MOST_WAITING: usize = 512;
// Under this many pairs the numbers say nothing.
pub(super) const FEWEST_PAIRS: usize = 100;

#[derive(Default)]
pub(super) struct Pairing {
    // Pictures whose sound had not played when they came, (Cv, Pv), kept
    // for one more round so the slow ones are not the ones left out.
    waiting: Vec<(u64, u64)>,
    // Over the last WINDOW_US: when each picture was shown, on this PC's
    // clock, and how many ms after it its sound left, negative when first.
    results: VecDeque<(u64, f32)>,
}

impl Pairing {
    // `pictures` are (Cv, Pv) of pictures shown since the last call, and
    // `played` the ear's (Cs', Es) of frames played lately, in any order.
    pub(super) fn pair(&mut self, pictures: &[(u64, u64)], played: &mut [(u64, u64)]) {
        played.sort_unstable_by_key(|&(sent, _)| sent);
        let newest = played.last().map(|&(sent, _)| sent);
        // Last round's waiting ones first, which wait no longer.
        let waited = std::mem::take(&mut self.waiting)
            .into_iter()
            .map(|picture| (picture, false));
        let fresh = pictures.iter().map(|&picture| (picture, true));
        for ((cv, pv), fresh) in waited.chain(fresh) {
            let at = played.partition_point(|&(sent, _)| sent <= cv.saturating_add(SLACK_US));
            let frame = at
                .checked_sub(1)
                .map(|at| played[at])
                .filter(|&(sent, _)| cv < sent.saturating_add(FRAME_US + SLACK_US));
            match frame {
                Some((sent, es)) => {
                    // One difference on each clock.
                    let here = es.wrapping_sub(pv) as i64;
                    let there = cv.wrapping_sub(sent) as i64;
                    let us = here.wrapping_add(there);
                    if (-FARTHEST_US..=FARTHEST_US).contains(&us) {
                        self.results.push_back((pv, us as f32 / 1000.0));
                    }
                }
                // Its sound has not played yet: once more next round.
                None if fresh
                    && newest.is_none_or(|newest| cv >= newest.saturating_add(FRAME_US))
                    && self.waiting.len() < MOST_WAITING =>
                {
                    self.waiting.push((cv, pv));
                }
                // In a gap in the sound, or waited a round already.
                None => {}
            }
        }
    }

    // Results older than WINDOW_US go.
    pub(super) fn prune(&mut self, now_us: u64) {
        while self
            .results
            .front()
            .is_some_and(|&(at, _)| now_us.saturating_sub(at) >= WINDOW_US)
        {
            self.results.pop_front();
        }
    }

    // The spread over the last WINDOW_US, once there are enough pairs.
    pub(super) fn apart(&self, now_us: u64) -> Option<AfterPicture> {
        let mut values: Vec<f32> = self
            .results
            .iter()
            .filter(|&&(at, _)| now_us.saturating_sub(at) < WINDOW_US)
            .map(|&(_, ms)| ms)
            .collect();
        if values.len() < FEWEST_PAIRS {
            return None;
        }
        values.sort_by(f32::total_cmp);
        let at = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
        Some(AfterPicture {
            median_ms: at(50),
            p5_ms: at(5),
            p95_ms: at(95),
            pictures: values.len() as u32,
        })
    }

    pub(super) fn clear(&mut self) {
        self.waiting.clear();
        self.results.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sound frames every 10 ms from stamp `from` on the sharer's clock, each
    // leaving for the speakers `late_us` after its capture on this PC's.
    fn played(from: u64, count: u64, late_us: u64) -> Vec<(u64, u64)> {
        (0..count)
            .map(|i| {
                let sent = from + i * FRAME_US;
                (sent, sent + late_us)
            })
            .collect()
    }

    // Pictures shown 30 ms after their capture, beside sound frames each
    // played 40 to 46 ms after their own: each picture's sound comes after
    // it by the lateness of the frame its capture falls in less those
    // 30 ms, wherever in the frame the capture is.
    #[test]
    fn a_picture_goes_with_the_frame_its_capture_falls_in() {
        let mut pairing = Pairing::default();
        let late = |frame: u64| 40_000 + frame % 7 * 1_000;
        let mut sound: Vec<(u64, u64)> = (0..300)
            .map(|frame| {
                let sent = 1_000_000 + frame * FRAME_US;
                (sent, sent + late(frame))
            })
            .collect();
        let pictures: Vec<(u64, u64)> = (0..150)
            .map(|i| {
                let cv = 1_000_000 + i * 16_667;
                (cv, cv + 30_000)
            })
            .collect();
        pairing.pair(&pictures, &mut sound);
        assert_eq!(pairing.results.len(), 150);
        for (&(cv, _), &(_, ms)) in pictures.iter().zip(&pairing.results) {
            let frame = (cv - 1_000_000) / FRAME_US;
            let after = (late(frame) - 30_000) as f32 / 1000.0;
            assert!((ms - after).abs() < 0.001, "{cv}: {ms} for {after}");
        }
        let apart = pairing.apart(4_000_000).expect("enough pairs");
        assert_eq!(apart.pictures, 150);
        assert!((10.0..=16.0).contains(&apart.median_ms), "{apart:?}");
    }

    // The difference is taken on each clock alone: moving everything on the
    // sharer's clock, or everything on this PC's, changes nothing.
    #[test]
    fn no_clock_offset_goes_into_it() {
        let measure = |sharer: u64, here: u64| {
            let mut pairing = Pairing::default();
            let mut sound: Vec<(u64, u64)> = played(5_000_000, 300, 25_000)
                .into_iter()
                .map(|(sent, es)| (sent + sharer, es + here))
                .collect();
            let pictures: Vec<(u64, u64)> = (0..120)
                .map(|i| {
                    let cv = 5_000_000 + i * 20_000 + 3_000;
                    (cv + sharer, cv + 45_000 + here)
                })
                .collect();
            pairing.pair(&pictures, &mut sound);
            pairing.apart(8_000_000 + here).expect("enough pairs")
        };
        let plain = measure(0, 0);
        assert!((plain.median_ms - -20.0).abs() < 0.001, "{plain:?}");
        for (sharer, here) in [(7_777_777_777, 0), (0, 123_456_789), (99, 3_000_000_000)] {
            assert_eq!(measure(sharer, here), plain, "{sharer} {here}");
        }
    }

    // A picture whose sound has not played yet waits one round and pairs
    // then; one that falls in a gap in the sound, or waited already, is left
    // out. Results older than WINDOW_US go, and with too few pairs the
    // numbers say nothing.
    #[test]
    fn pictures_wait_a_round_and_gaps_are_left_out() {
        let mut pairing = Pairing::default();
        let mut sound = played(0, 50, 30_000);
        // The last frame played covers 490 to 500 ms; a gap from 200 to
        // 300 ms.
        sound.retain(|&(sent, _)| !(200_000..300_000).contains(&sent));
        sound.reverse();
        let late = (520_000, 540_000);
        let gap = (250_000, 280_000);
        let fine = (100_000, 120_000);
        pairing.pair(&[late, gap, fine], &mut sound);
        assert_eq!(pairing.results.len(), 1);
        assert_eq!(pairing.waiting, [late]);
        // The next round has its sound.
        let mut more = played(500_000, 10, 30_000);
        pairing.pair(&[], &mut more);
        assert_eq!(pairing.results.len(), 2);
        assert!(pairing.waiting.is_empty());
        let (_, ms) = pairing.results[1];
        assert!((ms - 10.0).abs() < 0.001, "{ms}");
        // Still no sound for it a round later: it goes.
        pairing.pair(&[(900_000, 900_000)], &mut more);
        pairing.pair(&[], &mut more);
        assert!(pairing.waiting.is_empty());
        assert_eq!(pairing.results.len(), 2);
        assert_eq!(pairing.apart(960_000), None, "two pairs say nothing");
        pairing.prune(120_000 + WINDOW_US - 1);
        assert_eq!(pairing.results.len(), 2);
        pairing.prune(120_000 + WINDOW_US);
        assert_eq!(pairing.results.len(), 1, "the first is 10 s old");
        pairing.clear();
        assert!(pairing.results.is_empty());
    }

    // A sharer whose stamps are nonsense, a second or more off, adds
    // nothing.
    #[test]
    fn a_pair_further_apart_than_a_second_counts_for_nothing() {
        let mut pairing = Pairing::default();
        let mut sound = played(1_000_000, 5, 2_000_000);
        pairing.pair(&[(1_000_000, 1_000_000)], &mut sound);
        assert!(pairing.results.is_empty());
    }

    // The 5th, 50th and 95th percentiles by nearest rank, as the other
    // numbers are.
    #[test]
    fn the_spread_is_by_nearest_rank() {
        let mut pairing = Pairing::default();
        for i in 0..200u64 {
            pairing.results.push_back((1_000, i as f32 - 50.0));
        }
        let apart = pairing.apart(1_000).unwrap();
        assert_eq!(
            apart,
            AfterPicture {
                median_ms: 49.0,
                p5_ms: -41.0,
                p95_ms: 139.0,
                pictures: 200
            }
        );
    }
}
