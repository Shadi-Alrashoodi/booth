use input::{Action, Bindings, Chord, Elevation, Event, Key, Modifiers, RawKey, Tracker};
use proptest::prelude::*;

const KEYBOARD: usize = 0x0002_0017;
const SECOND_KEYBOARD: usize = 0x0002_0031;
const INJECTED: usize = 0;

const T: Key = Key::from_scan(0, 0x14);
const W: Key = Key::from_scan(0, 0x11);
const K: Key = Key::from_scan(0, 0x25);
const UP: Key = Key::from_scan(0xE0, 0x48);
const NUM_8: Key = Key::from_scan(0, 0x48);

fn tracker() -> Tracker {
    Tracker::new(Bindings::default())
}

fn down(tracker: &mut Tracker, key: Key) -> Vec<Event> {
    tracker.raw(RawKey::press(key, KEYBOARD))
}

fn up(tracker: &mut Tracker, key: Key) -> Vec<Event> {
    tracker.raw(RawKey::release(key, KEYBOARD))
}

fn keys(tracker: &mut Tracker, presses: &[Key]) -> Vec<Event> {
    presses.iter().flat_map(|key| down(tracker, *key)).collect()
}

const PRESSED_PTT: Event = Event::Pressed(Action::PushToTalk);
const RELEASED_PTT: Event = Event::Released(Action::PushToTalk);
const MUTE: Event = Event::Pressed(Action::Mute);

#[test]
fn right_ctrl_is_push_to_talk_and_left_ctrl_is_not() {
    let mut tracker = tracker();
    assert_eq!(down(&mut tracker, Key::LEFT_CTRL), []);
    assert_eq!(up(&mut tracker, Key::LEFT_CTRL), []);
    assert_eq!(down(&mut tracker, Key::RIGHT_CTRL), [PRESSED_PTT]);
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), [RELEASED_PTT]);
}

// E0 keys share their make code with another key: Right Alt with Left Alt,
// the arrows with the keypad digits under Num Lock off.
#[test]
fn keys_behind_e0_are_their_own_keys() {
    let mut bindings = Bindings::default();
    bindings.set(Action::Mute, Chord::new(Modifiers::NONE, UP));
    bindings.set(Action::Deafen, Chord::new(Modifiers::NONE, Key::RIGHT_ALT));
    let mut tracker = Tracker::new(bindings);
    assert_eq!(down(&mut tracker, NUM_8), []);
    assert_eq!(down(&mut tracker, Key::LEFT_ALT), []);
    up(&mut tracker, NUM_8);
    up(&mut tracker, Key::LEFT_ALT);
    assert_eq!(down(&mut tracker, UP), [MUTE]);
    assert_eq!(
        down(&mut tracker, Key::RIGHT_ALT),
        [Event::Pressed(Action::Deafen)]
    );

    let raw = RawKey::press(Key::RIGHT_CTRL, KEYBOARD);
    assert_eq!((raw.make_code, raw.flags), (0x1D, 2));
    assert_eq!((raw.key(), raw.down()), (Some(Key::RIGHT_CTRL), true));
    let raw = RawKey::release(Key::RIGHT_CTRL, KEYBOARD);
    assert_eq!((raw.key(), raw.down()), (Some(Key::RIGHT_CTRL), false));
}

// With Num Lock on, an arrow comes wrapped in a Shift break and make that
// nobody pressed. A real Shift held under it stays held.
#[test]
fn the_made_up_shift_around_the_arrows_is_not_a_key() {
    let mut tracker = tracker();
    down(&mut tracker, Key::LEFT_SHIFT);
    let fake_shift = |flags| RawKey {
        make_code: 0x2A,
        flags,
        vkey: 0x10,
        device: KEYBOARD,
    };
    assert_eq!(fake_shift(3).key(), None);
    assert_eq!(tracker.raw(fake_shift(3)), []);
    down(&mut tracker, UP);
    tracker.raw(fake_shift(2));
    down(&mut tracker, Key::LEFT_CTRL);
    assert_eq!(down(&mut tracker, Key::M), [MUTE]);

    // Raw Input marks the second half of an escape sequence with virtual
    // key 0xFF.
    let half = RawKey {
        make_code: 0x45,
        flags: 0,
        vkey: 0xFF,
        device: KEYBOARD,
    };
    assert_eq!(half.key(), None);
    let pause = RawKey {
        make_code: 0x1D,
        flags: 4,
        vkey: 0x13,
        device: KEYBOARD,
    };
    assert_eq!((pause.key(), pause.down()), (Some(Key::PAUSE), true));
}

