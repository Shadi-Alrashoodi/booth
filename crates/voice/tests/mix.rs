use std::f32::consts::TAU;

use voice::codec::{Encoder, MAX_FRAME, MAX_PACKET, Mode, SAMPLE_RATE};
use voice::jitter::{JitterBuffer, VoicePacket};
use voice::mix::{FrameSource, KNEE, Mixer, limit};

// Sample n of a counter's stream is (n + offset) mod 4096 over 16384:
// exact in f32 and under the knee even when two are added, so a mix of
// counters can be checked sample for sample.
fn counted(n: u64) -> f32 {
    (n % 4096) as f32 / 16384.0
}

struct Counter {
    frame: usize,
    next: u64,
    offset: u64,
    pulls: u64,
}

impl Counter {
    fn new(frame: usize, offset: u64) -> Counter {
        Counter {
            frame,
            next: 0,
            offset,
            pulls: 0,
        }
    }
}

impl FrameSource for Counter {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        self.pulls += 1;
        for (i, sample) in out[..self.frame].iter_mut().enumerate() {
            *sample = counted(self.next + self.offset + i as u64);
        }
        self.next += self.frame as u64;
        Some(self.frame)
    }
}

struct Level(f32);

impl FrameSource for Level {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        out[..240].fill(self.0);
        Some(240)
    }
}

struct Quiet;

impl FrameSource for Quiet {
    fn next_frame(&mut self, _: &mut [f32]) -> Option<usize> {
        None
    }
}

enum Talker {
    Counter(Counter),
    Level(Level),
    Quiet(Quiet),
}

impl FrameSource for Talker {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        match self {
            Talker::Counter(source) => source.next_frame(out),
            Talker::Level(source) => source.next_frame(out),
            Talker::Quiet(source) => source.next_frame(out),
        }
    }
}

fn pulls(mixer: &mut Mixer<u32, Talker>, key: u32) -> u64 {
    match mixer.source_mut(&key) {
        Some(Talker::Counter(counter)) => counter.pulls,
        _ => panic!("talker {key} is not a counter"),
    }
}

// Render periods seen on real drivers (128, 144 and 480 frames), plus odd
// sizes from a fixed pseudo-random sequence.
fn periods() -> impl Iterator<Item = usize> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..).map(move |i| match i % 4 {
        0 => 128,
        1 => 144,
        2 => 480,
        _ => {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            1 + (state % 1100) as usize
        }
    })
}

#[test]
fn sums_every_talker_with_audio() {
    let mut mixer = Mixer::new();
    mixer.add(1u32, Talker::Level(Level(0.1)));
    mixer.add(2, Talker::Level(Level(0.2)));
    mixer.add(3, Talker::Quiet(Quiet));
    for period in periods().take(50) {
        let mut out = vec![1.0; period];
        mixer.render(&mut out);
        assert!(
            out.iter().all(|&s| (s - 0.3).abs() < 1e-6),
            "period {period}"
        );
    }
}

#[test]
fn the_limiter_never_passes_full_scale() {
    for i in 0..=500 {
        let x = i as f32 / 1000.0;
        assert_eq!(limit(x), x);
        assert_eq!(limit(-x), -x);
    }
    let mut previous = limit(KNEE);
    for i in 1..=20_000 {
        let x = KNEE + i as f32 / 1000.0;
        let y = limit(x);
        assert!(y >= previous && y <= 1.0, "{x} gave {y}");
        assert_eq!(limit(-x), -y);
        previous = y;
    }
    // Leaves the knee without a kink: the step just past it matches the
    // input's.
    assert!((limit(KNEE + 1e-3) - (KNEE + 1e-3)).abs() < 1e-5);
    assert!((limit(1.0) - 0.8808).abs() < 1e-3);
    assert!(limit(1e30) <= 1.0 && limit(-1e30) >= -1.0);
    assert_eq!(limit(f32::NAN), 0.0);
    assert_eq!(limit(f32::INFINITY), 0.0);

    // Eight talkers at full scale, in phase.
    let mut mixer = Mixer::new();
    for key in 0..8u32 {
        mixer.add(key, Talker::Level(Level(1.0)));
    }
    let mut out = [0.0f32; 480];
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| s > 0.99 && s <= 1.0), "{}", out[0]);
}

#[test]
fn any_render_period_gets_every_sample() {
    let mut mixer = Mixer::new();
    mixer.add(1u32, Talker::Counter(Counter::new(240, 0)));
    mixer.add(2, Talker::Counter(Counter::new(480, 1000)));
    let minute = 60 * SAMPLE_RATE as u64;
    let mut n = 0u64;
    let mut out = vec![0.0f32; 1100];
    for period in periods() {
        if n >= minute {
            break;
        }
        let out = &mut out[..period];
        mixer.render(out);
        for (i, &sample) in out.iter().enumerate() {
            let at = n + i as u64;
            let want = counted(at) + counted(at + 1000);
            assert_eq!(sample, want, "sample {at}, period {period}");
        }
        n += period as u64;
        // Nothing is pulled ahead: what a talker holds is less than a frame.
        assert!(mixer.held(&1).unwrap() < 240);
        assert!(mixer.held(&2).unwrap() < 480);
        assert_eq!(pulls(&mut mixer, 1), n.div_ceil(240));
        assert_eq!(pulls(&mut mixer, 2), n.div_ceil(480));
    }
}

