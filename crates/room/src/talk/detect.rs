// Open mic: whether to send, from the sound itself. Each 5 ms block's level
// is held against the room's noise floor, the quietest block of the last
// second; a voice is well above it, the fan and the keyboard are not. After
// the voice stops, sending goes on for a random 200 to 800 ms (RFC 6562), so
// the moments sending stops do not trace the pauses in the speech. That tail
// also carries a talker across the gaps between words.

use std::time::Duration;

use voice::codec::SAMPLE_RATE;

// 5 ms, the smaller frame, so both frame sizes are whole blocks.
const BLOCK: usize = 240;
const BLOCKS_PER_SECOND: usize = SAMPLE_RATE as usize / BLOCK;

// Sending starts on a block this far over the floor. Speech on a headset
// microphone sits 20 to 40 dB over the room; a door or a chair creak that
// is not that loud does not start it.
const ONSET_DB: f32 = 12.0;
// Once started, a block this far over the floor still counts as voice, so
// the end of a word does not end it early.
const HOLD_DB: f32 = 6.0;
// Nothing quieter is a voice however quiet the room: a microphone that
// gives digital silence would otherwise send on its own hiss.
const QUIETEST_VOICE_DB: f32 = -50.0;
// The floor follows a quieter room at once and a louder one no faster than
// this, so a voice that goes on for more than a second, or music, does not
// become the floor and cut itself off.
const FLOOR_RISE_DB_PER_S: f32 = 3.0;
const SILENCE_DB: f32 = -100.0;

pub(crate) const TAIL_SHORTEST: Duration = Duration::from_millis(200);
pub(crate) const TAIL_LONGEST: Duration = Duration::from_millis(800);

pub(crate) struct Detector {
    // One level a block, the last second of them.
    levels: [f32; BLOCKS_PER_SECOND],
    next: usize,
    // Blocks heard so far, up to a second's worth. Until a second has been
    // heard the floor is simply the quietest block so far.
    heard: usize,
    floor: f32,
    voice: bool,
    // Samples of tail left once the voice stopped.
    tail: usize,
    // Uniform over 0..=u32::MAX; the operating system's generator outside
    // the tests.
    random: fn() -> u32,
}

impl Detector {
    pub(crate) fn new() -> Detector {
        Detector::with_random(random)
    }

    pub(crate) fn with_random(random: fn() -> u32) -> Detector {
        Detector {
            levels: [SILENCE_DB; BLOCKS_PER_SECOND],
            next: 0,
            heard: 0,
            floor: SILENCE_DB,
            voice: false,
            tail: 0,
            random,
        }
    }

    // One frame, 5 or 10 ms. True when this frame goes out.
    pub(crate) fn wanted(&mut self, frame: &[f32]) -> bool {
        let mut wanted = false;
        for block in frame.chunks(BLOCK) {
            wanted = self.block(block);
        }
        wanted
    }

    #[cfg(test)]
    pub(crate) fn floor_db(&self) -> f32 {
        self.floor
    }

    fn block(&mut self, block: &[f32]) -> bool {
        let level = level_db(block);
        self.levels[self.next] = level;
        self.next = (self.next + 1) % BLOCKS_PER_SECOND;
        self.heard = (self.heard + 1).min(BLOCKS_PER_SECOND);
        let quietest = self.levels[..self.heard]
            .iter()
            .copied()
            .fold(f32::INFINITY, f32::min);
        let rise = FLOOR_RISE_DB_PER_S * BLOCK as f32 / SAMPLE_RATE as f32;
        self.floor = if self.heard < BLOCKS_PER_SECOND {
            quietest
        } else {
            quietest.min(self.floor + rise)
        };

        let over = if self.voice { HOLD_DB } else { ONSET_DB };
        let voice = level >= QUIETEST_VOICE_DB && level >= self.floor + over;
        if voice {
            self.voice = true;
            self.tail = 0;
            return true;
        }
        if self.voice {
            self.voice = false;
            self.tail = self.draw_tail();
        }
        if self.tail == 0 {
            return false;
        }
        self.tail = self.tail.saturating_sub(block.len());
        true
    }

    fn draw_tail(&self) -> usize {
        let shortest = samples(TAIL_SHORTEST);
        let spread = samples(TAIL_LONGEST) - shortest;
        let picked = (u64::from((self.random)()) * (spread as u64 + 1)) >> 32;
        shortest + picked as usize
    }
}

fn samples(duration: Duration) -> usize {
    (duration.as_micros() * u128::from(SAMPLE_RATE) / 1_000_000) as usize
}

fn level_db(block: &[f32]) -> f32 {
    if block.is_empty() {
        return SILENCE_DB;
    }
    let power = block.iter().map(|&s| s * s).sum::<f32>() / block.len() as f32;
    if !power.is_finite() || power <= 0.0 {
        return SILENCE_DB;
    }
    (10.0 * power.log10()).max(SILENCE_DB)
}