#[test]
fn modifiers_count_in_any_order_and_from_either_side() {
    for order in [
        [Key::LEFT_CTRL, Key::LEFT_SHIFT],
        [Key::LEFT_SHIFT, Key::LEFT_CTRL],
        [Key::RIGHT_SHIFT, Key::LEFT_CTRL],
        [Key::LEFT_SHIFT, Key::RIGHT_CTRL],
    ] {
        let mut tracker = tracker();
        keys(&mut tracker, &order);
        let fired = down(&mut tracker, Key::M);
        assert!(fired.contains(&MUTE), "{order:?}: {fired:?}");
    }
}

// A press fires on exactly its modifiers: one more, or one fewer, is
// another chord.
#[test]
fn a_press_needs_exactly_its_modifiers() {
    let mut tracker = tracker();
    assert_eq!(
        keys(
            &mut tracker,
            &[Key::LEFT_CTRL, Key::LEFT_ALT, Key::LEFT_SHIFT, Key::M]
        ),
        []
    );
    let mut tracker = self::tracker();
    assert_eq!(keys(&mut tracker, &[Key::LEFT_CTRL, Key::M]), []);
    let mut tracker = self::tracker();
    assert_eq!(keys(&mut tracker, &[Key::M]), []);
}

// Push to talk still works while other keys are held, a player running with
// Shift and walking with W.
#[test]
fn push_to_talk_works_with_other_keys_held() {
    let mut tracker = tracker();
    assert_eq!(keys(&mut tracker, &[Key::LEFT_SHIFT, W]), []);
    assert_eq!(down(&mut tracker, Key::RIGHT_CTRL), [PRESSED_PTT]);
    assert_eq!(up(&mut tracker, W), []);
    assert_eq!(up(&mut tracker, Key::LEFT_SHIFT), []);
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), [RELEASED_PTT]);
}

#[test]
fn modifier_let_go_first() {
    let mut tracker = tracker();
    keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT]);
    assert_eq!(down(&mut tracker, Key::M), [MUTE]);
    assert_eq!(up(&mut tracker, Key::LEFT_SHIFT), []);
    assert_eq!(up(&mut tracker, Key::M), []);
    // Ctrl is still down, and Ctrl+M is nobody's.
    assert_eq!(down(&mut tracker, Key::M), []);
    up(&mut tracker, Key::M);
    assert_eq!(keys(&mut tracker, &[Key::LEFT_SHIFT, Key::M]), [MUTE]);

    let mut bindings = Bindings::default();
    bindings.set(Action::PushToTalk, Chord::new(Modifiers::CTRL, T));
    let mut tracker = Tracker::new(bindings);
    assert_eq!(keys(&mut tracker, &[Key::RIGHT_CTRL, T]), [PRESSED_PTT]);
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), [RELEASED_PTT]);
    assert_eq!(up(&mut tracker, T), []);
    // The key alone, with its modifier pressed after it, starts nothing.
    assert_eq!(keys(&mut tracker, &[T, Key::LEFT_CTRL]), []);
}

