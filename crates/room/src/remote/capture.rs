// The controller's side of control, joined up: the viewer says when capture
// may run, the app switches the input crate's feed on and off, and both hand
// their input to Controls. The viewer's switch goes through here, so it
// starts the app's capture only while the room says this PC controls, and
// each stop sends the held state with nothing held, so the other PC lets go
// of everything at once.

use std::sync::Arc;
use std::time::Instant;

use share::{Capturing, ControlOut, MouseButton, Pointing};

use super::send::{Aim, Controls};
use super::{Button, InputEvent, ScanCode};

// The default binding of the release key, for a room the app gave no
// capture: the strip still names a key.
const DEFAULT_RELEASE: &str = "Ctrl+Shift+End";

// What the app gives the room to read this PC's keys and mouse for a PC it
// controls: the real one switches the input crate's feed, and the app hands
// what the feed reads to Controls::captured. Calls come from the viewer's
// window thread and must return at once.
pub trait Capture: Send + Sync {
    // On for the viewer's window `window`, which the feed checks is in
    // front at every event, with the Windows keys when `how.windows_keys`;
    // or off, with None.
    fn capture(&self, window: isize, how: Option<Capturing>);
    // The panic key's binding as the strip prints it, "Ctrl+Shift+End": on
    // this side of control it is the release key.
    fn release_key(&self) -> String;
}

// The viewer's way out, as the room's watch thread opens it.
pub(crate) struct ViewerInput {
    controls: Arc<Controls>,
    capture: Option<Arc<dyn Capture>>,
}

impl ViewerInput {
    pub(crate) fn new(controls: Arc<Controls>, capture: Option<Arc<dyn Capture>>) -> ViewerInput {
        ViewerInput { controls, capture }
    }
}

impl ControlOut for ViewerInput {
    fn capture(&self, window: isize, how: Option<Capturing>) {
        let aim = how.map(|how| {
            if how.relative {
                Aim::Motion
            } else {
                Aim::Points
            }
        });
        // Set before the app's feed starts, so its first key finds the
        // capture on; refused when control is over.
        if aim.is_some() {
            let (_, running) = self.controls.set_capture(aim);
            if running {
                if let Some(capture) = &self.capture {
                    capture.capture(window, how);
                }
                return;
            }
        }
        // The app's feed stops first, so nothing it read after the stop
        // finds the capture still on.
        if let Some(capture) = &self.capture {
            capture.capture(window, None);
        }
        let (ran, _) = self.controls.set_capture(None);
        if ran {
            self.controls.let_go();
        }
    }

    fn point(&self, pointing: Pointing, at: Instant) {
        self.controls.pointed(&[pointed(pointing)], at);
    }

    fn release_key(&self) -> String {
        self.capture.as_ref().map_or_else(
            || DEFAULT_RELEASE.to_string(),
            |capture| capture.release_key(),
        )
    }
}

fn pointed(pointing: Pointing) -> InputEvent {
    match pointing {
        Pointing::At { x, y } => InputEvent::At { x, y },
        Pointing::Button { button, down } => InputEvent::Button {
            button: match button {
                MouseButton::Left => Button::Left,
                MouseButton::Right => Button::Right,
                MouseButton::Middle => Button::Middle,
                MouseButton::Back => Button::Back,
                MouseButton::Forward => Button::Forward,
            },
            down,
        },
        Pointing::Wheel { delta } => InputEvent::Wheel { delta },
        Pointing::HWheel { delta } => InputEvent::HWheel { delta },
    }
}

impl ScanCode {
    // A key as the input crate names it (input::Key::scan): the make code
    // with its prefix byte above it. Pause, the one key with E1, has no
    // place on the wire and stays on this PC.
    pub fn from_scan(scan: u16) -> Option<ScanCode> {
        let code = scan as u8;
        let e0 = match scan >> 8 {
            0 => false,
            0xE0 => true,
            _ => return None,
        };
        ScanCode::possible(code).then_some(ScanCode { code, e0 })
    }
}

