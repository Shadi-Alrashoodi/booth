use crate::codec::MAX_FRAME;
use crate::jitter::JitterBuffer;

// Above this level the limiter starts to bend; below it the mix passes
// through untouched. -6 dBFS: one talker from a sensible microphone stays
// under it, two or more shouting at once do not.
pub const KNEE: f32 = 0.5;

// Deafen, undeafen and a talker leaving fade over 2.5 ms (120 samples)
// instead of cutting mid-waveform, which clicks.
const FADE: usize = 120;

// The room holds the host and seven clients (the room crate's roster limit),
// so a listener hears seven others at most. With room for all of them from
// the start, a talker joining never grows the list on the render thread.
const ROOM: usize = 8;

// Where a talker's audio comes from: the jitter buffer in the app, anything
// that makes frames in tests.
pub trait FrameSource {
    // Writes the next 5 or 10 ms frame into `out`, which has room for
    // MAX_FRAME samples, and returns its length. None while the talker is
    // silent.
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize>;

    // The mixer stopped pulling (deafened) until the next frame it asks for.
    fn pause(&mut self) {}

    // The render side's period in samples.
    fn set_period(&mut self, _samples: usize) {}
}

impl FrameSource for JitterBuffer {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        self.pull(out).map(|pulled| pulled.samples)
    }

    fn pause(&mut self) {
        JitterBuffer::pause(self);
    }

    fn set_period(&mut self, samples: usize) {
        JitterBuffer::set_period(self, samples);
    }
}

// The soft limiter's curve: straight up to the knee, then a tanh that leaves
// the knee at the same slope and flattens toward full scale, which it never
// passes. 1.0 comes out at 0.88, 2.0 at 0.9975. A sample that is not a
// number comes out as silence.
pub fn limit(sample: f32) -> f32 {
    if !sample.is_finite() {
        return 0.0;
    }
    let size = sample.abs();
    if size <= KNEE {
        return sample;
    }
    let bent = KNEE + (1.0 - KNEE) * ((size - KNEE) / (1.0 - KNEE)).tanh();
    bent.min(1.0).copysign(sample)
}

struct Voice<K, S> {
    key: K,
    source: S,
    held: [f32; MAX_FRAME],
    start: usize,
    len: usize,
}

impl<K, S: FrameSource> Voice<K, S> {
    // Adds this talker's next `out.len()` samples into `out`. A frame is
    // pulled only when what is held runs out, and its first samples go out in
    // the same call, so the only audio ever waiting here is the rest of the
    // current frame: less than one frame (under 5 ms, under 10 ms in the 10 ms
    // mode), and that is the frame playing out, not added delay.
    fn add_into(&mut self, out: &mut [f32]) {
        let mut done = 0;
        while done < out.len() {
            if self.len == 0 {
                match self.source.next_frame(&mut self.held) {
                    Some(len) if len > 0 => {
                        self.start = 0;
                        self.len = len.min(MAX_FRAME);
                    }
                    _ => return,
                }
            }
            let take = self.len.min(out.len() - done);
            let from = &self.held[self.start..self.start + take];
            for (sum, sample) in out[done..done + take].iter_mut().zip(from) {
                *sum += sample;
            }
            self.start += take;
            self.len -= take;
            done += take;
        }
    }
}

// Sums every talker who has audio into what the render side asks for,
// whatever its period. The render thread owns it.
pub struct Mixer<K, S> {
    voices: Vec<Voice<K, S>>,
    deafened: bool,
    gain: f32,
    period: usize,
    // The next 2.5 ms of talkers who just left, faded to nothing, still to be
    // added to the output.
    tail: [f32; FADE],
}

impl<K: PartialEq, S: FrameSource> Mixer<K, S> {
    pub fn new() -> Mixer<K, S> {
        Mixer {
            voices: Vec::with_capacity(ROOM),
            deafened: false,
            gain: 1.0,
            period: 0,
            tail: [0.0; FADE],
        }
    }

    // A talker added under a key already in use replaces the old one, which
    // fades out and is handed back. Make the source off the render thread: a
    // jitter buffer allocates its decoder, and dropping one frees it.
    pub fn add(&mut self, key: K, mut source: S) -> Option<S> {
        let replaced = self.remove(&key);
        source.set_period(self.period);
        self.voices.push(Voice {
            key,
            source,
            held: [0.0; MAX_FRAME],
            start: 0,
            len: 0,
        });
        replaced
    }

    // The talker's next 2.5 ms still play, fading out, so someone who leaves
    // while talking does not end on a click. That may pull one more frame.
    pub fn remove(&mut self, key: &K) -> Option<S> {
        let at = self.voices.iter().position(|voice| voice.key == *key)?;
        let mut voice = self.voices.swap_remove(at);
        if !self.silent() {
            let mut last = [0.0f32; FADE];
            voice.add_into(&mut last);
            for (i, (tail, sample)) in self.tail.iter_mut().zip(last).enumerate() {
                *tail += sample * (FADE - i) as f32 / FADE as f32;
            }
        }
        Some(voice.source)
    }

    pub fn source_mut(&mut self, key: &K) -> Option<&mut S> {
        self.voices
            .iter_mut()
            .find(|voice| voice.key == *key)
            .map(|voice| &mut voice.source)
    }

    // The render side's period in samples, passed on to every talker's
    // source, now and as they join.
    pub fn set_period(&mut self, samples: usize) {
        self.period = samples;
        for voice in &mut self.voices {
            voice.source.set_period(samples);
        }
    }

    // Deafened, the mixer plays silence and pulls nothing once the fade out
    // is done (within the next render call at any period over 2.5 ms), so
    // every jitter buffer drains and starts again at its own depth when this
    // is turned off.
    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
    }

    pub fn deafened(&self) -> bool {
        self.deafened
    }

    // Samples held for one talker, for the stats panel and tests.
    pub fn held(&self, key: &K) -> Option<usize> {
        self.voices
            .iter()
            .find(|voice| voice.key == *key)
            .map(|voice| voice.len)
    }

    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        if self.silent() {
            self.pause_all();
            return;
        }
        for voice in &mut self.voices {
            voice.add_into(out);
        }
        let tail = out.len().min(FADE);
        for (sample, left) in out.iter_mut().zip(&self.tail[..tail]) {
            *sample += left;
        }
        self.tail.copy_within(tail.., 0);
        self.tail[FADE - tail..].fill(0.0);
        let step = 1.0 / FADE as f32;
        for sample in out.iter_mut() {
            self.gain = if self.deafened {
                (self.gain - step).max(0.0)
            } else {
                (self.gain + step).min(1.0)
            };
            *sample = limit(*sample * self.gain);
        }
        if self.silent() {
            self.pause_all();
        }
    }

    fn silent(&self) -> bool {
        self.deafened && self.gain == 0.0
    }

    // What was held belongs to before; after deafen every talker starts
    // clean. Telling each source every period, not once, covers a talker who
    // joins while deafened.
    fn pause_all(&mut self) {
        self.tail = [0.0; FADE];
        for voice in &mut self.voices {
            voice.len = 0;
            voice.source.pause();
        }
    }
}

impl<K: PartialEq, S: FrameSource> Default for Mixer<K, S> {
    fn default() -> Mixer<K, S> {
        Mixer::new()
    }
}
