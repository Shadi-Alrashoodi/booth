// The global hotkeys as the panel uses them. Push to talk, mute, deafen and
// the panic key act on the room from the hotkey thread itself: while the
// panel is minimized, eframe runs it at most ten times a second, and neither
// push to talk nor the panic key can wait for that. The rest comes to the
// panel through a channel.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui::Context;
use input::{Action, Bindings, Elevation, Event};
use room::Room;

use crate::remote::Flags;

// Starting a share by key takes two presses within a second. Ctrl+Shift+S
// is Save As in most editors, and the keys are heard whatever has the focus.
const SECOND_PRESS_WITHIN: Duration = Duration::from_secs(1);

// The room the hotkeys act on, shared with the hotkey thread. Hold to talk
// on the panel goes through it too, so the room is told the one answer to
// "is either of them held".
#[derive(Clone, Default)]
pub struct Target(Arc<Mutex<Aim>>);

#[derive(Default)]
struct Aim {
    room: Option<Arc<Room>>,
    key: bool,
    button: bool,
}

impl Target {
    pub fn enter(&self, room: Arc<Room>) {
        *self.lock() = Aim {
            room: Some(room),
            key: false,
            button: false,
        };
    }

    // After this the hotkey thread holds no copy of the room, so the panel's
    // is the last and the room can be left.
    pub fn leave(&self) {
        *self.lock() = Aim::default();
    }

    pub fn key(&self, held: bool) {
        let mut aim = self.lock();
        aim.key = held;
        aim.talk();
    }

    pub fn button(&self, held: bool) {
        let mut aim = self.lock();
        aim.button = held;
        aim.talk();
    }

    // The same as pressing the Mute button: it reads what the room says now.
    pub fn toggle_mute(&self) {
        if let Some(room) = &self.lock().room {
            room.mute(!room.view().voice.muted);
        }
    }

    pub fn toggle_deafen(&self) {
        if let Some(room) = &self.lock().room {
            room.deafen(!room.view().voice.deafened);
        }
    }

