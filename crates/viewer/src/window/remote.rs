// Remote control on the viewer's window thread (control.rs has the rules):
// capture follows the focus, the window's state and the mode the present
// thread sets; in absolute mode the mouse messages go to the sharer as
// points; in relative mode this PC's pointer is held in the window, hidden.

use std::cell::RefCell;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use super::{Shared, in_strip};
use crate::control::{self, Capturing, Hand, MouseButton, MouseMode};

thread_local! {
    static CONTROL: RefCell<Here> = RefCell::default();
}

// From WM_ACTIVATE and WM_ACTIVATEAPP.
pub(super) fn set_active(active: bool) {
    CONTROL.with(|cell| {
        if let Ok(mut here) = cell.try_borrow_mut() {
            here.active = active;
        }
    });
}

#[cfg(test)]
pub(super) fn pretend_front(front: bool) {
    CONTROL.with(|cell| {
        if let Ok(mut here) = cell.try_borrow_mut() {
            here.pretend_front = Some(front);
        }
    });
}

// The pointer left the window: the sharer's own position shows again.
pub(super) fn mouse_left(shared: &Shared) {
    CONTROL.with(|cell| {
        if let Ok(mut here) = cell.try_borrow_mut() {
            here.tracking = false;
        }
    });
    shared.set_local(None);
}

// The window thread's side of remote control.
#[derive(Debug, Default)]
struct Here {
    // The last WM_ACTIVATE said this window is the active one.
    active: bool,
    // What the ControlOut was last told.
    told: Option<Capturing>,
    // Where this PC's pointer is held, in screen pixels, in relative mode.
    clipped: Option<RECT>,
    hand: Hand,
    // The window holds the mouse, so the up of a press sent comes here
    // wherever the pointer goes.
    holding: bool,
    // Windows says when the pointer leaves the window.
    tracking: bool,
    #[cfg(test)]
    pretend_front: Option<bool>,
}

// What control does to this PC's own pointer. A test build records the
// calls instead of making them, so no test ever clips, hides or holds the
// real pointer.
#[cfg(not(test))]
pub(super) mod pointer {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    };
    use windows::Win32::UI::WindowsAndMessaging::{ClipCursor, IDC_ARROW, LoadCursorW, SetCursor};

    pub(crate) fn clip(rect: Option<&RECT>) {
        // SAFETY: a RECT alive for the call, or none, which lets go.
        let _ = unsafe { ClipCursor(rect.map(|rect| rect as *const RECT)) };
    }

    // Only while it is over this thread's window, which is where WM_SETCURSOR
    // and a clip to the window put it.
    pub(crate) fn hide() {
        // SAFETY: no cursor is a valid argument.
        unsafe { SetCursor(None) };
    }

    pub(crate) fn show() {
        // SAFETY: a system cursor, which is never destroyed.
        unsafe {
            if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(arrow));
            }
        }
    }

    pub(crate) fn hold(hwnd: HWND, on: bool) {
        // SAFETY: this thread's own window.
        unsafe {
            if on {
                SetCapture(hwnd);
            } else {
                let _ = ReleaseCapture();
            }
        }
    }

    pub(crate) fn track_leave(hwnd: HWND) {
        let mut track = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: a TRACKMOUSEEVENT with its size set, for this thread's
        // own window.
        let _ = unsafe { TrackMouseEvent(&mut track) };
    }
}

#[cfg(test)]
pub(crate) mod pointer {
    use std::sync::Mutex;

    use windows::Win32::Foundation::{HWND, RECT};

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(crate) enum Effect {
        Clip(Option<RECT>),
        Hide,
        Show,
        Hold(bool),
        TrackLeave,
    }

    static EFFECTS: Mutex<Vec<Effect>> = Mutex::new(Vec::new());

    fn record(effect: Effect) {
        EFFECTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(effect);
    }

