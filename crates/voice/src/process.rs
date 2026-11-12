// What the microphone goes through before anyone hears it, the same in a
// room and in Settings' microphone test, on the capture thread: noise
// suppression, automatic volume, then the level, after everything else,
// since that is what the meter shows and what the open mic gate reads.
// Each sample also comes out with a mark for the room's floors.

mod denoise;
mod volume;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crate::audio::{Level, Meter};
use denoise::Denoise;
use volume::Volume;

pub use denoise::DELAY as NOISE_SUPPRESSION_DELAY;

// Exact zeros this many in a row, a millisecond, are digital silence: no
// room's own noise stays that still.
pub const SILENT_RUN: usize = 48;
// A millisecond whose mean square is under this, -80 dBFS, is a mute's
// dither or a converter's last bits, not a room.
const NEAR_SILENCE: f32 = 1e-8;

// What a sample is to the room's floors, which open mic and automatic volume
// learn: Quiet is near silence or what noise suppression made of it, and
// Settling noise suppression's first blocks after it starts. Neither is
// learnt, and Settling is no voice either. A block takes its largest mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Room,
    Quiet,
    Settling,
}

// Which stages run, read once a packet: a room sets them when it opens the
// microphone and when Settings saves over it, Settings' test as its switches
// move.
pub struct Switches {
    pub noise_suppression: AtomicBool,
    pub automatic_volume: AtomicBool,
}

impl Switches {
    pub fn new(noise_suppression: bool, automatic_volume: bool) -> Switches {
        Switches {
            noise_suppression: AtomicBool::new(noise_suppression),
            automatic_volume: AtomicBool::new(automatic_volume),
        }
    }
}

pub struct Processor {
    switches: Arc<Switches>,
    marker: Marker,
    denoise: Denoise,
    volume: Volume,
    out: Vec<f32>,
    marks: Vec<Mark>,
    marks_out: Vec<Mark>,
    meter: Meter,
    level: Arc<Level>,
}

impl Processor {
    // Off the capture thread: it allocates.
    pub fn new(switches: Arc<Switches>) -> Processor {
        Processor {
            switches,
            marker: Marker::default(),
            denoise: Denoise::new(),
            volume: Volume::new(),
            out: Vec::with_capacity(4096),
            marks: Vec::with_capacity(4096),
            marks_out: Vec::with_capacity(4096),
            meter: Meter::default(),
            level: Arc::default(),
        }
    }

    // What automatic volume does to the microphone now, in dB.
    pub fn gain_db(&self) -> f32 {
        self.volume.gain_db()
    }

    // What automatic volume takes for the room's noise now, in dB.
    pub fn floor_db(&self) -> f32 {
        self.volume.floor_db()
    }

    // On the capture thread, when the microphone opened again after a gap,
    // or another one did: no half block from before it goes out with what
    // comes after, the meter's next reading holds nothing from before it,
    // and the gain starts over for whichever microphone this is. RNNoise
    // keeps its memory of the last block, since a new one would allocate
    // here; at most its fade out of the old sound is heard.
    pub fn restart(&mut self) {
        self.marker = Marker::default();
        self.denoise.clear();
        self.volume = Volume::new();
        self.meter = Meter::default();
    }

    // For the panel, which reads it whenever it draws.
    pub fn level(&self) -> Arc<Level> {
        Arc::clone(&self.level)
    }

    // One packet of mono 48 kHz captured from `captured` on. What comes out
    // is the same sound processed, and when its first sample was captured:
    // with noise suppression on, in whole 10 ms blocks, none at all from a
    // packet that did not finish one.
    pub fn run(&mut self, samples: &[f32], captured: Instant) -> (&[f32], Instant) {
        let (out, _, first) = self.run_marked(samples, captured);
        (out, first)
    }

