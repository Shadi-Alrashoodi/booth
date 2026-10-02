// Hearing: one jitter buffer per talker, filled by the receive thread and
// pulled by the render thread through the voice crate's mixer, and the
// mouth-to-ear time of every frame that leaves the speaker.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;
use voice::codec::{CodecError, SAMPLE_RATE};
use voice::jitter::{JitterBuffer, JitterStats, Source, VoicePacket};
use voice::mix::{FrameSource, Mixer};

use super::cue::{self, Tone};
use super::{Frame, Shared, lock};
use crate::peer::Clock;
use crate::view::{MouthToEar, VoiceLoss};

// Capture times remembered by sequence number: more than the 60 ms the
// buffer can hold at 5 ms frames, with room for what arrives early.
const STAMPS: usize = 64;
// Mouth to ear is shown as the last value and over the last 10 s. 10 s of
// 5 ms frames.
const WINDOW_US: u64 = 10_000_000;
pub(crate) const SAMPLES_KEPT: usize = 2048;
// As chat delivery: past this, or this far before its capture, a time is a
// clock or a peer that is wrong.
const LONGEST_US: i64 = 60_000_000;
const AHEAD_US: i64 = 1_000_000;

#[derive(Clone, Copy)]
struct Stamp {
    seq: u16,
    // On this PC's ping clock, already moved back by the codec's lookahead:
    // when what the frame's first decoded sample holds was spoken.
    spoken_us: u64,
    about: bool,
}

#[derive(Clone, Copy)]
struct Sample {
    at_us: u64,
    ms: f32,
    about: bool,
}

pub(crate) struct Ear {
    buffer: JitterBuffer,
    stamps: [Option<Stamp>; STAMPS],
    samples: VecDeque<Sample>,
}

impl Ear {
    pub(crate) fn new() -> Result<Ear, CodecError> {
        Ok(Ear {
            buffer: JitterBuffer::new()?,
            stamps: [None; STAMPS],
            samples: VecDeque::with_capacity(SAMPLES_KEPT),
        })
    }

    // False when the buffer would not take it.
    pub(crate) fn push(&mut self, frame: &Frame, captured: Option<u64>, about: bool) -> bool {
        if let Some(captured) = captured {
            let lookahead_us =
                frame.mode.lookahead_samples() as u64 * 1_000_000 / u64::from(SAMPLE_RATE);
            self.stamps[usize::from(frame.seq) % STAMPS] = Some(Stamp {
                seq: frame.seq,
                spoken_us: captured.saturating_sub(lookahead_us),
                about,
            });
        }
        self.buffer
            .push(VoicePacket {
                seq: frame.seq,
                frame: frame.frame,
                previous: frame.previous,
                redundancy: frame.redundancy,
                last: frame.last,
            })
            .is_ok()
    }

    pub(crate) fn stats(&self) -> JitterStats {
        self.buffer.stats()
    }

    pub(crate) fn loss(&self) -> VoiceLoss {
        let stats = self.buffer.stats();
        VoiceLoss {
            all_pct: stats.loss_percent,
            scattered_pct: stats.scattered_percent,
        }
    }

