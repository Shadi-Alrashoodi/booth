// The hotkeys hear every key whatever has the focus, and egui takes Space or
// Enter with any keys held, and a repeat of a key it never saw go down, as a
// press of the focused control. So the panel never gets a press that sets off
// a hotkey, unless the hotkey is on a key alone or with Shift and a text field
// has the focus, nor the repeats of a key already down when it reached the
// panel.
// No Windows here; the app's win.rs feeds it.

use crate::bindings::Bindings;
use crate::key::{Chord, Key, Modifiers};

#[derive(Default)]
pub struct PanelKeys {
    // None while the hotkey thread does not run, and then no key is theirs.
    bindings: Option<Bindings>,
    // Settings waits for a new key, and every key pressed is the hotkeys'.
    capture: bool,
    // A text field has the keyboard focus, as of the last pass.
    typing: bool,
    // Pressed while the panel had the focus and given to it, its input
    // method included, until let go; their repeats go through.
    down: Vec<Key>,
}

impl PanelKeys {
    pub fn set_bindings(&mut self, bindings: Option<Bindings>) {
        self.bindings = bindings;
    }

    // The Space or Enter that pressed Change is still down as the wait
    // starts, and its repeats would press Change again.
    pub fn set_capture(&mut self, on: bool) {
        if on && !self.capture {
            self.down.clear();
        }
        self.capture = on;
    }

    pub fn set_typing(&mut self, on: bool) {
        self.typing = on;
    }

    // egui forgets the keys it holds when the panel loses the focus, so their
    // repeats after that would be new presses to it.
    pub fn set_focused(&mut self, focused: bool) {
        if !focused {
            self.down.clear();
        }
    }

    // `readings`: the modifiers held, each way the hotkeys may have heard
    // them.
    pub fn press(&mut self, key: Key, readings: &[Modifiers]) -> bool {
        // Ctrl, Shift, Alt and Win press nothing in the panel alone, and winit
        // reads them from Windows' key state, not from their messages.
        if key.is_modifier() {
            return false;
        }
        // A key pressed anew was let go first, maybe where the panel could
        // not see.
        self.down.retain(|down| *down != key);
        let keep = self.capture || self.hotkeys_take(key, readings);
        if !keep {
            self.down.push(key);
        }
        keep
    }

    // Windows says the key was already down: a repeat, or the first message
    // for a key pressed while another window had the focus.
    pub fn repeat(&self, key: Key) -> bool {
        !key.is_modifier() && !self.down.contains(&key)
    }

    // Always the panel's: egui ignores the release of a key it does not hold.
    pub fn release(&mut self, key: Key) {
        self.down.retain(|down| *down != key);
    }

    fn hotkeys_take(&self, key: Key, readings: &[Modifiers]) -> bool {
        let Some(bindings) = &self.bindings else {
            return false;
        };
        let mut fired = bindings
            .iter()
            .filter(|(action, bound)| {
                readings
                    .iter()
                    .any(|held| action.fires(*bound, Chord::new(*held, key)))
            })
            .map(|(_, bound)| bound);
        if self.typing {
            fired.any(command)
        } else {
            fired.next().is_some()
        }
    }
}

