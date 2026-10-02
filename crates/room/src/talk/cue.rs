// The share cue: two short notes in this PC's own speakers when its share
// starts, rising, and the same two falling when it stops. The render thread
// makes them in the mixer beside the voices, so deafen silences them and the
// limiter holds them as it holds a voice.
//
// The control cue is the same shape on other notes, in the ears of the one
// controlled and the controller when control starts and stops. It is added
// after the mixer, so deafen never silences it: over a game in exclusive
// fullscreen, where no window shows, the sound is the indicator. Nothing
// here goes near the microphone or the socket.

use std::f64::consts::{PI, TAU};

use voice::codec::SAMPLE_RATE;
use voice::mix::FrameSource;

// G5 and C6, a fourth apart: above a voice's fundamental and most of a
// game's rumble and music, and well below the 2 to 4 kHz band where the ear
// is sharpest and a tone starts to sound piercing.
const LOW_HZ: f64 = 783.99;
const HIGH_HZ: f64 = 1046.50;
// D5 and A5 for control, a fifth apart: lower and wider than the share
// cue's fourth, so the two are told apart by ear, and in the same safe band.
const CONTROL_LOW_HZ: f64 = 587.33;
const CONTROL_HIGH_HZ: f64 = 880.00;
const NOTE: usize = 80 * SAMPLE_RATE as usize / 1000;
// Each note comes in over 5 ms and dies away over its last 20, so neither
// end clicks and it sounds struck rather than switched on.
const ATTACK: usize = 5 * SAMPLE_RATE as usize / 1000;
const RELEASE: usize = 20 * SAMPLE_RATE as usize / 1000;
// A cue cut short for a newer one dies away over 2.5 ms first: stopped
// mid-wave it would click, and the newer one waits no longer than that.
const FADE: usize = 25 * SAMPLE_RATE as usize / 10_000;
// -14 dBFS at the peak, about as loud as a friend's voice from a sensible
// microphone, and under the mixer's knee, so the limiter leaves the cue
// alone unless someone talks over it.
const PEAK: f32 = 0.2;
const _: () = assert!(PEAK < voice::mix::KNEE);
// Handed to the mixer 5 ms at a time, as a talker's frames are.
const CHUNK: usize = 240;
// The render thread takes a cue within a period, a few milliseconds on
// most drivers and tens on the slowest. One that waited longer found the
// speakers closed or opening again, and heard now it would mark a moment
// long past.
const LATE_US: u64 = 250_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cue {
    Rising,
    Falling,
}

// A cue and when it was asked for, on the ping clock, in one word the
// render thread can take without a lock. 0 is none.
pub(super) fn pack(cue: Cue, at_us: u64) -> u64 {
    let kind = match cue {
        Cue::Rising => 1,
        Cue::Falling => 2,
    };
    (at_us << 2) | kind
}

pub(super) fn unpack(word: u64, now_us: u64) -> Option<Cue> {
    let cue = match word & 3 {
        1 => Cue::Rising,
        2 => Cue::Falling,
        _ => return None,
    };
    (now_us.saturating_sub(word >> 2) <= LATE_US).then_some(cue)
}

// The cue under way, as the mixer pulls it; idle between cues.
pub(super) struct Tone {
    // The low note and the high one this cue plays.
    pitch: [f64; 2],
    notes: [f64; 2],
    next: usize,
    // A cue asked for while another played, and the sample the one under
    // way ends at, faded out.
    after: Option<(Cue, usize)>,
}

impl Tone {
    pub(super) fn idle() -> Tone {
        Tone::on([LOW_HZ, HIGH_HZ])
    }

    pub(super) fn control() -> Tone {
        Tone::on([CONTROL_LOW_HZ, CONTROL_HIGH_HZ])
    }

    fn on(pitch: [f64; 2]) -> Tone {
        Tone {
            pitch,
            notes: pitch,
            next: 2 * NOTE,
            after: None,
        }
    }

    // A cue asked for while another plays takes over once that one has
    // faded out: the newer one says what the share is doing now. Within
    // FADE of its end the one under way is dying away already and finishes
    // as it is.
    pub(super) fn start(&mut self, cue: Cue) {
        if self.next == 0 || self.next >= 2 * NOTE {
            self.begin(cue);
            return;
        }
        let ends = match self.after {
            Some((_, ends)) => ends,
            None => (self.next + FADE).min(2 * NOTE),
        };
        self.after = Some((cue, ends));
    }