#[test]
fn key_repeat_does_not_fire_twice() {
    let mut tracker = tracker();
    keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT]);
    assert_eq!(keys(&mut tracker, &[Key::M, Key::M, Key::M]), [MUTE]);
    assert_eq!(
        keys(
            &mut tracker,
            &[Key::RIGHT_CTRL, Key::RIGHT_CTRL, Key::RIGHT_CTRL]
        ),
        [PRESSED_PTT]
    );
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), [RELEASED_PTT]);
    // The modifiers repeating do not make it a new press either.
    assert_eq!(keys(&mut tracker, &[Key::LEFT_SHIFT, Key::LEFT_SHIFT]), []);
}

// Keys went unseen (an administrator window, a locked screen): push to talk
// is let go at once, and the next event starts from a clean state.
#[test]
fn a_stuck_hold_is_let_go_and_the_state_refreshed() {
    let mut tracker = tracker();
    keys(&mut tracker, &[Key::LEFT_CTRL, Key::RIGHT_CTRL]);
    assert!(tracker.watching());
    assert_eq!(tracker.blind(), [RELEASED_PTT]);
    assert!(!tracker.watching(), "nothing counts as down any more");
    assert_eq!(tracker.blind(), []);

    // Both were let go while nobody was looking. Shift+M is not
    // Ctrl+Shift+M, and M alone is nothing.
    assert_eq!(keys(&mut tracker, &[Key::LEFT_SHIFT, Key::M]), []);
    assert_eq!(tracker.held().count(), 2);
}

// A key held through the gap repeats when keys come back: a held action
// takes up again, a press does not fire again.
#[test]
fn a_key_held_through_the_gap_comes_back_as_a_repeat() {
    let mut tracker = tracker();
    down(&mut tracker, Key::RIGHT_CTRL);
    tracker.blind();
    assert_eq!(down(&mut tracker, Key::RIGHT_CTRL), [PRESSED_PTT]);
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), [RELEASED_PTT]);

    let mut bindings = Bindings::default();
    bindings.set(Action::Mute, Chord::new(Modifiers::NONE, Key::M));
    let mut tracker = Tracker::new(bindings);
    assert_eq!(down(&mut tracker, Key::M), [MUTE]);
    tracker.blind();
    assert_eq!(down(&mut tracker, Key::M), []);
    // Let go and pressed again, it is a press.
    assert_eq!(up(&mut tracker, Key::M), []);
    assert_eq!(down(&mut tracker, Key::M), [MUTE]);
}

#[test]
fn nothing_fires_while_paused_and_the_pause_lets_go() {
    let mut tracker = tracker();
    down(&mut tracker, Key::RIGHT_CTRL);
    assert_eq!(
        tracker.pause(Some(Elevation::Elevated)),
        [RELEASED_PTT, Event::Paused(Elevation::Elevated)]
    );
    assert!(tracker.paused());
    assert_eq!(tracker.pause(Some(Elevation::Unreadable)), []);
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), []);
    assert_eq!(down(&mut tracker, Key::RIGHT_CTRL), []);
    assert_eq!(tracker.pause(None), [Event::Resumed]);
    assert_eq!(tracker.pause(None), []);
    up(&mut tracker, Key::RIGHT_CTRL);
    assert_eq!(down(&mut tracker, Key::RIGHT_CTRL), [PRESSED_PTT]);
}

#[test]
fn each_key_keeps_the_device_it_came_from() {
    let mut tracker = tracker();
    tracker.raw(RawKey::press(Key::LEFT_CTRL, KEYBOARD));
    tracker.raw(RawKey::press(Key::LEFT_SHIFT, SECOND_KEYBOARD));
    tracker.raw(RawKey::press(W, INJECTED));
    let held: Vec<(Key, usize)> = tracker.held().collect();
    assert_eq!(
        held,
        [
            (Key::LEFT_CTRL, KEYBOARD),
            (Key::LEFT_SHIFT, SECOND_KEYBOARD),
            (W, INJECTED),
        ]
    );
}

