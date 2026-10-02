// Remote control of this PC, the app's side of it. The room decides who may
// control this PC and when, and the injector puts their input on it. What
// sits between the two here are the safeguards that must not wait for the
// panel to draw: the panic key's cut, set on the hotkey thread, and the
// controlled flag behind the settings lock, set on the room's thread before
// the first event goes in and cleared after the last.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;

use room::{ControlEnd, Injected, Injection, Injector, Started};

#[derive(Debug, Default)]
pub struct Flags {
    cut: AtomicBool,
    controlled: AtomicBool,
}

impl Flags {
    // The panic key, on the hotkey thread, before anything else hears it.
    // It takes no lock, so nothing can hold it up.
    pub fn cut(&self) {
        self.cut.store(true, SeqCst);
    }

    // The owner's own Allow, just before the room hears it, is the only
    // thing that clears the cut. A start never does: a panic press that
    // lands between Allow and the start must still hold.
    pub fn allow(&self) {
        self.cut.store(false, SeqCst);
    }

    pub fn is_cut(&self) -> bool {
        self.cut.load(SeqCst)
    }

    pub fn controlled(&self) -> bool {
        self.controlled.load(SeqCst)
    }

    // For the settings lock's tests, which have no room to be controlled
    // through.
    #[cfg(test)]
    pub fn controlled_for_tests() -> Flags {
        Flags {
            cut: AtomicBool::new(false),
            controlled: AtomicBool::new(true),
        }
    }
}

// What the room is given in place of the injector itself.
pub struct Guard {
    inner: Arc<dyn Injector>,
    flags: Arc<Flags>,
    // The hotkeys' switch: while on, injected keys set off none of this PC's
    // own hotkeys, and the owner's own keyboard and mouse are timed.
    hotkeys: Option<input::Remote>,
    // Everything held went up once after the cut, so a key the controller
    // held when the panic key came does not stay down until the room's end
    // arrives a moment later.
    let_go: AtomicBool,
}

impl Guard {
    pub fn new(
        inner: Arc<dyn Injector>,
        flags: Arc<Flags>,
        hotkeys: Option<input::Remote>,
    ) -> Guard {
        Guard {
            inner,
            flags,
            hotkeys,
            let_go: AtomicBool::new(false),
        }
    }
}

impl Injector for Guard {
    fn started(&self, started: &Started) {
        self.flags.controlled.store(true, SeqCst);
        if let Some(remote) = &self.hotkeys {
            remote.set_controlled(true);
        }
        self.let_go.store(false, SeqCst);
        self.inner.started(started);
    }

    fn inject(&self, input: &Injection<'_>) -> Injected {
        if self.flags.is_cut() {
            if !self.let_go.swap(true, SeqCst) {
                self.inner.cut_off();
            }
            return Injected {
                cut: u32::try_from(input.events.len()).unwrap_or(u32::MAX),
                ..Injected::default()
            };
        }
        self.inner.inject(input)
    }

    fn cut_off(&self) {
        self.inner.cut_off();
    }

    fn ended(&self, why: ControlEnd) {
        self.inner.ended(why);
        if let Some(remote) = &self.hotkeys {
            remote.set_controlled(false);
        }
        self.flags.controlled.store(false, SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};

    use room::{Held, InputEvent};

    use super::*;

    // What the injector behind the guard heard, in order: what kind of call,
    // never which key.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Heard {
        Started,
        Input(usize),
        CutOff,
        Ended(ControlEnd),
    }

    #[derive(Default)]
    struct Fake {
        heard: Mutex<Vec<Heard>>,
        // Whether the guard had turned the lock on by the time each call
        // came, which must be before the first event and after the last.
        locked: Mutex<Vec<bool>>,
        flags: Arc<Flags>,
    }

    impl Fake {
        fn note(&self, heard: Heard) {
            let locked = self.flags.controlled();
            self.locked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(locked);
            self.heard
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(heard);
        }

        fn heard(&self) -> Vec<Heard> {
            self.heard
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn locked(&self) -> Vec<bool> {
            self.locked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Injector for Fake {
        fn started(&self, _: &Started) {
            self.note(Heard::Started);
        }

        fn inject(&self, input: &Injection<'_>) -> Injected {
            self.note(Heard::Input(input.events.len()));
            Injected {
                sent: input.events.len() as u32,
                ..Injected::default()
            }
        }

        fn cut_off(&self) {
            self.note(Heard::CutOff);
        }

        fn ended(&self, why: ControlEnd) {
            self.note(Heard::Ended(why));
        }
    }

    fn guarded() -> (Arc<Fake>, Guard, Arc<Flags>) {
        let flags = Arc::new(Flags::default());
        let fake = Arc::new(Fake {
            flags: Arc::clone(&flags),
            ..Fake::default()
        });
        let guard = Guard::new(
            Arc::clone(&fake) as Arc<dyn Injector>,
            Arc::clone(&flags),
            None,
        );
        (fake, guard, flags)
    }

    fn started() -> Started {
        Started {
            name: String::from("Tom"),
            area: None,
        }
    }

    // Made-up input: two moves, nothing held.
    const MOVES: [InputEvent; 2] = [
        InputEvent::Move { dx: 3, dy: -2 },
        InputEvent::Move { dx: 1, dy: 0 },
    ];

    fn inject(guard: &Guard) -> Injected {
        let held = Held::default();
        guard.inject(&Injection {
            events: &MOVES,
            held: &held,
            area: None,
        })
    }

    // The settings lock is on before the injector hears the start and stays
    // on until it has heard the end.
    #[test]
    fn controlled_from_start_to_end() {
        let (fake, guard, flags) = guarded();
        assert!(!flags.controlled());
        guard.started(&started());
        assert!(flags.controlled());
        assert_eq!(inject(&guard).sent, 2);
        guard.cut_off();
        guard.ended(ControlEnd::Stopped);
        assert!(!flags.controlled());
        assert_eq!(
            fake.heard(),
            [
                Heard::Started,
                Heard::Input(2),
                Heard::CutOff,
                Heard::Ended(ControlEnd::Stopped)
            ]
        );
        assert_eq!(fake.locked(), [true, true, true, true]);
    }

    // The panic key's cut drops everything that comes after it, lets go of
    // what was held once, and is undone only by the owner's own Allow: a
    // new start alone does not.
    #[test]
    fn panic_key_cuts_until_allow() {
        let (fake, guard, flags) = guarded();
        flags.allow();
        guard.started(&started());
        assert_eq!(inject(&guard).sent, 2);
        flags.cut();
        let dropped = inject(&guard);
        assert_eq!((dropped.sent, dropped.cut), (0, 2));
        assert_eq!(inject(&guard).cut, 2);
        guard.ended(ControlEnd::Panic);
        assert_eq!(
            fake.heard(),
            [
                Heard::Started,
                Heard::Input(2),
                Heard::CutOff,
                Heard::Ended(ControlEnd::Panic)
            ]
        );

        // Pressed between Allow and the start the room makes of it.
        guard.started(&started());
        assert_eq!(inject(&guard).cut, 2, "a start does not undo the cut");
        guard.ended(ControlEnd::Panic);
        flags.allow();
        guard.started(&started());
        assert_eq!(inject(&guard).sent, 2);
        assert!(!flags.is_cut());
    }
}
