// The yellow border Windows draws around a window being captured. From build
// 20348, every Windows 11, a program may ask to leave it off, and Windows may
// ask the person first. The answer is asked for once a process, at the first
// window share, waited for there at most ANSWER_WAIT, and kept: a later
// share, or a share's capture opened again, uses it without waiting.

use std::any::Any;
use std::fmt;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use windows::Graphics::Capture::{
    GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureSession,
};
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;

// Long enough for an answer Windows gives without asking anyone, short
// enough not to hold up the first share when it does ask.
const ANSWER_WAIT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Unasked,
    Asking(Instant),
    Known(AppCapabilityAccessStatus),
    // The question could not be put; it is not put again.
    Failed,
}

static ANSWER: Mutex<Answer> = Mutex::new(Answer::Unasked);
static ANSWERED: Condvar = Condvar::new();
// The question, kept until the process ends: the answer comes from another
// process, through it.
static ASKED: Mutex<Option<Box<dyn Any + Send>>> = Mutex::new(None);

// What a window share's capture does about the border, for the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Border {
    NoSwitch,
    Kept(&'static str),
    Off,
}

impl fmt::Display for Border {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Border::NoSwitch => f.write_str(
                "Windows draws its yellow border around the shared window: this Windows has no switch for it (Windows 11 has)",
            ),
            Border::Kept(why) => write!(
                f,
                "Windows draws its yellow border around the shared window: {why}"
            ),
            Border::Off => f.write_str(
                "no yellow border around the shared window: Windows allowed leaving it off",
            ),
        }
    }
}

// Before the capture starts, so the border never shows only to go.
pub(crate) fn apply(session: &GraphicsCaptureSession) -> Border {
    if session.IsBorderRequired().is_err() {
        return Border::NoSwitch;
    }
    match answer() {
        Some(AppCapabilityAccessStatus::Allowed) => match session.SetIsBorderRequired(false) {
            Ok(()) => Border::Off,
            Err(_) => Border::Kept("Windows allowed leaving it off, then refused the switch"),
        },
        Some(AppCapabilityAccessStatus::DeniedByUser) => Border::Kept(
            "it is turned on in Windows' privacy settings, under screenshot borders (DeniedByUser)",
        ),
        Some(AppCapabilityAccessStatus::DeniedBySystem) => {
            Border::Kept("Windows does not let programs leave it off here (DeniedBySystem)")
        }
        Some(AppCapabilityAccessStatus::NotDeclaredByApp) => Border::Kept(
            "the package does not declare graphicsCaptureWithoutBorder (NotDeclaredByApp)",
        ),
        Some(_) => Border::Kept("Windows answered with something else than yes or no"),
        None => Border::Kept(
            "Windows has not answered whether it may go; a share after the answer uses it",
        ),
    }
}

fn answer() -> Option<AppCapabilityAccessStatus> {
    let unasked = {
        let mut state = lock(&ANSWER);
        let unasked = *state == Answer::Unasked;
        if unasked {
            *state = Answer::Asking(Instant::now());
        }
        unasked
    };
    // Without the lock: an answer that is in already runs the callback at
    // once, and it takes the lock.
    if unasked {
        ask();
    }
    let mut state = lock(&ANSWER);
    if let Answer::Asking(since) = *state {
        let left = (since + ANSWER_WAIT).saturating_duration_since(Instant::now());
        state = ANSWERED
            .wait_timeout_while(state, left, |state| matches!(state, Answer::Asking(_)))
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    match *state {
        Answer::Known(status) => Some(status),
        _ => None,
    }
}

fn ask() {
    let Ok(asked) =
        GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
    else {
        *lock(&ANSWER) = Answer::Failed;
        return;
    };
    let heard = asked.when(|result| {
        *lock(&ANSWER) = match result {
            Ok(status) => Answer::Known(status),
            Err(_) => Answer::Failed,
        };
        ANSWERED.notify_all();
    });
    if heard.is_err() {
        *lock(&ANSWER) = Answer::Failed;
        ANSWERED.notify_all();
        return;
    }
    *lock(&ASKED) = Some(Box::new(asked));
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