// Remote control relies on this: an injected key-up, device zero, cannot let
// go of a key a real keyboard holds down.
#[test]
fn a_break_from_another_device_does_not_let_go() {
    let mut tracker = tracker();
    assert_eq!(
        tracker.raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD)),
        [PRESSED_PTT]
    );
    assert_eq!(tracker.raw(RawKey::release(Key::RIGHT_CTRL, INJECTED)), []);
    assert_eq!(tracker.held().count(), 1);
    // The same key from a second keyboard is a repeat, not a new press.
    assert_eq!(
        tracker.raw(RawKey::press(Key::RIGHT_CTRL, SECOND_KEYBOARD)),
        []
    );
    assert_eq!(tracker.raw(RawKey::release(Key::RIGHT_CTRL, KEYBOARD)), []);
    assert_eq!(
        tracker.raw(RawKey::release(Key::RIGHT_CTRL, SECOND_KEYBOARD)),
        [RELEASED_PTT]
    );
}

// A Bluetooth keyboard that drops, or a USB one pulled out, with push to
// talk down: no break comes, and the same key from another device cannot end
// it. Windows says the device went, and what it held goes with it.
#[test]
fn a_keyboard_that_goes_away_lets_go_of_what_it_held() {
    let mut tracker = tracker();
    assert_eq!(
        tracker.raw(RawKey::press(Key::RIGHT_CTRL, KEYBOARD)),
        [PRESSED_PTT]
    );
    tracker.raw(RawKey::press(Key::LEFT_SHIFT, SECOND_KEYBOARD));
    assert_eq!(tracker.device_gone(KEYBOARD), [RELEASED_PTT]);
    let held: Vec<(Key, usize)> = tracker.held().collect();
    assert_eq!(held, [(Key::LEFT_SHIFT, SECOND_KEYBOARD)]);
    assert_eq!(tracker.device_gone(KEYBOARD), [], "said once");

    // Back with a new handle, its press is a new press and its release
    // ends it.
    const BACK: usize = 0x0002_0045;
    assert_eq!(
        tracker.raw(RawKey::press(Key::RIGHT_CTRL, BACK)),
        [PRESSED_PTT]
    );
    assert_eq!(
        tracker.raw(RawKey::release(Key::RIGHT_CTRL, BACK)),
        [RELEASED_PTT]
    );

    // The same key still down on a keyboard that stays keeps the hold.
    tracker.raw(RawKey::press(Key::RIGHT_CTRL, BACK));
    tracker.raw(RawKey::press(Key::RIGHT_CTRL, SECOND_KEYBOARD));
    assert_eq!(tracker.device_gone(BACK), []);
    assert_eq!(
        tracker.raw(RawKey::release(Key::RIGHT_CTRL, SECOND_KEYBOARD)),
        [RELEASED_PTT]
    );
    tracker.raw(RawKey::release(Key::LEFT_SHIFT, SECOND_KEYBOARD));
    assert!(!tracker.watching());
}

// While settings waits, a keyboard that goes mid-chord leaves nothing
// behind to finish it.
#[test]
fn a_keyboard_that_goes_away_mid_chord_reads_nothing() {
    let mut tracker = tracker();
    tracker.capture(true);
    tracker.raw(RawKey::press(Key::LEFT_CTRL, KEYBOARD));
    tracker.raw(RawKey::press(Key::RIGHT_ALT, SECOND_KEYBOARD));
    assert_eq!(tracker.device_gone(SECOND_KEYBOARD), []);
    assert_eq!(tracker.raw(RawKey::release(Key::LEFT_CTRL, KEYBOARD)), []);
    assert_eq!(
        tracker.raw(RawKey::press(Key::M, KEYBOARD)),
        [Event::Chord(Chord::new(Modifiers::NONE, Key::M))]
    );
}