    // A frame left for the speaker at `ear_us`, on this PC's ping clock.
    fn played(&mut self, seq: u16, ear_us: u64) {
        let Some(stamp) = self.stamps[usize::from(seq) % STAMPS].filter(|stamp| stamp.seq == seq)
        else {
            return;
        };
        let took = ear_us.wrapping_sub(stamp.spoken_us) as i64;
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

    // The times over the last 10 s, copied into `recent`, and the last one.
    // Only the copy is done under the talker's lock, which the render
    // thread takes for every pull; mouth_to_ear sorts them after.
    pub(crate) fn heard(&self, now_us: u64, recent: &mut Vec<f32>) -> Option<Heard> {
        let last = *self.samples.back()?;
        recent.clear();
        let mut about = last.about;
        for sample in &self.samples {
            if now_us.saturating_sub(sample.at_us) < WINDOW_US {
                recent.push(sample.ms);
                about |= sample.about;
            }
        }
        Some(Heard {
            last_ms: last.ms,
            about,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Heard {
    last_ms: f32,
    about: bool,
}

// The last time, and the average and 95th percentile of `recent`. The name
// is the caller's to fill in.
pub(crate) fn mouth_to_ear(heard: Heard, recent: &mut Vec<f32>) -> MouthToEar {
    if recent.is_empty() {
        recent.push(heard.last_ms);
    }
    let avg = recent.iter().sum::<f32>() / recent.len() as f32;
    recent.sort_unstable_by(f32::total_cmp);
    let p95 = recent[((recent.len() - 1) as f32 * 0.95).round() as usize];
    MouthToEar {
        name: String::new(),
        last_ms: heard.last_ms,
        avg_ms: avg,
        p95_ms: p95,
        about: heard.about,
    }
}

// What the mixer pulls a talker's frames through: the buffer's lock is
// held for one pull, and the frame is timed on its way out.
struct Played {
    ear: Arc<Mutex<Ear>>,
    shared: Arc<Shared>,
    // Where, in samples since the stream began, this talker's next frame
    // starts playing. The mixer pulls a frame when the one before has run
    // out, so that is where the last one ended, unless the talker went
    // quiet in between, when it is the start of the callback that pulls.
    next_start: u64,
}

impl FrameSource for Played {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        let mut ear = lock(&self.ear);
        let pulled = ear.buffer.pull(out)?;
        let now = &self.shared.render_now;
        let callback_start = now.start_sample.load(Ordering::Relaxed);
        let start = self.next_start.max(callback_start);
        self.next_start = start + pulled.samples as u64;
        if let Source::Packet(seq) = pulled.source {
            let into_callback = (start - callback_start) * 1_000_000 / u64::from(SAMPLE_RATE);
            let ear_us = now.start_us.load(Ordering::Relaxed)
                + into_callback
                + now.latency_us.load(Ordering::Relaxed);
            ear.played(seq, ear_us);
        }
        Some(pulled.samples)
    }

    fn pause(&mut self) {
        lock(&self.ear).buffer.pause();
        self.next_start = 0;
    }

    fn set_period(&mut self, samples: usize) {
        lock(&self.ear).buffer.set_period(samples);
    }
}

pub(crate) enum ToSpeaker {
    Add([u8; 32], Arc<Mutex<Ear>>),
    Remove([u8; 32]),
}

// What the mixer plays: every talker, and the share cue, which goes through
// deafen and the limiter as they do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Talker([u8; 32]),
    Cue,
}

enum Sound {
    Talker(Played),
    Cue(Tone),
}

impl FrameSource for Sound {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        match self {
            Sound::Talker(played) => played.next_frame(out),
            Sound::Cue(tone) => tone.next_frame(out),
        }
    }

    fn pause(&mut self) {
        match self {
            Sound::Talker(played) => played.pause(),
            Sound::Cue(tone) => tone.pause(),
        }
    }

    fn set_period(&mut self, samples: usize) {
        if let Sound::Talker(played) = self {
            played.set_period(samples);
        }
    }
}

// The render callback: every talker mixed into what the device asks for.
pub(crate) struct Speaker {
    mixer: Mixer<Key, Sound>,
    // The control cue, which plays past the mixer (cue.rs).
    control: Tone,
    orders: Receiver<ToSpeaker>,
    shared: Arc<Shared>,
    clock: Clock,
    // Samples played since the stream began.
    rendered: u64,
    period: usize,
}

impl Speaker {
    pub(crate) fn new(orders: Receiver<ToSpeaker>, shared: Arc<Shared>, clock: Clock) -> Speaker {
        // The cue stays in the mixer, idle between cues, so starting one
        // never adds to the mixer's list on the render thread.
        let mut mixer = Mixer::new();
        mixer.add(Key::Cue, Sound::Cue(Tone::idle()));
        Speaker {
            mixer,
            control: Tone::control(),
            orders,
            shared,
            clock,
            rendered: 0,
            period: 0,
        }
    }

    pub(crate) fn fill(&mut self, out: &mut [f32]) {
        let began = Instant::now();
        while let Ok(order) = self.orders.try_recv() {
            match order {
                ToSpeaker::Add(key, ear) => {
                    let played = Played {
                        ear,
                        shared: Arc::clone(&self.shared),
                        next_start: 0,
                    };
                    self.mixer.add(Key::Talker(key), Sound::Talker(played));
                }
                // The room keeps its own reference for a while, so the
                // decoder is not freed here.
                ToSpeaker::Remove(key) => {
                    self.mixer.remove(&Key::Talker(key));
                }
            }
        }
        let began_us = self.clock.micros(began);
        if let Some(cue) = self.shared.take_cue(began_us)
            && let Some(Sound::Cue(tone)) = self.mixer.source_mut(&Key::Cue)
        {
            tone.start(cue);
        }
        let now = &self.shared.render_now;
        let period = now.period.load(Ordering::Relaxed) as usize;
        if period != self.period {
            self.period = period;
            self.mixer.set_period(period);
        }
        self.mixer.set_deafened(self.shared.deafened());
        now.start_sample.store(self.rendered, Ordering::Relaxed);
        now.start_us.store(began_us, Ordering::Relaxed);
        self.mixer.render(out);
        if let Some(cue) = self.shared.take_control_cue(began_us) {
            self.control.start(cue);
        }
        cue::mix_over(&mut self.control, out);
        self.rendered += out.len() as u64;
        self.shared.render_times.record(began.elapsed());
    }
}

// The render callback driven by hand, so the timing math is checked without
// a device.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::talk::{Cue, Shared, VoiceConfig};
    use voice::codec::{Encoder, Mode};

    fn frames(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
        (0..count)
            .map(|i| {
                let pcm: Vec<f32> = (0..240)
                    .map(|n| 0.3 * ((i * 240 + n) as f32 * 0.05).sin())
                    .collect();
                let mut out = [0u8; 20];
                let len = encoder.encode(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    }

    fn frame(seq: u16, bytes: &[u8]) -> Frame<'_> {
        Frame {
            seq,
            captured: 0,
            mode: Mode::LowDelay,
            redundancy: false,
            last: false,
            frame: bytes,
            previous: None,
            pad: 0,
        }
    }

    // Each frame arrives 10 ms after its capture and goes out at the start of
    // the next callback, with 5 ms of render latency: 17.5 ms counting the
    // codec's 2.5 ms lookahead, which the capture time is moved back by.
    #[test]
    fn mouth_to_ear_timing() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let ear = Arc::new(Mutex::new(Ear::new().unwrap()));
        orders_in
            .send(ToSpeaker::Add([1; 32], Arc::clone(&ear)))
            .unwrap();
        shared.render_now.latency_us.store(5000, Ordering::Relaxed);
        let mut out = [0.0f32; 240];
        let mut heard_any = false;
        for (seq, bytes) in frames(20).iter().enumerate() {
            let captured = clock.micros(Instant::now()) - 10_000;
            assert!(lock(&ear).push(&frame(seq as u16, bytes), Some(captured), false));
            speaker.fill(&mut out);
            heard_any |= out.iter().any(|&s| s != 0.0);
        }
        let mut recent = Vec::new();
        let last = lock(&ear)
            .heard(clock.micros(Instant::now()), &mut recent)
            .unwrap();
        let heard = mouth_to_ear(last, &mut recent);
        println!(
            "mouth to ear {:.2} ms, p95 {:.2} ms, last {:.2} ms",
            heard.avg_ms, heard.p95_ms, heard.last_ms
        );
        assert!(heard_any);
        for ms in [heard.avg_ms, heard.p95_ms, heard.last_ms] {
            assert!((17.4..19.0).contains(&ms), "{heard:?}");
        }
        assert!(!heard.about);
        assert_eq!(lock(&ear).stats().depth_frames, 1);
    }

    // Deafened, the speakers play silence, the share cue included, and a
    // cue asked for then is not saved up for when it ends.
    #[test]
    fn deafen_drops_share_cue() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (_orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let mut out = [0.0f32; 240];
        // The loudest sample over the next 200 ms.
        let mut loudest = |speaker: &mut Speaker| {
            (0..40).fold(0.0f32, |most, _| {
                speaker.fill(&mut out);
                out.iter().fold(most, |most, s| most.max(s.abs()))
            })
        };
        shared.set_deafened(true);
        loudest(&mut speaker);
        shared.cue(Cue::Rising, clock.micros(Instant::now()));
        assert_eq!(loudest(&mut speaker), 0.0);
        shared.set_deafened(false);
        assert_eq!(loudest(&mut speaker), 0.0);
        shared.cue(Cue::Falling, clock.micros(Instant::now()));
        assert!(loudest(&mut speaker) > 0.1);
    }

    // The control cue is the indicator over a game in exclusive fullscreen,
    // so deafen does not silence it, while the share cue asked for at the
    // same moment stays silent. Deafen lets the control cue through at its
    // own level, and neither cue takes the other's slot.
    #[test]
    fn deafen_keeps_control_cue() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (_orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let mut out = [0.0f32; 240];
        let mut heard = |speaker: &mut Speaker| {
            let mut samples = Vec::new();
            for _ in 0..40 {
                speaker.fill(&mut out);
                samples.extend_from_slice(&out);
            }
            samples
        };
        shared.set_deafened(true);
        heard(&mut speaker);
        let now = clock.micros(Instant::now());
        shared.cue(Cue::Rising, now);
        shared.control_cue(Cue::Rising, now);
        let samples = heard(&mut speaker);
        let loudest = samples.iter().fold(0.0f32, |most, s| most.max(s.abs()));
        let sounding = samples.iter().filter(|s| s.abs() > 1e-3).count();
        println!("deafened: control cue peak {loudest:.3}, {sounding} samples");
        assert!(loudest > 0.15 && loudest <= 0.2 + 1e-6, "{loudest}");
        // Two 80 ms notes with their quiet ends: the control cue alone, the
        // share cue would make it twice as long.
        assert!((7000..=7680).contains(&sounding), "{sounding}");
        // D5 first: about 587 crossings a second, so about 94 in its note,
        // well short of the share cue's G5, about 125.
        let first_note = &samples[..3840];
        let crossings = first_note
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        assert!((85..=100).contains(&crossings), "{crossings}");
    }
}
