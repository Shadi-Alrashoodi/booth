use input::{Action, Bindings, Chord, Key, Modifiers, ParseError};
use proptest::prelude::*;

const CTRL_SHIFT: Modifiers = Modifiers::CTRL.with(Modifiers::SHIFT);

fn chord(text: &str) -> Chord {
    text.parse().unwrap_or_else(|err| panic!("{text}: {err}"))
}

#[test]
fn default_chords_as_text() {
    let written: Vec<String> = Bindings::default()
        .iter()
        .map(|(_, chord)| chord.to_string())
        .collect();
    assert_eq!(
        written,
        [
            "Right Ctrl",
            "Ctrl+Shift+M",
            "Ctrl+Shift+D",
            "Ctrl+Shift+S",
            "Ctrl+Shift+End",
            "Ctrl+Shift+Space",
            "Ctrl+Shift+I"
        ]
    );
    for action in Action::ALL {
        let chord = action.default_chord();
        assert_eq!(chord.to_string().parse::<Chord>(), Ok(chord));
    }
}

#[test]
fn modifiers_are_written_in_one_order_and_read_in_any() {
    let all = Chord::new(
        Modifiers::WIN
            .with(Modifiers::ALT)
            .with(Modifiers::SHIFT)
            .with(Modifiers::CTRL),
        Key::from_scan(0, 0x3B),
    );
    assert_eq!(all.to_string(), "Ctrl+Shift+Alt+Win+F1");
    assert_eq!(chord("win+alt+shift+ctrl+f1"), all);
    assert_eq!(
        chord(" Control + Shift + m "),
        Chord::new(CTRL_SHIFT, Key::M)
    );
    assert_eq!(chord("Windows+Left"), chord("Win+Left"));
}

#[test]
fn a_modifier_key_can_be_the_key() {
    assert_eq!(
        chord("right ctrl"),
        Chord::new(Modifiers::NONE, Key::RIGHT_CTRL)
    );
    let shift_right_ctrl = Chord::new(Modifiers::SHIFT, Key::RIGHT_CTRL);
    assert_eq!(shift_right_ctrl.to_string(), "Shift+Right Ctrl");
    assert_eq!(chord("Shift+Right Ctrl"), shift_right_ctrl);
}

#[test]
fn keys_without_a_name_round_trip() {
    let iso = Chord::new(Modifiers::CTRL, Key::from_scan(0, 0x56));
    assert_eq!(iso.to_string(), "Ctrl+Key 56");
    assert_eq!(chord("Ctrl+Key 56"), iso);
    let media = Chord::new(Modifiers::NONE, Key::from_scan(0xE0, 0x22));
    assert_eq!(chord(&media.to_string()), media);
    assert_eq!(chord("Pause"), Chord::new(Modifiers::NONE, Key::PAUSE));
    assert_eq!(chord("Ctrl+Num Plus").key, Key::from_scan(0, 0x4E));
}

#[test]
fn what_is_not_a_chord_says_why() {
    let cases: [(&str, ParseError); 7] = [
        ("", ParseError::NoKey),
        ("Ctrl+", ParseError::NoKey),
        ("Ctrl+Shift", ParseError::NotAKey(String::from("Shift"))),
        ("Ctrl+Pgup", ParseError::NotAKey(String::from("Pgup"))),
        ("M+Ctrl", ParseError::NotAModifier(String::from("M"))),
        ("Ctrl+Control+M", ParseError::Twice("Ctrl")),
        ("Ctrl++M", ParseError::Gap),
    ];
    for (text, err) in cases {
        assert_eq!(text.parse::<Chord>(), Err(err), "{text:?}");
    }
    assert_eq!(
        "Ctrl+Pgup".parse::<Chord>().unwrap_err().to_string(),
        "Pgup is not a key Booth knows"
    );
    assert_eq!(
        "Ctrl+".parse::<Chord>().unwrap_err().to_string(),
        "there is no key after the last +"
    );
}

#[test]
fn one_chord_for_two_actions_is_a_conflict() {
    let bindings = Bindings::default();
    assert_eq!(
        bindings.conflict(Action::Deafen, chord("Ctrl+Shift+M")),
        Some(Action::Mute)
    );
    assert_eq!(
        bindings.conflict(Action::Mute, chord("Right Ctrl")),
        Some(Action::PushToTalk)
    );
    // The action's own chord, and anything free, is fine.
    assert_eq!(bindings.conflict(Action::Mute, chord("Ctrl+Shift+M")), None);
    assert_eq!(bindings.conflict(Action::Mute, chord("F9")), None);
    // Other modifiers on the same key make another press.
    assert_eq!(bindings.conflict(Action::Deafen, chord("Ctrl+M")), None);
    assert_eq!(bindings.conflict(Action::Deafen, chord("Left Ctrl")), None);
    assert_eq!(bindings.first_conflict(), None);
}

