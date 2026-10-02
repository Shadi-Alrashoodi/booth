// Booth's own picture of NVENC's reference memory, which the driver does not
// expose: which recent frames the next frame could still predict from. It is
// what decides whether a lost frame can be invalidated or needs an IDR.
//
// The model is a sliding window over the last `capacity` frames encoded since
// the last IDR, which is how H.264 keeps references when every frame is one
// (no B frames, no non-reference P frames). Invalidated frames keep their
// place in the window until they slide out, the cautious reading: if NVENC
// frees their slots early it can only reach further back than this model
// assumes, never less far.

use std::collections::VecDeque;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Invalidate these frames, oldest first; a valid frame older than the
    /// lost one is still in memory to predict from.
    Invalidate(Vec<u64>),
    /// Nothing to do: an IDR after the lost frame already cut it off, or
    /// every frame from it on is already invalidated.
    Covered,
    /// The lost frame is gone from memory, or is one this encoder never
    /// made: only an IDR recovers.
    Idr,
}

struct Slot {
    index: u64,
    valid: bool,
}

pub(crate) struct References {
    capacity: usize,
    window: VecDeque<Slot>,
    last_idr: Option<u64>,
}

impl References {
    pub(crate) fn new(capacity: usize) -> References {
        References {
            capacity: capacity.max(1),
            window: VecDeque::with_capacity(capacity),
            last_idr: None,
        }
    }

    pub(crate) fn encoded(&mut self, index: u64, idr: bool) {
        if idr {
            self.window.clear();
            self.last_idr = Some(index);
        }
        self.window.push_back(Slot { index, valid: true });
        if self.window.len() > self.capacity {
            self.window.pop_front();
        }
    }

    pub(crate) fn plan(&self, lost: u64) -> Plan {
        let (Some(last_idr), Some(last)) = (self.last_idr, self.window.back()) else {
            return Plan::Idr;
        };
        if lost > last.index {
            return Plan::Idr;
        }
        if lost < last_idr {
            return Plan::Covered;
        }
        if !self.window.iter().any(|s| s.index < lost && s.valid) {
            return Plan::Idr;
        }
        let frames: Vec<u64> = self
            .window
            .iter()
            .filter(|s| s.index >= lost && s.valid)
            .map(|s| s.index)
            .collect();
        if frames.is_empty() {
            Plan::Covered
        } else {
            Plan::Invalidate(frames)
        }
    }

    pub(crate) fn invalidated(&mut self, frames: &[u64]) {
        for slot in &mut self.window {
            if frames.contains(&slot.index) {
                slot.valid = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(capacity: usize, frames: std::ops::RangeInclusive<u64>) -> References {
        let mut refs = References::new(capacity);
        for i in frames {
            refs.encoded(i, i == 0);
        }
        refs
    }

    #[test]
    fn recent_loss() {
        let refs = run(12, 0..=100);
        assert_eq!(refs.plan(98), Plan::Invalidate(vec![98, 99, 100]));
        assert_eq!(refs.plan(100), Plan::Invalidate(vec![100]));
    }

    #[test]
    fn loss_older_than_window() {
        let refs = run(12, 0..=100);
        // The window is 89..=100; recovering 90 needs 89 to predict from.
        assert_eq!(refs.plan(90), Plan::Invalidate((90..=100).collect()));
        assert_eq!(refs.plan(89), Plan::Idr);
        assert_eq!(refs.plan(50), Plan::Idr);
    }

    #[test]
    fn before_last_idr() {
        let mut refs = run(12, 0..=10);
        refs.encoded(11, true);
        refs.encoded(12, false);
        assert_eq!(refs.plan(5), Plan::Covered);
        assert_eq!(refs.plan(11), Plan::Idr);
        assert_eq!(refs.plan(12), Plan::Invalidate(vec![12]));
    }

    #[test]
    fn not_encoded_or_before_idr() {
        assert_eq!(References::new(12).plan(0), Plan::Idr);
        assert_eq!(run(12, 0..=10).plan(11), Plan::Idr);
    }

    #[test]
    fn repeated_and_overlapping_reports() {
        let mut refs = run(12, 0..=20);
        let Plan::Invalidate(frames) = refs.plan(15) else {
            panic!("15 is recoverable")
        };
        refs.invalidated(&frames);
        assert_eq!(refs.plan(15), Plan::Covered);
        assert_eq!(refs.plan(17), Plan::Covered);
        // A report from before the first one still has 12 to go back to.
        assert_eq!(refs.plan(13), Plan::Invalidate(vec![13, 14]));

        refs.encoded(21, false);
        refs.encoded(22, false);
        // 21 predicts from 14 at the latest; losing it goes back to 14 again.
        assert_eq!(refs.plan(21), Plan::Invalidate(vec![21, 22]));
    }

    #[test]
    fn invalidated_frames_not_used() {
        let mut refs = run(4, 0..=10);
        // Window 7..=10. Losing 8 leaves 7.
        refs.invalidated(&[8, 9, 10]);
        refs.encoded(11, false);
        // Window 8..=11 with 8, 9, 10 invalid: nothing valid before 11.
        assert_eq!(refs.plan(11), Plan::Idr);
    }
}
