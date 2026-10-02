// How frames reach the screen, from the swap chain's statistics, and when
// what they say can be believed.

use windows::Win32::Graphics::Dxgi::{
    DXGI_FRAME_PRESENTATION_MODE, DXGI_FRAME_PRESENTATION_MODE_COMPOSED,
    DXGI_FRAME_PRESENTATION_MODE_OVERLAY,
};

use crate::strip::PresentPath;

// Presents made since the statistics last told of a newer one, past which
// they no longer follow the window and say nothing of it. While they follow
// they tell of a present 1 to 5 behind the last one made (RTX 4070 Ti SUPER,
// 240 Hz, 120 fps, 2026-09-29). On that PC, in 11 runs of 14, they also
// stopped after the first present of a new window until it moved or changed
// size. 60 is half a second at 120 fps, and leaves room for the example's
// 1000 fps on a 60 Hz monitor, where one present in about 17 is shown.
const STALLED: u32 = 60;

// Presents asked for after a change while there is no word, so a still
// picture is presented again and Windows can report on a present made
// since: a share whose screen does not change sends no frames. In the runs
// noted at STALLED the word came back on the third present after a resize.
// The share thread asks at least every 250 ms, so this gives up after about
// two seconds when the statistics never come.
const ASKS: u32 = 8;

// What GetFrameStatisticsMedia gave: the id of the newest present Windows
// has put on screen, and how it got there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stats {
    pub present: u32,
    pub mode: DXGI_FRAME_PRESENTATION_MODE,
}

// Present ids are the swap chain's own, as GetLastPresentCount gives them,
// counting from 1. An unwritten DXGI_FRAME_STATISTICS_MEDIA has 0 there,
// which is never newer than anything, so its CompositionMode of 0, which is
// COMPOSED, is never read.
#[derive(Debug, Default)]
pub(crate) struct Reading {
    word: Option<PresentPath>,
    // The last present made before the latest change. Statistics about it
    // or an older one describe buffers, a window or a monitor that are gone.
    before_change: u32,
    // The newest present the statistics have told of, and the last present
    // made when they did.
    reported: u32,
    reported_at: u32,
    // Presents still to ask for since the last change (ASKS).
    asks: u32,
}

impl Reading {
    pub(crate) fn word(&self) -> Option<PresentPath> {
        self.word
    }

    // A resize, fullscreen on or off, another monitor or new display
    // settings: the path may be different now. `last` is the last present
    // made before it.
    pub(crate) fn changed(&mut self, last: u32) {
        self.forget(last);
        self.asks = ASKS;
    }

    // At the start of each present, whether it gets drawn or not, so the
    // asks run out while minimized too.
    pub(crate) fn presenting(&mut self) {
        self.asks = self.asks.saturating_sub(1);
    }

    // Whether a present of the still picture could bring the word back.
    // Only a change starts the asks: statistics that keep failing on their
    // own would otherwise have the picture presented again forever.
    pub(crate) fn wants_present(&self) -> bool {
        self.word.is_none() && self.asks > 0
    }

    fn forget(&mut self, last: u32) {
        self.word = None;
        self.before_change = self.before_change.max(last);
    }

    // After each present, with what GetFrameStatisticsMedia gave, None when
    // it failed. `last` is the present just made.
    pub(crate) fn read(&mut self, stats: Option<Stats>, last: u32) {
        let Some(stats) = stats else {
            // Most often DXGI_ERROR_FRAME_STATISTICS_DISJOINT: on the first
            // call, on the first after a resize (on the PC above), and
            // after anything that broke the run of statistics, a power
            // cycle for one. What came before it may no longer hold.
            self.forget(last);
            return;
        };
        // No present can be shown before it is made. Microsoft's page for
        // DXGI_FRAME_STATISTICS calls PresentCount a running count of images
        // shown since the computer started, which need not match the calls
        // to Present. On the PC above it is this swap chain's own id, but a
        // driver that gives the other count would have every reading look
        // newer than any change, and bring the old word back at once.
        if stats.present > last {
            self.forget(last);
            return;
        }
        if stats.present > self.reported {
            self.reported = stats.present;
            self.reported_at = last;
            if stats.present > self.before_change {
                self.word = path_of(stats.mode);
            }
        }
        if last.saturating_sub(self.reported_at) > STALLED {
            self.word = None;
        }
    }
}