#[test]
fn deafen_plays_silence_and_pulls_nothing() {
    let mut mixer = Mixer::new();
    mixer.add(7u32, Talker::Counter(Counter::new(240, 2000)));
    let mut out = [0.0f32; 128];
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| s > 0.0));

    // The fade out finishes inside the next period, then nothing is pulled.
    mixer.set_deafened(true);
    mixer.render(&mut out);
    assert!(out[0] > 0.0 && out[121..].iter().all(|&s| s == 0.0));
    let before = pulls(&mut mixer, 7);
    for _ in 0..100 {
        mixer.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }
    assert_eq!(pulls(&mut mixer, 7), before);
    assert_eq!(mixer.held(&7), Some(0));

    // Back on: a fresh frame, faded in over 2.5 ms.
    mixer.set_deafened(false);
    mixer.render(&mut out);
    assert_eq!(pulls(&mut mixer, 7), before + 1);
    let first = before * 240 + 2000;
    assert!((out[0] - counted(first) / 120.0).abs() < 1e-6);
    assert_eq!(out[127], counted(first + 127));
}

// A talker replaced under the same key, or removed, is handed back and fades
// out over 2.5 ms.
#[test]
fn talkers_come_and_go() {
    let mut mixer: Mixer<u32, Talker> = Mixer::new();
    assert!(mixer.add(1, Talker::Level(Level(0.1))).is_none());
    let replaced = mixer.add(1, Talker::Level(Level(0.3)));
    assert!(matches!(replaced, Some(Talker::Level(Level(level))) if level == 0.1));
    let mut out = [0.0f32; 240];
    mixer.render(&mut out);
    assert!((out[0] - 0.4).abs() < 1e-6, "{}", out[0]);
    assert!((out[60] - 0.35).abs() < 1e-6, "{}", out[60]);
    assert!(out[120..].iter().all(|&s| (s - 0.3).abs() < 1e-6));

    assert!(matches!(mixer.remove(&1), Some(Talker::Level(_))));
    assert!(mixer.remove(&1).is_none());
    mixer.render(&mut out);
    assert!((out[0] - 0.3).abs() < 1e-6, "{}", out[0]);
    assert!(out[120..].iter().all(|&s| s == 0.0));
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| s == 0.0));
}

struct Tone {
    next: u64,
}

impl FrameSource for Tone {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        for (i, sample) in out[..240].iter_mut().enumerate() {
            let t = (self.next + i as u64) as f32 / SAMPLE_RATE as f32;
            *sample = 0.4 * (TAU * 330.0 * t).sin();
        }
        self.next += 240;
        Some(240)
    }
}

fn largest_step(before: f32, audio: &[f32]) -> f32 {
    let mut last = before;
    let mut largest = 0.0f32;
    for &sample in audio {
        largest = largest.max((sample - last).abs());
        last = sample;
    }
    largest
}

// Someone who leaves in the middle of a word ends without a click, whether
// the render period ends inside one of their frames or on its edge. The
// tone's own largest step is 0.017; the fade adds at most 0.4 / 120.
#[test]
fn a_talker_who_leaves_while_talking_fades_out() {
    for renders in [7, 12] {
        let mut mixer: Mixer<u32, Tone> = Mixer::new();
        mixer.add(1, Tone { next: 0 });
        let mut out = [0.0f32; 100];
        for _ in 0..renders {
            mixer.render(&mut out);
        }
        let last = out[99];
        assert!(last.abs() > 0.1, "the tone is at {last} when they leave");
        let held = mixer.held(&1).unwrap();
        assert!(mixer.remove(&1).is_some());
        let mut after = [0.0f32; 240];
        mixer.render(&mut after);
        let step = largest_step(last, &after);
        assert!(
            step < 0.022,
            "step of {step} after leaving with {held} held"
        );
        assert!(after[120..].iter().all(|&s| s == 0.0));
    }
}