#[test]
fn settings_reads_the_next_chord() {
    let mut tracker = tracker();
    down(&mut tracker, Key::RIGHT_CTRL);
    assert_eq!(tracker.capture(true), [RELEASED_PTT]);
    up(&mut tracker, Key::RIGHT_CTRL);

    // Right Ctrl alone is read when it is let go.
    assert_eq!(down(&mut tracker, Key::RIGHT_CTRL), []);
    assert_eq!(
        up(&mut tracker, Key::RIGHT_CTRL),
        [Event::Chord(Chord::new(Modifiers::NONE, Key::RIGHT_CTRL))]
    );
    // The default mute chord, which does not mute.
    let chord = Chord::new(Modifiers::CTRL.with(Modifiers::SHIFT), Key::M);
    assert_eq!(
        keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::M]),
        [Event::Chord(chord)]
    );
    // Still held, another key is another try.
    assert_eq!(
        down(&mut tracker, K),
        [Event::Chord(Chord::new(
            Modifiers::CTRL.with(Modifiers::SHIFT),
            K
        ))]
    );
    // Letting go of the modifiers after a chord reads nothing more.
    for key in [Key::M, K, Key::LEFT_SHIFT, Key::LEFT_CTRL] {
        assert_eq!(up(&mut tracker, key), [], "{key}");
    }
    // A modifier held with the one let go.
    keys(&mut tracker, &[Key::LEFT_SHIFT, Key::RIGHT_CTRL]);
    assert_eq!(
        up(&mut tracker, Key::LEFT_SHIFT),
        [Event::Chord(Chord::new(Modifiers::SHIFT, Key::RIGHT_CTRL))]
    );
    up(&mut tracker, Key::RIGHT_CTRL);

    assert_eq!(down(&mut tracker, Key::ESC), [Event::Cancelled]);
    up(&mut tracker, Key::ESC);

    // Back on with the chord's keys still down: letting go fires nothing,
    // and neither do their repeats.
    keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::M]);
    assert_eq!(tracker.capture(false), []);
    assert_eq!(down(&mut tracker, Key::M), []);
    assert_eq!(up(&mut tracker, Key::M), []);
    assert_eq!(down(&mut tracker, Key::M), [MUTE]);
}

#[test]
fn new_bindings_let_go_of_a_hold_and_apply_at_once() {
    let mut tracker = tracker();
    down(&mut tracker, Key::RIGHT_CTRL);
    let mut bindings = Bindings::default();
    bindings.set(Action::PushToTalk, Chord::new(Modifiers::NONE, T));
    assert_eq!(tracker.set_bindings(bindings), [RELEASED_PTT]);
    assert_eq!(up(&mut tracker, Key::RIGHT_CTRL), []);
    assert_eq!(down(&mut tracker, T), [PRESSED_PTT]);
}

#[test]
fn what_is_not_a_key_is_dropped() {
    let mut tracker = tracker();
    for raw in [
        RawKey {
            make_code: 0,
            flags: 0,
            vkey: 0x41,
            device: KEYBOARD,
        },
        RawKey {
            make_code: 0xFF,
            flags: 0,
            vkey: 0,
            device: KEYBOARD,
        },
        RawKey {
            make_code: 0x1D,
            flags: 2,
            vkey: 0xFF,
            device: KEYBOARD,
        },
    ] {
        assert_eq!(raw.key(), None, "{raw:?}");
        assert_eq!(tracker.raw(raw), []);
    }
    assert!(!tracker.watching());
}

const PANIC: Event = Event::Pressed(Action::Panic);

fn physical(tracker: &mut Tracker, device: usize, steps: &[(Key, bool)]) -> Vec<Event> {
    steps
        .iter()
        .flat_map(|(key, down)| {
            tracker.raw(if *down {
                RawKey::press(*key, device)
            } else {
                RawKey::release(*key, device)
            })
        })
        .collect()
}

fn panic_chord() -> [(Key, bool); 3] {
    [
        (Key::LEFT_CTRL, true),
        (Key::RIGHT_SHIFT, true),
        (Key::END, true),
    ]
}

