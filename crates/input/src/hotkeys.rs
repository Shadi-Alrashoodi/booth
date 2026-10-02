use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::bindings::{Action, Bindings};
use crate::feed::{FEED_ROOM, Feed, RawMouse, Sink, Tap};
use crate::grab::Grab;
use crate::remote::{Remote, Shared};
use crate::tracker::{Elevation, Event, INJECTED, RawKey, Tracker};
use crate::win;

enum Command {
    Bindings(Bindings),
    Capture(bool),
}

// The global hotkeys: a thread of their own with a hidden window that Raw
// Input delivers every key to, whichever window has focus. Dropping it stops
// the thread.
pub struct Hotkeys {
    commands: Sender<Command>,
    thread: win::Thread,
    remote: Remote,
    feed: Option<Feed>,
}

impl Hotkeys {
    // `handler` runs on the hotkey thread, once per event, in order. Push to
    // talk waits for it, so it must not block: act or hand the event on.
    //
    // `panic` runs there too when a physical keyboard presses the panic
    // key, before `handler` hears Pressed(Panic) and before anything else:
    // it is the cut, and it must take no lock, so nothing can hold it up.
    //
    // Start this after the panel's window exists. A process has one Raw
    // Input registration per device type, the last one made wins, and the
    // window toolkit makes its own for the keyboard when it starts.
    pub fn start(
        bindings: Bindings,
        handler: impl FnMut(Event) + Send + 'static,
        panic: impl FnMut() + Send + 'static,
    ) -> io::Result<Hotkeys> {
        let (commands, waiting) = mpsc::channel();
        let (sender, events) = mpsc::sync_channel(FEED_ROOM);
        let remote = Remote::new();
        let sink = Sink::new(sender, remote.shared(), Arc::new(win::foreground_window));
        let run = Loop::new(
            Tracker::new(bindings),
            Box::new(handler),
            Box::new(panic),
            waiting,
            win::this_process_elevated(),
            remote.shared(),
            sink,
        );
        let thread = win::Thread::spawn(run)?;
        let feed = Feed::new(events, remote.shared());
        Ok(Hotkeys {
            commands,
            thread,
            remote,
            feed: Some(feed),
        })
    }

    // A held action is let go and starts again under the new keys.
    pub fn set_bindings(&self, bindings: Bindings) {
        self.send(Command::Bindings(bindings));
    }

    // While on, the next chord pressed comes back as Event::Chord and Esc as
    // Event::Cancelled, and no binding fires but the panic key.
    pub fn capture(&self, on: bool) {
        self.send(Command::Capture(on));
    }

    // Whether this process's keyboard registration still points at the
    // hotkey window, with the flag that brings keys while other windows have
    // focus. Without it there is no panic key.
    pub fn listening(&self) -> bool {
        self.thread.listening()
    }

    pub fn remote(&self) -> Remote {
        self.remote.clone()
    }

    // The keys and mouse to send while Remote::start_sending is on, for the
    // viewer. There is one feed, so the first call takes it.
    pub fn take_feed(&mut self) -> Option<Feed> {
        self.feed.take()
    }

    fn send(&self, command: Command) {
        // A thread that is gone has dropped its end, and its window with it.
        if self.commands.send(command).is_ok() {
            self.thread.wake();
        }
    }
}

// What the hotkey thread does with what its message loop hands it. The
// window part only turns Windows messages into these calls.
pub(crate) struct Loop {
    tracker: Tracker,
    handler: Box<dyn FnMut(Event) + Send>,
    panic: Box<dyn FnMut() + Send>,
    commands: Receiver<Command>,
    // An elevated process gets keys from elevated windows too, so it never
    // pauses. The panel refuses to run elevated, but this crate does not
    // rely on that.
    elevated_self: bool,
    // When the window in front was last taken for elevated, in the
    // milliseconds since boot that Windows stamps its messages with.
    paused_at: u32,
    // A key arrived while the window in front was taken for elevated, so it
    // is not, and the watch leaves it be until another window comes forward.
    keys_arrive: bool,
    shared: Arc<Shared>,
    tap: Tap,
}

impl Loop {
    fn new(
        tracker: Tracker,
        handler: Box<dyn FnMut(Event) + Send>,
        panic: Box<dyn FnMut() + Send>,
        commands: Receiver<Command>,
        elevated_self: bool,
        shared: Arc<Shared>,
        sink: Sink,
    ) -> Loop {
        Loop {
            tracker,
            handler,
            panic,
            commands,
            elevated_self,
            paused_at: 0,
            keys_arrive: false,
            tap: Tap::new(sink),
            shared,
        }
    }

