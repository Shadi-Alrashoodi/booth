// Remote control from the controller's side: what the viewer does while this
// PC controls the share it shows. The keys and a game's raw mouse come from
// the input crate's feed, which the app reads; the viewer only says when
// that capture may run: while this PC controls, the viewer's window is in
// front and it is not minimized. While the sharer's pointer shows, the mouse
// goes as points on the picture from the viewer's own mouse messages, so a
// click lands where the controller sees the pointer. While a game hides it,
// the mouse goes as raw motion from the feed, and the viewer keeps this PC's
// pointer inside its window, hidden.

use std::fmt;
use std::time::{Duration, Instant};

use crate::picture::Placement;

// Where a controlling viewer's input goes, which in the room is the room's
// sender. Called on the viewer's threads; each call must return at once.
pub trait ControlOut: Send + Sync {
    // Capture on, as `how` says, or off. `window` is the viewer's own
    // window: the input crate reads nothing unless it is in front.
    fn capture(&self, window: isize, how: Option<Capturing>);
    // Absolute mode: the controller's mouse on the picture, captured at `at`.
    fn point(&self, pointing: Pointing, at: Instant);
    // The release key as the strip prints it, "Ctrl+Shift+End": the panic
    // key of this PC, which ends this PC's control of the other.
    fn release_key(&self) -> String;
}

// How the mouse goes while this PC controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseMode {
    // Points on the picture: the desktop, where the sharer's pointer shows.
    Absolute,
    // Raw motion: a game, which hid the sharer's pointer.
    Relative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capturing {
    // Fullscreen: the Windows keys go to the sharer too, so a game gets
    // them. Windowed they stay here, so Alt+Tab always gets you out.
    pub windows_keys: bool,
    // The mouse goes as raw motion from the feed, not as points.
    pub relative: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    // X1 and X2: back and forward in a browser.
    Back,
    Forward,
}

impl MouseButton {
    fn bit(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
            MouseButton::Back => 8,
            MouseButton::Forward => 16,
        }
    }
}

// The controller's mouse in absolute mode, in the order it happened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pointing {
    // A point on the picture: x / 65536 of the way across and y / 65536 of
    // the way down, taken at the middle of the pixel the pointer is on.
    At { x: u16, y: u16 },
    Button { button: MouseButton, down: bool },
    // In Windows' units, 120 a notch; up and right are positive.
    Wheel { delta: i32 },
    HWheel { delta: i32 },
}

// Never where the pointer is: nothing of the controller's input goes to a
// log.
impl fmt::Debug for Pointing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Pointing::At { .. } => "At",
            Pointing::Button { .. } => "Button",
            Pointing::Wheel { .. } => "Wheel",
            Pointing::HWheel { .. } => "HWheel",
        })
    }
}

// What capture should be now: nothing unless this PC controls, the window
// is in front, and it is neither minimized nor closed.
pub(crate) fn capturing(
    mode: Option<MouseMode>,
    front: bool,
    minimized: bool,
    closed: bool,
    fullscreen: bool,
) -> Option<Capturing> {
    let mode = mode?;
    (front && !minimized && !closed).then_some(Capturing {
        windows_keys: fullscreen,
        relative: mode == MouseMode::Relative,
    })
}

// While this PC controls, Escape goes to the sharer, where games use it for
// their menu, so it leaves fullscreen only when this PC does not control.
// F11 always does.
pub(crate) fn escape_leaves_fullscreen(mode: Option<MouseMode>) -> bool {
    mode.is_none()
}

// Alt or F10 alone would give the next key to the window's menu. While this
// PC controls, those keys are the sharer's. Alt+Space, which opens the
// window menu and comes with its character, stays here in a window, as the
// Windows keys do.
pub(crate) fn menu_key_is_the_sharers(mode: Option<MouseMode>, character: isize) -> bool {
    mode.is_some() && character == 0
}

// Where a pixel of the window's client area falls on the picture. None off
// the picture.
pub(crate) fn on_picture(point: (i32, i32), placed: &Placement) -> Option<(u16, u16)> {
    let (x, y) = (point.0 - placed.left, point.1 - placed.top);
    ((0..placed.width).contains(&x) && (0..placed.height).contains(&y))
        .then(|| (across(x, placed.width), across(y, placed.height)))
}