    fn begin(&mut self, cue: Cue) {
        let [low, high] = self.pitch;
        self.notes = match cue {
            Cue::Rising => [low, high],
            Cue::Falling => [high, low],
        };
        self.next = 0;
        self.after = None;
    }

    fn sample(&self, at: usize) -> f32 {
        let (note, n) = (at / NOTE, at % NOTE);
        let mut envelope = if n < ATTACK {
            swell(n, ATTACK)
        } else if n > NOTE - RELEASE {
            swell(NOTE - n, RELEASE)
        } else {
            1.0
        };
        if let Some((_, ends)) = self.after
            && ends < 2 * NOTE
        {
            envelope *= swell(ends - at, FADE);
        }
        let cycles = (self.notes[note] * n as f64 / f64::from(SAMPLE_RATE)).fract();
        (f64::from(PEAK) * envelope * (TAU * cycles).sin()) as f32
    }
}

// A raised cosine: 0 when `at` is 0, 1 when it reaches `len`.
fn swell(at: usize, len: usize) -> f64 {
    0.5 - 0.5 * (PI * at as f64 / len as f64).cos()
}

// The control cue added to what the mixer made, and held by the limiter
// again where it plays: the mixer's own limit is behind it already.
pub(super) fn mix_over(tone: &mut Tone, out: &mut [f32]) {
    let mut chunk = [0.0f32; CHUNK];
    let mut done = 0;
    while done < out.len() {
        let want = (out.len() - done).min(CHUNK);
        let Some(len) = tone.next_frame(&mut chunk[..want]) else {
            return;
        };
        for (sample, add) in out[done..done + len].iter_mut().zip(&chunk[..len]) {
            *sample = voice::mix::limit(*sample + add);
        }
        done += len;
    }
}