#[test]
fn the_panic_key_is_ctrl_shift_end_from_a_physical_keyboard() {
    let mut tracker = tracker();
    assert_eq!(physical(&mut tracker, KEYBOARD, &panic_chord()), [PANIC]);
    // A repeat is not another press.
    assert_eq!(down(&mut tracker, Key::END), []);
    let mut tracker = self::tracker();
    assert_eq!(physical(&mut tracker, INJECTED, &panic_chord()), []);
    // Two keyboards make one chord between them.
    let mut tracker = self::tracker();
    tracker.raw(RawKey::press(Key::LEFT_CTRL, SECOND_KEYBOARD));
    tracker.raw(RawKey::press(Key::LEFT_SHIFT, KEYBOARD));
    assert_eq!(
        tracker.raw(RawKey::press(Key::END, SECOND_KEYBOARD)),
        [PANIC]
    );
}

// Nobody reaching for it in a hurry should have to let go of Alt first.
#[test]
fn panic_key_takes_more_modifiers_not_fewer() {
    let mut tracker = tracker();
    assert_eq!(
        keys(
            &mut tracker,
            &[Key::LEFT_ALT, Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::END]
        ),
        [PANIC]
    );
    let mut tracker = self::tracker();
    assert_eq!(keys(&mut tracker, &[Key::LEFT_CTRL, Key::END]), []);
}

// The chord's modifiers are tracked from physical keyboards only. Injected
// ones do not make the chord, and injected releases, however many, do not
// unmake it.
#[test]
fn injected_modifiers_neither_make_nor_mask_the_panic_chord() {
    for controlled in [false, true] {
        let mut tracker = tracker();
        tracker.set_controlled(controlled);
        physical(
            &mut tracker,
            INJECTED,
            &[(Key::LEFT_CTRL, true), (Key::LEFT_SHIFT, true)],
        );
        assert_eq!(down(&mut tracker, Key::END), [], "controlled {controlled}");
        up(&mut tracker, Key::END);
        down(&mut tracker, Key::LEFT_CTRL);
        assert_eq!(down(&mut tracker, Key::END), [], "Shift is injected");
        up(&mut tracker, Key::END);

        let mut tracker = self::tracker();
        tracker.set_controlled(controlled);
        keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT]);
        for _ in 0..1_000 {
            for key in [
                Key::LEFT_CTRL,
                Key::RIGHT_CTRL,
                Key::LEFT_SHIFT,
                Key::RIGHT_SHIFT,
            ] {
                assert_eq!(tracker.raw(RawKey::release(key, INJECTED)), []);
            }
        }
        assert_eq!(
            down(&mut tracker, Key::END),
            [PANIC],
            "controlled {controlled}"
        );
    }
}

// An injector holding End down cannot turn a real press into a repeat.
#[test]
fn an_injected_end_held_down_does_not_swallow_a_real_press() {
    for controlled in [false, true] {
        let mut tracker = tracker();
        tracker.raw(RawKey::press(Key::END, INJECTED));
        tracker.set_controlled(controlled);
        tracker.raw(RawKey::press(Key::END, INJECTED));
        keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT]);
        assert_eq!(
            down(&mut tracker, Key::END),
            [PANIC],
            "controlled {controlled}"
        );
    }
}

// Settings waiting for a key does not turn the panic key off. The chord is
// read for settings as well.
#[test]
fn the_panic_key_works_while_settings_waits_for_a_key() {
    let mut tracker = tracker();
    tracker.capture(true);
    let chord = Chord::new(Modifiers::CTRL.with(Modifiers::SHIFT), Key::END);
    assert_eq!(
        keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::END]),
        [PANIC, Event::Chord(chord)]
    );
}

// Rebound, it is the new chord and only that.
#[test]
fn the_panic_key_can_be_rebound() {
    let mut bindings = Bindings::default();
    bindings.set(Action::Panic, Chord::new(Modifiers::ALT, Key::PAUSE));
    let mut tracker = Tracker::new(bindings);
    assert_eq!(
        keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::END]),
        []
    );
    let mut tracker = Tracker::new(bindings);
    assert_eq!(keys(&mut tracker, &[Key::RIGHT_ALT, Key::PAUSE]), [PANIC]);
}