// The same, held to the picture's edge: a drag that left the picture goes
// on along its edge, as it would at the edge of the sharer's monitor.
pub(crate) fn toward_picture(point: (i32, i32), placed: &Placement) -> Option<(u16, u16)> {
    if placed.width <= 0 || placed.height <= 0 {
        return None;
    }
    let x = (point.0 - placed.left).clamp(0, placed.width - 1);
    let y = (point.1 - placed.top).clamp(0, placed.height - 1);
    Some((across(x, placed.width), across(y, placed.height)))
}

// At the pixel's middle, so every pixel of a picture shown at one to one
// comes out on the sharer as that same pixel, whose side maps a point back
// with `point * size / 65536`, as Windows maps absolute mouse input.
fn across(pixel: i32, size: i32) -> u16 {
    let fraction = (f64::from(pixel) + 0.5) / f64::from(size);
    (fraction * 65536.0).floor().clamp(0.0, 65535.0) as u16
}

// The controller's mouse in absolute mode, from the viewer's mouse
// messages. `on` is the point on the picture, None off it or on the strip;
// `edge` the point held to the picture's edge. Each press, release and turn
// of the wheel goes with the point it happened at. No point is kept once it
// is handed on; the window drops a move to where the pointer already was.
#[derive(Debug, Default)]
pub(crate) struct Hand {
    // Buttons this window sent down and not up yet: each gets its up
    // wherever the pointer went, and no other up is sent.
    down: u8,
}

impl Hand {
    // Sent while the pointer is on the picture, and along its edge while a
    // button this window sent is down.
    pub(crate) fn moved(
        &mut self,
        on: Option<(u16, u16)>,
        edge: Option<(u16, u16)>,
        send: &mut dyn FnMut(Pointing),
    ) {
        let to = if self.down != 0 { on.or(edge) } else { on };
        self.go(to, send);
    }

    // A press goes only on the picture, where the pointer is sent first.
    // True when something went, so the window holds the mouse for the up.
    pub(crate) fn button(
        &mut self,
        button: MouseButton,
        down: bool,
        on: Option<(u16, u16)>,
        edge: Option<(u16, u16)>,
        send: &mut dyn FnMut(Pointing),
    ) -> bool {
        let bit = button.bit();
        if down {
            let Some(at) = on else {
                return false;
            };
            self.go(Some(at), send);
            self.down |= bit;
        } else {
            if self.down & bit == 0 {
                return false;
            }
            self.go(on.or(edge), send);
            self.down &= !bit;
        }
        send(Pointing::Button { button, down });
        true
    }

    pub(crate) fn wheel(
        &mut self,
        delta: i32,
        horizontal: bool,
        on: Option<(u16, u16)>,
        send: &mut dyn FnMut(Pointing),
    ) {
        if on.is_none() || delta == 0 {
            return;
        }
        self.go(on, send);
        send(if horizontal {
            Pointing::HWheel { delta }
        } else {
            Pointing::Wheel { delta }
        });
    }

    pub(crate) fn holding(&self) -> bool {
        self.down != 0
    }

    // Capture stopped, or the mouse went relative: the room let go of
    // everything this sent, and nothing is owed an up.
    pub(crate) fn forget(&mut self) {
        *self = Hand::default();
    }

    fn go(&mut self, to: Option<(u16, u16)>, send: &mut dyn FnMut(Pointing)) {
        if let Some((x, y)) = to {
            send(Pointing::At { x, y });
        }
    }
}

// In absolute mode the viewer draws the sharer's pointer where the
// controller's mouse is, with no wait for the network, and the sharer's own
// position corrects drift once a second: after the mouse has been still this
// long, the pointer is drawn where the sharer says it is, if that is
// elsewhere. A program there moved it, or its owner did; the next move sends
// the controller's position again.
pub(crate) const DRIFT_AFTER: Duration = Duration::from_secs(1);
// Rounding between the two sides moves a pointer by up to a pixel.
const DRIFT_PX: f64 = 2.0;

