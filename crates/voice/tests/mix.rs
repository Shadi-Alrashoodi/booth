use std::f32::consts::TAU;

use voice::codec::{Encoder, MAX_FRAME, MAX_PACKET, Mode, SAMPLE_RATE};
use voice::jitter::{JitterBuffer, VoicePacket};
use voice::mix::{FrameSource, KNEE, Mixer, StereoSource, limit};

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

// A level that gives nothing while `gap` is set, as a talker between spells.
struct Gap {
    level: f32,
    gap: bool,
}

impl FrameSource for Gap {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        if self.gap {
            return None;
        }
        out[..240].fill(self.level);
        Some(240)
    }
}

enum Talker {
    Counter(Counter),
    Level(Level),
    Quiet(Quiet),
    Gap(Gap),
}

impl FrameSource for Talker {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        match self {
            Talker::Counter(source) => source.next_frame(out),
            Talker::Level(source) => source.next_frame(out),
            Talker::Quiet(source) => source.next_frame(out),
            Talker::Gap(source) => source.next_frame(out),
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

// Each talker's own volume, applied before the mix: from the first sample
// for a talker turned down before they spoke, and over 2.5 ms when changed
// while they talk, never past what was asked. Turned to nothing they are
// still pulled, so they come back on time when turned up again.
#[test]
fn each_talker_has_a_volume_of_their_own() {
    let mut mixer: Mixer<u32, Talker> = Mixer::new();
    mixer.add(1, Talker::Level(Level(0.2)));
    mixer.add(2, Talker::Level(Level(0.1)));
    mixer.set_gain(&1, 0.5);
    let mut out = [0.0f32; 240];
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| (s - 0.2).abs() < 1e-6), "{out:?}");

    mixer.set_gain(&2, 0.0);
    mixer.render(&mut out);
    // Down from 0.1 to nothing over the first 120 samples, a step at a time.
    assert!(out.windows(2).all(|pair| pair[1] <= pair[0] + 1e-6));
    assert!((out[0] - 0.2).abs() < 0.002, "{}", out[0]);
    assert!(out[120..].iter().all(|&s| (s - 0.1).abs() < 1e-6));

    mixer.add(3, Talker::Counter(Counter::new(240, 0)));
    mixer.set_gain(&3, 0.0);
    for _ in 0..4 {
        mixer.render(&mut out);
    }
    assert!(out.iter().all(|&s| (s - 0.1).abs() < 1e-6));
    assert_eq!(pulls(&mut mixer, 3), 4);
    mixer.set_gain(&1, 1.5);
    mixer.set_gain(&2, f32::NAN);
    mixer.render(&mut out);
    // 0.3 from the first, 0.1 from the second back at its own level, and
    // the counter still off.
    assert!(
        out[120..].iter().all(|&s| (s - 0.4).abs() < 1e-6),
        "{:?}",
        &out[120..]
    );
    // A talker who is not in the mix is no one's business.
    mixer.set_gain(&9, 0.0);
}

// A talker turned down while idle, between spells or while this PC was
// deafened, comes back at the new volume from the first sample. Turned up
// while idle, they still ramp up from where they were, and once back, a
// volume changed mid-word ramps as ever. One turned down while idle and
// taken out plays its last 2.5 ms at the new volume.
#[test]
fn a_volume_turned_down_while_idle_holds_from_the_first_sample() {
    let mut mixer: Mixer<u32, Talker> = Mixer::new();
    mixer.add(
        1,
        Talker::Gap(Gap {
            level: 0.2,
            gap: false,
        }),
    );
    let gap = |mixer: &mut Mixer<u32, Talker>, on: bool| match mixer.source_mut(&1) {
        Some(Talker::Gap(source)) => source.gap = on,
        _ => panic!("talker 1 is not a gap"),
    };
    let mut out = [0.0f32; 240];
    mixer.render(&mut out);
    gap(&mut mixer, true);
    mixer.render(&mut out);
    mixer.set_gain(&1, 0.0);
    gap(&mut mixer, false);
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| s == 0.0), "{out:?}");

    gap(&mut mixer, true);
    mixer.render(&mut out);
    mixer.set_gain(&1, 1.0);
    gap(&mut mixer, false);
    mixer.render(&mut out);
    assert!(out[0] > 0.0 && out[0] <= 0.2 / 120.0 + 1e-6, "{}", out[0]);
    assert!(out[120..].iter().all(|&s| (s - 0.2).abs() < 1e-6));
    mixer.set_gain(&1, 0.0);
    mixer.render(&mut out);
    assert!((out[0] - 0.2 * 119.0 / 120.0).abs() < 1e-6, "{}", out[0]);
    assert!(out[120..].iter().all(|&s| s == 0.0));

    mixer.set_gain(&1, 1.0);
    mixer.render(&mut out);
    mixer.set_deafened(true);
    mixer.render(&mut out);
    mixer.render(&mut out);
    mixer.set_gain(&1, 0.0);
    mixer.set_deafened(false);
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| s == 0.0), "{out:?}");

    mixer.set_gain(&1, 1.0);
    mixer.render(&mut out);
    gap(&mut mixer, true);
    mixer.render(&mut out);
    mixer.set_gain(&1, 0.0);
    gap(&mut mixer, false);
    assert!(mixer.remove(&1).is_some());
    mixer.render(&mut out);
    assert!(out.iter().all(|&s| s == 0.0), "{out:?}");
}

