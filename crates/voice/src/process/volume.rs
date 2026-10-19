// Automatic volume: a quiet microphone brought up to a steady speech level
// and a loud one down a little, slowly and only while someone speaks, so a
// pause never pumps the room's noise up. It adds no delay: each sample goes
// out at once, at the gain the blocks before it set, through the mixer's
// soft limiter so a word louder than the last never clips.

use crate::mix::limit;

// 5 ms, the open mic gate's blocks.
const BLOCK: usize = 240;
const BLOCKS_PER_SECOND: usize = 200;
// Speech is brought to about this, the RMS of a 5 ms block of it.
const TARGET_DB: f32 = -20.0;
const MOST_DB: f32 = 20.0;
const LEAST_DB: f32 = -10.0;
// How far the gain moves for each block of voice.
const UP_DB: f32 = 6.0 / BLOCKS_PER_SECOND as f32;
const DOWN_DB: f32 = 12.0 / BLOCKS_PER_SECOND as f32;
// A block is voice when it is over this, and this far over the room's floor:
// the quietest block of the last second, rising no faster than FLOOR_RISE_DB
// a block, as the open mic gate keeps it.
const QUIETEST_DB: f32 = -55.0;
const OVER_FLOOR_DB: f32 = 10.0;
const FLOOR_RISE_DB: f32 = 3.0 / BLOCKS_PER_SECOND as f32;
const SILENCE_DB: f32 = -100.0;

pub(super) struct Volume {
    gain_db: f32,
    gain: f32,
    levels: [f32; BLOCKS_PER_SECOND],
    next: usize,
    heard: usize,
    floor: f32,
    squares: f32,
    count: usize,
}

impl Volume {
    pub(super) fn new() -> Volume {
        Volume {
            gain_db: 0.0,
            gain: 1.0,
            levels: [SILENCE_DB; BLOCKS_PER_SECOND],
            next: 0,
            heard: 0,
            floor: SILENCE_DB,
            squares: 0.0,
            count: 0,
        }
    }

    pub(super) fn gain_db(&self) -> f32 {
        self.gain_db
    }

    pub(super) fn run(&mut self, samples: &mut [f32]) {
        for sample in samples {
            let heard = *sample;
            self.squares += heard * heard;
            self.count += 1;
            *sample = limit(heard * self.gain);
            if self.count == BLOCK {
                self.block();
            }
        }
    }

    fn block(&mut self) {
        let power = self.squares / BLOCK as f32;
        let level = if power.is_finite() && power > 0.0 {
            (10.0 * power.log10()).max(SILENCE_DB)
        } else {
            SILENCE_DB
        };
        self.squares = 0.0;
        self.count = 0;
        self.levels[self.next] = level;
        self.next = (self.next + 1) % BLOCKS_PER_SECOND;
        self.heard = (self.heard + 1).min(BLOCKS_PER_SECOND);
        let quietest = self.levels[..self.heard]
            .iter()
            .copied()
            .fold(f32::INFINITY, f32::min);
        self.floor = if self.heard < BLOCKS_PER_SECOND {
            quietest
        } else {
            quietest.min(self.floor + FLOOR_RISE_DB)
        };
        if level < QUIETEST_DB || level < self.floor + OVER_FLOOR_DB {
            return;
        }
        let wanted = (TARGET_DB - level).clamp(LEAST_DB, MOST_DB);
        self.gain_db += (wanted - self.gain_db).clamp(-DOWN_DB, UP_DB);
        self.gain = 10f32.powf(self.gain_db / 20.0);
    }
}
