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

// A stream of stereo frames, as PC sound is: like FrameSource, with each
// frame interleaved, left first.
pub trait StereoSource {
    // Writes the next frame into `out`, which has room for MAX_FRAME frames
    // of two samples, and returns its length in frames. None while silent.
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize>;

    fn pause(&mut self) {}

    fn set_period(&mut self, _frames: usize) {}
}

// The streams of a mixer that takes none.
pub enum NoStreams {}

impl StereoSource for NoStreams {
    fn next_frame(&mut self, _: &mut [f32]) -> Option<usize> {
        match *self {}
    }
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
    // This talker's own volume, which moves to `wanted` over FADE samples
    // so a slider dragged mid-word does not click. A talker who has not
    // played yet starts at it.
    gain: f32,
    wanted: f32,
    played: bool,
}

impl<K, S: FrameSource> Voice<K, S> {
    // Adds this talker's next `out.len()` samples into `out`. A frame is
    // pulled only when what is held runs out, and its first samples go out in
    // the same call, so the only audio ever waiting here is the rest of the
    // current frame: less than one frame (under 5 ms, under 10 ms in the 10 ms
    // mode), and that is the frame playing out, not added delay.
    fn add_into(&mut self, out: &mut [f32]) {
        let step = 1.0 / FADE as f32;
        let mut done = 0;
        while done < out.len() {
            if self.len == 0 {
                match self.source.next_frame(&mut self.held) {
                    Some(len) if len > 0 => {
                        self.start = 0;
                        self.len = len.min(MAX_FRAME);
                        self.played = true;
                    }
                    _ => return,
                }
            }
            let take = self.len.min(out.len() - done);
            let from = &self.held[self.start..self.start + take];
            for (sum, sample) in out[done..done + take].iter_mut().zip(from) {
                self.gain += (self.wanted - self.gain).clamp(-step, step);
                *sum += sample * self.gain;
            }
            self.start += take;
            self.len -= take;
            done += take;
        }
    }
}

// A stereo stream in the mix, as a Voice is, with its frames two samples
// wide.
struct Stream<K, T> {
    key: K,
    source: T,
    held: [f32; 2 * MAX_FRAME],
    // In frames.
    start: usize,
    len: usize,
    gain: f32,
    wanted: f32,
    played: bool,
}

impl<K, T: StereoSource> Stream<K, T> {
    // As Voice::add_into, into interleaved stereo. False when it had nothing,
    // so nothing was added, not even zeros.
    fn add_into(&mut self, out: &mut [f32]) -> bool {
        let step = 1.0 / FADE as f32;
        let frames = out.len() / 2;
        let mut done = 0;
        while done < frames {
            if self.len == 0 {
                match self.source.next_frame(&mut self.held) {
                    Some(len) if len > 0 => {
                        self.start = 0;
                        self.len = len.min(MAX_FRAME);
                        self.played = true;
                    }
                    _ => return done > 0,
                }
            }
            let take = self.len.min(frames - done);
            for i in 0..take {
                self.gain += (self.wanted - self.gain).clamp(-step, step);
                let (to, from) = (2 * (done + i), 2 * (self.start + i));
                out[to] += self.held[from] * self.gain;
                out[to + 1] += self.held[from + 1] * self.gain;
            }
            self.start += take;
            self.len -= take;
            done += take;
        }
        true
    }
}

// Sums every talker who has audio into what the render side asks for,
// whatever its period, and with render_stereo every stereo stream beside
// them. The render thread owns it.
pub struct Mixer<K, S, T = NoStreams> {
    voices: Vec<Voice<K, S>>,
    streams: Vec<Stream<K, T>>,
    deafened: bool,
    gain: f32,
    period: usize,
    // The next 2.5 ms of talkers who just left, faded to nothing, still to be
    // added to the output.
    tail: [f32; FADE],
    // The same for streams, in stereo, and how many of its frames are left.
    stream_tail: [f32; 2 * FADE],
    stream_tail_left: usize,
}

impl<K: PartialEq, S: FrameSource> Mixer<K, S> {
    pub fn new() -> Mixer<K, S> {
        Mixer::with_streams()
    }
}