    pub(crate) fn take() -> Vec<Effect> {
        std::mem::take(
            &mut *EFFECTS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub(crate) fn clip(rect: Option<&RECT>) {
        record(Effect::Clip(rect.copied()));
    }

    pub(crate) fn hide() {
        record(Effect::Hide);
    }

    pub(crate) fn show() {
        record(Effect::Show);
    }

    pub(crate) fn hold(_hwnd: HWND, on: bool) {
        record(Effect::Hold(on));
    }

    pub(crate) fn track_leave(_hwnd: HWND) {
        record(Effect::TrackLeave);
    }
}

fn in_front(hwnd: HWND, here: &Here) -> bool {
    #[cfg(test)]
    if let Some(front) = here.pretend_front {
        return front;
    }
    // Active within this thread is not enough: a window shown by a program
    // in the background is active without being in front.
    // SAFETY: a plain query.
    here.active && unsafe { GetForegroundWindow() } == hwnd
}

// Capture, the clip and the mouse hold as control, the focus and the
// window's state have them now. Called after each change to any of them;
// `leaving` stops everything for good. Nothing here waits: the ControlOut
// returns at once, and the pointer calls outside the borrow send this
// window messages of their own.
pub(super) fn follow_control(hwnd: HWND, shared: &Shared, leaving: bool) {
    let Some(out) = shared.out.as_deref() else {
        return;
    };
    let mut let_go = false;
    let mut clip = None;
    CONTROL.with(|cell| {
        let Ok(mut here) = cell.try_borrow_mut() else {
            return;
        };
        let want = control::capturing(
            shared.mode(),
            in_front(hwnd, &here),
            shared.minimized(),
            shared.closed() || leaving,
            shared.fullscreen(),
        );
        let window = hwnd.0 as isize;
        if want != here.told {
            // The Windows keys hook comes and goes with fullscreen. Capture
            // stops across the change, so the sharer lets go of what was
            // held and the input crate starts afresh.
            if let (Some(before), Some(now)) = (here.told, want)
                && before.windows_keys != now.windows_keys
            {
                out.capture(window, None);
            }
            out.capture(window, want);
            here.told = want;
            shared.set_capturing(want);
        }
        if want.is_none_or(|how| how.relative) {
            here.hand.forget();
            let_go = std::mem::take(&mut here.holding);
            shared.set_local(None);
        }
        let held = want
            .filter(|how| how.relative)
            .and_then(|_| client_on_screen(hwnd));
        if held != here.clipped {
            clip = Some((held, here.clipped.is_none()));
            here.clipped = held;
        }
    });
    if let_go {
        pointer::hold(hwnd, false);
    }
    match clip {
        Some((Some(rect), newly)) => {
            pointer::clip(Some(&rect));
            if newly {
                pointer::hide();
            }
        }
        Some((None, _)) => {
            pointer::clip(None);
            pointer::show();
        }
        None => {}
    }
}

// The client area in screen pixels, where relative mode holds the pointer.
fn client_on_screen(hwnd: HWND) -> Option<RECT> {
    let mut client = RECT::default();
    // SAFETY: a live window and a live out parameter.
    unsafe { GetClientRect(hwnd, &mut client) }.ok()?;
    let mut corners = [
        POINT {
            x: client.left,
            y: client.top,
        },
        POINT {
            x: client.right,
            y: client.bottom,
        },
    ];
    // SAFETY: a live window and live points.
    let mapped = corners
        .iter_mut()
        .all(|corner| unsafe { ClientToScreen(hwnd, corner) }.as_bool());
    let rect = RECT {
        left: corners[0].x,
        top: corners[0].y,
        right: corners[1].x,
        bottom: corners[1].y,
    };
    (mapped && rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

#[derive(Clone, Copy)]
pub(super) enum Mouse {
    Move,
    Button(MouseButton, bool),
    Wheel(i32, bool),
}

// A mouse message while capture runs in absolute mode, `at` in client
// pixels. True when it went to the sharer, so the window does nothing more
// with it. A click on the strip stays here: it opens the stats panel.
pub(super) fn point(hwnd: HWND, shared: &Shared, mouse: Mouse, at: (i32, i32)) -> bool {
    if shared.capturing() != Some(MouseMode::Absolute) {
        return false;
    }
    let Some(out) = shared.out.as_deref() else {
        return false;
    };
    let placed = shared.placed();
    let on = placed
        .as_ref()
        .filter(|_| !in_strip(shared, at.1))
        .and_then(|placed| control::on_picture(at, placed));
    let edge = placed
        .as_ref()
        .and_then(|placed| control::toward_picture(at, placed));
    let now = Instant::now();
    let mut send = |pointing| out.point(pointing, now);
    let mut hold = None;
    let mut track = false;
    let sent = CONTROL.with(|cell| {
        let Ok(mut here) = cell.try_borrow_mut() else {
            return false;
        };
        let sent = match mouse {
            Mouse::Move => {
                here.hand.moved(on, edge, &mut send);
                false
            }
            Mouse::Button(button, down) => here.hand.button(button, down, on, edge, &mut send),
            Mouse::Wheel(delta, horizontal) => {
                here.hand.wheel(delta, horizontal, on, &mut send);
                on.is_some()
            }
        };
        if here.hand.holding() != here.holding {
            here.holding = here.hand.holding();
            hold = Some(here.holding);
        }
        track = !std::mem::replace(&mut here.tracking, true);
        sent
    });
    shared.set_local(on.map(|_| at));
    if let Some(on) = hold {
        pointer::hold(hwnd, on);
    }
    if track {
        pointer::track_leave(hwnd);
    }
    sent
}

pub(super) fn client_point(lparam: LPARAM) -> (i32, i32) {
    (
        i32::from(lparam.0 as i16),
        i32::from((lparam.0 >> 16) as i16),
    )
}

// Down or up, and which button, for the button messages.
pub(super) fn button_of(msg: u32, wparam: WPARAM) -> Option<(MouseButton, bool)> {
    Some(match msg {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = match (wparam.0 >> 16) as u16 {
                XBUTTON1 => MouseButton::Back,
                XBUTTON2 => MouseButton::Forward,
                _ => return None,
            };
            (button, msg == WM_XBUTTONDOWN)
        }
        _ => return None,
    })
}

// Made-up window messages to a window that is never shown, and a
// ControlOut that records: nothing here reads a real key or moves, clips or
// hides the real pointer, and the pointer calls are the recorder's.
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Instant;

    use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageW, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    };

    use super::pointer::{self, Effect};
    use crate::Show;
    use crate::control::{self, Capturing, ControlOut, MouseButton, MouseMode, Pointing};
    use crate::picture::Placement;
    use crate::window::{WM_CONTROL, WM_FULLSCREEN, WM_PRETEND_FRONT, Window};

    // Pointing's own Debug never says where; a failure here should.
    #[derive(Debug, PartialEq)]
    enum Heard {
        Capture(Option<Capturing>),
        At(u16, u16),
        Button(MouseButton, bool),
        Wheel(i32),
        HWheel(i32),
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Heard>>);

    impl Recorder {
        fn take(&self) -> Vec<Heard> {
            std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }

        fn heard(&self, heard: Heard) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(heard);
        }
    }

