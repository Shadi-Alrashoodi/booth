// The Windows keys while controlling fullscreen. Windows acts on some keys
// before any window sees them: the Win keys, Alt+Tab and the like. While the
// viewer is fullscreen, focused and controlling, a low-level keyboard hook
// takes those from Windows and sends them to the viewer's feed itself; every
// other key passes the hook untouched and reaches the feed through Raw Input
// as always. This is the hook's decision, with no Windows in it.
//
// Whether Raw Input still reports a key the hook swallowed is not
// documented, so the Raw Input side makes the same decision on its own copy
// of the state and leaves the hook's keys alone either way. If Raw Input
// reports them, both copies see the same keys and agree. If it does not,
// the Raw Input copy misses only keys the hook took, of which only the Win
// keys are modifiers, and with fewer modifiers held it takes less, never
// something the hook passed.

use crate::bindings::Bindings;
use crate::key::{Chord, Key, Modifiers};
use crate::tracker::{INJECTED, RI_KEY_E0, RawKey};

// Windows takes a low-level hook away without a word when the thread that
// set it answers too late (LowLevelHooksTimeout). While it is in, both sides
// see every key the hook passes on, the hook first or about the same moment,
// so Raw Input running this many of those ahead of the hook means the hook
// is gone. The slack is for keys already on their way when the hook went
// in. Only physical keys the hook passes count, on both sides: whether Raw
// Input also reports the ones it swallows is not documented, and counting
// those would put the hook ahead by every Win chord pressed. Injected keys
// are some other program's, and Raw Input also reports what is not a key,
// such as the made-up Shift around the arrows and the second half of Pause,
// which the hook may not.
const AHEAD: u64 = 4;

const VK_PAUSE: u32 = 0x13;
const VK_NUMLOCK: u32 = 0x90;

pub(crate) struct Grab {
    bindings: Bindings,
    hook: Side,
    raw: Side,
}

#[derive(Clone, Default)]
struct Side {
    // Keys down as this side saw them, and whether the hook took each press.
    down: Vec<(Key, bool)>,
    passed: u64,
}

impl Grab {
    // `held` is what physical keyboards hold as the hook goes in. Those
    // presses reached Windows, so their releases must too.
    pub(crate) fn new(bindings: Bindings, held: impl Iterator<Item = Key>) -> Grab {
        let side = Side {
            down: held.map(|key| (key, false)).collect(),
            passed: 0,
        };
        Grab {
            bindings,
            hook: side.clone(),
            raw: side,
        }
    }

    // From the hook: true takes the event from Windows, and the hook sends
    // it to the feed. Injected keys are left to Windows and change nothing
    // here: on this side they are some other program's, and on a PC that is
    // itself controlled they are its controller's. `on` is the switch as it
    // stands: once it is off, no new press is taken, so the hook is as good
    // as gone before its thread gets round to removing it.
    pub(crate) fn hook(&mut self, key: Option<Key>, down: bool, injected: bool, on: bool) -> bool {
        match key {
            Some(key) if !injected => self.hook.event(key, down, on, &self.bindings),
            _ => false,
        }
    }

    // From Raw Input: true when the event is the hook's, so Raw Input
    // leaves it alone.
    pub(crate) fn raw(&mut self, raw: RawKey, on: bool) -> bool {
        match raw.key() {
            Some(key) if raw.device != INJECTED => {
                self.raw.event(key, raw.down(), on, &self.bindings)
            }
            _ => false,
        }
    }

    pub(crate) fn lost(&self) -> bool {
        self.raw.passed > self.hook.passed + AHEAD
    }
}

impl Side {
    // A physical key; true when the hook takes it.
    fn event(&mut self, key: Key, down: bool, on: bool, bindings: &Bindings) -> bool {
        let take = self.decide(key, down, on, bindings);
        if !take {
            self.passed += 1;
        }
        take
    }

    fn decide(&mut self, key: Key, down: bool, on: bool, bindings: &Bindings) -> bool {
        let at = self.down.iter().position(|(held, _)| *held == key);
        if !down {
            // A release goes where its press went. One whose press this side
            // never saw was pressed before the hook, and Windows saw it.
            return at.is_some_and(|at| self.down.remove(at).1);
        }
        // A repeat goes where its press went too, whatever is held now:
        // Windows must never see half of a key.
        if let Some(at) = at {
            return self.down[at].1;
        }
        let held = self
            .down
            .iter()
            .fold(Modifiers::NONE, |all, (held, _)| all.with(held.modifier()));
        // Booth's own keys stay with Booth. Settings refuses these chords,
        // but a hand-written settings file can hold one.
        let take = on && windows_acts(key, held) && !bindings.any_fires(Chord::new(held, key));
        self.down.push((key, take));
        take
    }
}