// Each value on its own, after Microsoft's DXGI_FRAME_PRESENTATION_MODE page:
// - OVERLAY: an overlay surface, which the display scans out itself without
//   DWM drawing it into the desktop: "flip". The page does not say what
//   independent flip without an overlay plane reports.
// - COMPOSED: a composition surface, which DWM draws into the desktop it
//   shows, a refresh later at worst: "composed".
// - NONE: "no presentation is specified". On the PC above it came on the
//   call after each DISJOINT, about a present from before, and Chromium's
//   swap chain code notes the same after an interrupted run of statistics.
//   It says nothing of the path.
// - COMPOSITION_FAILURE: a swap chain with hardware content protection ran
//   out of protected memory. This one asks for no protection, so it should
//   never come, and if it does it says a frame could not be composed, not
//   how a shown frame got there.
// - Anything newer means nothing here yet.
// Only the first two name a path; the rest show no word.
fn path_of(mode: DXGI_FRAME_PRESENTATION_MODE) -> Option<PresentPath> {
    if mode == DXGI_FRAME_PRESENTATION_MODE_OVERLAY {
        Some(PresentPath::Flip)
    } else if mode == DXGI_FRAME_PRESENTATION_MODE_COMPOSED {
        Some(PresentPath::Composed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Dxgi::{
        DXGI_FRAME_PRESENTATION_MODE_COMPOSITION_FAILURE, DXGI_FRAME_PRESENTATION_MODE_NONE,
    };

    use super::*;

    const OVERLAY: DXGI_FRAME_PRESENTATION_MODE = DXGI_FRAME_PRESENTATION_MODE_OVERLAY;
    const COMPOSED: DXGI_FRAME_PRESENTATION_MODE = DXGI_FRAME_PRESENTATION_MODE_COMPOSED;
    const NONE: DXGI_FRAME_PRESENTATION_MODE = DXGI_FRAME_PRESENTATION_MODE_NONE;

    fn stats(present: u32, mode: DXGI_FRAME_PRESENTATION_MODE) -> Option<Stats> {
        Some(Stats { present, mode })
    }

    // Presents `from` to `to`, with the statistics telling of the present
    // two before each, as in the runs noted at STALLED while they followed.
    fn follow(reading: &mut Reading, from: u32, to: u32, mode: DXGI_FRAME_PRESENTATION_MODE) {
        for last in from..=to {
            reading.read(stats(last.saturating_sub(2), mode), last);
        }
    }

    #[test]
    fn only_overlay_and_composed_name_a_path() {
        let cases = [
            (OVERLAY, Some(PresentPath::Flip)),
            (COMPOSED, Some(PresentPath::Composed)),
            (NONE, None),
            (DXGI_FRAME_PRESENTATION_MODE_COMPOSITION_FAILURE, None),
            (DXGI_FRAME_PRESENTATION_MODE(7), None),
        ];
        for (mode, path) in cases {
            assert_eq!(path_of(mode), path, "mode {}", mode.0);
        }
    }

    // What the runs noted at STALLED gave for a new window: DISJOINT, then
    // NONE about nothing, then COMPOSED about the first present, made before
    // the DISJOINT.
    #[test]
    fn a_new_window_has_no_word_at_first() {
        let mut reading = Reading::default();
        reading.read(None, 1);
        assert_eq!(reading.word(), None);
        reading.read(stats(0, NONE), 2);
        assert_eq!(reading.word(), None);
        reading.read(stats(1, COMPOSED), 3);
        assert_eq!(reading.word(), None);
        reading.read(stats(2, COMPOSED), 4);
        assert_eq!(reading.word(), Some(PresentPath::Composed));
    }

    // An unwritten struct: present 0, and COMPOSED, which is 0 too.
    #[test]
    fn an_empty_answer_is_not_composed() {
        let mut reading = Reading::default();
        for last in 1..=5 {
            reading.read(stats(0, COMPOSED), last);
            assert_eq!(reading.word(), None);
        }
    }

    #[test]
    fn overlay_from_a_present_after_the_change_is_flip() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 10, OVERLAY);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
    }

    // F11 with the old statistics still coming: the windowed "composed" must
    // not carry over into fullscreen.
    #[test]
    fn after_a_change_the_word_waits_for_a_present_made_since() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 120, COMPOSED);
        assert_eq!(reading.word(), Some(PresentPath::Composed));
        reading.changed(120);
        assert_eq!(reading.word(), None);
        // Presents 121 and 122 are made; the statistics still tell of 119
        // and 120, from before.
        reading.read(stats(119, COMPOSED), 121);
        assert_eq!(reading.word(), None);
        reading.read(stats(120, COMPOSED), 122);
        assert_eq!(reading.word(), None);
        reading.read(stats(121, OVERLAY), 123);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
    }

    #[test]
    fn composed_from_a_present_after_the_change_is_composed() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 60, OVERLAY);
        reading.changed(60);
        reading.read(stats(61, COMPOSED), 62);
        assert_eq!(reading.word(), Some(PresentPath::Composed));
    }

    // What the runs noted at STALLED gave after a resize: DISJOINT, then NONE
    // about a present from before, then the new ones.
    #[test]
    fn a_failed_call_leaves_the_word_unknown() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 120, COMPOSED);
        reading.changed(120);
        reading.read(None, 121);
        assert_eq!(reading.word(), None);
        reading.read(stats(1, NONE), 122);
        assert_eq!(reading.word(), None);
        reading.read(None, 123);
        assert_eq!(reading.word(), None);
        // Made before the last failed call, so it waits for the next.
        reading.read(stats(122, COMPOSED), 124);
        assert_eq!(reading.word(), None);
        reading.read(stats(124, COMPOSED), 125);
        assert_eq!(reading.word(), Some(PresentPath::Composed));
    }

    #[test]
    fn a_failed_call_clears_a_word_it_had() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 30, OVERLAY);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
        reading.read(None, 31);
        assert_eq!(reading.word(), None);
        reading.read(stats(29, OVERLAY), 32);
        assert_eq!(reading.word(), None);
        reading.read(stats(32, OVERLAY), 33);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
    }

    #[test]
    fn none_about_a_fresh_present_clears_the_word() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 30, OVERLAY);
        reading.read(stats(29, NONE), 31);
        assert_eq!(reading.word(), None);
    }

    // As in the runs noted at STALLED, for a window that has not moved yet:
    // one present is reported and then nothing newer, however many are made.
    #[test]
    fn statistics_that_stop_moving_stop_counting() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 20, COMPOSED);
        for last in 21..=20 + STALLED {
            reading.read(stats(18, COMPOSED), last);
            assert_eq!(
                reading.word(),
                Some(PresentPath::Composed),
                "present {last}"
            );
        }
        reading.read(stats(18, COMPOSED), 21 + STALLED);
        assert_eq!(reading.word(), None);
        // They follow again once the window moves.
        reading.read(stats(20 + STALLED, OVERLAY), 22 + STALLED);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
    }

    #[test]
    fn a_change_before_any_present_changes_nothing() {
        let mut reading = Reading::default();
        reading.changed(0);
        reading.read(stats(1, OVERLAY), 2);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
    }

    // A driver whose PresentCount counts images shown since the computer
    // started: every reading is about a present not made yet.
    #[test]
    fn statistics_about_a_present_not_made_yet_name_no_path() {
        let mut reading = Reading::default();
        for last in 1..=30 {
            reading.read(stats(4_000_000 + last - 2, OVERLAY), last);
            assert_eq!(reading.word(), None, "present {last}");
        }
        // One among ordinary readings clears the word they gave.
        follow(&mut reading, 31, 40, OVERLAY);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
        reading.read(stats(4_000_039, OVERLAY), 41);
        assert_eq!(reading.word(), None);
    }

    // F11 on a share whose screen is still: the viewer asks for the picture
    // again until Windows reports on a present made since. DISJOINT and
    // NONE come first, as noted at STALLED.
    #[test]
    fn presents_are_asked_for_until_the_word_is_back() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 30, COMPOSED);
        assert!(!reading.wants_present());
        // The present that finds the change.
        reading.presenting();
        reading.changed(30);
        reading.read(None, 31);
        assert!(reading.wants_present());
        reading.presenting();
        reading.read(stats(30, NONE), 32);
        assert!(reading.wants_present());
        reading.presenting();
        reading.read(stats(32, OVERLAY), 33);
        assert_eq!(reading.word(), Some(PresentPath::Flip));
        assert!(!reading.wants_present());
    }

    #[test]
    fn the_asks_run_out_when_the_statistics_never_come() {
        let mut reading = Reading::default();
        reading.presenting();
        reading.changed(0);
        for last in 1..=ASKS {
            assert!(reading.wants_present(), "present {last}");
            reading.presenting();
            reading.read(None, last);
        }
        assert!(!reading.wants_present());
    }

    // As for a window DWM does not show. Asking here would present the
    // picture again at every wake for as long as the calls fail.
    #[test]
    fn a_failed_call_alone_asks_for_nothing() {
        let mut reading = Reading::default();
        follow(&mut reading, 1, 30, OVERLAY);
        reading.presenting();
        reading.read(None, 31);
        assert_eq!(reading.word(), None);
        assert!(!reading.wants_present());
    }
}
