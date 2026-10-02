use std::f32::consts::TAU;
use std::sync::OnceLock;

use proptest::prelude::*;
use voice::codec::{Encoder, MAX_FRAME, MAX_PACKET, Mode, SAMPLE_RATE};
use voice::jitter::{JitterBuffer, MAX_DEPTH_MS, VoicePacket};

// Real Opus packets of a tone, made once: 5 ms frames, and 10 ms ones for a
// talker that switches mode.
fn pool(mode: Mode) -> &'static [Vec<u8>] {
    static LOW_DELAY: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    static REPAIR: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    let cell = match mode {
        Mode::LowDelay => &LOW_DELAY,
        Mode::Repair => &REPAIR,
    };
    cell.get_or_init(|| {
        let mut encoder = Encoder::new(mode, true).unwrap();
        let mut out = [0u8; MAX_PACKET];
        let samples = mode.frame_samples();
        (0..48)
            .map(|frame| {
                let pcm: Vec<f32> = (0..samples)
                    .map(|i| {
                        let t = (frame * samples + i) as f32 / SAMPLE_RATE as f32;
                        0.5 * (TAU * 330.0 * t).sin()
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
    // The packet `back` frames behind the talker's current one: 0 is on
    // time, more is late or overtaken, the same twice is a duplicate.
    Arrive {
        back: u16,
        copy: bool,
        redundancy: bool,
        last: bool,
        long: bool,
    },
    // The talker moves on; frames it skips never arrive. Big jumps too.
    Advance(u16),
    Pull(u16),
}

fn step(with_long_frames: bool) -> impl Strategy<Value = Step> {
    let back = prop_oneof![4 => 0u16..3, 1 => 0u16..40, 1 => any::<u16>()];
    let long = if with_long_frames {
        prop::bool::weighted(0.2).boxed()
    } else {
        Just(false).boxed()
    };
    prop_oneof![
        8 => (back, any::<bool>(), any::<bool>(), prop::bool::weighted(0.02), long).prop_map(
            |(back, copy, redundancy, last, long)| Step::Arrive {
                back,
                copy,
                redundancy,
                last,
                long,
            }
        ),
        5 => (1u16..3).prop_map(Step::Advance),
        1 => any::<u16>().prop_map(Step::Advance),
        6 => (1u16..4).prop_map(Step::Pull),
        1 => (10u16..300).prop_map(Step::Pull),
    ]
}

fn run(first: u16, steps: Vec<Step>, check_held: bool) -> Result<(), TestCaseError> {
    let mut buffer = JitterBuffer::new().unwrap();
    let mut current = first;
    let mut frame = [0f32; MAX_FRAME];
    for step in steps {
        match step {
            Step::Arrive {
                back,
                copy,
                redundancy,
                last,
                long,
            } => {
                let seq = current.wrapping_sub(back);
                let pool = pool(if long { Mode::Repair } else { Mode::LowDelay });
                let packet = &pool[usize::from(seq) % pool.len()];
                let previous = &pool[usize::from(seq.wrapping_sub(1)) % pool.len()];
                let pushed = buffer.push(VoicePacket {
                    seq,
                    frame: packet,
                    previous: copy.then_some(previous.as_slice()),
                    redundancy,
                    last,
                });
                prop_assert!(pushed.is_ok());
            }
            Step::Advance(by) => current = current.wrapping_add(by),
            Step::Pull(times) => {
                for _ in 0..times {
                    if let Some(pulled) = buffer.pull(&mut frame) {
                        prop_assert!(pulled.samples == 240 || pulled.samples == 480);
                        // Pool packets come in any order, so Opus decodes them
                        // from the wrong history and can go past full scale
                        // (2.2 seen). Bounding the level is the mixer's job.
                        let audio = &frame[..pulled.samples];
                        prop_assert!(audio.iter().all(|s| s.is_finite()));
                    }
                }
            }
        }
        let stats = buffer.stats();
        prop_assert!(stats.depth_ms <= MAX_DEPTH_MS, "{:?}", stats);
        if check_held {
            prop_assert!(
                stats.held_frames * stats.frame_ms <= MAX_DEPTH_MS,
                "{:?}",
                stats
            );
        }
        prop_assert!((0.0..=100.0).contains(&stats.loss_percent), "{:?}", stats);
        prop_assert!(
            (0.0..=stats.loss_percent).contains(&stats.scattered_percent),
            "{:?}",
            stats
        );
    }
    Ok(())
}

proptest! {
    #[test]
    fn random_arrivals_never_panic_or_pass_the_cap(
        first in any::<u16>(),
        steps in prop::collection::vec(step(false), 0..500),
    ) {
        run(first, steps, true)?;
    }

    // A talker switching between 5 and 10 ms frames at random, which Booth
    // never does this fast, still cannot make the buffer panic or go deeper
    // than the cap.
    #[test]
    fn random_mode_switches_never_panic_or_pass_the_cap(
        first in any::<u16>(),
        steps in prop::collection::vec(step(true), 0..500),
    ) {
        run(first, steps, false)?;
    }
}