impl<K: PartialEq, S: FrameSource, T: StereoSource> Mixer<K, S, T> {
    // A mixer that takes stereo streams as well (add_stream).
    pub fn with_streams() -> Mixer<K, S, T> {
        Mixer {
            voices: Vec::with_capacity(ROOM),
            streams: Vec::with_capacity(ROOM),
            deafened: false,
            gain: 1.0,
            period: 0,
            tail: [0.0; FADE],
            stream_tail: [0.0; 2 * FADE],
            stream_tail_left: 0,
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
            gain: 1.0,
            wanted: 1.0,
            played: false,
        });
        replaced
    }

    // 1.0 plays the talker as they come, 0.0 not at all; over 1.0 louder,
    // with the limiter after the mix as for any other loud sum.
    pub fn set_gain(&mut self, key: &K, gain: f32) {
        let Some(voice) = self.voices.iter_mut().find(|voice| voice.key == *key) else {
            return;
        };
        voice.wanted = if gain.is_finite() { gain.max(0.0) } else { 1.0 };
        if !voice.played {
            voice.gain = voice.wanted;
        }
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

    // A stream added under a key already in use replaces the old one, as for
    // talkers. Make the source off the render thread.
    pub fn add_stream(&mut self, key: K, mut source: T) -> Option<T> {
        let replaced = self.remove_stream(&key);
        source.set_period(self.period);
        self.streams.push(Stream {
            key,
            source,
            held: [0.0; 2 * MAX_FRAME],
            start: 0,
            len: 0,
            gain: 1.0,
            wanted: 1.0,
            played: false,
        });
        replaced
    }

    // As set_gain, for a stream.
    pub fn set_stream_gain(&mut self, key: &K, gain: f32) {
        let Some(stream) = self.streams.iter_mut().find(|stream| stream.key == *key) else {
            return;
        };
        stream.wanted = if gain.is_finite() { gain.max(0.0) } else { 1.0 };
        if !stream.played {
            stream.gain = stream.wanted;
        }
    }

    // As remove, for a stream: its next 2.5 ms still play, fading out.
    pub fn remove_stream(&mut self, key: &K) -> Option<T> {
        let at = self.streams.iter().position(|stream| stream.key == *key)?;
        let mut stream = self.streams.swap_remove(at);
        if !self.silent() {
            let mut last = [0.0f32; 2 * FADE];
            if stream.add_into(&mut last) {
                let (tail, _) = self.stream_tail.as_chunks_mut::<2>();
                let (last, _) = last.as_chunks::<2>();
                for (i, (tail, last)) in tail.iter_mut().zip(last).enumerate() {
                    let fade = (FADE - i) as f32 / FADE as f32;
                    tail[0] += last[0] * fade;
                    tail[1] += last[1] * fade;
                }
                self.stream_tail_left = FADE;
            }
        }
        Some(stream.source)
    }

    pub fn stream_mut(&mut self, key: &K) -> Option<&mut T> {
        self.streams
            .iter_mut()
            .find(|stream| stream.key == *key)
            .map(|stream| &mut stream.source)
    }

    // The render side's period in samples, passed on to every talker's
    // source, now and as they join; a stream's in frames, the same number.
    pub fn set_period(&mut self, samples: usize) {
        self.period = samples;
        for voice in &mut self.voices {
            voice.source.set_period(samples);
        }
        for stream in &mut self.streams {
            stream.source.set_period(samples);
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
        stats::gauge!("voice.mixer.voices", self.voices);
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

    // As render, into interleaved stereo: the talkers in the middle, each
    // stream on its own sides, under one deafen fade and one limiter, so a
    // loud talker and a loud stream bend together and nothing bends twice.
    // With no stream both channels are exactly what render gives.
    pub fn render_stereo(&mut self, out: &mut [f32]) {
        stats::gauge!("voice.mixer.voices", self.voices);
        stats::gauge!("voice.mixer.streams", self.streams);
        out.fill(0.0);
        if self.silent() {
            self.pause_all();
            return;
        }
        let frames = out.len() / 2;
        let (middle, _) = out.split_at_mut(frames);
        for voice in &mut self.voices {
            voice.add_into(middle);
        }
        let tail = frames.min(FADE);
        for (sample, left) in middle.iter_mut().zip(&self.tail[..tail]) {
            *sample += left;
        }
        self.tail.copy_within(tail.., 0);
        self.tail[FADE - tail..].fill(0.0);
        // From the back, so each sample is read before its place is written.
        for i in (0..frames).rev() {
            let value = out[i];
            out[2 * i] = value;
            out[2 * i + 1] = value;
        }
        for stream in &mut self.streams {
            stream.add_into(out);
        }
        if self.stream_tail_left > 0 {
            let tail = frames.min(self.stream_tail_left);
            for (sample, left) in out.iter_mut().zip(&self.stream_tail[..2 * tail]) {
                *sample += left;
            }
            self.stream_tail.copy_within(2 * tail.., 0);
            self.stream_tail[2 * (FADE - tail)..].fill(0.0);
            self.stream_tail_left -= tail;
        }
        let step = 1.0 / FADE as f32;
        let (pairs, _) = out.as_chunks_mut::<2>();
        for [left, right] in pairs {
            self.gain = if self.deafened {
                (self.gain - step).max(0.0)
            } else {
                (self.gain + step).min(1.0)
            };
            *left = limit(*left * self.gain);
            *right = limit(*right * self.gain);
        }
        if self.silent() {
            self.pause_all();
        }
    }

    fn silent(&self) -> bool {
        self.deafened && self.gain == 0.0
    }

    // What was held belongs to before; after deafen every talker and stream
    // starts clean. Telling each source every period, not once, covers one
    // that joins while deafened.
    fn pause_all(&mut self) {
        self.tail = [0.0; FADE];
        for voice in &mut self.voices {
            voice.len = 0;
            voice.source.pause();
        }
        self.stream_tail = [0.0; 2 * FADE];
        self.stream_tail_left = 0;
        for stream in &mut self.streams {
            stream.len = 0;
            stream.source.pause();
        }
    }
}

impl<K: PartialEq, S: FrameSource> Default for Mixer<K, S> {
    fn default() -> Mixer<K, S> {
        Mixer::new()
    }
}
