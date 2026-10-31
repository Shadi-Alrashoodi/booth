//! Faults a test can put into the share, built with the `fault` feature
//! only, which room's tests turn on and a release build never has. Capture's
//! test pattern draws no pointer, so a test of how a watcher's viewer draws
//! the sharer's pointer gives it one, and reads back what each viewer drew
//! for the pointers it held for their pictures, and what waits for a viewer
//! in its inbox.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use capture::{CursorKind, CursorShape, CursorUpdate, Next, Pattern};
use viewer::Cursor;

use crate::Inbox;

// Each picture of the pattern carries a pointer STEP pixels right of the
// one before, the first with a 32x32 shape. Every fourth pointer goes alone
// instead, ALONE_AFTER after the picture before it, and its picture comes
// without one, as when capture holds a picture for the fps cap. Two
// pictures in every WITHHELD_EVERY send their pointers and never go out, as
// a picture too big to send does, so a pointer held for its picture runs out
// of time.
const STEP: u64 = 4;
const ALONE_EVERY: u64 = 4;
const ALONE_AFTER: Duration = Duration::from_millis(6);
const WITHHELD_EVERY: u64 = 64;
const WITHHELD: [u64; 2] = [33, 34];
// Where the pointer runs on the pattern.
const ACROSS: u64 = 2400;
const DOWN: i32 = 200;

struct PatternPointer {
    made: u64,
    // When the last picture was made, and whether the next one's pointer
    // went alone already.
    last: Option<Instant>,
    alone: bool,
    // The picture just made never goes out.
    withheld: bool,
}

const FRESH: PatternPointer = PatternPointer {
    made: 0,
    last: None,
    alone: false,
    withheld: false,
};

static ON: AtomicBool = AtomicBool::new(false);
static POINTER: Mutex<PatternPointer> = Mutex::new(FRESH);
static RELEASED: Mutex<Vec<Released>> = Mutex::new(Vec::new());

thread_local! {
    // A pointer this thread's viewer let go, until its next present.
    static LET_GO: RefCell<Option<(LetGo, Spot)>> = const { RefCell::new(None) };
}

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// From now on each picture of the test pattern carries a pointer, until
/// pattern_pointer(false), and what viewers draw for a pointer they held is
/// kept for released().
pub fn pattern_pointer(on: bool) {
    *lock(&POINTER) = FRESH;
    lock(&RELEASED).clear();
    ON.store(on, Ordering::Release);
}

/// Where a pointer was, and the size of the shape that came with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape: Option<(u32, u32)>,
}

/// How a viewer let go of a pointer it held for its picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LetGo {
    /// With a picture, its own or not.
    Picture,
    /// Alone, once its wait ran out.
    RanOut,
}

/// A pointer a viewer let go, and the present that came next: whether it
/// had a picture, and the pointer it drew.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    pub by: LetGo,
    pub held: Spot,
    pub picture: bool,
    pub drawn: Option<Spot>,
}

/// Every pointer let go since pattern_pointer(true) or the last call.
pub fn released() -> Vec<Released> {
    std::mem::take(&mut *lock(&RELEASED))
}

/// The pointer update waiting in `inbox` for its viewer's thread, whether
/// it came with its picture, and whether it brings only a shape.
pub fn cursor_waiting(inbox: &Inbox) -> Option<(CursorUpdate, bool, bool)> {
    inbox.cursor_waiting()
}

// The pattern's next frame or lone pointer while the hook is on.
pub(crate) fn pattern_next(pattern: &mut Pattern) -> Option<Result<Next, String>> {
    if !ON.load(Ordering::Acquire) {
        return None;
    }
    let mut state = lock(&POINTER);
    let number = state.made;
    let alone = number % ALONE_EVERY == ALONE_EVERY - 1;
    if alone && !state.alone {
        state.alone = true;
        if let Some(last) = state.last {
            std::thread::sleep((last + ALONE_AFTER).saturating_duration_since(Instant::now()));
        }
        return Some(Ok(Next::Cursor(pointer(number))));
    }
    let mut frame = match pattern.next() {
        Ok(frame) => frame,
        Err(err) => return Some(Err(err.to_string())),
    };
    if !alone {
        frame.cursor = Some(pointer(number));
    }
    state.made += 1;
    state.last = Some(Instant::now());
    state.alone = false;
    state.withheld = WITHHELD.contains(&(number % WITHHELD_EVERY));
    Some(Ok(Next::Frame(frame)))
}

fn pointer(number: u64) -> CursorUpdate {
    CursorUpdate {
        x: (number * STEP % ACROSS) as i32,
        y: DOWN,
        visible: true,
        scale: 1.0,
        shape: (number == 0).then(|| CursorShape {
            kind: CursorKind::Color,
            width: 32,
            height: 32,
            pitch: 128,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![0xff; 32 * 32 * 4],
        }),
    }
}

// False, once, for a picture the pattern made never to go out.
// While the pattern carries a pointer, a viewer holds as on the flip path:
// the tests' viewers are windows, which DWM composes.
pub(crate) fn holds_as_flip() -> bool {
    ON.load(Ordering::Acquire)
}

pub(crate) fn picture_goes() -> bool {
    !ON.load(Ordering::Acquire) || !std::mem::take(&mut lock(&POINTER).withheld)
}

pub(crate) fn let_go(by: LetGo, update: &CursorUpdate) {
    if ON.load(Ordering::Acquire) {
        let held = Spot {
            x: update.x,
            y: update.y,
            visible: update.visible,
            shape: update
                .shape
                .as_ref()
                .map(|shape| (shape.width, shape.height)),
        };
        LET_GO.set(Some((by, held)));
    }
}

// A present was made: the pointer this thread's viewer let go just before
// is kept with what the present drew.
pub(crate) fn presented(picture: bool, cursor: Option<&Cursor>) {
    if let Some((by, held)) = LET_GO.take() {
        let drawn = cursor.map(|cursor| Spot {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible,
            shape: cursor
                .shape
                .as_ref()
                .map(|shape| (shape.width, shape.height)),
        });
        lock(&RELEASED).push(Released {
            by,
            held,
            picture,
            drawn,
        });
    }
}