    // After the cut: control of this PC ends as the panic key, and on the
    // controller's side control of another PC is let go.
    pub fn panic_key(&self) {
        if let Some(room) = &self.lock().room {
            room.panic_key();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Aim> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Aim {
    fn talk(&self) {
        if let Some(room) = &self.room {
            room.talk(self.key || self.button);
        }
    }
}

// Leaves a room the hotkeys may have held too. `target.leave()` comes first.
pub fn leave(room: Arc<Room>) {
    // Nothing else holds it once the target let go; if something did, the
    // room would stop when that copy goes instead.
    if let Ok(room) = Arc::try_unwrap(room) {
        room.leave();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub running: bool,
    pub paused: bool,
}

impl State {
    // Hold to talk shows only while no hotkey can do its job.
    pub fn button_needed(self) -> bool {
        !self.running || self.paused
    }

    // The global keys hear every press, the panel's own included, so the
    // panel's in-window stats key is for when they cannot.
    pub fn live(self) -> bool {
        self.running && !self.paused
    }
}

// The share key as it counts presses. Stopping takes one press; starting
// takes a second one within SECOND_PRESS_WITHIN of the first, and the first
// alone does nothing anyone can see.
#[derive(Debug, Default)]
pub struct ShareKey {
    first: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareStep {
    Start,
    Stop,
}

impl ShareKey {
    // `sharing` is this PC's share as the room has it now, asked for or
    // running.
    pub fn press(&mut self, at: Instant, sharing: bool) -> Option<ShareStep> {
        if sharing {
            self.first = None;
            return Some(ShareStep::Stop);
        }
        match self.first.take() {
            Some(first) if at.saturating_duration_since(first) <= SECOND_PRESS_WITHIN => {
                Some(ShareStep::Start)
            }
            _ => {
                self.first = Some(at);
                None
            }
        }
    }

    // Every frame, with the room's view. A share seen running ends a first
    // press, so one started by the button and stopped again leaves nothing
    // behind for the next press to finish.
    pub fn follow(&mut self, sharing: bool) {
        if sharing {
            self.first = None;
        }
    }
}

pub struct Hotkeys {
    running: Option<input::Hotkeys>,
    // Each with when the hotkey thread heard it: while the panel is
    // minimized it looks only a few times a second, and the share key's
    // second press is timed from the key, not from when the panel looked.
    heard: Receiver<(Event, Instant)>,
    // Why they could not start: said in settings, and in the log.
    failed: Option<String>,
    paused: bool,
    capturing: bool,
    // While this PC is controlled the bindings take no change and no key is
    // read for settings, whoever asks, so the controller cannot move the
    // panic key.
    flags: Arc<Flags>,
}

impl Hotkeys {
    // Call once the panel's window exists; input::Hotkeys says why. The
    // panic key sets the cut in `flags` before anything else happens, and
    // the room hears of it straight after, on the same thread.
    pub fn start(bindings: Bindings, target: Target, ctx: Context, flags: Arc<Flags>) -> Hotkeys {
        let (tell, heard) = mpsc::channel();
        let handler = move |event: Event| {
            let at = Instant::now();
            match event {
                Event::Pressed(Action::PushToTalk) => target.key(true),
                Event::Released(Action::PushToTalk) => target.key(false),
                Event::Pressed(Action::Mute) => target.toggle_mute(),
                Event::Pressed(Action::Deafen) => target.toggle_deafen(),
                Event::Pressed(Action::Panic) => target.panic_key(),
                _ => {}
            }
            let _ = tell.send((event, at));
            ctx.request_repaint();
        };
        let cut = Arc::clone(&flags);
        let panic = move || cut.cut();
        let (running, failed) = match input::Hotkeys::start(bindings, handler, panic) {
            Ok(running) => (Some(running), None),
            Err(err) => (None, Some(err.to_string())),
        };
        Hotkeys {
            running,
            heard,
            failed,
            paused: false,
            capturing: false,
            flags,
        }
    }

    // For a panel that can open no room. A second copy turned away is one,
    // and its keys would answer the first copy's presses.
    pub fn off() -> Hotkeys {
        let (_, heard) = mpsc::channel();
        Hotkeys {
            running: None,
            heard,
            failed: None,
            paused: false,
            capturing: false,
            flags: Arc::default(),
        }
    }

    // The switch the injector's guard turns while this PC is controlled.
    // None without hotkeys, and then there is no panic key either.
    pub fn remote(&self) -> Option<input::Remote> {
        self.running.as_ref().map(input::Hotkeys::remote)
    }

    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    pub fn state(&self) -> State {
        State {
            running: self.running.is_some(),
            paused: self.paused,
        }
    }

    pub fn heard(&mut self) -> Vec<(Event, Instant)> {
        let events: Vec<(Event, Instant)> = self.heard.try_iter().collect();
        for (event, _) in &events {
            match event {
                Event::Paused(_) => self.paused = true,
                Event::Resumed => self.paused = false,
                _ => {}
            }
        }
        events
    }

    // Settings waits for a new key, and the hotkeys do nothing meanwhile.
    // Not while this PC is controlled: mute and deafen would go deaf, and a
    // key read then is one to refuse anyway.
    pub fn capture(&mut self, on: bool) {
        let on = on && !self.flags.controlled();
        if on == self.capturing {
            return;
        }
        self.capturing = on;
        if let Some(running) = &self.running {
            running.capture(on);
        }
    }

    pub fn capturing(&self) -> bool {
        self.capturing
    }

    // False when refused because this PC is controlled.
    pub fn set_bindings(&self, bindings: Bindings) -> bool {
        if self.flags.controlled() {
            return false;
        }
        if let Some(running) = &self.running {
            running.set_bindings(bindings);
        }
        true
    }
}

// One log line per action by name while a room is open, and per pause at any
// time; key presses themselves never go in the log. Outside a room lines wait
// in memory for the next one, where a game that uses the push to talk key for
// something else would pile up thousands.
pub fn log_line(event: &Event, in_room: bool) -> Option<String> {
    if matches!(event, Event::Pressed(_) | Event::Released(_)) && !in_room {
        return None;
    }
    Some(match event {
        Event::Pressed(action) if action.held() => format!("hotkeys: {} down", action.name()),
        Event::Released(action) => format!("hotkeys: {} up", action.name()),
        Event::Pressed(action) => format!("hotkeys: {}", action.name()),
        Event::Paused(Elevation::Unreadable) => String::from(
            "hotkeys: paused, the window in front belongs to a process whose rights this one cannot read, taken as administrator",
        ),
        Event::Paused(_) => {
            String::from("hotkeys: paused, the window in front runs as administrator")
        }
        Event::KeysArrive => String::from(
            "hotkeys: a key arrived with that window in front, so it does not run as administrator after all",
        ),
        Event::Resumed => String::from("hotkeys: resumed"),
        Event::Lost => String::from(
            "hotkeys: keys stopped arriving (a locked screen or an administrator prompt), so held keys were let go",
        ),
        Event::Chord(_) | Event::Cancelled => return None,
    })
}

pub fn bindings_line(bindings: &Bindings) -> String {
    let each: Vec<String> = bindings
        .iter()
        .map(|(action, chord)| format!("{} {chord}", action.name()))
        .collect();
    format!("hotkeys: {}", each.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_to_talk_needed() {
        let live = State {
            running: true,
            paused: false,
        };
        assert!(!live.button_needed());
        assert!(live.live());
        let paused = State {
            paused: true,
            ..live
        };
        assert!(paused.button_needed());
        assert!(!paused.live());
        let off = State::default();
        assert!(off.button_needed());
        assert!(!off.live());
    }

    #[test]
    fn the_log_names_actions_and_never_keys() {
        assert_eq!(
            log_line(&Event::Pressed(Action::PushToTalk), true).as_deref(),
            Some("hotkeys: push to talk down")
        );
        assert_eq!(
            log_line(&Event::Released(Action::PushToTalk), true).as_deref(),
            Some("hotkeys: push to talk up")
        );
        assert_eq!(
            log_line(&Event::Pressed(Action::ShowPanel), true).as_deref(),
            Some("hotkeys: show or hide the panel")
        );
        assert_eq!(
            log_line(&Event::Paused(Elevation::Elevated), true).as_deref(),
            Some("hotkeys: paused, the window in front runs as administrator")
        );
        assert_eq!(
            log_line(&Event::Resumed, true).as_deref(),
            Some("hotkeys: resumed")
        );
        let chord = Bindings::default().chord(Action::Mute);
        assert_eq!(log_line(&Event::Chord(chord), true), None);
        assert_eq!(log_line(&Event::Cancelled, true), None);
    }

    // Before the first room, or between rooms, lines wait in memory. The
    // actions stay out of them; a pause is rare and says why keys go dead.
    #[test]
    fn outside_a_room_only_pauses_are_logged() {
        for action in Action::ALL {
            assert_eq!(log_line(&Event::Pressed(action), false), None, "{action:?}");
            assert_eq!(
                log_line(&Event::Released(action), false),
                None,
                "{action:?}"
            );
        }
        for event in [
            Event::Paused(Elevation::Unreadable),
            Event::KeysArrive,
            Event::Resumed,
            Event::Lost,
        ] {
            assert!(log_line(&event, false).is_some(), "{event:?}");
        }
    }

    // A panel that can open no room listens to nothing and hears nothing.
    #[test]
    fn hotkeys_that_are_off_hear_nothing() {
        let mut off = Hotkeys::off();
        assert_eq!(off.state(), State::default());
        assert_eq!(off.failed(), None);
        assert!(off.heard().is_empty());
        off.capture(true);
        off.set_bindings(Bindings::default());
        assert!(off.heard().is_empty());
    }

    // While this PC is controlled the bindings take no change and no key is
    // read for settings, whoever asks.
    #[test]
    fn while_controlled_the_bindings_stay_as_they_are() {
        let mut keys = Hotkeys::off();
        assert!(keys.set_bindings(Bindings::default()));
        keys.capture(true);
        assert!(keys.capturing());
        keys.capture(false);

        keys.flags = Arc::new(Flags::controlled_for_tests());
        let mut moved = Bindings::default();
        moved.set(Action::Panic, "F9".parse().unwrap());
        assert!(!keys.set_bindings(moved));
        keys.capture(true);
        assert!(!keys.capturing());
    }

    #[test]
    fn the_bindings_line_lists_every_action() {
        assert_eq!(
            bindings_line(&Bindings::default()),
            "hotkeys: push to talk Right Ctrl, mute Ctrl+Shift+M, deafen Ctrl+Shift+D, share (press twice) or stop sharing Ctrl+Shift+S, panic key Ctrl+Shift+End, show or hide the panel Ctrl+Shift+Space, stats panel Ctrl+Shift+I"
        );
    }

    fn after(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn second_press_starts_a_share() {
        let t = Instant::now();
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, false), None);
        assert_eq!(key.press(after(t, 900), false), Some(ShareStep::Start));
        // Used up: a third press is a first one again.
        assert_eq!(key.press(after(t, 1_000), false), None);
        assert_eq!(
            key.press(after(t, 1_000) + SECOND_PRESS_WITHIN, false),
            Some(ShareStep::Start)
        );
    }

    #[test]
    fn presses_more_than_a_second_apart_start_nothing() {
        let t = Instant::now();
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, false), None);
        assert_eq!(key.press(after(t, 1_100), false), None);
        assert_eq!(key.press(after(t, 2_200), false), None);
        assert_eq!(key.press(after(t, 5_000), false), None);
        // The late one counts as a first press.
        assert_eq!(key.press(after(t, 5_600), false), Some(ShareStep::Start));
    }

    // Stopping takes one press, and that press is no first one.
    #[test]
    fn a_press_while_sharing_stops_at_once() {
        let t = Instant::now();
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, true), Some(ShareStep::Stop));
        assert_eq!(key.press(after(t, 300), false), None);
        assert_eq!(key.press(after(t, 600), false), Some(ShareStep::Start));
        // Asked for and not answered yet counts as sharing.
        assert_eq!(key.press(after(t, 700), true), Some(ShareStep::Stop));
    }

    #[test]
    fn stop_clears_a_first_press() {
        let t = Instant::now();
        // Share pressed after the key, then the key stops it.
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, false), None);
        key.follow(true);
        assert_eq!(key.press(after(t, 300), true), Some(ShareStep::Stop));
        key.follow(false);
        assert_eq!(key.press(after(t, 600), false), None);

        // Share pressed after the key, then Stop sharing: the share came
        // and went between two presses of the key.
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, false), None);
        key.follow(true);
        key.follow(false);
        assert_eq!(key.press(after(t, 500), false), None);

        // A stop by key right after a first press, when the room had not
        // yet said the share was running.
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, false), None);
        assert_eq!(key.press(after(t, 200), true), Some(ShareStep::Stop));
        assert_eq!(key.press(after(t, 400), false), None);

        // Nothing running: following changes nothing.
        let mut key = ShareKey::default();
        assert_eq!(key.press(t, false), None);
        key.follow(false);
        assert_eq!(key.press(after(t, 900), false), Some(ShareStep::Start));
    }

    // No room: the keys do nothing, and nothing panics.
    #[test]
    fn a_target_without_a_room_does_nothing() {
        let target = Target::default();
        target.key(true);
        target.button(true);
        target.toggle_mute();
        target.toggle_deafen();
        target.leave();
    }
}
