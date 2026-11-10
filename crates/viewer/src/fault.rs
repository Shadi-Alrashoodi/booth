//! Faults a test can put into the viewer, built with the `fault` feature
//! only, which share's tests turn on and a release build never has. No test
//! can make a game leave too little video memory, so resizes, uploads of
//! the pointer's shape and copies of the last picture fail here as they
//! would then, or as a call the device refuses, and a test reads back what
//! the last present that reached the screen had.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY};

use crate::ViewerError;

static RESIZES_FAILING: AtomicU32 = AtomicU32::new(0);
static SHAPES_FAILING: AtomicU32 = AtomicU32::new(0);
static KEPT_FAILING: AtomicU32 = AtomicU32::new(0);
static REFUSING: AtomicU32 = AtomicU32::new(0);
static LAST_SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

/// The next `times` resizes of any viewer's buffers in this process fail
/// as ResizeBuffers does when video memory runs short. 0 ends what is left.
pub fn fail_resizes(times: u32) {
    RESIZES_FAILING.store(times, Ordering::Release);
}

/// The next `times` uploads of the sharer's pointer shape by any viewer in
/// this process fail as making its texture does when video memory runs
/// short. 0 ends what is left.
pub fn fail_shapes(times: u32) {
    SHAPES_FAILING.store(times, Ordering::Release);
}

/// The next `times` copies of a picture of a new size, which any viewer in
/// this process keeps to draw again, fail as making their texture does
/// when video memory runs short. 0 ends what is left.
pub fn fail_kept_pictures(times: u32) {
    KEPT_FAILING.store(times, Ordering::Release);
}

/// The next `times` of the failures above fail as a call the device refuses
/// (E_INVALIDARG) rather than for too little video memory. 0 ends what is
/// left.
pub fn refuse(times: u32) {
    REFUSING.store(times, Ordering::Release);
}

/// What a present that reached the screen had: the size of its buffers,
/// and whether the viewer had a shape for the sharer's pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub size: (u32, u32),
    pub shape: bool,
}

/// The last present of any viewer in this process that reached the screen.
pub fn last_shown() -> Option<Shown> {
    *LAST_SHOWN.lock().unwrap_or_else(|p| p.into_inner())
}

fn take(failing: &AtomicU32) -> bool {
    failing
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
            left.checked_sub(1)
        })
        .is_ok()
}

fn failure() -> windows::core::Error {
    if take(&REFUSING) {
        E_INVALIDARG.into()
    } else {
        E_OUTOFMEMORY.into()
    }
}

pub(crate) fn resize(size: (u32, u32)) -> Result<(), ViewerError> {
    if take(&RESIZES_FAILING) {
        return Err(ViewerError::windows(
            format!("resize the viewer's buffers to {}x{}", size.0, size.1),
            &failure(),
        ));
    }
    Ok(())
}

pub(crate) fn shape() -> Result<(), ViewerError> {
    if take(&SHAPES_FAILING) {
        return Err(ViewerError::windows(
            "make the pointer's texture",
            &failure(),
        ));
    }
    Ok(())
}

pub(crate) fn kept(step: String) -> Result<(), ViewerError> {
    if take(&KEPT_FAILING) {
        return Err(ViewerError::windows(step, &failure()));
    }
    Ok(())
}

pub(crate) fn shown(size: (u32, u32), shape: bool) {
    *LAST_SHOWN.lock().unwrap_or_else(|p| p.into_inner()) = Some(Shown { size, shape });
}