// Bound with Ctrl, Alt or Win a chord is a command; bound alone or with Shift
// its key also types, and a text field with the focus gets it as in any other
// program.
fn command(chord: Chord) -> bool {
    [Modifiers::CTRL, Modifiers::ALT, Modifiers::WIN]
        .into_iter()
        .any(|modifier| chord.modifiers.contains(modifier))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::bindings::Action;

    const ENTER: Key = Key::from_scan(0, 0x1C);
    const V: Key = Key::from_scan(0, 0x2F);
    const F1: Key = Key::from_scan(0, 0x3B);
    const CS: Modifiers = Modifiers::CTRL.with(Modifiers::SHIFT);
    const NONE: [Modifiers; 2] = [Modifiers::NONE; 2];
    const CTRL: [Modifiers; 2] = [Modifiers::CTRL; 2];
    const SHIFT: [Modifiers; 2] = [Modifiers::SHIFT; 2];
    const CTRL_SHIFT: [Modifiers; 2] = [CS; 2];

    fn with_hotkeys() -> PanelKeys {
        let mut keys = PanelKeys::default();
        keys.set_bindings(Some(Bindings::default()));
        keys
    }

    #[test]
    fn the_show_key_is_the_hotkeys_until_let_go() {
        let mut keys = with_hotkeys();
        assert!(keys.press(Key::SPACE, &CTRL_SHIFT));
        assert!(keys.repeat(Key::SPACE));
        keys.release(Key::SPACE);
        assert!(!keys.press(Key::SPACE, &NONE));
        assert!(!keys.repeat(Key::SPACE));
        // Pressed again with no release seen: the new press decides.
        assert!(keys.press(Key::SPACE, &CTRL_SHIFT));
        assert!(keys.repeat(Key::SPACE));
        assert!(!keys.press(Key::SPACE, &NONE));
        assert!(!keys.repeat(Key::SPACE));
    }

    #[test]
    fn every_default_hotkey_is_kept_and_nothing_else() {
        let mut keys = with_hotkeys();
        for (action, chord) in Bindings::default().iter() {
            if chord.key.is_modifier() {
                continue;
            }
            assert!(keys.press(chord.key, &[chord.modifiers; 2]), "{action:?}");
            keys.release(chord.key);
        }
        assert!(!keys.press(Key::RIGHT_CTRL, &NONE), "push to talk");
        keys.set_capture(true);
        for modifier in [
            Key::LEFT_CTRL,
            Key::RIGHT_CTRL,
            Key::LEFT_SHIFT,
            Key::LEFT_ALT,
            Key::LEFT_WIN,
        ] {
            assert!(!keys.press(modifier, &NONE), "{modifier} while waiting");
            assert!(!keys.repeat(modifier), "{modifier} while waiting");
        }
        keys.set_capture(false);
        for (key, readings) in [
            (Key::SPACE, CTRL),
            (Key::SPACE, SHIFT),
            (ENTER, NONE),
            (Key::M, NONE),
        ] {
            assert!(!keys.press(key, &readings), "{key} {readings:?}");
            keys.release(key);
        }
        let all_three = [CS.with(Modifiers::ALT); 2];
        assert!(!keys.press(Key::M, &all_three), "mute is Ctrl+Shift+M only");
        keys.release(Key::M);
        assert!(keys.press(Key::END, &all_three), "the panic key takes more");
    }

    #[test]
    fn either_reading_of_the_modifiers_counts() {
        let mut keys = with_hotkeys();
        for readings in [[Modifiers::NONE, CS], [CS, Modifiers::NONE]] {
            assert!(keys.press(Key::SPACE, &readings), "{readings:?}");
            keys.release(Key::SPACE);
        }
        assert!(!keys.press(Key::SPACE, &NONE));
    }

    #[test]
    fn a_key_held_from_before_the_focus_waits_to_be_let_go() {
        for bindings in [Some(Bindings::default()), None] {
            for key in [Key::SPACE, ENTER, Key::M] {
                let mut keys = PanelKeys::default();
                keys.set_bindings(bindings);
                assert!(keys.repeat(key), "{key}");
                assert!(keys.repeat(key), "{key}, still held");
                keys.release(key);
                assert!(!keys.press(key, &NONE), "{key}");
                assert!(!keys.repeat(key), "{key}");
            }
        }
    }

    #[test]
    fn losing_the_focus_forgets_what_the_panel_saw() {
        let mut keys = with_hotkeys();
        assert!(!keys.press(Key::SPACE, &NONE));
        assert!(!keys.repeat(Key::SPACE));
        keys.set_focused(true);
        keys.set_typing(true);
        keys.set_bindings(Some(Bindings::default()));
        keys.set_capture(false);
        assert!(!keys.repeat(Key::SPACE), "what every pass sets");
        keys.set_focused(false);
        keys.set_focused(true);
        assert!(keys.repeat(Key::SPACE));
    }

    #[test]
    fn a_text_field_gets_keys_bound_alone_or_with_shift() {
        let mut bindings = Bindings::default();
        bindings.set(Action::PushToTalk, Chord::new(Modifiers::NONE, V));
        bindings.set(Action::Mute, Chord::new(Modifiers::SHIFT, F1));
        let mut keys = PanelKeys::default();
        keys.set_bindings(Some(bindings));
        keys.set_typing(true);
        assert!(!keys.press(V, &NONE));
        assert!(!keys.repeat(V));
        keys.release(V);
        assert!(!keys.press(V, &CTRL), "a paste");
        keys.release(V);
        assert!(!keys.press(F1, &SHIFT));
        keys.release(F1);
        assert!(keys.press(Key::D, &CTRL_SHIFT));
        assert!(keys.press(Key::SPACE, &CTRL_SHIFT));
        keys.set_typing(false);
        assert!(keys.press(V, &NONE));
        assert!(keys.press(V, &CTRL));
    }

    #[test]
    fn the_wait_takes_every_key_and_the_one_that_pressed_change() {
        let mut keys = with_hotkeys();
        assert!(!keys.press(ENTER, &NONE), "Enter pressed Change");
        keys.set_capture(true);
        assert!(keys.repeat(ENTER));
        assert!(keys.press(Key::ESC, &NONE));
        assert!(keys.press(Key::M, &CTRL_SHIFT));
        assert!(!keys.repeat(Key::LEFT_SHIFT));
        keys.set_capture(false);
        assert!(keys.repeat(ENTER));
        assert!(keys.repeat(Key::M));
        keys.release(ENTER);
        assert!(!keys.press(ENTER, &NONE));
    }

    #[test]
    fn without_hotkeys_only_keys_from_before_are_kept() {
        let mut keys = with_hotkeys();
        keys.set_bindings(None);
        assert!(!keys.press(Key::SPACE, &CTRL_SHIFT));
        keys.release(Key::SPACE);
        assert!(keys.repeat(Key::SPACE), "held from before the focus");
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Down(Key),
        Up(Key),
        Focus(bool),
        Wait(bool),
        Typing(bool),
    }

    fn any_step() -> impl Strategy<Value = Step> {
        let key = || {
            prop::sample::select(vec![
                Key::SPACE,
                ENTER,
                Key::M,
                V,
                Key::END,
                Key::LEFT_CTRL,
                Key::RIGHT_CTRL,
                Key::LEFT_SHIFT,
                Key::LEFT_ALT,
            ])
        };
        prop_oneof![
            4 => key().prop_map(Step::Down),
            3 => key().prop_map(Step::Up),
            1 => any::<bool>().prop_map(Step::Focus),
            1 => any::<bool>().prop_map(Step::Wait),
            1 => any::<bool>().prop_map(Step::Typing),
        ]
    }

    fn held(keys: &[Key]) -> Modifiers {
        keys.iter()
            .fold(Modifiers::NONE, |all, key| all.with(key.modifier()))
    }

    proptest! {
        // Whatever the keyboard does, egui never gets a repeat of a key it
        // does not hold, gets every repeat of a key the panel gave it unless
        // the focus went or a wait began since, never gets a press that sets
        // off a hotkey, and gets no key while Settings waits.
        #[test]
        fn the_panel_sees_whole_keys(
            steps in prop::collection::vec(any_step(), 0..200),
        ) {
            let bindings = Bindings::default();
            let mut keys = PanelKeys::default();
            keys.set_bindings(Some(bindings));
            // The keyboard, the panel's thread as it read the messages, egui,
            // which hears only what passes and forgets what it holds when the
            // focus goes, and the keys whose press passed.
            let mut physical: Vec<Key> = Vec::new();
            let mut read: Vec<Key> = Vec::new();
            let mut egui: Vec<Key> = Vec::new();
            let mut passed: Vec<Key> = Vec::new();
            let mut focused = true;
            let mut waiting = false;
            for step in steps {
                match step {
                    Step::Down(key) => {
                        let repeat = physical.contains(&key);
                        if !repeat {
                            physical.push(key);
                        }
                        if !focused {
                            continue;
                        }
                        let modifiers = [held(&read), held(&physical)];
                        if !read.contains(&key) {
                            read.push(key);
                        }
                        let kept = if repeat {
                            keys.repeat(key)
                        } else {
                            keys.press(key, &modifiers)
                        };
                        if key.is_modifier() {
                            continue;
                        }
                        if repeat {
                            prop_assert!(
                                kept || egui.contains(&key),
                                "a repeat of {} egui never saw",
                                key
                            );
                            prop_assert_eq!(
                                kept,
                                !passed.contains(&key),
                                "a repeat of {}",
                                key
                            );
                        } else {
                            passed.retain(|down| *down != key);
                            if !kept {
                                passed.push(key);
                            }
                            let pressed = Chord::new(held(&physical), key);
                            prop_assert!(
                                kept || !bindings.any_fires(pressed),
                                "{} reached the panel",
                                pressed
                            );
                        }
                        prop_assert!(kept || !waiting, "{} while Settings waits", key);
                        if !kept && !egui.contains(&key) {
                            egui.push(key);
                        }
                    }
                    Step::Up(key) => {
                        if !physical.contains(&key) {
                            continue;
                        }
                        physical.retain(|down| *down != key);
                        if focused {
                            read.retain(|down| *down != key);
                            keys.release(key);
                            egui.retain(|down| *down != key);
                            passed.retain(|down| *down != key);
                        }
                    }
                    Step::Focus(on) => {
                        if !on {
                            egui.clear();
                            passed.clear();
                        }
                        focused = on;
                        keys.set_focused(on);
                    }
                    Step::Wait(on) => {
                        if on && !waiting {
                            passed.clear();
                        }
                        waiting = on;
                        keys.set_capture(on);
                    }
                    Step::Typing(on) => keys.set_typing(on),
                }
            }
        }
    }
}