// While this PC is controlled, a key from device zero is nobody's: not a
// binding's key, not a binding's modifier, and not a key that makes a
// physical press a repeat. Its release still clears what it pressed before.
#[test]
fn while_controlled_every_binding_ignores_device_zero() {
    let mut tracker = tracker();
    tracker.set_controlled(true);
    assert!(tracker.controlled());
    assert_eq!(
        physical(&mut tracker, INJECTED, &[(Key::RIGHT_CTRL, true)]),
        []
    );
    physical(
        &mut tracker,
        INJECTED,
        &[
            (Key::LEFT_CTRL, true),
            (Key::LEFT_SHIFT, true),
            (Key::M, true),
        ],
    );
    assert_eq!(tracker.held().count(), 0, "nothing injected is held");
    // A physical M under injected Ctrl+Shift is M alone.
    assert_eq!(down(&mut tracker, Key::M), []);
    up(&mut tracker, Key::M);
    assert_eq!(
        keys(&mut tracker, &[Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::M]),
        [MUTE]
    );

    // Pressed by a macro tool before control began, let go during it.
    let mut tracker = self::tracker();
    tracker.raw(RawKey::press(Key::RIGHT_CTRL, INJECTED));
    tracker.set_controlled(true);
    assert_eq!(
        tracker.raw(RawKey::release(Key::RIGHT_CTRL, INJECTED)),
        [RELEASED_PTT]
    );
    assert!(!tracker.watching());
    // And one still held counts for nothing while controlled.
    tracker.set_controlled(false);
    tracker.raw(RawKey::press(Key::LEFT_SHIFT, INJECTED));
    tracker.set_controlled(true);
    down(&mut tracker, Key::LEFT_CTRL);
    assert_eq!(down(&mut tracker, Key::M), []);
}

#[test]
fn while_controlled_settings_reads_no_injected_key() {
    let mut tracker = tracker();
    tracker.set_controlled(true);
    tracker.capture(true);
    assert_eq!(
        physical(
            &mut tracker,
            INJECTED,
            &[
                (Key::LEFT_CTRL, true),
                (Key::M, true),
                (Key::M, false),
                (Key::LEFT_CTRL, false)
            ]
        ),
        []
    );
    assert_eq!(
        down(&mut tracker, Key::M),
        [Event::Chord(Chord::new(Modifiers::NONE, Key::M))]
    );
}

fn any_key() -> impl Strategy<Value = Key> {
    prop::sample::select(vec![
        Key::LEFT_CTRL,
        Key::RIGHT_CTRL,
        Key::LEFT_SHIFT,
        Key::RIGHT_SHIFT,
        Key::LEFT_ALT,
        Key::M,
        Key::D,
        Key::I,
        Key::SPACE,
        Key::ESC,
        Key::END,
        T,
    ])
}

#[derive(Clone, Debug)]
enum Step {
    Raw(Key, bool, usize),
    Blind,
    Pause(bool),
    Capture(bool),
    Gone(usize),
    Controlled(bool),
}

fn any_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => (any_key(), any::<bool>(), 0usize..3).prop_map(|(key, down, device)| Step::Raw(key, down, device)),
        1 => Just(Step::Blind),
        1 => any::<bool>().prop_map(Step::Pause),
        1 => any::<bool>().prop_map(Step::Capture),
        1 => (1usize..3).prop_map(Step::Gone),
        1 => any::<bool>().prop_map(Step::Controlled),
    ]
}

fn step(tracker: &mut Tracker, step: &Step) -> Vec<Event> {
    match *step {
        Step::Raw(key, true, device) => tracker.raw(RawKey::press(key, device)),
        Step::Raw(key, false, device) => tracker.raw(RawKey::release(key, device)),
        Step::Blind => tracker.blind(),
        Step::Pause(on) => tracker.pause(on.then_some(Elevation::Elevated)),
        Step::Capture(on) => tracker.capture(on),
        Step::Gone(device) => tracker.device_gone(device),
        Step::Controlled(on) => {
            tracker.set_controlled(on);
            Vec::new()
        }
    }
}

