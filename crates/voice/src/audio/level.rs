// The level meter. The capture thread writes, the panel reads whenever it
// draws, so the two share nothing but two atomics and neither ever waits.

use std::sync::atomic::{AtomicU32, Ordering};

use super::format::RATE;

// 50 ms, kept as ten 5 ms blocks so the window slides in 5 ms steps.
const BLOCK: u32 = RATE / 200;
const BLOCKS: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    // All 0 to 1, linear, over the last 50 ms.
    pub peak: f32,
    pub rms: f32,
    // The RMS of the loudest 5 ms block: what the open mic gate reads,
    // block by block.
    pub loudest: f32,
}

#[derive(Debug, Default)]
pub struct Level {
    peak: AtomicU32,
    rms: AtomicU32,
    loudest: AtomicU32,
}

impl Level {
    pub fn read(&self) -> Reading {
        // Each number is right on its own; a peak from one block and an RMS
        // from the next is still a fine meter.
        Reading {
            peak: f32::from_bits(self.peak.load(Ordering::Relaxed)),
            rms: f32::from_bits(self.rms.load(Ordering::Relaxed)),
            loudest: f32::from_bits(self.loudest.load(Ordering::Relaxed)),
        }
    }

    fn publish(&self, reading: Reading) {
        self.peak.store(reading.peak.to_bits(), Ordering::Relaxed);
        self.rms.store(reading.rms.to_bits(), Ordering::Relaxed);
        self.loudest
            .store(reading.loudest.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn clear(&self) {
        self.publish(Reading::default());
    }
}

#[derive(Clone, Copy, Default)]
struct Block {
    peak: f32,
    squares: f64,
    count: u32,
}

#[derive(Default)]
pub(crate) struct Meter {
    done: [Block; BLOCKS],
    next: usize,
    open: Block,
}

impl Meter {
    pub(crate) fn push(&mut self, samples: &[f32], level: &Level) {
        for &sample in samples {
            let open = &mut self.open;
            open.peak = open.peak.max(sample.abs());
            open.squares += f64::from(sample) * f64::from(sample);
            open.count += 1;
            if open.count == BLOCK {
                self.done[self.next] = std::mem::take(&mut self.open);
                self.next = (self.next + 1) % BLOCKS;
                level.publish(self.reading());
            }
        }
    }

    fn reading(&self) -> Reading {
        let (peak, squares, count) =
            self.done
                .iter()
                .fold((0.0f32, 0.0f64, 0u32), |(peak, squares, count), block| {
                    (
                        peak.max(block.peak),
                        squares + block.squares,
                        count + block.count,
                    )
                });
        let rms = |squares: f64, count: u32| {
            if count == 0 {
                0.0
            } else {
                (squares / f64::from(count)).sqrt() as f32
            }
        };
        let loudest = self
            .done
            .iter()
            .map(|block| rms(block.squares, block.count))
            .fold(0.0f32, f32::max);
        Reading {
            peak,
            rms: rms(squares, count),
            loudest,
        }
    }
}
