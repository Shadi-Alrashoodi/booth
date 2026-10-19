// Noise suppression: RNNoise, through nnnoiseless, on 10 ms blocks. What it
// gives back is 10 ms behind what went in, since each block it puts out is
// overlapped from two it took in. With the wait to fill a block, and the two
// 5 ms frames of a block going out together, a room hears it 10 to 20 ms
// later than without it: the most on a device with small periods.

use std::time::{Duration, Instant};

use nnnoiseless::DenoiseState;

const BLOCK: usize = DenoiseState::FRAME_SIZE;
pub const DELAY: Duration = Duration::from_millis(10);
// RNNoise works on samples the size of 16-bit ones.
const SCALE: f32 = 32_768.0;

pub(super) struct Denoise {
    state: Box<DenoiseState<'static>>,
    block: Vec<f32>,
    // When the first sample in `block` was captured.
    started: Option<Instant>,
    out: [f32; BLOCK],
}

impl Denoise {
    // Off the capture thread: the state allocates, and its first block
    // makes the transforms it keeps and fades in from nothing, so a block
    // of silence goes through here first.
    pub(super) fn new() -> Denoise {
        let mut state = DenoiseState::new();
        let mut out = [0.0; BLOCK];
        state.process_frame(&mut out, &[0.0; BLOCK]);
        Denoise {
            state,
            block: Vec::with_capacity(BLOCK),
            started: None,
            out,
        }
    }

    // Turned off: a half filled block is dropped, and the next one starts
    // from what comes after.
    pub(super) fn clear(&mut self) {
        self.block.clear();
        self.started = None;
    }

    // Appends what is ready to `out`, and says when its first sample was
    // captured; None when nothing was.
    pub(super) fn run(
        &mut self,
        samples: &[f32],
        captured: Instant,
        out: &mut Vec<f32>,
    ) -> Option<Instant> {
        let mut first = None;
        for (at, &sample) in samples.iter().enumerate() {
            if self.block.is_empty() {
                let into = Duration::from_nanos(at as u64 * 1_000_000_000 / 48_000);
                self.started = Some(captured + into);
            }
            self.block.push(sample * SCALE);
            if self.block.len() < BLOCK {
                continue;
            }
            self.state.process_frame(&mut self.out, &self.block);
            self.block.clear();
            if let Some(started) = self.started.take() {
                first.get_or_insert(started.checked_sub(DELAY).unwrap_or(started));
            }
            out.extend(self.out.iter().map(|sample| sample / SCALE));
        }
        first
    }
}