proptest! {
    // Whatever arrives, push to talk is told down and up in turn, never up
    // without a down, and a held action is always one whose chord is down.
    #[test]
    fn holds_start_and_end_in_turn(steps in prop::collection::vec(any_step(), 0..200)) {
        let mut tracker = tracker();
        let mut talking = false;
        for step in steps {
            let events = self::step(&mut tracker, &step);
            for event in events {
                match event {
                    Event::Pressed(Action::PushToTalk) => {
                        prop_assert!(!talking);
                        talking = true;
                    }
                    Event::Released(Action::PushToTalk) => {
                        prop_assert!(talking);
                        talking = false;
                    }
                    _ => {}
                }
            }
            if talking {
                prop_assert!(tracker.held().any(|(key, _)| key == Key::RIGHT_CTRL));
                prop_assert!(!tracker.paused() && !tracker.capturing());
            }
        }
    }

    // Whatever arrives, injected keys included, the panic key fires only on
    // a physical End going down while physical keyboards hold Ctrl and
    // Shift, going by the keyboards' own presses and releases.
    #[test]
    fn only_a_physical_chord_is_the_panic_key(steps in prop::collection::vec(any_step(), 0..300)) {
        let mut tracker = tracker();
        let mut physical: Vec<(Key, usize)> = Vec::new();
        for step in steps {
            let events = self::step(&mut tracker, &step);
            if events.contains(&PANIC) {
                let held = |modifier: Modifiers| {
                    physical.iter().any(|(key, _)| key.modifier() == modifier)
                };
                prop_assert!(
                    matches!(step, Step::Raw(Key::END, true, device) if device != INJECTED),
                    "{:?}",
                    step
                );
                prop_assert!(held(Modifiers::CTRL) && held(Modifiers::SHIFT), "{:?}", physical);
            }
            match step {
                Step::Raw(key, true, device) if device != INJECTED => {
                    if !physical.contains(&(key, device)) {
                        physical.push((key, device));
                    }
                }
                Step::Raw(key, false, device) => physical.retain(|held| *held != (key, device)),
                Step::Gone(device) => physical.retain(|(_, from)| *from != device),
                _ => {}
            }
        }
    }

    // The other way round, the one the person being controlled relies on.
    // Whatever came before, injected keys, pauses and settings included,
    // once the physical keyboards have let go of everything or gone away, a
    // physical Ctrl+Shift+End is the panic key, whatever is injected between
    // its presses.
    #[test]
    fn a_physical_chord_is_always_the_panic_key(
        steps in prop::collection::vec(any_step(), 0..300),
        unplugged in any::<bool>(),
        injected in prop::array::uniform3(
            prop::collection::vec((any_key(), any::<bool>()), 0..20)
        ),
    ) {
        let mut tracker = tracker();
        for step in &steps {
            self::step(&mut tracker, step);
        }
        tracker.pause(None);
        if unplugged {
            tracker.device_gone(1);
            tracker.device_gone(2);
        } else {
            let held: Vec<(Key, usize)> = tracker
                .held()
                .filter(|(_, device)| *device != INJECTED)
                .collect();
            for (key, device) in held {
                tracker.raw(RawKey::release(key, device));
            }
        }
        let mut fired = Vec::new();
        for (noise, key) in injected.iter().zip([Key::LEFT_CTRL, Key::LEFT_SHIFT, Key::END]) {
            for &(noise, down) in noise {
                tracker.raw(if down {
                    RawKey::press(noise, INJECTED)
                } else {
                    RawKey::release(noise, INJECTED)
                });
            }
            fired = tracker.raw(RawKey::press(key, KEYBOARD));
        }
        prop_assert!(fired.contains(&PANIC), "{:?}", fired);
    }
}
