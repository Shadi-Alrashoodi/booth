use std::fmt;

use crate::session::REJECT_AFTER_MESSAGES;

pub const REPLAY_WINDOW: u64 = 8192;

const WORD_BITS: u64 = u64::BITS as u64;
const WORDS: usize = (REPLAY_WINDOW / WORD_BITS) as usize;

// The ring word that holds the newest counter also holds counters that have not arrived yet, so
// one word of the ring is never usable as history. This is the RFC 6479 trade.
const MAX_AGE: u64 = REPLAY_WINDOW - WORD_BITS;

pub struct ReplayWindow {
    words: [u64; WORDS],
    // One past the greatest counter accepted, zero before the first.
    next: u64,
}

impl ReplayWindow {
    pub fn new() -> ReplayWindow {
        ReplayWindow {
            words: [0; WORDS],
            next: 0,
        }
    }

    pub fn greatest(&self) -> Option<u64> {
        self.next.checked_sub(1)
    }

    pub fn check(&self, counter: u64) -> bool {
        if counter >= REJECT_AFTER_MESSAGES {
            return false;
        }
        let Some(greatest) = self.greatest() else {
            return true;
        };
        if counter > greatest {
            return true;
        }
        if greatest - counter > MAX_AGE {
            return false;
        }
        let (word, bit) = position(counter);
        self.words[word] & bit == 0
    }

    // Call only once the packet has decrypted, or a forged counter could push the window forward.
    pub fn update(&mut self, counter: u64) -> bool {
        if !self.check(counter) {
            return false;
        }
        if counter >= self.next {
            self.advance_to(counter);
        }
        let (word, bit) = position(counter);
        self.words[word] |= bit;
        true
    }

    fn advance_to(&mut self, counter: u64) {
        let new_block = counter / WORD_BITS;
        if let Some(greatest) = self.greatest() {
            let first_stale = greatest / WORD_BITS + 1;
            if new_block >= first_stale {
                if new_block - first_stale >= WORDS as u64 {
                    self.words = [0; WORDS];
                } else {
                    for block in first_stale..=new_block {
                        self.words[ring_slot(block)] = 0;
                    }
                }
            }
        }
        self.next = counter + 1;
    }
}

impl Default for ReplayWindow {
    fn default() -> ReplayWindow {
        ReplayWindow::new()
    }
}

impl fmt::Debug for ReplayWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplayWindow")
            .field("greatest", &self.greatest())
            .finish()
    }
}

fn ring_slot(block: u64) -> usize {
    (block % WORDS as u64) as usize
}