// Where the pointer's hotspot is drawn, in client pixels: the controller's
// mouse, or None for the sharer's position (`echo`, when it shows).
pub(crate) fn drawn_at(
    local: (i32, i32),
    still: Duration,
    echo: Option<(f64, f64)>,
) -> Option<(f64, f64)> {
    let here = (f64::from(local.0), f64::from(local.1));
    match echo {
        Some(echo)
            if still >= DRIFT_AFTER
                && ((echo.0 - here.0).abs() > DRIFT_PX || (echo.1 - here.1).abs() > DRIFT_PX) =>
        {
            None
        }
        _ => Some(here),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(left: i32, top: i32, width: i32, height: i32) -> Placement {
        Placement {
            left,
            top,
            width,
            height,
            source: (width.max(1) as u32, height.max(1) as u32),
        }
    }

    fn sent(
        hand: &mut Hand,
        act: impl FnOnce(&mut Hand, &mut dyn FnMut(Pointing)),
    ) -> Vec<Pointing> {
        let mut out = Vec::new();
        act(hand, &mut |pointing| out.push(pointing));
        out
    }

    #[test]
    fn capture_runs_only_while_controlling_in_front_and_shown() {
        let on = Some(MouseMode::Absolute);
        assert_eq!(
            capturing(on, true, false, false, false),
            Some(Capturing {
                windows_keys: false,
                relative: false
            })
        );
        assert_eq!(
            capturing(Some(MouseMode::Relative), true, false, false, true),
            Some(Capturing {
                windows_keys: true,
                relative: true
            })
        );
        assert_eq!(capturing(None, true, false, false, true), None);
        assert_eq!(capturing(on, false, false, false, true), None);
        assert_eq!(capturing(on, true, true, false, false), None);
        assert_eq!(capturing(on, true, false, true, false), None);
    }

    #[test]
    fn escape_goes_to_the_sharer_while_controlling() {
        assert!(escape_leaves_fullscreen(None));
        assert!(!escape_leaves_fullscreen(Some(MouseMode::Absolute)));
        assert!(!escape_leaves_fullscreen(Some(MouseMode::Relative)));
    }

    #[test]
    fn alt_goes_to_the_sharer_alt_space_stays() {
        let on = Some(MouseMode::Absolute);
        assert!(menu_key_is_the_sharers(on, 0));
        assert!(!menu_key_is_the_sharers(on, isize::from(b' ')));
        assert!(!menu_key_is_the_sharers(None, 0));
    }

    // The sharer maps a point back as `point * size / 65536`: at one to one
    // every pixel lands on itself, the corners included.
    #[test]
    fn a_picture_at_one_to_one_maps_each_pixel_onto_itself() {
        for (width, height) in [(1920, 1080), (2560, 1440), (641, 359)] {
            let at = placed(0, 20, width, height);
            for (x, y) in [
                (0, 0),
                (width - 1, height - 1),
                (width / 2, 7),
                (13, height / 3),
            ] {
                let (px, py) = on_picture((x, y + 20), &at).expect("on the picture");
                assert_eq!(
                    (
                        (u64::from(px) * width as u64 / 65536) as i32,
                        (u64::from(py) * height as u64 / 65536) as i32
                    ),
                    (x, y),
                    "{width}x{height}"
                );
            }
        }
    }

    #[test]
    fn a_drag_holds_to_the_picture_edge() {
        // A 16:9 picture in a tall window: bars above and below.
        let at = placed(0, 100, 640, 360);
        assert_eq!(on_picture((10, 99), &at), None);
        assert_eq!(on_picture((10, 460), &at), None);
        assert_eq!(on_picture((-1, 200), &at), None);
        assert_eq!(on_picture((640, 200), &at), None);
        let top_left = on_picture((0, 100), &at).unwrap();
        assert_eq!(toward_picture((-50, 0), &at), Some(top_left));
        let bottom_right = on_picture((639, 459), &at).unwrap();
        assert_eq!(toward_picture((5000, 5000), &at), Some(bottom_right));
        assert!(bottom_right.0 > 65400 && bottom_right.1 > 65400);
        assert_eq!(toward_picture((1, 1), &placed(0, 0, 0, 0)), None);
    }

    #[test]
    fn every_press_gets_its_release() {
        let mut hand = Hand::default();
        let here = Some((100, 200));
        let there = Some((300, 400));
        assert_eq!(
            sent(&mut hand, |h, s| h.moved(here, here, s)),
            [Pointing::At { x: 100, y: 200 }]
        );
        let down = sent(&mut hand, |h, s| {
            assert!(h.button(MouseButton::Left, true, there, there, s));
        });
        assert_eq!(
            down,
            [
                Pointing::At { x: 300, y: 400 },
                Pointing::Button {
                    button: MouseButton::Left,
                    down: true
                }
            ]
        );
        assert!(hand.holding());
        // Dragged off the picture: the drag goes on along its edge.
        let edge = Some((65535, 400));
        assert_eq!(
            sent(&mut hand, |h, s| h.moved(None, edge, s)),
            [Pointing::At { x: 65535, y: 400 }]
        );
        let up = sent(&mut hand, |h, s| {
            assert!(h.button(MouseButton::Left, false, None, edge, s));
        });
        assert_eq!(
            up,
            [
                Pointing::At { x: 65535, y: 400 },
                Pointing::Button {
                    button: MouseButton::Left,
                    down: false
                }
            ]
        );
        assert!(!hand.holding());
        // Off the picture with nothing held, a move goes nowhere, and so
        // does a press there, and the release of a press never sent.
        assert!(sent(&mut hand, |h, s| h.moved(None, edge, s)).is_empty());
        assert!(
            sent(&mut hand, |h, s| {
                assert!(!h.button(MouseButton::Right, true, None, edge, s));
                assert!(!h.button(MouseButton::Right, false, here, here, s));
            })
            .is_empty()
        );
    }

    #[test]
    fn the_wheel_turns_only_over_the_picture() {
        let mut hand = Hand::default();
        assert!(sent(&mut hand, |h, s| h.wheel(120, false, None, s)).is_empty());
        assert!(sent(&mut hand, |h, s| h.wheel(0, false, Some((1, 1)), s)).is_empty());
        assert_eq!(
            sent(&mut hand, |h, s| h.wheel(-240, true, Some((5, 6)), s)),
            [
                Pointing::At { x: 5, y: 6 },
                Pointing::HWheel { delta: -240 }
            ]
        );
    }

    #[test]
    fn a_forgotten_hand_owes_no_release() {
        let mut hand = Hand::default();
        let here = Some((7, 7));
        sent(&mut hand, |h, s| {
            h.button(MouseButton::Middle, true, here, here, s);
        });
        hand.forget();
        assert!(!hand.holding());
        assert!(
            sent(&mut hand, |h, s| {
                h.button(MouseButton::Middle, false, here, here, s);
            })
            .is_empty()
        );
    }

    #[test]
    fn the_pointer_follows_the_mouse_until_it_rests() {
        let mouse = (400, 300);
        let moving = Duration::from_millis(200);
        // Moving, the mouse wins wherever the sharer's pointer is.
        assert_eq!(
            drawn_at(mouse, moving, Some((10.0, 10.0))),
            Some((400.0, 300.0))
        );
        // Resting a second, the sharer's position wins when it is elsewhere.
        assert_eq!(drawn_at(mouse, DRIFT_AFTER, Some((10.0, 10.0))), None);
        // A pixel of rounding is no drift.
        assert_eq!(
            drawn_at(mouse, DRIFT_AFTER, Some((401.0, 299.0))),
            Some((400.0, 300.0))
        );
        // Nothing heard from the sharer: the mouse.
        assert_eq!(drawn_at(mouse, DRIFT_AFTER, None), Some((400.0, 300.0)));
    }

    #[test]
    fn a_pointing_event_never_says_where() {
        let text = format!(
            "{:?} {:?}",
            Pointing::At { x: 1234, y: 5678 },
            Pointing::Wheel { delta: 360 }
        );
        assert_eq!(text, "At Wheel");
    }
}