    // The same, with each sample's mark for the open mic floor.
    pub fn run_marked(&mut self, samples: &[f32], captured: Instant) -> (&[f32], &[Mark], Instant) {
        self.marker.mark(samples, &mut self.marks);
        self.out.clear();
        self.marks_out.clear();
        let mut first = captured;
        let suppressing = self.switches.noise_suppression.load(Ordering::Relaxed);
        if suppressing {
            let marks = &self.marks;
            if let Some(at) =
                self.denoise
                    .run(samples, marks, captured, &mut self.out, &mut self.marks_out)
            {
                first = at;
            }
        } else {
            self.denoise.clear();
            self.out.extend_from_slice(samples);
        }
        let marks = if suppressing {
            &self.marks_out
        } else {
            &self.marks
        };
        if self.switches.automatic_volume.load(Ordering::Relaxed) {
            self.volume.run(&mut self.out, marks);
        }
        self.meter.push(&self.out, &self.level);
        (&self.out, marks, first)
    }
}

#[derive(Default)]
struct Marker {
    squares: f32,
    count: usize,
    zeros: usize,
}

impl Marker {
    // Windows of SILENT_RUN count from the stream's start, as the floors'
    // blocks do, so with noise suppression off one falls inside a block
    // unless a switch has moved the blocks since.
    fn mark(&mut self, samples: &[f32], marks: &mut Vec<Mark>) {
        marks.clear();
        for &sample in samples {
            marks.push(Mark::Room);
            self.zeros = if sample == 0.0 { self.zeros + 1 } else { 0 };
            if self.zeros == SILENT_RUN {
                quiet(marks, SILENT_RUN);
            } else if self.zeros > SILENT_RUN {
                quiet(marks, 1);
            }
            self.squares += sample * sample;
            self.count += 1;
            if self.count == SILENT_RUN {
                if self.squares < SILENT_RUN as f32 * NEAR_SILENCE {
                    quiet(marks, SILENT_RUN);
                }
                self.squares = 0.0;
                self.count = 0;
            }
        }
    }
}

// The last `samples` marks Quiet, as far back as this packet goes.
fn quiet(marks: &mut [Mark], samples: usize) {
    let from = marks.len().saturating_sub(samples);
    marks[from..].fill(Mark::Quiet);
}

// At most 20 ms waits between the microphone and the speakers; past that the
// oldest goes, so two devices whose clocks drift apart never add delay.
const PLAYBACK_MOST: usize = 960;
// Close to full scale for 300 ms on end is the speakers feeding the
// microphone, not a voice.
const TRIP_PEAK: f32 = 0.708;
const TRIP_SAMPLES: usize = 14_400;

// Settings' Hear yourself: the processed microphone handed from the capture
// thread to a render thread, each holding the lock only to copy. Nothing is
// added to it: what plays is what a room would send.
#[derive(Clone, Default)]
pub struct Playback(Arc<Mutex<Queue>>);

#[derive(Default)]
struct Queue {
    on: bool,
    tripped: bool,
    samples: VecDeque<f32>,
    // Samples in a row in packets that came close to full scale.
    loud: usize,
}

impl Playback {
    // Off the audio threads: turning it on makes room for the queue.
    pub fn set_on(&self, on: bool) {
        let mut queue = lock(&self.0);
        queue.on = on;
        queue.samples.clear();
        queue.loud = 0;
        if on {
            queue.tripped = false;
            queue.samples.reserve(PLAYBACK_MOST + 4096);
        }
    }

    // Turned itself off: the microphone heard the speakers.
    pub fn tripped(&self) -> bool {
        lock(&self.0).tripped
    }

    // On the capture thread. With noise suppression on, a packet that did
    // not finish a block gives nothing, which says nothing about feedback.
    pub fn push(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let mut queue = lock(&self.0);
        if !queue.on {
            return;
        }
        let peak = samples.iter().fold(0.0f32, |most, s| most.max(s.abs()));
        queue.loud = if peak >= TRIP_PEAK {
            queue.loud + samples.len()
        } else {
            0
        };
        if queue.loud >= TRIP_SAMPLES {
            queue.on = false;
            queue.tripped = true;
            queue.samples.clear();
            return;
        }
        queue.samples.extend(samples);
        let over = queue.samples.len().saturating_sub(PLAYBACK_MOST);
        queue.samples.drain(..over);
    }

    // On the render thread: what has come, then silence.
    pub fn fill(&self, out: &mut [f32]) {
        let mut queue = lock(&self.0);
        let have = queue.samples.len().min(out.len());
        for (to, from) in out.iter_mut().zip(queue.samples.drain(..have)) {
            *to = from;
        }
        out[have..].fill(0.0);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
