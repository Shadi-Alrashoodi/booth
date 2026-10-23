// Hearing: one jitter buffer per talker, filled by the receive thread and
// pulled by the render thread through the voice crate's mixer, and the
// mouth-to-ear time of every frame that leaves the speaker. The PC sound of
// the share watched plays through the same mixer (sound.rs).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;
use voice::codec::{CodecError, SAMPLE_RATE};
use voice::jitter::{JitterBuffer, JitterStats, Source, VoicePacket};
use voice::mix::{FrameSource, Mixer};

use super::cue::{self, Tone};
use super::{Frame, RenderNow, Shared, lock};
use crate::peer::Clock;
use crate::sound::{self, SOUND_LEVEL};
use crate::view::{MouthToEar, VoiceLoss};

// Capture times remembered by sequence number: more than the 60 ms the
// buffer can hold at 5 ms frames, with room for what arrives early.
const STAMPS: usize = 64;
// Mouth to ear is shown as the last value and over the last 10 s. 10 s of
// 5 ms frames.
pub(crate) const WINDOW_US: u64 = 10_000_000;
pub(crate) const SAMPLES_KEPT: usize = 2048;
// As chat delivery: past this, or this far before its capture, a time is a
// clock or a peer that is wrong.
pub(crate) const LONGEST_US: i64 = 60_000_000;
pub(crate) const AHEAD_US: i64 = 1_000_000;

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

    pub(crate) fn gauge(&self) {
        stats::gauge!("room.ear.samples", self.samples);
        self.buffer.gauge();
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

impl RenderNow {
    // A frame pulled now starts where the one before it ended, `next_start`
    // in frames since the stream began, or at the start of the callback
    // under way when its source went quiet in between. That place, and when
    // the frame's first sample leaves the speaker, on the ping clock.
    pub(crate) fn place(&self, next_start: u64) -> (u64, u64) {
        let callback_start = self.start_sample.load(Ordering::Relaxed);
        let start = next_start.max(callback_start);
        let into_callback = (start - callback_start) * 1_000_000 / u64::from(SAMPLE_RATE);
        let ear_us = self.start_us.load(Ordering::Relaxed)
            + into_callback
            + self.latency_us.load(Ordering::Relaxed);
        (start, ear_us)
    }
}

impl FrameSource for Played {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        let mut ear = lock(&self.ear);
        let pulled = ear.buffer.pull(out)?;
        let (start, ear_us) = self.shared.render_now.place(self.next_start);
        self.next_start = start + pulled.samples as u64;
        if let Source::Packet(seq) = pulled.source {
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
    // With the volume this PC plays them at, 1.0 as they come.
    Add([u8; 32], Arc<Mutex<Ear>>, f32),
    Remove([u8; 32]),
    Volume([u8; 32], f32),
    // The PC sound of the share watched, by its sharer's key, with the
    // volume this PC plays it at, 1.0 for 100 percent, which is SOUND_LEVEL
    // of what was captured.
    AddStream([u8; 32], Arc<Mutex<sound::Ear>>, f32),
    RemoveStream([u8; 32]),
    StreamVolume([u8; 32], f32),
}

// What the mixer plays: every talker, the PC sound of the share watched, and
// the share cue, which goes through deafen and the limiter as they do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Talker([u8; 32]),
    Sharer([u8; 32]),
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

// The render callback: every talker mixed into what the device asks for, in
// stereo, the talkers in the middle and PC sound on its own sides.
pub(crate) struct Speaker {
    mixer: Mixer<Key, Sound, sound::Stream>,
    // The control cue, which plays past the mixer (cue.rs).
    control: Tone,
    orders: Receiver<ToSpeaker>,
    shared: Arc<Shared>,
    clock: Clock,
    // Frames played since the stream began.
    rendered: u64,
    period: usize,
}

impl Speaker {
    pub(crate) fn new(orders: Receiver<ToSpeaker>, shared: Arc<Shared>, clock: Clock) -> Speaker {
        // The cue stays in the mixer, idle between cues, so starting one
        // never adds to the mixer's list on the render thread.
        let mut mixer = Mixer::with_streams();
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
                ToSpeaker::Add(key, ear, gain) => {
                    let played = Played {
                        ear,
                        shared: Arc::clone(&self.shared),
                        next_start: 0,
                    };
                    self.mixer.add(Key::Talker(key), Sound::Talker(played));
                    self.mixer.set_gain(&Key::Talker(key), gain);
                }
                // The room keeps its own reference for a while, so the
                // decoder is not freed here.
                ToSpeaker::Remove(key) => {
                    self.mixer.remove(&Key::Talker(key));
                }
                ToSpeaker::Volume(key, gain) => self.mixer.set_gain(&Key::Talker(key), gain),
                ToSpeaker::AddStream(key, ear, gain) => {
                    let stream = sound::Stream::new(ear, Arc::clone(&self.shared));
                    self.mixer.add_stream(Key::Sharer(key), stream);
                    self.mixer
                        .set_stream_gain(&Key::Sharer(key), SOUND_LEVEL * gain);
                }
                // As for a talker, the decoder is not freed here.
                ToSpeaker::RemoveStream(key) => {
                    self.mixer.remove_stream(&Key::Sharer(key));
                }
                ToSpeaker::StreamVolume(key, gain) => self
                    .mixer
                    .set_stream_gain(&Key::Sharer(key), SOUND_LEVEL * gain),
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
        self.mixer.render_stereo(out);
        if let Some(cue) = self.shared.take_control_cue(began_us) {
            self.control.start(cue);
        }
        cue::mix_over(&mut self.control, out);
        self.rendered += (out.len() / 2) as u64;
        self.shared.render_times.record(began.elapsed());
    }
}

// The render callback driven by hand, so the timing math is checked without
// a device.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::talk::{Cue, Shared, VoiceConfig};
    use std::f32::consts::TAU;
    use voice::codec::{Encoder, Mode};
    use voice::stereo::{FRAME, MAX_PACKET};

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
            .send(ToSpeaker::Add([1; 32], Arc::clone(&ear), 1.0))
            .unwrap();
        shared.render_now.latency_us.store(5000, Ordering::Relaxed);
        // 240 frames of stereo.
        let mut out = [0.0f32; 480];
        let mut heard_any = false;
        for (seq, bytes) in frames(20).iter().enumerate() {
            let captured = clock.micros(Instant::now()) - 10_000;
            assert!(lock(&ear).push(&frame(seq as u16, bytes), Some(captured), false));
            speaker.fill(&mut out);
            heard_any |= out.iter().any(|&s| s != 0.0);
            for [left, right] in out.as_chunks::<2>().0 {
                assert_eq!(left.to_bits(), right.to_bits());
            }
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

    // 10 ms stereo frames as a sharer's encoder makes them: 500 Hz at 0.4
    // on the left, 700 Hz at 0.2 on the right.
    fn stereo_frames(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = voice::stereo::Encoder::new().unwrap();
        (0..count)
            .map(|i| {
                let pcm: Vec<f32> = (0..FRAME)
                    .flat_map(|n| {
                        let t = (i * FRAME + n) as f32 / 48_000.0;
                        [0.4 * (TAU * 500.0 * t).sin(), 0.2 * (TAU * 700.0 * t).sin()]
                    })
                    .collect();
                let mut out = [0u8; MAX_PACKET];
                let len = encoder.encode(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    }

    fn sound_frame(seq: u16, bytes: &[u8]) -> sound::SoundFrame<'_> {
        sound::SoundFrame {
            slot: 2,
            share: 4,
            seq,
            captured: 1,
            first: seq == 0,
            last: false,
            frame: bytes,
        }
    }

    fn peak(frames: &[[f32; 2]], side: usize) -> f32 {
        frames
            .iter()
            .fold(0.0f32, |most, frame| most.max(frame[side].abs()))
    }

    // PC sound is timed as voice is, in 10 ms callbacks: each frame arrives
    // 10 ms after its capture and goes out at the start of the next
    // callback, with 5 ms of render latency, 17.5 ms counting the codec's
    // lookahead. Left and right keep to their sides, at SOUND_LEVEL.
    #[test]
    fn pc_sound_capture_to_ear_timing() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let ear = Arc::new(Mutex::new(sound::Ear::new().unwrap()));
        orders_in
            .send(ToSpeaker::AddStream([2; 32], Arc::clone(&ear), 1.0))
            .unwrap();
        shared.render_now.latency_us.store(5000, Ordering::Relaxed);
        // 480 frames of stereo.
        let mut out = [0.0f32; 960];
        let (mut left, mut right) = (0.0f32, 0.0f32);
        for (seq, bytes) in stereo_frames(20).iter().enumerate() {
            let captured = clock.micros(Instant::now()) - 10_000;
            let frame = sound_frame(seq as u16, bytes);
            assert!(lock(&ear).push(&frame, Some((captured, false))));
            speaker.fill(&mut out);
            // Past the fade in at the start.
            if seq >= 2 {
                let (frames, _) = out.as_chunks::<2>();
                left = left.max(peak(frames, 0));
                right = right.max(peak(frames, 1));
            }
        }
        let mut recent = Vec::new();
        let about = lock(&ear).heard(clock.micros(Instant::now()), &mut recent);
        let (median, p95) = share::spread(&mut recent).expect("capture to ear times");
        println!(
            "pc sound capture to ear {median:.2} ms, p95 {p95:.2} ms, peak left {left:.3}, right {right:.3}"
        );
        for ms in [median, p95] {
            assert!((17.4..19.0).contains(&ms), "{median} {p95}");
        }
        assert!(!about);
        assert!((0.17..0.23).contains(&left), "{left}");
        assert!((0.085..0.115).contains(&right), "{right}");
        assert_eq!(lock(&ear).stats().depth_frames, 1);
    }

    // The volume asked is applied at SOUND_LEVEL and ramped as a talker's,
    // and PC sound let go plays its next 2.5 ms fading out, from where it
    // was, then nothing.
    #[test]
    fn pc_sound_volume_and_its_fade_out() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let ear = Arc::new(Mutex::new(sound::Ear::new().unwrap()));
        orders_in
            .send(ToSpeaker::AddStream([2; 32], Arc::clone(&ear), 1.0))
            .unwrap();
        let frames = stereo_frames(32);
        // One callback, 10 ms, with frame `seq` in just before it.
        let mut callback = |seq: usize| {
            let frame = sound_frame(seq as u16, &frames[seq]);
            assert!(lock(&ear).push(&frame, None));
            let mut out = [0.0f32; 960];
            speaker.fill(&mut out);
            out
        };
        let left = |out: &[f32; 960], from: usize| peak(&out.as_chunks::<2>().0[from..], 0);
        let mut full = 0.0f32;
        for seq in 0..10 {
            let out = callback(seq);
            if seq >= 2 {
                full = full.max(left(&out, 0));
            }
        }
        orders_in
            .send(ToSpeaker::StreamVolume([2; 32], 1.5))
            .unwrap();
        let mut louder = 0.0f32;
        for seq in 10..20 {
            let out = callback(seq);
            if seq >= 11 {
                louder = louder.max(left(&out, 0));
            }
        }
        orders_in
            .send(ToSpeaker::StreamVolume([2; 32], 0.0))
            .unwrap();
        callback(20);
        let muted = callback(21);
        orders_in
            .send(ToSpeaker::StreamVolume([2; 32], 1.0))
            .unwrap();
        let mut before = 0.0;
        for seq in 22..30 {
            before = callback(seq)[958];
        }
        orders_in.send(ToSpeaker::RemoveStream([2; 32])).unwrap();
        let gone = callback(30);
        let (gone, _) = gone.as_chunks::<2>();
        let after = callback(31);
        println!(
            "pc sound peaks: {full:.3} at 100 percent, {louder:.3} at 150; first sample let go {:.3} after {before:.3}",
            gone[0][0]
        );
        assert!((0.17..0.23).contains(&full), "{full}");
        assert!((0.26..0.34).contains(&louder), "{louder}");
        assert!(muted.iter().all(|&s| s == 0.0));
        // No step where it was let go, a fade, then silence.
        assert!((gone[0][0] - before).abs() < 0.04);
        assert!(peak(&gone[..60], 0) > 0.08);
        assert!(peak(&gone[100..], 0) < 0.04 && peak(&gone[100..], 1) < 0.04);
        assert!(gone[120..].iter().all(|&[l, r]| l == 0.0 && r == 0.0));
        assert!(after.iter().all(|&s| s == 0.0));
    }

    // PC sound added at a volume of 0 plays nothing from its first callback:
    // a stream that has not played takes its volume whole, where a ramp
    // down from 100 percent would let its start through.
    #[test]
    fn pc_sound_at_no_volume_plays_nothing_from_the_start() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let ear = Arc::new(Mutex::new(sound::Ear::new().unwrap()));
        orders_in
            .send(ToSpeaker::AddStream([2; 32], Arc::clone(&ear), 0.0))
            .unwrap();
        for (seq, bytes) in stereo_frames(10).iter().enumerate() {
            assert!(lock(&ear).push(&sound_frame(seq as u16, bytes), None));
            let mut out = [0.0f32; 960];
            speaker.fill(&mut out);
            assert!(out.iter().all(|&s| s == 0.0), "callback {seq}");
        }
        assert!(lock(&ear).stats().held_frames <= 1);
    }

    // Deafened, the speakers play silence, the share cue included, and a
    // cue asked for then is not saved up for when it ends.
    #[test]
    fn deafen_drops_share_cue() {
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let clock = Clock::new(Instant::now());
        let (_orders_in, orders) = crossbeam_channel::unbounded();
        let mut speaker = Speaker::new(orders, Arc::clone(&shared), clock);
        let mut out = [0.0f32; 480];
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
        let mut out = [0.0f32; 480];
        // The left side; the right is the same.
        let mut heard = |speaker: &mut Speaker| {
            let mut samples = Vec::new();
            for _ in 0..40 {
                speaker.fill(&mut out);
                for [left, right] in out.as_chunks::<2>().0 {
                    assert_eq!(left.to_bits(), right.to_bits());
                    samples.push(*left);
                }
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
