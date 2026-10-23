// Hearing PC sound: the stereo buffer of the share watched, filled by the
// receive thread and pulled by the render thread through the voice crate's
// mixer, each holding the ear's own lock for one frame, and the capture to
// ear time of every frame that leaves the speaker.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use voice::mix::StereoSource;
use voice::stereo::buffer::{Buffer, BufferStats, SoundPacket, Source};
use voice::stereo::{LOOKAHEAD, SAMPLE_RATE, StereoError};

use super::{SoundFrame, lock};
use crate::talk::{AHEAD_US, LONGEST_US, Shared, WINDOW_US};

// Capture times remembered by sequence number: more than the 60 ms the
// buffer can hold, with room for what arrives early.
const STAMPS: usize = 64;
// 10 s of 10 ms frames, with room to spare.
pub(super) const SAMPLES_KEPT: usize = 1024;
const LOOKAHEAD_US: u64 = LOOKAHEAD as u64 * 1_000_000 / SAMPLE_RATE as u64;

#[derive(Clone, Copy)]
struct Stamp {
    seq: u16,
    // On this PC's ping clock, already moved back by the codec's lookahead:
    // when what the frame's first decoded sample holds was captured.
    captured_us: u64,
    about: bool,
}

#[derive(Clone, Copy)]
struct Sample {
    at_us: u64,
    ms: f32,
    about: bool,
}

pub(crate) struct Ear {
    buffer: Buffer,
    stamps: [Option<Stamp>; STAMPS],
    samples: VecDeque<Sample>,
    // The render thread took it from its orders (Stream::new).
    taken: bool,
}

impl Ear {
    pub(crate) fn new() -> Result<Ear, StereoError> {
        Ok(Ear {
            buffer: Buffer::new()?,
            stamps: [None; STAMPS],
            samples: VecDeque::with_capacity(SAMPLES_KEPT),
            taken: false,
        })
    }

    pub(crate) fn taken(&self) -> bool {
        self.taken
    }

    // `captured` is the frame's capture time on this PC's ping clock, and
    // whether it is only about right, once the offsets are known. False
    // when the buffer would not take it.
    pub(crate) fn push(&mut self, frame: &SoundFrame, captured: Option<(u64, bool)>) -> bool {
        if let Some((at_us, about)) = captured {
            self.stamps[usize::from(frame.seq) % STAMPS] = Some(Stamp {
                seq: frame.seq,
                captured_us: at_us.saturating_sub(LOOKAHEAD_US),
                about,
            });
        }
        self.buffer
            .push(SoundPacket {
                seq: frame.seq,
                frame: frame.frame,
                first: frame.first,
                last: frame.last,
            })
            .is_ok()
    }

    pub(crate) fn stats(&self) -> BufferStats {
        self.buffer.stats()
    }

    pub(crate) fn gauge(&self) {
        stats::gauge!("room.sound.samples", self.samples);
    }

    // A frame left for the speaker at `ear_us`, on this PC's ping clock.
    fn played(&mut self, seq: u16, ear_us: u64) {
        let Some(stamp) = self.stamps[usize::from(seq) % STAMPS].filter(|stamp| stamp.seq == seq)
        else {
            return;
        };
        // The capture time came from the sharer's PC: one this far off is a
        // clock or a peer that is wrong, and counts for nothing.
        let took = ear_us.wrapping_sub(stamp.captured_us) as i64;
        if !(-AHEAD_US..=LONGEST_US).contains(&took) {
            return;
        }
        if self.samples.len() == SAMPLES_KEPT {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            at_us: ear_us,
            ms: took.max(0) as f32 / 1000.0,
            about: stamp.about,
        });
    }

    // The times over the last 10 s, copied into `recent`, and whether any of
    // them is only about right. Only the copy is done under the ear's lock,
    // which the render thread takes for every pull; the caller sorts.
    pub(crate) fn heard(&self, now_us: u64, recent: &mut Vec<f32>) -> bool {
        recent.clear();
        let mut about = false;
        for sample in &self.samples {
            if now_us.saturating_sub(sample.at_us) < WINDOW_US {
                recent.push(sample.ms);
                about |= sample.about;
            }
        }
        about
    }
}

// What the mixer pulls the share's sound through, as voice's Played: the
// ear's lock is held for one pull, and the frame is timed on its way out.
pub(crate) struct Stream {
    ear: Arc<Mutex<Ear>>,
    shared: Arc<Shared>,
    // Where, in frames since the stream began, the next frame starts.
    next_start: u64,
}

impl Stream {
    // On the render thread, once for each ear it takes.
    pub(crate) fn new(ear: Arc<Mutex<Ear>>, shared: Arc<Shared>) -> Stream {
        lock(&ear).taken = true;
        Stream {
            ear,
            shared,
            next_start: 0,
        }
    }
}

impl StereoSource for Stream {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        let mut ear = lock(&self.ear);
        let pulled = ear.buffer.pull(out)?;
        let (start, ear_us) = self.shared.render_now().place(self.next_start);
        self.next_start = start + pulled.frames as u64;
        if let Source::Packet(seq) = pulled.source {
            ear.played(seq, ear_us);
        }
        Some(pulled.frames)
    }

    fn pause(&mut self) {
        lock(&self.ear).buffer.pause();
        self.next_start = 0;
    }

    fn set_period(&mut self, frames: usize) {
        lock(&self.ear).buffer.set_period(frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voice::stereo::{Encoder, FRAME, MAX_PACKET};

    fn opus_frame() -> Vec<u8> {
        let mut encoder = Encoder::new().unwrap();
        let mut out = [0u8; MAX_PACKET];
        let len = encoder.encode(&[0.1; FRAME * 2], &mut out).unwrap();
        out[..len].to_vec()
    }

    fn frame(seq: u16, bytes: &[u8]) -> SoundFrame<'_> {
        SoundFrame {
            slot: 2,
            share: 4,
            seq,
            captured: 1,
            first: false,
            last: false,
            frame: bytes,
        }
    }

    // A frame captured 10 ms before it leaves the speaker took 12.5 ms
    // counting the codec's lookahead. A time from more than a minute before,
    // or more than a second after, says nothing and is left out, and a frame
    // with no time, from before the offsets were known, has none.
    #[test]
    fn capture_to_ear_takes_the_lookahead_and_leaves_nonsense_out() {
        let bytes = opus_frame();
        let mut ear = Ear::new().unwrap();
        let now_us = 1_000_000_000;
        let captures = [
            Some(now_us - 10_000),
            Some(now_us - 61_000_000),
            Some(now_us + 2_000_000),
            Some(now_us - 20_000),
            None,
        ];
        for (seq, captured) in captures.into_iter().enumerate() {
            let seq = seq as u16;
            assert!(ear.push(&frame(seq, &bytes), captured.map(|at| (at, seq == 3))));
            ear.played(seq, now_us);
        }
        let mut recent = Vec::new();
        let about = ear.heard(now_us, &mut recent);
        assert_eq!(recent, [12.5, 22.5]);
        assert!(about);
        // Out of the window 10 s later.
        assert!(!ear.heard(now_us + WINDOW_US, &mut recent));
        assert!(recent.is_empty());
    }
}