// Every source hears the render period, including one that joins later;
// deafened, every source hears that the mixer stopped pulling, including
// one that joins while deafened.
#[test]
fn sources_hear_the_period_and_the_pause() {
    #[derive(Default)]
    struct Watched {
        pulls: u32,
        pauses: u32,
        period: usize,
    }
    impl FrameSource for Watched {
        fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
            self.pulls += 1;
            out[..240].fill(0.1);
            Some(240)
        }
        fn pause(&mut self) {
            self.pauses += 1;
        }
        fn set_period(&mut self, samples: usize) {
            self.period = samples;
        }
    }
    let mut mixer: Mixer<u32, Watched> = Mixer::new();
    mixer.add(1, Watched::default());
    mixer.set_period(144);
    mixer.add(2, Watched::default());
    let mut out = [0.0f32; 144];
    mixer.render(&mut out);
    for key in [1, 2] {
        let source = mixer.source_mut(&key).unwrap();
        assert_eq!((source.period, source.pauses), (144, 0));
    }

    mixer.set_deafened(true);
    mixer.render(&mut out);
    mixer.add(3, Watched::default());
    mixer.render(&mut out);
    for key in [1, 2, 3] {
        let source = mixer.source_mut(&key).unwrap();
        assert!(source.pauses > 0, "talker {key} was not told");
    }
    assert_eq!(mixer.source_mut(&3).unwrap().pulls, 0);

    mixer.set_deafened(false);
    mixer.render(&mut out);
    assert_eq!(mixer.source_mut(&3).unwrap().pulls, 1);
}

// A 10 ms render period takes two 5 ms frames at a time, so the second has
// to be in before the call: told the period, a talker's buffer starts two
// frames deep instead of growing there through concealed frames.
#[test]
fn a_10_ms_period_starts_two_frames_deep() {
    let mut mixer: Mixer<u32, JitterBuffer> = Mixer::new();
    mixer.set_period(480);
    mixer.add(1, JitterBuffer::new().unwrap());
    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    let mut packet = [0u8; MAX_PACKET];
    let mut out = [0.0f32; 480];
    let mut sent = 0u64;
    let mut heard = false;
    let mut quiet_periods = 0;
    for call in 0..200u64 {
        // Each frame arrives 37 samples after it is complete.
        let now = call * 480;
        while (sent + 1) * 240 + 37 <= now {
            let pcm: Vec<f32> = (0..240)
                .map(|k| {
                    let t = (sent * 240 + k) as f32 / SAMPLE_RATE as f32;
                    0.4 * (TAU * 330.0 * t).sin()
                })
                .collect();
            let len = encoder.encode(&pcm, &mut packet).unwrap();
            mixer
                .source_mut(&1)
                .unwrap()
                .push(VoicePacket {
                    seq: sent as u16,
                    frame: &packet[..len],
                    previous: None,
                    redundancy: false,
                    last: false,
                })
                .unwrap();
            sent += 1;
        }
        mixer.render(&mut out);
        let loud = out.iter().any(|s| s.abs() > 0.1);
        if heard && !loud {
            quiet_periods += 1;
        }
        heard |= loud;
    }
    assert!(heard);
    assert_eq!(quiet_periods, 0);
    let stats = mixer.source_mut(&1).unwrap().stats();
    assert_eq!(
        (
            stats.depth_frames,
            stats.concealed,
            stats.late,
            stats.inserted
        ),
        (2, 0, 0, 0)
    );
}

// Two real talkers through their jitter buffers, rendered at 128 frames
// (2.67 ms) while packets arrive every 5 ms: once both are talking, every
// period has sound and none of it goes past full scale.
#[test]
fn two_talkers_through_their_buffers_at_a_short_period() {
    let mut mixer: Mixer<u32, JitterBuffer> = Mixer::new();
    let mut encoders = Vec::new();
    for key in [1u32, 2] {
        mixer.add(key, JitterBuffer::new().unwrap());
        encoders.push(Encoder::new(Mode::LowDelay, true).unwrap());
    }
    let mut packet = [0u8; MAX_PACKET];
    let mut out = [0.0f32; 128];
    let mut rendered = 0u64;
    let mut sent = 0u64;
    let mut quiet_periods = 0;
    while rendered < SAMPLE_RATE as u64 {
        // Each frame is in before the render call that first needs it.
        while sent * 240 < rendered + out.len() as u64 {
            for (i, key) in [1u32, 2].into_iter().enumerate() {
                let hz = [220.0, 330.0][i];
                let pcm: Vec<f32> = (0..240)
                    .map(|k| {
                        let t = (sent * 240 + k) as f32 / SAMPLE_RATE as f32;
                        0.9 * (TAU * hz * t).sin()
                    })
                    .collect();
                let len = encoders[i].encode(&pcm, &mut packet).unwrap();
                let buffer = mixer.source_mut(&key).unwrap();
                buffer
                    .push(VoicePacket {
                        seq: sent as u16,
                        frame: &packet[..len],
                        previous: None,
                        redundancy: false,
                        last: false,
                    })
                    .unwrap();
            }
            sent += 1;
        }
        mixer.render(&mut out);
        rendered += out.len() as u64;
        assert!(out.iter().all(|s| s.abs() <= 1.0));
        if rendered > 2 * 240 && out.iter().all(|s| s.abs() < 0.01) {
            quiet_periods += 1;
        }
        assert!(mixer.held(&1).unwrap() < MAX_FRAME);
    }
    assert_eq!(quiet_periods, 0);
    for key in [1, 2] {
        let stats = mixer.source_mut(&key).unwrap().stats();
        assert_eq!((stats.depth_frames, stats.concealed, stats.lost), (1, 0, 0));
    }
}