    // `time` is when Windows took the key in. A key pressed just before an
    // elevated window came forward can still be read after the pause began,
    // since Windows hands out posted messages ahead of input, so only a
    // press stamped after the pause shows that keys still arrive.
    //
    // `hook_took` says the Windows keys hook sent this key to the feed
    // already.
    pub(crate) fn raw(&mut self, raw: RawKey, time: u32, hook_took: bool) {
        self.follow_controlled();
        if self.tracker.controlled() && raw.device != INJECTED {
            self.shared.touch();
        }
        let pressed = raw.key().is_some() && raw.down();
        if self.tracker.paused() && pressed && later(time, self.paused_at) {
            self.keys_arrive = true;
            self.shared.set_paused(None);
            let mut events = vec![Event::KeysArrive];
            events.extend(self.tracker.pause(None));
            self.hand(events);
        }
        let events = self.tracker.raw(raw);
        if events.contains(&Event::Pressed(Action::Panic)) {
            (self.panic)();
            // On the controller's side the same key lets go of control.
            // The viewer clears these too, but a viewer that hangs must not
            // keep this PC's keys, the Windows keys least of all.
            self.shared.release();
        }
        self.tap.key(raw, &events, hook_took);
        self.hand(events);
    }

    // Registered only while controlled or sending. On the controlled PC a
    // physical mouse only says when its owner last moved it; nothing about
    // the event is kept.
    pub(crate) fn mouse(&mut self, raw: RawMouse) {
        self.follow_controlled();
        if self.tracker.controlled() && raw.device != INJECTED {
            self.shared.touch();
        }
        self.tap.mouse(raw);
    }

    // Another window came to the front at `time`.
    pub(crate) fn front(&mut self, front: io::Result<Elevation>, time: u32) {
        self.keys_arrive = false;
        self.judge(front, time);
    }

    // The watch's look while something is held. `front` is None when the
    // input desktop is one this process cannot open: a locked screen or the
    // administrator prompt, where no key reaches it.
    pub(crate) fn watch(&mut self, front: Option<io::Result<Elevation>>, time: u32) {
        match front {
            Some(front) => self.judge(front, time),
            None if self.tracker.watching() => {
                let mut events = self.tracker.blind();
                events.push(Event::Lost);
                self.hand(events);
            }
            None => {}
        }
    }

    // A check that failed (the process ended while it ran) says nothing
    // either way, and the hotkeys stay or come back on: pausing them wrongly
    // costs more than one missed pause.
    fn judge(&mut self, front: io::Result<Elevation>, time: u32) {
        let why = match front {
            Ok(Elevation::Normal) | Err(_) => None,
            Ok(_) if self.elevated_self || self.keys_arrive => None,
            Ok(why) => Some(why),
        };
        if why.is_some() {
            self.paused_at = time;
        }
        self.shared.set_paused(why);
        let events = self.tracker.pause(why);
        self.hand(events);
    }

    pub(crate) fn device_gone(&mut self, device: usize) {
        let events = self.tracker.device_gone(device);
        self.hand(events);
    }

    pub(crate) fn commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            let events = match command {
                Command::Bindings(bindings) => self.tracker.set_bindings(bindings),
                Command::Capture(on) => self.tracker.capture(on),
            };
            self.hand(events);
        }
    }

    pub(crate) fn watching(&self) -> bool {
        self.tracker.watching()
    }

    // What the window part has to ask Windows for: the mouse while
    // controlled or sending, and the Windows keys hook while sending
    // fullscreen.
    pub(crate) fn wants(&mut self) -> (bool, bool) {
        self.follow_controlled();
        (
            self.shared.controlled() || self.shared.sending_period().is_some(),
            self.shared.windows_keys(),
        )
    }

    // The Windows keys hook's own state and sender, as it goes in.
    pub(crate) fn grab(&self) -> (Grab, Sink) {
        let held = self
            .tracker
            .held()
            .filter(|(_, device)| *device != INJECTED)
            .map(|(key, _)| key);
        (Grab::new(*self.tracker.bindings(), held), self.tap.sink())
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    // Control starting or ending also forgets when the owner last touched
    // their keyboard, so nothing of it outlives control.
    fn follow_controlled(&mut self) {
        let on = self.shared.controlled();
        if on != self.tracker.controlled() {
            self.tracker.set_controlled(on);
            self.shared.forget_touch();
        }
    }

    fn hand(&mut self, events: Vec<Event>) {
        for event in events {
            (self.handler)(event);
        }
    }
}

// The tick count wraps after 49.7 days, so the difference decides.
fn later(time: u32, than: u32) -> bool {
    (time.wrapping_sub(than) as i32) > 0
}

#[cfg(test)]
mod tests;
