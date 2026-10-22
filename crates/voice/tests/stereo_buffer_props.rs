use std::f32::consts::TAU;
use std::sync::OnceLock;

use proptest::prelude::*;
use voice::stereo::buffer::{Buffer, FRAME_MS, MAX_DEPTH_MS, SoundPacket};
use voice::stereo::{CHANNELS, Encoder, FRAME, MAX_PACKET};

// Real stereo Opus frames of two tones, made once.
fn pool() -> &'static [Vec<u8>] {
    static POOL: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let mut encoder = Encoder::new().unwrap();
        let mut out = [0u8; MAX_PACKET];
        (0..48)
            .map(|frame| {
                let pcm: Vec<f32> = (0..FRAME)
                    .flat_map(|i| {
                        let t = (frame * FRAME + i) as f32 / 48_000.0;
                        [0.5 * (TAU * 330.0 * t).sin(), 0.5 * (TAU * 550.0 * t).sin()]
                    })
                    .collect();
                let len = encoder.encode(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    })
}

#[derive(Debug, Clone)]
enum Step {
    // The frame `back` behind the sharer's current one: 0 is on time, more
    // is late or overtaken, the same twice is a duplicate.
    Arrive { back: u16, first: bool, last: bool },
    // The sharer moves on; frames it skips never arrive. Big jumps too.
    Advance(u16),
    Pull(u16),
    Pause,
    Period(u16),
}

fn step() -> impl Strategy<Value = Step> {
    let back = prop_oneof![4 => 0u16..3, 1 => 0u16..40, 1 => any::<u16>()];
    prop_oneof![
        8 => (back, prop::bool::weighted(0.02), prop::bool::weighted(0.02))
            .prop_map(|(back, first, last)| Step::Arrive { back, first, last }),
        5 => (1u16..3).prop_map(Step::Advance),
        1 => any::<u16>().prop_map(Step::Advance),
        6 => (1u16..4).prop_map(Step::Pull),
        1 => (10u16..300).prop_map(Step::Pull),
        1 => Just(Step::Pause),
        1 => (0u16..3000).prop_map(Step::Period),
    ]
}

proptest! {
    #[test]
    fn random_arrivals_and_losses_never_panic_or_pass_the_cap(
        first in any::<u16>(),
        steps in prop::collection::vec(step(), 0..500),
    ) {
        let mut buffer = Buffer::new().unwrap();
        let mut current = first;
        let mut frame = [0f32; FRAME * CHANNELS];
        for step in steps {
            match step {
                Step::Arrive { back, first, last } => {
                    let seq = current.wrapping_sub(back);
                    let pool = pool();
                    let pushed = buffer.push(SoundPacket {
                        seq,
                        frame: &pool[usize::from(seq) % pool.len()],
                        first,
                        last,
                    });
                    prop_assert!(pushed.is_ok());
                }
                Step::Advance(by) => current = current.wrapping_add(by),
                Step::Pull(times) => {
                    for _ in 0..times {
                        if let Some(pulled) = buffer.pull(&mut frame) {
                            prop_assert_eq!(pulled.frames, FRAME);
                            // Pool frames come in any order, so Opus decodes
                            // them from the wrong history and can go past full
                            // scale. Bounding the level is the mixer's job.
                            prop_assert!(frame.iter().all(|s| s.is_finite()));
                        }
                    }
                }
                Step::Pause => buffer.pause(),
                Step::Period(frames) => buffer.set_period(usize::from(frames)),
            }
            let stats = buffer.stats();
            prop_assert!(stats.depth_ms <= MAX_DEPTH_MS, "{:?}", stats);
            prop_assert!(stats.held_frames * FRAME_MS <= MAX_DEPTH_MS, "{:?}", stats);
            prop_assert!((0.0..=100.0).contains(&stats.loss_percent), "{:?}", stats);
        }
    }
}