    impl ControlOut for Recorder {
        fn capture(&self, _window: isize, how: Option<Capturing>) {
            self.heard(Heard::Capture(how));
        }

        fn point(&self, pointing: Pointing, _at: Instant) {
            self.heard(match pointing {
                Pointing::At { x, y } => Heard::At(x, y),
                Pointing::Button { button, down } => Heard::Button(button, down),
                Pointing::Wheel { delta } => Heard::Wheel(delta),
                Pointing::HWheel { delta } => Heard::HWheel(delta),
            });
        }

        fn release_key(&self) -> String {
            String::from("Ctrl+Shift+End")
        }
    }

    fn at(x: i32, y: i32) -> LPARAM {
        LPARAM(((y & 0xffff) << 16 | (x & 0xffff)) as isize)
    }

    fn click(button: MouseButton, down: bool) -> Heard {
        Heard::Button(button, down)
    }

    fn to(spot: (u16, u16)) -> Heard {
        Heard::At(spot.0, spot.1)
    }

    #[test]
    fn capture_follows_control_and_focus() {
        let _screen = crate::tests::one_at_a_time();
        // The window is per-monitor aware; a sender that is not has the
        // points in its mouse messages scaled on the way.
        // SAFETY: a constant context; the previous one is not needed.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let out = Arc::new(Recorder::default());
        let window = Window::open(
            "Booth viewer control test",
            (640, 360),
            Show::Hidden,
            None,
            Some(Arc::clone(&out) as Arc<dyn ControlOut>),
        )
        .unwrap();
        let hwnd = window.hwnd();
        let send = |msg: u32, wparam: usize, lparam: LPARAM| {
            // SAFETY: this test's live window; SendMessageW returns once the
            // window's thread has handled the message.
            unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wparam)), Some(lparam)) };
        };
        let shared = window.shared();
        pointer::take();
        let windowed = Capturing {
            windows_keys: false,
            relative: false,
        };
        let fullscreen = Capturing {
            windows_keys: true,
            relative: false,
        };
        let game = Capturing {
            windows_keys: true,
            relative: true,
        };

        // Controlling, and another window in front: nothing is captured.
        shared.set_mode(Some(MouseMode::Absolute));
        send(WM_CONTROL, 0, LPARAM(0));
        assert_eq!(out.take(), []);
        send(WM_PRETEND_FRONT, 1, LPARAM(0));
        assert_eq!(out.take(), [Heard::Capture(Some(windowed))]);

        // A present placed a 1280x720 picture at half size. A click goes
        // where the pointer is, and the window holds the mouse for its up.
        let placed = Placement {
            left: 0,
            top: 0,
            width: 640,
            height: 360,
            source: (1280, 720),
        };
        shared.set_placed(Some(placed));
        let middle = control::on_picture((320, 180), &placed).unwrap();
        send(WM_MOUSEMOVE, 0, at(320, 180));
        send(WM_LBUTTONDOWN, 1, at(320, 180));
        send(WM_LBUTTONUP, 0, at(320, 180));
        assert_eq!(
            out.take(),
            [
                to(middle),
                to(middle),
                click(MouseButton::Left, true),
                to(middle),
                click(MouseButton::Left, false)
            ]
        );
        assert_eq!(shared.local(), Some((320, 180)));
        assert_eq!(
            pointer::take(),
            [Effect::TrackLeave, Effect::Hold(true), Effect::Hold(false)]
        );

        // The strip's click stays on this PC: it opens the stats panel.
        let strip = shared.size().1 as i32 - 2;
        send(WM_MOUSEMOVE, 0, at(10, strip));
        send(WM_LBUTTONDOWN, 1, at(10, strip));
        send(WM_LBUTTONUP, 0, at(10, strip));
        assert_eq!(out.take(), []);
        assert_eq!(shared.local(), None, "the sharer's own pointer shows there");
        assert!(shared.take_strip_click());

        // The wheel's point comes on the screen.
        let mut screen = POINT { x: 100, y: 50 };
        // SAFETY: this test's live window and a live point.
        let _ = unsafe { ClientToScreen(hwnd, &mut screen) };
        send(WM_MOUSEWHEEL, 120 << 16, at(screen.x, screen.y));
        assert_eq!(
            out.take(),
            [
                to(control::on_picture((100, 50), &placed).unwrap()),
                Heard::Wheel(120)
            ]
        );

        // Fullscreen: the Windows keys go too, and capture stops across the
        // change. Escape is the sharer's now, and fullscreen stays.
        send(WM_FULLSCREEN, 1, LPARAM(0));
        assert_eq!(
            out.take(),
            [Heard::Capture(None), Heard::Capture(Some(fullscreen))]
        );
        send(WM_KEYDOWN, usize::from(VK_ESCAPE.0), LPARAM(0));
        assert!(shared.fullscreen());

        // A game hid the sharer's pointer: the mouse goes as raw motion from
        // the feed, and this PC's pointer is held in the window, hidden.
        shared.set_mode(Some(MouseMode::Relative));
        send(WM_CONTROL, 0, LPARAM(0));
        assert_eq!(out.take(), [Heard::Capture(Some(game))]);
        let effects = pointer::take();
        assert!(
            matches!(effects.as_slice(), [Effect::Clip(Some(_)), Effect::Hide]),
            "{effects:?}"
        );
        let strip = shared.size().1 as i32 - 2;
        send(WM_MOUSEMOVE, 0, at(200, 100));
        send(WM_LBUTTONDOWN, 1, at(200, strip));
        send(WM_LBUTTONUP, 0, at(200, strip));
        assert_eq!(out.take(), [], "the game's clicks come from the feed");
        assert!(
            !shared.take_strip_click(),
            "a game's click opened the stats panel"
        );

        // Another window in front: capture stops, and the pointer is let go
        // at once.
        send(WM_PRETEND_FRONT, 0, LPARAM(0));
        assert_eq!(out.take(), [Heard::Capture(None)]);
        assert_eq!(pointer::take(), [Effect::Clip(None), Effect::Show]);

        // Back in front, then control ends.
        send(WM_PRETEND_FRONT, 1, LPARAM(0));
        assert_eq!(out.take(), [Heard::Capture(Some(game))]);
        pointer::take();
        shared.set_mode(None);
        send(WM_CONTROL, 0, LPARAM(0));
        assert_eq!(out.take(), [Heard::Capture(None)]);
        assert_eq!(pointer::take(), [Effect::Clip(None), Effect::Show]);
        send(WM_KEYDOWN, usize::from(VK_ESCAPE.0), LPARAM(0));
        assert!(!shared.fullscreen(), "Escape is this PC's again");

        // A viewer closed while capturing stops the capture.
        shared.set_mode(Some(MouseMode::Absolute));
        send(WM_CONTROL, 0, LPARAM(0));
        assert_eq!(out.take(), [Heard::Capture(Some(windowed))]);
        drop(window);
        assert_eq!(out.take(), [Heard::Capture(None)]);
        assert_eq!(pointer::take(), []);
    }
}