// Windows' own shortcuts: the Win keys and everything pressed with one, the
// window switchers, and Start. Then the two the viewer's own window would
// act on: Alt+F4 would close it and Alt+Space open its window menu. Not
// Ctrl+Alt+Delete, which never reaches a hook, and not Print Screen, which
// takes a picture of this PC and leaves the focus where it was. Win+L is
// taken like any Win chord; Windows is reported to lock this PC anyway,
// whatever a hook answers, and the sharer's injector drops it.
//
// Ctrl, Shift and Alt are never taken, Win held or not. Windows does
// nothing with them alone, and the tracker has to see them for the release
// key, which it would not if Raw Input leaves out what the hook swallows.
pub(crate) fn windows_acts(key: Key, held: Modifiers) -> bool {
    if key.modifier() == Modifiers::WIN {
        return true;
    }
    if key.is_modifier() {
        return false;
    }
    let with = |modifier| held.contains(modifier);
    with(Modifiers::WIN)
        || (key == Key::TAB && with(Modifiers::ALT))
        || (key == Key::ESC && (with(Modifiers::ALT) || with(Modifiers::CTRL)))
        || ((key == Key::F4 || key == Key::SPACE) && with(Modifiers::ALT))
}

// A key as the hook reports it: the scan code without its prefix and a flag
// for E0, named the way Raw Input names it. The hook reports Pause as 45
// without the flag and Num Lock as 45 with it, the other way round from
// what the keyboard sends.
pub(crate) fn from_hook(vk: u32, scan: u32, extended: bool) -> Option<Key> {
    match vk {
        VK_PAUSE => return Some(Key::PAUSE),
        VK_NUMLOCK => return Some(Key::from_scan(0, 0x45)),
        _ => {}
    }
    let raw = RawKey {
        make_code: u16::try_from(scan).ok()?,
        flags: if extended { RI_KEY_E0 } else { 0 },
        vkey: u16::try_from(vk).ok()?,
        device: INJECTED,
    };
    raw.key()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::bindings::Action;

    const KEYBOARD: usize = 0x0002_0017;
    const E: Key = Key::from_scan(0, 0x12);
    const L: Key = Key::from_scan(0, 0x26);
    const W: Key = Key::from_scan(0, 0x11);

    fn grab() -> Grab {
        Grab::new(Bindings::default(), std::iter::empty())
    }

    // Presses and releases in turn, each with whether the hook took it.
    fn run(grab: &mut Grab, steps: &[(Key, bool)]) -> Vec<bool> {
        steps
            .iter()
            .map(|(key, down)| grab.hook(Some(*key), *down, false, ON))
            .collect()
    }

    const DOWN: bool = true;
    const UP: bool = false;
    const ON: bool = true;
    const OFF: bool = false;

    // The viewer left fullscreen before the hook's thread took it out: no
    // new press is taken, and a key taken before still ends with the
    // sharer, so Windows never sees half of it.
    #[test]
    fn once_the_switch_is_off_nothing_new_is_taken() {
        let mut grab = grab();
        assert!(grab.hook(Some(Key::LEFT_WIN), DOWN, false, ON));
        assert!(grab.hook(Some(E), DOWN, false, ON));
        assert!(
            grab.hook(Some(E), DOWN, false, OFF),
            "a repeat of a taken key"
        );
        assert!(!grab.hook(Some(L), DOWN, false, OFF), "Win held, but new");
        assert!(grab.hook(Some(E), UP, false, OFF));
        assert!(grab.hook(Some(Key::LEFT_WIN), UP, false, OFF));
        assert!(!grab.hook(Some(Key::LEFT_WIN), DOWN, false, OFF));
    }

    #[test]
    fn the_win_keys_and_everything_with_them_go_to_the_sharer() {
        let mut grab = grab();
        assert_eq!(
            run(
                &mut grab,
                &[
                    (Key::LEFT_WIN, DOWN),
                    (Key::LEFT_WIN, UP),
                    (Key::RIGHT_WIN, DOWN),
                    (E, DOWN),
                    (E, DOWN),
                    (E, UP),
                    (Key::RIGHT_WIN, UP),
                ]
            ),
            [true; 7]
        );
        // A modifier pressed while Win is held reaches both PCs, so the
        // tracker here still sees it, and a key pressed with both goes with
        // Win.
        assert_eq!(
            run(
                &mut grab,
                &[
                    (Key::LEFT_WIN, DOWN),
                    (Key::LEFT_SHIFT, DOWN),
                    (Key::RIGHT_CTRL, DOWN),
                    (Key::LEFT_ALT, DOWN),
                    (E, DOWN),
                    (Key::LEFT_WIN, UP),
                    (Key::LEFT_SHIFT, UP),
                    (E, UP),
                ]
            ),
            [true, false, false, false, true, true, false, true]
        );
        assert_eq!(run(&mut grab, &[(L, DOWN), (L, UP)]), [false; 2]);
    }

    #[test]
    fn switchers_and_start_go_to_the_sharer() {
        let mut grab = grab();
        // Alt reaches both PCs; Tab only the sharer, its release too, even
        // after Alt went up first.
        assert_eq!(
            run(
                &mut grab,
                &[
                    (Key::LEFT_ALT, DOWN),
                    (Key::TAB, DOWN),
                    (Key::TAB, DOWN),
                    (Key::LEFT_ALT, UP),
                    (Key::TAB, UP),
                ]
            ),
            [false, true, true, false, true]
        );
        for chord in [
            [Key::LEFT_SHIFT, Key::LEFT_ALT, Key::TAB],
            [Key::LEFT_CTRL, Key::LEFT_ALT, Key::TAB],
            [Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::ESC],
        ] {
            let mut grab = self::grab();
            let steps: Vec<(Key, bool)> = chord.iter().map(|key| (*key, DOWN)).collect();
            assert_eq!(run(&mut grab, &steps), [false, false, true], "{chord:?}");
        }
        for (modifier, key) in [
            (Key::LEFT_ALT, Key::ESC),
            (Key::RIGHT_CTRL, Key::ESC),
            (Key::RIGHT_ALT, Key::F4),
            (Key::LEFT_ALT, Key::SPACE),
        ] {
            let mut grab = self::grab();
            assert_eq!(
                run(&mut grab, &[(modifier, DOWN), (key, DOWN), (key, UP)]),
                [false, true, true],
                "{modifier} {key}"
            );
        }
    }

    #[test]
    fn keys_windows_leaves_to_the_window_pass_untouched() {
        for steps in [
            vec![Key::TAB],
            vec![Key::ESC],
            vec![Key::LEFT_SHIFT, Key::ESC],
            vec![Key::LEFT_CTRL, Key::TAB],
            vec![Key::F4],
            vec![Key::LEFT_CTRL, Key::F4],
            vec![Key::SPACE],
            vec![Key::LEFT_CTRL, Key::LEFT_ALT, Key::DELETE],
            vec![Key::LEFT_ALT, Key::from_scan(0, 0x1C)],
            vec![Key::from_scan(0xE0, 0x37)],
            vec![Key::LEFT_SHIFT, W],
        ] {
            let mut grab = grab();
            let presses: Vec<(Key, bool)> = steps.iter().map(|key| (*key, DOWN)).collect();
            assert!(
                run(&mut grab, &presses).iter().all(|took| !took),
                "{steps:?}"
            );
        }
    }

    // A key already down reached Windows, so its repeats and its release
    // do too, whatever is pressed around it.
    #[test]
    fn a_key_goes_one_way_from_press_to_release() {
        let mut grab = grab();
        assert_eq!(
            run(
                &mut grab,
                &[
                    (Key::TAB, DOWN),
                    (Key::LEFT_ALT, DOWN),
                    (Key::TAB, DOWN),
                    (Key::TAB, UP),
                ]
            ),
            [false; 4]
        );
        // Held from before the hook went in.
        let mut grab = Grab::new(
            Bindings::default(),
            [Key::LEFT_WIN, Key::LEFT_ALT].into_iter(),
        );
        assert!(!grab.hook(Some(Key::LEFT_WIN), UP, false, ON));
        assert!(grab.hook(Some(Key::TAB), DOWN, false, ON), "Alt still held");
        assert!(!grab.hook(Some(Key::LEFT_ALT), UP, false, ON));
        assert!(!grab.hook(Some(E), UP, false, ON), "never seen going down");
    }

    #[test]
    fn injected_keys_are_left_to_windows_and_change_nothing() {
        let mut grab = grab();
        assert!(!grab.hook(Some(Key::LEFT_WIN), DOWN, true, ON));
        assert!(
            !grab.hook(Some(E), DOWN, false, ON),
            "an injected Win holds nothing"
        );
        assert!(!grab.hook(Some(Key::LEFT_ALT), DOWN, true, ON));
        assert!(!grab.hook(Some(Key::TAB), DOWN, false, ON));
        assert!(!grab.hook(None, DOWN, false, ON), "not a key");
        assert!(!grab.raw(RawKey::press(Key::LEFT_WIN, INJECTED), ON));
    }

    // A chord of Booth's own, written into settings.txt by hand, stays
    // with Booth.
    #[test]
    fn booths_own_keys_stay_even_where_windows_would_act() {
        let mut bindings = Bindings::default();
        bindings.set(Action::StatsPanel, Chord::new(Modifiers::ALT, Key::SPACE));
        let mut grab = Grab::new(bindings, std::iter::empty());
        assert_eq!(
            run(&mut grab, &[(Key::LEFT_ALT, DOWN), (Key::SPACE, DOWN)]),
            [false, false]
        );
    }

    #[test]
    fn a_hook_that_hears_nothing_while_raw_input_does_is_gone() {
        let mut grab = grab();
        for _ in 0..3 {
            grab.hook(Some(L), DOWN, false, ON);
            grab.raw(RawKey::press(L, KEYBOARD), ON);
        }
        assert!(!grab.lost());
        // With Num Lock on, every arrow comes wrapped in a made-up Shift that
        // the hook may not report. It is no key, and does not count.
        let made_up_shift = RawKey {
            make_code: 0x2A,
            flags: RI_KEY_E0,
            vkey: 0x10,
            device: KEYBOARD,
        };
        for _ in 0..10 {
            grab.raw(made_up_shift, ON);
        }
        assert!(!grab.lost());
        // Raw Input a little ahead: keys on their way as the hook went in.
        for _ in 0..AHEAD {
            grab.raw(RawKey::release(L, KEYBOARD), ON);
        }
        assert!(!grab.lost());
        grab.raw(RawKey::press(L, KEYBOARD), ON);
        assert!(grab.lost());
    }

    // However many keys the hook swallowed before, and whether or not Raw
    // Input reported them, a hook that is gone shows within a few keys.
    #[test]
    fn keys_the_hook_swallowed_do_not_hide_that_it_is_gone() {
        for reported in [false, true] {
            let mut grab = grab();
            for _ in 0..50 {
                for (key, down) in [
                    (Key::LEFT_WIN, DOWN),
                    (E, DOWN),
                    (E, UP),
                    (Key::LEFT_WIN, UP),
                ] {
                    assert!(grab.hook(Some(key), down, false, ON));
                    if reported {
                        let raw = if down {
                            RawKey::press(key, KEYBOARD)
                        } else {
                            RawKey::release(key, KEYBOARD)
                        };
                        assert!(grab.raw(raw, ON));
                    }
                }
            }
            // Injected keys count on neither side.
            grab.raw(RawKey::press(L, INJECTED), ON);
            grab.hook(Some(L), DOWN, true, ON);
            // The hook is gone, and Raw Input alone sees what follows.
            for _ in 0..=AHEAD {
                assert!(!grab.lost(), "reported {reported}");
                grab.raw(RawKey::press(L, KEYBOARD), ON);
            }
            assert!(grab.lost(), "reported {reported}");
        }
    }

    #[test]
    fn hook_keys_are_named_the_way_raw_input_names_them() {
        let cases = [
            ((0x13, 0x45, false), Some(Key::PAUSE)),
            ((0x90, 0x45, true), Some(Key::from_scan(0, 0x45))),
            ((0x5B, 0x5B, true), Some(Key::LEFT_WIN)),
            ((0xA3, 0x1D, true), Some(Key::RIGHT_CTRL)),
            ((0xA2, 0x1D, false), Some(Key::LEFT_CTRL)),
            ((0x09, 0x0F, false), Some(Key::TAB)),
            ((0x23, 0x4F, true), Some(Key::END)),
            // The made-up Shift around the arrows under Num Lock.
            ((0xA0, 0x2A, true), None),
            // A key sent by its virtual key alone has no scan code.
            ((0x41, 0, false), None),
            ((0x41, 0x1_0000, false), None),
        ];
        for ((vk, scan, extended), key) in cases {
            assert_eq!(from_hook(vk, scan, extended), key, "{vk:#x} {scan:#x}");
        }
    }

    fn any_key() -> impl Strategy<Value = Key> {
        prop::sample::select(vec![
            Key::LEFT_WIN,
            Key::RIGHT_WIN,
            Key::LEFT_ALT,
            Key::RIGHT_ALT,
            Key::LEFT_CTRL,
            Key::LEFT_SHIFT,
            Key::TAB,
            Key::ESC,
            Key::F4,
            Key::SPACE,
            Key::END,
            E,
            L,
        ])
    }

    proptest! {
        // Whatever physical keys come, the Raw Input side never keeps a key
        // the hook passed from the feed, whether Raw Input reports the keys
        // the hook swallowed or not. If it does, the two agree on each one.
        #[test]
        fn raw_input_and_the_hook_agree(
            steps in prop::collection::vec((any_key(), any::<bool>()), 0..200),
        ) {
            let mut sees_all = grab();
            let mut sees_passed = grab();
            for (key, down) in steps {
                let took = sees_all.hook(Some(key), down, false, ON);
                let raw = if down {
                    RawKey::press(key, KEYBOARD)
                } else {
                    RawKey::release(key, KEYBOARD)
                };
                prop_assert_eq!(sees_all.raw(raw, ON), took, "{} {}", key, down);
                if !sees_passed.hook(Some(key), down, false, ON) {
                    prop_assert!(!sees_passed.raw(raw, ON), "{} {}", key, down);
                }
            }
        }
    }
}