// A stereo stream that plays the same pair in every frame.
struct Pair {
    left: f32,
    right: f32,
    frame: usize,
    paused: u64,
    // Gives nothing while set, as PC sound between spells.
    gap: bool,
}

impl Pair {
    fn new(left: f32, right: f32) -> Pair {
        Pair {
            left,
            right,
            frame: 480,
            paused: 0,
            gap: false,
        }
    }
}

impl StereoSource for Pair {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        if self.gap {
            return None;
        }
        let (frames, _) = out[..2 * self.frame].as_chunks_mut::<2>();
        for frame in frames {
            *frame = [self.left, self.right];
        }
        Some(self.frame)
    }

    fn pause(&mut self) {
        self.paused += 1;
    }
}

type Both = Mixer<u32, Talker, Pair>;

// The same script on two mixers, one through render and one through
// render_stereo with no stream: both channels are the mono mix to the bit.
fn same_bits(script: impl Fn(&mut Both, usize)) {
    let mut mono: Both = Mixer::with_streams();
    let mut stereo: Both = Mixer::with_streams();
    for (step, period) in periods().take(200).enumerate() {
        script(&mut mono, step);
        script(&mut stereo, step);
        let mut one = vec![0.0f32; period];
        let mut two = vec![0.0f32; 2 * period];
        mono.render(&mut one);
        stereo.render_stereo(&mut two);
        let (pairs, _) = two.as_chunks::<2>();
        for (i, (m, [l, r])) in one.iter().zip(pairs).enumerate() {
            assert_eq!(
                (l.to_bits(), r.to_bits()),
                (m.to_bits(), m.to_bits()),
                "step {step}, sample {i}"
            );
        }
    }
}

#[test]
fn stereo_with_no_stream_is_the_mono_mix_on_both_sides() {
    // Talkers summed, quiet ones too.
    same_bits(|mixer, step| {
        if step == 0 {
            mixer.add(1, Talker::Counter(Counter::new(240, 0)));
            mixer.add(2, Talker::Counter(Counter::new(480, 100)));
            mixer.add(3, Talker::Quiet(Quiet));
        }
    });
    // Past the knee, and past full scale.
    same_bits(|mixer, step| {
        if step == 0 {
            mixer.add(1, Talker::Level(Level(0.9)));
            mixer.add(2, Talker::Level(Level(0.8)));
        }
    });
    // Deafened and back.
    same_bits(|mixer, step| match step {
        0 => {
            mixer.add(1, Talker::Counter(Counter::new(240, 7)));
        }
        20 => mixer.set_deafened(true),
        40 => mixer.set_deafened(false),
        _ => {}
    });
    // Talkers coming and going, one leaving mid-frame.
    same_bits(|mixer, step| match step {
        0 => {
            mixer.add(1, Talker::Counter(Counter::new(240, 0)));
        }
        10 => {
            mixer.add(2, Talker::Counter(Counter::new(480, 9)));
        }
        30 => {
            mixer.remove(&1);
        }
        50 => {
            mixer.add(1, Talker::Level(Level(0.3)));
        }
        70 => {
            mixer.remove(&2);
        }
        _ => {}
    });
    // Volumes moving.
    same_bits(|mixer, step| match step {
        0 => {
            mixer.add(1, Talker::Counter(Counter::new(240, 0)));
            mixer.add(2, Talker::Level(Level(0.2)));
        }
        10 => mixer.set_gain(&1, 0.3),
        30 => mixer.set_gain(&2, 1.4),
        _ => {}
    });
}

fn rendered(mixer: &mut Both, frames: usize) -> Vec<[f32; 2]> {
    let mut out = vec![0.0f32; 2 * frames];
    mixer.render_stereo(&mut out);
    out.as_chunks::<2>().0.to_vec()
}

#[test]
fn a_stream_keeps_its_sides_beside_the_talkers() {
    let mut mixer: Both = Mixer::with_streams();
    mixer.add(1, Talker::Level(Level(0.1)));
    assert!(mixer.add_stream(1, Pair::new(0.2, -0.2)).is_none());
    for [left, right] in rendered(&mut mixer, 480) {
        assert!((left - 0.3).abs() < 1e-6 && (right + 0.1).abs() < 1e-6);
    }
    // One under the same key takes its place.
    assert!(mixer.add_stream(1, Pair::new(0.0, 0.25)).is_some());
    let frames = rendered(&mut mixer, 480);
    let [left, right] = frames[479];
    assert!((left - 0.1).abs() < 1e-6 && (right - 0.35).abs() < 1e-6);
}