// One raw mouse report from the input crate's feed (input::Mouse: the
// buttons as bits, left first, as Button::ALL has them), as the events it
// makes, in the order they happened: the motion, the presses, the
// releases, then the wheels. Appended to `into`, which the caller reuses.
pub fn mouse_report(
    dx: i32,
    dy: i32,
    pressed: u8,
    released: u8,
    wheel: i16,
    hwheel: i16,
    into: &mut Vec<InputEvent>,
) {
    if dx != 0 || dy != 0 {
        into.push(InputEvent::Move { dx, dy });
    }
    for (bits, down) in [(pressed, true), (released, false)] {
        for button in Button::ALL {
            if bits & (1 << button.index()) != 0 {
                into.push(InputEvent::Button { button, down });
            }
        }
    }
    if wheel != 0 {
        into.push(InputEvent::Wheel {
            delta: i32::from(wheel),
        });
    }
    if hwheel != 0 {
        into.push(InputEvent::HWheel {
            delta: i32::from(hwheel),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use net::pace::Signal;

    use super::*;
    use crate::peer::Clock;
    use crate::remote::send::tests::{last, link_and_reader, sent};

    #[derive(Default)]
    struct App {
        told: Mutex<Vec<Option<Capturing>>>,
    }

    impl Capture for App {
        fn capture(&self, _window: isize, how: Option<Capturing>) {
            self.told.lock().unwrap().push(how);
        }

        fn release_key(&self) -> String {
            String::from("Ctrl+Alt+Scroll Lock")
        }
    }

    impl App {
        fn take(&self) -> Vec<Option<Capturing>> {
            std::mem::take(&mut self.told.lock().unwrap())
        }
    }

    const WINDOW: isize = 0x0003_0C4A;
    const A: ScanCode = ScanCode {
        code: 0x1E,
        e0: false,
    };

    fn key(down: bool) -> InputEvent {
        InputEvent::Key { key: A, down }
    }

    fn left(down: bool) -> InputEvent {
        InputEvent::Button {
            button: Button::Left,
            down,
        }
    }

    fn rig() -> (Arc<Controls>, Arc<App>, ViewerInput) {
        let controls = Controls::new(
            Clock::new(Instant::now()),
            None,
            Signal::new().expect("an event"),
        );
        let app = Arc::new(App::default());
        let input = ViewerInput::new(
            Arc::clone(&controls),
            Some(Arc::clone(&app) as Arc<dyn Capture>),
        );
        (controls, app, input)
    }

    const DESKTOP: Capturing = Capturing {
        windows_keys: false,
        relative: false,
    };
    const GAME: Capturing = Capturing {
        windows_keys: true,
        relative: true,
    };

    // The viewer's switch reaches the app only while this PC controls, and
    // until it does nothing the feed or the viewer hand over goes anywhere.
    #[test]
    fn capture_only_while_controlling() {
        let (controls, app, input) = rig();
        input.capture(WINDOW, Some(DESKTOP));
        assert_eq!(app.take(), [None], "started with no control running");
        controls.captured(&[key(true)], Instant::now());
        assert_eq!(sent(&controls), 0);

        let (link, mut reader) = link_and_reader();
        controls.set_link(Some(link));
        controls.captured(&[key(true)], Instant::now());
        input.point(Pointing::At { x: 1, y: 2 }, Instant::now());
        assert_eq!(sent(&controls), 0, "nothing before the viewer captures");

        input.capture(WINDOW, Some(DESKTOP));
        assert_eq!(app.take(), [Some(DESKTOP)]);
        controls.captured(&[key(true)], Instant::now());
        assert_eq!(sent(&controls), 1);
        // The stop: the app's feed stops, and a packet with nothing held
        // goes at once.
        input.capture(WINDOW, None);
        assert_eq!(app.take(), [None]);
        assert_eq!(sent(&controls), 2);
        let (events, held) = last(&controls, &mut reader);
        assert!(events.is_empty() && held.is_empty());
        controls.captured(&[key(true)], Instant::now());
        assert_eq!(sent(&controls), 2);

        // Control ends while the viewer still captures: the next switch the
        // viewer makes stops the app's feed rather than starting it.
        input.capture(WINDOW, Some(GAME));
        assert_eq!(app.take(), [Some(GAME)]);
        controls.set_link(None);
        input.capture(WINDOW, Some(DESKTOP));
        assert_eq!(app.take(), [None]);
        assert_eq!(input.release_key(), "Ctrl+Alt+Scroll Lock");
    }

    // Points and clicks come from the viewer in absolute mode and from the
    // feed in relative mode, never both; a click that went down from one
    // comes up from either.
    #[test]
    fn mouse_from_one_side() {
        let (controls, _app, input) = rig();
        let (link, mut reader) = link_and_reader();
        controls.set_link(Some(link));
        input.capture(WINDOW, Some(DESKTOP));
        let now = Instant::now();
        controls.captured(&[left(true), InputEvent::Move { dx: 3, dy: 1 }], now);
        assert_eq!(sent(&controls), 0, "the feed's mouse in absolute mode");
        input.point(Pointing::At { x: 10, y: 20 }, now);
        input.point(
            Pointing::Button {
                button: MouseButton::Left,
                down: true,
            },
            now,
        );
        let (events, held) = last(&controls, &mut reader);
        assert_eq!(events, [left(true)]);
        assert!(held.button(Button::Left));

        // A game hides the pointer: the up comes from the feed now.
        input.capture(WINDOW, Some(GAME));
        input.point(Pointing::At { x: 11, y: 21 }, now);
        controls.captured(&[left(false)], now);
        let (events, held) = last(&controls, &mut reader);
        assert_eq!(events, [left(false)]);
        assert!(held.is_empty());
        controls.captured(&[InputEvent::Move { dx: 3, dy: 1 }, key(true)], now);
        let (events, _) = last(&controls, &mut reader);
        assert_eq!(events, [InputEvent::Move { dx: 3, dy: 1 }, key(true)]);
        // The viewer never sends keys, and an up for a button not held is
        // nobody's.
        input.point(
            Pointing::Button {
                button: MouseButton::Right,
                down: false,
            },
            now,
        );
        let before = sent(&controls);
        controls.pointed(&[key(false)], now);
        assert_eq!(sent(&controls), before);
    }

    // What the viewer's window thread spends on a click: from the viewer
    // handing it over to a sealed packet, with a real session and no socket.
    // Each click goes at once, so each is a packet.
    #[test]
    fn click_sealed_under_a_millisecond() {
        let (controls, _app, input) = rig();
        let (link, _reader) = link_and_reader();
        controls.set_link(Some(link));
        input.capture(WINDOW, Some(DESKTOP));
        let mut took = Vec::with_capacity(2000);
        for n in 0..2000u32 {
            let pointing = Pointing::Button {
                button: MouseButton::Left,
                down: n % 2 == 0,
            };
            let at = Instant::now();
            input.point(pointing, at);
            took.push(at.elapsed().as_secs_f64() * 1e6);
        }
        assert_eq!(sent(&controls), 2000);
        took.sort_by(f64::total_cmp);
        let (median, p99) = (took[1000], took[1980]);
        println!("a click to a sealed packet: median {median:.1} us, p99 {p99:.1} us");
        assert!(median < 1000.0, "median {median:.1} us");
    }

    #[test]
    fn scan_codes_and_pause() {
        assert_eq!(ScanCode::from_scan(0x001E), Some(A));
        assert_eq!(
            ScanCode::from_scan(0xE01D),
            Some(ScanCode {
                code: 0x1D,
                e0: true
            }),
            "right ctrl"
        );
        assert_eq!(ScanCode::from_scan(0xE11D), None, "pause");
        assert_eq!(ScanCode::from_scan(0x00FF), None, "a keyboard overrun");
        assert_eq!(ScanCode::from_scan(0x0000), None);
    }

    #[test]
    fn mouse_report_order() {
        let mut events = Vec::new();
        mouse_report(4, -2, 1 | 16, 2, 120, 0, &mut events);
        assert_eq!(
            events,
            [
                InputEvent::Move { dx: 4, dy: -2 },
                left(true),
                InputEvent::Button {
                    button: Button::Forward,
                    down: true
                },
                InputEvent::Button {
                    button: Button::Right,
                    down: false
                },
                InputEvent::Wheel { delta: 120 },
            ]
        );
        events.clear();
        mouse_report(0, 0, 0, 0, 0, -240, &mut events);
        assert_eq!(events, [InputEvent::HWheel { delta: -240 }]);
        events.clear();
        mouse_report(0, 0, 0, 0, 0, 0, &mut events);
        assert!(events.is_empty());
    }
}