fn position(counter: u64) -> (usize, u64) {
    (ring_slot(counter / WORD_BITS), 1 << (counter % WORD_BITS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn accept(window: &mut ReplayWindow, counter: u64) -> bool {
        let checked = window.check(counter);
        let updated = window.update(counter);
        assert_eq!(checked, updated, "check and update disagree on {counter}");
        updated
    }

    #[test]
    fn in_order() {
        let mut window = ReplayWindow::new();
        assert_eq!(window.greatest(), None);
        for counter in 0..20_000 {
            assert!(accept(&mut window, counter));
        }
        assert_eq!(window.greatest(), Some(19_999));
    }

    #[test]
    fn duplicates_are_refused() {
        let mut window = ReplayWindow::new();
        assert!(accept(&mut window, 0));
        assert!(!accept(&mut window, 0));
        assert!(accept(&mut window, 7));
        assert!(!accept(&mut window, 7));
        assert!(!accept(&mut window, 0));
    }

    #[test]
    fn reordering_inside_the_window() {
        let mut window = ReplayWindow::new();
        assert!(accept(&mut window, 100));
        assert!(accept(&mut window, 50));
        assert!(accept(&mut window, 99));
        assert!(accept(&mut window, 0));
        assert!(accept(&mut window, 101));
        assert!(!accept(&mut window, 50));
        assert!(!accept(&mut window, 99));
        assert!(accept(&mut window, 98));
        assert_eq!(window.greatest(), Some(101));
    }

    #[test]
    fn too_old_is_refused() {
        let mut window = ReplayWindow::new();
        let greatest = 100_000;
        assert!(accept(&mut window, greatest));
        assert!(accept(&mut window, greatest - MAX_AGE));
        assert!(!accept(&mut window, greatest - MAX_AGE - 1));
        assert!(!accept(&mut window, 0));
    }

    #[test]
    fn oldest_usable_counter_at_every_offset_in_a_word() {
        for offset in 0..WORD_BITS {
            let mut window = ReplayWindow::new();
            let greatest = 10 * REPLAY_WINDOW + offset;
            assert!(accept(&mut window, greatest));
            assert!(accept(&mut window, greatest - MAX_AGE));
            assert!(!accept(&mut window, greatest - MAX_AGE - 1));
        }
    }

    #[test]
    fn huge_jumps() {
        let mut window = ReplayWindow::new();
        assert!(accept(&mut window, 1));
        assert!(accept(&mut window, 130));
        // Lands on the same ring slot as 1 after exactly one lap.
        assert!(accept(&mut window, REPLAY_WINDOW + 1));
        assert!(!accept(&mut window, REPLAY_WINDOW + 1));
        assert!(!accept(&mut window, 1));
        assert!(!accept(&mut window, 130));
        assert!(accept(&mut window, 65));

        assert!(accept(&mut window, 1 << 40));
        assert!(!accept(&mut window, REPLAY_WINDOW + 1));
        assert!(accept(&mut window, (1 << 40) - 1));
        assert!(accept(&mut window, (1 << 40) - MAX_AGE));
        assert!(!accept(&mut window, (1 << 40) - 1));
    }

    #[test]
    fn counters_near_the_limit() {
        let mut window = ReplayWindow::new();
        assert!(accept(&mut window, REJECT_AFTER_MESSAGES - 2));
        assert!(accept(&mut window, REJECT_AFTER_MESSAGES - 1));
        assert!(!accept(&mut window, REJECT_AFTER_MESSAGES));
        assert!(!accept(&mut window, u64::MAX - 1));
        assert!(!accept(&mut window, u64::MAX));
        assert!(!accept(&mut window, REJECT_AFTER_MESSAGES - 1));
        assert!(accept(&mut window, REJECT_AFTER_MESSAGES - 3));
        assert_eq!(window.greatest(), Some(REJECT_AFTER_MESSAGES - 1));
    }

    #[test]
    fn check_alone_changes_nothing() {
        let mut window = ReplayWindow::new();
        assert!(window.check(5));
        assert!(window.check(5));
        assert_eq!(window.greatest(), None);
        assert!(window.update(5));
        assert!(!window.check(5));
    }

    // Every counter ever accepted, and the rule for how far back we look, with no bit tricks.
    #[derive(Default)]
    struct Model {
        seen: HashSet<u64>,
        greatest: Option<u64>,
    }

    impl Model {
        fn would_accept(&self, counter: u64) -> bool {
            counter < REJECT_AFTER_MESSAGES
                && !self.seen.contains(&counter)
                && self
                    .greatest
                    .is_none_or(|greatest| counter > greatest || greatest - counter <= MAX_AGE)
        }

        fn accept(&mut self, counter: u64) -> bool {
            let fresh = self.would_accept(counter);
            if fresh {
                self.seen.insert(counter);
                self.greatest = Some(self.greatest.map_or(counter, |g| g.max(counter)));
            }
            fresh
        }
    }

    fn start() -> impl Strategy<Value = u64> {
        prop_oneof![0u64..1_000_000, (REJECT_AFTER_MESSAGES - 20_000)..=u64::MAX,]
    }

    fn step() -> impl Strategy<Value = i64> {
        prop_oneof![
            1 => Just(0i64),
            8 => -200i64..200,
            2 => -9_000i64..9_000,
            1 => -40_000i64..40_000,
        ]
    }

    proptest! {
        #[test]
        fn matches_the_model(
            start in start(),
            steps in proptest::collection::vec(step(), 1..400),
        ) {
            let mut window = ReplayWindow::new();
            let mut model = Model::default();
            let mut counter = start;
            for step in steps {
                counter = counter.saturating_add_signed(step);
                prop_assert_eq!(window.check(counter), model.would_accept(counter), "check {}", counter);
                prop_assert_eq!(window.update(counter), model.accept(counter), "update {}", counter);
                prop_assert_eq!(window.greatest(), model.greatest);
            }
        }
    }
}
