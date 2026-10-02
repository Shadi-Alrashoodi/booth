//! Faults a test can put into the encoders, built with the `fault` feature
//! only, which share's and room's tests turn on and a release build never
//! has. Intel's HEVC encoder on an Iris Xe laptop stopped asking for frames
//! after its first; NVIDIA's encoders never do and NVENC never fails here,
//! so the share's way out of both is tried on them by making them.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::Codec;

#[derive(Default)]
struct Faults {
    stalls: [Option<u64>; 2],
    nvenc_fails: Option<u64>,
    software_fails: Option<u64>,
    software_refused: bool,
}

static FAULTS: Mutex<Faults> = Mutex::new(Faults {
    stalls: [None, None],
    nvenc_fails: None,
    software_fails: None,
    software_refused: false,
});
static LIVE: AtomicUsize = AtomicUsize::new(0);
static LIVE_AT_LAST_OPEN: AtomicUsize = AtomicUsize::new(0);

fn faults() -> MutexGuard<'static, Faults> {
    FAULTS.lock().unwrap_or_else(|p| p.into_inner())
}

fn slot(codec: Codec) -> usize {
    match codec {
        Codec::H264 => 0,
        Codec::Hevc => 1,
    }
}

/// The next Media Foundation hardware encoder opened in `codec` stops
/// asking for frames once it has given `frames` back: from then on its
/// requests go unheard, so encode() waits for one as for an encoder that
/// makes none, and the encoder itself stays as it was for its shutdown.
pub fn stall(codec: Codec, frames: u64) {
    faults().stalls[slot(codec)] = Some(frames);
}

/// The next NVENC encoder opened fails the frame after the `frames` it
/// gives back: the driver encodes it, and its release of the output buffer
/// is then reported as failed, the way a driver error after the submit
/// comes back.
pub fn fail_nvenc(frames: u64) {
    faults().nvenc_fails = Some(frames);
}

/// The next software encoder opened fails the frame after the `frames` it
/// gives back, as if Media Foundation had refused it.
pub fn fail_software(frames: u64) {
    faults().software_fails = Some(frames);
}

/// The next software encoder does not open.
pub fn refuse_software() {
    faults().software_refused = true;
}

/// Takes back faults set and not used yet, for a test that ended early.
pub fn clear() {
    *faults() = Faults::default();
}

pub(crate) fn take_stall(codec: Codec) -> Option<u64> {
    faults().stalls[slot(codec)].take()
}

pub(crate) fn take_nvenc_failure() -> Option<u64> {
    faults().nvenc_fails.take()
}

pub(crate) fn take_software_failure() -> Option<u64> {
    faults().software_fails.take()
}

pub(crate) fn take_software_refusal() -> bool {
    std::mem::take(&mut faults().software_refused)
}

/// Media Foundation hardware encoders started and not yet shut down with
/// their event thread ended. One whose thread had not ended within the 2 s
/// its shutdown waits stays counted for good, since its driver may still
/// hold the encoder.
pub fn live() -> usize {
    LIVE.load(Ordering::SeqCst)
}

/// How many of them were live as the last one started.
pub fn live_at_last_open() -> usize {
    LIVE_AT_LAST_OPEN.load(Ordering::SeqCst)
}

pub(crate) fn started() {
    LIVE_AT_LAST_OPEN.store(LIVE.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
}

pub(crate) fn shut_down(thread_ended: bool) {
    if thread_ended {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}
