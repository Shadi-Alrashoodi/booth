use crate::key::{Chord, Key, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    PushToTalk,
    Mute,
    Deafen,
    Share,
    // Cuts remote control on the PC being controlled, and lets go of it on
    // the controller's. Only a physical keyboard presses it (tracker.rs).
    Panic,
    ShowPanel,
    StatsPanel,
}

impl Action {
    // In the order settings lists them.
    pub const ALL: [Action; 7] = [
        Action::PushToTalk,
        Action::Mute,
        Action::Deafen,
        Action::Share,
        Action::Panic,
        Action::ShowPanel,
        Action::StatsPanel,
    ];

    // Push to talk lasts as long as its chord is held. The others happen once
    // per press.
    pub fn held(self) -> bool {
        self == Action::PushToTalk
    }

    // Also set off with more modifiers down than the chord's own. A player
    // holding Shift to run still has to be heard, and someone reaching for
    // the panic key in a hurry should not have to let go of anything first.
    pub fn loose(self) -> bool {
        self.held() || self == Action::Panic
    }

    // As the log and the settings sentences write it.
    pub fn name(self) -> &'static str {
        match self {
            Action::PushToTalk => "push to talk",
            Action::Mute => "mute",
            Action::Deafen => "deafen",
            Action::Share => "share (press twice) or stop sharing",
            Action::Panic => "panic key",
            Action::ShowPanel => "show or hide the panel",
            Action::StatsPanel => "stats panel",
        }
    }

    pub fn default_chord(self) -> Chord {
        let ctrl_shift = Modifiers::CTRL.with(Modifiers::SHIFT);
        match self {
            Action::PushToTalk => Chord::new(Modifiers::NONE, Key::RIGHT_CTRL),
            Action::Mute => Chord::new(ctrl_shift, Key::M),
            Action::Deafen => Chord::new(ctrl_shift, Key::D),
            Action::Share => Chord::new(ctrl_shift, Key::S),
            Action::Panic => Chord::new(ctrl_shift, Key::END),
            Action::ShowPanel => Chord::new(ctrl_shift, Key::SPACE),
            Action::StatsPanel => Chord::new(ctrl_shift, Key::I),
        }
    }

    // A press of `pressed` sets this action off when bound to `bound`.
    pub(crate) fn fires(self, bound: Chord, pressed: Chord) -> bool {
        bound.key == pressed.key
            && if self.loose() {
                pressed.modifiers.contains(bound.modifiers)
            } else {
                pressed.modifiers == bound.modifiers
            }
    }

    fn index(self) -> usize {
        match self {
            Action::PushToTalk => 0,
            Action::Mute => 1,
            Action::Deafen => 2,
            Action::Share => 3,
            Action::Panic => 4,
            Action::ShowPanel => 5,
            Action::StatsPanel => 6,
        }
    }
}

// One chord per action, always: nothing can be left without a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bindings {
    chords: [Chord; Action::ALL.len()],
}

impl Default for Bindings {
    fn default() -> Bindings {
        Bindings {
            chords: Action::ALL.map(Action::default_chord),
        }
    }
}

impl Bindings {
    pub fn chord(&self, action: Action) -> Chord {
        self.chords[action.index()]
    }

    pub fn set(&mut self, action: Action, chord: Chord) {
        self.chords[action.index()] = chord;
    }

    // The other action one press of `chord` would also set off if it were
    // bound to `action`.
    pub fn conflict(&self, action: Action, chord: Chord) -> Option<Action> {
        Action::ALL
            .into_iter()
            .filter(|other| *other != action)
            .find(|other| clash((action, chord), (*other, self.chord(*other))))
    }

    // Two actions that one press would set off together, if any. Booth never
    // saves that, but settings written by hand can hold it, and so can a key
    // saved before a newer version made it a new action's default.
    pub fn first_conflict(&self) -> Option<(Action, Action)> {
        Action::ALL.into_iter().find_map(|action| {
            self.conflict(action, self.chord(action))
                .map(|other| (action, other))
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (Action, Chord)> + '_ {
        Action::ALL
            .into_iter()
            .map(|action| (action, self.chord(action)))
    }

    pub(crate) fn any_fires(&self, pressed: Chord) -> bool {
        self.iter()
            .any(|(action, bound)| action.fires(bound, pressed))
    }
}

// A press fires on exactly its modifiers, so Ctrl+Shift+M and Ctrl+M are
// two keys. A loose action also runs with more modifiers down than its own,
// so it takes every chord on its key that includes its modifiers.
fn clash(a: (Action, Chord), b: (Action, Chord)) -> bool {
    let ((a_action, a), (b_action, b)) = (a, b);
    if a.key != b.key {
        return false;
    }
    a.modifiers == b.modifiers
        || (a_action.loose() && b.modifiers.contains(a.modifiers))
        || (b_action.loose() && a.modifiers.contains(b.modifiers))
}