#[test]
fn deafen_fades_and_pauses_streams() {
    let mut mixer: Both = Mixer::with_streams();
    mixer.add_stream(1, Pair::new(0.3, 0.3));
    rendered(&mut mixer, 240);
    mixer.set_deafened(true);
    rendered(&mut mixer, 240);
    let silent = rendered(&mut mixer, 240);
    assert!(silent.iter().all(|&frame| frame == [0.0, 0.0]));
    assert!(mixer.stream_mut(&1).unwrap().paused > 0);
    mixer.set_deafened(false);
    rendered(&mut mixer, 240);
    let back = rendered(&mut mixer, 240);
    assert!(back.iter().all(|&[left, _]| (left - 0.3).abs() < 1e-6));
}

#[test]
fn a_stream_that_leaves_fades_out_over_2_5_ms() {
    let mut mixer: Both = Mixer::with_streams();
    mixer.add_stream(1, Pair::new(0.4, -0.4));
    rendered(&mut mixer, 240);
    assert!(mixer.remove_stream(&1).is_some());
    let after = rendered(&mut mixer, 240);
    assert!((after[0][0] - 0.4).abs() < 0.01, "{:?}", after[0]);
    for pair in after.windows(2) {
        assert!(pair[1][0] <= pair[0][0] && pair[1][1] >= pair[0][1]);
    }
    assert!(after[120..].iter().all(|&frame| frame == [0.0, 0.0]));
    assert!(mixer.remove_stream(&1).is_none());
}

#[test]
fn a_stream_volume_ramps_without_a_step() {
    let mut mixer: Both = Mixer::with_streams();
    mixer.add_stream(1, Pair::new(0.4, -0.4));
    // Set before it plays, it starts there.
    mixer.set_stream_gain(&1, 0.5);
    let first = rendered(&mut mixer, 240);
    assert!((first[0][0] - 0.2).abs() < 1e-6);
    mixer.set_stream_gain(&1, 0.0);
    let ramp = rendered(&mut mixer, 240);
    let most = 0.4 / 120.0 + 1e-6;
    let mut before = first[239][0];
    for [left, _] in &ramp {
        assert!((before - left).abs() <= most, "{before} to {left}");
        before = *left;
    }
    assert_eq!(ramp[239], [0.0, 0.0]);
}

// As for a talker: a stream turned down while idle, between spells or
// while this PC was deafened, comes back at the new volume from its first
// frame, and one turned up while idle still ramps, as does a change once it
// is back.
#[test]
fn a_stream_turned_down_while_idle_holds_from_its_first_frame() {
    let mut mixer: Both = Mixer::with_streams();
    mixer.add_stream(1, Pair::new(0.4, -0.4));
    let gap = |mixer: &mut Both, on: bool| mixer.stream_mut(&1).unwrap().gap = on;
    rendered(&mut mixer, 480);
    gap(&mut mixer, true);
    rendered(&mut mixer, 480);
    mixer.set_stream_gain(&1, 0.0);
    gap(&mut mixer, false);
    let back = rendered(&mut mixer, 480);
    assert!(back.iter().all(|&frame| frame == [0.0, 0.0]), "{back:?}");

    gap(&mut mixer, true);
    rendered(&mut mixer, 480);
    mixer.set_stream_gain(&1, 1.0);
    gap(&mut mixer, false);
    let up = rendered(&mut mixer, 480);
    assert!(
        up[0][0] > 0.0 && up[0][0] <= 0.4 / 120.0 + 1e-6,
        "{:?}",
        up[0]
    );
    assert!((up[479][0] - 0.4).abs() < 1e-6);
    mixer.set_stream_gain(&1, 0.0);
    let down = rendered(&mut mixer, 480);
    assert!(
        (down[0][0] - 0.4 * 119.0 / 120.0).abs() < 1e-6,
        "{:?}",
        down[0]
    );
    assert_eq!(down[479], [0.0, 0.0]);

    mixer.set_stream_gain(&1, 1.0);
    rendered(&mut mixer, 480);
    mixer.set_deafened(true);
    rendered(&mut mixer, 480);
    rendered(&mut mixer, 480);
    mixer.set_stream_gain(&1, 0.0);
    mixer.set_deafened(false);
    let undeafened = rendered(&mut mixer, 480);
    assert!(
        undeafened.iter().all(|&frame| frame == [0.0, 0.0]),
        "{undeafened:?}"
    );
}
