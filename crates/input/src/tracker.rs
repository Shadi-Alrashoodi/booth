// Which keys are down, worked out from Raw Input's make and break events and
// nothing else. The system's own key state (GetKeyState, GetAsyncKeyState)
// also counts injected input, and remote control needs a state a controller
// cannot write to. Every down key keeps the device it came from for the same
// reason: injected input arrives with device zero.

use crate::bindings::{Action, Bindings};
use crate::key::{Chord, Key, Modifiers};

// The device handle Windows gives input that SendInput made. Observed on
// Windows 10 and 11, not documented, which is why the panic key's first
// check is that Booth's own injector never sends its chord.
pub(crate) const INJECTED: usize = 0;

const RI_KEY_BREAK: u16 = 1;
pub(crate) const RI_KEY_E0: u16 = 2;
const RI_KEY_E1: u16 = 4;
// Raw Input's virtual key for the parts of an escape sequence that are not
// keys of their own, such as the second half of Pause.
const VK_NONE: u16 = 0xFF;
// What a keyboard sends when its buffer overflowed.
const OVERRUN: u16 = 0xFF;

// One keyboard event as Raw Input reports it: RAWKEYBOARD's fields and the
// device handle from its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawKey {
    pub make_code: u16,
    pub flags: u16,
    pub vkey: u16,
    pub device: usize,
}

impl RawKey {
    pub fn press(key: Key, device: usize) -> RawKey {
        RawKey::made_up(key, device, 0)
    }

    pub fn release(key: Key, device: usize) -> RawKey {
        RawKey::made_up(key, device, RI_KEY_BREAK)
    }

    fn made_up(key: Key, device: usize, flags: u16) -> RawKey {
        let prefix = match key.scan() >> 8 {
            0xE0 => RI_KEY_E0,
            0xE1 => RI_KEY_E1,
            _ => 0,
        };
        RawKey {
            make_code: key.scan() & 0xFF,
            flags: flags | prefix,
            vkey: 0,
            device,
        }
    }

    pub fn key(&self) -> Option<Key> {
        let prefix_alone = self.make_code == 0xE0 || self.make_code == 0xE1;
        if self.vkey == VK_NONE || self.make_code == 0 || self.make_code >= OVERRUN || prefix_alone
        {
            return None;
        }
        let code = self.make_code as u8;
        let key = if self.flags & RI_KEY_E1 != 0 {
            // Pause is the only key that sends E1, and Raw Input reports it
            // with either half of its sequence depending on the driver.
            Key::PAUSE
        } else if self.flags & RI_KEY_E0 != 0 {
            Key::from_scan(0xE0, code)
        } else {
            Key::from_scan(0, code)
        };
        // With Num Lock on, the arrows and the six keys above them arrive
        // wrapped in a made-up Shift, E0 2A before and E0 AA after, and some
        // keyboards do the same with E0 36. Neither is a key anyone pressed.
        if key == Key::from_scan(0xE0, 0x2A) || key == Key::from_scan(0xE0, 0x36) {
            return None;
        }
        Some(key)
    }

    pub fn down(&self) -> bool {
        self.flags & RI_KEY_BREAK == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    // A press action, or a held one starting.
    Pressed(Action),
    // A held action ending.
    Released(Action),
    // While settings waits for a new key: the chord that was pressed.
    Chord(Chord),
    // While settings waits for a new key: Esc.
    Cancelled,
    // An administrator window came to the front, so Windows sends this
    // process no keys until it goes. Everything held was let go.
    Paused(Elevation),
    // While paused, a key pressed after the pause began still arrived. An
    // elevated window keeps keys from this process, so the one in front is
    // not elevated after all: another user's program, say, whose rights
    // cannot be read from here. Resumed follows.
    KeysArrive,
    Resumed,
    // Keys went unseen for another reason (a locked or secure screen), and
    // everything held was let go.
    Lost,
}

// What the window in front runs as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    Normal,
    Elevated,
    // Its token could not be read from here, which only a process with more
    // rights than this one refuses.
    Unreadable,
}

pub struct Tracker {
    bindings: Bindings,
    // In the order they went down, with the device each came from.
    down: Vec<(Key, usize)>,
    // What was down when keys stopped reaching this process. Their breaks
    // may have been missed, so they no longer count as down.
    stale: Vec<(Key, usize)>,
    holding: Vec<Action>,
    // Some while settings waits for a new key; the bindings do nothing then.
    capture: Option<Gesture>,
    paused: bool,
    // This PC is being controlled: injected keys are no binding's keys.
    controlled: bool,
}