// Push to talk takes its key with any extra modifiers, so a press on the
// same key with more modifiers would set off both.
#[test]
fn a_held_key_conflicts_with_its_key_under_more_modifiers() {
    let mut bindings = Bindings::default();
    bindings.set(Action::PushToTalk, chord("M"));
    assert_eq!(
        bindings.first_conflict(),
        Some((Action::PushToTalk, Action::Mute))
    );
    assert_eq!(
        bindings.conflict(Action::PushToTalk, chord("Shift+D")),
        Some(Action::Deafen)
    );
    // Deafen's Ctrl+Shift+D has no Alt in it, so Alt+D starts only push to
    // talk.
    assert_eq!(bindings.conflict(Action::PushToTalk, chord("Alt+D")), None);
    assert_eq!(
        bindings.conflict(Action::PushToTalk, chord("Shift+M")),
        Some(Action::Mute)
    );
    bindings.set(Action::PushToTalk, chord("Ctrl+Shift+Alt+M"));
    assert_eq!(bindings.first_conflict(), None);
}

// The panic key also fires with more modifiers held, like push to talk, so
// its key under more modifiers is taken too.
#[test]
fn the_panic_key_takes_its_key_under_more_modifiers() {
    let bindings = Bindings::default();
    assert_eq!(
        bindings.conflict(Action::Mute, chord("Ctrl+Shift+Alt+End")),
        Some(Action::Panic)
    );
    assert_eq!(
        bindings.conflict(Action::Panic, chord("Ctrl+End")),
        None,
        "Ctrl+End is nobody's"
    );
    assert_eq!(bindings.conflict(Action::Mute, chord("Ctrl+End")), None);
    assert_eq!(
        bindings.conflict(Action::Panic, chord("Ctrl+Shift+M")),
        Some(Action::Mute)
    );
    assert_eq!(Action::Panic.name(), "panic key");
}

// Raw Input hears these like any other keys, but Windows acts on them first,
// mostly by moving to another window, so settings does not take them.
#[test]
fn the_chords_windows_keeps_for_itself() {
    for text in [
        "Left Win",
        "Right Win",
        "Shift+Left Win",
        "Win+G",
        "Ctrl+Win+Right",
        "Alt+Tab",
        "Shift+Alt+Tab",
        "Ctrl+Alt+Tab",
        "Alt+Esc",
        "Ctrl+Esc",
        "Ctrl+Shift+Esc",
        "Ctrl+Alt+Delete",
    ] {
        assert!(chord(text).taken_by_windows(), "{text}");
    }
    for text in [
        "Tab",
        "Ctrl+Tab",
        "Shift+Esc",
        "Delete",
        "Ctrl+Delete",
        "Alt+Delete",
        "Left Alt",
        "F9",
    ] {
        assert!(!chord(text).taken_by_windows(), "{text}");
    }
    for (_, chord) in Bindings::default().iter() {
        assert!(!chord.taken_by_windows(), "{chord}");
    }
}

fn any_chord() -> impl Strategy<Value = Chord> {
    let prefix = prop::sample::select(vec![0u8, 0xE0, 0xE1]);
    (prefix, 1u8..=0xFE, 0u8..16)
        .prop_filter("E0 and E1 are prefixes, not keys", |(prefix, code, _)| {
            *prefix != 0 || (*code != 0xE0 && *code != 0xE1)
        })
        .prop_map(|(prefix, code, bits)| {
            let modifiers = [
                Modifiers::CTRL,
                Modifiers::SHIFT,
                Modifiers::ALT,
                Modifiers::WIN,
            ]
            .into_iter()
            .enumerate()
            .filter(|(i, _)| bits & (1 << i) != 0)
            .fold(Modifiers::NONE, |all, (_, modifier)| all.with(modifier));
            Chord::new(modifiers, Key::from_scan(prefix, code))
        })
}

proptest! {
    #[test]
    fn every_chord_reads_back_as_written(chord in any_chord()) {
        let text = chord.to_string();
        prop_assert_eq!(text.parse::<Chord>(), Ok(chord), "{}", text);
        prop_assert_eq!(text.to_lowercase().parse::<Chord>(), Ok(chord), "{}", text);
    }

    #[test]
    fn nothing_typed_into_settings_txt_panics(text in "\\PC{0,40}") {
        let _ = text.parse::<Chord>();
    }
}
