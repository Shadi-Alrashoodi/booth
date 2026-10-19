// What the microphone goes through before anyone hears it, the same in a
// room and in Settings' microphone test, on the capture thread. For now
// only its level is taken, after everything else, since that is what the
// meter shows and what the open mic gate reads.

use std::sync::Arc;
use std::time::Instant;

use crate::audio::{Level, Meter};

pub struct Processor {
    out: Vec<f32>,
    meter: Meter,
    level: Arc<Level>,
}

impl Processor {
    // Off the capture thread: it allocates.
    pub fn new() -> Processor {
        Processor {
            out: Vec::with_capacity(4096),
            meter: Meter::default(),
            level: Arc::default(),
        }
    }

    // For the panel, which reads it whenever it draws.
    pub fn level(&self) -> Arc<Level> {
        Arc::clone(&self.level)
    }

    // One packet of mono 48 kHz captured from `captured` on. What comes out
    // is the same sound processed, and when its first sample was captured.
    pub fn run(&mut self, samples: &[f32], captured: Instant) -> (&[f32], Instant) {
        self.out.clear();
        self.out.extend_from_slice(samples);
        self.meter.push(&self.out, &self.level);
        (&self.out, captured)
    }
}

impl Default for Processor {
    fn default() -> Processor {
        Processor::new()
    }
}