impl FrameSource for Tone {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        if let Some((cue, ends)) = self.after
            && self.next >= ends
        {
            self.begin(cue);
        }
        let ends = self.after.map_or(2 * NOTE, |(_, ends)| ends);
        let len = ends.saturating_sub(self.next).min(CHUNK).min(out.len());
        if len == 0 {
            return None;
        }
        for (i, sample) in out[..len].iter_mut().enumerate() {
            *sample = self.sample(self.next + i);
        }
        self.next += len;
        Some(len)
    }

    // Deafened, the cue is dropped rather than kept for later.
    fn pause(&mut self) {
        self.next = 2 * NOTE;
        self.after = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voice::codec::MAX_FRAME;

    fn whole(cue: Cue) -> Vec<f32> {
        let mut tone = Tone::idle();
        let mut out = [0.0f32; MAX_FRAME];
        assert_eq!(tone.next_frame(&mut out), None, "idle plays nothing");
        tone.start(cue);
        let mut samples = Vec::new();
        drain(&mut tone, &mut samples);
        samples
    }

    // How much of a sine at `hz` the samples hold, as its amplitude.
    fn amplitude(samples: &[f32], hz: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &sample) in samples.iter().enumerate() {
            let phase = TAU * hz * n as f64 / f64::from(SAMPLE_RATE);
            re += f64::from(sample) * phase.cos();
            im += f64::from(sample) * phase.sin();
        }
        2.0 * (re * re + im * im).sqrt() / samples.len() as f64
    }

    // A full-size sine at the high note moves at most this much a sample.
    fn steepest() -> f64 {
        f64::from(PEAK) * TAU * HIGH_HZ / f64::from(SAMPLE_RATE)
    }

    fn largest_step(samples: &[f32]) -> f64 {
        samples
            .windows(2)
            .map(|pair| f64::from((pair[1] - pair[0]).abs()))
            .fold(0.0, f64::max)
    }

    fn pull_to(tone: &mut Tone, samples: &mut Vec<f32>, until: usize) {
        let mut out = [0.0f32; MAX_FRAME];
        while samples.len() < until {
            let want = (until - samples.len()).min(CHUNK);
            let len = tone.next_frame(&mut out[..want]).expect("the cue plays on");
            samples.extend_from_slice(&out[..len]);
        }
    }

    fn drain(tone: &mut Tone, samples: &mut Vec<f32>) {
        let mut out = [0.0f32; MAX_FRAME];
        while let Some(len) = tone.next_frame(&mut out) {
            samples.extend_from_slice(&out[..len]);
        }
    }

    #[test]
    fn rising_and_falling_notes() {
        let rising = whole(Cue::Rising);
        let falling = whole(Cue::Falling);
        assert_eq!(rising.len(), 2 * NOTE);
        assert_eq!(falling.len(), 2 * NOTE);
        let (first, second) = rising.split_at(NOTE);
        for (note, hz, other) in [(first, LOW_HZ, HIGH_HZ), (second, HIGH_HZ, LOW_HZ)] {
            let (main, rest) = (amplitude(note, hz), amplitude(note, other));
            assert!(
                main > 0.15 && rest < 0.02,
                "{hz} Hz: {main:.3} against {rest:.3}"
            );
        }
        let (first, second) = falling.split_at(NOTE);
        assert_eq!(first, &rising[NOTE..]);
        assert_eq!(second, &rising[..NOTE]);
    }

    // No sample past the peak, none short of it by much, and each note
    // starts and ends at silence without a step a click would make.
    #[test]
    fn notes_swell_without_click() {
        let samples = whole(Cue::Rising);
        let peak = samples.iter().fold(0.0f32, |most, s| most.max(s.abs()));
        assert!(peak <= PEAK && peak > 0.99 * PEAK, "{peak}");
        for note in samples.chunks(NOTE) {
            assert_eq!(note[0], 0.0);
            assert!(note[NOTE - 1].abs() < 1e-4, "{}", note[NOTE - 1]);
        }
        let step = largest_step(&samples);
        assert!(step <= steepest() * 1.01, "{step} against {}", steepest());
    }

    // The rising cue cut short by the falling one at points all through it,
    // as when a share fails in its first frames or is stopped at once: the
    // rising one dies away, the falling one follows whole, and nothing on
    // the way clicks. Near its end the rising one is dying away already
    // and finishes.
    #[test]
    fn cut_cue_fades_first() {
        let falling = whole(Cue::Falling);
        let cuts =
            (1..2 * NOTE / CHUNK)
                .map(|k| k * CHUNK)
                .chain([1000, 2 * NOTE - 60, 2 * NOTE - 1]);
        for cut in cuts {
            let mut tone = Tone::idle();
            let mut samples = Vec::new();
            tone.start(Cue::Rising);
            pull_to(&mut tone, &mut samples, cut);
            tone.start(Cue::Falling);
            drain(&mut tone, &mut samples);
            let faded = FADE.min(2 * NOTE - cut);
            assert_eq!(samples.len(), cut + faded + 2 * NOTE, "cut at {cut}");
            assert_eq!(samples[cut + faded..], falling[..], "cut at {cut}");
            let step = largest_step(&samples);
            assert!(
                step <= steepest() * 1.01,
                "cut at {cut}: {step} against {}",
                steepest()
            );
        }

        // A share stopped and started again before the rising cue died away:
        // the newest cue is the one that plays.
        let mut tone = Tone::idle();
        let mut samples = Vec::new();
        tone.start(Cue::Rising);
        pull_to(&mut tone, &mut samples, 1200);
        tone.start(Cue::Falling);
        pull_to(&mut tone, &mut samples, 1200 + FADE / 2);
        tone.start(Cue::Rising);
        drain(&mut tone, &mut samples);
        assert_eq!(samples.len(), 1200 + FADE + 2 * NOTE);
        assert_eq!(samples[1200 + FADE..], whole(Cue::Rising)[..]);
        assert!(largest_step(&samples) <= steepest() * 1.01);
    }

    #[test]
    fn late_cue_dropped() {
        let at = 1_700_000_000_000_000;
        assert_eq!(unpack(0, at), None);
        assert_eq!(unpack(pack(Cue::Rising, at), at), Some(Cue::Rising));
        assert_eq!(
            unpack(pack(Cue::Falling, at), at + LATE_US),
            Some(Cue::Falling)
        );
        assert_eq!(unpack(pack(Cue::Falling, at), at + LATE_US + 1), None);
        // A clock read on another thread a moment earlier is not late.
        assert_eq!(unpack(pack(Cue::Rising, at), at - 50), Some(Cue::Rising));
    }

    #[test]
    fn deafened_drops_a_cue_under_way() {
        let mut tone = Tone::idle();
        let mut out = [0.0f32; MAX_FRAME];
        tone.start(Cue::Rising);
        assert_eq!(tone.next_frame(&mut out), Some(CHUNK));
        tone.pause();
        assert_eq!(tone.next_frame(&mut out), None);
    }
}