fn random() -> u32 {
    let mut bytes = [0u8; 4];
    // getrandom's Windows 10+ backend is ProcessPrng, which cannot fail. A
    // tail that is always the shortest would still hide the pause lengths
    // behind 200 ms of tail.
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    u32::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const MS: usize = SAMPLE_RATE as usize / 1000;

    fn tone(ms: usize, amplitude: f32, start: usize) -> Vec<f32> {
        (start..start + ms * MS)
            .map(|i| amplitude * (TAU * 220.0 * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    // Low-passed noise at about `db` dBFS, as a fan or a room is.
    fn noise(ms: usize, db: f32, seed: &mut u64) -> Vec<f32> {
        // The smoothing leaves uniform noise at 0.24 of full scale RMS.
        let scale = 10f32.powf(db / 20.0) / 0.2425;
        let mut smooth = 0.0f32;
        (0..ms * MS)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                let white = (*seed >> 40) as f32 / (1u64 << 23) as f32 - 1.0;
                smooth = 0.7 * smooth + 0.3 * white;
                smooth * scale
            })
            .collect()
    }

    // Whether each 5 ms frame went out.
    fn run(detector: &mut Detector, signal: &[f32]) -> Vec<bool> {
        signal
            .chunks(BLOCK)
            .map(|frame| detector.wanted(frame))
            .collect()
    }

    fn ms_of(frames: usize) -> usize {
        frames * 5
    }

    #[test]
    fn voice_sends_at_once_then_tail() {
        let mut seed = 7;
        let draws: [(fn() -> u32, usize); 3] =
            [(|| 0, 200), (|| u32::MAX, 800), (|| u32::MAX / 2, 500)];
        for (draw, tail_ms) in draws {
            let mut detector = Detector::with_random(draw);
            let mut signal = noise(1000, -60.0, &mut seed);
            let quiet = signal.len();
            signal.extend(tone(600, 0.2, 0));
            let loud_end = signal.len();
            signal.extend(noise(1500, -60.0, &mut seed));
            let sent = run(&mut detector, &signal);
            let first = sent.iter().position(|&on| on).expect("sent at all");
            let last = sent.iter().rposition(|&on| on).unwrap();
            assert_eq!(first * BLOCK, quiet, "starts on the voice's first frame");
            assert!(sent[first..=last].iter().all(|&on| on), "no gaps");
            let tail = ms_of(last + 1) - loud_end / MS;
            println!("tail {tail} ms, {tail_ms} ms drawn");
            assert!(tail.abs_diff(tail_ms) <= 5, "{tail} ms, wanted {tail_ms}");
        }
    }

    #[test]
    fn tails_are_spread_over_200_to_800_ms() {
        let mut tails = Vec::new();
        let mut seed = 11;
        for _ in 0..40 {
            let mut detector = Detector::new();
            let mut signal = noise(1000, -65.0, &mut seed);
            signal.extend(tone(300, 0.2, 0));
            let loud_end = signal.len() / MS;
            signal.extend(noise(1000, -65.0, &mut seed));
            let sent = run(&mut detector, &signal);
            let last = sent.iter().rposition(|&on| on).unwrap();
            tails.push(ms_of(last + 1) - loud_end);
        }
        let (short, long) = (*tails.iter().min().unwrap(), *tails.iter().max().unwrap());
        println!("40 tails from {short} to {long} ms");
        assert!(short >= 200 && long <= 805, "{tails:?}");
        assert!(long - short > 200, "not spread out: {tails:?}");
    }

    #[test]
    fn a_fan_and_a_quiet_keyboard_do_not_send() {
        let mut seed = 3;
        let mut detector = Detector::with_random(|| 0);
        let mut signal = noise(3000, -45.0, &mut seed);
        // A key click: 10 ms, 8 dB over the fan.
        let click = noise(10, -37.0, &mut seed);
        signal.splice(48_000..48_000 + click.len(), click);
        let sent = run(&mut detector, &signal);
        assert!(sent.iter().all(|&on| !on));
        assert!(
            (detector.floor_db() + 45.0).abs() < 3.0,
            "{}",
            detector.floor_db()
        );

        // Over the same fan, a voice 25 dB louder does.
        let voice = tone(400, 0.1, 0);
        let sent = run(&mut detector, &voice);
        assert!(sent.iter().all(|&on| on));
    }

    #[test]
    fn digital_silence_then_hiss_is_not_a_voice() {
        let mut seed = 5;
        let mut detector = Detector::with_random(|| 0);
        let mut signal = vec![0.0; 48_000];
        signal.extend(noise(2000, -70.0, &mut seed));
        assert!(run(&mut detector, &signal).iter().all(|&on| !on));
    }

    // A voice that never pauses, or music: the floor creeps up slowly, so
    // it is still sending ten seconds in.
    #[test]
    fn steady_voice_not_the_floor() {
        let mut seed = 9;
        let mut detector = Detector::with_random(|| 0);
        let mut signal = noise(1000, -60.0, &mut seed);
        let quiet = signal.len() / BLOCK;
        signal.extend(tone(10_000, 0.1, 0));
        let sent = run(&mut detector, &signal);
        assert!(sent[quiet..].iter().all(|&on| on));
        assert!(detector.floor_db() < -25.0, "{}", detector.floor_db());
    }
}