// One run of keys from the first going down to the last coming up, while
// settings waits for a new key.
#[derive(Default)]
struct Gesture {
    // A chord was read, so letting go of the modifiers reads nothing more.
    done: bool,
    last_modifier: Option<Key>,
}

impl Tracker {
    pub fn new(bindings: Bindings) -> Tracker {
        Tracker {
            bindings,
            down: Vec::new(),
            stale: Vec::new(),
            holding: Vec::new(),
            capture: None,
            paused: false,
            controlled: false,
        }
    }

    pub(crate) fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    // While on, a key from device zero sets off nothing and counts as
    // nobody's modifier, so a key the injector lets through by mistake still
    // cannot press this PC's own hotkeys. Its release still lets go of a key
    // it pressed before control began. Off, injected keys work like any
    // other, for the macro tools some people use; the panic key never
    // listens to them either way.
    pub fn set_controlled(&mut self, on: bool) {
        self.controlled = on;
    }

    pub fn controlled(&self) -> bool {
        self.controlled
    }

    // A held action ends with the bindings it started under.
    pub fn set_bindings(&mut self, bindings: Bindings) -> Vec<Event> {
        let out = self.let_go();
        self.bindings = bindings;
        out
    }

    pub fn capture(&mut self, on: bool) -> Vec<Event> {
        if on == self.capture.is_some() {
            return Vec::new();
        }
        if !on {
            self.capture = None;
            return Vec::new();
        }
        self.capture = Some(Gesture::default());
        self.let_go()
    }

    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    // `why` is Some while an administrator window is in front. Events that
    // still arrive meanwhile are not acted on.
    pub fn pause(&mut self, why: Option<Elevation>) -> Vec<Event> {
        match (why, self.paused) {
            (Some(why), false) => {
                self.paused = true;
                let mut out = self.blind();
                out.push(Event::Paused(why));
                out
            }
            (None, true) => {
                self.paused = false;
                vec![Event::Resumed]
            }
            _ => Vec::new(),
        }
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    // Keys may be going unseen from now on. A held action is let go at once,
    // since its end might never arrive. What is down becomes stale, and the
    // next event starts from a clean state.
    pub fn blind(&mut self) -> Vec<Event> {
        let out = self.let_go();
        self.stale.append(&mut self.down);
        if let Some(gesture) = &mut self.capture {
            *gesture = Gesture::default();
        }
        out
    }

    // Something is down, so a key could get stuck if input stopped now.
    pub fn watching(&self) -> bool {
        !self.down.is_empty()
    }

    pub fn held(&self) -> impl Iterator<Item = (Key, usize)> + '_ {
        self.down.iter().copied()
    }

    pub fn raw(&mut self, raw: RawKey) -> Vec<Event> {
        let mut out = Vec::new();
        let Some(key) = raw.key() else {
            return out;
        };
        if self.paused {
            return out;
        }
        if !self.counts(raw.device) {
            if !raw.down()
                && let Some(at) = self
                    .down
                    .iter()
                    .position(|entry| *entry == (key, raw.device))
            {
                self.down.remove(at);
                self.end_holds(&mut out);
            }
            return out;
        }
        let resumed = self.refresh(key, raw.device);
        if raw.down() {
            self.key_down(key, raw.device, resumed, &mut out);
        } else {
            self.key_up(key, raw.device, &mut out);
        }
        out
    }

    fn counts(&self, device: usize) -> bool {
        !self.controlled || device != INJECTED
    }

    // The first event after keys went unseen. Stale keys are forgotten; the
    // one this event is about, if it was stale on the same device and comes
    // down again, was held all along, and its event is a repeat.
    fn refresh(&mut self, key: Key, device: usize) -> bool {
        let resumed = self.stale.contains(&(key, device));
        self.stale.clear();
        resumed
    }

