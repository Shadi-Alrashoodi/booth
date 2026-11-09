// The sound cues about the room and about this PC's own voice and link,
// made here from plain steps and glides, nothing recorded. Each pair is one
// cue played backwards: up for someone arriving or something turning on or
// coming back, down for the opposite. Cues about friends and the room carry
// a soft octave, so they sound warm; cues about this PC are pure.
//
// The room's cues play in the mixer beside the voices, so deafen silences
// them as it does the share cue. This PC's own play after the mixer, as the
// control cue does, so the deafen cue and a dropped link are heard while
// deafened. Nothing here goes near the microphone or the socket.

use std::f64::consts::TAU;

use voice::codec::SAMPLE_RATE;
use voice::mix::FrameSource;

use super::cue::{self, ATTACK, CHUNK, FADE, RELEASE, swell};

// -20 dBFS at the peak at 100 percent, 6 dB under the share cue, so a change
// in the room is noticed without talking over anyone. At 150 percent on top
// of the share cue it still stays under the mixer's knee.
pub(super) const LEVEL: f32 = 0.1;
// The largest value of sin x + 0.3 sin 2x, so a warm sample never passes the
// level.
const WARM_PEAK: f64 = 1.1365;
const _: () = assert!(cue::PEAK + 1.5 * LEVEL < voice::mix::KNEE);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chime {
    FriendJoins,
    FriendLeaves,
    FriendDrops,
    FriendBack,
    ShareStarts,
    ShareStops,
    YouMute,
    YouUnmute,
    YouDeafen,
    YouUndeafen,
    YouDrop,
    YouBack,
}

impl Chime {
    const ALL: [Chime; 12] = [
        Self::FriendJoins,
        Self::FriendLeaves,
        Self::FriendDrops,
        Self::FriendBack,
        Self::ShareStarts,
        Self::ShareStops,
        Self::YouMute,
        Self::YouUnmute,
        Self::YouDeafen,
        Self::YouUndeafen,
        Self::YouDrop,
        Self::YouBack,
    ];

