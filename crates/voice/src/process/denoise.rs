// Noise suppression: RNNoise, through nnnoiseless, on 10 ms blocks. What it
// gives back is 10 ms behind what went in, since each block it puts out is
// overlapped from two it took in. With the wait to fill a block, and the two
// 5 ms frames of a block going out together, a room hears it 10 to 20 ms
// later than without it: the most on a device with small periods.

use std::time::{Duration, Instant};

use nnnoiseless::DenoiseState;

use super::Mark;

const BLOCK: usize = DenoiseState::FRAME_SIZE;
pub const DELAY: Duration = Duration::from_millis(10);
// Each block out is shaped by the nine blocks in before it as well: it
// overlaps two frames, and a frame's features look back over eight frames.
const GUARD: usize = 9;
// RNNoise works on samples the size of 16-bit ones.
const SCALE: f32 = 32_768.0;
// A float microphone can go past full scale. Far past it, RNNoise's sums
// overflow, and its filters keep the NaN that follows for good.
const LOUDEST: f32 = 8.0 * SCALE;

pub(super) struct Denoise {
    state: Box<DenoiseState<'static>>,
    block: Vec<f32>,
    // When the first sample in `block` was captured.
    started: Option<Instant>,
    out: [f32; BLOCK],
    // Whether the block being filled holds a marked sample, and as bit i,
    // whether the block i + 1 before it did.
    saw: bool,
    recent: u16,
    // Blocks still to come out marked Settling.
    fresh: usize,
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
            saw: false,
            recent: 0,
            fresh: GUARD,
        }
    }

    // Turned off: a half filled block is dropped, and the next one starts
    // from what comes after.
    pub(super) fn clear(&mut self) {
        self.block.clear();
        self.started = None;
        self.saw = false;
        self.recent = 0;
        self.fresh = GUARD;
    }

    // Appends what is ready to `out`, and says when its first sample was
    // captured; None when nothing was.
    pub(super) fn run(
        &mut self,
        samples: &[f32],
        marks: &[Mark],
        captured: Instant,
        out: &mut Vec<f32>,
        marks_out: &mut Vec<Mark>,
    ) -> Option<Instant> {
        debug_assert_eq!(samples.len(), marks.len());
        let mut first = None;
        for (at, (&sample, &mark)) in samples.iter().zip(marks).enumerate() {
            if self.block.is_empty() {
                let into = Duration::from_nanos(at as u64 * 1_000_000_000 / 48_000);
                self.started = Some(captured + into);
            }
            self.block.push((sample * SCALE).clamp(-LOUDEST, LOUDEST));
            self.saw |= mark != Mark::Room;
            if self.block.len() < BLOCK {
                continue;
            }
            self.state.process_frame(&mut self.out, &self.block);
            self.block.clear();
            if let Some(started) = self.started.take() {
                first.get_or_insert(started.checked_sub(DELAY).unwrap_or(started));
            }
            out.extend(self.out.iter().map(|sample| sample / SCALE));
            let mark = if self.fresh > 0 {
                Mark::Settling
            } else if self.saw || self.recent != 0 {
                Mark::Quiet
            } else {
                Mark::Room
            };
            self.fresh = self.fresh.saturating_sub(1);
            self.recent = ((self.recent << 1) | u16::from(self.saw)) & ((1 << GUARD) - 1);
            self.saw = false;
            marks_out.extend(std::iter::repeat_n(mark, BLOCK));
        }
        first
    }
}