    fn key_down(&mut self, key: Key, device: usize, resumed: bool, out: &mut Vec<Event>) {
        // A repeat, or the same key on a second keyboard. The panic key asks
        // about physical keyboards only, so a key an injector holds down
        // cannot turn a real press of it into a repeat.
        let physical_before = self
            .down
            .iter()
            .any(|(down, from)| *down == key && *from != INJECTED);
        let already = self
            .down
            .iter()
            .any(|(down, from)| *down == key && self.counts(*from));
        if !self.down.contains(&(key, device)) {
            self.down.push((key, device));
        }
        // First, and even while settings waits for a key: its modifiers too
        // come from physical keyboards only, so injected presses cannot make
        // the chord and injected releases cannot unmake it.
        if device != INJECTED && !physical_before && !resumed {
            let pressed = Chord::new(self.modifiers_without(key, true), key);
            if Action::Panic.fires(self.bindings.chord(Action::Panic), pressed) {
                out.push(Event::Pressed(Action::Panic));
            }
        }
        if already {
            return;
        }
        let modifiers = self.modifiers_without(key, self.controlled);
        if let Some(gesture) = &mut self.capture {
            if resumed {
                return;
            }
            if key == Key::ESC {
                out.push(Event::Cancelled);
                gesture.done = true;
            } else if !key.is_modifier() {
                out.push(Event::Chord(Chord::new(modifiers, key)));
                gesture.done = true;
            } else if !gesture.done {
                gesture.last_modifier = Some(key);
            }
            return;
        }
        for action in Action::ALL {
            let chord = self.bindings.chord(action);
            if chord.key != key || action == Action::Panic {
                continue;
            }
            if action.held() {
                if modifiers.contains(chord.modifiers) && !self.holding.contains(&action) {
                    self.holding.push(action);
                    out.push(Event::Pressed(action));
                }
            } else if !resumed && modifiers == chord.modifiers {
                out.push(Event::Pressed(action));
            }
        }
    }

    fn key_up(&mut self, key: Key, device: usize, out: &mut Vec<Event>) {
        // A break for something not down here: down before Booth started,
        // or from a device other than the one that pressed it. An injected
        // break lands here and cannot let go of a physical press.
        let Some(at) = self.down.iter().position(|entry| *entry == (key, device)) else {
            return;
        };
        // A modifier let go with no other key pressed: the modifier pressed
        // last is the key, the others its modifiers. That is how Right Ctrl
        // alone is read.
        if let Some(gesture) = &mut self.capture
            && key.is_modifier()
            && !gesture.done
            && let Some(last) = gesture.last_modifier
        {
            let others = self
                .down
                .iter()
                .filter(|(down, _)| *down != last)
                .fold(Modifiers::NONE, |all, (down, _)| all.with(down.modifier()));
            out.push(Event::Chord(Chord::new(others, last)));
            gesture.done = true;
        }
        self.down.remove(at);
        if self.down.is_empty()
            && let Some(gesture) = &mut self.capture
        {
            *gesture = Gesture::default();
        }
        self.end_holds(out);
    }

    // A keyboard was unplugged, or its radio link dropped, with keys down.
    // Their breaks will never come, and a key of the same name from another
    // device cannot end them either (key_up says why), so they go here.
    pub fn device_gone(&mut self, device: usize) -> Vec<Event> {
        let mut out = Vec::new();
        self.stale.retain(|(_, from)| *from != device);
        let before = self.down.len();
        self.down.retain(|(_, from)| *from != device);
        if self.down.len() == before {
            return out;
        }
        if let Some(gesture) = &mut self.capture {
            *gesture = Gesture::default();
        }
        self.end_holds(&mut out);
        out
    }

    fn end_holds(&mut self, out: &mut Vec<Event>) {
        let mut still = Vec::with_capacity(self.holding.len());
        for action in std::mem::take(&mut self.holding) {
            if self.chord_down(self.bindings.chord(action)) {
                still.push(action);
            } else {
                out.push(Event::Released(action));
            }
        }
        self.holding = still;
    }

    // The chord's key is down with at least its modifiers.
    fn chord_down(&self, chord: Chord) -> bool {
        self.down
            .iter()
            .any(|(down, from)| *down == chord.key && self.counts(*from))
            && self
                .modifiers_without(chord.key, self.controlled)
                .contains(chord.modifiers)
    }

    // The modifiers held, not counting `key` itself, so Right Ctrl alone is
    // Right Ctrl with no modifiers. `physical` leaves injected ones out.
    fn modifiers_without(&self, key: Key, physical: bool) -> Modifiers {
        self.down
            .iter()
            .filter(|(down, from)| *down != key && (!physical || *from != INJECTED))
            .fold(Modifiers::NONE, |all, (down, _)| all.with(down.modifier()))
    }

    fn let_go(&mut self) -> Vec<Event> {
        self.holding.drain(..).map(Event::Released).collect()
    }
}