    fn segments(self) -> &'static [Segment] {
        match self {
            Self::FriendJoins => const { &[step(659.26, 70), step(987.77, 70)] },
            Self::FriendLeaves => const { &[step(987.77, 70), step(659.26, 70)] },
            Self::FriendDrops => const { &[step(880.0, 50), step(880.0, 50), step(587.33, 50)] },
            Self::FriendBack => const { &[step(587.33, 50), step(880.0, 50), step(880.0, 50)] },
            Self::ShareStarts => const { &[step(440.0, 90), step(554.37, 90)] },
            Self::ShareStops => const { &[step(554.37, 90), step(440.0, 90)] },
            Self::YouMute => const { &[step(659.26, 50), step(523.25, 50)] },
            Self::YouUnmute => const { &[step(523.25, 50), step(659.26, 50)] },
            Self::YouDeafen => const { &[step(659.26, 40), step(523.25, 40), step(440.0, 40)] },
            Self::YouUndeafen => const { &[step(440.0, 40), step(523.25, 40), step(659.26, 40)] },
            Self::YouDrop => const { &[glide(987.77, 493.88, 220)] },
            Self::YouBack => const { &[glide(493.88, 987.77, 220)] },
        }
    }

    fn warm(self) -> bool {
        matches!(
            self,
            Self::FriendJoins
                | Self::FriendLeaves
                | Self::FriendDrops
                | Self::FriendBack
                | Self::ShareStarts
                | Self::ShareStops
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    from_hz: f64,
    to_hz: f64,
    ms: usize,
}

const fn step(hz: f64, ms: usize) -> Segment {
    glide(hz, hz, ms)
}

const fn glide(from_hz: f64, to_hz: f64, ms: usize) -> Segment {
    Segment { from_hz, to_hz, ms }
}

impl Segment {
    fn len(self) -> usize {
        self.ms * SAMPLE_RATE as usize / 1000
    }

    fn cycles(self, at: usize) -> f64 {
        let n = at as f64;
        let rate = f64::from(SAMPLE_RATE);
        if self.from_hz == self.to_hz {
            self.from_hz * n / rate
        } else {
            let k = (self.to_hz / self.from_hz).ln();
            let len = self.len() as f64;
            self.from_hz * len / (rate * k) * (k * n / len).exp_m1()
        }
    }
}

pub(super) fn pack(chime: Chime, at_us: u64) -> u64 {
    (at_us << 5) | (chime as u64 + 1)
}

pub(super) fn unpack(word: u64, now_us: u64) -> Option<Chime> {
    let code = (word & 31) as usize;
    let chime = *Chime::ALL.get(code.checked_sub(1)?)?;
    (now_us.saturating_sub(word >> 5) <= cue::LATE_US).then_some(chime)
}

pub(super) struct Player {
    warm: bool,
    segments: &'static [Segment],
    len: usize,
    next: usize,
    level: f32,
    after: Option<(Chime, f32, usize)>,
}

impl Player {
    pub(super) fn idle() -> Player {
        Player {
            warm: false,
            segments: &[],
            len: 0,
            next: 0,
            level: 0.0,
            after: None,
        }
    }

    pub(super) fn start(&mut self, chime: Chime, level: f32) {
        if self.next == 0 || self.next >= self.len {
            self.begin(chime, level);
            return;
        }
        let ends = self
            .after
            .map_or((self.next + FADE).min(self.len), |(_, _, ends)| ends);
        self.after = Some((chime, level, ends));
    }

    fn begin(&mut self, chime: Chime, level: f32) {
        self.warm = chime.warm();
        self.segments = chime.segments();
        self.len = self.segments.iter().map(|segment| segment.len()).sum();
        self.next = 0;
        self.level = level;
        self.after = None;
    }

    fn sample(&self, at: usize) -> f32 {
        let mut n = at;
        let segment = self
            .segments
            .iter()
            .find(|segment| {
                if n < segment.len() {
                    true
                } else {
                    n -= segment.len();
                    false
                }
            })
            .expect("a sample within the chime");
        let len = segment.len();
        let mut envelope = if n < ATTACK {
            swell(n, ATTACK)
        } else if n > len - RELEASE {
            swell(len - n, RELEASE)
        } else {
            1.0
        };
        if let Some((_, _, ends)) = self.after
            && ends < self.len
        {
            envelope *= swell(ends - at, FADE);
        }
        let cycles = segment.cycles(n);
        let pure = (TAU * cycles.fract()).sin();
        let wave = if self.warm {
            (pure + 0.3 * (TAU * (2.0 * cycles).fract()).sin()) / WARM_PEAK
        } else {
            pure
        };
        (f64::from(self.level) * envelope * wave) as f32
    }
}

impl FrameSource for Player {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        if let Some((chime, level, ends)) = self.after
            && self.next >= ends
        {
            self.begin(chime, level);
        }
        let ends = self.after.map_or(self.len, |(_, _, ends)| ends);
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

    fn pause(&mut self) {
        self.next = self.len;
        self.after = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(player: &mut Player, samples: &mut Vec<f32>) {
        let mut out = [0.0; CHUNK];
        while let Some(len) = player.next_frame(&mut out) {
            samples.extend_from_slice(&out[..len]);
        }
    }

    fn whole(chime: Chime, level: f32) -> Vec<f32> {
        let mut player = Player::idle();
        player.start(chime, level);
        let mut samples = Vec::new();
        drain(&mut player, &mut samples);
        samples
    }

    fn amplitude(samples: &[f32], hz: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, sample) in samples.iter().enumerate() {
            let phase = TAU * hz * n as f64 / f64::from(SAMPLE_RATE);
            re += f64::from(*sample) * phase.cos();
            im += f64::from(*sample) * phase.sin();
        }
        2.0 * re.hypot(im) / samples.len() as f64
    }

    #[test]
    fn every_chime_has_its_notes() {
        for chime in Chime::ALL {
            let samples = whole(chime, LEVEL);
            let segments = chime.segments();
            assert_eq!(
                samples.len(),
                segments.iter().map(|segment| segment.len()).sum::<usize>()
            );
            let mut at = 0;
            for segment in segments {
                let note = &samples[at..at + segment.len()];
                if segment.from_hz == segment.to_hz {
                    let main = amplitude(note, segment.from_hz);
                    assert!(main > 0.5 * f64::from(LEVEL), "{chime:?}: {main}");
                    for other in segments
                        .iter()
                        .filter(|other| other.from_hz != segment.from_hz)
                    {
                        assert!(amplitude(note, other.from_hz) < 0.1 * main, "{chime:?}");
                    }
                } else {
                    let first = &note[..480];
                    let last = &note[note.len() - 480..];
                    assert!(
                        amplitude(first, segment.from_hz) > 2.0 * amplitude(first, segment.to_hz),
                        "{chime:?}"
                    );
                    assert!(
                        amplitude(last, segment.to_hz) > 2.0 * amplitude(last, segment.from_hz),
                        "{chime:?}"
                    );
                }
                at += segment.len();
            }
        }
    }

    #[test]
    fn every_chime_swells_without_click() {
        for chime in Chime::ALL {
            let samples = whole(chime, LEVEL);
            let peak = samples
                .iter()
                .fold(0.0f32, |most, sample| most.max(sample.abs()));
            assert!(peak <= LEVEL && peak > 0.95 * LEVEL, "{chime:?}: {peak}");
            let mut at = 0;
            for segment in chime.segments() {
                assert_eq!(samples[at], 0.0);
                at += segment.len();
                assert!(samples[at - 1].abs() < 1e-4, "{chime:?}");
            }
            let hz = chime
                .segments()
                .iter()
                .map(|segment| segment.from_hz.max(segment.to_hz))
                .fold(0.0f64, f64::max);
            let slope = f64::from(LEVEL) * TAU * hz / f64::from(SAMPLE_RATE)
                * 1.02
                * if chime.warm() { 1.6 / WARM_PEAK } else { 1.0 };
            let step = samples
                .windows(2)
                .map(|pair| (pair[1] - pair[0]).abs())
                .fold(0.0f32, f32::max);
            assert!(
                f64::from(step) <= slope,
                "{chime:?}: {step} against {slope}"
            );
        }
    }

    #[test]
    fn timbres_tell_whose_change_it_is() {
        for (chime, octave) in [(Chime::FriendJoins, 0.3), (Chime::YouMute, 0.0)] {
            let samples = whole(chime, LEVEL);
            let note = &samples[..chime.segments()[0].len()];
            let fundamental = amplitude(note, 659.26);
            let harmonic = amplitude(note, 2.0 * 659.26);
            assert!(
                (harmonic / fundamental - octave).abs() < 0.01,
                "{chime:?}: fundamental {fundamental}, octave {harmonic}"
            );
        }
    }

    #[test]
    fn pairs_mirror() {
        for pair in Chime::ALL.as_chunks::<2>().0 {
            let a = pair[0].segments();
            let b = pair[1].segments();
            assert_eq!(a.len(), b.len());
            assert_eq!(pair[0].warm(), pair[1].warm());
            for (left, right) in a.iter().zip(b.iter().rev()) {
                assert_eq!(*left, glide(right.to_hz, right.from_hz, right.ms));
            }
        }
    }

    #[test]
    fn lengths_tell_kinds_apart() {
        let mut lengths = vec![7680];
        for (pair, ms) in Chime::ALL
            .as_chunks::<2>()
            .0
            .iter()
            .zip([140, 150, 180, 100, 120, 220])
        {
            let len = whole(pair[0], LEVEL).len();
            assert_eq!(len, ms * SAMPLE_RATE as usize / 1000, "{pair:?}");
            assert!(lengths.iter().all(|other| len.abs_diff(*other) >= 480));
            assert_eq!(len, whole(pair[1], LEVEL).len());
            lengths.push(len);
        }
    }

    #[test]
    fn nothing_near_2_khz() {
        for chime in Chime::ALL {
            for segment in chime.segments() {
                let highest = segment.from_hz.max(segment.to_hz);
                assert!(highest <= if chime.warm() { 1000.0 } else { 2000.0 });
                assert!(segment.from_hz.min(segment.to_hz) >= 440.0);
            }
        }
    }

    #[test]
    fn one_sound_at_the_lowest_volume() {
        for chime in Chime::ALL {
            let samples = whole(chime, LEVEL * 0.1);
            let mut last = None;
            for (at, _) in samples
                .iter()
                .enumerate()
                .filter(|(_, sample)| sample.abs() > 1e-3)
            {
                if let Some(last) = last {
                    assert!(at - last < 480, "{chime:?}");
                }
                last = Some(at);
            }
            assert!(last.is_some(), "{chime:?}");
        }
    }

    #[test]
    fn chimes_pack() {
        let at = 1_700_000_000_000_000;
        for chime in Chime::ALL {
            assert_eq!(unpack(pack(chime, at), at), Some(chime));
            assert_eq!(unpack(pack(chime, at), at - 50), Some(chime));
            assert_eq!(unpack(pack(chime, at), at + cue::LATE_US), Some(chime));
            assert_eq!(unpack(pack(chime, at), at + cue::LATE_US + 1), None);
        }
        for code in [0].into_iter().chain(13..=31) {
            assert_eq!(unpack((at << 5) | code, at), None);
        }
    }

    #[test]
    fn newer_chime_fades_the_older_first() {
        let new = whole(Chime::FriendLeaves, LEVEL);
        for chime in Chime::ALL {
            let original = whole(chime, LEVEL);
            for cut in (1..original.len())
                .step_by(CHUNK)
                .chain([original.len() - 1])
            {
                let mut player = Player::idle();
                player.start(chime, LEVEL);
                let mut samples = Vec::new();
                let mut out = [0.0; CHUNK];
                while samples.len() < cut {
                    let want = (cut - samples.len()).min(CHUNK);
                    let len = player.next_frame(&mut out[..want]).unwrap();
                    samples.extend_from_slice(&out[..len]);
                }
                player.start(Chime::FriendLeaves, LEVEL);
                drain(&mut player, &mut samples);
                let fade = FADE.min(original.len() - cut);
                assert_eq!(&samples[..cut], &original[..cut]);
                assert_eq!(&samples[cut + fade..], &new, "{chime:?} at {cut}");
                for n in 0..fade {
                    let gain = if cut + fade < original.len() {
                        swell(fade - n, FADE) as f32
                    } else {
                        1.0
                    };
                    assert!((samples[cut + n] - original[cut + n] * gain).abs() < 1e-7);
                }
            }
        }
    }

    #[test]
    fn level_taken_at_start() {
        let low = whole(Chime::YouDrop, 0.05);
        let mut player = Player::idle();
        player.start(Chime::YouDrop, 0.05);
        let mut first = [0.0; CHUNK];
        player.next_frame(&mut first);
        player.start(Chime::YouBack, 0.15);
        let mut after = Vec::new();
        drain(&mut player, &mut after);
        assert_eq!(after[0], low[CHUNK]);
        assert_eq!(&after[FADE..], &whole(Chime::YouBack, 0.15));
        player.start(Chime::YouDrop, LEVEL);
        player.pause();
        assert_eq!(player.next_frame(&mut first), None);
    }

    #[test]
    fn newest_chime_takes_over_after_the_fade() {
        let mut player = Player::idle();
        player.start(Chime::FriendJoins, LEVEL);
        let mut out = [0.0; CHUNK];
        player.next_frame(&mut out);
        player.start(Chime::FriendLeaves, LEVEL);
        player.next_frame(&mut out[..FADE / 2]);
        player.start(Chime::ShareStarts, 0.05);
        let mut samples = Vec::new();
        drain(&mut player, &mut samples);
        assert_eq!(&samples[FADE / 2..], &whole(Chime::ShareStarts, 0.05));
    }
}
